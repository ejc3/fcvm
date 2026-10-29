#!/bin/bash
# an.sh <run dir> [workload]: analyze and print the summary (or one workload's detail)
cd "$1" || exit 1
python3 /mnt/fcvm-btrfs/bench/scripts/analyze.py /mnt/fcvm-btrfs/android-emulator/d59ea6c7eee2/src/hardware/google/gfxstream . || exit 1
[ -n "${2:-}" ] && python3 -c '
import json,sys; d=json.load(open("summary.json"))[sys.argv[1]]
for k in ("app","startup","threads","gfxinfo_frames","capture","x264","compress"):
    print(k, json.dumps(d.get(k)))
print("top_replies", d["all"]["top_replies"]); print("top_bytes", d["all"]["top_bytes"])' "$2"
