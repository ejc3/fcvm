#!/bin/bash
# Builds Robot Park (fleet/phone-app) against the SDK scripts/phone/sdk.sh fetched, in the
# app's build container (fleet/phone-app/Containerfile), and publishes it by rename as
# $ANDROID_SDK/phone-work/robotpark-<id>.apk, <id> a hash of the app's sources; prints the path.
# A published app is never rebuilt or replaced, as sdk.sh does for the probe.
#
#   scripts/phone/app.sh              build if needed, print the path
#   scripts/phone/app.sh --apk-path   the path this checkout's sources build to
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)/fleet/phone-app
SDK=$("$(dirname "$0")/sdk.sh" --sdk-path)
id=$(cd "$SRC" && find . -type f -not -path '*/build/*' -not -path './.gradle/*' -not -path './.kotlin/*' -print0 \
  | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
APK=$SDK/phone-work/robotpark-$id.apk
if [ "${1:-}" = --apk-path ]; then echo "$APK"; exit 0; fi
if [ ! -f "$APK" ]; then
  [ -d "$SDK/platforms/android-35" ] || { echo "app.sh: run scripts/phone/sdk.sh first" >&2; exit 2; }
  make -C "$SRC" apk SDK="$SDK" >&2
  tmp=$(mktemp "$SDK/phone-work/robotpark.XXXXXX")
  cp "$SRC/app/build/outputs/apk/release/app-release.apk" "$tmp"
  mv "$tmp" "$APK"
fi
echo "$APK"
