#!/bin/bash
# Run a command while holding the host-global hugepage pool lock.
#
#   scripts/hugepage-pool-lock.sh <command...>
#
# The lock is $HUGEPAGE_POOL_LOCK (default /mnt/fcvm-btrfs/hugepage-pool.lock),
# the same file bench/chromium/reqbench.sh holds for a phase lifetime, so a
# bench phase already in flight and a `make setup-hugepages` from another
# checkout serialize on one inode. Two rules make one file usable by root and
# by unprivileged callers in the same sticky, user-owned data root:
#
# - It is opened READ-ONLY, never with O_CREAT. fs.protected_regular refuses an
#   O_CREAT open of a file owned by someone else in a sticky world-writable
#   directory, root included, and util-linux `flock <path>` and bash `<>` both
#   use O_CREAT. That is what a root-created lock did to every later
#   unprivileged run (`flock: cannot open lock file ...: Permission denied`).
#   flock(2) does not care about the open mode.
# - It is created atomically when absent: a hard link fails if the name already
#   exists, so concurrent creators agree on one inode. Nothing here chmods or
#   chowns a path: `install -m` sets the mode on the file it creates, and a
#   planted symlink at that name is replaced, not followed.
#
# The command runs as a CHILD while this script keeps fd 9: `exec` would hand
# the descriptor to the command, and sudo closes inherited descriptors, which
# would release the lock right before the privileged pool write.
set -eu
lock="${HUGEPAGE_POOL_LOCK:-/mnt/fcvm-btrfs/hugepage-pool.lock}"
wait_s="${HUGEPAGE_POOL_LOCK_WAIT:-60}"
[ $# -gt 0 ] || { echo "usage: $0 <command...>" >&2; exit 2; }

# $1 = lock path. Runs as whoever can write the directory.
create_if_absent() {
	local tmp
	tmp="$(mktemp -u "$1.XXXXXX")"
	install -m 644 /dev/null "$tmp"
	ln "$tmp" "$1" 2>/dev/null || [ -e "$1" ]
	rm -f "$tmp"
}

if [ ! -e "$lock" ]; then
	dir="$(dirname "$lock")"
	if [ -d "$dir" ] && [ -w "$dir" ]; then
		create_if_absent "$lock"
	else
		sudo mkdir -p "$dir"
		sudo bash -c "$(declare -f create_if_absent); create_if_absent \"\$1\"" _ "$lock"
	fi
fi

exec 9<"$lock"
if ! flock -x -w "$wait_s" 9; then
	echo "hugepage-pool-lock: $lock busy for ${wait_s}s; refusing to race the owner" >&2
	exit 1
fi
"$@"
