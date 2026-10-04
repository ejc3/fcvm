#!/bin/bash
# Build the same upstream commit as rootfs-config.toml, plus the one local patch
# fcvm carries. This commit includes the addr_seen fix for #661, the earlier
# netlink neighbour-sync fix, and the fix that keeps -a, -g and -n on a host
# without IPv4. The checksum verifies the immutable upstream archive.
#
# Local patch (passt-*.patch, kept next to this script) is applied on top:
#   - passt-udp-sock-errs-null-flow.patch: guards udp_sock_errs() against a NULL
#     flow, so a published UDP port's listening socket does not crash pasta on a
#     socket error. Submitted upstream; drop it when the pin moves past the merge.
set -euo pipefail

PASST_COMMIT="4e8aa70379a35ec9deb11d76513e7f6c4123b667"
PASST_TARBALL_URL="https://passt.top/passt/snapshot/passt-${PASST_COMMIT}.tar.xz"
PASST_TARBALL_SHA256="ef88ad2c6137b52286e6fcd10311d61f2ab329e5558abe097170e9e5851da8b9"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASST_PATCHES=(
    "$SCRIPT_DIR/passt-udp-sock-errs-null-flow.patch"
)
# Key the kept source on the tarball and patch contents so a tree from an older
# pin or patch set is never picked up.
BUILD_FINGERPRINT="$({ echo "$PASST_TARBALL_SHA256"; cat "${PASST_PATCHES[@]}"; } | sha256sum | cut -c1-12)"
BUILD_DIR="${BUILD_DIR:-/tmp/passt-build-${BUILD_FINGERPRINT}}"

# fill_build_dir <kept> <build> <url> <sha256> [patch...]
#
# Fill <build>, an empty directory only this run uses, with the verified and
# patched source.
#
# <kept> holds that source between runs, so a later run needs no download. A tree
# gets that name in one rename, after it has been verified, patched and marked,
# and is never written to afterwards. A directory under that name that carries
# the mark is therefore a finished tree. Anything else under it is neither
# trusted nor touched. A run that is killed leaves directories under names of its
# own, which no run looks up. When two runs fetch at once, both rename: the
# second rename fails, because the name then holds a directory that is not empty,
# and that run removes its copy. No lock is involved.
#
# Nothing is built in <kept>: two runs of make in one directory would each remove
# and rewrite the binaries the other is about to install.
fill_build_dir() {
    local kept="$1" build="$2" url="$3" sha256="$4" complete=".fcvm-source-complete" stage p
    shift 4
    if [ -f "$kept/$complete" ]; then
        cp -a "$kept/." "$build/"
        return
    fi
    curl -fsSL -o "$build/passt.orig.tar.xz" "$url"
    echo "$sha256  $build/passt.orig.tar.xz" | sha256sum -c -
    tar -xJf "$build/passt.orig.tar.xz" -C "$build" --strip-components=1
    for p in "$@"; do
        patch -p1 -d "$build" < "$p"
    done
    touch "$build/$complete"
    # Keeping the source saves the next run a download. A run that cannot keep it
    # still builds.
    stage="$(mktemp -d "$kept.stage.XXXXXXXX")"
    if ! { cp -a "$build/." "$stage/" && mv -T "$stage" "$kept" 2>/dev/null; }; then
        rm -rf "$stage"
        [ -f "$kept/$complete" ] ||
            echo "==> Not keeping the source: $kept is not a finished tree and is left alone" >&2
    fi
}

echo "==> Building passt from upstream commit ${PASST_COMMIT}..."

mkdir -p "$(dirname "$BUILD_DIR")"
RUN_DIR="$(mktemp -d "${BUILD_DIR}.build.XXXXXXXX")"
trap 'rm -rf "$RUN_DIR"' EXIT
fill_build_dir "$BUILD_DIR" "$RUN_DIR" "$PASST_TARBALL_URL" "$PASST_TARBALL_SHA256" "${PASST_PATCHES[@]}"

cd "$RUN_DIR"
make -j"$(nproc)" VERSION="$PASST_COMMIT"

# Install (atomic rename avoids ETXTBSY when pasta/passt are running)
for bin in pasta passt; do
    sudo cp "$bin" "/usr/local/bin/${bin}.tmp.$$"
    sudo mv -f "/usr/local/bin/${bin}.tmp.$$" "/usr/local/bin/${bin}"
done
echo "==> Installed pasta $(./pasta --version 2>&1 | head -1) to /usr/local/bin/"
