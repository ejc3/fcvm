#!/bin/bash
# On a GPU host with fcvm built (make build, make setup-gpu) and the renderer image built
# (make -C fleet/renderer image TAG=fleet): warm the snapshot cache with one VM, start the
# coordinator with a local agent on every GPU bound to vfio-pci (EC2 fake), run
# fleet/tools/fleetrun.py, stop the coordinator. Results in $W/results.
set -uo pipefail
W=${W:-/mnt/work}; R=$W/results; mkdir -p $R
SRC=$(cd "$(dirname "$0")/../.." && pwd)
. "$SRC/fleet/tools/lib.sh"
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $R/progress.log; }
F=$SRC/target/release/fcvm
IMG=localhost/fcvm-renderer:fleet
export RUST_LOG=fcvm::fleet=info
G0=$(ls /sys/bus/pci/drivers/vfio-pci | grep -E '^[0-9a-f]{4}:' | head -1)
log "warm-up on $G0"
t0=$(date +%s%N)
setsid $F podman run --name warm --kernel-profile gpu --gpu $G0 --publish 127.0.0.1:20099:8080 $IMG > $R/warm.log 2>&1 < /dev/null &
WP=$!
ok=0
for i in $(seq 1 1500); do
  if curl -fsS -m 2 http://127.0.0.1:20099/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then ok=1; break; fi
  kill -0 $WP 2>/dev/null || break
  sleep 1
done
log "warm-up ok=$ok after $(( ($(date +%s%N) - t0) / 1000000 )) ms"
kill -TERM $WP; wait $WP
[ $ok = 1 ] || { log "RUN_FAILED warm-up"; exit 1; }
log "coordinator start"
setsid $F fleet coord --listen 127.0.0.1:20000 --state-file $R/state.json --ec2 fake --restart-cooldown 1 \
  --agent-cmd "$F fleet agent --stdio --host gpu-box --image $IMG --base-port 20100 --publish-ip 127.0.0.1" \
  > $R/coord.log 2>&1 < /dev/null &
CP=$!
if ! wait_listening 127.0.0.1 20000 120 $CP; then
  log "RUN_FAILED the coordinator did not listen"
  kill -TERM $CP; wait $CP
  exit 1
fi
python3 $SRC/fleet/tools/fleetrun.py ws://127.0.0.1:20000/live $R ${RUNS:-6} > $R/fleetrun.log 2>&1
rc=$?
log "fleetrun exit=$rc"
nvidia-smi -L > $R/nvidia-smi.txt 2>&1 || true
lspci -nn | grep -i nvidia > $R/lspci.txt
kill -TERM $CP; wait $CP
log "coordinator exit=$? (state file version $(jq .version $R/state.json))"
log "RUN_DONE"
