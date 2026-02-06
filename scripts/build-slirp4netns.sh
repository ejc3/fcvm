#!/bin/bash
# Build slirp4netns with libslirp >= 4.7.0 for IPv6 DNS proxying support.
#
# This script builds slirp4netns statically linked against a newer libslirp,
# solving the IPv6-only DNS issue on hosts with old system libslirp (< 4.7.0).
#
# Usage: ./scripts/build-slirp4netns.sh [output_dir]
#        Output defaults to /mnt/fcvm-btrfs/deps/bin/

set -euo pipefail

LIBSLIRP_VERSION="4.8.0"
SLIRP4NETNS_VERSION="1.3.1"
OUTPUT_DIR="${1:-/mnt/fcvm-btrfs/deps/bin}"

# Source URLs
LIBSLIRP_URL="https://gitlab.freedesktop.org/slirp/libslirp/-/archive/v${LIBSLIRP_VERSION}/libslirp-v${LIBSLIRP_VERSION}.tar.gz"
SLIRP4NETNS_URL="https://github.com/rootless-containers/slirp4netns/archive/refs/tags/v${SLIRP4NETNS_VERSION}.tar.gz"
LIBCAP_HEADER_URL="https://git.kernel.org/pub/scm/libs/libcap/libcap.git/plain/libcap/include/sys/capability.h?h=libcap-2.48"
LIBSECCOMP_VERSION="2.5.4"
LIBSECCOMP_URL="https://github.com/seccomp/libseccomp/releases/download/v${LIBSECCOMP_VERSION}/libseccomp-${LIBSECCOMP_VERSION}.tar.gz"

# Build directory
BUILD_DIR="/tmp/slirp4netns-build-$$"
PREFIX="$BUILD_DIR/install"

cleanup() {
    if [[ -d "$BUILD_DIR" ]]; then
        rm -rf "$BUILD_DIR"
    fi
}
trap cleanup EXIT

echo "=== Building slirp4netns with libslirp ${LIBSLIRP_VERSION} ==="
echo "Output: $OUTPUT_DIR/slirp4netns"
echo ""

mkdir -p "$BUILD_DIR" "$PREFIX"
cd "$BUILD_DIR"

# Check dependencies
echo "Checking dependencies..."
MISSING=""

# Required for libslirp build
if ! command -v meson &>/dev/null; then
    MISSING="$MISSING meson"
fi
if ! command -v ninja &>/dev/null; then
    MISSING="$MISSING ninja-build"
fi
if ! pkg-config --exists glib-2.0 2>/dev/null; then
    MISSING="$MISSING glib2-devel"
fi

# Required for slirp4netns build
if ! command -v autoconf &>/dev/null; then
    MISSING="$MISSING autoconf"
fi
if ! command -v automake &>/dev/null; then
    MISSING="$MISSING automake"
fi

if [[ -n "$MISSING" ]]; then
    echo "Missing dependencies:$MISSING"
    echo ""
    echo "Install with: sudo dnf install -y$MISSING"
    exit 1
fi

# Check if libcap-devel is missing and provide workaround
if ! pkg-config --exists libcap 2>/dev/null; then
    echo "libcap-devel not found, setting up workaround..."

    # Check if libcap library exists
    if [[ ! -f /usr/lib64/libcap.so.2 ]]; then
        echo "ERROR: libcap library not found at /usr/lib64/libcap.so.2"
        exit 1
    fi

    # Create libcap development files
    LIBCAP_DIR="$BUILD_DIR/libcap-devel"
    mkdir -p "$LIBCAP_DIR/include/sys" "$LIBCAP_DIR/lib64/pkgconfig"

    # Download capability.h header
    echo "Downloading libcap header..."
    curl -sL "$LIBCAP_HEADER_URL" -o "$LIBCAP_DIR/include/sys/capability.h"

    # Create pkgconfig file
    cat > "$LIBCAP_DIR/lib64/pkgconfig/libcap.pc" << EOF
prefix=/usr
exec_prefix=\${prefix}
libdir=\${exec_prefix}/lib64
includedir=$LIBCAP_DIR/include

Name: libcap
Description: POSIX capabilities library
Version: 2.48
Libs: -L\${libdir} -lcap
Cflags: -I\${includedir}
EOF

    export PKG_CONFIG_PATH="$LIBCAP_DIR/lib64/pkgconfig:${PKG_CONFIG_PATH:-}"
    echo "libcap development files created"
fi

# Check if libseccomp-devel is missing and provide workaround
if ! pkg-config --exists libseccomp 2>/dev/null; then
    echo "libseccomp-devel not found, setting up workaround..."

    # Check if libseccomp library exists
    if [[ ! -f /usr/lib64/libseccomp.so.2 ]]; then
        echo "ERROR: libseccomp library not found at /usr/lib64/libseccomp.so.2"
        exit 1
    fi

    # Create libseccomp development files
    LIBSECCOMP_DIR="$BUILD_DIR/libseccomp-devel"
    mkdir -p "$LIBSECCOMP_DIR/include" "$LIBSECCOMP_DIR/lib64/pkgconfig"

    # Download and extract libseccomp headers from source tarball
    echo "Downloading libseccomp headers..."
    curl -sL "$LIBSECCOMP_URL" | tar xz -C "$BUILD_DIR"
    cp "$BUILD_DIR/libseccomp-${LIBSECCOMP_VERSION}/include/seccomp.h" "$LIBSECCOMP_DIR/include/"
    cp "$BUILD_DIR/libseccomp-${LIBSECCOMP_VERSION}/include/seccomp-syscalls.h" "$LIBSECCOMP_DIR/include/"

    # Create symlink from libseccomp.so -> libseccomp.so.2 (system doesn't have .so symlink)
    ln -sf /usr/lib64/libseccomp.so.2 "$LIBSECCOMP_DIR/lib64/libseccomp.so"

    # Create pkgconfig file pointing to our lib directory with the symlink
    cat > "$LIBSECCOMP_DIR/lib64/pkgconfig/libseccomp.pc" << EOF
prefix=$LIBSECCOMP_DIR
exec_prefix=\${prefix}
libdir=\${exec_prefix}/lib64
includedir=\${prefix}/include

Name: libseccomp
Description: Enhanced seccomp library
Version: ${LIBSECCOMP_VERSION}
Libs: -L\${libdir} -lseccomp
Cflags: -I\${includedir}
EOF

    export PKG_CONFIG_PATH="$LIBSECCOMP_DIR/lib64/pkgconfig:${PKG_CONFIG_PATH:-}"
    echo "libseccomp development files created"
fi

# Download and build libslirp
echo ""
echo "=== Building libslirp ${LIBSLIRP_VERSION} ==="
curl -sL "$LIBSLIRP_URL" | tar xz
cd "libslirp-v${LIBSLIRP_VERSION}"

# Configure with meson - build as static library
meson setup builddir \
    --prefix="$PREFIX" \
    --default-library=static \
    --buildtype=release

ninja -C builddir
ninja -C builddir install

cd "$BUILD_DIR"

# Download and build slirp4netns
echo ""
echo "=== Building slirp4netns ${SLIRP4NETNS_VERSION} ==="
curl -sL "$SLIRP4NETNS_URL" | tar xz
cd "slirp4netns-${SLIRP4NETNS_VERSION}"

# Set PKG_CONFIG_PATH to find our libslirp
export PKG_CONFIG_PATH="$PREFIX/lib64/pkgconfig:$PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

# Generate configure script
./autogen.sh

# Configure - link against our static libslirp
# Disable seccomp (optional security feature, not required for our use case)
./configure \
    --prefix="$PREFIX" \
    CFLAGS="-I$PREFIX/include" \
    LDFLAGS="-L$PREFIX/lib64 -L$PREFIX/lib"

# Build
make -j$(nproc)

# Verify the build
echo ""
echo "=== Verifying build ==="
./slirp4netns --version

# Check libslirp version
LIBSLIRP_VER=$(./slirp4netns --version | grep "libslirp:" | awk '{print $2}')
echo "Built with libslirp: $LIBSLIRP_VER"

# Verify it's >= 4.7.0
MAJOR=$(echo "$LIBSLIRP_VER" | cut -d. -f1)
MINOR=$(echo "$LIBSLIRP_VER" | cut -d. -f2)
if [[ "$MAJOR" -lt 4 ]] || [[ "$MAJOR" -eq 4 && "$MINOR" -lt 7 ]]; then
    echo "ERROR: libslirp version $LIBSLIRP_VER is too old (need >= 4.7.0)"
    exit 1
fi

# Install to output directory
echo ""
echo "=== Installing to $OUTPUT_DIR ==="
mkdir -p "$OUTPUT_DIR"
cp -v slirp4netns "$OUTPUT_DIR/slirp4netns"
chmod +x "$OUTPUT_DIR/slirp4netns"

echo ""
echo "=== Success! ==="
echo "Binary: $OUTPUT_DIR/slirp4netns"
echo "libslirp: $LIBSLIRP_VER (supports IPv6 DNS proxying)"
echo ""
echo "Test with: $OUTPUT_DIR/slirp4netns --version"
