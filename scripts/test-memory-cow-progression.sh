#!/bin/bash
# Test CoW progression: measure how shared memory decreases as clones write more pages.
# Shows the file-backend sharing degrades linearly with writes.
#
# Usage: ./scripts/test-memory-cow-progression.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FCVM="${SCRIPT_DIR}/../target/release/fcvm"
SUFFIX=$(date +%s)
BASELINE="cow-base-$SUFFIX"
SNAP="cow-snap-$SUFFIX"
CLONE1="cow-c1-$SUFFIX"
CLONE2="cow-c2-$SUFFIX"
MEM_MIB=512
FILL_MIB=200

cleanup_pids=()
cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    for pid in "${cleanup_pids[@]}"; do
        kill -0 "$pid" 2>/dev/null && { kill "$pid" 2>/dev/null; sleep 1; kill -9 "$pid" 2>/dev/null; } || true
    done
}
trap cleanup EXIT

get_fc_pid() {
    local fcvm_pid=$1
    pgrep -f "firecracker.*--api-sock" --parent "$fcvm_pid" 2>/dev/null | head -1 || \
        for child in $(pgrep -P "$fcvm_pid" 2>/dev/null); do
            pgrep -f "firecracker.*--api-sock" --parent "$child" 2>/dev/null | head -1 && break || true
        done
}

get_rss_mb() { grep VmRSS /proc/"$1"/status 2>/dev/null | awk '{printf "%d", $2/1024}'; }
get_pss_mb() { grep '^Pss:' /proc/"$1"/smaps_rollup 2>/dev/null | awk '{sum+=$2} END{printf "%d", sum/1024}'; }

wait_healthy() {
    local name=$1
    for i in $(seq 1 120); do
        $FCVM ls --json 2>/dev/null | jq -e ".[] | select(.name == \"$name\" and .health_status == \"healthy\")" > /dev/null 2>&1 && return 0
        sleep 1
    done
    echo "TIMEOUT waiting for $name"; return 1
}

get_pid() { $FCVM ls --json 2>/dev/null | jq -r ".[] | select(.name == \"$1\") | .pid"; }

measure() {
    local label=$1 fc1=$2 fc2=$3
    local rss1=$(get_rss_mb "$fc1") pss1=$(get_pss_mb "$fc1")
    local rss2=$(get_rss_mb "$fc2") pss2=$(get_pss_mb "$fc2")
    local shared1=$((rss1 - pss1))
    local shared2=$((rss2 - pss2))
    printf "%-25s  C1: RSS=%3dMB PSS=%3dMB shared=%3dMB  |  C2: RSS=%3dMB PSS=%3dMB shared=%3dMB\n" \
        "$label" "$rss1" "$pss1" "$shared1" "$rss2" "$pss2" "$shared2"
}

echo "=== CoW Progression Test ==="
echo "512MB VM, 200MB random data in snapshot, file backend"
echo ""

# Setup: baseline -> fill -> snapshot -> kill baseline
echo "Setting up..."
$FCVM podman run --name "$BASELINE" --network rootless --mem $MEM_MIB --setup \
    ecr-public.aws.com/docker/library/alpine:latest sleep 3600 &
BASELINE_BG=$!; cleanup_pids+=("$BASELINE_BG")
wait_healthy "$BASELINE"
BPID=$(get_pid "$BASELINE")
echo "  Baseline healthy (PID $BPID)"

$FCVM exec --pid "$BPID" --vm -- sh -c \
    "dd if=/dev/urandom of=/dev/shm/fill bs=1M count=$FILL_MIB 2>/dev/null; echo filled"

$FCVM snapshot create --pid "$BPID" --tag "$SNAP" > /dev/null
echo "  Snapshot created"

kill "$BASELINE_BG" 2>/dev/null || true; sleep 2; kill -9 "$BASELINE_BG" 2>/dev/null || true
wait "$BASELINE_BG" 2>/dev/null || true
echo "  Baseline killed"

# Clone twice via file backend
$FCVM snapshot run --snapshot "$SNAP" --name "$CLONE1" --network rootless &
C1_BG=$!; cleanup_pids+=("$C1_BG")
$FCVM snapshot run --snapshot "$SNAP" --name "$CLONE2" --network rootless &
C2_BG=$!; cleanup_pids+=("$C2_BG")

wait_healthy "$CLONE1"
wait_healthy "$CLONE2"
C1_PID=$(get_pid "$CLONE1")
C2_PID=$(get_pid "$CLONE2")
FC1=$(get_fc_pid "$C1_BG")
FC2=$(get_fc_pid "$C2_BG")
echo "  Clone 1: PID=$C1_PID fc=$FC1"
echo "  Clone 2: PID=$C2_PID fc=$FC2"
echo ""

# Header
echo "=== Results ==="
printf "%-25s  %-40s  |  %-40s\n" "Step" "Clone 1" "Clone 2"
echo "-------------------------------------------------------------------------------------------------------------------"

# Step 0: Read all pages first (fault them all in)
$FCVM exec --pid "$C1_PID" --vm -- sh -c "cat /dev/shm/fill > /dev/null" 2>/dev/null || true
$FCVM exec --pid "$C2_PID" --vm -- sh -c "cat /dev/shm/fill > /dev/null" 2>/dev/null || true
measure "0MB written (read only)" "$FC1" "$FC2"

# Progressive writes: write unique random data to each clone
# Each step writes ADDITIONAL data -- accumulates dirty pages
WRITTEN=0
for step_mb in 25 50 50 75; do
    WRITTEN=$((WRITTEN + step_mb))
    # Write unique data to each clone (different data = no KSM dedup)
    $FCVM exec --pid "$C1_PID" --vm -- sh -c \
        "dd if=/dev/urandom of=/dev/shm/dirty bs=1M count=$step_mb oflag=append conv=notrunc 2>/dev/null; true" 2>/dev/null || true
    $FCVM exec --pid "$C2_PID" --vm -- sh -c \
        "dd if=/dev/urandom of=/dev/shm/dirty bs=1M count=$step_mb oflag=append conv=notrunc 2>/dev/null; true" 2>/dev/null || true
    measure "${WRITTEN}MB written" "$FC1" "$FC2"
done

# Final: overwrite the original fill data too
$FCVM exec --pid "$C1_PID" --vm -- sh -c \
    "dd if=/dev/urandom of=/dev/shm/fill bs=1M count=$FILL_MIB 2>/dev/null; true" 2>/dev/null || true
$FCVM exec --pid "$C2_PID" --vm -- sh -c \
    "dd if=/dev/urandom of=/dev/shm/fill bs=1M count=$FILL_MIB 2>/dev/null; true" 2>/dev/null || true
measure "${WRITTEN}MB + overwrite 200MB" "$FC1" "$FC2"

echo ""
echo "Expected: 'shared' column stays ~150MB for new writes (fresh pages),"
echo "          but drops to ~50MB when overwriting snapshot data (CoW triggered)."
echo ""
echo "Done!"
