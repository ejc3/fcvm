#!/bin/bash
# Put the fcvm store (/mnt/fcvm-btrfs) on this instance's local NVMe at boot. The NVMe is
# wiped by every stop and start, so the store is rebuilt each boot from the copy the AMI
# carries (/var/lib/fcvm-fleet/store.tar.zst: kernels, rootfs, initrd, Firecracker, the
# NVIDIA driver image, the renderer image's export and the warmed snapshot cache). Never
# formats the disk that holds /.
#
# Once the store is restored it writes READY (the agent's --store-ready-file): a JSON object
# of the boot's times in Unix milliseconds, which the agent reports in its hello
# (fleet/PROTOCOL.md, "Agent to coordinator") and the coordinator logs for each wake.
set -euo pipefail
M=/mnt/fcvm-btrfs
ARCHIVE=/var/lib/fcvm-fleet/store.tar.zst
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
# The archive is fetched from S3 when the AMI names one (/etc/fcvm-fleet/store.env:
# STORE_S3_BUCKET, STORE_S3_KEY, STORE_S3_REGION): a volume restored from the AMI's
# snapshot reads its blocks lazily from S3, which measured 4.6 MB/s (1.9 GB in 419 s),
# while the instance role reads the object at network speed. The AMI's own copy is the
# fallback.
#
# The archive is extracted into a tmpfs staging directory and then copied into the btrfs
# with one `cp -a` (which keeps hard links and holes). GNU tar writing the store's sparse
# images straight into the btrfs ran at about 270 MB/s: on a g4dn.metal (2026-10-25, the
# bdaa0003 store, 12.7 GB of tar stream) the one stream took 51.8 s at boot, while the same
# stream into tmpfs took 21.6 s and the copy into the btrfs 7.8 s (30.5 s in all, the
# copy identical file for file). The staging needs the uncompressed store in memory; with
# less than STAGE_MIN_KB free in /dev/shm the archive is extracted into the btrfs directly.
STAGE_MIN_KB=$((48 * 1024 * 1024))
stage=""
if [ "$(df --output=avail -k /dev/shm 2>/dev/null | tail -1)" -ge "$STAGE_MIN_KB" ] 2>/dev/null; then
  stage=$(mktemp -d /dev/shm/fcvm-fleet-store.XXXXXX)
  trap 'rm -rf "$stage"' EXIT
fi
# --- restore functions (fleet/ami/check.sh runs these on a fixture) ---
# extract: a tar stream on stdin into the store, through the staging directory if any.
extract() {
  if [ -n "$stage" ]; then
    tar -x --sparse --same-owner --numeric-owner -C "$stage" && cp -a --sparse=always "$stage/." "$M/"
  else
    tar -x --sparse --same-owner --numeric-owner -C "$M"
  fi
}
# empty: what a failed extraction left, before the fallback.
empty() {
  find "$M" -mindepth 1 -delete
  if [ -n "$stage" ]; then find "$stage" -mindepth 1 -delete; fi
}
# --- end of restore functions ---
src=none
if [ -r /etc/fcvm-fleet/store.env ]; then
  . /etc/fcvm-fleet/store.env
  if /usr/local/lib/fcvm-fleet/s3-cat.py "$STORE_S3_REGION" "$STORE_S3_BUCKET" "$STORE_S3_KEY" \
      | zstd -q -d -T0 | extract; then
    src=s3
  else
    echo "fetching s3://$STORE_S3_BUCKET/$STORE_S3_KEY failed; restoring from the AMI's copy"
    empty
  fi
fi
if [ "$src" = none ] && [ -s "$ARCHIVE" ]; then
  zstd -q -d -T0 -c "$ARCHIVE" | extract
  src=ami
fi
if [ -n "$stage" ]; then rm -rf "$stage"; src="$src via tmpfs"; fi
chown ubuntu:ubuntu "$M"
t2=$(date +%s%N)
echo "fcvm-fleet-store: mkfs+mount $(( (t1 - t0) / 1000000 )) ms, restore from $src $(( (t2 - t1) / 1000000 )) ms, $(du -sh --apparent-size "$M" | cut -f1) apparent"
mkdir -p "$(dirname "$READY")"
printf '{"store_started_at": %d, "store_mounted_at": %d, "store_source": "%s"}\n' \
  $((t0 / 1000000)) $((t1 / 1000000)) "$src" > "$READY.tmp"
mv -f "$READY.tmp" "$READY"
