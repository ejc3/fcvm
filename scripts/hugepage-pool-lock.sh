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
# - Created atomically: a hard link fails if the name already exists, so
#   concurrent creators agree on one inode. The invoking user creates it when
#   the directory is writable, root (sudo) only on a fresh box whose data root
#   is still root-owned. Nothing here chmods or chowns a path: `install -m`
#   sets the mode on the file it creates, and a planted symlink at that name
#   is replaced, not followed.
# - The entry is checked before AND after opening. A symlink, a non-regular
#   file, or a file owned by anyone but root, the invoking user, or the
#   directory's owner is refused: its owner could repoint or recreate it under
#   a holder, and a later caller would lock a different inode and write the
#   pool concurrently. After the open, the descriptor's inode must be the
#   path's own (lstat) inode, which closes the window between check and open.
#
# The command runs as a CHILD while this script keeps fd 9: `exec` would hand
# the descriptor to the command, and sudo closes inherited descriptors, which
# would release the lock right before the privileged pool write.
set -eu
lock="${HUGEPAGE_POOL_LOCK:-/mnt/fcvm-btrfs/hugepage-pool.lock}"
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
	dir="$(dirname "$lock")"
	if [ -d "$dir" ] && [ -w "$dir" ]; then
		create_if_absent "$lock"
	else
		# A fresh box: the data root is root-owned until setup hands it over.
		sudo mkdir -p "$dir"
		sudo bash -c "$(declare -f create_if_absent); create_if_absent \"\$1\"" _ "$lock"
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
