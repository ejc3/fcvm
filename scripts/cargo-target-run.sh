#!/usr/bin/env bash
# Run one Cargo command while holding this worktree target's shared lease.
#
# runner-disk-preflight.sh locks the same directory inode exclusively before
# removing idle artifacts.  The target directory is permanent: deleting it
# while a process has the inode open would let a replacement directory become
# a second lock object and reintroduce the unlink/flock ABA race this protocol
# exists to prevent.
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
if [[ ! -d $target ]]; then
	echo "ERROR: CARGO_TARGET_DIR '$target' is not a usable directory; the Make target is missing its cargo-target-link prerequisite" >&2
	exit 1
fi

# Lock the target directory inode itself.  This works through the checkout's
# `target` symlink, requires no writable lock file, and gives the pruner a
# stable inode that also exists for targets created before this protocol.  The
# pruner must therefore retain the directory rather than rm -rf it.
exec {lease_fd}<"$target"
flock -s "$lease_fd"

# The descriptor intentionally survives exec, holding the shared lease for the
# complete Cargo process lifetime (including every rustc/linker child).
exec "$@"
