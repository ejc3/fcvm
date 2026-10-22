#!/bin/bash
# Put the fcvm store (/mnt/fcvm-btrfs) on this instance's local NVMe at boot. The NVMe is
# wiped by every stop and start, so the store is rebuilt each boot from the copy the AMI
# carries (/var/lib/fcvm-fleet/store.tar.zst: kernels, rootfs, initrd, Firecracker, the
# NVIDIA driver image, the renderer image's export and the warmed snapshot cache). Never
# formats the disk that holds /.
set -euo pipefail
M=/mnt/fcvm-btrfs
ARCHIVE=/var/lib/fcvm-fleet/store.tar.zst
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
if [ -s "$ARCHIVE" ]; then
  zstd -q -d -T0 -c "$ARCHIVE" | tar -x --sparse --same-owner --numeric-owner -C "$M"
fi
chown ubuntu:ubuntu "$M"
t2=$(date +%s%N)
echo "fcvm-fleet-store: mkfs+mount $(( (t1 - t0) / 1000000 )) ms, restore $(( (t2 - t1) / 1000000 )) ms, $(du -sh --apparent-size "$M" | cut -f1) apparent"
