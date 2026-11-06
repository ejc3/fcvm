#!/bin/bash
# Builds Robot Park's APK in its build container (`make apk`), with the tools of the AOSP build
# (scripts/phone/aosp.sh) mounted at $AOSP_TOOLS: Gradle compiles the code against their
# android.jar, d8 dexes it with Kotlin's standard library, aapt2 links the manifest, resources
# and shaders against android.jar, then zipalign and apksigner. Nothing it runs or packages is
# from the Android SDK. Prints the APK's path.
#
# The signing key is GRADLE_USER_HOME's android/debug.keystore (made here on first use, with
# the debug key's usual names), so rebuilds on one host sign alike and install over each other.
set -euo pipefail
T=${AOSP_TOOLS:?AOSP_TOOLS names the tools directory of the AOSP build}
JAR=$T/android.jar
for f in "$JAR" "$T/bin/aapt2" "$T/bin/zipalign" "$T/framework/d8.jar" "$T/framework/apksigner.jar"; do
  [ -e "$f" ] || { echo "build-apk.sh: $f is missing (scripts/phone/aosp.sh fetches the tools)" >&2; exit 2; }
done
cd "$(dirname "$0")"
# one build at a time in this checkout: each deletes and writes app/build/apk
mkdir -p app/build
exec 9> app/build/.apk.lock
flock 9
gradle --console=plain -q -PandroidJar="$JAR" :app:dexInputs >&2
B=app/build/apk
rm -rf "$B" && mkdir -p "$B/dex"
java -cp "$T/framework/d8.jar" com.android.tools.r8.D8 --release --min-api 30 --lib "$JAR" --output "$B/dex" \
  app/build/dex-inputs/*.jar >&2
"$T/bin/aapt2" compile --dir app/src/main/res -o "$B/res.zip"
"$T/bin/aapt2" link -o "$B/base.apk" --manifest app/src/main/AndroidManifest.xml -I "$JAR" \
  -A app/src/main/assets "$B/res.zip"
(cd "$B/dex" && zip -q -X ../base.apk classes*.dex)
"$T/bin/zipalign" -f -p 4 "$B/base.apk" "$B/aligned.apk"
KS=${GRADLE_USER_HOME:-$HOME/.gradle}/android/debug.keystore
mkdir -p "$(dirname "$KS")"
# made once under a lock (checkouts share the Gradle home), in a directory of its own, and
# published by rename, so an interrupted or concurrent run leaves no half-made key behind
exec 8> "$KS.lock"
flock 8
if [ ! -f "$KS" ]; then
  kd=$(mktemp -d "$(dirname "$KS")/.keystore.XXXXXX")
  keytool -genkeypair -keystore "$kd/k" -storepass android -keypass android -alias androiddebugkey \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US" > /dev/null 2>&1
  mv "$kd/k" "$KS"
  rmdir "$kd"
fi
flock -u 8
java -cp "$T/framework/apksigner.jar" com.android.apksigner.ApkSignerTool sign --ks "$KS" --ks-pass pass:android \
  --ks-key-alias androiddebugkey --key-pass pass:android --out "$B/robotpark.apk" "$B/aligned.apk"
echo "$(pwd)/$B/robotpark.apk"
