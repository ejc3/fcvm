#!/bin/bash
# Build passt/pasta from source using a pinned Debian source tarball.
# Pinned to commit 038c51e (2026-05-26, upstream release 2026_05_26.038c51e)
# from https://passt.top/passt. This pin includes the upstream fix for the
# netlink neighbour-sync race that made pasta exit right after startup
# ("netlink: Unexpected sequence number"), previously carried here as a patch.
# Served from snapshot.debian.org: deb.debian.org drops superseded versions
# from its pool (which 404'd a previous pin), while snapshot.debian.org
# archives every version permanently. The checksum guards the pin.
#
# Local patches (passt-*.patch, kept next to this script) are applied on top:
#   - passt-addr-seen.patch: stops overheard bridge traffic from retargeting
#     pasta's inbound port forwarding away from the guest.
set -euo pipefail

PASST_TARBALL_URL="https://snapshot.debian.org/archive/debian/20260527T083727Z/pool/main/p/passt/passt_0.0~git20260526.038c51e.orig.tar.xz"
PASST_TARBALL_SHA256="78d9a5c11592a30ebbb35e39614d4ae5059c443f7f4f96ddce195ba3e9129172"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASST_PATCHES=(
    "$SCRIPT_DIR/passt-addr-seen.patch"
)
# Key the build dir on the tarball and patch contents so a cached build tree
# from an older pin or patch set is rebuilt instead of silently reused.
BUILD_FINGERPRINT="$({ echo "$PASST_TARBALL_SHA256"; cat "${PASST_PATCHES[@]}"; } | sha256sum | cut -c1-12)"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build-${BUILD_FINGERPRINT}}"

echo "==> Building passt from Debian source tarball (commit 038c51e)..."

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
