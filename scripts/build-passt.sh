#!/bin/bash
# Build passt/pasta from source
# The distro-packaged version lacks --no-splice which is required
# for fcvm's bridge architecture (L2 frames through TAP).
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

# Verify --no-splice support
if ! ./pasta --help 2>&1 | grep -q 'no-splice'; then
    echo "ERROR: Built pasta binary does not support --no-splice"
    exit 1
fi

# Install
sudo cp pasta passt /usr/local/bin/
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
