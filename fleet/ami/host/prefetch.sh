#!/bin/bash
# Read every file in $1 (default /etc/fcvm-fleet/prefetch.list, written by warm-root.sh at
# bake: what a GPU VM launch opens and runs from the root volume) once, early in boot. The
# root volume is restored from the AMI's snapshot and fetches each block from S3 the first
# time it is read. Read in the launch's own order, at its first launch after a wake those
# files cost 34 s; read here in 1 MiB pieces 64 at a time they took 2.7 s against 14.3 s
# one file after another (130 MB in 181 files, a volume fresh from the AMI's snapshot,
# g4dn.metal, 2026-10-27). A file the list names and the image lacks is skipped.
set -uo pipefail
LIST=${1:-/etc/fcvm-fleet/prefetch.list}
[ -r "$LIST" ] || { echo "fcvm-fleet-prefetch: no $LIST"; exit 0; }
t0=$(date +%s%N)
files=0; bytes=0
while read -r f; do
  [ -f "$f" ] || continue
  s=$(stat -L -c %s "$f") || continue
  files=$((files + 1)); bytes=$((bytes + s))
  for ((i = 0; i * 1048576 < s; i++)); do printf '%s\0%s\0' "$f" "$i"; done
done < "$LIST" | xargs -0 -n 2 -P 64 sh -c 'dd if="$0" of=/dev/null bs=1M skip="$1" count=1 status=none'
rc=$?
echo "fcvm-fleet-prefetch: read the files of $LIST in $(( ($(date +%s%N) - t0) / 1000000 )) ms (status $rc)"
