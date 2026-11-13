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
cp "$here/../../fleet/phone-app/.gitignore" "$dir/fleet/phone-app/.gitignore"
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

# A build leaves its outputs in the app's directory, and a cold one first stages the Gradle
# archive there (make apk's image prerequisite, fleet/phone-app/Makefile). build.sh builds the
# app, then asks for its path: the path must be the app it built (ejc3/aws#823, where it named
# an app nobody built and the warm phone's adb install failed).
cold=$dir/cold
mkdir -p "$cold/scripts/phone" "$cold/fleet/phone-app/app" "$cold/bin"
cp "$here/app.sh" "$cold/scripts/phone/"
cp "$here/../../fleet/phone-app/.gitignore" "$cold/fleet/phone-app/.gitignore"
cp "$dir/scripts/phone/aosp.sh" "$cold/scripts/phone/"
echo cold source > "$cold/fleet/phone-app/app/a.kt"
cat > "$cold/bin/make" <<'STUB'
#!/bin/bash
for a in "$@"; do case "$a" in -C) next=1 ;; *) [ -n "${next:-}" ] && { src=$a; next=""; } ;; esac; done
cd "$src"
mkdir -p .gradle-dist && echo zip > .gradle-dist/gradle-bin.zip
mkdir -p app/build/apk && echo apk > app/build/apk/robotpark.apk
echo "$src/app/build/apk/robotpark.apk"
STUB
chmod +x "$cold/bin/make"
before=$("$cold/scripts/phone/app.sh" --apk-path)
built=$(PATH="$cold/bin:$PATH" "$cold/scripts/phone/app.sh")
after=$("$cold/scripts/phone/app.sh" --apk-path)
if [ "$built" = "$before" ] && [ "$after" = "$built" ] && [ -f "$after" ]; then
  echo "ok   the path asked for after a cold build is the app it built"
else
  echo "FAIL a cold build moved the app's path: before $before, built $built, after $after"
  fail=1
fi
# Every output .gitignore names, at the top or deeper, leaves the path alone; a source does not.
while IFS= read -r pattern; do
  case "$pattern" in '' | '#'*) continue ;; esac
  name=${pattern%/}; name=${name//\*/x}
  for at in "" "app/"; do
    if [ "$pattern" != "$name" ]; then f=$cold/fleet/phone-app/$at$name/output; else f=$cold/fleet/phone-app/$at$name; fi
    base=$("$cold/scripts/phone/app.sh" --apk-path)
    mkdir -p "$(dirname "$f")" && echo output > "$f"
    now=$("$cold/scripts/phone/app.sh" --apk-path)
    rm -f "$f"
    if [ "$now" = "$base" ]; then echo "ok   an ignored $at$pattern leaves the path alone"
    else echo "FAIL an ignored $at$pattern moved the path to $now"; fail=1; fi
  done
done < "$here/../../fleet/phone-app/.gitignore"
base=$("$cold/scripts/phone/app.sh" --apk-path)
echo changed > "$cold/fleet/phone-app/app/a.kt"
if [ "$("$cold/scripts/phone/app.sh" --apk-path)" != "$base" ]; then echo "ok   a changed source moves the path"
else echo "FAIL a changed source left the path alone"; fail=1; fi
exit $fail
