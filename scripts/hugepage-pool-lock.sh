#!/bin/bash
# Run a command while holding the host-global hugepage pool lock.
#
#   scripts/hugepage-pool-lock.sh <command...>
#
# The lock is $HUGEPAGE_POOL_LOCK (default /mnt/fcvm-btrfs/hugepage-pool.lock),
# the same file bench/chromium/reqbench.sh holds for a phase lifetime, so a
# bench phase already in flight and a `make setup-hugepages` from another
# checkout serialize on one inode. The data root is a sticky, world-writable
# directory owned by the box's user, and the lock has to be usable there by
# root and by unprivileged callers. Three rules:
#
# - Opened READ-ONLY, never with O_CREAT. fs.protected_regular refuses an
#   O_CREAT open of a file owned by someone else in a sticky world-writable
#   directory, and util-linux `flock <path>` and bash `<>` both use O_CREAT.
#   That is what a root-created lock did to every later unprivileged run
#   (`flock: cannot open lock file ...: Permission denied`). flock(2) does not
#   care about the open mode.
# - Created atomically, as root: a hard link fails if the name already exists,
#   so concurrent creators agree on one inode, and root is an owner every
#   caller's allowlist accepts. Privilege is spent only on the fixed shared
#   path, through fixed programs with quoted arguments; an overridden path
#   (HUGEPAGE_POOL_LOCK, a test knob) is created unprivileged or refused.
#   Nothing here chmods or chowns a path: `install -m` sets the mode on the
#   file it creates, and a planted symlink at that name is replaced, not
#   followed.
# - The entry is checked before AND after opening. A symlink, a non-regular
#   file, or a file owned by anyone but root, the invoking user, or the
#   directory's owner is refused: its owner could repoint or recreate it under
#   a holder, and a later caller would lock a different inode and write the
#   pool concurrently. After the open, the descriptor's inode must be the
#   path's own (lstat) inode, which closes the window between check and open.
#
# Not defended: the data root's owner. In a sticky directory that owner can
# unlink any entry, root-owned included, and could replace the lock under a
# holder. That account is the box's operator (it ran setup, owns the checkout,
# holds sudo on every box this runs on), and a lock cannot defend against its
# own operator; moving the lock out of the data root would change the identity
# in-flight reqbench.sh phases hold.
#
# The command runs as a CHILD while this script keeps fd 9: `exec` would hand
# the descriptor to the command, and sudo closes inherited descriptors, which
# would release the lock right before the privileged pool write.
set -eu
default_dir=/mnt/fcvm-btrfs
default_lock="$default_dir/hugepage-pool.lock"
lock="${HUGEPAGE_POOL_LOCK:-$default_lock}"
wait_s="${HUGEPAGE_POOL_LOCK_WAIT:-60}"
[ $# -gt 0 ] || { echo "usage: $0 <command...>" >&2; exit 2; }

refuse() {
	echo "hugepage-pool-lock: $*" >&2
	exit 1
}

# $1 = lock path. Runs as whoever can write the directory.
create_if_absent() {
	local tmp
	tmp="$(mktemp -u "$1.XXXXXX")"
	install -m 644 /dev/null "$tmp"
	ln "$tmp" "$1" 2>/dev/null || true
	rm -f "$tmp"
}

if ! [ -e "$lock" ] && ! [ -L "$lock" ]; then
	if [ "$lock" = "$default_lock" ]; then
		# The shared lock is created as ROOT: a stable owner every caller's
		# allowlist accepts, whoever runs first. Privilege is spent only here,
		# only on the fixed path, and only through fixed programs with quoted
		# arguments: no shell string, no caller-supplied path.
		tmp="$(mktemp -u -- "$default_lock.XXXXXXXX")"
		sudo mkdir -p -- "$default_dir"
		sudo install -m 644 -- /dev/null "$tmp"
		sudo ln -- "$tmp" "$default_lock" 2>/dev/null || true
		sudo rm -f -- "$tmp"
	else
		# An overridden path (HUGEPAGE_POOL_LOCK, a test knob) is never created
		# with privileges.
		dir="$(dirname -- "$lock")"
		if ! [ -d "$dir" ] || ! [ -w "$dir" ]; then
			refuse "$lock is not the shared lock and its directory is not writable; not creating it with privileges"
		fi
		create_if_absent "$lock"
	fi
fi

# The entry as it is on disk: stat without -L reports a symlink as itself.
if [ -L "$lock" ]; then
	refuse "$lock is a symlink (owner uid $(stat -c %u "$lock")); a path another user can repoint is not a lock"
fi
[ -f "$lock" ] || refuse "$lock is not a regular file"
owner="$(stat -c %u "$lock")"
dir_owner="$(stat -c %u "$(dirname "$lock")")"
case "$owner" in
0 | "$(id -u)" | "$dir_owner") ;;
*) refuse "$lock is owned by uid $owner, who can recreate it under a holder; refusing" ;;
esac

exec 9<"$lock"
if [ "$(stat -c %d:%i "$lock")" != "$(stat -Lc %d:%i "/proc/$$/fd/9")" ]; then
	refuse "$lock was replaced between check and open; refusing"
fi
if ! flock -x -w "$wait_s" 9; then
	refuse "$lock busy for ${wait_s}s; refusing to race the owner"
fi
"$@"
