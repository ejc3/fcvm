#!/bin/bash
# sheet.sh <run dir>: one frame 30 s into each workload's capture, tiled into <run dir>/sheet.png
R=$1; cd "$R" || exit 1
for f in *.nv12; do
  w=${f%.nv12}
  ffmpeg -y -loglevel error -f rawvideo -pix_fmt nv12 -s 720x1280 -r 30 -ss 30 -i "$f" -frames:v 1 -vf "scale=240:-1,drawtext=text=$w:fontcolor=red:fontsize=20:x=5:y=5" "$w.frame.png" 2>/dev/null \
    || ffmpeg -y -loglevel error -f rawvideo -pix_fmt nv12 -s 720x1280 -r 30 -ss 30 -i "$f" -frames:v 1 -vf scale=240:-1 "$w.frame.png"
done
ffmpeg -y -loglevel error -pattern_type glob -i '*.frame.png' -vf tile=5x2 sheet.png && ls -la sheet.png
