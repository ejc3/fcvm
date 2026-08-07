#!/usr/bin/env bash
#
# Clone hot-path latency benchmark: spawn -> exec-server-ready.
#
# Boots a minimal alpine VM, snapshots it, serves the snapshot, then times N
# `fcvm snapshot run` cycles from process spawn until fc-agent's exec server is
# reachable. Readiness is EVENT-DRIVEN: a `tail -F | grep -m1` on the clone's
# RUST_LOG=fcvm=debug log releases the instant the marker line is written (no
# fixed-interval sleeping in the measurement loop).
#
# Also parses the per-clone debug timeline into a stage breakdown so a change in
# total latency can be attributed to a specific stage.
#
# Results go to /tmp (never committed):
#   /tmp/fcvm-clone-latency-<label>/
#     clone-NN.log     per-clone debug log
#     samples.tsv      one row per clone: total_ms + stage_ms columns
#     summary.txt      p50/min/max + stage breakdown (also printed to stdout)
#
# Usage:
#   bench/clone-latency.sh [label] [iterations]
#
# Env:
#   FCVM_BIN   fcvm binary to measure (default: ./target/release/fcvm). When it
#              lives outside the repo (A/B against a stashed build), also set
#              FC_AGENT_PATH — fcvm looks for fc-agent next to its own binary.
#   N          iterations (default 10, overridden by $2)
#   KEEP       set to 1 to leave the baseline VM + serve process running
#
# A/B example (interleaved so host load drift hits both arms equally):
#   git stash push -- src/ && make build && cp target/release/fcvm /tmp/fcvm-base
#   git stash pop && make build && cp target/release/fcvm /tmp/fcvm-after
#   for r in 1 2; do
#     FC_AGENT_PATH=$PWD/target/release/fc-agent FCVM_BIN=/tmp/fcvm-base  bench/clone-latency.sh base-r$r
#     FC_AGENT_PATH=$PWD/target/release/fc-agent FCVM_BIN=/tmp/fcvm-after bench/clone-latency.sh after-r$r
#   done
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

LABEL="${1:-run}"
N="${2:-${N:-10}}"
FCVM_BIN="${FCVM_BIN:-$REPO_ROOT/target/release/fcvm}"
OUT="/tmp/fcvm-clone-latency-${LABEL}"
IMAGE="${IMAGE:-docker.io/library/alpine:latest}"

# Marker written by `fcvm snapshot run` once fc-agent's output channel is
# connected and its exec server is accepting (src/commands/snapshot.rs).
READY_MARKER="exec server ready"

if [[ ! -x "$FCVM_BIN" ]]; then
    echo "ERROR: fcvm binary not found/executable: $FCVM_BIN" >&2
    echo "       run 'make build' first" >&2
    exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"

STAMP="$(date +%s)"
BASE="clbench-${LABEL}-base-${STAMP}"
SNAP="clbench-${LABEL}-snap-${STAMP}"

BASE_PID=""
SERVE_PID=""

cleanup() {
    local rc=$?
    set +e
    if [[ "${KEEP:-0}" != "1" ]]; then
        [[ -n "$SERVE_PID" ]] && kill -TERM "$SERVE_PID" 2>/dev/null
        [[ -n "$BASE_PID" ]] && kill -TERM "$BASE_PID" 2>/dev/null
        # Give both a moment to tear their VMs down cleanly.
        local waited=0
        while [[ $waited -lt 100 ]]; do
            kill -0 "$SERVE_PID" 2>/dev/null || kill -0 "$BASE_PID" 2>/dev/null || break
            sleep 0.1
            waited=$((waited + 1))
        done
        [[ -n "$SERVE_PID" ]] && kill -KILL "$SERVE_PID" 2>/dev/null
        [[ -n "$BASE_PID" ]] && kill -KILL "$BASE_PID" 2>/dev/null
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

echo "==> fcvm clone latency bench (label=$LABEL, N=$N)"
echo "    binary : $FCVM_BIN"
echo "    version: $(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)$(git -C "$REPO_ROOT" diff --quiet 2>/dev/null || echo '+dirty')"
echo "    output : $OUT"
echo "    load   : $(cut -d' ' -f1-3 /proc/loadavg)"

# ---------------------------------------------------------------------------
# 1. Baseline VM
# ---------------------------------------------------------------------------
echo "==> booting baseline VM ($IMAGE)"
RUST_LOG=fcvm=debug "$FCVM_BIN" podman run --name "$BASE" "$IMAGE" sleep infinity \
    >"$OUT/baseline.log" 2>&1 &
BASE_PID=$!

# Baseline readiness: the state file flips to healthy. Poll it (this is setup,
# not the measured path) but keep the interval tight.
deadline=$((SECONDS + 300))
while :; do
    if ! kill -0 "$BASE_PID" 2>/dev/null; then
        echo "ERROR: baseline VM exited early; see $OUT/baseline.log" >&2
        tail -30 "$OUT/baseline.log" >&2
        exit 1
    fi
    if "$FCVM_BIN" ls --json 2>/dev/null \
        | jq -e --arg n "$BASE" 'map(select(.name == $n and .health_status == "healthy")) | length > 0' \
             >/dev/null 2>&1; then
        break
    fi
    if [[ $SECONDS -gt $deadline ]]; then
        echo "ERROR: baseline VM did not become healthy in 300s" >&2
        tail -30 "$OUT/baseline.log" >&2
        exit 1
    fi
    sleep 0.2
done
echo "    baseline healthy (pid $BASE_PID)"

# ---------------------------------------------------------------------------
# 2. Snapshot + serve
# ---------------------------------------------------------------------------
echo "==> creating snapshot $SNAP"
"$FCVM_BIN" snapshot create --pid "$BASE_PID" --tag "$SNAP" >"$OUT/snapshot-create.log" 2>&1

echo "==> serving snapshot"
"$FCVM_BIN" snapshot serve "$SNAP" >"$OUT/serve.log" 2>&1 &
SERVE_SHELL_PID=$!
deadline=$((SECONDS + 60))
while :; do
    SERVE_PID="$(sed -n 's/^ *Serve PID: *//p' "$OUT/serve.log" | head -1)"
    [[ -n "$SERVE_PID" ]] && break
    if ! kill -0 "$SERVE_SHELL_PID" 2>/dev/null; then
        echo "ERROR: serve process exited; see $OUT/serve.log" >&2
        cat "$OUT/serve.log" >&2
        exit 1
    fi
    [[ $SECONDS -gt $deadline ]] && { echo "ERROR: serve did not print its PID" >&2; exit 1; }
    sleep 0.05
done
echo "    serve pid $SERVE_PID"

# ---------------------------------------------------------------------------
# 3. Timed clone cycles
# ---------------------------------------------------------------------------
echo "==> timing $N clone spawn->exec-ready cycles"
: >"$OUT/totals.txt"
CLONE_PIDS=()

for i in $(seq 1 "$N"); do
    idx="$(printf '%02d' "$i")"
    clone="clbench-${LABEL}-c${idx}-${STAMP}"
    log="$OUT/clone-${idx}.log"
    : >"$log"

    t0=$(date +%s%N)
    RUST_LOG=fcvm=debug "$FCVM_BIN" snapshot run --pid "$SERVE_PID" --name "$clone" \
        >"$log" 2>&1 &
    cpid=$!
    CLONE_PIDS+=("$cpid")

    # Event-driven readiness: grep releases the pipeline the moment the marker
    # line lands in the log; tail then dies on SIGPIPE.
    if ! timeout 120 grep -q -m1 -F "$READY_MARKER" \
            < <(tail -n +1 -F --pid=$$ "$log" 2>/dev/null); then
        echo "ERROR: clone $idx never reached '$READY_MARKER'; see $log" >&2
        tail -40 "$log" >&2
        exit 1
    fi
    t1=$(date +%s%N)

    ms=$(( (t1 - t0) / 1000000 ))
    echo "$ms" >>"$OUT/totals.txt"
    printf '    clone %s: %s ms\n' "$idx" "$ms"
done

# ---------------------------------------------------------------------------
# 4. Tear down the clones before analysing (keeps the host quiet)
# ---------------------------------------------------------------------------
for cpid in "${CLONE_PIDS[@]}"; do kill -TERM "$cpid" 2>/dev/null || true; done
for cpid in "${CLONE_PIDS[@]}"; do wait "$cpid" 2>/dev/null || true; done

# ---------------------------------------------------------------------------
# 5. Stage breakdown from the RUST_LOG=fcvm=debug timeline
# ---------------------------------------------------------------------------
python3 "$REPO_ROOT/bench/clone-latency-report.py" "$OUT" | tee "$OUT/summary.txt"

echo
echo "==> results in $OUT"
