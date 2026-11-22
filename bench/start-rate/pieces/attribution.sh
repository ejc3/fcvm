#!/usr/bin/env bash
# Kernel attribution pass for pieces: one cell per piece with kernel.bt running over it
# (rtnl_mutex hold and wait, RCU syncs, netns cleanup, KVM VM create and destroy, CoW and
# userfaultfd faults, KVM exits by class, execs).
# Timings from these cells are not measurements: the tracers cost CPU.
# Usage: attribution.sh OUT_ROOT PIECE[,PIECE...]   (env: C, N, ALIVE, plus matrix.sh's)
set -uo pipefail
out_root=$1
pieces=$2
C=${C:-128}; N=${N:-1000}; ALIVE=${ALIVE:-1000}
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/../../.."
mkdir -p "$out_root"
for piece in ${pieces//,/ }; do
  cell="$out_root/$piece-c$C-n$N-a$ALIVE"
  [ -f "$cell/summary.json" ] && continue
  rm -rf "$cell" "$cell.bt"
  sudo bpftrace "$here/kernel.bt" > "$cell.bt" 2>&1 &
  bt=$!
  sleep 5
  echo "=== $(date -u +%H:%M:%S) attribution $piece C=$C N=$N ALIVE=$ALIVE"
  make --no-print-directory bench-start-pieces PIECE="$piece" OUT="$cell" COUNT="$N" \
    START_PIECES="${START_PIECES:-$PWD/target/release/start-pieces}" \
    CONCURRENCY="$C" ALIVE="$ALIVE" SNAPSHOT_DIR="${SNAPSHOT_DIR:-}" FIRECRACKER="${FIRECRACKER:-}" \
    UFFD_SOCKET="${UFFD_SOCKET:-}" ARGS="--steady-secs 5" > "$cell.log" 2>&1
  echo "exit=$?"
  sudo kill -INT "$bt"
  wait "$bt"
  sudo chown "$(id -u):$(id -g)" "$cell.bt" 2>/dev/null
done
echo ATTRIBUTION-DONE
