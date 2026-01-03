#!/bin/bash
# Build a host Linux kernel with fcvm patches for EC2 instances
#
# Uses the current running kernel's config as base, applies fcvm patches,
# and builds installable deb packages.
#
# The output is content-addressed by SHA of build inputs:
#   - This script
#   - Shared patches (*.patch, excluding *.vm.patch)
#
# If the output deb already exists, the build is skipped.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCHES_DIR="$SCRIPT_DIR/patches"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-build-host}"
KERNEL_VERSION="${KERNEL_VERSION:-6.18.3}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
NPROC="${NPROC:-$(nproc)}"
SOURCE_DIR="$BUILD_DIR/linux-${KERNEL_VERSION}"
SHA_MARKER="$SOURCE_DIR/.fcvm-patches-sha"

# Compute SHA from build inputs (host kernel excludes .vm.patch files)
compute_sha() {
    (
        cat "$SCRIPT_DIR/build-host.sh"
        for f in "$PATCHES_DIR"/*.patch; do
            [[ -f "$f" ]] || continue
            [[ "$f" == *.vm.patch ]] && continue
            cat "$f"
        done
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
if dpkg -l 2>/dev/null | grep -q "${DEB_NAME}"; then
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

# Check if source exists and has matching SHA
if [[ -d "$SOURCE_DIR" ]]; then
    if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
        echo "Source already patched with current SHA, reusing..."
    else
        echo "Source exists but SHA mismatch (patches changed), re-extracting..."
        rm -rf "$SOURCE_DIR"
    fi
fi

if [[ ! -d "$SOURCE_DIR" ]]; then
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

cd "$SOURCE_DIR"

# Apply patches (shared only, skip .vm.patch which are VM-only)
# Skip patching if already done with this SHA
if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
    echo "Patches already applied (SHA: $BUILD_SHA)"
else
    echo "Applying patches..."
    for patch_file in "$PATCHES_DIR"/*.patch; do
        [[ ! -f "$patch_file" ]] && continue
        [[ "$patch_file" == *.vm.patch ]] && continue  # Skip VM-only patches
        patch_name=$(basename "$patch_file")

        echo "  Applying $patch_name..."
        if patch -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
            patch -p1 --forward < "$patch_file"
        else
            echo "    ERROR: Patch does not apply cleanly"
            echo "    This usually means the source is corrupt. Deleting and retrying..."
            cd "$BUILD_DIR"
            rm -rf "$SOURCE_DIR"
            echo "    Re-run this script to rebuild from fresh source."
            exit 1
        fi
    done

    # Mark source as patched with this SHA
    echo "$BUILD_SHA" > "$SHA_MARKER"
    echo "Patches applied successfully (SHA: $BUILD_SHA)"
fi

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
