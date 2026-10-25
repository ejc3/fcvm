#!/bin/bash
# Put the fcvm store (/mnt/fcvm-btrfs) on this instance's local NVMe at boot. The NVMe is
# wiped by every stop and start, so the store is rebuilt each boot from the copy the AMI
# carries (/var/lib/fcvm-fleet/store/: kernels, rootfs, initrd, Firecracker, the NVIDIA
# driver image, the renderer image's export and the warmed snapshot cache, packed by
# store-pack.py into parts that are extracted at once). Never formats the disk that holds /.
#
# Once the store is restored it writes READY (the agent's --store-ready-file): a JSON object
# of the boot's times in Unix milliseconds, which the agent reports in its hello
# (fleet/PROTOCOL.md, "Agent to coordinator") and the coordinator logs for each wake.
set -euo pipefail
M=/mnt/fcvm-btrfs
ARCHIVE=/var/lib/fcvm-fleet/store
READY=${STORE_READY_FILE:-/var/lib/fcvm-fleet/store-ready}
t0=$(date +%s%N)
if mountpoint -q "$M"; then echo "$M already mounted"; exit 0; fi
root_disk=$(lsblk -no PKNAME "$(findmnt -no SOURCE /)")
mapfile -t devs < <(lsblk -dno NAME,MODEL | awk '/Instance Storage/ {print "/dev/"$1}')
[ "${#devs[@]}" -gt 0 ] || { echo "no instance-store NVMe on this instance"; exit 1; }
for d in "${devs[@]}"; do [ "$d" != "/dev/$root_disk" ] || { echo "REFUSING: $d backs /"; exit 1; }; done
if [ "${#devs[@]}" -gt 1 ]; then mkfs.btrfs -q -f -d raid0 -m raid0 "${devs[@]}"; else mkfs.btrfs -q -f "${devs[0]}"; fi
mkdir -p "$M"
mount -o noatime "${devs[0]}" "$M"
t1=$(date +%s%N)
# The parts are fetched from S3 when the AMI names a location (/etc/fcvm-fleet/store.env:
# STORE_S3_BUCKET, STORE_S3_PREFIX, STORE_S3_REGION; part <name> is the object
# <prefix><name>.tar.zst): a volume restored from the AMI's snapshot reads its blocks lazily
# from S3, which measured 4.6 MB/s (1.9 GB in 419 s), while the instance role reads the
# objects at network speed. The AMI's own copy is the fallback.
#
# Each part is extracted into a tmpfs staging directory of its own and then copied into
# the btrfs with `cp -a` (which keeps hard links and holes). GNU tar writing the store's
# sparse images straight into the btrfs ran at about 270 MB/s: on a g4dn.metal (2026-10-25,
# the bdaa0003 store, 12.7 GB of tar stream) the one stream took 51.8 s at boot, while the
# same stream into tmpfs took 21.6 s and one copy into the btrfs 7.8 s. One zstd stream
# decompresses on one core, so the store is packed in parts (16 by default), each fetched,
# decompressed, extracted and copied while the others are: on the same box and store the
# restore from S3 took 10.2 to 11.8 s, against 30.4 to 30.9 s for one stream through tmpfs.
# The staging needs the uncompressed parts in flight in memory; with less than STAGE_MIN_KB
# free in /dev/shm the parts are extracted into the btrfs directly.
STAGE_MIN_KB=$((48 * 1024 * 1024))
stage=""
if [ "$(df --output=avail -k /dev/shm 2>/dev/null | tail -1)" -ge "$STAGE_MIN_KB" ] 2>/dev/null; then
  stage=$(mktemp -d /dev/shm/fcvm-fleet-store.XXXXXX)
  trap 'rm -rf "$stage"' EXIT
fi
# --- restore functions (fleet/ami/check.sh runs these on a fixture) ---
# untar DIR: a tar stream on stdin into DIR.
untar() { tar -x --sparse --same-owner --numeric-owner -C "$1"; }
# part FETCH PART: one part into the store: through a staging directory of its own, which
# is copied into the store and removed once the part is extracted, or straight in without
# staging. The part's hard links stay links: all names of an inode are in one part.
part() {
  if [ -n "$stage" ]; then
    mkdir "$stage/$2" && "$1" "$2" | zstd -q -d | untar "$stage/$2" &&
      cp -a --sparse=always "$stage/$2/." "$M/" && rm -rf "${stage:?}/$2"
  else
    "$1" "$2" | zstd -q -d | untar "$M"
  fi
}
# restore FETCH PART...: every PART at once, each one's .tar.zst written to stdout by
# `FETCH PART`. Fails if any part fails. The part `dirs` holds every directory and is
# extracted twice: before the parts, so that every directory exists and no part creates
# one, and after them, to set each directory's mode, owner and times once nothing is
# written in it any more. Extracted only after, a directory that a part had created and
# that holds a directory only `dirs` creates kept the time of that creation (tar sets an
# existing directory's times at once, then makes the new one in it).
restore() {
  local fetch=$1 p rc=0 pids=()
  shift
  "$fetch" dirs | zstd -q -d | untar "$M" || return 1
  for p in "$@"; do
    part "$fetch" "$p" &
    pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p" || rc=1; done
  [ "$rc" = 0 ] || return 1
  "$fetch" dirs | zstd -q -d | untar "$M"
}
# empty: what a failed restore left, before the fallback.
empty() {
  find "$M" -mindepth 1 -delete
  if [ -n "$stage" ]; then find "$stage" -mindepth 1 -delete; fi
}
# --- end of restore functions ---
from_s3() { /usr/local/lib/fcvm-fleet/s3-cat.py "$STORE_S3_REGION" "$STORE_S3_BUCKET" "$STORE_S3_PREFIX$1.tar.zst"; }
from_ami() { cat "$ARCHIVE/$1.tar.zst"; }
# No parts (the AMI build's first boot, before bake): the store is mounted empty.
parts=()
if [ -s "$ARCHIVE/parts" ]; then mapfile -t parts < "$ARCHIVE/parts"; fi
src=none
if [ "${#parts[@]}" -gt 0 ] && [ -r /etc/fcvm-fleet/store.env ]; then
  . /etc/fcvm-fleet/store.env
  if restore from_s3 "${parts[@]}"; then
    src=s3
  else
    echo "fetching the store from s3://$STORE_S3_BUCKET/$STORE_S3_PREFIX failed; restoring from the AMI's copy"
    empty
  fi
fi
if [ "${#parts[@]}" -gt 0 ] && [ "$src" = none ]; then
  restore from_ami "${parts[@]}"
  src=ami
fi
if [ -n "$stage" ]; then rm -rf "$stage"; src="$src via tmpfs"; fi
chown ubuntu:ubuntu "$M"
t2=$(date +%s%N)
echo "fcvm-fleet-store: mkfs+mount $(( (t1 - t0) / 1000000 )) ms, restore of ${#parts[@]} parts from $src $(( (t2 - t1) / 1000000 )) ms, $(du -sh --apparent-size "$M" | cut -f1) apparent"
mkdir -p "$(dirname "$READY")"
printf '{"store_started_at": %d, "store_mounted_at": %d, "store_source": "%s"}\n' \
  $((t0 / 1000000)) $((t1 / 1000000)) "$src" > "$READY.tmp"
mv -f "$READY.tmp" "$READY"
