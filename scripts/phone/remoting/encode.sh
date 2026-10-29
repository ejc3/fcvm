#!/bin/bash
# encode.sh <run dir> <workload>: x264 at CRF 23 (preset medium, high profile, 30 fps, keyframe
# every 2 s like the streamer's NVENC) of the captured NV12 window; writes <w>.x264.json.
set -euo pipefail
run=$1; w=$2
cap=$run/$w.capture.json
W=$(jq -r .width "$cap"); H=$(jq -r .height "$cap"); FPS=$(jq -r .fps "$cap"); N=$(jq -r .frames "$cap")
for crf in 23; do
  ffmpeg -y -loglevel error -f rawvideo -pix_fmt nv12 -s "${W}x${H}" -r "$FPS" -i "$run/$w.nv12" \
    -c:v libx264 -preset medium -profile:v high -crf $crf -g $((FPS * 2)) -an "$run/$w.crf$crf.mp4"
done
bytes=$(stat -c %s "$run/$w.crf23.mp4")
secs=$(python3 -c "print($N/$FPS)")
jq -n --argjson b "$bytes" --argjson s "$secs" --argjson n "$N" \
  '{crf: 23, preset: "medium", bytes: $b, seconds: $s, frames: $n, kbps: ($b * 8 / $s / 1000)}' > "$run/$w.x264.json"
cat "$run/$w.x264.json"
