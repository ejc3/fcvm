#!/bin/bash
# Fetches the pinned Android SDK pieces the phone VM uses into $ANDROID_SDK (default
# /mnt/fcvm-btrfs/android-sdk/<id>, <id> a hash of the pins below, so checkouts with other
# pins never share a directory and an installed package is never replaced), checking each
# archive's SHA-256, and builds the probe app (fleet/phone/probe) without Gradle into
# $ANDROID_SDK/phone-work/probe-<id>.apk, <id> a hash of the probe's sources. An app is built once, in a directory of its own,
# and published by rename; nothing replaces it, so a run installs the app of its own
# checkout however many other checkouts share the SDK.
#
#   ACCEPT_ANDROID_SDK_LICENSE=1 scripts/phone/sdk.sh
#   scripts/phone/sdk.sh --probe-path     the app this checkout's sources build to
#   scripts/phone/sdk.sh --sdk-path       the SDK directory this checkout uses
#
# The archives are Google's and come under the Android Software Development Kit License
# Agreement (https://developer.android.com/studio/terms), which sdkmanager would show and ask
# you to accept; this script refuses to fetch them unless you have read and accepted it. The
# agreement permits using the SDK to develop applications for Android, which is what the
# phone test does. It forbids modifying or redistributing the SDK, so nothing here changes a
# fetched file, and nothing fetched leaves this host. Needs curl, python3 and a JDK (javac,
# keytool).
set -euo pipefail
B=https://dl.google.com/android/repository
# url sha256 directory-in-zip destination-under-sdk
PINS="$B/emulator-linux_x64-16428233.zip b08fc43d8608d2955f607f1b287beb525041e730086bdca3af152accac3af9c1 emulator emulator
$B/platform-tools_r37.0.1-linux.zip d230f13842f60f782a8645f9c813f8f845bf36089ea7289f28c48f17979313f1 platform-tools platform-tools
$B/build-tools_r35_linux.zip bd3a4966912eb8b30ed0d00b0cda6b6543b949d5ffe00bea54c04c81e1561d88 android-15 build-tools/35.0.0
$B/platform-35_r02.zip 0988cacad01b38a18a47bac14a0695f246bc76c1b06c0eeb8eb0dc825ab0c8e0 android-35 platforms/android-35
$B/sys-img/android/x86_64-35_r02.zip 6dd7de33e63ef105cf2fabea6badda1dbe7665c96d8908e5f6e1407e63ff4556 x86_64 system-images/android-35/default/x86_64"
SDK=${ANDROID_SDK:-/mnt/fcvm-btrfs/android-sdk/$(printf '%s\n' "$PINS" | sha256sum | cut -c1-12)}
WORK=$SDK/phone-work
SRC=$(cd "$(dirname "$0")/../.." && pwd)/fleet/phone/probe
probe_id=$(cd "$SRC" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
APK=$WORK/probe-$probe_id.apk
case "${1:-}" in
  --probe-path) echo "$APK"; exit 0 ;;
  --sdk-path) echo "$SDK"; exit 0 ;;
esac
[ "${ACCEPT_ANDROID_SDK_LICENSE:-}" = 1 ] || {
  echo "sdk.sh: read https://developer.android.com/studio/terms and set ACCEPT_ANDROID_SDK_LICENSE=1 to accept it" >&2
  exit 2
}
for tool in curl python3 javac keytool sha256sum; do
  command -v "$tool" > /dev/null || { echo "sdk.sh: $tool is missing (javac/keytool: apt-get install openjdk-17-jdk-headless)" >&2; exit 2; }
done
mkdir -p "$SDK/.downloads" "$WORK"
# One install at a time per SDK: concurrent runs (two GPUs, two worktrees) share the
# downloads, the unpack directory and the packages. The lock is held until this script
# exits.
exec 9> "$SDK/.lock"
flock 9
while read -r url sha top dest; do
  [ -f "$SDK/$dest/.fcvm-sha256" ] && [ "$(cat "$SDK/$dest/.fcvm-sha256")" = "$sha" ] && continue
  zip="$SDK/.downloads/$(basename "$url")"
  if [ ! -f "$zip" ]; then curl -fsSL -o "$zip.tmp" "$url" && mv "$zip.tmp" "$zip"; fi
  echo "$sha  $zip" | sha256sum -c --status || { echo "sdk.sh: $zip does not have SHA-256 $sha" >&2; rm -f "$zip"; exit 1; }
  rm -rf "${SDK:?}/$dest" "$SDK/.unpack"
  # Python's zipfile drops modes and symlinks; the emulator needs both.
  python3 -I - "$zip" "$SDK/.unpack" <<'PY'
import os, stat, sys, zipfile
z = zipfile.ZipFile(sys.argv[1]); out = sys.argv[2]
for info in z.infolist():
    path = os.path.join(out, info.filename)
    if not os.path.realpath(path).startswith(os.path.realpath(out) + os.sep):
        sys.exit(f"refusing {info.filename}: outside the destination")
    mode = info.external_attr >> 16
    if info.is_dir():
        os.makedirs(path, exist_ok=True); continue
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if stat.S_ISLNK(mode):
        os.symlink(z.read(info).decode(), path); continue
    with z.open(info) as src, open(path, "wb") as dst:
        dst.write(src.read())
    os.chmod(path, (mode & 0o777) or 0o644)
PY
  mkdir -p "$(dirname "$SDK/$dest")"
  mv "$SDK/.unpack/$top" "$SDK/$dest"
  rm -rf "$SDK/.unpack"
  echo "$sha" > "$SDK/$dest/.fcvm-sha256"
  echo "sdk.sh: $dest from $(basename "$url")"
done <<< "$PINS"

# The probe app, with the SDK's own tools, unless this checkout's sources are built already.
if [ ! -f "$APK" ]; then
  BT=$SDK/build-tools/35.0.0; JAR=$SDK/platforms/android-35/android.jar
  OUT=$(mktemp -d "$WORK/build.XXXXXX")
  trap 'rm -rf "$OUT"' EXIT
  mkdir -p "$OUT/classes" "$OUT/dex"
  mapfile -t sources < <(find "$SRC/src" -name '*.java')
  javac -source 11 -target 11 -Xlint:-options -classpath "$JAR" -d "$OUT/classes" "${sources[@]}"
  mapfile -t classes < <(find "$OUT/classes" -name '*.class')
  "$BT/d8" --lib "$JAR" --min-api 30 --output "$OUT/dex" "${classes[@]}"
  "$BT/aapt2" link -o "$OUT/base.apk" --manifest "$SRC/AndroidManifest.xml" -I "$JAR" --min-sdk-version 30 --target-sdk-version 35
  python3 -I -c 'import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],"a"); z.write(sys.argv[2],"classes.dex"); z.close()' "$OUT/base.apk" "$OUT/dex/classes.dex"
  "$BT/zipalign" -f 4 "$OUT/base.apk" "$OUT/aligned.apk"
  keytool -genkeypair -keystore "$OUT/probe.keystore" -storepass android -keypass android \
    -alias probe -keyalg RSA -dname CN=probe -validity 10000 > /dev/null 2>&1
  "$BT/apksigner" sign --ks "$OUT/probe.keystore" --ks-pass pass:android --out "$OUT/probe.apk" "$OUT/aligned.apk"
  mv "$OUT/probe.apk" "$APK"
fi
echo "sdk.sh: $SDK ready, probe app $APK"
