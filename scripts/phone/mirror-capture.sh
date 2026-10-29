#!/bin/bash
# The GPU mirror's capture, on a host running a phone VM (docs/fleet/ARCHITECTURE.md, "The GPU
# mirror"): both of the phone streamer's streams as the page receives them, the app restarted
# into the mirror's copy, and the phone's own screenshots, for the replay test
# (fleet/web/e2e/mirror.spec.ts) and the bandwidth numbers.
#
#   scripts/phone/mirror-capture.sh <vm name> <published port> <out dir> <seconds> [restart_at_s|none|home]
#
# The mirror joins at a snapshot of the phone's color buffers (gfxstream's JOIN records). With
# restart_at_s the app is restarted that many seconds in; with home the phone is sent to its
# home screen before the capture and nothing is restarted, so the mirror's first frame is the
# join's. SHOTS (4) screenshots, 2 s apart, from 4 s after the restart (from 1 s after the
# capture starts with none or home). Writes <out>/cap.tap, cap.json, shot-<n>.png (adb
# screencap: lossless, the screen HWC composed) and health.json.
set -euo pipefail
NAME=$1 PORT=$2 OUT=$3 SECS=$4 RESTART=${5:-1}
F=${FCVM:-./target/release/fcvm}
mkdir -p "$OUT"
vm() { "$F" ls --json | jq -r --arg n "$NAME" ".[] | select(.name == \$n) | $1"; }
P=$(vm .pid)
IP=$(vm .config.network.loopback_ip)
[ -n "$P" ] && [ -n "$IP" ] || { echo "mirror-capture.sh: no VM named $NAME" >&2; exit 2; }
X() { "$F" exec --pid "$P" -- "$@"; }
if [ "$RESTART" = home ]; then
  X sh -c '/android/platform-tools/adb shell input keyevent KEYCODE_HOME' > /dev/null
  sleep 3
fi
python3 "$(dirname "$0")/mirror-capture.py" "$IP:$PORT" "$SECS" "$OUT/cap" > "$OUT/cap.out" 2>&1 &
C=$!
if [ "$RESTART" != none ] && [ "$RESTART" != home ]; then
  sleep "$RESTART"
  X /usr/local/bin/phone.sh action restart_app > /dev/null
  sleep 4
else
  sleep 1
fi
i=0
while kill -0 "$C" 2>/dev/null && [ "$i" -lt "${SHOTS:-4}" ]; do
  X sh -c '/android/platform-tools/adb exec-out screencap -p' > "$OUT/shot-$i.png"
  i=$((i + 1))
  sleep 2
done
wait "$C"
cat "$OUT/cap.out"
curl -s "http://$IP:$PORT/health" > "$OUT/health.json"
