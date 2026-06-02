#!/bin/bash
# Build passt/pasta from source using a pinned Debian source tarball.
# Pinned to commit 386b5f5 (2026-01-20) from https://passt.top/passt
# Served from snapshot.debian.org: deb.debian.org drops superseded versions
# from its pool (which 404'd the previous pin), while snapshot.debian.org
# archives every version permanently. The checksum guards the pin.
#
# A local patch (passt-addr-seen.patch, kept next to this script) is applied
# on top: it stops overheard bridge traffic from retargeting pasta's inbound
# port forwarding away from the guest. See the patch header for details.
set -euo pipefail

PASST_TARBALL_URL="https://snapshot.debian.org/archive/debian/20260301T000000Z/pool/main/p/passt/passt_0.0~git20260120.386b5f5.orig.tar.xz"
PASST_TARBALL_SHA256="cc0a86b0ac28e1e5b2a4243bcf7fa84b14dd91c7dc883a78896060111e12d105"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASST_PATCH="$SCRIPT_DIR/passt-addr-seen.patch"
# Key the build dir on the patch contents so a cached build tree from an older
# patch (or no patch) is rebuilt instead of silently reused.
PATCH_FINGERPRINT="$(sha256sum "$PASST_PATCH" | cut -c1-12)"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build-${PATCH_FINGERPRINT}}"

echo "==> Building passt from Debian source tarball (commit 386b5f5)..."

if [ ! -f "$BUILD_DIR/Makefile" ]; then
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    curl -fsSL -o "$BUILD_DIR/passt.orig.tar.xz" "$PASST_TARBALL_URL"
    echo "$PASST_TARBALL_SHA256  $BUILD_DIR/passt.orig.tar.xz" | sha256sum -c -
    tar -xJf "$BUILD_DIR/passt.orig.tar.xz" -C "$BUILD_DIR" --strip-components=1
    patch -p1 -d "$BUILD_DIR" < "$PASST_PATCH"
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
