#!/bin/bash
# Builds the phone's Android system image and the host tools the phone test uses from AOSP
# source (docs/fleet/ARCHITECTURE.md, "Phones mode", Licensing), into
# $AOSP_BUILD_DIR/dist (default /mnt/fcvm-btrfs/aosp-build/<id>/dist, <id> a hash of this
# script and the pinned manifest), and prints that directory:
#
#   aosp-system-<id>.zip    `m emu_img_zip` for sdk_phone64_x86_64: the emulator's system
#                           image in the layout of system-images/android-35/default/x86_64
#   aosp-tools-<id>.tar.gz  adb, and what builds an app without the SDK: aapt2, d8,
#                           zipalign, apksigner and the public API's android.jar
#   SHA256SUMS
#
#   scripts/phone/aosp-build.sh          build unless built; print the dist directory
#   scripts/phone/aosp-build.sh --id     the <id> this checkout's recipe and pins build
#
# scripts/phone/aosp.sh pins the published artifacts' checksums and fetches them; after a
# change here or to the manifest, build, publish the dist directory as the GitHub release
# aosp-<id> on ejc3/fcvm and pin the new checksums there.
#
# The sources are AOSP's android-15.0.0_r36, every project pinned to a commit in
# fleet/phone/aosp.xml. The build uses the toolchains and prebuilts the tree pins, with `repo`
# fetched at a pinned checksum, and fixes the stamps a build otherwise takes from the host
# (date, user, host name, build number), so the image does not record where or when it was
# built. AOSP's prebuilt toolchains are x86_64 Linux binaries, so the build runs only on an
# x86_64 Linux host. It needs git, curl, python3, zip, unzip, rsync and about 400 GB free;
# the sources and build tree are deleted afterwards unless AOSP_KEEP_SOURCES=1.
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)
MANIFEST=$SRC/fleet/phone/aosp.xml
TAG=android-15.0.0_r36
LUNCH=sdk_phone64_x86_64-bp1a-userdebug
# 2025-05-05T00:00:00Z, the date in the tag's build ID (BP1A.250505.005.D1).
BUILD_DATETIME=1746403200
# A versioned launcher: the unversioned .../repo is replaced by every release, so a
# checksum of it goes stale (it had by 2026-10-28).
REPO_URL=https://storage.googleapis.com/git-repo-downloads/repo-2.71
REPO_SHA256=05f571deafd6ed6da0d20fa707dde29aa5624dbf35553d697a40dbdd4c8d178a
ID=$(cat "$0" "$MANIFEST" | sha256sum | cut -c1-12)
ROOT=${AOSP_BUILD_DIR:-/mnt/fcvm-btrfs/aosp-build/$ID}
DIST=$ROOT/dist
case "${1:-}" in
  --id) echo "$ID"; exit 0 ;;
esac
[ -f "$DIST/SHA256SUMS" ] && { echo "$DIST"; exit 0; }
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] \
  || { echo "aosp-build.sh: AOSP builds only on an x86_64 Linux host (its toolchains are linux-x86 prebuilts)" >&2; exit 2; }
for tool in git curl python3 sha256sum flock zip unzip rsync readelf; do
  command -v "$tool" > /dev/null || { echo "aosp-build.sh: $tool is missing" >&2; exit 2; }
done
mkdir -p "$ROOT"
# One build per pin set: concurrent callers wait for it and reuse it.
exec 9> "$ROOT.lock"
flock 9
[ -f "$DIST/SHA256SUMS" ] && { echo "$DIST"; exit 0; }
WORK=$ROOT/src
LOG=$ROOT/build.log
mkdir -p "$WORK"
repo=$ROOT/repo
if [ ! -x "$repo" ]; then
  curl -fsSL -o "$repo.tmp" "$REPO_URL"
  echo "$REPO_SHA256  $repo.tmp" | sha256sum -c --status \
    || { echo "aosp-build.sh: $REPO_URL does not have SHA-256 $REPO_SHA256" >&2; rm -f "$repo.tmp"; exit 1; }
  chmod +x "$repo.tmp" && mv "$repo.tmp" "$repo"
fi
cd "$WORK"
t0=$(date +%s)
echo "aosp-build.sh: fetching the pinned sources into $WORK (log $LOG)" >&2
{
  # repo needs a manifest repository to start from; the pinned manifest then replaces its
  # manifest, and every project is checked out at the commit it names.
  [ -d .repo ] || "$repo" init -q --depth=1 -u https://android.googlesource.com/platform/manifest -b "refs/tags/$TAG" < /dev/null
  cp "$MANIFEST" .repo/manifests/fcvm.xml
  "$repo" init -q -m fcvm.xml < /dev/null
  # android.googlesource.com answers RESOURCE_EXHAUSTED to too many fetches at once (it did
  # at 64), so the sync is narrower and retried; a rerun fetches only what is missing.
  synced=0
  for _ in 1 2 3; do
    if "$repo" sync -q -c -j16 --retry-fetches=5 --no-tags --no-clone-bundle; then synced=1; break; fi
    sleep 60
  done
  [ "$synced" = 1 ]
} >> "$LOG" 2>&1 || { tail -30 "$LOG" >&2; exit 1; }
t1=$(date +%s)
# repo checks out the pinned commits; make sure, since the build is only as pinned as this.
# A checkout must also hold every file of its commit: a sync stopped part way (a fetch
# refused with RESOURCE_EXHAUSTED did that) left projects at the right commit with files
# missing from the work tree.
python3 -I - "$MANIFEST" "$WORK" <<'PY' || exit 1
import subprocess, sys, xml.etree.ElementTree as ET
from concurrent.futures import ThreadPoolExecutor
manifest, work = sys.argv[1:]
def check(p):
    path = p.get("path") or p.get("name")
    git = ["git", "-C", f"{work}/{path}"]
    head = subprocess.run(git + ["rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    if head != p.get("revision"):
        return f"{path}: {head or 'missing'} != {p.get('revision')}"
    dirty = subprocess.run(git + ["status", "--porcelain", "--untracked-files=no"],
                           capture_output=True, text=True).stdout
    if dirty:
        return f"{path}: the work tree differs from {head} ({len(dirty.splitlines())} files)"
with ThreadPoolExecutor(32) as pool:
    bad = [b for b in pool.map(check, ET.parse(manifest).getroot().iter("project")) if b]
if bad:
    sys.exit("aosp-build.sh: checkouts differ from the manifest:\n  " + "\n  ".join(bad))
PY
echo "aosp-build.sh: building $LUNCH (log $LOG)" >&2
# envsetup.sh and lunch read unset variables and return non-zero from internal checks, so
# they run without -eu; `m` decides the result.
TARGETS="droid emu_img_zip adb aapt2 zipalign apksigner d8 android_stubs_current"
env BUILD_DATETIME=$BUILD_DATETIME BUILD_USERNAME=fcvm BUILD_HOSTNAME=fcvm BUILD_NUMBER="$ID" \
  bash -c 'set +eu; source build/envsetup.sh > /dev/null && lunch "$1" && m $2' _ "$LUNCH" "$TARGETS" \
  >> "$LOG" 2>&1 || { tail -40 "$LOG" >&2; exit 1; }
t2=$(date +%s)

# The artifacts, staged and then published into $DIST by rename.
stage=$ROOT/dist.tmp
rm -rf "$stage" && mkdir -p "$stage"
mapfile -t zips < <(find out/target/product/emu64x -maxdepth 1 -name 'sdk-repo-linux-system-images*.zip')
[ "${#zips[@]}" = 1 ] || { echo "aosp-build.sh: expected one system image package, found ${#zips[@]}: ${zips[*]}" >&2; exit 1; }
cp "${zips[0]}" "$stage/aosp-system-$ID.zip"

H=out/host/linux-x86
T=$stage/tools
mkdir -p "$T/bin" "$T/lib64" "$T/framework" "$T/notices"
for b in adb aapt2 zipalign; do cp "$H/bin/$b" "$T/bin/"; done
for j in d8.jar apksigner.jar; do
  [ -f "$H/framework/$j" ] || { echo "aosp-build.sh: $H/framework/$j was not built" >&2; exit 1; }
  cp "$H/framework/$j" "$T/framework/"
done
# The host binaries find their shared libraries through RUNPATH $ORIGIN/../lib64; copy what
# they need from the build, transitively. Anything not in the build is the host's libc.
python3 -I - "$H/lib64" "$T" <<'PY'
import os, shutil, subprocess, sys
libdir, tools = sys.argv[1:]
def needed(path):
    out = subprocess.run(["readelf", "-d", path], capture_output=True, text=True, check=True).stdout
    return [l.split("[", 1)[1].split("]", 1)[0] for l in out.splitlines() if "(NEEDED)" in l]
todo = [os.path.join(tools, "bin", b) for b in os.listdir(os.path.join(tools, "bin"))]
seen = set()
while todo:
    for lib in needed(todo.pop()):
        src = os.path.join(libdir, lib)
        if lib in seen or not os.path.exists(src):
            continue
        seen.add(lib)
        dst = os.path.join(tools, "lib64", lib)
        shutil.copy(src, dst)
        todo.append(dst)
PY
# What an app builds against, as the SDK's platforms/android-35/android.jar holds it: the
# public API's stubs for javac and d8, and the framework's resource table and manifest
# (from framework-res.apk) for aapt2's -I.
mapfile -t jars < <(find out/soong/.intermediates/frameworks/base -path '*android_stubs_current/android_common/combined/android_stubs_current.jar')
[ "${#jars[@]}" = 1 ] || { echo "aosp-build.sh: expected one android_stubs_current.jar, found ${#jars[@]}: ${jars[*]}" >&2; exit 1; }
python3 -I - "${jars[0]}" out/target/product/emu64x/system/framework/framework-res.apk "$T/android.jar" <<'PY'
import sys, zipfile
stubs, res, out = sys.argv[1:]
entries = {}
with zipfile.ZipFile(stubs) as z:
    for i in z.infolist():
        if not i.is_dir():
            entries[i.filename] = z.read(i)
with zipfile.ZipFile(res) as z:
    for name in ("AndroidManifest.xml", "resources.arsc"):
        entries[name] = z.read(name)
with zipfile.ZipFile(out, "w") as z:
    for name in sorted(entries):
        info = zipfile.ZipInfo(name, date_time=(2008, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        info.external_attr = 0o644 << 16
        z.writestr(info, entries[name])
PY
# Each tool's license, from its source: adb, aapt2, android.jar, zipalign and apksigner are
# Apache 2.0, d8 (prebuilts/r8, the R8 project's release) BSD 3-clause.
cp build/soong/licenses/LICENSE "$T/notices/Apache-2.0"
cp packages/modules/adb/NOTICE "$T/notices/adb.NOTICE"
cp frameworks/base/NOTICE "$T/notices/aapt2-and-android.jar.NOTICE"
cp tools/apksig/LICENSE "$T/notices/apksigner.LICENSE"
cp prebuilts/r8/LICENSE "$T/notices/d8.LICENSE"
cp "$MANIFEST" "$T/notices/aosp.xml"
tar -C "$stage" --sort=name --mtime="@$BUILD_DATETIME" --owner=0 --group=0 --numeric-owner \
  --mode=u+rwX,go+rX,go-w -cf - tools \
  | gzip -n > "$stage/aosp-tools-$ID.tar.gz"
rm -rf "$T"
(cd "$stage" && sha256sum aosp-system-"$ID".zip aosp-tools-"$ID".tar.gz > SHA256SUMS)
printf 'sync_s=%s build_s=%s\n' $(( t1 - t0 )) $(( t2 - t1 )) > "$stage/times"
rm -rf "$DIST" && mv "$stage" "$DIST"
cd /
[ "${AOSP_KEEP_SOURCES:-}" = 1 ] || rm -rf "$WORK"
echo "$DIST"
