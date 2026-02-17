#!/bin/bash
# Build btrfs-progs >= 6.12 for --rootdir --subvol support.
#
# btrfs-progs 6.12+ includes mkfs.btrfs --rootdir --subvol which creates
# btrfs images from directories containing btrfs subvolumes — without root.
# Ubuntu 24.04 ships btrfs-progs 6.6.3 which lacks this feature.
#
# Usage: ./scripts/build-btrfs-progs.sh [output_dir]
#        Output defaults to /mnt/fcvm-btrfs/deps/bin/

set -euo pipefail

BTRFS_PROGS_VERSION="6.12"
OUTPUT_DIR="${1:-/mnt/fcvm-btrfs/deps/bin}"

# Source URL
BTRFS_PROGS_URL="https://github.com/kdave/btrfs-progs/archive/refs/tags/v${BTRFS_PROGS_VERSION}.tar.gz"

# Build directory
BUILD_DIR="/tmp/btrfs-progs-build-$$"
PREFIX="$BUILD_DIR/install"

cleanup() {
    if [[ -d "$BUILD_DIR" ]]; then
        rm -rf "$BUILD_DIR"
    fi
}
trap cleanup EXIT

echo "=== Building btrfs-progs ${BTRFS_PROGS_VERSION} ==="
echo "Output: $OUTPUT_DIR/mkfs.btrfs"
echo ""

# Check if already built
if [[ -x "$OUTPUT_DIR/mkfs.btrfs" ]]; then
    EXISTING_VERSION=$("$OUTPUT_DIR/mkfs.btrfs" --version 2>/dev/null | grep -oP '[\d.]+' || echo "0")
    MAJOR=$(echo "$EXISTING_VERSION" | cut -d. -f1)
    MINOR=$(echo "$EXISTING_VERSION" | cut -d. -f2)
    if [[ "$MAJOR" -gt 6 ]] || [[ "$MAJOR" -eq 6 && "$MINOR" -ge 12 ]]; then
        echo "mkfs.btrfs $EXISTING_VERSION already exists at $OUTPUT_DIR/mkfs.btrfs (>= 6.12)"
        echo "Delete it to force rebuild."
        exit 0
    fi
fi

mkdir -p "$BUILD_DIR" "$PREFIX"
cd "$BUILD_DIR"

# Check and install build dependencies
echo "Checking build dependencies..."
MISSING_PKGS=""

for pkg in uuid-dev libblkid-dev liblzo2-dev libzstd-dev zlib1g-dev libext2fs-dev libudev-dev; do
    if ! dpkg -s "$pkg" &>/dev/null; then
        MISSING_PKGS="$MISSING_PKGS $pkg"
    fi
done

# Also need python3 for documentation/autogen (python3-setuptools for sphinx)
if ! command -v python3 &>/dev/null; then
    MISSING_PKGS="$MISSING_PKGS python3"
fi

if ! command -v autoconf &>/dev/null; then
    MISSING_PKGS="$MISSING_PKGS autoconf"
fi

if ! command -v automake &>/dev/null; then
    MISSING_PKGS="$MISSING_PKGS automake"
fi

if ! command -v pkg-config &>/dev/null; then
    MISSING_PKGS="$MISSING_PKGS pkg-config"
fi

if [[ -n "$MISSING_PKGS" ]]; then
    echo "Installing missing build dependencies:$MISSING_PKGS"
    sudo apt-get update -qq
    sudo apt-get install -y --no-install-recommends $MISSING_PKGS
fi

# Download and extract
echo ""
echo "=== Downloading btrfs-progs ${BTRFS_PROGS_VERSION} ==="
curl -sL "$BTRFS_PROGS_URL" | tar xz
cd "btrfs-progs-${BTRFS_PROGS_VERSION}"

# Generate configure script
echo ""
echo "=== Running autogen ==="
./autogen.sh

# Configure - disable documentation (requires sphinx) and python bindings
echo ""
echo "=== Configuring ==="
./configure \
    --prefix="$PREFIX" \
    --disable-documentation \
    --disable-python

# Build
echo ""
echo "=== Building ==="
make -j"$(nproc)"

# Verify the build
echo ""
echo "=== Verifying build ==="
./mkfs.btrfs --version

# Check that --rootdir is supported (mkfs.btrfs has had -r/--rootdir since 4.x)
# Verify by checking the version is >= 6.12
BUILT_VERSION=$(./mkfs.btrfs --version 2>/dev/null | grep -oP '[\d.]+' || echo "0")
echo "Built version: $BUILT_VERSION"
BUILT_MAJOR=$(echo "$BUILT_VERSION" | cut -d. -f1)
BUILT_MINOR=$(echo "$BUILT_VERSION" | cut -d. -f2)
if [[ "$BUILT_MAJOR" -lt 6 ]] || [[ "$BUILT_MAJOR" -eq 6 && "$BUILT_MINOR" -lt 12 ]]; then
    echo "ERROR: built mkfs.btrfs version $BUILT_VERSION is too old (need >= 6.12 for --rootdir --subvol)"
    exit 1
fi

# Install to output directory
echo ""
echo "=== Installing to $OUTPUT_DIR ==="
mkdir -p "$OUTPUT_DIR"
cp -v mkfs.btrfs "$OUTPUT_DIR/mkfs.btrfs"
chmod +x "$OUTPUT_DIR/mkfs.btrfs"

echo ""
echo "=== Success! ==="
echo "Binary: $OUTPUT_DIR/mkfs.btrfs"
VERSION=$("$OUTPUT_DIR/mkfs.btrfs" --version 2>/dev/null || echo "unknown")
echo "Version: $VERSION"
echo ""
echo "Test with: $OUTPUT_DIR/mkfs.btrfs --version"
