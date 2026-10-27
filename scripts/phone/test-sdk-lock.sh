#!/bin/bash
# Two sdk.sh runs on one SDK directory must not install or build at the same time: they
# share the downloads, the unpack directory, the installed packages and the probe build.
# Two runs on different SDK directories hold different locks, so they must share nothing:
# their probe builds must be in different directories.
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
