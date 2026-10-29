#!/bin/bash
# post.sh <mode>: after a pass, x264 at CRF 23 and zstd/lz4 of each workload, then the summary
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
R=$B/out/$1
exec > $B/logs/post-$1.log 2>&1
for m in $R/*.markers.jsonl; do
  w=$(basename "$m" .markers.jsonl)
  [ -f "$R/$w.nv12" ] && $B/scripts/encode.sh "$R" "$w"
  [ -f "$R/$w.dump" ] && python3 $B/scripts/compress.py "$R" "$w"
done
$B/scripts/an.sh "$R"
echo POST DONE
