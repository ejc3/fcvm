#!/bin/bash
# Test memory sharing between file-backend clones.
# Result: File clones DO share memory — MAP_PRIVATE mmap shares read-only pages via page cache.
#
# Usage: ./scripts/test-memory-sharing-file.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FCVM="${SCRIPT_DIR}/../target/release/fcvm"
SUFFIX=$(date +%s)
BASELINE="memf-base-$SUFFIX"
SNAP="memf-snap-$SUFFIX"
MEM_MIB=512
FILL_MIB=200

cleanup_pids=()
cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    for pid in "${cleanup_pids[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            echo "Killing PID $pid"
            kill "$pid" 2>/dev/null || true
            sleep 1
            kill -9 "$pid" 2>/dev/null || true
        fi
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

get_rss_mb() {
    local pid=$1
    grep VmRSS /proc/"$pid"/status 2>/dev/null | awk '{printf "%d", $2/1024}'
}

get_pss_mb() {
    local pid=$1
    grep '^Pss:' /proc/"$pid"/smaps_rollup 2>/dev/null | awk '{sum+=$2} END{printf "%d", sum/1024}'
}

wait_healthy() {
    local name=$1
    local timeout=$2
    for i in $(seq 1 "$timeout"); do
        if $FCVM ls --json 2>/dev/null | jq -e ".[] | select(.name == \"$name\" and .health_status == \"healthy\")" > /dev/null 2>&1; then
            echo "  '$name' healthy after ${i}s"
            return 0
        fi
        if [[ $((i % 15)) -eq 0 ]]; then
            echo "  still waiting... (${i}s)"
        fi
        sleep 1
    done
    echo "  TIMEOUT waiting for '$name'"
    return 1
}

get_pid() {
    $FCVM ls --json 2>/dev/null | jq -r ".[] | select(.name == \"$1\") | .pid"
}

echo "=== Memory Sharing Test (FILE backend, no UFFD) ==="
echo "VM memory: ${MEM_MIB}MB, filling ${FILL_MIB}MB with random data"
echo ""

# Step 1: Start baseline VM
echo "--- Step 1: Starting baseline VM ---"
$FCVM podman run \
    --name "$BASELINE" \
    --network rootless \
    --mem "$MEM_MIB" \
    --setup \
    ecr-public.aws.com/docker/library/alpine:latest \
    sleep 3600 &
BASELINE_BG=$!
cleanup_pids+=("$BASELINE_BG")

wait_healthy "$BASELINE" 120
BASELINE_PID=$(get_pid "$BASELINE")
echo "  PID: $BASELINE_PID"

# Step 2: Fill memory with random data
echo ""
echo "--- Step 2: Filling ${FILL_MIB}MB with random data ---"
$FCVM exec --pid "$BASELINE_PID" --vm -- sh -c \
    "dd if=/dev/urandom of=/dev/shm/fill bs=1M count=$FILL_MIB 2>/dev/null; md5sum /dev/shm/fill"
echo ""

# Step 3: Snapshot
echo "--- Step 3: Creating snapshot ---"
$FCVM snapshot create --pid "$BASELINE_PID" --tag "$SNAP"
echo ""

# Step 4: Kill baseline
echo "--- Step 4: Killing baseline ---"
kill "$BASELINE_BG" 2>/dev/null || true
sleep 2
kill -9 "$BASELINE_BG" 2>/dev/null || true
wait "$BASELINE_BG" 2>/dev/null || true
echo "  done"
echo ""

# Step 5+6: Clone 1 and Clone 2 using FILE backend (--snapshot, no serve)
CLONE1="memf-clone1-$SUFFIX"
CLONE2="memf-clone2-$SUFFIX"

echo "--- Step 5: Starting clone 1 (file backend) ---"
$FCVM snapshot run --snapshot "$SNAP" --name "$CLONE1" --network rootless &
CLONE1_BG=$!
cleanup_pids+=("$CLONE1_BG")

echo "--- Step 6: Starting clone 2 (file backend) ---"
$FCVM snapshot run --snapshot "$SNAP" --name "$CLONE2" --network rootless &
CLONE2_BG=$!
cleanup_pids+=("$CLONE2_BG")

wait_healthy "$CLONE1" 120
wait_healthy "$CLONE2" 120

CLONE1_PID=$(get_pid "$CLONE1")
CLONE2_PID=$(get_pid "$CLONE2")
echo "  Clone 1 PID: $CLONE1_PID"
echo "  Clone 2 PID: $CLONE2_PID"
echo ""

# Measure memory BEFORE reading pages
echo "=== Memory BEFORE reading pages ==="
for label_pid in "Clone1:$CLONE1_BG" "Clone2:$CLONE2_BG"; do
    label="${label_pid%%:*}"
    pid="${label_pid##*:}"
    fc_pid=$(get_fc_pid "$pid" || true)
    if [[ -n "$fc_pid" ]]; then
        rss=$(get_rss_mb "$fc_pid" || echo "?")
        pss=$(get_pss_mb "$fc_pid" || echo "?")
        echo "  $label (fc PID $fc_pid): RSS=${rss}MB  PSS=${pss}MB"
    else
        echo "  $label: no firecracker found (fcvm PID $pid)"
    fi
done
echo ""

# Read all pages in both clones
echo "--- Reading all ${FILL_MIB}MB in both clones ---"
echo ""
echo "Clone 1:"
$FCVM exec --pid "$CLONE1_PID" --vm -- sh -c \
    "md5sum /dev/shm/fill && free -m | head -2"
echo ""
echo "Clone 2:"
$FCVM exec --pid "$CLONE2_PID" --vm -- sh -c \
    "md5sum /dev/shm/fill && free -m | head -2"
echo ""

# Measure memory AFTER reading pages
echo "=== Memory AFTER reading all pages ==="
for label_pid in "Clone1:$CLONE1_BG" "Clone2:$CLONE2_BG"; do
    label="${label_pid%%:*}"
    pid="${label_pid##*:}"
    fc_pid=$(get_fc_pid "$pid" || true)
    if [[ -n "$fc_pid" ]]; then
        rss=$(get_rss_mb "$fc_pid" || echo "?")
        pss=$(get_pss_mb "$fc_pid" || echo "?")
        echo "  $label (fc PID $fc_pid): RSS=${rss}MB  PSS=${pss}MB"
    else
        echo "  $label: no firecracker found (fcvm PID $pid)"
    fi
done

echo ""
echo "Host memory:"
free -m | head -2

echo ""
echo "=== SUMMARY ==="
echo "Each clone has ${MEM_MIB}MB VM memory with ${FILL_MIB}MB random data."
echo "File backend: Firecracker mmaps memory.bin with MAP_PRIVATE."
echo "RSS >> PSS means pages ARE shared via page cache CoW."
echo ""
echo "Done!"
