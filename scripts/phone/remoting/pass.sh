#!/bin/bash
# pass.sh <mode> [emulator args]: cold boot, then every workload once, into /bench/out/<mode>
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
mode=$1; shift
mkdir -p $B/out/$mode
exec > $B/logs/pass-$mode.log 2>&1
cp $B/scripts/wl.sh $B/wl-$mode.sh   # a new file: the container's FUSE view caches old sizes
$B/scripts/boot.sh "$mode" "$@"
sleep 20
for w in robotpark launcher settings shade firefox_article firefox_images wikipedia maps vlc game; do
  echo "$(date +%T) $w load $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a $B/out/$mode/load.txt
  $B/scripts/x.sh env DUMP=/bench/out/$mode/dump.bin /bench/wl-$mode.sh run "$w" "/bench/out/$mode"
done
$B/scripts/x.sh sync
echo PASS DONE
