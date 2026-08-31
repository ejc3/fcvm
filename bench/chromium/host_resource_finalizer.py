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

    def __init__(self, path: str):
        self.path = path
        self.fd: int | None = None
        self.acquired = False
        self.retired = False

    def __enter__(self):
        flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        try:
            self.fd = os.open(self.path, flags)
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
                path_identity = os.stat(self.path, follow_symlinks=False)
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


def inspect_owned_container(name: str, owner_token: str) -> str | None:
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
    container_id, actual_owner = match.groups()
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
        else:
            raise FinalizerError("FCVM_FINALIZER_MODE must be dnsmasq or container")
        return 0
    except FinalizerError as exc:
        print(f"FAILED: host resource finalizer: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
