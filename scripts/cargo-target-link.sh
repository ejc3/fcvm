#!/usr/bin/env bash
# Point ./target at this worktree's own build directory on btrfs, and guarantee
# that it resolves to a directory cargo can write into.
#
# Every build and test recipe runs cargo with CARGO_TARGET_DIR=target, so this
# runs first and its postcondition is the whole contract: after it returns 0,
# `target` IS a usable directory.
#
# Three properties this has to hold, the first two learned the hard way:
#
# 1. PER WORKTREE. Cargo names a test binary from a hash over package
#    name/version/features that does NOT include the checkout path, so every
#    fcvm worktree produces the same filename. Pointing them all at one
#    directory means `cargo test` in one worktree can run a binary another
#    worktree built. Observed 2026-08-08: a run in worktree A listed a test that
#    exists only in worktree B and silently omitted the one under test, which
#    makes red/green verification meaningless.
#
# 2. THE LINK MUST STILL RESOLVE. The btrfs volume is ephemeral —
#    scripts/runner-disk-preflight.sh prunes idle cargo-target dirs, and a runner
#    can come back with the volume reset — while a CI checkout persists across
#    jobs. Cargo does not create the directory through a dangling symlink: it
#    builds a tempdir and rename()s it onto the path, and rename() onto an
#    existing symlink is ENOTDIR. That is the "failed to create directory ...
#    Not a directory (os error 20)" that took out Host-arm64 on every open PR on
#    2026-08-08. `~/.cargo` in the Makefile's check-disk target already
#    self-heals for exactly this reason; target/ did not.
#
# 3. A FALLBACK MUST BE REVERSIBLE, AND MUST NOT SURVIVE ITS OUTAGE. A real
#    target/ the script did not create is retained for good (its dentry may
#    carry a mount visible only in another mount namespace), so a fallback
#    shaped as a real target/ would keep every later build on the root
#    filesystem after the volume returned. The fallback is a symlink to a
#    checkout-local payload, replaced like any other published link once the
#    volume is usable again. Each activation publishes its OWN payload: a name
#    the script stops publishing is never chosen again, so a later outage
#    cannot resurrect a cargo cache that a `--rotate` in between reported gone.
#
# Everything below runs under an exclusive flock on the checkout directory,
# because more than one `make` can run in one checkout (several CI entry points
# depend on this target, and developers do it routinely). Without it, two
# processes can both observe the dangling link, the first replaces it with a real
# directory, and the second's `rm` then fails with "Is a directory" and kills a
# build for no visible reason.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=scripts/cargo-target-lib.sh
. "$script_dir/cargo-target-lib.sh"

BTRFS_ROOT="${BTRFS_ROOT:-/mnt/fcvm-btrfs}"
FORCE_ROTATE=0
case "$#" in
	0) ;;
	1)
		if [[ $1 != --rotate ]]; then
			echo "usage: $0 [--rotate]" >&2
			exit 2
		fi
		FORCE_ROTATE=1
		;;
	*)
		echo "usage: $0 [--rotate]" >&2
		exit 2
		;;
esac

# Serialization is what makes the sequence below safe; running without it would
# reintroduce the race silently. Fail loudly instead.
if ! command -v flock >/dev/null 2>&1; then
	echo "ERROR: flock is required to serialize target/ setup (util-linux)" >&2
	exit 2
fi

# Re-exec under an exclusive lock on the checkout directory. No lock FILE: a
# directory fd is lockable, needs no cleanup, and cannot itself become the stale
# artifact we are trying to avoid.
if [ "${CARGO_TARGET_LINK_LOCKED:-}" != "1" ]; then
	export CARGO_TARGET_LINK_LOCKED=1
	exec flock "$(pwd -P)" "$0" "$@"
fi

# Per-worktree directory name: sanitized basename + a hash of the absolute path,
# so two worktrees that share a basename still get separate directories. Derived
# from `pwd -P` rather than interpolated by make, so a checkout path containing a
# quote cannot inject shell syntax, and the basename is restricted to
# [A-Za-z0-9._-] so it cannot introduce path separators.
p="$(pwd -P)"
name="$(printf '%s' "$(basename "$p")" | LC_ALL=C tr -c 'A-Za-z0-9._-' '_')"
hash="$(printf '%s' "$p" | sha256sum | cut -c1-8)"
WT_TARGET="$BTRFS_ROOT/cargo-target/$name-$hash"
# Fallback payloads sit beside the checkout and are published through target/
# as a symlink, never as a real target/ dentry. mktemp names each one, so a
# payload is reachable only while target/ names it.
LOCAL_TARGET_PREFIX="$p/.cargo-target-local"

# The fallback payload target/ publishes right now, on stdout. Non-zero when
# target/ publishes something else.
published_fallback() {
	[ -L target ] || return 1
	local linked
	linked="$(readlink target)"
	case "$linked" in
		"$LOCAL_TARGET_PREFIX".generation-*) printf '%s' "$linked" ;;
		*) return 1 ;;
	esac
}

# True while any fallback payload exists, published or not.
any_fallback_payload() {
	local entry
	for entry in "$LOCAL_TARGET_PREFIX".generation-*; do
		[ -e "$entry" ] && return 0
	done
	return 1
}

# Reclaim every fallback payload except the one target/ publishes. An
# unpublished payload is unreachable (nothing but target/ ever names one) and
# it sits on the root filesystem this indirection exists to keep free, while
# nothing else reclaims it: the pruner enumerates a checkout's target/ and the
# btrfs generations, never a checkout's other entries. A payload another build
# still leases is kept, and stays unreachable.
#
# Removal is destructive, so the walk is fd-relative and bounded. openat2 with
# RESOLVE_BENEATH|NO_XDEV|NO_SYMLINKS refuses to leave the payload and refuses
# to descend into a mount; a payload sits in the checkout for the length of an
# outage, and the dentry of a mount placed under it can be the only reference
# another namespace has to that data. A child whose device differs is the same
# refusal for a file bind mount, which opens like an ordinary file.
#
# A payload owned by another uid is never removed. The container lane runs as
# root against the same bind-mounted checkout, so a root-owned payload beside a
# user-owned one is an ordinary state, and root is not stopped by permissions.
#
# Every outcome is named. Failing the run is reserved for a payload this
# identity owns and could not reclaim: nothing else enumerates it and its name
# is never published again, so no later run clears it either, and each outage
# cycle would leave one more tree on the root filesystem.
discard_unpublished_fallbacks() {
	local keep="${1:-}"
	/usr/bin/python3 -c '
import ctypes
import errno
import fcntl
import os
import stat
import sys

checkout, prefix, keep = sys.argv[1:4]

RESOLVE_NO_XDEV = 0x01
RESOLVE_NO_MAGICLINKS = 0x02
RESOLVE_NO_SYMLINKS = 0x04
RESOLVE_BENEATH = 0x08
SYS_OPENAT2 = 437
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


class OpenHow(ctypes.Structure):
    _fields_ = [
        ("flags", ctypes.c_uint64),
        ("mode", ctypes.c_uint64),
        ("resolve", ctypes.c_uint64),
    ]


class MountBoundary(RuntimeError):
    pass


def open_beneath(parent_fd, name):
    how = OpenHow(
        flags=DIR_FLAGS,
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
    code = ctypes.get_errno()
    if code == errno.EXDEV:
        raise MountBoundary(f"{name} is a mount point")
    if code == errno.ENOSYS:
        raise RuntimeError("openat2 is unavailable; refusing an unbounded removal")
    raise OSError(code, os.strerror(code), name)


def prune(directory_fd, device):
    for name in os.listdir(directory_fd):
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if info.st_dev != device:
            raise MountBoundary(f"{name} crosses a mount boundary")
        if stat.S_ISDIR(info.st_mode):
            child_fd = open_beneath(directory_fd, name)
            try:
                prune(child_fd, device)
            finally:
                os.close(child_fd)
            os.rmdir(name, dir_fd=directory_fd)
        else:
            os.unlink(name, dir_fd=directory_fd)


def report(message):
    print(message, file=sys.stderr, flush=True)


failed = False
checkout_fd = os.open(checkout, DIR_FLAGS)
try:
    for name in sorted(os.listdir(checkout_fd)):
        if not name.startswith(prefix):
            continue
        path = os.path.join(checkout, name)
        if path == keep:
            continue
        try:
            info = os.stat(name, dir_fd=checkout_fd, follow_symlinks=False)
        except OSError as error:
            report(f"==> ERROR: cannot stat the fallback payload {path}: {error.strerror}")
            failed = True
            continue
        if not stat.S_ISDIR(info.st_mode):
            report(f"==> ERROR: the fallback payload {path} is not a directory; not reclaimed")
            failed = True
            continue
        if info.st_uid != os.geteuid():
            report(
                f"==> WARNING: the fallback payload {path} is owned by uid {info.st_uid}, not "
                f"{os.geteuid()}; it is not this identity to reclaim and stays on the root "
                f"filesystem"
            )
            continue
        try:
            payload_fd = open_beneath(checkout_fd, name)
        except MountBoundary as error:
            report(f"==> ERROR: refusing to reclaim the fallback payload {path}: {error}")
            failed = True
            continue
        except OSError as error:
            report(
                f"==> ERROR: cannot open the fallback payload {path} to reclaim it: "
                f"{error.strerror}"
            )
            failed = True
            continue
        try:
            try:
                fcntl.flock(payload_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                report(f"==> WARNING: {path} is leased by another build and is not reclaimed")
                continue
            payload = os.fstat(payload_fd)
            prune(payload_fd, payload.st_dev)
            final = os.stat(name, dir_fd=checkout_fd, follow_symlinks=False)
            if (final.st_dev, final.st_ino) != (payload.st_dev, payload.st_ino):
                raise RuntimeError("the payload name changed inode while it was reclaimed")
            os.rmdir(name, dir_fd=checkout_fd)
        except (MountBoundary, OSError, RuntimeError) as error:
            report(f"==> ERROR: cannot reclaim the fallback payload {path}: {error}")
            failed = True
        else:
            report(f"==> Reclaimed the unpublished fallback payload {path}")
        finally:
            os.close(payload_fd)
finally:
    os.close(checkout_fd)
raise SystemExit(1 if failed else 0)
' "$p" "$(basename -- "$LOCAL_TARGET_PREFIX").generation-" "$keep"
}

# What does `target` resolve to, with the errno preserved? `[ -d target ]`
# reports the same false for "nothing is published there" and for "resolving it
# was refused", and only the first makes replacing the link safe: a build past
# the checkout lock holds nothing but its generation lease, so dropping the
# pathname without taking that lease splits it across two trees. Exit 0 is a
# directory, 3 is proven absent (ENOENT/ENOTDIR), and every other status is a
# resolution failure the caller must refuse on.
target_resolution_state() {
	local rc=0
	/usr/bin/python3 -c '
import errno
import os
import stat
import sys

try:
    metadata = os.stat(sys.argv[1])
except OSError as error:
    if error.errno in (errno.ENOENT, errno.ENOTDIR):
        raise SystemExit(3)
    print(f"cannot resolve {sys.argv[1]}: {os.strerror(error.errno)}", file=sys.stderr)
    raise SystemExit(4)
if not stat.S_ISDIR(metadata.st_mode):
    print(f"{sys.argv[1]} resolves to a non-directory", file=sys.stderr)
    raise SystemExit(5)
' "$1" || rc=$?
	return "$rc"
}

publish_target_link() {
	local destination="$1" staging
	staging="$(mktemp -d -- "$p/.fcvm-target-link.XXXXXXXX")"
	ln -s -- "$destination" "$staging/target"
	# The checkout lock excludes every cooperating resolver. Replacing only
	# this symlink is atomic and never renames a physical target dentry (which
	# could move a mount that exists solely in another namespace).
	mv -Tf -- "$staging/target" target
	rmdir -- "$staging"
}

# Every exit that leaves target/ on the root filesystem proves a file can be
# created in it first. `-d` passes on procfs, a read-only mount, and a 0555
# directory. `-w` is no better for root, which passes access(W_OK) on a
# directory it still cannot create entries in. Creating an entry is the
# operation cargo needs, and the only test of it. When nothing is published,
# the fallback is a link to a payload created here; a real target/ dentry is
# never created, because every later run would retain it as unmanaged.
require_writable_local_target() {
	local payload
	payload="$(published_fallback)" || payload=""
	if [ -n "$payload" ]; then
		# A fallback link whose payload was removed keeps its name and gets an
		# empty payload back.
		if ! mkdir -p -- "$payload"; then
			echo "ERROR: cannot recreate the published fallback payload $payload:" >&2
			ls -ld -- "$payload" "$p" >&2 2>/dev/null || true
			exit 1
		fi
	else
		# A managed link was already dropped by the caller under its lease. An
		# unmanaged dangling link is dropped here.
		if [ -L target ] && ! [ -e target ]; then
			echo "==> WARNING: dropping dangling target/ → $(readlink target); the local fallback takes its place" >&2
			rm -f -- target
		fi
		if ! [ -e target ]; then
			# A fresh payload, never one an earlier activation published: that
			# one holds artifacts a `--rotate` in between reported gone.
			if ! payload="$(mktemp -d -- "$LOCAL_TARGET_PREFIX.generation-XXXXXXXX")"; then
				echo "ERROR: cannot create a local fallback payload beside $LOCAL_TARGET_PREFIX:" >&2
				ls -ld -- "$p" >&2 2>/dev/null || true
				exit 1
			fi
			publish_target_link "$payload"
			echo "==> Symlinked target/ → $payload (local fallback)" >&2
		fi
	fi
	if [ ! -d target ]; then
		echo "ERROR: target exists but is not a usable directory:" >&2
		ls -ld target >&2
		exit 1
	fi
	local probe
	if ! probe="$(mktemp -p target .cargo-target-link-probe.XXXXXXXX 2>/dev/null)"; then
		echo "ERROR: target/ is a directory nothing can write; cargo cannot use it:" >&2
		ls -ld target >&2
		exit 1
	fi
	rm -f -- "$probe"
	if ! discard_unpublished_fallbacks "$payload"; then
		echo "ERROR: a fallback payload this run owns could not be reclaimed; nothing else \
enumerates it and its name is never published again, so it stays on the root filesystem until \
it is removed by hand" >&2
		exit 1
	fi
}

# Drops the checkout's managed target/ link; the generation stays for the
# pruner. A resolving link is dropped only under an exclusive lease on the
# directory it publishes: a cargo wrapper may hold that lease shared for the
# life of its build, and removing the pathname under it splits the build across
# two trees. A directory that cannot be opened cannot be leased, so the drop is
# refused. The lease fd stays open until exit. A dangling link publishes
# nothing and is dropped without one. The script's own fallback link is leased
# the same way and kept, since it already publishes the directory the fallback
# uses.
drop_managed_link() {
	[ -L target ] || return 0
	local linked
	linked="$(readlink target)"
	case "$linked" in
		"$BTRFS_ROOT"/cargo-target/* | "$LOCAL_TARGET_PREFIX".generation-*) ;;
		*) return 0 ;;
	esac
	if [[ -z ${old_target_lease_fd:-} ]]; then
		local resolution=0
		target_resolution_state target || resolution=$?
		case "$resolution" in
			0)
				if ! exec {old_target_lease_fd}<target; then
					echo "ERROR: cannot open the published directory $linked to lease it; a running build may still hold it, refusing to replace target/" >&2
					exit 1
				fi
				flock -x "$old_target_lease_fd"
				;;
			3) ;;
			*)
				echo "ERROR: cannot tell whether target/ → $linked still publishes a directory (rc=$resolution); a running build may hold it, refusing to replace target/" >&2
				exit 1
				;;
		esac
	fi
	case "$linked" in
		"$LOCAL_TARGET_PREFIX".generation-*) return 0 ;;
	esac
	echo "==> WARNING: dropping target/ → $linked; its volume cannot be used" >&2
	rm -f -- target
}

# True while every generation this checkout could publish is durably retired
# or proven absent. Dropping a link is not a clean: the generation keeps its
# payload, and the next run reuses an unretired one and republishes every byte
# the clean reported gone. A generation whose state cannot be read is refused
# for the same reason.
managed_namespace_is_retired() {
	local entry state fd rc
	for entry in "$WT_TARGET" "$WT_TARGET".generation-*; do
		state=0
		target_resolution_state "$entry" || state=$?
		case "$state" in
			3) continue ;;
			0) ;;
			*)
				echo "ERROR: cannot tell whether $entry still holds a payload (rc=$state)" >&2
				return 1
				;;
		esac
		if ! exec {fd}<"$entry"; then
			echo "ERROR: cannot open $entry to read its retirement state" >&2
			return 1
		fi
		rc=0
		target_is_retired "$fd" || rc=$?
		exec {fd}<&-
		case "$rc" in
			0) ;;
			*)
				echo "ERROR: $entry keeps its payload and is not retired (rc=$rc)" >&2
				return 1
				;;
		esac
	done
	return 0
}

# `--rotate` promises a clean target/. A retained link or directory, a
# republished fallback payload, or a managed generation that was never retired
# would each report one while its payload survives.
refuse_unsafe_rotation() {
	((FORCE_ROTATE)) || return 0
	if [ -L target ] || [ -d target ] || any_fallback_payload || ! managed_namespace_is_retired; then
		echo "ERROR: local target/ cannot be atomically rotated without the managed btrfs namespace; refusing unsafe clean" >&2
		exit 1
	fi
}

fallback_to_local() {
	echo "==> WARNING: $1; build artifacts stay on the root filesystem" >&2
	drop_managed_link
	refuse_unsafe_rotation
	require_writable_local_target
	exit 0
}

# `mkdir -p` returns 0 on an existing directory without proving it can be
# written; that is settled under the generation's lease, at the reuse probe.
old_target_lease_fd=""
btrfs_usable=0
if [ -d "$BTRFS_ROOT" ]; then
	if mkdir -p "$WT_TARGET" 2>/dev/null; then
		btrfs_usable=1
	else
		echo "==> WARNING: $BTRFS_ROOT exists but $WT_TARGET cannot be created; build artifacts stay on the root filesystem" >&2
	fi
fi
if ((btrfs_usable)); then

retire_target() {
	local fd="$1"
	/usr/bin/python3 -c '
import errno
import os
import sys

directory_fd = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY)
try:
    try:
        value = os.getxattr(directory_fd, b"user.fcvm.retired")
    except OSError as error:
        if error.errno not in (errno.ENODATA, getattr(errno, "ENOATTR", errno.ENODATA)):
            raise
        os.setxattr(
            directory_fd,
            b"user.fcvm.retired",
            b"v1",
            flags=os.XATTR_CREATE,
        )
    else:
        if value != b"v1":
            raise RuntimeError(f"unsupported retired-generation marker: {value!r}")
    os.fsync(directory_fd)
finally:
    os.close(directory_fd)
' "/proc/$$/fd/$fd"
}

new_generation() {
	local retired_rc
	while :; do
		FRESH_GENERATION="$(mktemp -d -- "${WT_TARGET}.generation-XXXXXXXX" 2>/dev/null)" \
			|| fallback_to_local "cannot create a fresh generation beside $WT_TARGET"
		exec {fresh_lease_fd}<"$FRESH_GENERATION"
		flock -x "$fresh_lease_fd"
		retired_rc=0
		target_is_retired "$fresh_lease_fd" || retired_rc=$?
		case "$retired_rc" in
			0)
				# The pruner acquired the new name between mkdir and flock. It
				# retired that physical inode, so leave it untouched and try a
				# different immutable sibling.
				exec {fresh_lease_fd}<&-
				;;
			3)
				break
				;;
			*)
				echo "ERROR: cannot initialize target generation $FRESH_GENERATION (rc=$retired_rc)" >&2
				exit 1
				;;
		esac
	done

	# Initialize through the locked fd. A fresh generation must not qualify as
	# idle in the link-to-wrapper gap; later Cargo writes provide the normal
	# activity signal. Persist the sentinel before publishing the symlink.
	/usr/bin/python3 -c '
import os
import sys

directory_fd = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY)
try:
    sentinel_fd = os.open(
        ".fcvm-generation",
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
        0o600,
        dir_fd=directory_fd,
    )
    try:
        os.write(sentinel_fd, b"v1\n")
        os.fsync(sentinel_fd)
    finally:
        os.close(sentinel_fd)
    os.fsync(directory_fd)
finally:
    os.close(directory_fd)
' "/proc/$$/fd/$fresh_lease_fd"
	flock -s "$fresh_lease_fd"
}

mkdir -p -- "$(dirname "$WT_TARGET")"

# A pre-protocol real target cannot be replaced safely: another mount
# namespace may have a mount on that exact dentry. Keep it local and unmanaged;
# the disk guard will fail its hard floor and quarantine the runner rather than
# corrupt a hidden mount. Fresh checkouts always take the managed-symlink path.
if [ -e target ] && ! [ -L target ]; then
	if [ ! -d target ]; then
		echo "ERROR: target exists but is not a usable directory:" >&2
		ls -ld target >&2
		exit 1
	fi
	if ((FORCE_ROTATE)); then
		echo "ERROR: unmanaged local target/ cannot be atomically rotated; refusing unsafe clean" >&2
		exit 1
	fi
	require_writable_local_target
	echo "==> WARNING: retaining unmanaged local target/; it cannot be atomically rotated" >&2
	exit 0
fi

candidate="$WT_TARGET"
if [ -L target ]; then
	linked="$(readlink target)"
	# target/ is replaced only under an exclusive lease on the generation it
	# publishes: a cargo wrapper past the checkout→target lock handoff holds
	# only that lease, shared, and the flock below blocks until it is done. A
	# generation that cannot be opened cannot be leased, so replacing it is
	# refused; a 0333 generation still accepts cargo's writes. A link that does
	# not resolve publishes nothing and needs no lease, but only a PROVEN absent
	# destination says so; a refused resolution is not the same answer.
	resolution=0
	target_resolution_state target || resolution=$?
	case "$resolution" in
		0)
			if ! exec {old_target_lease_fd}<target; then
				echo "ERROR: cannot open the published generation $linked to lease it; a running build may still hold it, refusing to replace target/" >&2
				exit 1
			fi
			flock -x "$old_target_lease_fd"
			case "$linked" in
				"$WT_TARGET"|"$WT_TARGET".generation-*)
					candidate="$linked"
					;;
			esac
			;;
		3) ;;
		*)
			echo "ERROR: cannot tell whether target/ → $linked still publishes a directory (rc=$resolution); a running build may hold it, refusing to replace target/" >&2
			exit 1
			;;
	esac
fi

mkdir -p -- "$candidate"
if [[ -n $old_target_lease_fd && ${linked:-} == "$candidate" ]]; then
	candidate_state_fd="$old_target_lease_fd"
else
	exec {candidate_lease_fd}<"$candidate" ||
		fallback_to_local "$candidate cannot be opened for its lease"
	if [[ -n $old_target_lease_fd ]] &&
		[[ "$(stat -Lc '%d:%i' -- "/proc/$$/fd/$old_target_lease_fd")" == \
		"$(stat -Lc '%d:%i' -- "/proc/$$/fd/$candidate_lease_fd")" ]]; then
		exec {candidate_lease_fd}<&-
		candidate_lease_fd=""
		candidate_state_fd="$old_target_lease_fd"
	else
		flock -x "$candidate_lease_fd"
		candidate_state_fd="$candidate_lease_fd"
	fi
fi
retired_rc=0
target_is_retired "$candidate_state_fd" || retired_rc=$?
if ((FORCE_ROTATE)) && [ "$retired_rc" != 0 ]; then
	# Retiring writes a marker into the candidate, so an unwritable one falls
	# back, and the fallback refuses under --rotate. An already retired
	# candidate needs no marker.
	if ! write_probe="$(mktemp -p "$candidate" .fcvm-write-probe.XXXXXXXX 2>/dev/null)"; then
		fallback_to_local "$candidate cannot be written"
	fi
	rm -f -- "$write_probe"
	retire_target "$candidate_state_fd"
	retired_rc=0
fi
case "$retired_rc" in
	0)
		new_generation
		candidate="$FRESH_GENERATION"
		final_lease_fd="$fresh_lease_fd"
		echo "==> Rotating retired target/ → $candidate"
		;;
	3)
		# A generation is probed for writability only when it is reused, and
		# only under its exclusive lease: the pruner holds LOCK_EX across its
		# census and rewalk, and an entry created or removed outside the lease
		# aborts it. A retired candidate is never probed; new_generation needs
		# only the parent writable.
		if ! write_probe="$(mktemp -p "$candidate" .fcvm-write-probe.XXXXXXXX 2>/dev/null)"; then
			fallback_to_local "$candidate cannot be written"
		fi
		rm -f -- "$write_probe"
		# Downgrade the exclusive inspection lease while publishing the link.
		flock -s "$candidate_state_fd"
		final_lease_fd="$candidate_state_fd"
		;;
	*)
		# An unreadable marker on an unwritable directory is an unusable
		# volume; on a writable one it is a protocol error.
		if ! write_probe="$(mktemp -p "$candidate" .fcvm-write-probe.XXXXXXXX 2>/dev/null)"; then
			fallback_to_local "$candidate cannot be written"
		fi
		rm -f -- "$write_probe"
		echo "ERROR: cannot read target retirement state for $candidate (rc=$retired_rc)" >&2
		exit 1
		;;
esac

if ! [ -L target ] || [ "$(readlink target)" != "$candidate" ]; then
	publish_target_link "$candidate"
	echo "==> Symlinked target/ → $candidate"
fi

# Keep only the published generation's shared lease until script exit. In
# particular, the old resolved target stays exclusively pinned through the
# atomic symlink switch above.
if [[ -n $old_target_lease_fd && $old_target_lease_fd != "$final_lease_fd" ]]; then
	exec {old_target_lease_fd}<&-
fi
if [[ -n ${candidate_lease_fd:-} && $candidate_lease_fd != "$final_lease_fd" ]]; then
	exec {candidate_lease_fd}<&-
fi
else
	refuse_unsafe_rotation
	if ! [ -d "$BTRFS_ROOT" ]; then
		echo "==> NOTE: $BTRFS_ROOT is not a directory; build artifacts stay on the root filesystem"
	fi
	# target/ may still resolve into a generation that outlived its canonical
	# path; drop_managed_link leases it before dropping the link.
	drop_managed_link
fi

# Self-heal a dangling link. `[ -d target ]` follows the symlink, so this is
# false exactly when it does not resolve. A dangling fallback link is left for
# require_writable_local_target, which recreates its payload.
if [ -L target ] && ! [ -d target ] && ! published_fallback >/dev/null; then
	TGT="$(readlink target)"
	if ! [ -d "$BTRFS_ROOT" ]; then
		# The VOLUME is gone, not just our directory on it. Recreating the path
		# would silently write build artifacts under an unmounted mountpoint —
		# i.e. onto the small root filesystem, the exact thing this indirection
		# exists to avoid — while still looking like btrfs. Drop the link and let
		# the fail-closed step below make a real local directory.
		echo "==> WARNING: $BTRFS_ROOT is not mounted; using a local target/ on the root filesystem"
		rm -f target
	elif mkdir -p "$TGT" 2>/dev/null; then
		echo "==> target/ → $TGT was dangling (pruned?); recreated"
	else
		echo "==> WARNING: cannot recreate $TGT; using a local target/ on the root filesystem"
		rm -f target
	fi
fi

# Fail closed on every path that reaches here.
require_writable_local_target
