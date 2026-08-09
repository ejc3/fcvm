#!/usr/bin/env bash
# Run one Cargo command while holding this worktree target's shared lease.
#
# runner-disk-preflight.sh locks the same physical generation exclusively
# before reclaiming idle payload. It retires that inode before changing cache
# bytes; this wrapper then asks cargo-target-link.sh to publish a fresh sibling
# generation. Physical target dentries are never deleted or renamed.
set -euo pipefail

if (($# == 0)); then
	echo "usage: $0 <cargo> [args...]" >&2
	exit 2
fi

if ! command -v flock >/dev/null 2>&1; then
	echo "ERROR: flock is required to lease the cargo target (util-linux)" >&2
	exit 2
fi

target="${CARGO_TARGET_DIR:-target}"
link_script="$(cd "$(dirname "$0")" && pwd -P)/cargo-target-link.sh"

target_is_retired() {
	local fd="$1" rc=0
	/usr/bin/python3 -c '
import errno
import os
import sys

try:
    value = os.getxattr(sys.argv[1], b"user.fcvm.retired")
except OSError as error:
    if error.errno in (errno.ENODATA, getattr(errno, "ENOATTR", errno.ENODATA)):
        raise SystemExit(3)
    raise
if value != b"v1":
    print(f"unsupported retired-generation marker: {value!r}", file=sys.stderr)
    raise SystemExit(4)
' "/proc/$$/fd/$fd" || rc=$?
	return "$rc"
}

while :; do
	# cargo-target-link.sh takes this same checkout-directory lock exclusively
	# before publishing target/. Hold it shared until the resolved generation fd
	# itself is leased, closing the stale-inode window without serializing Cargo
	# commands for their full lifetimes.
	checkout="$(pwd -P)"
	exec {checkout_lease_fd}<"$checkout"
	flock -s "$checkout_lease_fd"

	if [[ ! -d $target ]]; then
		echo "ERROR: CARGO_TARGET_DIR '$target' is not a usable directory; the Make target is missing its cargo-target-link prerequisite" >&2
		exit 1
	fi

	# Lock the resolved generation inode itself, then inspect its durable
	# retirement state while both the checkout name and inode are pinned.
	exec {lease_fd}<"$target"
	flock -s "$lease_fd"
	retired_rc=0
	target_is_retired "$lease_fd" || retired_rc=$?
	case "$retired_rc" in
		3)
			exec {checkout_lease_fd}<&-
			break
			;;
		0)
			exec {lease_fd}<&-
			exec {checkout_lease_fd}<&-
			if [[ $target != target ]]; then
				echo "ERROR: retired CARGO_TARGET_DIR '$target' is not the managed target/ path" >&2
				exit 1
			fi
			BTRFS_ROOT="${BTRFS_ROOT:-/mnt/fcvm-btrfs}" "$link_script"
			;;
		*)
			echo "ERROR: cannot read target retirement state (rc=$retired_rc)" >&2
			exit 1
			;;
	esac
done

# The target descriptor intentionally survives exec, holding the shared lease
# for the complete Cargo process lifetime (including every rustc/linker child).
exec "$@"
