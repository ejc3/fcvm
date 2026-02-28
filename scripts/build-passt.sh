#!/bin/bash
# Build passt/pasta from source using Debian source tarball.
# Pinned to commit 386b5f5 (2025-01-20) from https://passt.top/passt
# Debian tarball: reliable CDN, no dependency on passt.top uptime.
set -euo pipefail

PASST_DEBIAN_TARBALL="http://deb.debian.org/debian/pool/main/p/passt/passt_0.0~git20260120.386b5f5.orig.tar.xz"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build}"

echo "==> Building passt from Debian source tarball (commit 386b5f5)..."

if [ ! -f "$BUILD_DIR/Makefile" ]; then
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    curl -fsSL "$PASST_DEBIAN_TARBALL" | tar -xJ -C "$BUILD_DIR" --strip-components=1
fi

cd "$BUILD_DIR"
make clean 2>/dev/null || true
make -j"$(nproc)"

# Install (atomic rename avoids ETXTBSY when pasta/passt are running)
for bin in pasta passt; do
    sudo cp "$bin" "/usr/local/bin/${bin}.tmp.$$"
    sudo mv -f "/usr/local/bin/${bin}.tmp.$$" "/usr/local/bin/${bin}"
done
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
