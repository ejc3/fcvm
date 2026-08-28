#!/usr/bin/env bash
# Point ./target at this worktree's own build directory on btrfs, and guarantee
# that it resolves to a directory cargo can write into.
#
# Every build and test recipe runs cargo with CARGO_TARGET_DIR=target, so this
# runs first and its postcondition is the whole contract: after it returns 0,
# `target` IS a usable directory.
#
# Two properties this has to hold, both learned the hard way:
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

# "Is a directory" is not "is usable". On a GitHub-hosted runner /mnt/fcvm-btrfs
# does not exist, and podman CREATES it as an empty root-owned directory to
# satisfy CONTAINER_RUN_BASE's bind mount -- so inside the container the volume
# passes `-d` and fails `mkdir` (Permission denied), while the checkout's own
# target/ is a bind mount this script should simply keep. Testing `-d` alone
# took the managed branch and died before reaching its own "retaining unmanaged
# local target/" exit; every Weekly container-bench from 2026-08-10 on read
#     mkdir: cannot create directory '/mnt/fcvm-btrfs/cargo-target': Permission denied
# `-w` would not do either: root passes access(W_OK) on directories it still
# cannot create in (procfs, a read-only mount). Try the mkdir, and let its
# outcome decide -- loudly, because artifacts on the root filesystem are the
# thing this indirection exists to avoid and a reader should see it happen.
# Give up on the managed volume and leave a REAL local target/ behind.
#
# Loud, because artifacts on the root filesystem are the thing this indirection
# exists to avoid and a reader should see it happen. If target/ is a managed
# symlink it is REPLACED, not left: a link that still resolves would pass every
# later `-d target` check while Cargo cannot create a file there (raised on
# #867). Replacing it is safe here because this script holds the checkout lock
# (no Cargo runs in this checkout meanwhile) and, whenever target/ resolved,
# the exclusive lease on its generation taken below.
# Every exit that leaves target/ as a plain local directory ends here. "Is a
# directory" is not "is writable": procfs, a read-only mount, and (for a
# non-root user) a 0555 directory all pass `-d`, and an unwritable target/
# turns into an opaque cargo error several steps later, which is precisely
# how the original outage read. So create it if absent, then prove a file
# can be made in it.
require_writable_local_target() {
	[ -e target ] || mkdir -p target
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
}

# A managed target/ link is dropped, never probed through, on any path that
# holds no lease on the generation it points at: creating and removing a file
# inside an unleased generation is the census/rewalk race the pruner's lease
# protocol exists to prevent. Only the checkout's link goes; the generation is
# left for the pruner.
drop_managed_link() {
	[ -L target ] || return 0
	case "$(readlink target)" in
		"$BTRFS_ROOT"/cargo-target/*)
			echo "==> WARNING: dropping target/ → $(readlink target); no lease can be taken on it" >&2
			rm -f -- target ;;
	esac
}

fallback_to_local() {
	echo "==> WARNING: $1; build artifacts stay on the root filesystem" >&2
	drop_managed_link
	require_writable_local_target
	exit 0
}

# "Is a directory" is not "is usable": on a GitHub-hosted runner podman creates
# a missing /mnt/fcvm-btrfs as an empty root-owned directory to satisfy the
# bind mount, so the volume passes `-d` and the mkdir below fails. Whether an
# EXISTING directory can be written is settled later, under its lease -- see
# the write probe in the lease block. Probing here, before the generation is
# leased, is the race the pruner's census/rewalk cannot tolerate (raised on
# #867): an entry that appears after the census or vanishes during the rewalk
# aborts the hourly preflight.
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
old_target_lease_fd=""
if [ -L target ] && [ -d target ]; then
	linked="$(readlink target)"
	# A Cargo wrapper that already crossed the checkout→target lock handoff no
	# longer holds the checkout lease. Pin and exclusively lease its resolved
	# generation before publishing any different symlink target.
	exec {old_target_lease_fd}<target
	flock -x "$old_target_lease_fd"
	case "$linked" in
		"$WT_TARGET"|"$WT_TARGET".generation-*)
			candidate="$linked"
			;;
	esac
fi

mkdir -p -- "$candidate"
if [[ -n $old_target_lease_fd && ${linked:-} == "$candidate" ]]; then
	candidate_state_fd="$old_target_lease_fd"
else
	exec {candidate_lease_fd}<"$candidate"
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
# The write probe, under the exclusive lease: `mkdir -p` above is idempotent
# and says nothing about an EXISTING directory that has since gone read-only
# (ownership change, ro remount). Creating an entry is the operation Cargo
# needs, and the one root cannot fake past EROFS. It happens here, and only
# here, because the pruner holds LOCK_EX on a generation across its census and
# rewalk -- an entry created or removed outside that lock is exactly the
# "target entry disappeared during reclaim" abort.
if ! write_probe="$(mktemp -p "$candidate" .fcvm-write-probe.XXXXXXXX 2>/dev/null)"; then
	fallback_to_local "$candidate cannot be written"
fi
rm -f -- "$write_probe"
retired_rc=0
target_is_retired "$candidate_state_fd" || retired_rc=$?
if ((FORCE_ROTATE)); then
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
		# Downgrade the exclusive inspection lease while publishing the link.
		flock -s "$candidate_state_fd"
		final_lease_fd="$candidate_state_fd"
		;;
	*)
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
	if ((FORCE_ROTATE)) && [ -d target ]; then
		echo "ERROR: local target/ cannot be atomically rotated without the managed btrfs namespace; refusing unsafe clean" >&2
		exit 1
	fi
	if ! [ -d "$BTRFS_ROOT" ]; then
		echo "==> NOTE: $BTRFS_ROOT is not a directory; build artifacts stay on the root filesystem"
	fi
	# This branch never leases a generation, and target/ may still resolve into
	# one (a rotated generation outliving the canonical path the pruner removed).
	drop_managed_link
fi

# Self-heal a dangling link. `[ -d target ]` follows the symlink, so this is
# false exactly when it does not resolve.
if [ -L target ] && ! [ -d target ]; then
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

# Fail closed, for every path that reaches here (the managed link just
# published, the unusable-volume branch, a healed or dropped dangling link).
require_writable_local_target
