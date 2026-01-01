#!/bin/bash
# Build a host kernel deb package with nested virtualization support
#
# This creates linux-image-*.deb and linux-headers-*.deb packages
# that can be installed on EC2 instances for nested KVM support.
#
# Usage: ./build-host.sh [output_dir]
#
# Environment:
#   KERNEL_VERSION - kernel version (default: 6.18.2)
#   LOCALVERSION - local version suffix (default: -nested)
#   NPROC - parallel jobs (default: nproc)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_DIR="${1:-$SCRIPT_DIR/../artifacts}"
KERNEL_VERSION="${KERNEL_VERSION:-6.18.2}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
LOCALVERSION="${LOCALVERSION:--nested}"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-host-build}"
NPROC="${NPROC:-$(nproc)}"

# Architecture detection
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) KERNEL_ARCH=arm64 ;;
    x86_64)  KERNEL_ARCH=x86_64 ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "=== Host Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION$LOCALVERSION"
echo "Architecture: $KERNEL_ARCH"
echo "Output: $OUTPUT_DIR"
echo ""

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

# Apply patches if present
PATCHES_DIR="$SCRIPT_DIR/patches"
if [[ -d "$PATCHES_DIR" ]]; then
    for patch in "$PATCHES_DIR"/*.patch; do
        [[ -f "$patch" ]] || continue
        echo "Applying patch: $(basename "$patch")"
        patch -p1 < "$patch" || true
    done
fi

# Start with defconfig for the architecture
echo "Creating base config..."
make ARCH="$KERNEL_ARCH" defconfig

# Enable options from inception.conf
INCEPTION_CONF="$SCRIPT_DIR/inception.conf"
if [[ -f "$INCEPTION_CONF" ]]; then
    echo "Applying options from inception.conf..."
    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ -z "${line// }" ]] && continue
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=y ]]; then
            opt="${BASH_REMATCH[1]}"
            echo "  Enabling $opt"
            ./scripts/config --enable "$opt"
        fi
    done < "$INCEPTION_CONF"
fi

# Additional options for host kernel
echo "Enabling additional host kernel options..."
./scripts/config --enable CONFIG_BTRFS_FS
./scripts/config --enable CONFIG_OVERLAY_FS
./scripts/config --enable CONFIG_CGROUPS
./scripts/config --enable CONFIG_MEMCG
./scripts/config --enable CONFIG_BLK_CGROUP
./scripts/config --enable CONFIG_USERFAULTFD
./scripts/config --enable CONFIG_VIRTIO
./scripts/config --enable CONFIG_VIRTIO_PCI
./scripts/config --enable CONFIG_VIRTIO_NET
./scripts/config --enable CONFIG_VIRTIO_BLK
./scripts/config --enable CONFIG_EXT4_FS
./scripts/config --enable CONFIG_XFS_FS
./scripts/config --enable CONFIG_NVME_CORE
./scripts/config --enable CONFIG_BLK_DEV_NVME
./scripts/config --enable CONFIG_MODULES
./scripts/config --enable CONFIG_MODULE_UNLOAD

# Set local version
./scripts/config --set-str CONFIG_LOCALVERSION "$LOCALVERSION"
./scripts/config --disable CONFIG_LOCALVERSION_AUTO

# Update config with defaults
make ARCH="$KERNEL_ARCH" olddefconfig

# Show key options
echo ""
echo "Verifying configuration:"
grep -E "^CONFIG_(KVM|FUSE_FS|VIRTUALIZATION|BTRFS_FS|USERFAULTFD)=" .config || true
echo ""

# Build deb packages
echo "Building kernel deb packages with $NPROC parallel jobs..."
make ARCH="$KERNEL_ARCH" -j"$NPROC" bindeb-pkg LOCALVERSION="$LOCALVERSION"

# Copy output
echo "Copying deb packages to $OUTPUT_DIR..."
cp -v ../*.deb "$OUTPUT_DIR/"

echo ""
echo "=== Build Complete ==="
ls -la "$OUTPUT_DIR"/*.deb
