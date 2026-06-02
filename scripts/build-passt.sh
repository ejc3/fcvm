#!/bin/bash
# Build passt/pasta from source using a pinned Debian source tarball.
# Pinned to commit 386b5f5 (2026-01-20) from https://passt.top/passt
# Served from snapshot.debian.org: deb.debian.org drops superseded versions
# from its pool (which 404'd the previous pin), while snapshot.debian.org
# archives every version permanently. The checksum guards the pin.
#
# Local patches (passt-*.patch, kept next to this script) are applied on top:
#   - passt-addr-seen.patch: stops overheard bridge traffic from retargeting
#     pasta's inbound port forwarding away from the guest.
#   - passt-netlink-neigh-sync.patch: upstream fix (post-pin) for a netlink
#     sequence-number race during the initial neighbour sync that makes pasta
#     exit right after startup.
set -euo pipefail

PASST_TARBALL_URL="https://snapshot.debian.org/archive/debian/20260301T000000Z/pool/main/p/passt/passt_0.0~git20260120.386b5f5.orig.tar.xz"
PASST_TARBALL_SHA256="cc0a86b0ac28e1e5b2a4243bcf7fa84b14dd91c7dc883a78896060111e12d105"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASST_PATCHES=(
    "$SCRIPT_DIR/passt-addr-seen.patch"
    "$SCRIPT_DIR/passt-netlink-neigh-sync.patch"
)
# Key the build dir on the patch contents so a cached build tree from an older
# patch set (or no patches) is rebuilt instead of silently reused.
PATCH_FINGERPRINT="$(cat "${PASST_PATCHES[@]}" | sha256sum | cut -c1-12)"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build-${PATCH_FINGERPRINT}}"

echo "==> Building passt from Debian source tarball (commit 386b5f5)..."

if [ ! -f "$BUILD_DIR/Makefile" ]; then
    rm -rf "$BUILD_DIR"
    mkdir -p "$BUILD_DIR"
    curl -fsSL -o "$BUILD_DIR/passt.orig.tar.xz" "$PASST_TARBALL_URL"
    echo "$PASST_TARBALL_SHA256  $BUILD_DIR/passt.orig.tar.xz" | sha256sum -c -
    tar -xJf "$BUILD_DIR/passt.orig.tar.xz" -C "$BUILD_DIR" --strip-components=1
    for p in "${PASST_PATCHES[@]}"; do
        patch -p1 -d "$BUILD_DIR" < "$p"
    done
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
