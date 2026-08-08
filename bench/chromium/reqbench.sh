#!/bin/bash
# Standalone driver for the request-optimized path (CDP direct + fast teardown).
#
# Separate from bench.sh ON PURPOSE, and not merged into it: bash reads a script
# incrementally as it executes, so editing bench.sh while a run is in flight
# corrupts that run. This file is self-contained; bench.sh is untouched.
#
#   ./reqbench.sh golden      # cold boot with CDP published, snapshot at the health gate
#   ./reqbench.sh verify      # prove all three hops on a RESTORED CLONE (do this first)
#   ./reqbench.sh run         # the three-arm A/B
#
# The two changes under test:
#   PART 1  the request path is Chromium's own CDP endpoint, driven from the host
#           over fcvm's port forwarding. Nothing of ours is resident in the guest.
#   PART 2  the response is delivered the instant the image is in hand; teardown
#           is ONE SIGKILL to fcvm, which the kernel fans out to Firecracker and
#           the namespace holder concurrently via PR_SET_PDEATHSIG.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
FCVM="${FCVM:-$REPO/target/release/fcvm}"
# rootless needs no sudo. SUDO is kept as a hook only so the same script can be
# pointed at a root-only mode later without rewriting every call site.
SUDO="${SUDO:-}"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
# 9223 is the RELAY port (socat, in entry.sh). Chromium ignores
# --remote-debugging-address and binds guest loopback 9222 only; fcvm has no
# feature that exposes a guest LOOPBACK port, so the relay + --publish is the
# path. See Containerfile.chromium-bench for the measured evidence.
CDP_PORT="${CDP_PORT:-9223}"
# rootless: --publish is supported (pasta -t) and clones inherit port_mappings
# from snapshot metadata (src/commands/snapshot.rs:1070). No root needed, and it
# matches the network mode the exec-path baseline was measured on.
NETMODE="${NETMODE:-rootless}"
CPU="${CPU:-2}"
MEM="${MEM:-1024}"
RUNID="${RUNID:-$(date +%H%M%S)-$$}"
RESULTS="${RESULTS:-$HERE/results/reqbench-$RUNID}"
TAG="${TAG:-cb-req-golden}"
FCVM_LOG="${FCVM_LOG:-fcvm=debug}"   # AGENTS.md defect 4: never measure at info
URL="${URL:-http://127.0.0.1:8000/medium.html}"

mkdir -p "$RESULTS/logs"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# Refuse to measure on a box that is already busy. AGENTS.md: contention silently
# inflates every number; a run published without saying so is the failure mode.
guard_quiet() {
    local fc; fc=$(pgrep -c firecracker || true)
    local la; la=$(cut -d' ' -f1 /proc/loadavg)
    log "load=$la firecracker=$fc"
    if [ "${ALLOW_BUSY:-0}" != 1 ] && { [ "${fc:-0}" -gt 0 ] || \
       [ "$(printf '%.0f' "$la")" -gt 2 ]; }; then
        log "REFUSING: box is busy (load=$la, $fc firecracker). Set ALLOW_BUSY=1 to override"
        log "and SAY SO in the report — a number measured under contention is not a number."
        exit 3
    fi
}

state_pid_by_name() {
    $SUDO "$FCVM" ls --json 2>/dev/null | python3 -c '
import json,sys
for v in json.load(sys.stdin):
    if v.get("name")==sys.argv[1]: print(v.get("pid") or ""); break' "$1"
}

cmd_build() {
    log "building $IMAGE"
    # --format docker is LOAD-BEARING: podman's default OCI format DROPS
    # HEALTHCHECK with only a warning, and fcvm's health gate is what
    # triggers the golden snapshot (src/health.rs AND-logic).
    podman build --format docker -t "$IMAGE" -f "$REPO/Containerfile.chromium-bench" "$REPO"
    podman inspect "$IMAGE" --format '{{json .HealthCheck}}' | grep -q cdp_health \
        || { log "FATAL: image has no HEALTHCHECK (OCI format drop?)"; return 1; }
}

cmd_golden() {
    log "golden: cold boot with CDP published on $CDP_PORT -> snapshot $TAG"
    $SUDO "$FCVM" snapshots delete -f "$TAG" >/dev/null 2>&1 || true
    local name="cb-req-g-$RUNID" lf="$RESULTS/logs/golden.log"
    # --publish carries host -> guest; socat inside the container carries
    # guest-wildcard 9223 -> guest-loopback 9222, which is the hop Chromium
    # refuses to make itself. Clones inherit port_mappings from the snapshot
    # metadata (src/commands/snapshot.rs:1070), which is what makes a restored
    # clone drivable at all.
    # NO --health-check URL: leaving it unset is what makes fcvm consult the
    # image's podman HEALTHCHECK (src/health.rs "AND logic"), which is the real
    # CDP round trip we want as the snapshot trigger.
    FCVM_NO_SNAPSHOT=1 $SUDO env RUST_LOG=$FCVM_LOG "$FCVM" podman run --name "$name" \
        --cpu "$CPU" --mem "$MEM" --network "$NETMODE" --publish "$CDP_PORT:$CDP_PORT" \
        "$IMAGE" >"$lf" 2>&1 &
    local t0=$SECONDS pid=""
    until grep -q CHROMIUM_BENCH_READY "$lf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 300 ] || { log "golden: BOOT TIMEOUT"; tail -20 "$lf" >&2; return 1; }
        sleep 1
    done
    pid=$(state_pid_by_name "$name")
    [ -n "$pid" ] || { log "golden: no state pid"; return 1; }

    # Wait for fcvm to publish Healthy — i.e. for the container HEALTHCHECK's real
    # CDP round trip to pass. THIS is the warm point the snapshot must capture.
    log "golden: waiting for fcvm health gate (pid $pid)"
    t0=$SECONDS
    until [ "$($SUDO "$FCVM" ls --json --pid "$pid" | python3 -c \
              'import json,sys; print(json.load(sys.stdin)[0].get("health_status",""))')" = healthy ]; do
        [ $((SECONDS-t0)) -lt 300 ] || { log "golden: HEALTH TIMEOUT"; tail -20 "$lf" >&2; return 1; }
        sleep 1
    done
    log "golden: healthy after $((SECONDS-t0))s — snapshotting"
    $SUDO "$FCVM" snapshot create --pid "$pid" --tag "$TAG" >>"$lf" 2>&1
    $SUDO kill "$pid"
    log "golden: done ($TAG)"
}

# ---------------------------------------------------------------------------
# The end-to-end chain proof. Run this BEFORE trusting any number: it checks the
# three hops separately so a failure names the hop instead of looking like
# "networking is broken".
cmd_verify() {
    log "verify: starting serve for $TAG"
    local sf="$RESULTS/logs/verify-serve.log"
    $SUDO "$FCVM" snapshot serve "$TAG" >"$sf" 2>&1 &
    local serve_pid=$!
    local t0=$SECONDS
    until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 60 ] || { log "verify: serve never came up"; cat "$sf" >&2; return 1; }
        sleep 0.5
    done
    local spid; spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
    log "verify: serve pid $spid"

    local cname="cb-req-verify-$RUNID" cl="$RESULTS/logs/verify-clone.log"
    $SUDO env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run --pid "$spid" --name "$cname" \
        --no-dirty-tracking --no-swap >"$cl" 2>&1 &
    local clone_bg=$!
    t0=$SECONDS
    local cpid=""
    until [ -n "$cpid" ]; do
        cpid=$(state_pid_by_name "$cname")
        [ $((SECONDS-t0)) -lt 120 ] || { log "verify: clone never registered"; tail -20 "$cl" >&2; return 1; }
        sleep 0.2
    done

    local ip
    ip=$($SUDO "$FCVM" ls --json --pid "$cpid" | python3 -c '
import json,sys
n=json.load(sys.stdin)[0]["config"]["network"]
print(n.get("loopback_ip") or n.get("host_ip") or n.get("guest_ip") or "")')
    log "verify: clone pid=$cpid host-side ip=$ip"

    echo "--- HOP A: healthcheck path, 127.0.0.1:$CDP_PORT INSIDE the container ---"
    $SUDO "$FCVM" exec --pid "$cpid" -c -- python3 /opt/bench/cdp_health.py || \
        echo "HOP A FAILED (in-container CDP round trip)"

    echo "--- HOP B: GET /json/version from the HOST against $ip:$CDP_PORT ---"
    python3 - "$ip:$CDP_PORT" <<'PY' || echo "HOP B FAILED (host -> clone CDP HTTP)"
import json, sys, urllib.request
host = sys.argv[1]
# Host-header check: Chromium's DevTools endpoint rejects Host values that are
# neither localhost nor an IP literal, with a 403 that reads like a network fault.
req = urllib.request.Request(f"http://{host}/json/version", headers={"Host": host})
with urllib.request.urlopen(req, timeout=10) as r:
    v = json.load(r)
print("  OK", v.get("Browser"), "| protocol", v.get("Protocol-Version"))
req2 = urllib.request.Request(f"http://{host}/json/version", headers={"Host": "evil.example.com"})
try:
    urllib.request.urlopen(req2, timeout=10)
    print("  note: non-IP Host header ACCEPTED (no Host validation on this build)")
except Exception as e:
    print(f"  note: non-IP Host header REJECTED ({e}) — expected; connect by IP")
PY

    echo "--- HOP C: WebSocket upgrade + one CDP command from the HOST ---"
    python3 "$HERE/cdpdrive.py" "$ip:$CDP_PORT" "$URL" --format jpeg --nav-timing \
        --out-prefix "$RESULTS/verify" || echo "HOP C FAILED (host WS + CDP)"

    echo "--- target id (is it stable across clones? -> can skip /json/list per request) ---"
    python3 - "$ip:$CDP_PORT" <<'PY' || true
import json, sys, urllib.request
with urllib.request.urlopen(f"http://{sys.argv[1]}/json/list", timeout=10) as r:
    for t in json.load(r):
        if t.get("type") == "page":
            print(f"  target id={t.get('id')}")
PY

    $SUDO kill -9 "$cpid" 2>/dev/null || true
    wait "$clone_bg" 2>/dev/null || true
    $SUDO kill "$spid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    log "verify: done (clone state/data left for inspection; reqbench.py reaps its own)"
}

cmd_run() {
    guard_quiet
    log "run: starting serve for $TAG"
    local sf="$RESULTS/logs/serve.log"
    $SUDO "$FCVM" snapshot serve "$TAG" >"$sf" 2>&1 &
    local t0=$SECONDS
    until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 60 ] || { log "run: serve never came up"; cat "$sf" >&2; return 1; }
        sleep 0.5
    done
    local spid; spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
    log "run: serve pid $spid -> reqbench.py"
    $SUDO env RUST_LOG=$FCVM_LOG python3 "$HERE/reqbench.py" --serve-pid "$spid" --url "$URL" \
        --out-dir "$RESULTS" --reps "${REPS:-10}" --warmup "${WARMUP:-2}" \
        --cdp-port "$CDP_PORT" --fcvm "$FCVM" --rust-log "$FCVM_LOG" \
        --arms "${ARMS:-exec,cdp,cdp-fast,noop}"
    $SUDO kill "$spid" 2>/dev/null || true
    log "run: results in $RESULTS"
}

case "${1:-}" in
    build)  cmd_build ;;
    golden) cmd_golden ;;
    verify) cmd_verify ;;
    run)    cmd_run ;;
    all)    cmd_build; cmd_golden; cmd_verify; cmd_run ;;
    *) echo "usage: $0 {build|golden|verify|run|all}" >&2; exit 2 ;;
esac
