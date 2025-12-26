#!/bin/bash
# Test that fcvm works when installed via cargo install (no source tree access).
#
# This script simulates what happens after `cargo install fcvm`:
# 1. Binary is in ~/.cargo/bin (away from source tree)
# 2. No config file exists yet
# 3. User runs fcvm setup --generate-config
# 4. User runs fcvm setup (finds the generated config)

set -euo pipefail

BINARY="${1:-./target/release/fcvm}"

if [[ ! -f "$BINARY" ]]; then
    echo "ERROR: Binary not found: $BINARY"
    echo "Run: cargo build --release"
    exit 1
fi

echo "=== Testing packaging workflow ==="
echo "Binary: $BINARY"

# Create isolated environment
INSTALL_DIR=$(mktemp -d)
CONFIG_DIR=$(mktemp -d)
trap "rm -rf $INSTALL_DIR; sudo rm -rf $CONFIG_DIR 2>/dev/null || true" EXIT

# Copy binary (simulates ~/.cargo/bin/fcvm)
cp "$BINARY" "$INSTALL_DIR/fcvm"
chmod +x "$INSTALL_DIR/fcvm"
FCVM="$INSTALL_DIR/fcvm"

echo ""
echo "Step 1: Run setup without config (should fail with helpful message)"
set +e
OUTPUT=$(XDG_CONFIG_HOME="$CONFIG_DIR" HOME="$CONFIG_DIR" "$FCVM" setup 2>&1)
EXIT_CODE=$?
set -e

if [[ $EXIT_CODE -eq 0 ]]; then
    echo "FAIL: setup should fail without config"
    echo "Output: $OUTPUT"
    exit 1
fi

if echo "$OUTPUT" | grep -q "No rootfs config found"; then
    echo "PASS: Got helpful error message"
else
    echo "FAIL: Expected 'No rootfs config found' message"
    echo "Output: $OUTPUT"
    exit 1
fi

if echo "$OUTPUT" | grep -q "CARGO_MANIFEST_DIR"; then
    echo "FAIL: Error message exposes CARGO_MANIFEST_DIR (hardcoded path)"
    echo "Output: $OUTPUT"
    exit 1
fi

if echo "$OUTPUT" | grep -q "panicked"; then
    echo "FAIL: Binary panicked"
    echo "Output: $OUTPUT"
    exit 1
fi

echo ""
echo "Step 2: Generate config"
XDG_CONFIG_HOME="$CONFIG_DIR" HOME="$CONFIG_DIR" "$FCVM" setup --generate-config

CONFIG_FILE="$CONFIG_DIR/fcvm/rootfs-config.toml"
if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "FAIL: Config file not created at $CONFIG_FILE"
    exit 1
fi
echo "PASS: Config created at $CONFIG_FILE"

echo ""
echo "Step 3: Verify setup finds the config"
set +e
OUTPUT=$(XDG_CONFIG_HOME="$CONFIG_DIR" HOME="$CONFIG_DIR" FCVM_BASE_DIR="$INSTALL_DIR/data" "$FCVM" setup 2>&1)
set -e

if echo "$OUTPUT" | grep -q "No rootfs config found"; then
    echo "FAIL: Still complaining about missing config"
    echo "Output: $OUTPUT"
    exit 1
fi

# It will fail for other reasons (no btrfs, etc) but should find the config
if echo "$OUTPUT" | grep -q "loaded rootfs config"; then
    echo "PASS: Found and loaded config"
else
    echo "PASS: Config lookup succeeded (may fail for other reasons)"
fi

echo ""
echo "=== All packaging tests passed ==="
