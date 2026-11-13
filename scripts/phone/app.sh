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
# The id names the app's sources: every file in $SRC but those its .gitignore ignores, the
# outputs a build leaves there. A build must not change the id: build.sh builds the app and
# asks for its path afterwards, and a cold `make apk` stages the Gradle archive in
# .gradle-dist/, which once moved the path to an app nobody built (ejc3/aws#823). Each line of
# .gitignore is a directory (`name/`) or a file name pattern, ignored at any depth, as git does.
skip=()
while IFS= read -r pattern; do
  case "$pattern" in '' | '#'*) continue ;; esac
  [[ "$pattern" =~ ^[A-Za-z0-9._*-]+/?$ ]]     || { echo "app.sh: $SRC/.gitignore: '$pattern' is not a directory or file name pattern" >&2; exit 2; }
  if [[ "$pattern" == */ ]]; then skip+=(-not -path "*/${pattern%/}/*"); else skip+=(-not -name "$pattern"); fi
done < "$SRC/.gitignore"
id=$(cd "$SRC" && find . -type f "${skip[@]}" -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
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
