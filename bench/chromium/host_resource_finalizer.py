#!/usr/bin/env python3
"""Restore host resources after a supervised benchmark process tree exits."""

from __future__ import annotations

import errno
import fcntl
import os
import re
import signal
import stat
import subprocess
import sys
import time
from contextlib import ExitStack
from dataclasses import dataclass
from typing import Sequence


COMMAND_TIMEOUT_SECONDS = 30.0
COMMAND_TERM_GRACE_SECONDS = 5.0
COMMAND_KILL_REAP_SECONDS = 5.0
DNSMASQ_COMMAND_TIMEOUT_SECONDS = 3.0
DNSMASQ_COMMAND_TERM_GRACE_SECONDS = 0.5
DNSMASQ_COMMAND_KILL_REAP_SECONDS = 0.5
CREATE_LOCK_WAIT_SECONDS = 30.0
DNSMASQ_START_ATTEMPTS = 10
DNSMASQ_RETRY_SECONDS = 1.0

CONTAINER_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")
CONTAINER_ID_RE = re.compile(r"[0-9a-f]{64}")
OWNER_TOKEN_RE = re.compile(r"[0-9a-f]{32}")


class FinalizerError(RuntimeError):
    """An external state could not be restored or proved safe."""


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: bytes
    stderr: bytes


def _signal_process_group(process: subprocess.Popen, signum: int) -> None:
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass


def _close_command_pipes(process: subprocess.Popen) -> None:
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            stream.close()


def run_bounded(
        argv: Sequence[str], timeout: float = COMMAND_TIMEOUT_SECONDS,
        term_grace: float = COMMAND_TERM_GRACE_SECONDS,
        kill_reap: float = COMMAND_KILL_REAP_SECONDS) -> CommandResult:
    """Run one command with bounded TERM, KILL, and post-KILL reap stages."""
    try:
        process = subprocess.Popen(
            list(argv),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as exc:
        raise FinalizerError(f"cannot start {argv[0]}: {exc}") from exc

    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return CommandResult(process.returncode, stdout, stderr)
    except subprocess.TimeoutExpired:
        _signal_process_group(process, signal.SIGTERM)

    try:
        stdout, stderr = process.communicate(
            timeout=term_grace,
        )
    except subprocess.TimeoutExpired:
        _signal_process_group(process, signal.SIGKILL)
    else:
        return CommandResult(124, stdout, stderr)

    try:
        stdout, stderr = process.communicate(
            timeout=kill_reap,
        )
    except subprocess.TimeoutExpired as exc:
        _close_command_pipes(process)
        raise FinalizerError(
            f"{argv[0]} survived SIGKILL past its reap deadline"
        ) from exc
    return CommandResult(124, stdout, stderr)


def required_environment(name: str) -> str:
    try:
        value = os.environ[name]
    except KeyError as exc:
        raise FinalizerError(f"{name} is required") from exc
    if not value:
        raise FinalizerError(f"{name} must not be empty")
    return value


def finalize_dnsmasq() -> None:
    was_active = required_environment("FCVM_DNSMASQ_WAS_ACTIVE")
    if was_active not in ("yes", "no"):
        raise FinalizerError("FCVM_DNSMASQ_WAS_ACTIVE must be yes or no")
    if was_active == "no":
        return

    for attempt in range(DNSMASQ_START_ATTEMPTS):
        result = run_bounded(
            ("systemctl", "start", "dnsmasq"),
            DNSMASQ_COMMAND_TIMEOUT_SECONDS,
            DNSMASQ_COMMAND_TERM_GRACE_SECONDS,
            DNSMASQ_COMMAND_KILL_REAP_SECONDS,
        )
        if result.returncode == 0:
            break
        if attempt + 1 < DNSMASQ_START_ATTEMPTS:
            time.sleep(DNSMASQ_RETRY_SECONDS)

    active = run_bounded(
        ("systemctl", "is-active", "--quiet", "dnsmasq"),
        DNSMASQ_COMMAND_TIMEOUT_SECONDS,
        DNSMASQ_COMMAND_TERM_GRACE_SECONDS,
        DNSMASQ_COMMAND_KILL_REAP_SECONDS,
    )
    if active.returncode != 0:
        raise FinalizerError(
            "dnsmasq did not become active after bounded restart attempts"
        )


class ExistingCreateLock:
    """Hold an existing create-operation lease, without creating a new file."""

    def __init__(self, path: str, directory_fd: int | None = None):
        self.path = path
        self.directory_fd = directory_fd
        self.fd: int | None = None
        self.acquired = False
        self.retired = False

    def __enter__(self):
        flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        try:
            self.fd = os.open(self.path, flags, dir_fd=self.directory_fd)
        except FileNotFoundError:
            return self
        except OSError as exc:
            raise FinalizerError(
                f"cannot open container create-operation lock: {exc}"
            ) from exc

        try:
            identity = os.fstat(self.fd)
            if not stat.S_ISREG(identity.st_mode):
                raise FinalizerError(
                    "container create-operation lock is not a regular file"
                )
            deadline = time.monotonic() + CREATE_LOCK_WAIT_SECONDS
            while True:
                try:
                    fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except OSError as exc:
                    if exc.errno not in (errno.EACCES, errno.EAGAIN, errno.EINTR):
                        raise FinalizerError(
                            f"cannot lock container create-operation lease: {exc}"
                        ) from exc
                    if time.monotonic() >= deadline:
                        raise FinalizerError(
                            "container create operation did not quiesce before "
                            "the finalizer deadline"
                        ) from exc
                    time.sleep(0.05)

            try:
                path_identity = os.stat(
                    self.path, dir_fd=self.directory_fd, follow_symlinks=False)
            except OSError as exc:
                raise FinalizerError(
                    "container create-operation lock changed while it was acquired"
                ) from exc
            if ((identity.st_dev, identity.st_ino)
                    != (path_identity.st_dev, path_identity.st_ino)):
                raise FinalizerError(
                    "container create-operation lock identity changed"
                )
            os.lseek(self.fd, 0, os.SEEK_SET)
            state = os.read(self.fd, 64)
            if state not in (b"", b"retired\n"):
                raise FinalizerError(
                    "container create-operation lock has an invalid state"
                )
            self.acquired = True
            self.retired = state == b"retired\n"
            return self
        except BaseException:
            os.close(self.fd)
            self.fd = None
            raise

    def __exit__(self, _exc_type, _exc, _traceback):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        self.acquired = False
        self.retired = False


def prove_container_absent(reference: str) -> bool:
    result = run_bounded(("podman", "container", "exists", reference))
    if result.returncode == 1:
        return True
    if result.returncode == 0:
        return False
    raise FinalizerError(
        f"cannot establish whether container exists (status={result.returncode})"
    )


def inspect_container_identity(name: str) -> tuple[str, str] | None:
    result = run_bounded((
        "podman", "inspect", "--type", "container", "--format",
        '{{.Id}}|{{index .Config.Labels "io.fcvm.bench.owner"}}', name,
    ))
    if result.returncode != 0:
        if prove_container_absent(name):
            return None
        raise FinalizerError(
            f"container exists but cannot be inspected (status={result.returncode})"
        )

    try:
        identity = result.stdout.decode("ascii")
    except UnicodeDecodeError as exc:
        raise FinalizerError(
            "container inspection returned a non-ASCII identity"
        ) from exc
    match = re.fullmatch(r"([0-9a-f]{64})\|([0-9a-f]{32})\n?", identity)
    if match is None:
        raise FinalizerError("container inspection returned an invalid identity")
    return match.groups()


def inspect_owned_container(name: str, owner_token: str) -> str | None:
    identity = inspect_container_identity(name)
    if identity is None:
        return None
    container_id, actual_owner = identity
    if actual_owner != owner_token:
        raise FinalizerError("same-name container has a different owner label")
    return container_id


def finalize_container() -> None:
    name = required_environment("FCVM_CONTAINER_NAME")
    owner_token = required_environment("FCVM_CONTAINER_OWNER_TOKEN")
    lock_path = required_environment("FCVM_CONTAINER_CREATE_LOCK_PATH")
    if CONTAINER_NAME_RE.fullmatch(name) is None:
        raise FinalizerError("FCVM_CONTAINER_NAME is not a safe exact name")
    if OWNER_TOKEN_RE.fullmatch(owner_token) is None:
        raise FinalizerError(
            "FCVM_CONTAINER_OWNER_TOKEN must be exactly 32 lowercase hex characters"
        )
    if "\n" in lock_path or "\r" in lock_path:
        raise FinalizerError(
            "FCVM_CONTAINER_CREATE_LOCK_PATH must be one filesystem path"
        )

    with ExistingCreateLock(lock_path) as create_lock:
        if not create_lock.acquired or create_lock.retired:
            return
        container_id = inspect_owned_container(name, owner_token)
        if container_id is None:
            return
        if CONTAINER_ID_RE.fullmatch(container_id) is None:
            raise FinalizerError("container inspection returned a non-exact ID")

        removed = run_bounded(("podman", "rm", "-f", "--", container_id))
        if removed.returncode != 0:
            if prove_container_absent(container_id):
                return
            raise FinalizerError(
                f"could not remove owned container (status={removed.returncode})"
            )
        if not prove_container_absent(container_id):
            raise FinalizerError("owned container survived podman rm")


def classify_memory_containers(
        run_id: str, owner_token: str
) -> tuple[list[tuple[str, str]], list[tuple[str, str]]]:
    """Classify exact run-prefix rows without letting collisions block cleanup."""
    result = run_bounded((
        "podman", "ps", "-a", "--no-trunc", "--format", "{{.ID}}|{{.Names}}",
    ))
    if result.returncode != 0:
        raise FinalizerError(
            f"cannot enumerate memory containers (status={result.returncode})")
    try:
        listing = result.stdout.decode("ascii")
    except UnicodeDecodeError as exc:
        raise FinalizerError("container listing is not ASCII") from exc

    prefix = f"cbmem-{run_id}-"
    owned = []
    foreign = []
    seen_ids = set()
    seen_names = set()
    for line in listing.splitlines():
        if not line:
            continue
        fields = line.split("|")
        if len(fields) != 2:
            raise FinalizerError("container listing returned an invalid row")
        container_id, name = fields
        if not name.startswith(prefix):
            continue
        if (CONTAINER_ID_RE.fullmatch(container_id) is None
                or CONTAINER_NAME_RE.fullmatch(name) is None):
            raise FinalizerError("memory container listing has an invalid identity")
        if container_id in seen_ids or name in seen_names:
            raise FinalizerError("memory container listing has a duplicate identity")
        identity = inspect_container_identity(container_id)
        if identity is None:
            continue
        inspected_id, actual_owner = identity
        if inspected_id != container_id:
            raise FinalizerError("memory container changed exact identity")
        seen_ids.add(container_id)
        seen_names.add(name)
        target = owned if actual_owner == owner_token else foreign
        target.append((container_id, name))
    return owned, foreign


def hold_memory_create_leases(lock_dir: str, run_id: str, stack: ExitStack) -> None:
    """Hold every run create lease and prove the stable, quiescent set."""
    try:
        directory_fd = os.open(
            lock_dir,
            os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY
            | getattr(os, "O_NOFOLLOW", 0),
        )
    except FileNotFoundError:
        return
    except OSError as exc:
        raise FinalizerError(
            f"cannot open container create-operation directory: {exc}") from exc
    stack.callback(os.close, directory_fd)
    identity = os.fstat(directory_fd)
    prefix = f"cbmem-{run_id}-"

    def matching_entries() -> set[str]:
        try:
            entries = os.listdir(directory_fd)
        except OSError as exc:
            raise FinalizerError(
                f"cannot enumerate container create-operation leases: {exc}") from exc
        matches = {entry for entry in entries
                   if entry.startswith(prefix) and entry.endswith(".lock")}
        if any(CONTAINER_NAME_RE.fullmatch(entry[:-5]) is None for entry in matches):
            raise FinalizerError("container create-operation lease has an invalid name")
        return matches

    before = matching_entries()
    for entry in sorted(before):
        lease = stack.enter_context(
            ExistingCreateLock(entry, directory_fd))
        if not lease.acquired:
            raise FinalizerError("container create-operation lease disappeared")
    try:
        current = os.stat(lock_dir, follow_symlinks=False)
    except OSError as exc:
        raise FinalizerError(
            "container create-operation directory changed while locked") from exc
    if ((identity.st_dev, identity.st_ino)
            != (current.st_dev, current.st_ino)):
        raise FinalizerError("container create-operation directory changed identity")
    if matching_entries() != before:
        raise FinalizerError("container create-operation lease set changed while locked")


def finalize_container_set() -> None:
    """Remove every exact, owner-labelled memory container for one run."""
    run_id = required_environment("FCVM_CONTAINER_RUN_ID")
    owner_token = required_environment("FCVM_CONTAINER_OWNER_TOKEN")
    lock_dir = required_environment("FCVM_CONTAINER_CREATE_LOCK_DIR")
    if OWNER_TOKEN_RE.fullmatch(run_id) is None:
        raise FinalizerError(
            "FCVM_CONTAINER_RUN_ID must be exactly 32 lowercase hex characters")
    if OWNER_TOKEN_RE.fullmatch(owner_token) is None:
        raise FinalizerError(
            "FCVM_CONTAINER_OWNER_TOKEN must be exactly 32 lowercase hex characters")
    if "\n" in lock_dir or "\r" in lock_dir:
        raise FinalizerError(
            "FCVM_CONTAINER_CREATE_LOCK_DIR must be one filesystem path")

    with ExitStack() as create_leases:
        hold_memory_create_leases(lock_dir, run_id, create_leases)
        rows, foreign = classify_memory_containers(run_id, owner_token)
        if not rows:
            if foreign:
                raise FinalizerError(
                    "memory container prefix has a different owner label")
            return
        identifiers = tuple(container_id for container_id, _name in rows)
        removed = run_bounded(("podman", "rm", "-f", "--", *identifiers))
        survivor_ids = [
            container_id for container_id in identifiers
            if not prove_container_absent(container_id)
        ]
        replacements, later_foreign = classify_memory_containers(
            run_id, owner_token)
        if survivor_ids or replacements:
            survivor_ids.extend(
                container_id for container_id, _name in replacements
                if container_id not in survivor_ids)
            raise FinalizerError(
                "owned memory containers survived podman rm: "
                + ",".join(survivor_ids))
        if foreign or later_foreign:
            raise FinalizerError(
                "memory container prefix has a different owner label")
        if removed.returncode != 0:
            # Podman may report a concurrent already-absent ID as failure. The
            # second exact enumeration above is the authoritative absence proof.
            return


def main(argv: Sequence[str] | None = None) -> int:
    argv = sys.argv if argv is None else argv
    try:
        if len(argv) != 1:
            raise FinalizerError("host resource finalizer accepts no arguments")
        mode = required_environment("FCVM_FINALIZER_MODE")
        if mode == "dnsmasq":
            finalize_dnsmasq()
        elif mode == "container":
            finalize_container()
        elif mode == "container-set":
            finalize_container_set()
        else:
            raise FinalizerError(
                "FCVM_FINALIZER_MODE must be dnsmasq, container, or container-set")
        return 0
    except FinalizerError as exc:
        print(f"FAILED: host resource finalizer: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
