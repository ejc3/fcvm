#!/bin/bash
# Kitesurf for pages mode's host image, as the build user (renders/build.sh runs it in a named
# unit beside fcvm's own build): bench/chromium/kitesurf_build.sh at the commit
# bench/chromium/kitesurf_pins.sh pins, then the production bundle (//packages/engine:dist) in
# Bazel's default mode, which is what Kitesurf's own deploy builds. The bundle is served from
# Bazel's output directory behind the link $DIST: it finds its assets (the fonts) at ../public,
# so a copy of the directory alone has none. wrangler, run by the bundle's Bazel action, cannot
# create its state under ~/.config/.wrangler from inside the sandbox, so the repository's own
# prepare step runs once outside Bazel first. Last, RUNTIME.json seals the tree as the image
# carries it (host-check.py verifies it on every validation boot).
#
#   kitesurf-dist.sh SRC KITESURF DIST
set -euo pipefail
SRC=$1; K=$2; DIST=$3
KITESURF_DIR=$K bash "$SRC/bench/chromium/kitesurf_build.sh"
# shellcheck source=../../../bench/chromium/kitesurf_pins.sh
. "$SRC/bench/chromium/kitesurf_pins.sh"
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
python3 "$SRC/bench/chromium/kitesurf_seal.py" write --src "$K/src" --out "$K/RUNTIME.json" \
  --rev "$KITESURF_REV" --node "$("$node" --version)" --pnpm "$KITESURF_PNPM_VERSION" \
  --bazel "$(jq -r .bazel "$K/BUILD.json")" --arch "$(uname -m)" --node-bin "$node"
echo "kitesurf-dist: Kitesurf $KITESURF_REV, bundle $(sha256sum "$DIST/index.js" | cut -c1-64)"
