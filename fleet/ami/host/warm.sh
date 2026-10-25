#!/bin/bash
# Start one renderer VM on GPU $1 and wait for its first frame (fleet/tools/gpurun.sh's
# warm-up), then stop it. Prints the time to first frame. Run as the fcvm user.
set -uo pipefail
G=$1; LOG=$2
IMG=localhost/fcvm-renderer:fleet
t0=$(date +%s%N)
setsid fcvm podman run --name warm --kernel-profile gpu --gpu "$G" --publish 127.0.0.1:20099:8080 $IMG > "$LOG" 2>&1 < /dev/null &
WP=$!
ok=0
for _ in $(seq 1 1800); do
  if curl -fsS -m 2 http://127.0.0.1:20099/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then ok=1; break; fi
  kill -0 $WP 2>/dev/null || break
  sleep 1
done
echo "warm: ok=$ok first_frame_ms=$(( ($(date +%s%N) - t0) / 1000000 )) gpu=$G"
kill -TERM $WP 2>/dev/null; wait $WP
[ $ok = 1 ]
