#!/bin/bash
# Enumerate and reclaim idle Cargo targets without a pathname handoff. The same
# privileged process opens and locks candidates as it discovers them, retains
# every fd until all roots have been enumerated successfully, then validates
# and truncates reclaimable payloads through those fds. Before changing any
# payload, it durably retires the target inode. The Cargo wrapper will publish
# a fresh physical generation before the next build. Directory entries are
# never removed or renamed: Linux VFS deletion can detach a bind mount that is
# visible only in another mount namespace, and retained build-script OUT_DIR
# names are not a valid fresh Cargo cache namespace even after fingerprints are
# invalidated.
set -euo pipefail

if (($# != 4)); then
  printf 'usage: %s CUTOFF DRY_RUN RUNNER_WORK_ROOT BTRFS_TARGET_ROOT\n' "$0" >&2
  exit 2
fi

# Stay in the caller's mount namespace so openat2 rejects every mount visible at
# traversal time. A mount visible only in another namespace cannot be detected
# here; retaining every dentry is what keeps that foreign mount attached at the
# same path without traversing its source.

exec /usr/bin/python3 -c '
import ctypes
import errno
import fcntl
import os
import socket
import stat
import subprocess
import sys

cutoff, dry_run_text, runner_root, btrfs_target_root = sys.argv[1:]
if dry_run_text not in ("0", "1"):
    print(f"invalid DRY_RUN value: {dry_run_text}", file=sys.stderr)
    raise SystemExit(2)
dry_run = dry_run_text == "1"

OPEN_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
PATH_FLAGS = os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC
RESOLVE_NO_XDEV = 0x01
RESOLVE_NO_MAGICLINKS = 0x02
RESOLVE_NO_SYMLINKS = 0x04
RESOLVE_BENEATH = 0x08
SYS_OPENAT2 = 437
RETIRED_XATTR = b"user.fcvm.retired"
XATTR_VERSION = b"v1"


class OpenHow(ctypes.Structure):
    _fields_ = [
        ("flags", ctypes.c_uint64),
        ("mode", ctypes.c_uint64),
        ("resolve", ctypes.c_uint64),
    ]


class MountBoundaryError(RuntimeError):
    pass


libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


def log(message):
    print(f"[disk-preflight] {message}", file=sys.stderr, flush=True)


def human(size):
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024 or unit == "TiB":
            return f"{value:.1f}{unit}"
        value /= 1024


def parse_cutoff(value):
    result = subprocess.run(
        ["/usr/bin/date", "-d", value, "+%s"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"invalid cutoff: {value}")
    return int(result.stdout.strip())


try:
    cutoff_epoch = parse_cutoff(cutoff)
except BaseException as error:
    log(f"ERROR: cannot parse target cutoff: {error}")
    raise SystemExit(2)


def open_beneath(parent_fd, name, flags):
    """Open one child atomically without symlinks, magic links, or mounts."""
    if not name or name in (".", "..") or "/" in name:
        raise OSError(errno.EINVAL, "invalid child name", name)
    how = OpenHow(
        flags=flags,
        mode=0,
        resolve=(
            RESOLVE_NO_XDEV
            | RESOLVE_NO_MAGICLINKS
            | RESOLVE_NO_SYMLINKS
            | RESOLVE_BENEATH
        ),
    )
    fd = libc.syscall(
        ctypes.c_long(SYS_OPENAT2),
        ctypes.c_int(parent_fd),
        ctypes.c_char_p(os.fsencode(name)),
        ctypes.byref(how),
        ctypes.sizeof(how),
    )
    if fd >= 0:
        return fd
    error = ctypes.get_errno()
    if error == errno.EXDEV:
        raise MountBoundaryError(f"mount boundary at child {name!r}")
    if error == errno.ENOSYS:
        raise RuntimeError("openat2 is unavailable; refusing unsafe target traversal")
    raise OSError(error, os.strerror(error), name)


def open_absolute_directory(path):
    """Open every component without following a symlink."""
    if not path or not os.path.isabs(path):
        raise OSError(errno.EINVAL, "directory root must be an absolute path", path)
    current = os.open("/", OPEN_FLAGS)
    try:
        for component in [part for part in path.split("/") if part]:
            next_fd = os.open(component, OPEN_FLAGS, dir_fd=current)
            os.close(current)
            current = next_fd
        return current
    except BaseException:
        os.close(current)
        raise


def open_child_directory(parent_fd, name):
    try:
        return open_beneath(parent_fd, name, OPEN_FLAGS)
    except OSError as error:
        if error.errno in (errno.ELOOP, errno.ENOENT, errno.ENOTDIR):
            return None
        raise


def open_child_path(parent_fd, name):
    try:
        return open_beneath(parent_fd, name, PATH_FLAGS)
    except OSError as error:
        if error.errno == errno.ENOENT:
            return None
        raise


def child_directory_names(parent_fd):
    # scandir returns names only. The subsequent O_NOFOLLOW open is the act of
    # discovery: from that point on the fd, not a reusable stat token, owns the
    # candidate identity.
    with os.scandir(parent_fd) as entries:
        return [entry.name for entry in entries]


def add_candidate(path, fd, candidates, managed):
    if not managed:
        candidates.append({"path": path, "fd": None, "state": "unmanaged"})
        os.close(fd)
        return
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        candidates.append({"path": path, "fd": None, "state": "busy"})
        os.close(fd)
        return
    except OSError:
        os.close(fd)
        raise
    candidates.append({"path": path, "fd": fd, "state": "locked"})


def enumerate_runner_root(path, candidates):
    root_fd = open_absolute_directory(path)
    try:
        for repo_name in child_directory_names(root_fd):
            repo_fd = open_child_directory(root_fd, repo_name)
            if repo_fd is None:
                continue
            try:
                for checkout_name in child_directory_names(repo_fd):
                    checkout_fd = open_child_directory(repo_fd, checkout_name)
                    if checkout_fd is None:
                        continue
                    try:
                        target_fd = open_child_directory(checkout_fd, "target")
                        if target_fd is not None:
                            add_candidate(
                                os.path.join(path, repo_name, checkout_name, "target"),
                                target_fd,
                                candidates,
                                False,
                            )
                    finally:
                        os.close(checkout_fd)
            finally:
                os.close(repo_fd)
    finally:
        os.close(root_fd)


def enumerate_btrfs_root(path, candidates):
    root_fd = open_absolute_directory(path)
    try:
        for name in child_directory_names(root_fd):
            candidate_fd = open_child_directory(root_fd, name)
            if candidate_fd is not None:
                add_candidate(os.path.join(path, name), candidate_fd, candidates, True)
    finally:
        os.close(root_fd)


def regular_key(metadata):
    return (metadata.st_dev, metadata.st_ino)


def record_regular(census, metadata, in_fingerprint):
    key = regular_key(metadata)
    record = census.get(key)
    if record is None:
        record = {
            "seen": 0,
            "fingerprint_seen": 0,
            "nlink": metadata.st_nlink,
            "mode": stat.S_IFMT(metadata.st_mode),
            "size": metadata.st_size,
            "blocks": metadata.st_blocks * 512,
            "mtime_ns": metadata.st_mtime_ns,
            "ctime_ns": metadata.st_ctime_ns,
        }
        census[key] = record
    elif (
        record["nlink"] != metadata.st_nlink
        or record["mode"] != stat.S_IFMT(metadata.st_mode)
        or record["size"] != metadata.st_size
        or record["mtime_ns"] != metadata.st_mtime_ns
        or record["ctime_ns"] != metadata.st_ctime_ns
    ):
        raise RuntimeError(f"regular inode changed during census: {key}")
    record["seen"] += 1
    if in_fingerprint:
        record["fingerprint_seen"] += 1


def inspect_tree(fd, census, in_fingerprint=False):
    """Return activity, apparent bytes, and retained metadata bytes."""
    active = False
    root_metadata = os.fstat(fd)
    size = root_metadata.st_size
    retained_metadata = root_metadata.st_blocks * 512
    for name in child_directory_names(fd):
        child_fd = open_child_directory(fd, name)
        if child_fd is not None:
            try:
                child_active, child_size, child_retained = inspect_tree(
                    child_fd,
                    census,
                    in_fingerprint or name == ".fingerprint",
                )
                active = active or child_active
                size += child_size
                retained_metadata += child_retained
            finally:
                os.close(child_fd)
            continue

        path_fd = open_child_path(fd, name)
        if path_fd is None:
            continue
        try:
            metadata = os.fstat(path_fd)
            if name == ".fingerprint":
                raise RuntimeError("Cargo .fingerprint entry is not a real directory")
            size += metadata.st_size
            if stat.S_ISREG(metadata.st_mode):
                record_regular(census, metadata, in_fingerprint)
                # A prior interrupted reclaim can leave a zero-length file with
                # a new timestamp between ftruncate and timestamp restoration.
                # Its zero size is the durable invalidation signal, not activity.
                if metadata.st_size > 0 and (
                    metadata.st_mtime > cutoff_epoch
                    or metadata.st_atime > cutoff_epoch
                ):
                    active = True
            else:
                if in_fingerprint:
                    raise RuntimeError(
                        f"nonregular entry inside Cargo .fingerprint: {name}"
                    )
                retained_metadata += metadata.st_blocks * 512
        finally:
            os.close(path_fd)
    return active, size, retained_metadata


def metadata_matches(record, metadata):
    return (
        record["nlink"] == metadata.st_nlink
        and record["mode"] == stat.S_IFMT(metadata.st_mode)
        and record["size"] == metadata.st_size
        and record["mtime_ns"] == metadata.st_mtime_ns
        and record["ctime_ns"] == metadata.st_ctime_ns
    )


def open_payload_writer(path_fd, record):
    writer = os.open(
        f"/proc/self/fd/{path_fd}",
        os.O_WRONLY | os.O_CLOEXEC,
    )
    metadata = os.fstat(writer)
    if regular_key(metadata) != regular_key(os.fstat(path_fd)) or not metadata_matches(
        record, metadata
    ):
        os.close(writer)
        raise RuntimeError("regular inode changed before payload truncation")
    return writer, metadata


def visit_regular_payloads(fd, census, fingerprint_phase, action):
    processed = set()

    def visit(directory_fd, in_fingerprint=False):
        for name in child_directory_names(directory_fd):
            child_fd = open_child_directory(directory_fd, name)
            if child_fd is not None:
                try:
                    visit(child_fd, in_fingerprint or name == ".fingerprint")
                finally:
                    os.close(child_fd)
                continue

            path_fd = open_child_path(directory_fd, name)
            if path_fd is None:
                raise RuntimeError(f"target entry disappeared during reclaim: {name}")
            try:
                metadata = os.fstat(path_fd)
                if name == ".fingerprint":
                    raise RuntimeError("Cargo .fingerprint entry is not a real directory")
                if not stat.S_ISREG(metadata.st_mode) or in_fingerprint != fingerprint_phase:
                    continue
                key = regular_key(metadata)
                if key in processed:
                    continue
                processed.add(key)
                record = census.get(key)
                if record is None or not metadata_matches(record, metadata):
                    raise RuntimeError(f"regular inode changed after census: {key}")
                action(path_fd, record, in_fingerprint)
            finally:
                os.close(path_fd)

    visit(fd)


def validate_fingerprint_payloads(fd, census):
    for key, record in census.items():
        if record["fingerprint_seen"] and (
            record["fingerprint_seen"] != record["seen"]
            or record["seen"] != record["nlink"]
        ):
            raise RuntimeError(
                f"fingerprint inode has an alias outside .fingerprint: {key}"
            )

    def validate(path_fd, record, _in_fingerprint):
        writer, _metadata = open_payload_writer(path_fd, record)
        os.close(writer)

    # Prove every fingerprint is still the censused writable inode before the
    # first one is invalidated. No payload is touched if this phase fails.
    visit_regular_payloads(fd, census, True, validate)


def truncate_payload_phase(fd, census, fingerprint_phase):
    reclaimed = 0
    skipped_external = 0

    def truncate(path_fd, record, in_fingerprint):
        nonlocal reclaimed, skipped_external
        if record["seen"] != record["nlink"]:
            if in_fingerprint:
                raise RuntimeError("fingerprint alias escaped prevalidation")
            skipped_external += record["blocks"]
            return

        writer, metadata = open_payload_writer(path_fd, record)
        try:
            os.ftruncate(writer, 0)
            os.utime(
                writer,
                ns=(metadata.st_atime_ns, metadata.st_mtime_ns),
            )
            if fingerprint_phase:
                # Payload truncation cannot become durable before its cache
                # invalidation. Persist every zero fingerprint before phase 2.
                os.fsync(writer)
            reclaimed += record["blocks"]
        finally:
            os.close(writer)

    visit_regular_payloads(fd, census, fingerprint_phase, truncate)
    return reclaimed, skipped_external


def reclaim_target(fd, census):
    # This xattr is the durable namespace retirement record. It is persisted
    # before any cache byte changes, so a crash can only leave an old generation
    # that the Cargo wrapper refuses to reuse. Xattrs avoid introducing or
    # replacing a dentry that could be a mountpoint in another namespace.
    try:
        retired = os.getxattr(fd, RETIRED_XATTR)
    except OSError as error:
        if error.errno not in (errno.ENODATA, getattr(errno, "ENOATTR", errno.ENODATA)):
            raise
        os.setxattr(fd, RETIRED_XATTR, XATTR_VERSION, flags=os.XATTR_CREATE)
    else:
        if retired != XATTR_VERSION:
            raise RuntimeError(f"unsupported retired-generation marker: {retired!r}")
    os.fsync(fd)

    validate_fingerprint_payloads(fd, census)
    fingerprint_reclaimed, _ = truncate_payload_phase(fd, census, True)
    if os.environ.get("FCVM_PRUNE_TEST_FAIL_AFTER_FINGERPRINTS") == "1":
        raise RuntimeError("injected failure after durable fingerprint invalidation")
    payload_reclaimed, skipped_external = truncate_payload_phase(fd, census, False)
    return fingerprint_reclaimed + payload_reclaimed, skipped_external


def unique_regular_blocks(census):
    reclaimable = 0
    skipped_external = 0
    for record in census.values():
        if record["seen"] == record["nlink"]:
            reclaimable += record["blocks"]
        else:
            skipped_external += record["blocks"]
    return reclaimable, skipped_external


def synchronize_test_after_enumeration():
    # runner-disk-preflight clears test controls at its privilege boundary.
    # Direct test
    # invocations use it only to make replacement/mount races deterministic.
    endpoint = os.environ.get("FCVM_PRUNE_TEST_SYNC_SOCKET")
    if not endpoint:
        return
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
        channel.connect(endpoint)
        channel.sendall(b"E")
        if channel.recv(1) != b"C":
            raise RuntimeError("test synchronization peer closed before continuation")


candidates = []
try:
    # No pruning occurs until every supplied root has been traversed and all
    # candidate fds/locks have been retained. A failure cannot authorize a
    # partial-view cleanup of the other root.
    if runner_root:
        enumerate_runner_root(runner_root, candidates)
    if btrfs_target_root:
        enumerate_btrfs_root(btrfs_target_root, candidates)
except MountBoundaryError as error:
    log(f"ERROR: refusing cargo target root containing a mount: {error}")
    raise SystemExit(50)
except BaseException as error:
    log(f"ERROR: cannot enumerate every cargo target root: {error}")
    raise SystemExit(51)

synchronize_test_after_enumeration()

# Inspect every retained candidate before the first mutation. openat2 checks
# mount identity at each fd-relative component resolution, so path renames
# cannot create a mountinfo/readlink ABA. The retained locked fd is the cleanup
# authority: if its pathname is replaced, the replacement is never opened or
# touched, while the originally authorized target remains safe to reclaim.
for candidate in candidates:
    fd = candidate["fd"]
    if fd is None:
        continue
    candidate_path = candidate["path"]
    try:
        census = {}
        active, size, retained_metadata = inspect_tree(fd, census)
    except MountBoundaryError as error:
        log(f"ERROR: refusing target containing an exact or descendant mount: {candidate_path} ({error})")
        raise SystemExit(50)
    except BaseException as error:
        log(f"ERROR: cannot inspect target safely: {candidate_path} ({error})")
        raise SystemExit(49)
    candidate["active"] = active
    candidate["size"] = size
    candidate["census"] = census
    candidate["retained_metadata"] = retained_metadata

considered = 0
for candidate in candidates:
    considered += 1
    path = candidate["path"]
    fd = candidate["fd"]
    if candidate["state"] == "unmanaged":
        log(f"  keeping (local target has no rotatable generation): {path}")
        continue
    if candidate["state"] == "busy":
        log(f"  keeping (concurrent cargo holds target lease): {path}")
        continue
    if candidate["active"]:
        log(f"  keeping (active within cutoff): {path}")
        continue

    try:
        # Rewalk under the retained exclusive lease immediately before reclaim.
        # The second census detects activity, topology, or mount changes before
        # the first fingerprint is invalidated.
        census = {}
        active, size, retained_metadata = inspect_tree(fd, census)
        if active:
            log(f"  keeping (became active before reclaim): {path}")
            continue
        reclaimable, skipped_external = unique_regular_blocks(census)
        if dry_run:
            log(
                f"  would reclaim: {path} ({human(reclaimable)} payload; "
                f"{human(skipped_external)} external-hardlink payload and "
                f"{human(retained_metadata)} metadata/nonregular retained)"
            )
        else:
            reclaimed, skipped_external = reclaim_target(fd, census)
            log(
                f"  reclaimed: {path} ({human(reclaimed)} payload; "
                f"{human(skipped_external)} external-hardlink payload and "
                f"{human(retained_metadata)} metadata/nonregular retained)"
            )
    except MountBoundaryError as error:
        log(f"ERROR: refusing target containing a mount at reclaim: {path} ({error})")
        raise SystemExit(50)
    except BaseException as error:
        log(f"ERROR: target cleanup failed for {path}: {error}")
        raise SystemExit(49)

log(
    f"[idle cargo target dirs] considered {considered} per-worktree targets; "
    "directories retained"
)
' "$@"
