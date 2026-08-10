#!/usr/bin/env bash
# Reflink a golden snapshot into a PRIVATE tag that only this run knows about.
#
# Why: /mnt/fcvm-btrfs/snapshots is shared with every other agent on this box, and a
# concurrent `fcvm snapshots delete` or a golden rebuild part-way through a profile
# destroys the run silently — the clones that already restored keep working, the ones
# after it fail, and the medians end up computed over two different memory images.
# A reflink copy is CoW: ~10 ms, no extra space, and nothing outside this run refers to it.
#
# config.json carries ABSOLUTE paths to memory.bin/vmstate.bin/disk.raw, so a bare
# `cp --reflink` produces a snapshot that still reads the ORIGINAL files — i.e. it looks
# private and is not. The paths and the name are rewritten here, and then verified.
set -euo pipefail

SNAP_DIR=/mnt/fcvm-btrfs/snapshots
SRC=${1:?usage: mkprivate.sh <source-tag> [dest-tag]}
DST=${2:-${SRC%%-*}-private-$$}

[ -d "$SNAP_DIR/$SRC" ] || { echo "no such snapshot: $SRC" >&2; exit 1; }
[ -e "$SNAP_DIR/$DST" ] && { echo "destination exists: $DST" >&2; exit 1; }

mkdir -p "$SNAP_DIR/$DST"
for f in memory.bin vmstate.bin disk.raw; do
    cp --reflink=always "$SNAP_DIR/$SRC/$f" "$SNAP_DIR/$DST/$f"
done

jq --arg d "$SNAP_DIR/$DST" --arg n "$DST" '
    .name = $n
  | .memory_path  = ($d + "/memory.bin")
  | .vmstate_path = ($d + "/vmstate.bin")
  | .disk_path    = ($d + "/disk.raw")
' "$SNAP_DIR/$SRC/config.json" > "$SNAP_DIR/$DST/config.json"

# Verify, never assume: every path must point INSIDE the private dir, and the
# port mappings that make a clone reachable must have survived the rewrite.
jq -e --arg d "$SNAP_DIR/$DST" '
    (.memory_path  | startswith($d))
and (.vmstate_path | startswith($d))
and (.disk_path    | startswith($d))
and (.metadata.port_mappings | length > 0)
' "$SNAP_DIR/$DST/config.json" >/dev/null \
    || { echo "FATAL: private snapshot $DST did not rewrite cleanly" >&2; exit 1; }

echo "$DST"
