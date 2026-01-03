#!/bin/bash
# Build a host Linux kernel with fcvm patches for EC2 instances
#
# Uses the current running kernel's config as base, applies fcvm patches,
# and builds installable deb packages.
#
# The output is content-addressed by SHA of build inputs:
#   - This script
#   - All patches in patches/
#
# If the output deb already exists, the build is skipped.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCHES_DIR="$SCRIPT_DIR/patches"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-build-host}"
KERNEL_VERSION="${KERNEL_VERSION:-6.18.3}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
NPROC="${NPROC:-$(nproc)}"

# Compute SHA from build inputs
compute_sha() {
    (
        cat "$SCRIPT_DIR/build-host.sh"
        cat "$PATCHES_DIR"/*.patch 2>/dev/null || true
    ) | sha256sum | cut -c1-12
}

BUILD_SHA=$(compute_sha)
LOCALVERSION="-fcvm-${BUILD_SHA}"
DEB_NAME="linux-image-${KERNEL_VERSION}${LOCALVERSION}"

echo "=== fcvm Host Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Build SHA: $BUILD_SHA"
echo "LOCALVERSION: $LOCALVERSION"
echo ""

# Check if already built (look for installed deb or deb file)
if dpkg -l | grep -q "${DEB_NAME}"; then
    echo "Kernel already installed: ${DEB_NAME}"
    echo "Skipping build."
    exit 0
fi

if ls "$BUILD_DIR"/${DEB_NAME}*.deb 2>/dev/null | head -1; then
    echo "Deb already built: $(ls "$BUILD_DIR"/${DEB_NAME}*.deb | head -1)"
    echo "Run: sudo dpkg -i $BUILD_DIR/${DEB_NAME}*.deb"
    exit 0
fi

# Create build directory
mkdir -p "$BUILD_DIR"
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

# Apply patches
echo "Applying patches..."
for patch_file in "$PATCHES_DIR"/*.patch; do
    [[ ! -f "$patch_file" ]] && continue
    patch_name=$(basename "$patch_file")

    echo "  Checking $patch_name..."
    if patch -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
        patch -p1 --forward < "$patch_file"
        echo "    Applied successfully"
    else
        if patch -p1 --reverse --dry-run < "$patch_file" >/dev/null 2>&1; then
            echo "    Already applied (skipping)"
        else
            echo "    ERROR: Patch does not apply cleanly: $patch_name"
            patch -p1 --forward --dry-run < "$patch_file" || true
            exit 1
        fi
    fi
done

# Copy current kernel config as base
echo "Using current kernel config as base..."
CURRENT_VERSION=$(uname -r)
if [[ -f "/boot/config-${CURRENT_VERSION}" ]]; then
    cp "/boot/config-${CURRENT_VERSION}" .config
    echo "  Copied /boot/config-${CURRENT_VERSION}"
elif [[ -f /proc/config.gz ]]; then
    zcat /proc/config.gz > .config
    echo "  Extracted from /proc/config.gz"
else
    echo "ERROR: Cannot find current kernel config"
    exit 1
fi

# Update config for new kernel version
echo "Updating config for kernel ${KERNEL_VERSION}..."
make ARCH=arm64 olddefconfig

# Build deb packages
echo ""
echo "Building kernel deb packages with $NPROC parallel jobs..."
echo "LOCALVERSION=$LOCALVERSION"
echo "This takes 15-30 minutes..."
echo ""

make -j"$NPROC" ARCH=arm64 LOCALVERSION="$LOCALVERSION" bindeb-pkg

echo ""
echo "=== Build Complete ==="
echo "Deb packages:"
ls -la "$BUILD_DIR"/*.deb | grep -v dbg || true
echo ""
echo "To install:"
echo "  sudo dpkg -i $BUILD_DIR/linux-image-${KERNEL_VERSION}${LOCALVERSION}*.deb"
echo "  sudo update-grub"
echo "  sudo reboot"
