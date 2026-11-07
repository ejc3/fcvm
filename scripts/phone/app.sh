#!/bin/bash
# Builds Robot Park (fleet/phone-app) with the AOSP build's tools that scripts/phone/aosp.sh
# fetched (android.jar, aapt2, d8, zipalign, apksigner), in the app's build container
# (fleet/phone-app/Containerfile), and publishes it by rename as
# <aosp.sh --root-path>/phone-work/robotpark-<id>.apk, beside the probe app, <id> a hash of the
# app's sources; prints the path. A published app is never rebuilt or replaced, as aosp.sh does
# for the probe. Nothing it uses is from the Android SDK.
#
#   scripts/phone/app.sh              build if needed, print the path
#   scripts/phone/app.sh --apk-path   the path this checkout's sources build to
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)/fleet/phone-app
ROOT=$("$(dirname "$0")/aosp.sh" --root-path)
WORK=$ROOT/phone-work
id=$(cd "$SRC" && find . -type f -not -path '*/build/*' -not -path './.gradle/*' -not -path './.kotlin/*' -print0 \
  | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
APK=$WORK/robotpark-$id.apk
if [ "${1:-}" = --apk-path ]; then echo "$APK"; exit 0; fi
# One build and publication at a time in this checkout: a second run waits, then finds the
# app published (each build deletes and writes the checkout's app/build/apk, which the first
# run would otherwise copy from after the second had started).
mkdir -p "$WORK" "$SRC/app/build"
exec 9> "$SRC/app/build/.app-sh.lock"
flock 9
if [ ! -f "$APK" ]; then
  [ -f "$ROOT/tools/android.jar" ] || { echo "app.sh: run scripts/phone/aosp.sh first" >&2; exit 2; }
  # --no-print-directory: under another make (make phone-test) MAKEFLAGS asks for the
  # entering/leaving lines, which would join the path
  built=$(make -s --no-print-directory -C "$SRC" apk AOSP_TOOLS="$ROOT/tools")
  tmp=$(mktemp "$WORK/robotpark.XXXXXX")
  cp "$built" "$tmp"
  mv "$tmp" "$APK"
  rm -rf "$(dirname "$built")"
fi
echo "$APK"
