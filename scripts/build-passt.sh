#!/bin/bash
# Build passt/pasta from source
set -euo pipefail

PASST_REPO="https://passt.top/passt"
PASST_TAG="${PASST_TAG:-2025_01_20.386b5f5}"
PASST_DEBIAN_TARBALL="http://deb.debian.org/debian/pool/main/p/passt/passt_0.0~git20260120.386b5f5.orig.tar.xz"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build}"

echo "==> Building passt from source (tag: ${PASST_TAG})..."

# Clone if not already present, with Debian source tarball fallback
if [ ! -d "$BUILD_DIR" ]; then
    if ! git clone "$PASST_REPO" "$BUILD_DIR" 2>/dev/null; then
        echo "==> passt.top unreachable, fetching Debian source tarball..."
        mkdir -p "$BUILD_DIR"
        curl -fsSL "$PASST_DEBIAN_TARBALL" | tar -xJ -C "$BUILD_DIR" --strip-components=1
    fi
fi

cd "$BUILD_DIR"
# Checkout tag if this is a git repo (skip for tarball extraction)
if [ -d .git ]; then
    git fetch --tags 2>/dev/null || echo "==> git fetch failed (upstream may be down), using cached checkout"
    git checkout "$PASST_TAG" 2>/dev/null || git checkout origin/master
fi

# Build
make clean 2>/dev/null || true
make -j"$(nproc)"

# Install (atomic rename avoids ETXTBSY when pasta/passt are running)
for bin in pasta passt; do
    sudo cp "$bin" "/usr/local/bin/${bin}.tmp.$$"
    sudo mv -f "/usr/local/bin/${bin}.tmp.$$" "/usr/local/bin/${bin}"
done
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
