#!/bin/bash
# Builds Robot Park (fleet/phone-app) against the SDK platform scripts/phone/sdk.sh fetched, in
# the app's build container (fleet/phone-app/Containerfile), and publishes it by rename as
# <aosp.sh --root-path>/phone-work/robotpark-<id>.apk, beside the probe app, <id> a hash of the
# app's sources; prints the path. A published app is never rebuilt or replaced, as aosp.sh does
# for the probe.
#
#   scripts/phone/app.sh              build if needed, print the path
#   scripts/phone/app.sh --apk-path   the path this checkout's sources build to
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)/fleet/phone-app
SDK=$("$(dirname "$0")/sdk.sh" --sdk-path)
WORK=$("$(dirname "$0")/aosp.sh" --root-path)/phone-work
id=$(cd "$SRC" && find . -type f -not -path '*/build/*' -not -path './.gradle/*' -not -path './.kotlin/*' -print0 \
  | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
APK=$WORK/robotpark-$id.apk
if [ "${1:-}" = --apk-path ]; then echo "$APK"; exit 0; fi
if [ ! -f "$APK" ]; then
  [ -d "$SDK/platforms/android-35" ] || { echo "app.sh: run scripts/phone/sdk.sh first" >&2; exit 2; }
  make -C "$SRC" apk SDK="$SDK" >&2
  mkdir -p "$WORK"
  tmp=$(mktemp "$WORK/robotpark.XXXXXX")
  cp "$SRC/app/build/outputs/apk/release/app-release.apk" "$tmp"
  mv "$tmp" "$APK"
fi
echo "$APK"
