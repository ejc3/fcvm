#!/bin/bash
# Build a custom Linux kernel with FUSE and KVM support for fcvm inception
#
# The output kernel name includes version + build script hash for caching:
#   vmlinux-{version}-{script_sha}.bin
#
# This script must be idempotent - it checks for existing builds before running.

set -euo pipefail

# Configuration
KERNEL_VERSION="${KERNEL_VERSION:-6.12.10}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
OUTPUT_DIR="${OUTPUT_DIR:-/mnt/fcvm-btrfs/kernels}"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-build}"
NPROC="${NPROC:-$(nproc)}"

# Architecture detection
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) KERNEL_ARCH=arm64; KERNEL_IMAGE=Image ;;
    x86_64)  KERNEL_ARCH=x86_64; KERNEL_IMAGE=bzImage ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Compute build script hash (for cache key)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_SHA=$(cat "$SCRIPT_DIR/build.sh" "$SCRIPT_DIR/inception.conf" 2>/dev/null | sha256sum | cut -c1-12)

# Output kernel name
KERNEL_NAME="vmlinux-${KERNEL_VERSION}-${SCRIPT_SHA}.bin"
KERNEL_PATH="${OUTPUT_DIR}/${KERNEL_NAME}"

echo "=== fcvm Inception Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Architecture: $KERNEL_ARCH"
echo "Build script SHA: $SCRIPT_SHA"
echo "Output: $KERNEL_PATH"
echo ""

# Check if already built
if [[ -f "$KERNEL_PATH" ]]; then
    echo "Kernel already exists: $KERNEL_PATH"
    echo "Skipping build."
    exit 0
fi

# Create directories
mkdir -p "$OUTPUT_DIR" "$BUILD_DIR"
cd "$BUILD_DIR"

# Download kernel source if needed
KERNEL_TARBALL="linux-${KERNEL_VERSION}.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/${KERNEL_TARBALL}"

if [[ ! -f "$KERNEL_TARBALL" ]]; then
    echo "Downloading kernel source..."
    curl -fSL "$KERNEL_URL" -o "$KERNEL_TARBALL"
fi

if [[ ! -d "linux-${KERNEL_VERSION}" ]]; then
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

cd "linux-${KERNEL_VERSION}"

# Download Firecracker base config
FC_CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-${ARCH}-6.1.config"
echo "Downloading Firecracker base config..."
curl -fSL "$FC_CONFIG_URL" -o .config

# Enable FUSE and KVM
echo "Enabling FUSE and KVM..."
./scripts/config --enable CONFIG_FUSE_FS
./scripts/config --enable CONFIG_VIRTUALIZATION
./scripts/config --enable CONFIG_KVM

# Update config with defaults for new options
make ARCH="$KERNEL_ARCH" olddefconfig

# Show enabled options
echo ""
echo "Verifying configuration:"
grep -E "^CONFIG_(FUSE_FS|KVM|VIRTUALIZATION)=" .config || true
echo ""

# Build kernel
echo "Building kernel with $NPROC parallel jobs..."
make ARCH="$KERNEL_ARCH" -j"$NPROC" "$KERNEL_IMAGE"

# Copy output
echo "Copying kernel to $KERNEL_PATH..."
case "$KERNEL_ARCH" in
    arm64)  cp "arch/arm64/boot/Image" "$KERNEL_PATH" ;;
    x86_64) cp "arch/x86/boot/bzImage" "$KERNEL_PATH" ;;
esac

echo ""
echo "=== Build Complete ==="
echo "Kernel: $KERNEL_PATH"
echo "Size: $(du -h "$KERNEL_PATH" | cut -f1)"
