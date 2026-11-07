#!/bin/bash
# fleet/phone-app/build-apk.sh with stand-ins for Gradle and the AOSP tools: two builds in one
# checkout at once each print an APK that is their own, so neither run deletes or replaces
# what the other copies (scripts/phone/app.sh, or a second `make apk`).
#   fleet/phone-app/test-build-apk.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir/src/app/src/main/res" "$dir/src/app/src/main/assets" "$dir/tools/bin" "$dir/tools/framework" "$dir/bin" "$dir/home"
cp "$here/build-apk.sh" "$dir/src/"
: > "$dir/src/app/src/main/AndroidManifest.xml"
: > "$dir/tools/android.jar"; : > "$dir/tools/framework/d8.jar"; : > "$dir/tools/framework/apksigner.jar"
# each stand-in writes the run's name into what it makes, after a pause that lets the runs overlap
cat > "$dir/bin/gradle" <<'S'
#!/bin/bash
mkdir -p app/build/dex-inputs && echo "$RUN" > app/build/dex-inputs/a.jar; sleep 0.3
S
cat > "$dir/bin/java" <<'S'
#!/bin/bash
out=""; prev=""
for a in "$@"; do [ "$prev" = --output ] || [ "$prev" = --out ] && out=$a; prev=$a; done
case "$*" in
  *D8*) echo "$RUN" > "$out/classes.dex" ;;
  *ApkSignerTool*) sleep 0.5; echo "$RUN" > "$out" ;;
esac
S
cat > "$dir/tools/bin/aapt2" <<'S'
#!/bin/bash
prev=""; for a in "$@"; do [ "$prev" = -o ] && echo "$RUN" > "$a"; prev=$a; done
S
printf '#!/bin/bash\ncp "${@: -2:1}" "${@: -1}"\n' > "$dir/tools/bin/zipalign"
printf '#!/bin/bash\ntrue\n' > "$dir/bin/zip"
printf '#!/bin/bash\nprev=""; for a in "$@"; do [ "$prev" = -keystore ] && : > "$a"; prev=$a; done\n' > "$dir/bin/keytool"
chmod +x "$dir/bin/"* "$dir/tools/bin/"* "$dir/src/build-apk.sh"
for r in A B; do
  env PATH="$dir/bin:$PATH" RUN=$r AOSP_TOOLS="$dir/tools" GRADLE_USER_HOME="$dir/home" "$dir/src/build-apk.sh" > "$dir/out.$r" 2> "$dir/err.$r" &
  eval "p$r=\$!"
done
rc=0
wait "$pA" || rc=1
wait "$pB" || rc=1
a=$(cat "$dir/out.A"); b=$(cat "$dir/out.B")
if [ "$rc" = 0 ] && [ "$a" != "$b" ] && [ "$(cat "$a" 2> /dev/null)" = A ] && [ "$(cat "$b" 2> /dev/null)" = B ]; then
  echo "ok   two builds at once each print an APK of their own"
  exit 0
fi
echo "FAIL two builds at once: exit $rc; A printed '$a' holding '$(cat "$a" 2> /dev/null)', B printed '$b' holding '$(cat "$b" 2> /dev/null)'; $(cat "$dir/err.A" "$dir/err.B" | tail -3 | tr '\n' ' ')"
exit 1
