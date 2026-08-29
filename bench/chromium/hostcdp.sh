#!/bin/bash
# Host-native direct-CDP warm-pool baseline: the same bench image as a podman
# container ON THE HOST (no VM), driven by the same cdpdrive.py at Chromium's
# own CDP port, same page, same rep discipline as reqbench's cdp arm.
#
# This exists because the corrected record's 218 ms host warm-pool figure was
# measured on the EXEC-style driver: comparing reqbench's direct-CDP VM arm
# against it inflates fcvm's ratio in fcvm's favor (the host side carries
# driver-startup overhead the VM side already shed). The shared-nothing
# premium must be (VM direct-CDP) / (host direct-CDP), same driver both sides.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE="${IMAGE:-localhost/chromium-bench-req}"
REPS="${REPS:-202}"
WARMUP="${WARMUP:-2}"
URL="${URL:-http://127.0.0.1:8000/medium.html}"
CDP_PORT="${CDP_PORT:-9222}"
RUNID="${RUNID:-$(tr -d - </proc/sys/kernel/random/uuid)}"
RESULTS="${RESULTS:-$HERE/results/hostcdp-$RUNID}"
CNAME="hostcdp-$$"
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"

mkdir -p "$RESULTS"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# Same quiet-box refusal as the VM harness: a contaminated baseline poisons
# every ratio computed against it. SETTLE_WAIT_SECS > 0 bounds a wait for the
# box to go quiet before refusing (same knob as reqbench.sh guard_quiet; the
# Makefile runs this right after `build`, whose load a 1-minute average still
# carries). Default 0 keeps the fail-fast refusal.
SETTLE_WAIT_SECS="${SETTLE_WAIT_SECS:-0}"
[[ "$SETTLE_WAIT_SECS" =~ ^[0-9]+$ ]] \
    || { log "SETTLE_WAIT_SECS must be a whole number of seconds (got '$SETTLE_WAIT_SECS')"; exit 2; }
quiet_sample() {
    la=$(cut -d' ' -f1 "$LOADAVG_FILE" || true)
    # This function runs as an `until` condition, where set -e is suppressed
    # and awk reads an empty string as 0, so a missing, unreadable, or empty
    # LOADAVG_FILE would otherwise pass the gate without a load reading.
    [[ "$la" =~ ^[0-9]+([.][0-9]+)?$ ]] \
        || { log "REFUSING: no numeric 1-minute load readable from $LOADAVG_FILE (got '$la')"; exit 2; }
    fc=$(pgrep -c firecracker || true)
    [ "${ALLOW_BUSY:-0}" != 1 ] || return 0
    [ "${fc:-0}" -eq 0 ] || return 1
    # Same 1.0 gate as faultbench; the earlier printf-rounding form refused at 0.70.
    if awk -v l="$la" 'BEGIN{exit !(l >= 1.0)}'; then return 1; fi
    return 0
}
# Base 10: "08" passes the validator and is invalid octal to bash arithmetic.
SETTLE_WAIT_SECS=$((10#$SETTLE_WAIT_SECS))
deadline=$((SECONDS + SETTLE_WAIT_SECS))
until quiet_sample; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        log "REFUSING: load=$la firecracker=$fc. ALLOW_BUSY=1 overrides and taints the run."
        exit 3
    fi
    nap=$((deadline - SECONDS))
    [ "$nap" -le 5 ] || nap=5
    log "settling: load=$la firecracker=$fc; re-sampling in ${nap}s ($((deadline - SECONDS))s left in the ${SETTLE_WAIT_SECS}s window)"
    sleep "$nap"
done

cleanup() { podman rm -f "$CNAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# BENCH_RESOLVE_ALL_TO=<ip> is the resolver rule the VM arm bakes into its
# golden through reqbench.sh GUEST_ENV; the same variable goes to this
# container with -e (entry.sh assembles the Chromium flag from it), so the
# host control runs under the A/B's one variable too. Recorded in run.json as
# resolve_all_to, null when unset.
resolve_env=()
if [ -n "${BENCH_RESOLVE_ALL_TO:-}" ]; then
    resolve_env=(-e "BENCH_RESOLVE_ALL_TO=$BENCH_RESOLVE_ALL_TO")
fi
resolve_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1] or None))' \
    "${BENCH_RESOLVE_ALL_TO:-}")

log "starting host container ($IMAGE) with CDP on $CDP_PORT"
podman run -d --name "$CNAME" --network host "${resolve_env[@]}" "$IMAGE" >/dev/null

# Ready = the same two conditions the VM golden gates on: warm marker file AND
# a live CDP round trip that finds a page target (cdp_health inside the image).
t0=$SECONDS
until podman exec "$CNAME" test -f /run/bench-ready 2>/dev/null; do
    [ $((SECONDS - t0)) -lt 120 ] || { log "container never became ready"; podman logs "$CNAME" | tail -20 >&2; exit 1; }
    sleep 0.5
done
log "warm marker up after $((SECONDS - t0))s; measuring $REPS reps ($WARMUP warmup) against $URL"

# Record the measured configuration beside the numbers, not in prose.
{
    echo "{\"image\": \"$IMAGE\", \"image_id\": \"$(podman inspect --format '{{.Image}}' "$CNAME")\","
    echo " \"reps\": $REPS, \"warmup\": $WARMUP, \"url\": \"$URL\", \"cdp_port\": $CDP_PORT,"
    echo " \"driver\": \"cdpdrive.py\", \"network\": \"host (no VM, no DNAT)\","
    echo " \"resolve_all_to\": $resolve_json,"
    echo " \"host_kernel\": \"$(uname -r)\", \"loadavg_at_start\": \"$la\"}"
} > "$RESULTS/run.json"

OUT="$RESULTS/hostcdp.jsonl"
: > "$OUT"
for rep in $(seq 0 $((REPS - 1))); do
    t_start=$(date +%s.%N)
    if out=$(python3 "$HERE/cdpdrive.py" "127.0.0.1:$CDP_PORT" "$URL" --format jpeg --nav-timing 2>&1); then
        ok=true
    else
        ok=false
    fi
    t_end=$(date +%s.%N)
    wall_ms=$(python3 -c "print(f'{(${t_end}-${t_start})*1000:.1f}')")
    warm=$([ "$rep" -lt "$WARMUP" ] && echo true || echo false)
    printf '{"rep": %d, "ok": %s, "warmup": %s, "wall_ms": %s, "driver": %s}\n' \
        "$rep" "$ok" "$warm" "$wall_ms" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1][-2000:]))' "$out")" >> "$OUT"
    [ "$ok" = true ] || { log "rep $rep FAILED: $out"; exit 4; }
done

python3 - "$OUT" "$WARMUP" <<'PY'
import json, statistics, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
measured = [r["wall_ms"] for r in rows if not r["warmup"]]
measured.sort()
n = len(measured)
p50 = measured[max(0, -(-50*n//100) - 1)]
p95 = measured[max(0, -(-95*n//100) - 1)]
print(f"host direct-CDP warm pool: n={n} p50={p50:.1f}ms p95={p95:.1f}ms "
      f"mean={statistics.mean(measured):.1f}ms failures=0")
PY
log "results in $RESULTS"
