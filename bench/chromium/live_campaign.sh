#!/bin/bash
# Live-site arms, one host, one window: fcvm and Cloudflare's hosted Browser Run.
#
# Browser Run's Quick Actions fetch each corpus URL from the live site, so the
# hosted arms (browserrun.py: Browser Run's Chromium and Kitesurf) cannot read
# the byte replay the local arms read. This puts an fcvm arm on the same live
# pages at the same moments: a golden with no baked resolver (the guest
# resolves through pasta with the host's own DNS, so it renders the live web),
# measured by reqbench's cdp, html-fast and noop arms over the 14 corpus URLs,
# while browserrun.py runs batch after batch beside it until reqbench is done.
# Both run on this host, so the hosted arms' client and the fcvm arm share one
# network location (on the Ohio benchmark worker: its reserved addresses).
#
# The two streams overlap in time; they are not interleaved request by request.
# browserrun's work on this host is an HTTPS client waiting on Cloudflare, so
# its CPU beside the measured clones is small, and every record carries the
# host's 1-minute load. Both write their own records under one directory, with
# the window each one ran in (live.json), so drift in a page's content between
# arms can be matched by time: reqbench records each render's image and HTML
# size, browserrun each reply's status, size and digest.
#
#   make bench-live-campaign [LIVE_STAMP=] [CPU=4] [REPS=202] [WARMUP=28] [BR_REPS=14]
#
# Needs make build, make setup-default and make bench-chromium-request-build
# first, and a Browser Run credential this host may read (browserrun.py).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd -P)"
STAMP="${LIVE_STAMP:-$(date -u +%Y%m%d-%H%M%S)}"
TAG="${LIVE_TAG:-cb-live-$STAMP}"
CPU="${CPU:-4}"
REPS="${REPS:-202}"
WARMUP="${WARMUP:-28}"
BR_REPS="${BR_REPS:-14}"
ARMS="${LIVE_ARMS:-cdp,html-fast,noop}"
OUT="${LIVE_RESULTS:-$HERE/results/live-$STAMP}"
URLS=$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import report; print(",".join(report.KITESURF_CORPUS.values()))' "$HERE")

[[ "$STAMP" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] || { echo "REFUSING: LIVE_STAMP must be filename-safe" >&2; exit 2; }
for knob in CPU REPS WARMUP BR_REPS; do
    [[ "${!knob}" =~ ^[0-9]+$ ]] || { echo "REFUSING: $knob must be a nonnegative integer" >&2; exit 2; }
done
[ "$REPS" -ge 1 ] && [ "$BR_REPS" -ge 1 ] || { echo "REFUSING: REPS and BR_REPS must be positive" >&2; exit 2; }
mkdir -- "$OUT" 2>/dev/null || { echo "REFUSING: $OUT exists; every campaign writes a new directory" >&2; exit 2; }

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" >&2; }
now() { python3 -c 'import time; print(time.time())'; }

log "golden $TAG (CPU=$CPU, no baked resolver: the guest renders the live web)"
make -C "$REPO" --no-print-directory bench-chromium-request-golden TAG="$TAG" CPU="$CPU" \
    > "$OUT/golden.log" 2>&1 || { tail -40 "$OUT/golden.log" >&2; exit 1; }
make -C "$REPO" --no-print-directory bench-chromium-request-verify TAG="$TAG" \
    > "$OUT/verify.log" 2>&1 || { tail -40 "$OUT/verify.log" >&2; exit 1; }

# The hosted batches run until reqbench has finished, so the two windows overlap
# from the first measured request to the last. Each batch owns a new directory.
DONE="$OUT/.reqbench-done"
(
    batch=0
    while [ ! -e "$DONE" ]; do
        batch=$((batch + 1))
        python3 "$HERE/browserrun.py" --out "$OUT/browserrun-$(printf '%03d' "$batch")" \
            --reps "$BR_REPS" --warmup 1 > "$OUT/browserrun-$(printf '%03d' "$batch").log" 2>&1 \
            || { echo "browserrun batch $batch exited $?" >> "$OUT/browserrun.failures"; sleep 5; }
    done
) &
BR_PID=$!
trap 'touch "$DONE"; wait "$BR_PID" 2>/dev/null || true' EXIT

log "fcvm live arm: $ARMS, $REPS measured + $WARMUP warmup over the 14 live URLs, Browser Run batches alongside"
rb_start=$(now)
rb_rc=0
URL="$URLS" ARMS="$ARMS" make -C "$REPO" --no-print-directory bench-chromium-request-run TAG="$TAG" \
    REPS="$REPS" WARMUP="$WARMUP" RESULTS="$OUT/reqbench" > "$OUT/reqbench.log" 2>&1 || rb_rc=$?
rb_end=$(now)
touch "$DONE"
wait "$BR_PID" || true
trap - EXIT

python3 - "$OUT" "$rb_start" "$rb_end" "$rb_rc" "$TAG" "$CPU" "$REPS" "$WARMUP" "$ARMS" "$URLS" <<'PY'
import glob, json, os, sys
out, start, end, rc, tag, cpu, reps, warmup, arms, urls = sys.argv[1:]
batches = []
for run in sorted(glob.glob(os.path.join(out, "browserrun-*", "run.json"))):
    d = os.path.dirname(run)
    rows_path = os.path.join(d, "browserrun.jsonl")
    ts = []
    if os.path.exists(rows_path):
        with open(rows_path) as handle:
            ts = [json.loads(line)["ts"] for line in handle if line.strip()]
    batches.append({"dir": os.path.basename(d), "first_ts": min(ts, default=None),
                    "last_ts": max(ts, default=None), "requests": len(ts),
                    "summary": os.path.exists(os.path.join(d, "summary.json"))})
record = {"tag": tag, "cpu": int(cpu), "reps": int(reps), "warmup": int(warmup),
          "arms": arms.split(","), "urls": urls.split(","),
          "resolver": "guest default (pasta, the host's DNS): live sites",
          "reqbench": {"start": float(start), "end": float(end), "exit": int(rc)},
          "browserrun_batches": batches}
with open(os.path.join(out, "live.json"), "w") as handle:
    json.dump(record, handle, indent=1, sort_keys=True)
    handle.write("\n")
print(f"reqbench exit {rc}; {len(batches)} Browser Run batch(es), "
      f"{sum(b['requests'] for b in batches)} hosted requests", file=sys.stderr)
PY
log "records: $OUT"
exit "$rb_rc"
