#!/usr/bin/env bash
# Run pieces over a concurrency x alive matrix through `make bench-start-pieces`.
# Usage: matrix.sh OUT_ROOT PIECE[,PIECE...]   (env: RUNGS, ALIVES, SNAPSHOT_DIR, FIRECRACKER,
#        UFFD_SOCKET, PERF_FREQ, PERF_EVENTS, LOAD_MAX, COLD, START_PIECES (a binary to
#        measure instead of the built one, for an A/B), ARGS)
# A rung is C:COUNT. A cell whose summary.json exists is skipped, so a rerun resumes.
set -uo pipefail
RUNGS=${RUNGS:-"1:100 32:400 128:1000 512:1000"}
ALIVES=${ALIVES:-"0 1000"}
LOAD_MAX=${LOAD_MAX:-2}  # busy CPUs (from /proc/stat over 1 s) below which the host is quiet
QUIET_TRIES=${QUIET_TRIES:-600}  # one-second samples quiet() takes before it gives up

# The previous cell's teardown has settled and nothing else is loading the host. Network
# namespaces are destroyed after their last process exits, by the kernel's netns workqueue,
# which holds rtnl_mutex for whole batches (seconds for 1000 namespaces): a cell started while
# it runs measures that instead of itself, so wait for it (its worker is named "+netns" while
# it works).
busy_cores() {  # host busy CPUs over one second, from /proc/stat
  local a b
  a=$(awk '/^cpu /{print $2+$3+$4+$7+$8+$9}' /proc/stat); sleep 1
  b=$(awk '/^cpu /{print $2+$3+$4+$7+$8+$9}' /proc/stat)
  echo $(( (b - a) / $(getconf CLK_TCK) ))
}
quiet() {
  local l n start=$SECONDS
  for _ in $(seq 1 "$QUIET_TRIES"); do
    l=$(busy_cores)
    n=$(ps -eo comm= | grep -c '+netns')
    if [ "$l" -lt "$LOAD_MAX" ] && [ "$n" -eq 0 ] \
        && ! pgrep firecracker >/dev/null && ! pgrep -f '[s]tart-pieces instance' >/dev/null; then
      echo "quiet after $((SECONDS - start)) s"
      return 0
    fi
  done
  echo "host not quiet before $1: $l CPUs busy, netns workers busy $n"
  return 1
}

# A cell counts once its summary says nothing failed; any other cell is run again.
cell_done() {
  [ -f "$1/summary.json" ] &&
    jq -e '.timed.failed == 0 and .teardown.failed == 0 and ((.perf_errors // []) | length) == 0' \
      "$1/summary.json" > /dev/null
}

# Sourced (by test_pieces.py) for its functions only.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then return 0; fi

out_root=$1
pieces=$2
cd "$(dirname "$0")/../../.."
mkdir -p "$out_root"

for piece in ${pieces//,/ }; do
  for alive in $ALIVES; do
    for rung in $RUNGS; do
      c=${rung%%:*}; n=${rung##*:}
      cell="$out_root/$piece-c$c-n$n-a$alive"
      cell_done "$cell" && continue
      rm -rf "$cell"
      quiet "$cell" || { echo "SKIPPED $cell"; continue; }
      echo "=== $(date -u +%H:%M:%S) $piece C=$c N=$n ALIVE=$alive"
      make --no-print-directory bench-start-pieces PIECE="$piece" OUT="$cell" COUNT="$n" COLD="${COLD:-}" \
        START_PIECES="${START_PIECES:-$PWD/target/release/start-pieces}" ARGS="${ARGS:-}" \
        CONCURRENCY="$c" ALIVE="$alive" SNAPSHOT_DIR="${SNAPSHOT_DIR:-}" \
        FIRECRACKER="${FIRECRACKER:-}" UFFD_SOCKET="${UFFD_SOCKET:-}" \
        PERF_FREQ="${PERF_FREQ:-}" PERF_EVENTS="${PERF_EVENTS:-}" > "$cell.log" 2>&1
      echo "exit=$? $(jq -c '{ops_s: .timed.ops_per_s, p50: .timed.latency_ms.p50, p99: .timed.latency_ms.p99, cg: .timed.cgroup_cpu_s_per_op_net, host: .timed.host_cpu_s_per_op_net, failed: .timed.failed}' "$cell/summary.json" 2>/dev/null)"
    done
  done
done
echo MATRIX-DONE
