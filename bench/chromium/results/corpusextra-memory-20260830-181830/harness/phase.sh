#!/bin/bash
# One measured phase, with the idle-watchdog keepalive in front of it.
#   $1 = PHASES value for corpus_extra.sh, rest = extra env assignments
set -euo pipefail
cd ~/src/fcvm && . ~/.cargo/env
export REPO=$PWD
PH="$1"; shift
STAMP=$(date +%Y%m%d-%H%M%S)
RES="$PWD/bench/chromium/results/corpusextra-$PH-$STAMP"

echo "== keepalive burst before $PH ($(date +%H:%M:%S))"
CORES=96 SECS=120 bash /tmp/keepalive.sh

echo "== waiting for the 1-minute load to fall back under 1.5"
until awk '{exit !($1 < 1.5)}' /proc/loadavg; do sleep 10; done
echo "   load=$(cut -d' ' -f1 /proc/loadavg) at $(date +%H:%M:%S)"

env "$@" PHASES="$PH" RESULTS="$RES" LOGDIR="/tmp/corpusextra-$PH-$STAMP" \
    SETTLE_WAIT_SECS=900 bash bench/chromium/corpus_extra.sh 2>&1 | tail -90
echo "EXTRA_EXIT=${PIPESTATUS[0]} RESULTS=$RES"

mkdir -p "$RES/harness"
cp bench/chromium/hostcdp.sh bench/chromium/corpus_mem.py bench/chromium/corpus_extra.sh \
   bench/chromium/test_hostcdp_corpus.py "$RES/harness/" 2>/dev/null || true
git -C "$PWD" diff -- bench/chromium/hostcdp.sh > "$RES/harness/hostcdp.sh.diff" || true
sha256sum "$RES/harness/"* > "$RES/harness/SHA256SUMS" || true
bash /tmp/mirror.sh "$RES"
