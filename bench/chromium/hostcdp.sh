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
# REPS is the MEASURED rep count and WARMUP is EXTRA, exactly as reqbench.py
# reads --reps/--warmup ("for rep in range(args.warmup + args.reps)"), so the
# campaign's REPS/WARMUP can be handed to both arms and produce the same
# schedule. The default measured count is what REPS=202 WARMUP=2 used to yield.
REPS="${REPS:-200}"
WARMUP="${WARMUP:-2}"
URL="${URL:-http://127.0.0.1:8000/medium.html}"
CDP_PORT="${CDP_PORT:-9222}"
RUNID="${RUNID:-$(tr -d - </proc/sys/kernel/random/uuid)}"
RESULTS="${RESULTS:-$HERE/results/hostcdp-$RUNID}"
CNAME="hostcdp-$RUNID"
LOADAVG_FILE="${LOADAVG_FILE:-/proc/loadavg}"

[[ "$RUNID" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,95}$ ]] \
    || { echo "REFUSING: RUNID must be 1-96 filename-safe characters" >&2; exit 2; }
[[ "$REPS" =~ ^[0-9]+$ && "$WARMUP" =~ ^[0-9]+$ ]] \
    || { echo "REFUSING: REPS and WARMUP must be nonnegative integers" >&2; exit 2; }
if ! [[ "$CDP_PORT" =~ ^[0-9]+$ ]] \
        || [ "$CDP_PORT" -lt 1 ] || [ "$CDP_PORT" -gt 65535 ]; then
    echo "REFUSING: CDP_PORT must be in 1..65535" >&2
    exit 2
fi
for tool in awk cut date dirname mkdir pgrep podman python3 sed seq sha256sum \
        sleep timeout tr uname; do
    command -v "$tool" >/dev/null 2>&1 \
        || { echo "REFUSING: '$tool' missing" >&2; exit 2; }
done

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
results_parent=$(dirname -- "$RESULTS")
mkdir -p -- "$results_parent"
if ! mkdir -- "$RESULTS" 2>/dev/null; then
    log "REFUSING: RESULTS must name a new directory; could not claim $RESULTS"
    exit 2
fi

container_owns_cdp() {
    timeout 30 podman exec "$CNAME" python3 -c '
import os
import sys

port = int(sys.argv[1])
inodes = set()
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        rows = open(table).read().splitlines()[1:]
    except OSError:
        continue
    for row in rows:
        fields = row.split()
        if len(fields) <= 9 or fields[3] != "0A":
            continue
        try:
            local_port = int(fields[1].rsplit(":", 1)[1], 16)
        except (IndexError, ValueError):
            continue
        if local_port == port:
            inodes.add(fields[9])
if not inodes:
    raise SystemExit(1)
for pid in os.listdir("/proc"):
    if not pid.isdigit():
        continue
    try:
        fds = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        continue
    for fd in fds:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{fd}")
        except OSError:
            continue
        if target.startswith("socket:[") and target[8:-1] in inodes:
            raise SystemExit(0)
raise SystemExit(1)
' "$CDP_PORT" >/dev/null 2>&1
}

# URL may name ONE url (today's contract) or a comma-separated list. The list is
# cycled exactly as reqbench.py cycles the VM arm's schedule -- url_for_rep()
# returns urls[rep % len(urls)], rep counted from 0 across warmup and measured
# reps alike -- so this host control can run the SAME corpus schedule as the VM
# arm rather than a different workload. Whitespace around a member is stripped
# and empty members are dropped, matching reqbench.parse_urls.
mapfile -t URLS < <(printf '%s' "$URL" | tr ',' '\n' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' | awk 'NF')
[ "${#URLS[@]}" -gt 0 ] || { log "REFUSING: URL names no urls"; exit 2; }
# Nothing to summarise, and the summary would die on an empty list after the
# warmup reps had already run.
[ "$REPS" -ge 1 ] || { log "REFUSING: REPS must be >= 1 (it is the MEASURED count; WARMUP is extra), got $REPS"; exit 2; }
# reqbench refuses a multi-URL run with fewer than two full cycles of warmup;
# the same floor here keeps the two schedules the same shape rather than
# comparing a warmed VM arm against a cold host arm.
if [ "${#URLS[@]}" -gt 1 ] && [ "$WARMUP" -lt $((2 * ${#URLS[@]})) ]; then
    log "REFUSING: a ${#URLS[@]}-url schedule needs WARMUP >= $((2 * ${#URLS[@]})) (two full cycles), got $WARMUP"
    exit 2
fi

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
    fc=0
    for process in fcvm firecracker; do
        if count=$(pgrep -c -x "$process" 2>&1); then rc=0; else rc=$?; fi
        case "$rc" in
            0)
                [[ "$count" =~ ^[0-9]+$ ]] \
                    || { log "REFUSING: pgrep returned a nonnumeric count for $process: $count"; exit 2; }
                fc=$((fc + count))
                ;;
            1) ;;
            *) log "REFUSING: pgrep exited $rc while checking $process: $count"; exit 2 ;;
        esac
    done
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

cleanup() {
    original_rc=$?
    trap - EXIT
    if ! timeout 30 podman rm -f -- "$CNAME" >/dev/null 2>&1; then
        log "FAILED: could not remove owned container $CNAME"
        [ "$original_rc" -ne 0 ] && exit "$original_rc"
        exit 1
    fi
    exit "$original_rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

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
urls_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "${URLS[@]}")
run_id_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$RUNID")

log "starting host container ($IMAGE) with CDP on $CDP_PORT"
cpus_arg=()
if [ -n "${CPUS:-}" ]; then
    cpus_arg=(--cpus "$CPUS")
fi
cpus_json=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1] or None))' "${CPUS:-}")
timeout 120 podman run -d --name "$CNAME" --network host \
    "${cpus_arg[@]}" "${resolve_env[@]}" "$IMAGE" >/dev/null

# Ready = the same two conditions the VM golden gates on: warm marker file AND
# a live CDP round trip that finds a page target (cdp_health inside the image).
t0=$SECONDS
until timeout 30 podman exec "$CNAME" test -f /run/bench-ready 2>/dev/null \
        && container_owns_cdp; do
    [ $((SECONDS - t0)) -lt 120 ] || {
        log "container never became ready with an owned CDP listener"
        timeout 30 podman logs --tail 20 "$CNAME" >&2 || true
        exit 1
    }
    sleep 0.5
done
log "warm marker up after $((SECONDS - t0))s; measuring $REPS reps after $WARMUP warmup ($((WARMUP + REPS)) total) against $URL"

# Record the measured configuration beside the numbers, not in prose. Each row
# below carries this file's exact SHA-256, so a copied jsonl cannot acquire a
# different run's image, resolver, corpus, or count metadata by co-location.
{
    echo "{\"run_id\": $run_id_json, \"image\": \"$IMAGE\", \"image_id\": \"$(timeout 30 podman inspect --format '{{.Image}}' "$CNAME")\","
    echo " \"reps\": $REPS, \"warmup\": $WARMUP, \"total_reps\": $((WARMUP + REPS)),"
    echo " \"url\": \"$URL\", \"cdp_port\": $CDP_PORT,"
    echo " \"urls\": $urls_json, \"url_count\": ${#URLS[@]}, \"cpus\": $cpus_json,"
    echo " \"driver\": \"cdpdrive.py\", \"network\": \"host (no VM, no DNAT)\","
    echo " \"resolve_all_to\": $resolve_json,"
    echo " \"host_kernel\": \"$(uname -r)\", \"loadavg_at_start\": \"$la\"}"
} > "$RESULTS/run.json"
run_json_sha256=$(sha256sum "$RESULTS/run.json" | cut -d' ' -f1)

OUT="$RESULTS/hostcdp.jsonl"
: > "$OUT"
TOTAL_REPS=$((WARMUP + REPS))
for rep in $(seq 0 $((TOTAL_REPS - 1))); do
    rep_url="${URLS[$((rep % ${#URLS[@]}))]}"
    t_start=$(date +%s.%N)
    if out=$(python3 "$HERE/cdpdrive.py" "127.0.0.1:$CDP_PORT" "$rep_url" --format jpeg --nav-timing 2>&1); then
        ok=true
    else
        ok=false
    fi
    t_end=$(date +%s.%N)
    wall_ms=$(python3 -c "print(f'{(${t_end}-${t_start})*1000:.1f}')")
    warm=$([ "$rep" -lt "$WARMUP" ] && echo true || echo false)
    # Per-rep 1-minute load, the same field reqbench.py puts on every record
    # (rec["loadavg1"]) and reqanalyze reports as min/median/max "during run".
    # The start-of-run reading in run.json cannot show contention that arrived
    # mid-run, which is the contention that would move these numbers. A
    # non-numeric read becomes null rather than killing a run mid-loop.
    la_rep=$(cut -d' ' -f1 "$LOADAVG_FILE" 2>/dev/null || true)
    [[ "$la_rep" =~ ^[0-9]+([.][0-9]+)?$ ]] || la_rep=null
    printf '{"run_json_sha256": "%s", "rep": %d, "ok": %s, "warmup": %s, "wall_ms": %s, "loadavg1": %s, "url": %s, "driver": %s}\n' \
        "$run_json_sha256" "$rep" "$ok" "$warm" "$wall_ms" "$la_rep" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$rep_url")" \
        "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1][-2000:]))' "$out")" >> "$OUT"
    [ "$ok" = true ] || { log "rep $rep FAILED ($rep_url): $out"; exit 4; }
done

python3 - "$OUT" "$WARMUP" "$RESULTS/summary.json" <<'PY'
import json, statistics, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
measured_rows = [r for r in rows if not r["warmup"]]


def pct(values, p):
    """p50 is statistics.median, which is what reqanalyze uses for every median
    it publishes, so a ratio taken between this number and a VM arm's median is
    between two numbers computed the same way. Other percentiles are
    nearest-rank."""
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


measured = [r["wall_ms"] for r in measured_rows]
n = len(measured)
if n == 0:
    # REPS >= 1 is enforced before any rep runs and measured == REPS, so this
    # can only mean the jsonl and the warmup count disagree. Refuse rather than
    # die on an IndexError after minutes of work with no summary beside the
    # jsonl.
    sys.exit(f"REFUSING: no measured reps in {len(rows)} rows (warmup={sys.argv[2]}); nothing to summarise")
p50, p95 = pct(measured, 50), pct(measured, 95)
print(f"host direct-CDP warm pool: n={n} p50={p50:.1f}ms p95={p95:.1f}ms "
      f"mean={statistics.mean(measured):.1f}ms failures=0")
per_url = {}
if any("url" in r for r in measured_rows):
    by_url = {}
    for r in measured_rows:
        by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
    for url, vals in by_url.items():
        per_url[url] = {"n": len(vals), "p50_ms": round(pct(vals, 50), 1),
                        "p95_ms": round(pct(vals, 95), 1),
                        "mean_ms": round(statistics.mean(vals), 1)}
    if len(by_url) > 1:
        print("per-url wall p50 (ms):")
        for url, s in sorted(per_url.items(), key=lambda kv: kv[1]["p50_ms"]):
            print(f"  {s['p50_ms']:8.1f}  n={s['n']:3d}  {url}")
# Name the convention in the record, not only in the code that produced it: a
# ratio between this p50 and a reqanalyze median is only meaningful if both are
# statistics.median, and a reader of summary.json alone cannot otherwise tell.
la = [r["loadavg1"] for r in measured_rows if isinstance(r.get("loadavg1"), (int, float))]
load = None
if la:
    load = {"n": len(la), "min": round(min(la), 2),
            "median": round(statistics.median(la), 2), "max": round(max(la), 2)}
    print(f"loadavg1 during measured reps: min={load['min']} median={load['median']} "
          f"max={load['max']}   <-- contention check")
json.dump({"n": n, "p50_ms": round(p50, 1), "p95_ms": round(p95, 1),
           "mean_ms": round(statistics.mean(measured), 1),
           "failures": 0, "p50_convention": "statistics.median",
           "loadavg1_measured": load, "per_url": per_url},
          open(sys.argv[3], "w"), indent=1)
PY
log "results in $RESULTS"
