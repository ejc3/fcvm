#!/usr/bin/env bash
# Hand root-owned content-addressed store entries back to the current user.
#
# A root-invoked `fcvm setup` that failed mid-build used to leave a root-owned
# store directory and 0600 lock file behind, after which every rootless setup
# on the same machine died with EACCES opening the lock (the Build Btrfs
# Kernel / Host-arm64 pair on runner i-0d8e07f6732c331cc). fcvm now hands
# entries back to the sudo invoker as it creates them; this script heals
# stores poisoned before that fix, covering the ordering fcvm cannot: a
# rootless job scheduled onto the machine before any root run touches the
# store again. It runs from `make setup-btrfs` and the kernels workflow
# bootstrap.
#
# Only the content-addressed asset stores are touched (the list mirrors
# src/paths.rs — keep them in sync). state/, vm-disks/, snapshots/,
# containers/ and image-cache/ are left alone: root-mode runs own entries
# there legitimately, and container storage carries subuid-mapped ownership
# that a blanket chown would destroy.
set -euo pipefail

STORE="${1:-/mnt/fcvm-btrfs}"
user="$(id -un)"
group="$(id -gn)"

# Nothing to hand back when the whole run is root (container CI).
[ "$user" = "root" ] && exit 0

dirs=()
for d in kernels pasta firecracker cloud-hypervisor initrd rootfs cache; do
    [ -d "$STORE/$d" ] && dirs+=("$STORE/$d")
done
[ "${#dirs[@]}" -eq 0 ] && exit 0

# One traversal as root; -h so a symlink is re-owned itself, never its target.
sudo find "${dirs[@]}" ! -user "$user" -exec chown -h "$user:$group" {} +
