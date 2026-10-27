#!/bin/bash
# Two sdk.sh runs on one SDK directory must not install or build at the same time: they
# share the downloads, the unpack directory, the installed packages and the probe build.
# Two runs on different SDK directories hold different locks, so they must share nothing:
# their probe builds must be in different directories. And a run's probe app must stay its
# own until it is installed: two checkouts with different probe sources build in turn, and
# the app the first would install must still be the first's. Checkouts with different pins
# must not share an SDK directory, where one would replace a package the other is using.
# A stub curl records how many sdk.sh runs are inside it at once, holds each call for 2 s
# and then fails, which stops that run; with the SDK lock the second run waits for the
# first to exit, so at most one is ever inside. Stub javac and keytool satisfy the tool
# check. The test fails unless both runs reached curl, so a run that stopped earlier
# cannot read as serialized.
#   scripts/phone/test-sdk-lock.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir/bin" "$dir/sdk"
cat > "$dir/bin/curl" <<STUB
#!/bin/bash
exec 8>>"$dir/count.lock"
flock 8
n=\$(( \$(cat "$dir/active" 2>/dev/null || echo 0) + 1 ))
echo \$n > "$dir/active"
[ \$n -le \$(cat "$dir/max" 2>/dev/null || echo 0) ] || echo \$n > "$dir/max"
echo call >> "$dir/calls"
flock -u 8
sleep 2
flock 8
echo \$(( \$(cat "$dir/active") - 1 )) > "$dir/active"
exit 1
STUB
printf '#!/bin/sh\nexit 0\n' > "$dir/bin/javac"
cp "$dir/bin/javac" "$dir/bin/keytool"
chmod +x "$dir/bin/curl" "$dir/bin/javac" "$dir/bin/keytool"
run() {
  PATH="$dir/bin:$PATH" ACCEPT_ANDROID_SDK_LICENSE=1 ANDROID_SDK="$dir/sdk" "$here/sdk.sh" \
    > "$dir/out.$1" 2>&1 || true
}
run 1 &
a=$!
run 2 &
b=$!
wait "$a" "$b"
calls=$(wc -l < "$dir/calls" 2>/dev/null || echo 0)
max=$(cat "$dir/max" 2>/dev/null || echo 0)
if [ "$calls" -lt 2 ]; then
  echo "FAIL: only $calls of the two sdk.sh runs reached the download"
  cat "$dir/out.1" "$dir/out.2"
  exit 1
fi
if [ "$max" -ne 1 ]; then
  echo "FAIL: $max sdk.sh runs were installing into one SDK at the same time"
  exit 1
fi
echo "ok   two sdk.sh runs on one SDK took turns ($calls downloads, at most $max at once)"

# Two SDKs side by side, each with every package already installed (the recorded SHA-256
# matches its pin), so each run goes straight to the probe build. The stub javac records
# where it was asked to build, then fails, which stops that run.
pins=0
for s in a b; do
  sed -n 's/^\(PINS="\)\{0,1\}\$B[^ ]* \([0-9a-f]\{64\}\) [^ ]* \([^"]*\)"\{0,1\}$/\2 \3/p' "$here/sdk.sh" | while read -r sha dest; do
    mkdir -p "$dir/$s/$dest"; echo "$sha" > "$dir/$s/$dest/.fcvm-sha256"; echo >> "$dir/pins"
  done
done
pins=$(wc -l < "$dir/pins")
[ "$pins" -eq 10 ] || { echo "FAIL: read $pins pins from sdk.sh, expected 5 for each of two SDKs"; exit 1; }
printf '#!/bin/bash
while [ $# -gt 0 ]; do [ "$1" = -d ] && echo "$2" >> "%s/javac-out"; shift; done
exit 1
' "$dir" > "$dir/bin/javac"
for s in a b; do PATH="$dir/bin:$PATH" ACCEPT_ANDROID_SDK_LICENSE=1 ANDROID_SDK="$dir/$s" "$here/sdk.sh" > "$dir/out.$s" 2>&1 || true; done
builds=$( (sort -u "$dir/javac-out" 2>/dev/null || true) | wc -l)
if [ "$(wc -l < "$dir/javac-out" 2>/dev/null || echo 0)" -ne 2 ]; then
  echo "FAIL: the two SDKs' runs did not both reach the probe build"; cat "$dir/out.a" "$dir/out.b"; exit 1
fi
if [ "$builds" -ne 2 ]; then
  echo "FAIL: two SDK directories built the probe app in one directory: $(sort -u "$dir/javac-out")"; exit 1
fi
echo "ok   two SDK directories build the probe app in two directories"

# Two checkouts on one SDK whose probe sources differ (B's manifest carries a marker). Stub
# build tools put the manifest they were given into the app, so the app shows whose it is.
# A builds, then B; the app A's run would install (the path its sdk.sh printed, which
# run.sh asks for with --probe-path) must still hold A's manifest.
sdk=$dir/shared
sed -n 's/^\(PINS="\)\{0,1\}\$B[^ ]* \([0-9a-f]\{64\}\) [^ ]* \([^"]*\)"\{0,1\}$/\2 \3/p' "$here/sdk.sh" | while read -r sha dest; do
  mkdir -p "$sdk/$dest"; echo "$sha" > "$sdk/$dest/.fcvm-sha256"
done
: > "$sdk/platforms/android-35/android.jar"
bt=$sdk/build-tools/35.0.0
printf '#!/bin/bash\nwhile [ $# -gt 0 ]; do [ "$1" = --output ] && touch "$2/classes.dex"; shift; done\n' > "$bt/d8"
cat > "$bt/aapt2" <<'STUB'
#!/bin/bash
while [ $# -gt 0 ]; do
  case "$1" in -o) out=$2 ;; --manifest) manifest=$2 ;; esac
  shift
done
python3 -I -c 'import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],"w"); z.write(sys.argv[2],"AndroidManifest.xml"); z.close()' "$out" "$manifest"
STUB
printf '#!/bin/bash\ncp "$3" "$4"\n' > "$bt/zipalign"
printf '#!/bin/bash\nwhile [ $# -gt 1 ]; do [ "$1" = --out ] && out=$2; shift; done\ncp "$1" "$out"\n' > "$bt/apksigner"
chmod +x "$bt/d8" "$bt/aapt2" "$bt/zipalign" "$bt/apksigner"
printf '#!/bin/sh\nexit 0\n' > "$dir/bin/javac"
printf '#!/bin/bash\nwhile [ $# -gt 0 ]; do [ "$1" = -keystore ] && touch "$2"; shift; done\n' > "$dir/bin/keytool"
root=$(cd "$here/../.." && pwd)
for c in A B; do
  mkdir -p "$dir/$c/scripts" "$dir/$c/fleet/phone"
  cp -r "$root/scripts/phone" "$dir/$c/scripts/"
  cp -r "$root/fleet/phone/probe" "$dir/$c/fleet/phone/"
done
echo "<!-- checkout B -->" >> "$dir/B/fleet/phone/probe/AndroidManifest.xml"
for c in A B; do
  PATH="$dir/bin:$PATH" ACCEPT_ANDROID_SDK_LICENSE=1 ANDROID_SDK="$sdk" "$dir/$c/scripts/phone/sdk.sh" > "$dir/out.$c" 2>&1 \
    || { echo "FAIL: checkout $c's sdk.sh failed:"; cat "$dir/out.$c"; exit 1; }
done
apk_a=$(sed -n 's/.*probe app \(.*\)$/\1/p' "$dir/out.A" | tail -1)
if ! python3 -I -c 'import sys,zipfile; sys.exit(0 if zipfile.ZipFile(sys.argv[1]).read("AndroidManifest.xml") == open(sys.argv[2],"rb").read() else 1)' \
    "$apk_a" "$dir/A/fleet/phone/probe/AndroidManifest.xml" 2>/dev/null; then
  echo "FAIL: checkout A's probe app at $apk_a was replaced by another checkout's build"; exit 1
fi
asked=$(ANDROID_SDK="$sdk" "$dir/A/scripts/phone/sdk.sh" --probe-path 2>/dev/null || true)
[ "$asked" = "$apk_a" ] || { echo "FAIL: sdk.sh --probe-path names '$asked', not the app it built ($apk_a)"; exit 1; }
echo "ok   a checkout's probe app stays its own when another checkout builds"

# The default SDK directory follows the pins: the same pins share it, other pins do not.
same=$(env -u ANDROID_SDK "$dir/B/scripts/phone/sdk.sh" --sdk-path 2>/dev/null || true)
[ -n "$same" ] && [ "$same" = "$(env -u ANDROID_SDK "$dir/A/scripts/phone/sdk.sh" --sdk-path 2>/dev/null || true)" ] \
  || { echo "FAIL: two checkouts with the same pins use different SDK directories"; exit 1; }
sed -i 's/b08fc43d/c08fc43d/' "$dir/B/scripts/phone/sdk.sh"
other=$(env -u ANDROID_SDK "$dir/B/scripts/phone/sdk.sh" --sdk-path 2>/dev/null || true)
[ "$other" != "$same" ] || { echo "FAIL: a checkout with other pins uses the same SDK directory ($same)"; exit 1; }
echo "ok   checkouts with other pins use another SDK directory"
