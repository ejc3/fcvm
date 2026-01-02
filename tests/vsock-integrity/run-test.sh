#!/bin/bash
# Vsock integrity test - runs in L1, starts L2 with vsock client
#
# This script:
# 1. Starts echo server listening on /tmp/vsock-test/vsock.sock_9999
# 2. Starts L2 VM with --vsock-dir /tmp/vsock-test
# 3. L2 runs vsock-integrity client-vsock 2 9999
# 4. Reports pass/fail based on data integrity

set -e

VSOCK_DIR="/tmp/vsock-test"
VSOCK_PORT=9999
MARKER="VSOCK_INTEGRITY_TEST_COMPLETE"

echo "=== Vsock Integrity Test ==="
echo "Testing vsock data integrity under NV2 nested virtualization"
echo ""

# Cleanup any previous run
rm -rf "$VSOCK_DIR"
mkdir -p "$VSOCK_DIR"

# Start echo server in background
echo "Starting echo server on $VSOCK_DIR port $VSOCK_PORT..."
vsock-integrity server "$VSOCK_DIR" "$VSOCK_PORT" &
SERVER_PID=$!
sleep 1

# Verify server is running
if ! kill -0 $SERVER_PID 2>/dev/null; then
    echo "ERROR: Echo server failed to start"
    exit 1
fi
echo "Echo server running (PID: $SERVER_PID)"

# Get paths from environment or use defaults
KERNEL_PATH="${KERNEL_PATH:-/mnt/fcvm-btrfs/kernels/vmlinux-nested.bin}"
IMAGE_CACHE="${IMAGE_CACHE:-}"

# Build L2 command - runs vsock-integrity client
L2_CMD="vsock-integrity client-vsock 2 $VSOCK_PORT && echo $MARKER"

echo ""
echo "Starting L2 VM..."
echo "  --vsock-dir $VSOCK_DIR"
echo "  L2 will connect to port $VSOCK_PORT"
echo ""

# Run L2 VM
# Note: This container must have vsock-integrity binary and fcvm available
fcvm podman run \
    --name l2-vsock-test \
    --network bridged \
    --privileged \
    --vsock-dir "$VSOCK_DIR" \
    --kernel-profile nested \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    localhost/vsock-integrity \
    --cmd "$L2_CMD" 2>&1 | tee /tmp/l2-output.log

# Kill echo server
kill $SERVER_PID 2>/dev/null || true

# Check results
echo ""
echo "=== Test Results ==="
if grep -q "VSOCK_INTEGRITY_OK" /tmp/l2-output.log; then
    echo "PASS: No corruption detected"
    echo "$MARKER"
    exit 0
elif grep -q "CORRUPTION" /tmp/l2-output.log; then
    echo "FAIL: Data corruption detected!"
    grep "CORRUPTION" /tmp/l2-output.log
    exit 1
else
    echo "UNKNOWN: Test did not complete"
    tail -20 /tmp/l2-output.log
    exit 1
fi
