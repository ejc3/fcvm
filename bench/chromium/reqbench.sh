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

STATE_DIR="${STATE_DIR:-/mnt/fcvm-btrfs/state}"

mkdir -p "$RESULTS/logs"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# ---------------------------------------------------------------------------
# TEARDOWN. Every background fcvm this script starts is registered here the
# instant it exists, and the trap fires on EXIT/INT/TERM.
#
# Without it, `set -euo pipefail` turns every error path into a leak: an
# unguarded failure exits the shell BEFORE the matching kill, and the VM or
# serve it started keeps running. Two of the three phases did not even capture
# `$!`, so on those paths there was no handle to kill at all. bench.sh in this
# same directory has had this shape all along; this file did not have a single
# `trap`. AGENTS.md: contention silently inflates every number, and a leaked
# serve holds the snapshot mapping into the NEXT run.
CLEANUP_PIDS=()
track() { [ -n "${1:-}" ] && CLEANUP_PIDS+=("$1"); return 0; }

cleanup() {
    local rc=$?
    set +e
    for p in "${CLEANUP_PIDS[@]:-}"; do
        [ -n "$p" ] && $SUDO kill -9 "$p" 2>/dev/null
    done
    # Belt and braces: sweep state files this script's naming owns, in case a
    # pid was never captured (e.g. the process died between spawn and track).
    for f in "$STATE_DIR"/*.json; do
        [ -e "$f" ] || continue
        local nm pid
        nm=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("name") or "")' \
             "$f" 2>/dev/null) || continue
        case "$nm" in
            cb-req-*)
                pid=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("pid") or "")' \
                      "$f" 2>/dev/null)
                [ -n "$pid" ] && $SUDO kill -9 "$pid" 2>/dev/null
                ;;
        esac
    done
    wait 2>/dev/null
    return $rc
}
trap cleanup EXIT INT TERM

# Refuse to measure on a box that is already busy. AGENTS.md: contention silently
# inflates every number; a run published without saying so is the failure mode.
guard_quiet() {
    # `pgrep -c firecracker` alone never matches a leaked `fcvm snapshot serve`
    # (its comm is `fcvm`), which is exactly the residue this script's own error
    # paths used to leave — it would accumulate invisibly while holding the
    # snapshot mapping. scripts/ci-stray-vm-guard.sh uses this wider pattern for
    # the same reason.
    local fc; fc=$(pgrep -c -f '^(.*/)?(firecracker|cloud-hypervisor|fcvm)( |$)' || true)
    local la; la=$(cut -d' ' -f1 /proc/loadavg)
    log "load=$la vm-processes=$fc"
    if [ "${ALLOW_BUSY:-0}" != 1 ] && { [ "${fc:-0}" -gt 0 ] || \
       [ "$(printf '%.0f' "$la")" -gt 2 ]; }; then
        log "REFUSING: box is busy (load=$la, $fc firecracker/fcvm). Set ALLOW_BUSY=1 to override"
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
    # The assignment goes AFTER $SUDO, inside the `env` that follows it. Written
    # as `FCVM_NO_SNAPSHOT=1 $SUDO env RUST_LOG=... fcvm ...` it binds to `sudo`,
    # whose default env_reset drops it — RUST_LOG survived only because it already
    # rode the `env`. bench.sh in this same directory has always done it right.
    # With the variable dropped, src/commands/podman/mod.rs gates `no_snapshot`
    # false and this phase — documented as "golden: cold boot" — would RESTORE
    # from a stale cached snapshot and then snapshot THAT as $TAG, contaminating
    # every arm derived from it. Latent while SUDO defaults to "", live the moment
    # anyone uses the documented root-mode hook.
    $SUDO env FCVM_NO_SNAPSHOT=1 RUST_LOG="$FCVM_LOG" "$FCVM" podman run --name "$name" \
        --cpu "$CPU" --mem "$MEM" --network "$NETMODE" --publish "$CDP_PORT:$CDP_PORT" \
        "$IMAGE" >"$lf" 2>&1 &
    # Capture the handle IMMEDIATELY. The BOOT TIMEOUT path below fires before any
    # state file exists, so `$!` is the ONLY way to reach this VM at that point.
    local vm_bg=$!
    track "$vm_bg"
    local t0=$SECONDS pid=""
    until grep -q CHROMIUM_BENCH_READY "$lf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 300 ] || { log "golden: BOOT TIMEOUT"; tail -20 "$lf" >&2; return 1; }
        sleep 1
    done
    pid=$(state_pid_by_name "$name")
    [ -n "$pid" ] || { log "golden: no state pid"; return 1; }
    track "$pid"

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
    # Guarded: unguarded, a failed `snapshot create` exits the shell under `set -e`
    # before the kill below, leaking a live VM. Now it reaches the trap.
    $SUDO "$FCVM" snapshot create --pid "$pid" --tag "$TAG" >>"$lf" 2>&1 \
        || { log "golden: SNAPSHOT CREATE FAILED"; tail -20 "$lf" >&2; return 1; }
    $SUDO kill "$pid" 2>/dev/null || true
    wait "$vm_bg" 2>/dev/null || true
    log "golden: done ($TAG)"
}

# ---------------------------------------------------------------------------
# The end-to-end chain proof. Run this BEFORE trusting any number: it checks the
# three hops separately so a failure names the hop instead of looking like
# "networking is broken".

# Start one clone against the running serve. Sets CLONE_PID and CLONE_IP.
# Factored out so verify can start TWO of them: a single clone's target id
# cannot answer a question about stability ACROSS clones.
#
# Results come back in GLOBALS, not on stdout, on purpose: `x=$(start_clone …)`
# runs the function in a subshell, so its `track` calls would update a copy of
# CLEANUP_PIDS that is discarded on return — i.e. the trap would never see the
# clone it just started, which is the exact leak this trap exists to close.
CLONE_PID=""
CLONE_IP=""
start_clone() {
    local spid="$1" cname="$2" cl="$3"
    CLONE_PID=""; CLONE_IP=""
    $SUDO env RUST_LOG=$FCVM_LOG "$FCVM" snapshot run --pid "$spid" --name "$cname" \
        --no-dirty-tracking --no-swap >"$cl" 2>&1 &
    track "$!"
    local t0=$SECONDS cpid=""
    until [ -n "$cpid" ]; do
        cpid=$(state_pid_by_name "$cname")
        [ $((SECONDS-t0)) -lt 120 ] || { log "clone $cname never registered"; tail -20 "$cl" >&2; return 1; }
        sleep 0.2
    done
    track "$cpid"
    CLONE_PID="$cpid"
    CLONE_IP=$($SUDO "$FCVM" ls --json --pid "$cpid" | python3 -c '
import json,sys
n=json.load(sys.stdin)[0]["config"]["network"]
print(n.get("loopback_ip") or n.get("host_ip") or n.get("guest_ip") or "")')
}

# Print the page target id for a clone, or nothing.
#
# Delegates to cdpdrive.py --print-target rather than hand-rolling the lookup,
# which fixes TWO defects at once. (1) READINESS: this was a single-shot urlopen
# with `2>/dev/null || true`, and `start_clone` returns as soon as the state file
# carries a pid — it never waits for the CDP port (contrast reqbench.py's
# `wait_port`). Clone 1 is warm because HOPs A/B/C ran against it; clone 2 was
# queried the instant it registered, so a connection refused produced an empty id
# and the documented stability gate failed on a RACE. It fails closed, which is
# the right direction, but a flaky gate is the thing people learn to bypass.
# (2) FILTER MISMATCH: this took the first `type == "page"`, while the driver that
# actually consumes the id skips `devtools://` pages — so the two could compare
# different targets. `resolve_target` now retries against a real deadline and
# applies the devtools:// filter, and both halves are covered by
# CdpDriveResolveThrottling in test_reqbench.py.
TARGET_ID_TIMEOUT="${TARGET_ID_TIMEOUT:-60}"
target_id() {
    python3 "$HERE/cdpdrive.py" "$1" http://unused/ --print-target \
        --timeout "$TARGET_ID_TIMEOUT" 2>/dev/null || true
}

cmd_verify() {
    # Every hop feeds this counter and the function RETURNS it. Each hop used to
    # be `... || echo "HOP X FAILED"`, which makes the compound command SUCCEED —
    # so cmd_verify exited 0 no matter how many hops failed, and `all` (which
    # relies on `set -e` to stop the chain) went straight on to the measured run
    # after printing three FAILED lines. verify is documented as the gate; a gate
    # that cannot fail is not a gate.
    local fail=0
    log "verify: starting serve for $TAG"
    local sf="$RESULTS/logs/verify-serve.log"
    $SUDO "$FCVM" snapshot serve "$TAG" >"$sf" 2>&1 &
    local serve_bg=$!
    track "$serve_bg"
    local t0=$SECONDS
    until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
        [ $((SECONDS-t0)) -lt 60 ] || { log "verify: serve never came up"; cat "$sf" >&2; return 1; }
        sleep 0.5
    done
    local spid; spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
    track "$spid"
    log "verify: serve pid $spid"

    local cname="cb-req-verify-$RUNID" cl="$RESULTS/logs/verify-clone.log"
    start_clone "$spid" "$cname" "$cl" || return 1
    local cpid="$CLONE_PID" ip="$CLONE_IP"
    # An empty IP is the most likely real misconfiguration, and it used to be
    # fully swallowed: hops B and C then ran against ":9223", failed, printed
    # FAILED, and verify still exited 0.
    [ -n "$ip" ] || { log "verify: NO host-side IP in the clone's network config"; fail=1; }
    log "verify: clone pid=$cpid host-side ip=$ip"

    echo "--- HOP A: healthcheck path, 127.0.0.1:$CDP_PORT INSIDE the container ---"
    $SUDO "$FCVM" exec --pid "$cpid" -c -- python3 /opt/bench/cdp_health.py \
        || { echo "HOP A FAILED (in-container CDP round trip)"; fail=1; }

    echo "--- HOP B: GET /json/version from the HOST against $ip:$CDP_PORT ---"
    python3 - "$ip:$CDP_PORT" <<'PY' || { echo "HOP B FAILED (host -> clone CDP HTTP)"; fail=1; }
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
        --out-prefix "$RESULTS/verify" || { echo "HOP C FAILED (host WS + CDP)"; fail=1; }

    # --- target id stability, ACROSS CLONES, asserted.
    # This block used to print one clone's id and compare it with nothing, under
    # a heading that asked a cross-clone question. One id cannot answer it. The
    # serve is already up, so a second clone is cheap.
    echo "--- target id stability ACROSS CLONES (-> can /json/list be skipped?) ---"
    local id1 id2 cname2="cb-req-verify2-$RUNID" cpid2 ip2
    id1=$(target_id "$ip:$CDP_PORT")
    start_clone "$spid" "$cname2" "$RESULTS/logs/verify-clone2.log" || return 1
    cpid2="$CLONE_PID"; ip2="$CLONE_IP"
    id2=$(target_id "$ip2:$CDP_PORT")
    echo "  clone1 ($ip) id=${id1:-<none>}"
    echo "  clone2 ($ip2) id=${id2:-<none>}"
    if [ -z "$id1" ] || [ -z "$id2" ]; then
        echo "  TARGET ID UNREADABLE on at least one clone — cannot judge stability"
        fail=1
    elif [ "$id1" = "$id2" ]; then
        echo "  STABLE across 2 clones — --ws-url prewiring is sound for this snapshot"
    else
        echo "  TARGET ID NOT STABLE ACROSS CLONES ($id1 != $id2) — /json/list cannot be skipped"
        fail=1
    fi

    $SUDO kill -9 "$cpid2" 2>/dev/null || true
    $SUDO kill -9 "$cpid" 2>/dev/null || true
    $SUDO kill "$spid" 2>/dev/null || true
    wait 2>/dev/null || true
    # Return AFTER the cleanup above, never before it.
    [ "$fail" -eq 0 ] || { log "verify: FAILED ($fail check(s)) — do NOT run the A/B"; return 1; }
    log "verify: done (clone state/data left for inspection; reqbench.py reaps its own)"
}

# BOTH memory backends are runnable from here. reqbench.py has had the FILE path
# fully built (`--snapshot-tag` -> fcvm's `--snapshot <name>`, recorded as
# `"backend": "file"` in the run metadata) while this driver hardcoded the UFFD
# serve and never passed `--snapshot-tag` at all — so the recorded metadata was
# honest but could only ever carry one value, and REVIEW.md's re-run gate
# (">=200 CDP requests PER BACKEND at 0 failures") was not runnable.
BACKEND="${BACKEND:-uffd}"

cmd_run() {
    guard_quiet
    local rc=0 spid="" serve_bg=""
    local backend_args=()
    case "$BACKEND" in
        uffd)
            log "run: BACKEND=uffd — starting serve for $TAG"
            local sf="$RESULTS/logs/serve.log"
            $SUDO "$FCVM" snapshot serve "$TAG" >"$sf" 2>&1 &
            serve_bg=$!
            track "$serve_bg"
            local t0=$SECONDS
            until grep -q "Waiting for VMs" "$sf" 2>/dev/null; do
                [ $((SECONDS-t0)) -lt 60 ] || { log "run: serve never came up"; cat "$sf" >&2; return 1; }
                sleep 0.5
            done
            spid=$(grep -oP 'Serve PID: \K[0-9]+' "$sf" | head -1)
            [ -n "$spid" ] || { log "run: could not read Serve PID from $sf"; return 1; }
            track "$spid"
            log "run: serve pid $spid -> reqbench.py"
            backend_args=(--serve-pid "$spid")
            ;;
        file)
            # No serve at all: clones restore MAP_PRIVATE from the snapshot files.
            log "run: BACKEND=file — no UFFD serve, restoring from $TAG directly"
            backend_args=(--snapshot-tag "$TAG")
            ;;
        *)
            log "run: unknown BACKEND=$BACKEND (want uffd|file)"; return 2 ;;
    esac
    # Guarded: unguarded, ANY non-zero exit from reqbench.py (including its
    # exit 4 when a teardown leaves a survivor) exits the shell under `set -e`
    # before the kill below, leaking the serve into the next phase.
    $SUDO env RUST_LOG=$FCVM_LOG python3 "$HERE/reqbench.py" "${backend_args[@]}" --url "$URL" \
        --out-dir "$RESULTS" --reps "${REPS:-10}" --warmup "${WARMUP:-2}" \
        --cdp-port "$CDP_PORT" --fcvm "$FCVM" --rust-log "$FCVM_LOG" \
        --arms "${ARMS:-exec,cdp,cdp-fast,noop}" || rc=$?
    if [ -n "$spid" ]; then
        $SUDO kill "$spid" 2>/dev/null || true
        # WAIT for teardown to finish, so a following phase's guard_quiet does not
        # race a serve that is still shutting down.
        wait "$serve_bg" 2>/dev/null || true
    fi
    log "run: results in $RESULTS (backend=$BACKEND, reqbench.py exit $rc)"
    return $rc
}

# Only dispatch when EXECUTED. Sourcing the file makes its helpers unit-testable
# (see ReqbenchShell in test_reqbench.py) instead of reachable only through a
# whole phase.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    case "${1:-}" in
        build)  cmd_build ;;
        golden) cmd_golden ;;
        verify) cmd_verify ;;
        run)    cmd_run ;;
        all)    cmd_build; cmd_golden; cmd_verify; cmd_run ;;
        *) echo "usage: $0 {build|golden|verify|run|all}" >&2; exit 2 ;;
    esac
fi
