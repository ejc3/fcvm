#!/bin/bash
# On a GPU host with fcvm and the renderer image built (see gpurun.sh), after one warm-up
# VM with --env ENC_BPP=4: the coordinator (EC2 fake, EC2 aggregator) with a local agent
# running its host aggregator, measured by aggrun.py while the director flies the camera.
set -uo pipefail
W=${W:-/mnt/nvme}; R=$W/results; mkdir -p $R
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $R/progress.log; }
SRC=$(cd "$(dirname "$0")/../.." && pwd)
. "$SRC/fleet/tools/lib.sh"
F=$SRC/target/release/fcvm
IMG=localhost/fcvm-renderer:fleet
export RUST_LOG=fcvm::fleet=info
log "run3 coordinator start"
setsid $F fleet coord --listen 127.0.0.1:20000 --state-file $R/state3.json --ec2 fake \
  --ec2-aggregate-listen 127.0.0.1:20300 --ec2-aggregate-url ws://127.0.0.1:20300/frames \
  --agent-cmd "$F fleet agent --stdio --host gpu-box --image $IMG --base-port 20100 --publish-ip 127.0.0.1 --aggregate-port 20200 --fcvm-arg=--env --fcvm-arg=ENC_BPP=4" \
  > $R/coord3.log 2>&1 < /dev/null &
CP=$!
if ! wait_listening 127.0.0.1 20000 120 $CP; then
  log "RUN3_FAILED the coordinator did not listen"
  kill -TERM $CP; wait $CP
  exit 1
fi
FCVM=$F python3 $SRC/fleet/tools/aggrun.py ws://127.0.0.1:20000/live $R/agg ${SECS:-30} > $R/aggrun.log 2>&1
log "aggrun exit=$?"
kill -TERM $CP; wait $CP
log "RUN3_DONE"
