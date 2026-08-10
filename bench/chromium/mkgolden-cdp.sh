#!/usr/bin/env bash
# Build the golden snapshot the CDP-path profile restores from.
#
# Differs from mkgolden.sh in exactly two ways, both required and both deliberate:
#
#   1. IMAGE = localhost/chromium-bench-req, not localhost/chromium-bench.
#      MEASURED: `localhost/chromium-bench` has NO socat binary and no relay in its
#      /opt/bench/entry.sh (`grep -c socat` = 0). Chromium ignores
#      --remote-debugging-address on this build and binds guest loopback only, so a
#      clone restored from that image is UNREACHABLE from the host by construction.
#      The exec-path baseline was measured on it, which is fine — the exec driver runs
#      inside the guest — but it cannot carry the CDP arm.
#
#   2. --publish 9223:9223. The relay listens on guest-wildcard 9223 and forwards to
#      guest-loopback 9222; --publish carries host -> guest. Clones inherit
#      port_mappings from snapshot metadata (src/commands/snapshot.rs:1001/1051/1070),
#      so this one flag on the golden is what makes every restored clone drivable.
#
# Everything else mirrors mkgolden.sh so the warm state that gets frozen matches the
# exec-path baseline: same CPU/MEM, same rootless egress, boot with FCVM_NO_SNAPSHOT=1,
# wait for CHROMIUM_BENCH_READY, render one host-served fixture through the egress path
# (this warms Chromium, and the warm state is what is snapshotted), then snapshot create.
#
# Adds one pre-snapshot gate mkgolden.sh has no need for: the host-side relay is proven
# on the ORIGINAL VM before the snapshot is taken, so a golden can never be frozen around
# a browser the host cannot reach.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." &>/dev/null && pwd)
FCVM=$REPO_ROOT/target/release/fcvm
IMAGE=${IMAGE:-localhost/chromium-bench-req}
STATE_DIR=/mnt/fcvm-btrfs/state
SNAP_DIR=/mnt/fcvm-btrfs/snapshots
CPU=${CPU:-2}
MEM=${MEM:-2048}
RELAY_PORT=${RELAY_PORT:-9223}
PORT=${PORT:-18997}
LOG=${LOG:-/tmp/mkgolden-cdp-$$.log}

log() { printf '[mkgolden-cdp %s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

IID=$(podman images --format '{{.ID}}' "$IMAGE" | head -1)
[ -n "$IID" ] || { echo "image $IMAGE missing" >&2; exit 1; }
# The relay is the whole point of this golden; refuse to build one that lacks it.
podman run --rm --entrypoint sh "$IMAGE" -c 'command -v socat >/dev/null && grep -q socat /opt/bench/entry.sh' \
    || { echo "FATAL: $IMAGE has no socat relay — a clone from it is unreachable from the host" >&2; exit 1; }

H=$(printf '%s' "$IID|$CPU|$MEM|rootless|pub$RELAY_PORT|v1" | sha256sum | cut -c1-8)
TAG=cb-cdp-golden-$H
log "image=$IID mem=$MEM publish=$RELAY_PORT tag=$TAG"

if [ -d "$SNAP_DIR/$TAG" ]; then log "already exists: $TAG"; echo "$TAG"; exit 0; fi

HOST4=$(ip -4 route get 1.1.1.1 | grep -oP 'src \K\S+' | head -1)
python3 "$SCRIPT_DIR/hostserver.py" --root "$SCRIPT_DIR/pages" --port "$PORT" \
    >/tmp/mkgolden-cdp-http-$$.log 2>&1 &
HTTP_PID=$!
trap 'kill $HTTP_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do
    curl -sf -o /dev/null "http://$HOST4:$PORT/minimal.html" && break
    sleep 0.1
done

NAME=cb-cdpg-$$
log "cold boot $NAME (log $LOG)"
FCVM_NO_SNAPSHOT=1 RUST_LOG=fcvm=debug "$FCVM" podman run --name "$NAME" \
    --cpu "$CPU" --mem "$MEM" --network rootless \
    --publish "$RELAY_PORT:$RELAY_PORT" "$IMAGE" >"$LOG" 2>&1 &
BOOT=$!

t0=$SECONDS
until grep -q CHROMIUM_BENCH_READY "$LOG" 2>/dev/null; do
    if [ $((SECONDS - t0)) -ge 420 ]; then log "BOOT TIMEOUT"; tail -20 "$LOG" >&2; exit 1; fi
    if ! kill -0 $BOOT 2>/dev/null; then log "boot process died"; tail -20 "$LOG" >&2; exit 1; fi
    sleep 1
done
log "container ready after $((SECONDS - t0))s"

PID=$(jq -r --arg n "$NAME" 'select(.name==$n) | .pid // empty' "$STATE_DIR"/*.json | head -1)
[ -n "$PID" ] || { log "no state pid"; exit 1; }
LOOPIP=$(jq -r --arg n "$NAME" 'select(.name==$n) | .config.network.loopback_ip // empty' \
    "$STATE_DIR"/*.json | head -1)
[ -n "$LOOPIP" ] || { log "no loopback_ip in state"; exit 1; }

# PRE-SNAPSHOT CHAIN GATE. Prove the host can reach Chromium through
# --publish -> guest-wildcard 9223 -> socat -> guest-loopback 9222 on the ORIGINAL VM.
# If this fails the golden is worthless, and failing here names the hop.
log "verifying host->guest CDP chain on $LOOPIP:$RELAY_PORT"
ok=0
for _ in $(seq 1 100); do
    if curl -sf --max-time 3 "http://$LOOPIP:$RELAY_PORT/json/version" -o /tmp/cdpver-$$.json; then
        ok=1; break
    fi
    sleep 0.5
done
[ "$ok" = 1 ] || { log "FATAL: host cannot reach CDP at $LOOPIP:$RELAY_PORT"; tail -30 "$LOG" >&2; exit 1; }
log "chain ok: $(jq -r '.Browser' /tmp/cdpver-$$.json)"

log "egress verification render (warms Chromium; this warm state is what gets frozen)"
"$FCVM" exec --pid "$PID" -c -- python3 /opt/bench/render.py \
    "http://$HOST4:$PORT/minimal.html" --out-prefix /tmp/verify 2>&1 | tee -a "$LOG" | grep -q RENDER_OK \
    || { log "egress verification FAILED"; tail -20 "$LOG" >&2; kill "$PID" 2>/dev/null; exit 1; }

log "snapshot create -> $TAG"
"$FCVM" snapshot create --pid "$PID" --tag "$TAG" >>"$LOG" 2>&1
kill "$PID" 2>/dev/null || true
for _ in $(seq 1 120); do
    jq -r --arg n "$NAME" 'select(.name==$n) | .pid // empty' "$STATE_DIR"/*.json 2>/dev/null \
        | grep -q . || break
    sleep 0.5
done
# port_mappings must have survived into the snapshot metadata, or clones restore with no
# host-side ingress and every CDP request fails with a connect timeout that looks like a
# guest problem. Verified, not assumed.
jq -e '.metadata.port_mappings | length > 0' "$SNAP_DIR/$TAG/config.json" >/dev/null \
    || { log "FATAL: snapshot $TAG has no port_mappings"; exit 1; }
log "done: $TAG (port_mappings: $(jq -c '.metadata.port_mappings' "$SNAP_DIR/$TAG/config.json"))"
echo "$TAG"
