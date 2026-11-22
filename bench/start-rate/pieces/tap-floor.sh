#!/usr/bin/env bash
# The tap-attach campaign: what a clone's start and stop still cost in rtnl_mutex when its
# namespace, pasta and bridge are reused. Every cell holds TOTAL namespaces (each with a
# pasta), split between the held set and the cell's own ops.
# Usage: tap-floor.sh OUT_ROOT   (env: TOTAL, default 2000; RUNGS, C:COUNT pairs;
#        NO_PASTA=1 for namespaces with the TAP and bridge and no pasta;
#        WARM, attach and detach cycles before the timed one, default 1)
# OUT_ROOT/plain holds the measured cells, OUT_ROOT/traced the same cells under tap.bt
# (their timings are not measurements), with a tap-summary.json in each traced cell.
set -uo pipefail
out_root=$1
TOTAL=${TOTAL:-2000}
RUNGS=${RUNGS:-"1:200 32:400 128:1000 512:1000"}
here=$(cd "$(dirname "$0")" && pwd)
extra="--tap-warm-cycles ${WARM:-1} ${NO_PASTA:+--tap-no-pasta}"
bin=${START_PIECES:-$here/../../../target/release/start-pieces}
for rung in $RUNGS; do
  n=${rung##*:}
  RUNGS=$rung ALIVES=$((TOTAL - n)) ARGS="$extra" \
    bash "$here/matrix.sh" "$out_root/plain" tap-attach
  RUNGS=$rung ALIVES=$((TOTAL - n)) PERF_EVENTS=tap ARGS="--perf-cmd $here/tap-bt.sh $extra" \
    bash "$here/matrix.sh" "$out_root/traced" tap-attach
  cell="$out_root/traced/tap-attach-c${rung%%:*}-n$n-a$((TOTAL - n))"
  "$bin" tap-summary "$cell" > "$cell/tap-summary.json" || echo "WARN: no summary for $cell"
done
echo TAP-FLOOR-DONE
