#!/bin/bash
# Builds the Android Emulator from source into $ANDROID_EMULATOR (default
# /mnt/fcvm-btrfs/android-emulator/<id>, <id> a hash of this script and the pinned manifest)
# and prints the directory holding the `emulator` launcher. A build is reused once it exists.
#
#   scripts/phone/emulator.sh          build unless built; print the emulator directory
#   scripts/phone/emulator.sh --path   the emulator directory this checkout's pins build to
#
# The sources are AOSP's emu-main-dev, every project pinned to a commit in
# fleet/phone/emulator.xml, with external/qemu and hardware/google/gfxstream from the
# ejc3/platform_external_qemu and ejc3/gfxstream forks, which add the console's `gpu release`
# and `gpu retake` (docs/fleet/ARCHITECTURE.md, "Phones mode"). The SDK's emulator may not
# be modified, so the phone runs this one. The build uses the toolchain and prebuilts the
# manifest pins (clang, cmake, ninja, python), with `repo` fetched at a pinned checksum.
# It needs git, curl, python3 and about 120 GB free while it builds; the sources and build
# tree are deleted afterwards unless EMULATOR_KEEP_SOURCES=1. About 25 minutes on 96 cores.
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)
MANIFEST=$SRC/fleet/phone/emulator.xml
REPO_URL=https://storage.googleapis.com/git-repo-downloads/repo
REPO_SHA256=6ed2f6325705b6ea0af99466d6ebd04adec387a5b6a906cd77bc8c6da65123e5
ID=$(cat "$0" "$MANIFEST" | sha256sum | cut -c1-12)
ROOT=${ANDROID_EMULATOR:-/mnt/fcvm-btrfs/android-emulator/$ID}
OUT=$ROOT/emulator
[ "${1:-}" = --path ] && { echo "$OUT"; exit 0; }
[ -x "$OUT/emulator" ] && { echo "$OUT"; exit 0; }
for tool in git curl python3 sha256sum flock; do
  command -v "$tool" > /dev/null || { echo "emulator.sh: $tool is missing" >&2; exit 2; }
done
mkdir -p "$ROOT"
# One build per pin set: concurrent callers wait for it and reuse it.
exec 9> "$ROOT.lock"
flock 9
[ -x "$OUT/emulator" ] && { echo "$OUT"; exit 0; }
WORK=$ROOT/src
LOG=$ROOT/build.log
mkdir -p "$WORK"
repo=$ROOT/repo
if [ ! -x "$repo" ]; then
  curl -fsSL -o "$repo.tmp" "$REPO_URL"
  echo "$REPO_SHA256  $repo.tmp" | sha256sum -c --status \
    || { echo "emulator.sh: $REPO_URL does not have SHA-256 $REPO_SHA256" >&2; rm -f "$repo.tmp"; exit 1; }
  chmod +x "$repo.tmp" && mv "$repo.tmp" "$repo"
fi
cd "$WORK"
echo "emulator.sh: fetching the pinned sources into $WORK (log $LOG)" >&2
{
  # repo needs a manifest repository to start from; the pinned manifest then replaces its
  # manifest, and every project is checked out at the commit it names.
  [ -d .repo ] || "$repo" init -q --depth=1 -u https://android.googlesource.com/platform/manifest -b emu-main-dev < /dev/null
  cp "$MANIFEST" .repo/manifests/fcvm.xml
  "$repo" init -q -m fcvm.xml < /dev/null
  "$repo" sync -q -c -j32 --no-tags --no-clone-bundle --fail-fast
} >> "$LOG" 2>&1 || { tail -30 "$LOG" >&2; exit 1; }
# repo checks out the pinned commits; make sure, since the build is only as pinned as this.
python3 -I - "$MANIFEST" "$WORK" <<'PY' || exit 1
import subprocess, sys, xml.etree.ElementTree as ET
manifest, work = sys.argv[1:]
bad = []
for p in ET.parse(manifest).getroot().iter("project"):
    path = p.get("path") or p.get("name")
    head = subprocess.run(["git", "-C", f"{work}/{path}", "rev-parse", "HEAD"],
                          capture_output=True, text=True).stdout.strip()
    if head != p.get("revision"):
        bad.append(f"{path}: {head or 'missing'} != {p.get('revision')}")
if bad:
    sys.exit("emulator.sh: checkouts differ from the manifest:\n  " + "\n  ".join(bad))
PY
echo "emulator.sh: building (log $LOG)" >&2
(
  cd external/qemu
  android/rebuild.sh --target linux --task-disable ctest --task-disable coveragereport \
    --task-disable packagesamples --task-disable zipintegrationtests --task-disable integrationtest
) >> "$LOG" 2>&1 || { tail -40 "$LOG" >&2; exit 1; }
rm -rf "$OUT.tmp"
cp -a external/qemu/objs/distribution/emulator "$OUT.tmp"
mv "$OUT.tmp" "$OUT"
cd /
[ "${EMULATOR_KEEP_SOURCES:-}" = 1 ] || rm -rf "$WORK"
echo "$OUT"
