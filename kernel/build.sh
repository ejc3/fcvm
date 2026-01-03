#!/bin/bash
# Build a custom Linux kernel with FUSE and KVM support for fcvm nested virtualization
#
# Optional env vars:
#   KERNEL_PATH - output path (if not set, computed from SHA)
#   KERNEL_VERSION - kernel version (default: 6.18.3)
#   BUILD_DIR - build directory (default: /tmp/kernel-build)
#   NPROC - parallel jobs (default: nproc)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Compute SHA from build inputs
compute_sha() {
    (
        cat "$SCRIPT_DIR/build.sh"
        cat "$SCRIPT_DIR/nested.conf"
        cat "$SCRIPT_DIR/patches/"*.patch "$SCRIPT_DIR/patches/"*.vm.patch 2>/dev/null || true
    ) | sha256sum | cut -c1-12
}

# Set KERNEL_PATH if not provided
KERNEL_VERSION="${KERNEL_VERSION:-6.18.3}"
BUILD_SHA=$(compute_sha)
if [[ -z "${KERNEL_PATH:-}" ]]; then
    KERNEL_PATH="/mnt/fcvm-btrfs/kernels/vmlinux-${KERNEL_VERSION}-${BUILD_SHA}.bin"
    echo "Computed KERNEL_PATH: $KERNEL_PATH"
fi

KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
BUILD_DIR="${BUILD_DIR:-/tmp/kernel-build}"
NPROC="${NPROC:-$(nproc)}"
SOURCE_DIR="$BUILD_DIR/linux-${KERNEL_VERSION}"
SHA_MARKER="$SOURCE_DIR/.fcvm-patches-sha"

# Architecture detection
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) KERNEL_ARCH=arm64; KERNEL_IMAGE=Image ;;
    x86_64)  KERNEL_ARCH=x86_64; KERNEL_IMAGE=bzImage ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "=== fcvm Nested Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Architecture: $KERNEL_ARCH"
echo "Build SHA: $BUILD_SHA"
echo "Output: $KERNEL_PATH"
echo ""

# Check if already built
if [[ -f "$KERNEL_PATH" ]]; then
    echo "Kernel already exists: $KERNEL_PATH"
    echo "Skipping build."
    exit 0
fi

# Create directories
mkdir -p "$(dirname "$KERNEL_PATH")" "$BUILD_DIR"
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

# Apply patches from patches directory
# VM kernel applies: *.patch (shared) + *.vm.patch (VM-only)
PATCHES_DIR="$SCRIPT_DIR/patches"

# Skip patching if already done with this SHA
if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
    echo "Patches already applied (SHA: $BUILD_SHA)"
else
    echo "Applying patches..."

    # Track applied patches to avoid duplicates (*.patch glob also matches *.vm.patch)
    declare -A applied_patches

    for patch_file in "$PATCHES_DIR"/*.patch "$PATCHES_DIR"/*.vm.patch; do
        [[ ! -f "$patch_file" ]] && continue
        [[ -n "${applied_patches[$patch_file]:-}" ]] && continue
        applied_patches[$patch_file]=1
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

# Download Firecracker base config
FC_CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-${ARCH}-6.1.config"
echo "Downloading Firecracker base config..."
curl -fSL "$FC_CONFIG_URL" -o .config

# Apply options from nested.conf
echo "Applying options from nested.conf..."
KERNEL_CONF="$SCRIPT_DIR/nested.conf"
if [[ -f "$KERNEL_CONF" ]]; then
    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ -z "${line// }" ]] && continue
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=y ]]; then
            opt="${BASH_REMATCH[1]}"
            echo "  Enabling $opt"
            ./scripts/config --enable "$opt"
        fi
    done < "$KERNEL_CONF"
else
    echo "  WARNING: $KERNEL_CONF not found"
fi

# Always enable BTRFS
./scripts/config --enable CONFIG_BTRFS_FS

# Update config with defaults for new options
make ARCH="$KERNEL_ARCH" olddefconfig

# Show enabled options
echo ""
echo "Verifying configuration:"
grep -E "^CONFIG_(FUSE_FS|KVM|VIRTUALIZATION|BTRFS_FS|TUN|VETH)=" .config || true
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
