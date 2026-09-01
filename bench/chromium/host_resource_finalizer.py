#!/usr/bin/env python3
"""Restore host resources after a supervised benchmark process tree exits."""

from __future__ import annotations

import errno
import fcntl
import os
import re
import secrets
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
MEMORY_CGROUP_CHILD_RE = re.compile(r"(?:serve|req)-[A-Za-z0-9_.-]+")


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


def safe_absolute_path(name: str) -> str:
    value = required_environment(name)
    if ("\n" in value or "\r" in value or not os.path.isabs(value)
            or os.path.normpath(value) != value):
        raise FinalizerError(f"{name} must be one normalized absolute path")
    return value


def memory_cgroup_names(run_id: str, owner_token: str) -> tuple[str, str, str]:
    if OWNER_TOKEN_RE.fullmatch(run_id) is None:
        raise FinalizerError(
            "FCVM_CONTAINER_RUN_ID must be exactly 32 lowercase hex characters")
    if OWNER_TOKEN_RE.fullmatch(owner_token) is None:
        raise FinalizerError(
            "FCVM_CONTAINER_OWNER_TOKEN must be exactly 32 lowercase hex characters")
    base_name = f"cbmem-{run_id}.slice"
    stem = f".cbmem-cgroup-{run_id}-{owner_token}"
    return base_name, stem + ".owned", stem + ".identity"


def open_directory(path: str, description: str) -> int:
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY
    flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        raise FinalizerError(f"cannot open {description}: {exc}") from exc
    identity = os.fstat(descriptor)
    if not stat.S_ISDIR(identity.st_mode):
        os.close(descriptor)
        raise FinalizerError(f"{description} is not a directory")
    return descriptor


def stat_entry(directory_fd: int, name: str) -> os.stat_result | None:
    try:
        return os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise FinalizerError(f"cannot inspect {name}: {exc}") from exc


def write_all(descriptor: int, payload: bytes) -> None:
    offset = 0
    while offset < len(payload):
        written = os.write(descriptor, payload[offset:])
        if written <= 0:
            raise FinalizerError("short write while publishing cgroup ownership")
        offset += written


def publish_identity(directory_fd: int, name: str, payload: bytes) -> None:
    temporary = f"{name}.tmp-{os.getpid()}-{secrets.token_hex(8)}"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = None
    try:
        descriptor = os.open(temporary, flags, 0o600, dir_fd=directory_fd)
        write_all(descriptor, payload)
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = None
        os.replace(
            temporary, name, src_dir_fd=directory_fd, dst_dir_fd=directory_fd)
        os.fsync(directory_fd)
    except OSError as exc:
        raise FinalizerError(f"cannot publish cgroup ownership: {exc}") from exc
    finally:
        if descriptor is not None:
            os.close(descriptor)
        try:
            os.unlink(temporary, dir_fd=directory_fd)
        except FileNotFoundError:
            pass


def read_identity(directory_fd: int, name: str) -> tuple[int, int] | str | None:
    flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(name, flags, dir_fd=directory_fd)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise FinalizerError(f"cannot open cgroup identity: {exc}") from exc
    try:
        identity = os.fstat(descriptor)
        if not stat.S_ISREG(identity.st_mode):
            raise FinalizerError("cgroup identity is not a regular file")
        payload = os.read(descriptor, 128)
        if os.read(descriptor, 1):
            raise FinalizerError("cgroup identity is too long")
    finally:
        os.close(descriptor)
    if payload == b"absent\n":
        return "absent"
    match = re.fullmatch(rb"inode ([0-9]+) ([0-9]+)\n", payload)
    if match is None:
        raise FinalizerError("cgroup identity has invalid contents")
    return int(match.group(1)), int(match.group(2))


class MemoryCgroupLock:
    def __init__(self, path: str):
        self.path = path
        self.descriptor: int | None = None

    def __enter__(self):
        self.descriptor = open_directory(self.path, "memory cgroup lock")
        deadline = time.monotonic() + CREATE_LOCK_WAIT_SECONDS
        while True:
            try:
                fcntl.flock(
                    self.descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError as exc:
                if exc.errno not in (errno.EACCES, errno.EAGAIN, errno.EINTR):
                    os.close(self.descriptor)
                    self.descriptor = None
                    raise FinalizerError(
                        f"cannot lock memory cgroup creation: {exc}") from exc
                if time.monotonic() >= deadline:
                    os.close(self.descriptor)
                    self.descriptor = None
                    raise FinalizerError(
                        "memory cgroup creation lock stayed busy") from exc
                time.sleep(0.05)
        return self

    def __exit__(self, _exc_type, _exc, _traceback):
        if self.descriptor is not None:
            os.close(self.descriptor)
            self.descriptor = None


def memory_cgroup_environment() -> tuple[str, str, str, str, str, str, str]:
    root = safe_absolute_path("FCVM_MEMORY_CGROUP_ROOT")
    lock_path = safe_absolute_path("FCVM_MEMORY_CGROUP_LOCK_DIR")
    claim_dir = safe_absolute_path("FCVM_CONTAINER_CREATE_LOCK_DIR")
    run_id = required_environment("FCVM_CONTAINER_RUN_ID")
    owner_token = required_environment("FCVM_CONTAINER_OWNER_TOKEN")
    base_name, marker_name, identity_name = memory_cgroup_names(
        run_id, owner_token)
    return (root, lock_path, claim_dir, base_name, marker_name,
            identity_name, os.path.join(root, base_name))


def ensure_memory_cgroup_lock(path: str) -> None:
    result = run_bounded((
        "sudo", "-n", "install", "-d", "-o", "root", "-g", "root",
        "-m", "0755", "--", path,
    ))
    if result.returncode != 0:
        raise FinalizerError(
            f"cannot create memory cgroup lock (status={result.returncode})")
    descriptor = open_directory(path, "memory cgroup lock")
    os.close(descriptor)


def remove_memory_cgroup_exact(
        root: str, base_name: str, expected_dev: int, expected_ino: int,
        run_id: str, owner_token: str, before_remove=None) -> None:
    """Validate and remove one exact cgroup in one privileged process.

    The caller holds the host-wide memory-cgroup lock, so every fcvm creator
    and finalizer is excluded for the whole operation. The retained descriptor
    prevents reuse of the expected inode while this helper runs.
    """
    expected_base, _marker, _identity = memory_cgroup_names(
        run_id, owner_token)
    if base_name != expected_base:
        raise FinalizerError("memory cgroup removal name is not run-derived")
    if (not os.path.isabs(root) or os.path.normpath(root) != root
            or "\n" in root or "\r" in root):
        raise FinalizerError("memory cgroup removal root is not absolute")
    expected = (expected_dev, expected_ino)
    root_fd = open_directory(root, "memory cgroup root")
    try:
        current = stat_entry(root_fd, base_name)
        if current is None or (current.st_dev, current.st_ino) != expected:
            raise FinalizerError(
                "same-name memory cgroup is not the pinned inode; preserving it")
        try:
            owned_fd = os.open(
                base_name,
                os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY
                | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=root_fd,
            )
        except OSError as exc:
            raise FinalizerError(
                f"cannot open pinned memory cgroup for removal: {exc}") from exc
        try:
            owned = os.fstat(owned_fd)
            if (owned.st_dev, owned.st_ino) != expected:
                raise FinalizerError("memory cgroup identity changed before removal")
            children = []
            for child in sorted(os.listdir(owned_fd)):
                child_identity = stat_entry(owned_fd, child)
                if child_identity is None or not stat.S_ISDIR(child_identity.st_mode):
                    continue
                if MEMORY_CGROUP_CHILD_RE.fullmatch(child) is None:
                    raise FinalizerError(
                        f"claimed memory cgroup has unexpected child {child!r}")
                children.append(child)
            for child in children:
                try:
                    os.rmdir(child, dir_fd=owned_fd)
                except OSError as exc:
                    raise FinalizerError(
                        f"cannot remove memory cgroup child {child}: {exc}") from exc
        finally:
            os.close(owned_fd)
        if before_remove is not None:
            before_remove()
        current = stat_entry(root_fd, base_name)
        if current is None or (current.st_dev, current.st_ino) != expected:
            raise FinalizerError(
                "memory cgroup changed before final removal; preserving it")
        try:
            os.rmdir(base_name, dir_fd=root_fd)
        except OSError as exc:
            raise FinalizerError(f"cannot remove memory cgroup: {exc}") from exc
        if stat_entry(root_fd, base_name) is not None:
            raise FinalizerError("memory cgroup survived removal")
    finally:
        os.close(root_fd)


def exact_cgroup_remove_command(
        root: str, base_name: str, expected: tuple[int, int],
        run_id: str, owner_token: str) -> tuple[str, ...]:
    return (
        "sudo", "-n", sys.executable, os.path.abspath(__file__),
        "--internal-remove-memory-cgroup", root, base_name,
        str(expected[0]), str(expected[1]), run_id, owner_token,
    )


def clear_memory_cgroup_claim(
        claim_fd: int, marker_name: str, identity_name: str) -> None:
    publish_identity(claim_fd, identity_name, b"absent\n")
    os.unlink(marker_name, dir_fd=claim_fd)
    os.fsync(claim_fd)
    os.unlink(identity_name, dir_fd=claim_fd)
    os.fsync(claim_fd)


def claim_memory_cgroup(base: str) -> tuple[int, int]:
    """Claim and create one exact cgroup while its detached finalizer is armed."""
    (root, lock_path, claim_dir, base_name, marker_name,
     identity_name, expected_base) = memory_cgroup_environment()
    if os.path.abspath(base) != expected_base:
        raise FinalizerError(
            f"memory cgroup must be the run-derived path {expected_base}")
    ensure_memory_cgroup_lock(lock_path)
    with MemoryCgroupLock(lock_path), ExitStack() as stack:
        root_fd = open_directory(root, "memory cgroup root")
        stack.callback(os.close, root_fd)
        claim_fd = open_directory(claim_dir, "memory cgroup claim directory")
        stack.callback(os.close, claim_fd)
        if stat_entry(claim_fd, marker_name) is not None:
            raise FinalizerError("memory cgroup ownership marker already exists")
        if stat_entry(claim_fd, identity_name) is not None:
            raise FinalizerError("memory cgroup identity already exists")
        if stat_entry(root_fd, base_name) is not None:
            raise FinalizerError(
                f"cgroup {expected_base} already exists; this run does not own it")

        marker_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
        marker_flags |= getattr(os, "O_NOFOLLOW", 0)
        try:
            marker_fd = os.open(
                marker_name, marker_flags, 0o600, dir_fd=claim_fd)
        except OSError as exc:
            raise FinalizerError(f"cannot publish cgroup ownership intent: {exc}") from exc
        os.close(marker_fd)
        os.fsync(claim_fd)

        created = run_bounded(("sudo", "-n", "mkdir", "--", expected_base))
        current = stat_entry(root_fd, base_name)
        if current is None:
            clear_memory_cgroup_claim(
                claim_fd, marker_name, identity_name)
            raise FinalizerError(
                f"cannot create memory cgroup (status={created.returncode})")
        if not stat.S_ISDIR(current.st_mode):
            raise FinalizerError("created memory cgroup is not a directory")
        try:
            owned_fd = os.open(
                base_name,
                os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY
                | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=root_fd,
            )
        except OSError as exc:
            rollback = run_bounded(exact_cgroup_remove_command(
                root, base_name, (current.st_dev, current.st_ino),
                required_environment("FCVM_CONTAINER_RUN_ID"),
                required_environment("FCVM_CONTAINER_OWNER_TOKEN")))
            if rollback.returncode == 0 and stat_entry(root_fd, base_name) is None:
                clear_memory_cgroup_claim(
                    claim_fd, marker_name, identity_name)
                raise FinalizerError(
                    f"cannot pin created memory cgroup: {exc}") from exc
            raise FinalizerError(
                f"cannot pin or roll back created memory cgroup: {exc}; "
                f"rollback status={rollback.returncode}") from exc
        pinned = os.fstat(owned_fd)
        if (pinned.st_dev, pinned.st_ino) != (current.st_dev, current.st_ino):
            os.close(owned_fd)
            raise FinalizerError("memory cgroup changed before it could be pinned")
        try:
            publish_identity(
                claim_fd, identity_name,
                f"inode {current.st_dev} {current.st_ino}\n".encode("ascii"))
            retained_claim_fd = os.dup(claim_fd)
        except BaseException:
            rollback = run_bounded(exact_cgroup_remove_command(
                root, base_name, (pinned.st_dev, pinned.st_ino),
                required_environment("FCVM_CONTAINER_RUN_ID"),
                required_environment("FCVM_CONTAINER_OWNER_TOKEN")))
            os.close(owned_fd)
            if rollback.returncode == 0 and stat_entry(root_fd, base_name) is None:
                clear_memory_cgroup_claim(
                    claim_fd, marker_name, identity_name)
            raise
        return owned_fd, retained_claim_fd


def required_descriptor(name: str, description: str) -> tuple[int, os.stat_result]:
    raw_descriptor = required_environment(name)
    if re.fullmatch(r"[0-9]+", raw_descriptor) is None:
        raise FinalizerError(f"{name} must be one decimal descriptor")
    descriptor = int(raw_descriptor)
    try:
        identity = os.fstat(descriptor)
    except OSError as exc:
        raise FinalizerError(f"cannot inspect {description}: {exc}") from exc
    if not stat.S_ISDIR(identity.st_mode):
        raise FinalizerError(f"{description} descriptor is not a directory")
    return descriptor, identity


def pinned_memory_cgroup(base: str) -> tuple[int, os.stat_result]:
    (_root, _lock_path, _claim_dir, _base_name, _marker_name,
     _identity_name, expected_base) = memory_cgroup_environment()
    if os.path.abspath(base) != expected_base:
        raise FinalizerError(
            f"memory cgroup must be the run-derived path {expected_base}")
    return required_descriptor(
        "FCVM_MEMORY_CGROUP_FD", "pinned memory cgroup")


def pinned_memory_claim_directory() -> int:
    descriptor, _identity = required_descriptor(
        "FCVM_MEMORY_CGROUP_CLAIM_FD", "pinned memory cgroup claim directory")
    return descriptor


def verify_pinned_memory_cgroup(base: str) -> int:
    descriptor, pinned = pinned_memory_cgroup(base)
    (root, _lock_path, _claim_dir, base_name, marker_name,
     _identity_name, _expected_base) = memory_cgroup_environment()
    root_fd = open_directory(root, "memory cgroup root")
    try:
        current = stat_entry(root_fd, base_name)
    finally:
        os.close(root_fd)
    if current is None:
        raise FinalizerError("pinned memory cgroup is absent")
    if (current.st_dev, current.st_ino) != (pinned.st_dev, pinned.st_ino):
        raise FinalizerError("pinned memory cgroup has been replaced")
    claim_fd = pinned_memory_claim_directory()
    marker = stat_entry(claim_fd, marker_name)
    if marker is None or not stat.S_ISREG(marker.st_mode):
        raise FinalizerError("pinned memory cgroup has no ownership marker")
    return descriptor


def finalize_memory_cgroup() -> None:
    """Remove only the cgroup inode claimed by this exact run and owner token."""
    (root, lock_path, _claim_dir, base_name, marker_name,
     identity_name, base_path) = memory_cgroup_environment()
    claim_fd = pinned_memory_claim_directory()
    marker = stat_entry(claim_fd, marker_name)
    if marker is None or not stat.S_ISREG(marker.st_mode):
        raise FinalizerError("pinned cgroup ownership marker is absent or invalid")

    with MemoryCgroupLock(lock_path), ExitStack() as stack:
        root_fd = open_directory(root, "memory cgroup root")
        stack.callback(os.close, root_fd)
        marker = stat_entry(claim_fd, marker_name)
        if marker is None or not stat.S_ISREG(marker.st_mode):
            raise FinalizerError("pinned cgroup ownership marker changed")

        _pinned_fd, pinned = pinned_memory_cgroup(base_path)

        recorded = read_identity(claim_fd, identity_name)
        current = stat_entry(root_fd, base_name)
        expected = (pinned.st_dev, pinned.st_ino)
        if recorded != expected:
            raise FinalizerError(
                "pinned memory cgroup does not match its ownership record")
        if current is None:
            raise FinalizerError(
                "owned memory cgroup was renamed; refusing to lose its pinned inode")
        if (current.st_dev, current.st_ino) != expected:
            raise FinalizerError(
                "same-name memory cgroup has a different inode; preserving it")
        run_id = required_environment("FCVM_CONTAINER_RUN_ID")
        owner_token = required_environment("FCVM_CONTAINER_OWNER_TOKEN")
        removed = run_bounded(exact_cgroup_remove_command(
            root, base_name, expected, run_id, owner_token))
        if removed.returncode != 0:
            raise FinalizerError(
                f"exact memory cgroup removal failed (status={removed.returncode})")
        if stat_entry(root_fd, base_name) is not None:
            raise FinalizerError("memory cgroup name survived exact removal")
        clear_memory_cgroup_claim(claim_fd, marker_name, identity_name)


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


def finalize_memory_resource_set() -> None:
    """Run independent container and cgroup teardown before reporting errors."""
    errors = []
    for name, finalizer in (
        ("finalize_container_set", finalize_container_set),
        ("finalize_memory_cgroup", finalize_memory_cgroup),
    ):
        try:
            finalizer()
        except Exception as exc:
            errors.append(f"{name}: {type(exc).__name__}: {exc}")
    if errors:
        raise FinalizerError("; ".join(errors))


def main(argv: Sequence[str] | None = None) -> int:
    argv = sys.argv if argv is None else argv
    try:
        if len(argv) == 8 and argv[1] == "--internal-remove-memory-cgroup":
            try:
                expected_dev = int(argv[4])
                expected_ino = int(argv[5])
            except ValueError as exc:
                raise FinalizerError(
                    "memory cgroup identity must contain decimal integers") from exc
            if expected_dev < 0 or expected_ino <= 0:
                raise FinalizerError("memory cgroup identity is out of range")
            remove_memory_cgroup_exact(
                argv[2], argv[3], expected_dev, expected_ino,
                argv[6], argv[7])
            return 0
        if len(argv) != 1:
            raise FinalizerError("host resource finalizer accepts no arguments")
        mode = required_environment("FCVM_FINALIZER_MODE")
        if mode == "dnsmasq":
            finalize_dnsmasq()
        elif mode == "container":
            finalize_container()
        elif mode == "container-set":
            finalize_container_set()
        elif mode == "memory-set":
            finalize_memory_resource_set()
        else:
            raise FinalizerError(
                "FCVM_FINALIZER_MODE must be dnsmasq, container, "
                "container-set, or memory-set")
        return 0
    except FinalizerError as exc:
        print(f"FAILED: host resource finalizer: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
