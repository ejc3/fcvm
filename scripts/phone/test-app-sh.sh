#!/bin/bash
# scripts/phone/app.sh against a stand-in make: two runs from one checkout at once both get
# the published app. The stand-in builds as build-apk.sh does: under its own lock it deletes
# and writes app/build/apk. It prints the APK's path 0.2 s after it lets the next build in,
# which writes its own APK 0.5 s after deleting the old one, so without app.sh's own lock
# the first run copies an APK that is no longer there.
#   scripts/phone/test-app-sh.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir/scripts/phone" "$dir/fleet/phone-app/app" "$dir/aosp/tools" "$dir/bin"
cp "$here/app.sh" "$dir/scripts/phone/"
printf '#!/bin/bash\necho "%s/aosp"\n' "$dir" > "$dir/scripts/phone/aosp.sh"
echo source > "$dir/fleet/phone-app/app/a.kt"
: > "$dir/aosp/tools/android.jar"
cat > "$dir/bin/make" <<'STUB'
#!/bin/bash
src=${@: -2:1}
for a in "$@"; do case "$a" in -C) next=1 ;; *) [ -n "${next:-}" ] && { src=$a; next=""; } ;; esac; done
cd "$src"
mkdir -p app/build
exec 9> app/build/.apk.lock
flock 9
rm -rf app/build/apk && mkdir -p app/build/apk
sleep 0.5
echo "apk $$" > app/build/apk/robotpark.apk
# lets the next build in, and returns only after it has started, as a slow return would:
# that build's delete then lands before this caller copies the APK
flock -u 9
sleep 0.2
echo "$src/app/build/apk/robotpark.apk"
STUB
chmod +x "$dir/scripts/phone/aosp.sh" "$dir/bin/make" "$dir/scripts/phone/app.sh"
fail=0
for n in 1 2; do PATH="$dir/bin:$PATH" "$dir/scripts/phone/app.sh" > "$dir/out.$n" 2>&1 & eval "p$n=\$!"; done
rc1=0; rc2=0
wait "$p1" || rc1=$?
wait "$p2" || rc2=$?
if [ "$rc1" = 0 ] && [ "$rc2" = 0 ] && [ -s "$(cat "$dir/out.1")" ] && [ "$(cat "$dir/out.1")" = "$(cat "$dir/out.2")" ]; then
  echo "ok   two runs from one checkout at once both get the published app"
else
  echo "FAIL two runs at once: exit $rc1 and $rc2: $(cat "$dir/out.1" "$dir/out.2" | tr '\n' ' ')"
  fail=1
fi
exit $fail
