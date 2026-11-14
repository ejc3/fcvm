#!/bin/bash
# Build the pinned Kitesurf engine for the kitesurf arm of the corpus campaign.
#
# Kitesurf (github.com/cloudflare/kitesurf, Apache-2.0) is a browser engine that
# runs in V8 isolates on Workers and speaks CDP. Its local form is `wrangler dev`
# over workerd, with the Rust core compiled to WebAssembly by Bazel. This script
# builds that tree at one commit, OUTSIDE this checkout (the campaign fails a step
# that dirties it), and writes a seal the measured phase checks before it starts
# the engine (kitesurf.sh): every file wrangler dev reads that the build produced
# or installed, hashed into one digest, plus the source revision and tool versions.
#
#   KITESURF_DIR   where the clone, the toolchain and the Bazel cache live
#                  (default /mnt/fcvm-btrfs/kitesurf, the box's disposable store)
#
# Everything is pinned: the Kitesurf commit, Node.js and Bazelisk by version and
# sha256, pnpm by version (npm checks the registry's integrity hash), Bazel by
# the repository's .bazelversion, the npm graph by its lockfile (--frozen-lockfile),
# and the Rust toolchains and crates by MODULE.bazel.lock. A second run over an
# existing directory checks out the same commit again and rebuilds incrementally.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd -P)"
# Shared with kitesurf.sh, so the build and the run cannot disagree on a pin.
# shellcheck source=kitesurf_pins.sh
. "$HERE/kitesurf_pins.sh"

KITESURF_DIR="${KITESURF_DIR:-/mnt/fcvm-btrfs/kitesurf}"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

mkdir -p "$KITESURF_DIR"
KITESURF_DIR="$(cd "$KITESURF_DIR" && pwd -P)"
case "$KITESURF_DIR/" in
    "$REPO"/*) echo "BLOCKED: KITESURF_DIR is inside this checkout; the build must not dirty it" >&2; exit 2 ;;
esac
for tool in curl git python3 sha256sum tar xz; do
    command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done

arch=$(uname -m)
case "$arch" in
    aarch64) node_arch=arm64; node_sha=$KITESURF_NODE_SHA256_ARM64
             bazelisk_arch=arm64; bazelisk_sha=$KITESURF_BAZELISK_SHA256_ARM64 ;;
    x86_64) node_arch=x64; node_sha=$KITESURF_NODE_SHA256_X64
            bazelisk_arch=amd64; bazelisk_sha=$KITESURF_BAZELISK_SHA256_AMD64 ;;
    *) echo "BLOCKED: no pinned toolchain for $arch" >&2; exit 2 ;;
esac

TOOLS="$KITESURF_DIR/tools"
SRC="$KITESURF_DIR/src"
mkdir -p "$TOOLS/bin" "$KITESURF_DIR/downloads"

fetch() {  # url sha256 dest
    local url=$1 sha=$2 dest=$3
    if [ -f "$dest" ] && echo "$sha  $dest" | sha256sum -c --quiet - 2>/dev/null; then
        return 0
    fi
    curl -fsSL --retry 3 -o "$dest.part" "$url"
    echo "$sha  $dest.part" | sha256sum -c --quiet - \
        || { echo "BLOCKED: $url does not match its pinned sha256" >&2; rm -f "$dest.part"; exit 2; }
    mv -f "$dest.part" "$dest"
}

node_name="node-$KITESURF_NODE_VERSION-linux-$node_arch"
fetch "https://nodejs.org/dist/$KITESURF_NODE_VERSION/$node_name.tar.xz" "$node_sha" \
    "$KITESURF_DIR/downloads/$node_name.tar.xz"
if [ ! -x "$TOOLS/$node_name/bin/node" ]; then
    tar -xJf "$KITESURF_DIR/downloads/$node_name.tar.xz" -C "$TOOLS"
fi
fetch "https://github.com/bazelbuild/bazelisk/releases/download/$KITESURF_BAZELISK_VERSION/bazelisk-linux-$bazelisk_arch" \
    "$bazelisk_sha" "$KITESURF_DIR/downloads/bazelisk-$KITESURF_BAZELISK_VERSION-$bazelisk_arch"
install -m 0755 "$KITESURF_DIR/downloads/bazelisk-$KITESURF_BAZELISK_VERSION-$bazelisk_arch" "$TOOLS/bin/bazel"

export PATH="$TOOLS/bin:$TOOLS/$node_name/bin:/usr/bin:/bin"
export npm_config_prefix="$TOOLS/npm"
export npm_config_cache="$KITESURF_DIR/npm-cache"
export PATH="$TOOLS/npm/bin:$PATH"
if [ "$(pnpm --version 2>/dev/null || true)" != "$KITESURF_PNPM_VERSION" ]; then
    npm install --global --no-fund --no-audit "pnpm@$KITESURF_PNPM_VERSION" >/dev/null
fi
[ "$(pnpm --version)" = "$KITESURF_PNPM_VERSION" ] \
    || { echo "BLOCKED: pnpm is not $KITESURF_PNPM_VERSION" >&2; exit 2; }

if [ ! -d "$SRC/.git" ]; then
    git clone -q "$KITESURF_REPO_URL" "$SRC"
fi
git -C "$SRC" fetch -q origin "$KITESURF_REV"
git -C "$SRC" checkout -q --detach "$KITESURF_REV"
[ "$(git -C "$SRC" rev-parse HEAD)" = "$KITESURF_REV" ] \
    || { echo "BLOCKED: Kitesurf checkout is not at $KITESURF_REV" >&2; exit 2; }
[ -z "$(git -C "$SRC" status --porcelain --untracked-files=no)" ] \
    || { echo "BLOCKED: tracked Kitesurf sources differ from $KITESURF_REV" >&2; git -C "$SRC" status --short >&2; exit 2; }

cd "$SRC"
# pnpm's own store and Bazel's caches stay under KITESURF_DIR too.
export PNPM_HOME="$TOOLS/pnpm-home"
export XDG_CACHE_HOME="$KITESURF_DIR/cache"
export BAZELISK_HOME="$KITESURF_DIR/bazelisk"
log "pnpm install --frozen-lockfile (Kitesurf $KITESURF_REV)"
pnpm install --frozen-lockfile --store-dir "$KITESURF_DIR/pnpm-store" >"$KITESURF_DIR/pnpm-install.log" 2>&1 \
    || { tail -40 "$KITESURF_DIR/pnpm-install.log" >&2; echo "FAILED: pnpm install" >&2; exit 1; }
log "bazel run //packages/engine:dev_build (first build compiles the Rust core; see $KITESURF_DIR/bazel-build.log)"
bazel --output_user_root="$KITESURF_DIR/bazel-root" run //packages/engine:dev_build \
    >"$KITESURF_DIR/bazel-build.log" 2>&1 \
    || { tail -60 "$KITESURF_DIR/bazel-build.log" >&2; echo "FAILED: bazel build" >&2; exit 1; }
bazel_version=$(bazel --version | awk '{print $2}')
# A Bazel server stays up for hours after a build and wakes on its own; on a box
# that measures next it is foreign CPU. Stop it.
bazel --output_user_root="$KITESURF_DIR/bazel-root" shutdown

log "sealing the built engine tree"
python3 "$HERE/kitesurf_seal.py" write --src "$SRC" --out "$KITESURF_DIR/BUILD.json" \
    --rev "$KITESURF_REV" --node "$(node --version)" --pnpm "$(pnpm --version)" \
    --bazel "$bazel_version" --arch "$arch"
log "built: $KITESURF_DIR/BUILD.json"
