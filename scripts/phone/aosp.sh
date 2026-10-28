#!/bin/bash
# Fetches the phone's Android pieces built from AOSP source (scripts/phone/aosp-build.sh)
# into $ANDROID_AOSP (default /mnt/fcvm-btrfs/android-aosp/<id>, <id> a hash of the pins
# below, so checkouts with other pins never share a directory and an installed piece is
# never replaced), checking each archive's SHA-256, and builds the probe app
# (fleet/phone/probe) with the fetched tools into $ANDROID_AOSP/phone-work/probe-<id>.apk,
# <id> a hash of the probe's sources. An app is built once, in a directory of its own, and
# published by rename; nothing replaces it, so a run installs the app of its own checkout
# however many other checkouts share the directory.
#
#   scripts/phone/aosp.sh
#   scripts/phone/aosp.sh --probe-path    the app this checkout's sources build to
#   scripts/phone/aosp.sh --root-path     the directory this checkout uses (mapped at
#                                         /android in the phone container)
#
# The directory has the layout the emulator looks for under ANDROID_SDK_ROOT:
#   system-images/android-35/default/x86_64/   the system image (sdk_phone64_x86_64)
#   platform-tools/adb                         a link to tools/bin/adb
#   tools/                                     adb, aapt2, zipalign, d8, apksigner,
#                                              android.jar, and their licenses
# Nothing in it is from the Android SDK: the archives are the GitHub release aosp-<build id>
# on ejc3/fcvm, which aosp-build.sh built from the pinned AOSP sources. Needs curl,
# python3 and a JDK (javac, java, keytool).
set -euo pipefail
# The recipe (aosp-build.sh --id) the archives below were built from.
BUILD_ID=a255e314f1c7
# Where they are published; AOSP_RELEASE_URL points at another copy (file:// for a build on
# this host before it is published).
R=${AOSP_RELEASE_URL:-https://github.com/ejc3/fcvm/releases/download/aosp-$BUILD_ID}
# url sha256 directory-in-archive destination
PINS="$R/aosp-system-$BUILD_ID.zip 81a705bff998b2eddc12dd5a69308e608db05db8d713a0346686cb71e49fa8fa x86_64 system-images/android-35/default/x86_64
$R/aosp-tools-$BUILD_ID.tar.gz 3d88498d9dd7675be475567c882efdf55cad57b768df6c4619f16ae8fc21bd58 tools tools"
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=${ANDROID_AOSP:-/mnt/fcvm-btrfs/android-aosp/$(printf '%s\n' "$PINS" | sha256sum | cut -c1-12)}
WORK=$ROOT/phone-work
SRC=$(cd "$HERE/../.." && pwd)/fleet/phone/probe
probe_id=$(cd "$SRC" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
APK=$WORK/probe-$probe_id.apk
case "${1:-}" in
  --probe-path) echo "$APK"; exit 0 ;;
  --root-path) echo "$ROOT"; exit 0 ;;
esac
# The pins must be the build of this checkout's recipe and manifest; after a change to
# either, build (make phone-system-image), publish and pin.
recipe=$("$HERE/aosp-build.sh" --id)
[ "$recipe" = "$BUILD_ID" ] || {
  echo "aosp.sh: the pinned archives are build $BUILD_ID, but aosp-build.sh and fleet/phone/aosp.xml now build $recipe: run make phone-system-image on an x86_64 host, publish its dist directory as the release aosp-$recipe and pin it here" >&2
  exit 2
}
for tool in curl python3 javac java keytool sha256sum tar; do
  command -v "$tool" > /dev/null || { echo "aosp.sh: $tool is missing (javac/java/keytool: apt-get install openjdk-17-jdk-headless)" >&2; exit 2; }
done
mkdir -p "$ROOT/.downloads" "$WORK"
# One install at a time per directory: concurrent runs (two GPUs, two worktrees) share the
# downloads, the unpack directory and the installed pieces. The lock is held until this
# script exits.
exec 9> "$ROOT/.lock"
flock 9
while read -r url sha top dest; do
  [ -f "$ROOT/$dest/.fcvm-sha256" ] && [ "$(cat "$ROOT/$dest/.fcvm-sha256")" = "$sha" ] && continue
  archive="$ROOT/.downloads/$(basename "$url")"
  if [ ! -f "$archive" ]; then curl -fsSL -o "$archive.tmp" "$url" && mv "$archive.tmp" "$archive"; fi
  echo "$sha  $archive" | sha256sum -c --status || { echo "aosp.sh: $archive does not have SHA-256 $sha" >&2; rm -f "$archive"; exit 1; }
  rm -rf "${ROOT:?}/$dest" "$ROOT/.unpack"
  mkdir -p "$ROOT/.unpack"
  case "$archive" in
  *.tar.gz) tar -C "$ROOT/.unpack" -xzf "$archive" ;;
  *.zip)
    # Python's zipfile drops modes and symlinks; the emulator needs both.
    python3 -I - "$archive" "$ROOT/.unpack" <<'PY'
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
    ;;
  esac
  mkdir -p "$(dirname "$ROOT/$dest")"
  mv "$ROOT/.unpack/$top" "$ROOT/$dest"
  rm -rf "$ROOT/.unpack"
  echo "$sha" > "$ROOT/$dest/.fcvm-sha256"
  echo "aosp.sh: $dest from $(basename "$url")"
done <<< "$PINS"
mkdir -p "$ROOT/platform-tools"
ln -sfn ../tools/bin/adb "$ROOT/platform-tools/adb"

# The probe app, with the fetched tools, unless this checkout's sources are built already.
if [ ! -f "$APK" ]; then
  T=$ROOT/tools; JAR=$T/android.jar
  OUT=$(mktemp -d "$WORK/build.XXXXXX")
  trap 'rm -rf "$OUT"' EXIT
  mkdir -p "$OUT/classes" "$OUT/dex"
  mapfile -t sources < <(find "$SRC/src" -name '*.java')
  javac -source 11 -target 11 -Xlint:-options -classpath "$JAR" -d "$OUT/classes" "${sources[@]}"
  mapfile -t classes < <(find "$OUT/classes" -name '*.class')
  java -cp "$T/framework/d8.jar" com.android.tools.r8.D8 --lib "$JAR" --min-api 30 --output "$OUT/dex" "${classes[@]}"
  "$T/bin/aapt2" link -o "$OUT/base.apk" --manifest "$SRC/AndroidManifest.xml" -I "$JAR" --min-sdk-version 30 --target-sdk-version 35
  python3 -I -c 'import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],"a"); z.write(sys.argv[2],"classes.dex"); z.close()' "$OUT/base.apk" "$OUT/dex/classes.dex"
  "$T/bin/zipalign" -f 4 "$OUT/base.apk" "$OUT/aligned.apk"
  keytool -genkeypair -keystore "$OUT/probe.keystore" -storepass android -keypass android \
    -alias probe -keyalg RSA -dname CN=probe -validity 10000 > /dev/null 2>&1
  java -cp "$T/framework/apksigner.jar" com.android.apksigner.ApkSignerTool sign \
    --ks "$OUT/probe.keystore" --ks-pass pass:android --out "$OUT/probe.apk" "$OUT/aligned.apk"
  mv "$OUT/probe.apk" "$APK"
fi
echo "aosp.sh: $ROOT ready, probe app $APK"
