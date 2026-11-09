#!/bin/bash
# `make image` stages the cached Gradle distribution into the build context (.gradle-dist). Two
# `make image` runs in one checkout at once must not change the file the other's build is
# reading: run A's build holds the staged archive open while run B stages and builds, and the
# archive A holds must come out unmodified (its mtime and size). A stand-in podman plays the build.
#   fleet/phone-app/test-gradle-dist.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir/src" "$dir/dist" "$dir/bin" "$dir/sync"
cp "$here/Makefile" "$here/Containerfile" "$dir/src/"
version=$(sed -n 's/^ARG GRADLE_VERSION=//p' "$here/Containerfile")
head -c 1048576 /dev/urandom > "$dir/dist/gradle-$version-bin.zip"
sha=$(sha256sum "$dir/dist/gradle-$version-bin.zip" | cut -d' ' -f1)
cat > "$dir/bin/podman" <<'S'
#!/bin/bash
# image exists: never; build: A holds the staged archive open until B has built, then reports
# whether the file it holds was modified meanwhile.
case "$1" in
  image) exit 1 ;;
  build)
    f=$(ls .gradle-dist/*.zip)
    exec 3< "$f"
    if [ "$ROLE" = A ]; then
      before=$(stat -L -c '%y %s' "/proc/$$/fd/3")
      : > "$SYNC/a-open"
      for _ in $(seq 1 200); do [ -e "$SYNC/b-built" ] && break; sleep 0.05; done
      [ -e "$SYNC/b-built" ] || { echo "B never built" > "$SYNC/a-result"; exit 1; }
      after=$(stat -L -c '%y %s' "/proc/$$/fd/3")
      [ "$before" = "$after" ] && echo unchanged > "$SYNC/a-result" || echo "changed: $before -> $after" > "$SYNC/a-result"
    else
      : > "$SYNC/b-built"
    fi ;;
esac
S
chmod +x "$dir/bin/podman"
mk() { make -s -C "$dir/src" image PODMAN="$dir/bin/podman" GRADLE_DIST="$dir/dist" GRADLE_SHA256="$sha"; }
ROLE=A SYNC="$dir/sync" mk > "$dir/out.A" 2>&1 &
pA=$!
for _ in $(seq 1 200); do [ -e "$dir/sync/a-open" ] && break; sleep 0.05; done
rc=0
[ -e "$dir/sync/a-open" ] || { echo "FAIL: run A's build never started"; cat "$dir/out.A"; exit 1; }
ROLE=B SYNC="$dir/sync" mk > "$dir/out.B" 2>&1 || { echo "FAIL: run B failed"; cat "$dir/out.B"; rc=1; }
wait "$pA" || { echo "FAIL: run A failed"; cat "$dir/out.A"; rc=1; }
result=$(cat "$dir/sync/a-result" 2>/dev/null)
[ "$result" = unchanged ] || { echo "FAIL: run B modified the archive run A's build was reading ($result)"; rc=1; }
[ "$(ls -A "$dir/src/.gradle-dist" | grep -vc '\.zip$')" = 0 ] || { echo "FAIL: staging left temporary files: $(ls -A "$dir/src/.gradle-dist")"; rc=1; }
[ $rc = 0 ] && echo "ok: concurrent make image runs leave each build's staged archive intact"
exit $rc
