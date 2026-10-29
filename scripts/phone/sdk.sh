#!/bin/bash
# Fetches the Android SDK pieces Robot Park's Gradle build compiles against (the API 35
# platform and build-tools 35) into $ANDROID_SDK (default /mnt/fcvm-btrfs/android-sdk/<id>,
# <id> a hash of the pins below, so checkouts with other pins never share a directory and an
# installed package is never replaced), checking each archive's SHA-256. Nothing from here
# runs on the phone: the system image, adb and the probe app's tools are built from AOSP
# source (scripts/phone/aosp.sh), and scripts/phone/app.sh mounts this directory only into
# the app's build container.
#
#   ACCEPT_ANDROID_SDK_LICENSE=1 scripts/phone/sdk.sh
#   scripts/phone/sdk.sh --sdk-path       the SDK directory this checkout uses
#
# The archives are Google's and come under the Android Software Development Kit License
# Agreement (https://developer.android.com/studio/terms), which sdkmanager would show and ask
# you to accept; this script refuses to fetch them unless you have read and accepted it. The
# agreement permits using the SDK to develop applications for Android, which is what
# building Robot Park is. It forbids modifying or redistributing the SDK, so nothing here
# changes a fetched file, and nothing fetched leaves this host. Needs curl and python3.
set -euo pipefail
B=https://dl.google.com/android/repository
# url sha256 directory-in-zip destination-under-sdk
PINS="$B/build-tools_r35_linux.zip bd3a4966912eb8b30ed0d00b0cda6b6543b949d5ffe00bea54c04c81e1561d88 android-15 build-tools/35.0.0
$B/platform-35_r02.zip 0988cacad01b38a18a47bac14a0695f246bc76c1b06c0eeb8eb0dc825ab0c8e0 android-35 platforms/android-35"
SDK=${ANDROID_SDK:-/mnt/fcvm-btrfs/android-sdk/$(printf '%s\n' "$PINS" | sha256sum | cut -c1-12)}
[ "${1:-}" = --sdk-path ] && { echo "$SDK"; exit 0; }
[ "${ACCEPT_ANDROID_SDK_LICENSE:-}" = 1 ] || {
  echo "sdk.sh: read https://developer.android.com/studio/terms and set ACCEPT_ANDROID_SDK_LICENSE=1 to accept it" >&2
  exit 2
}
for tool in curl python3 sha256sum; do
  command -v "$tool" > /dev/null || { echo "sdk.sh: $tool is missing" >&2; exit 2; }
done
mkdir -p "$SDK/.downloads"
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

echo "sdk.sh: $SDK ready"
