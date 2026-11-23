#!/usr/bin/env bash
# The netpool campaign: claims from a running pool and the pool's resets, with ALIVE leases
# held. Starts one pool of SLOTS namespaces in a data directory of its own, runs the cells,
# then stops the pool and waits for the kernel to finish destroying its namespaces.
# Usage: netpool.sh OUT_ROOT   (env: RUNGS, C:COUNT pairs; ALIVE, default 1000; SLOTS,
#        default ALIVE + the largest COUNT; FCVM, the fcvm binary; TRACE=1 to also run each
#        cell under kernel.bt and summarize it with rtnl.py)
set -uo pipefail
out_root=$1
RUNGS=${RUNGS:-"1:200 32:1000 128:1000 512:1000"}
ALIVE=${ALIVE:-1000}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
fcvm=${FCVM:-$repo/target/release/fcvm}
bin=${START_PIECES:-$repo/target/release/start-pieces}
max=0
for rung in $RUNGS; do n=${rung##*:}; [ "$n" -gt "$max" ] && max=$n; done
SLOTS=${SLOTS:-$((ALIVE + max))}
source "$here/matrix.sh"  # quiet()
mkdir -p "$out_root"
data_dir=$(mktemp -d /mnt/fcvm-btrfs/netpool-bench.XXXXXX)

quiet "the pool's start" || exit 1
build_start=$SECONDS
FCVM_DATA_DIR=$data_dir "$fcvm" netpool serve --slots "$SLOTS" > "$out_root/pool.log" 2>&1 &
pool=$!
stop_pool() {
  kill -TERM "$pool" 2> /dev/null
  wait "$pool" 2> /dev/null
  rm -rf "$data_dir"
}
trap stop_pool EXIT
stats() { FCVM_DATA_DIR=$data_dir "$fcvm" netpool stats 2> /dev/null; }
until [ "$(stats | jq -r .ready)" = "$SLOTS" ]; do
  kill -0 "$pool" 2> /dev/null || { echo "the pool exited while building; see $out_root/pool.log"; exit 1; }
  sleep 1
done
echo "pool of $SLOTS namespaces built in $((SECONDS - build_start)) s"
stats > "$out_root/pool-built.json"
socket=$(echo "$data_dir"/netpool/*/pool.sock)

cell() {  # OUT C COUNT
  local cell=$1/netpool-c$2-n$3-a$ALIVE
  [ -f "$cell/summary.json" ] && return 0
  rm -rf "$cell"
  mkdir -p "$1"
  quiet "netpool C=$2" || return 1
  echo "=== $(date -u +%H:%M:%S) netpool C=$2 N=$3 ALIVE=$ALIVE"
  make --no-print-directory -C "$repo" bench-start-pieces PIECE=netpool OUT="$cell" COUNT="$3" \
    START_PIECES="$bin" CONCURRENCY="$2" ALIVE="$ALIVE" \
    ARGS="--netpool-socket $socket --steady-secs 5" > "$cell.log" 2>&1
  echo "exit=$? $(jq -c '{claims_s: .timed.ops_per_s, p50: .timed.latency_ms.p50, p99: .timed.latency_ms.p99,
    failed: .timed.failed, resets_s: .pool.resets_per_s, reset_ms: .pool.reset_ms_mean, destroys: .pool.destroys}' \
    "$cell/summary.json" 2>&1)"
}

for rung in $RUNGS; do
  c=${rung%%:*}; n=${rung##*:}
  cell "$out_root/plain" "$c" "$n"
  if [ -n "${TRACE:-}" ]; then
    traced=$out_root/traced/netpool-c$c-n$n-a$ALIVE
    mkdir -p "$out_root/traced"
    if [ ! -f "$traced/summary.json" ]; then
      quiet "traced netpool C=$c" || continue
      rm -f "$traced.bt" "$traced.bt.err"
      sudo bpftrace "$here/kernel.bt" > "$traced.bt" 2> "$traced.bt.err" &
      bt=$!
      sleep 8
      cell "$out_root/traced" "$c" "$n"
      quiet "the end of the traced cell"
      sudo kill -INT "$bt"; wait "$bt"
      sudo chown "$(id -u):$(id -g)" "$traced.bt" 2> /dev/null
      python3 "$here/rtnl.py" "$traced.bt" "$((ALIVE + n))" > "$traced/rtnl.json" || echo "WARN: no rtnl summary for $traced"
    fi
  fi
done
stats > "$out_root/pool-end.json"
drain_start=$SECONDS
trap - EXIT
stop_pool
quiet "the pool's namespaces being destroyed"
echo "pool of $SLOTS namespaces destroyed in $((SECONDS - drain_start)) s"
echo NETPOOL-DONE
