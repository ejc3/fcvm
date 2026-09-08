#!/bin/bash
# Build the same upstream commit as rootfs-config.toml, without local patches.
# This commit includes the addr_seen fix for #661 and the earlier netlink
# neighbour-sync fix. The checksum verifies the immutable upstream archive.
set -euo pipefail

PASST_COMMIT="3f57f0382f6a72c0b8ce0c5ff92248b5117ed9b6"
PASST_TARBALL_URL="https://passt.top/passt/snapshot/passt-${PASST_COMMIT}.tar.xz"
PASST_TARBALL_SHA256="2d698e3f7a96408231aa11bb1b27775de6964e2de7b0fa29399847d012044e3a"
BUILD_FINGERPRINT="${PASST_TARBALL_SHA256:0:12}"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build-${BUILD_FINGERPRINT}}"

echo "==> Building passt from upstream commit ${PASST_COMMIT}..."

if [ ! -f "$BUILD_DIR/Makefile" ]; then
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    curl -fsSL -o "$BUILD_DIR/passt.orig.tar.xz" "$PASST_TARBALL_URL"
    echo "$PASST_TARBALL_SHA256  $BUILD_DIR/passt.orig.tar.xz" | sha256sum -c -
    tar -xJf "$BUILD_DIR/passt.orig.tar.xz" -C "$BUILD_DIR" --strip-components=1
fi

cd "$BUILD_DIR"
make clean 2>/dev/null || true
make -j"$(nproc)" VERSION="$PASST_COMMIT"

# Install (atomic rename avoids ETXTBSY when pasta/passt are running)
for bin in pasta passt; do
    sudo cp "$bin" "/usr/local/bin/${bin}.tmp.$$"
    sudo mv -f "/usr/local/bin/${bin}.tmp.$$" "/usr/local/bin/${bin}"
done
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
