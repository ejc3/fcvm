#!/bin/bash
# End-to-end test for cargo install workflow.
#
# This script tests that fcvm works correctly after `cargo install`:
# 1. Install binary to isolated location (simulates ~/.cargo/bin)
# 2. Run from /tmp (no source tree access)
# 3. Start a VM and verify it becomes healthy
# 4. Clean up
#
# Requires: KVM, btrfs, network setup (runs on self-hosted CI runner)

set -euo pipefail

BINARY="${1:-./target/release/fcvm}"

if [[ ! -f "$BINARY" ]]; then
    echo "ERROR: Binary not found: $BINARY"
    echo "Run: cargo build --release"
    exit 1
fi

echo "=== End-to-end packaging test ==="
echo "Binary: $BINARY"

# Create isolated install directory
INSTALL_DIR=$(mktemp -d)

cleanup() {
    echo ""
    echo "Step 4: Clean up"
    # Kill any VMs we started
    if [[ -n "${FCVM_PID:-}" ]]; then
        sudo kill "$FCVM_PID" 2>/dev/null || true
        sleep 1
    fi
    rm -rf "$INSTALL_DIR"
    rm -f /tmp/pkg-test-vm.log
}
trap cleanup EXIT

# Copy binaries (simulates ~/.cargo/bin/fcvm and fc-agent)
cp "$BINARY" "$INSTALL_DIR/fcvm"
chmod +x "$INSTALL_DIR/fcvm"

# fc-agent should be in same directory as fcvm binary
FC_AGENT_SRC="$(dirname "$BINARY")/fc-agent"
if [[ -f "$FC_AGENT_SRC" ]]; then
    cp "$FC_AGENT_SRC" "$INSTALL_DIR/fc-agent"
    chmod +x "$INSTALL_DIR/fc-agent"
else
    echo "WARNING: fc-agent not found at $FC_AGENT_SRC"
    echo "Set FC_AGENT_PATH if needed"
fi

FCVM="$INSTALL_DIR/fcvm"
export FC_AGENT_PATH="$INSTALL_DIR/fc-agent"

# Generate unique VM name
VM_NAME="pkg-test-$(date +%s)"

echo ""
echo "Step 1: Verify binary works from outside source tree"
cd /tmp

# Should be able to show help
if ! "$FCVM" --help > /dev/null 2>&1; then
    echo "FAIL: Binary doesn't work from /tmp"
    exit 1
fi
echo "PASS: Binary runs from /tmp"

echo ""
echo "Step 2: Start VM using installed binary"
# Use bridged networking with sudo (standard production usage)
# The binary should use existing setup (kernel, rootfs, initrd already present)

# Start VM in background with sudo (nohup to prevent signal issues)
# Use nginx so the HTTP health check works (default checks port 80)
sudo FC_AGENT_PATH="$FC_AGENT_PATH" nohup "$FCVM" podman run \
    --name "$VM_NAME" \
    --network bridged \
    nginx:alpine \
    > /tmp/pkg-test-vm.log 2>&1 &

echo "Waiting for VM to become healthy..."

# Wait for VM to become healthy (max 120 seconds)
TIMEOUT=120
HEALTHY=false
FCVM_PID=""
for i in $(seq 1 $TIMEOUT); do
    # Check if VM is healthy via ls command (use jq to parse JSON properly)
    LS_OUTPUT=$(sudo "$FCVM" ls --json 2>/dev/null || echo "[]")

    # Find healthy VM using jq
    HEALTHY_VM=$(echo "$LS_OUTPUT" | jq -r '.[] | select(.health_status == "healthy") | .pid' 2>/dev/null | head -1)

    if [[ -n "$HEALTHY_VM" ]]; then
        HEALTHY=true
        FCVM_PID="$HEALTHY_VM"
        break
    fi

    # Debug: show status every 10 seconds
    if [[ $((i % 10)) -eq 0 ]]; then
        STATUS=$(echo "$LS_OUTPUT" | jq -r '.[0] | "\(.name): \(.health_status)"' 2>/dev/null || echo "no VMs yet")
        echo "  [$i s] $STATUS"
    fi

    # Check for errors in log
    if grep -q "ERROR\|Error:" /tmp/pkg-test-vm.log 2>/dev/null; then
        echo "FAIL: VM startup error detected"
        cat /tmp/pkg-test-vm.log
        exit 1
    fi

    sleep 1
done

if [[ "$HEALTHY" != "true" ]]; then
    echo "FAIL: VM did not become healthy within ${TIMEOUT}s"
    echo "=== VM log ==="
    cat /tmp/pkg-test-vm.log
    exit 1
fi

echo "PASS: VM is healthy (PID: $FCVM_PID)"

echo ""
echo "Step 3: Execute command in VM"
# Run a simple command to verify exec works (use sh -c for portability)
OUTPUT=$(sudo "$FCVM" exec --pid "$FCVM_PID" -- sh -c "echo hello" 2>/dev/null || true)
if [[ "$OUTPUT" == *"hello"* ]]; then
    echo "PASS: Exec works"
else
    echo "WARN: Exec output unexpected: $OUTPUT"
fi

echo ""
echo "=== End-to-end packaging test passed ==="
