#!/bin/bash
# Two sdk.sh runs on one SDK directory must not install or build at the same time: they
# share the downloads, the unpack directory, the installed packages and the probe build.
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
