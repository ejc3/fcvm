#!/usr/bin/env bash
# ComputeSDK Burst TTI against `fcvm serve`.
#
# Runs computesdk/benchmarks' own sandbox TTI task (benchmarks/sandbox/tti.bench.ts) at a
# pinned commit, with one participant added: `fcvm`, the provider in fcvm-provider.ts,
# pointed at an `fcvm serve` on this host. The task, its burst schedule, its timer and its
# statistics are the harness's; the only change to the checkout is that participant.
#
#   bench/computesdk/run.sh            # via `make bench-computesdk`
#
# Knobs (environment):
#   CONCURRENCY=100      sandboxes launched at once (the published setting)
#   ITERATIONS=CONCURRENCY  sandboxes per run; the published runs set it equal to CONCURRENCY,
#                        which makes one run one burst
#   BURSTS=5             runs, one after another
#   WORK=<dir>           scratch for the harness checkout and results
#                        (default /mnt/fcvm-btrfs/computesdk-bench)
#   OUT=<dir>            where results go (default $WORK/results/<UTC time>)
#
# fcvm serve passes --startup-snapshot, so each sandbox restores its image's startup snapshot,
# with the container already running. Before the first run one sandbox is created and
# destroyed, untimed, which creates the pre-start and startup snapshots. The published runs
# measure providers whose images or templates already exist, so a run here starts from the
# same place.
set -euo pipefail

HARNESS_REPO=https://github.com/computesdk/benchmarks.git
HARNESS_COMMIT=9135206e8153ae3c6e0d556e73a47696c7deeabb # 2026-10-22
PROVIDER_PKG=@computesdk/provider@2.1.14                # the version the harness lockfile pins
PNPM=(npx --yes pnpm@10.33.0)

CONCURRENCY=${CONCURRENCY:-100}
ITERATIONS=${ITERATIONS:-$CONCURRENCY}
BURSTS=${BURSTS:-5}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
FCVM=$REPO/target/release/fcvm
WORK=${WORK:-/mnt/fcvm-btrfs/computesdk-bench}
OUT=${OUT:-$WORK/results/$(date -u +%Y%m%dT%H%M%SZ)}

[ -x "$FCVM" ] || { echo "missing $FCVM: run make build" >&2; exit 2; }
for tool in node git curl jq python3; do
  command -v "$tool" >/dev/null || { echo "BLOCKED: $tool missing" >&2; exit 2; }
done
# A run never mixes with another: one at a time per WORK (the harness checkout is shared),
# and OUT must be new or empty so the summary covers this run's bursts only.
# The OUT check runs under the WORK lock, so two runs cannot both pass it.
mkdir -p "$WORK"
exec 9>>"$WORK/.lock"
flock -n 9 || { echo "another bench-computesdk run holds $WORK/.lock" >&2; exit 2; }
if [ -e "$OUT" ] && [ -n "$(ls -A "$OUT")" ]; then
  echo "OUT=$OUT already holds results; pick a new OUT" >&2
  exit 2
fi
mkdir -p "$OUT"

# 1. The harness at the pinned commit, with the fcvm participant added.
H=$WORK/benchmarks
if [ ! -d "$H/.git" ]; then
  git clone -q "$HARNESS_REPO" "$H"
fi
git -C "$H" fetch -q origin "$HARNESS_COMMIT"
git -C "$H" checkout -q --force "$HARNESS_COMMIT"
git -C "$H" clean -qfdx -e node_modules -e 'packages/*/dist'
(
  cd "$H"
  "${PNPM[@]}" install --frozen-lockfile --ignore-scripts >/dev/null
  "${PNPM[@]}" add -w --ignore-scripts "$PROVIDER_PKG" >/dev/null
  "${PNPM[@]}" -r --filter './packages/**' run build >/dev/null
)
cp "$HERE/fcvm-provider.ts" "$H/benchmarks/sandbox/fcvm-provider.ts"
python3 - "$H/benchmarks/sandbox/providers.ts" <<'PY'
import sys
path = sys.argv[1]
src = open(path).read()
anchor = "export const providers: ProviderConfig[] = ["
assert anchor in src, "providers.ts no longer has the providers array"
src = "import { fcvm } from './fcvm-provider.js';\n" + src
src = src.replace(anchor, anchor + """
  {
    name: 'fcvm',
    requiredEnvVars: ['FCVM_SERVE_URL'],
    createCompute: () => fcvm({ url: process.env.FCVM_SERVE_URL!, runtime: 'node' }),
  },""", 1)
open(path, "w").write(src)
PY

# 2. fcvm serve on a port the kernel picks.
SERVE_LOG=$OUT/serve.log
# serve logs its bound port at info. The sandboxes inherit the filter, so they log warnings
# and their restored records (stdout) only.
RUST_LOG=${RUST_LOG:-warn,fcvm::commands::serve=info} \
  "$FCVM" serve --port 0 >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!
trap 'kill -TERM $SERVE_PID 2>/dev/null; wait $SERVE_PID 2>/dev/null || true' EXIT
PORT=""
for _ in $(seq 1 300); do
  PORT=$(sed -n 's/.*Listening.*addr=127\.0\.0\.1:\([0-9]*\).*/\1/p' "$SERVE_LOG" | head -1)
  [ -n "$PORT" ] && break
  kill -0 "$SERVE_PID" 2>/dev/null || { echo "fcvm serve exited:" >&2; tail -20 "$SERVE_LOG" >&2; exit 1; }
  sleep 0.1
done
[ -n "$PORT" ] || { echo "fcvm serve did not report its port" >&2; exit 1; }
URL=http://127.0.0.1:$PORT

# 3. Untimed warm-up: one sandbox, created and destroyed. It creates the snapshot.
ID=$(curl -sf -X POST "$URL/v1/sandboxes" -H 'Content-Type: application/json' \
  -d '{"runtime":"node"}' --max-time 900 | jq -er '.data.sandboxId')
curl -sf -X POST "$URL/s/$ID/run/command" -H 'Content-Type: application/json' \
  -d '{"command":"node -v"}' | jq -e '.data.exit_code == 0' >/dev/null
curl -sf -X DELETE "$URL/v1/sandboxes/$ID" >/dev/null

# 4. The runs. The harness writes results/burst_tti/<date>.json for each.
{
  echo "concurrency=$CONCURRENCY iterations=$ITERATIONS bursts=$BURSTS"
  echo "harness=$HARNESS_REPO@$HARNESS_COMMIT provider_pkg=$PROVIDER_PKG"
  echo "fcvm_commit=$(git -C "$REPO" rev-parse HEAD) fcvm_dirty=$(git -C "$REPO" status --porcelain | grep -vc '^??' || true)"
  echo "node=$(node -v) kernel=$(uname -r) arch=$(uname -m) cpus=$(nproc)"
  echo "mem_kib=$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
} >"$OUT/env.txt"
for b in $(seq 1 "$BURSTS"); do
  rm -rf "$H/results/burst_tti"
  (
    cd "$H"
    FCVM_SERVE_URL=$URL npx tsx packages/benchsdk-runner/dist/bin.js run \
      benchmarks/sandbox/tti.bench.ts --provider fcvm \
      --iterations "$ITERATIONS" --concurrency "$CONCURRENCY" --no-ingest
  ) >"$OUT/burst-$b.log" 2>&1
  cp "$H/results/burst_tti/latest.json" "$OUT/burst-$b.json"
  echo "burst $b: $(jq -r '.results[] | select(.provider=="fcvm") | "median=\(.summary.ttiMs.median|floor) p95=\(.summary.ttiMs.p95|floor) p99=\(.summary.ttiMs.p99|floor) success=\(.successRate)"' "$OUT/burst-$b.json")"
done

python3 "$HERE/summarize.py" "$OUT"/burst-*.json >"$OUT/summary.txt"
cat "$OUT/summary.txt"
