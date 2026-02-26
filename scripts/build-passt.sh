#!/bin/bash
# Build passt/pasta from source
set -euo pipefail

PASST_REPO="https://passt.top/passt"
PASST_TAG="${PASST_TAG:-2025_01_20.386b5f5}"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build}"

echo "==> Building passt from source (tag: ${PASST_TAG})..."

# Clone if not already present
if [ ! -d "$BUILD_DIR" ]; then
    git clone "$PASST_REPO" "$BUILD_DIR"
fi

cd "$BUILD_DIR"
git fetch --tags
git checkout "$PASST_TAG" 2>/dev/null || git checkout origin/master

# Build
make clean 2>/dev/null || true
make -j"$(nproc)"

# Install (atomic rename avoids ETXTBSY when pasta/passt are running)
for bin in pasta passt; do
    sudo cp "$bin" "/usr/local/bin/${bin}.tmp.$$"
    sudo mv -f "/usr/local/bin/${bin}.tmp.$$" "/usr/local/bin/${bin}"
done
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
