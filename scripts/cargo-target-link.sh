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

BTRFS_ROOT="${BTRFS_ROOT:-/mnt/fcvm-btrfs}"

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

if [ -d "$BTRFS_ROOT" ]; then
	mkdir -p "$WT_TARGET" || { echo "ERROR: cannot create $WT_TARGET" >&2; exit 1; }
	# The disk pruner locks this directory exclusively.  Hold its shared lease
	# while wiring or populating it so setup cannot race cleanup before the Cargo
	# wrapper takes over the same lease.
	exec {wt_target_lease_fd}<"$WT_TARGET"
	flock -s "$wt_target_lease_fd"
	if [ -L target ] && [ "$(readlink target)" != "$WT_TARGET" ]; then
		echo "==> Repointing shared target/ → $WT_TARGET (per-worktree)"
		rm -f target
	fi
	if ! [ -L target ]; then
		if [ -d target ]; then
			# A Cargo process from an earlier invocation can still be using the
			# local target while btrfs comes online.  Take the directory lease
			# exclusively before migrating and removing that old lock inode.
			exec {local_target_lease_fd}<target
			flock -x "$local_target_lease_fd"
			echo "==> Moving existing target/ to $WT_TARGET..."
			# Never `rm -rf` the source on a partial move. A full volume, a
			# permission error, or a plain `target/*` glob (which skips
			# .rustc_info.json and other dotfiles) would otherwise discard build
			# artifacts silently. Move everything, dotfiles included, and abort if
			# any of it fails; `rmdir` then succeeds only if the move was complete.
			shopt -s dotglob nullglob
			entries=(target/*)
			shopt -u dotglob nullglob
			if [ ${#entries[@]} -gt 0 ]; then
				mv -- "${entries[@]}" "$WT_TARGET"/ || {
					echo "ERROR: could not migrate target/ to $WT_TARGET; leaving it in place" >&2
					exit 1
				}
			fi
			rmdir target || {
				echo "ERROR: target/ is not empty after migration; leaving it in place" >&2
				exit 1
			}
		fi
		ln -s "$WT_TARGET" target
		echo "==> Symlinked target/ → $WT_TARGET"
	fi
else
	echo "==> NOTE: $BTRFS_ROOT is not a directory; build artifacts stay on the root filesystem"
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

# Fail closed: leaving target/ unusable turns into an opaque cargo error several
# steps later, which is precisely how the original outage read.
[ -e target ] || mkdir -p target
if [ ! -d target ]; then
	echo "ERROR: target exists but is not a usable directory:" >&2
	ls -ld target >&2
	exit 1
fi
