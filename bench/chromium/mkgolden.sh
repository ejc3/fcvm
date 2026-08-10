#!/usr/bin/env bash
# Rebuild ONLY the rootless golden snapshot that twopath.py profiles.
#
# bench.sh phase1 builds six goldens (rootless, noredir, noiso, huge, bridged, routed);
# this rebuilds the one, with the same steps and the same content-addressed tag, so a
# concurrent agent deleting the snapshot mid-profile costs ~2 min instead of ~15.
#
# Mirrors bench.sh::boot_golden rootless exactly: cold boot with FCVM_NO_SNAPSHOT=1, wait
# for CHROMIUM_BENCH_READY, render one host-served fixture through the egress path (this
# also warms Chromium, and the warm state is what gets frozen), then snapshot create.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." &>/dev/null && pwd)
FCVM=$REPO_ROOT/target/release/fcvm
IMAGE=localhost/chromium-bench
STATE_DIR=/mnt/fcvm-btrfs/state
SNAP_DIR=/mnt/fcvm-btrfs/snapshots
CPU=${CPU:-2}
MEM=${MEM:-2048}
PORT=${PORT:-18999}
LOG=${LOG:-/tmp/mkgolden-$$.log}

log() { printf '[mkgolden %s] %s\n' "$(date +%H:%M:%S)" "$*"; }

IID=$(podman images --format '{{.ID}}' "$IMAGE" | head -1)
[ -n "$IID" ] || { echo "image $IMAGE missing"; exit 1; }
H=$(printf '%s' "$IID|$CPU|$MEM|rootless|v1" | sha256sum | cut -c1-8)
TAG=cb-golden-rootless-$H
log "image=$IID tag=$TAG"

if [ -d "$SNAP_DIR/$TAG" ]; then log "already exists: $TAG"; echo "$TAG"; exit 0; fi

HOST4=$(ip -4 route get 1.1.1.1 | grep -oP 'src \K\S+' | head -1)
python3 "$SCRIPT_DIR/hostserver.py" --root "$SCRIPT_DIR/pages" --port "$PORT" \
    >/tmp/mkgolden-http-$$.log 2>&1 &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do
    curl -sf -o /dev/null "http://$HOST4:$PORT/minimal.html" && break
    sleep 0.1
done

NAME=cb-g-rootless-$$
log "cold boot $NAME (log $LOG)"
FCVM_NO_SNAPSHOT=1 RUST_LOG=fcvm=debug "$FCVM" podman run --name "$NAME" \
    --cpu "$CPU" --mem "$MEM" "$IMAGE" >"$LOG" 2>&1 &
BOOT=$!

t0=$SECONDS
until grep -q CHROMIUM_BENCH_READY "$LOG" 2>/dev/null; do
    if [ $((SECONDS - t0)) -ge 420 ]; then log "BOOT TIMEOUT"; tail -20 "$LOG"; exit 1; fi
    if ! kill -0 $BOOT 2>/dev/null; then log "boot process died"; tail -20 "$LOG"; exit 1; fi
    sleep 1
done
log "container ready after $((SECONDS - t0))s"

PID=$(jq -r --arg n "$NAME" 'select(.name==$n) | .pid // empty' "$STATE_DIR"/*.json | head -1)
[ -n "$PID" ] || { log "no state pid"; exit 1; }

log "egress verification render"
"$FCVM" exec --pid "$PID" -c -- python3 /opt/bench/render.py \
    "http://$HOST4:$PORT/minimal.html" --out-prefix /tmp/verify 2>&1 | tee -a "$LOG" | grep -q RENDER_OK \
    || { log "egress verification FAILED"; tail -20 "$LOG"; kill "$PID" 2>/dev/null; exit 1; }

log "snapshot create -> $TAG"
"$FCVM" snapshot create --pid "$PID" --tag "$TAG" >>"$LOG" 2>&1
kill "$PID" 2>/dev/null || true
for _ in $(seq 1 120); do
    jq -r --arg n "$NAME" 'select(.name==$n) | .pid // empty' "$STATE_DIR"/*.json 2>/dev/null \
        | grep -q . || break
    sleep 0.5
done
log "done: $TAG"
echo "$TAG"
