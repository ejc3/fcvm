#!/bin/bash
# Kitesurf for pages mode's host image, as the build user (renders/build.sh runs it in a named
# unit beside fcvm's own build), at REV, renders.conf's RENDERS_KITESURF_REV: the recorded
# campaign's commit. bench/chromium/kitesurf_pins.sh pins the other benchmark arm's commit for
# its sealed records and is not changed: the repository's kitesurf_build.sh runs from a copy,
# beside copies of kitesurf_pins.sh with its KITESURF_REV line replaced and of
# kitesurf_seal.py, as the campaign's host built it. The pins' toolchain (node, pnpm,
# Bazelisk) is the repository's.
#
# Then the production bundle (//packages/engine:dist) in Bazel's default mode, which is what
# Kitesurf's own deploy builds. The bundle is served from Bazel's output directory behind the
# link $DIST: it finds its assets (the fonts) at ../public, so a copy of the directory alone
# has none. wrangler, run by the bundle's Bazel action, cannot create its state under
# ~/.config/.wrangler from inside the sandbox, so the repository's own prepare step runs once
# outside Bazel first. Last, RUNTIME.json seals the tree as the image carries it
# (host-check.py verifies it on every validation boot).
#
#   kitesurf-dist.sh SRC KITESURF DIST REV
set -euo pipefail

# tools SRC REV DIR: the build, pins and seal scripts of SRC's bench/chromium in DIR, the pins
# naming REV.
tools() {
  local src=$1 rev=$2 dir=$3
  [[ "$rev" =~ ^[0-9a-f]{40}$ ]] || { echo "kitesurf-dist: '$rev' is not a full Kitesurf commit"; return 1; }
  rm -rf "$dir"
  mkdir -p "$dir"
  cp "$src/bench/chromium/kitesurf_build.sh" "$src/bench/chromium/kitesurf_pins.sh" "$src/bench/chromium/kitesurf_seal.py" "$dir/"
  sed -i "s/^KITESURF_REV=.*/KITESURF_REV=$rev/" "$dir/kitesurf_pins.sh"
  if [ "$(grep -c '^KITESURF_REV=' "$dir/kitesurf_pins.sh")" != 1 ] || ! grep -qx "KITESURF_REV=$rev" "$dir/kitesurf_pins.sh"; then
    echo "kitesurf-dist: the pins copy does not name $rev once"; return 1
  fi
}

[ -z "${KITESURF_DIST_LIB:-}" ] || return 0

SRC=$1; K=$2; DIST=$3; REV=$4
# Outside both the checkout (kitesurf_build.sh refuses a KITESURF_DIR inside the repository it
# runs from) and the Kitesurf directory; the bake removes it with the build's other records.
TOOLS=/mnt/fcvm-btrfs/renders/kitesurf-tools/bench/chromium
tools "$SRC" "$REV" "$TOOLS"
KITESURF_DIR=$K bash "$TOOLS/kitesurf_build.sh"
# shellcheck source=../../../bench/chromium/kitesurf_pins.sh
. "$TOOLS/kitesurf_pins.sh"
[ "$(git -C "$K/src" rev-parse HEAD)" = "$REV" ] || { echo "kitesurf-dist: Kitesurf's checkout is not at $REV"; exit 1; }
node=$(ls "$K"/tools/node-*-linux-*/bin/node)
[ "$(printf '%s\n' "$node" | wc -l)" = 1 ] || { echo "kitesurf-dist: not one node under $K/tools"; exit 1; }
nodebin=$(dirname "$node")
mkdir -p "$HOME/.config/.wrangler/logs"
( cd "$K/src" && env PATH="$nodebin:/usr/bin:/bin" WRANGLER_SEND_METRICS=false CI=1 \
    node scripts/wpt-engine.mjs prepare --dist "$K/dist-intree" ) > "$K/dist-prepare.log" 2>&1 \
  || { tail -20 "$K/dist-prepare.log"; echo "kitesurf-dist: prepare outside Bazel failed"; exit 1; }
bazel_env=(env PATH="$K/tools/bin:$nodebin:$K/tools/npm/bin:/usr/bin:/bin" XDG_CACHE_HOME="$K/cache"
  BAZELISK_HOME="$K/bazelisk" PNPM_HOME="$K/tools/pnpm-home")
cd "$K/src"
"${bazel_env[@]}" bazel --output_user_root="$K/bazel-root" build //packages/engine:dist > "$K/dist-default.log" 2>&1 \
  || { tail -40 "$K/dist-default.log"; echo "kitesurf-dist: the bundle's build failed"; exit 1; }
out=$("${bazel_env[@]}" bazel --output_user_root="$K/bazel-root" cquery --output=files //packages/engine:dist 2>/dev/null | head -1)
if [ -z "$out" ] || [ ! -f "$out/index.js" ]; then echo "kitesurf-dist: the bundle has no index.js ($out)"; exit 1; fi
rm -f "$DIST"
ln -s "$(readlink -f "$out")" "$DIST"
# A Bazel server stays up for hours after a build; nothing on the host needs it.
"${bazel_env[@]}" bazel --output_user_root="$K/bazel-root" shutdown
python3 "$TOOLS/kitesurf_seal.py" write --src "$K/src" --out "$K/RUNTIME.json" \
  --rev "$REV" --node "$("$node" --version)" --pnpm "$KITESURF_PNPM_VERSION" \
  --bazel "$(jq -r .bazel "$K/BUILD.json")" --arch "$(uname -m)" --node-bin "$node"
echo "kitesurf-dist: Kitesurf $REV, bundle $(sha256sum "$DIST/index.js" | cut -c1-64)"
