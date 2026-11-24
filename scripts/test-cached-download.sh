#!/bin/bash
# scripts/cached-download.sh against file:// sources: a cold cache downloads and publishes the
# file, a warm one serves it after the source is gone (no request), a checksum mismatch fails
# and publishes nothing, and two cold runs at once both end with the right bytes.
#   bash scripts/test-cached-download.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dl="$here/cached-download.sh"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
rc=0
fail() { echo "FAIL: $*"; rc=1; }
mkdir -p "$dir/src"
head -c 300000 /dev/urandom > "$dir/src/a.tgz"
sha=$(sha256sum "$dir/src/a.tgz" | cut -d' ' -f1)
url="file://$dir/src/a.tgz"

out=$("$dl" "$url" "$sha" "$dir/out1" "$dir/cache" 2>&1) || fail "cold run failed: $out"
grep -q 'downloading' <<<"$out" || fail "cold run did not download: $out"
cmp -s "$dir/src/a.tgz" "$dir/out1" || fail "cold run wrote the wrong bytes"
[ -f "$dir/cache/$sha-a.tgz" ] || fail "cold run published nothing to the cache"

mv "$dir/src/a.tgz" "$dir/src/gone.tgz"
out=$("$dl" "$url" "$sha" "$dir/out2" "$dir/cache" 2>&1) || fail "warm run needed the source: $out"
grep -q 'no download' <<<"$out" || fail "warm run did not report a cache hit: $out"
cmp -s "$dir/src/gone.tgz" "$dir/out2" || fail "warm run wrote the wrong bytes"

bad=$(printf '0%.0s' $(seq 64))
mv "$dir/src/gone.tgz" "$dir/src/a.tgz"
# Nothing on stdout, cold or warm: callers capture their stdout (make apk's APK path).
stdout=$("$dl" "$url" "$sha" "$dir/out4" "$dir/cache-quiet" 2>/dev/null) || fail "quiet cold run failed"
[ -z "$stdout" ] || fail "a cold run wrote to stdout: $stdout"
stdout=$("$dl" "$url" "$sha" "$dir/out5" "$dir/cache-quiet" 2>/dev/null) || fail "quiet warm run failed"
[ -z "$stdout" ] || fail "a warm run wrote to stdout: $stdout"

if "$dl" "$url" "$bad" "$dir/out3" "$dir/cache2" > "$dir/bad.log" 2>&1; then fail "a checksum mismatch succeeded"; fi
[ -z "$(ls -A "$dir/cache2" 2>/dev/null)" ] || fail "a checksum mismatch left files: $(ls -A "$dir/cache2")"
[ ! -e "$dir/out3" ] || fail "a checksum mismatch wrote OUT"

head -c 300000 /dev/urandom > "$dir/src/b.tgz"
shb=$(sha256sum "$dir/src/b.tgz" | cut -d' ' -f1)
for i in 1 2 3 4; do "$dl" "file://$dir/src/b.tgz" "$shb" "$dir/par$i" "$dir/cache3" > "$dir/par$i.log" 2>&1 & done
wait
for i in 1 2 3 4; do cmp -s "$dir/src/b.tgz" "$dir/par$i" || fail "concurrent run $i wrote the wrong bytes: $(cat "$dir/par$i.log")"; done
[ "$(ls -A "$dir/cache3")" = "$shb-b.tgz" ] || fail "concurrent runs left: $(ls -A "$dir/cache3")"

# A run that replaces the cache entry while another copies it: cp stats its source and then opens
# it, and coreutils skips a source that changed in between. The shim renames a fresh copy of the
# entry over it at the moment cp opens it, which is the interleaving a concurrent run's publish
# produces. First prove the shim does that to a plain cp of the entry, then that the script's
# output survives it.
if command -v cc >/dev/null; then
  mkdir -p "$dir/shim" "$dir/race-cache"
  cat > "$dir/shim/shim.c" <<'C'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int openat(int dirfd, const char *path, int flags, ...) {
  static int (*real)(int, const char *, int, ...);
  if (!real) real = dlsym(RTLD_NEXT, "openat");
  const char *entry = getenv("SHIM_ENTRY"), *fresh = getenv("SHIM_FRESH");
  if (entry && fresh && strcmp(path, entry) == 0 && access(fresh, F_OK) == 0) rename(fresh, entry);
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap); }
  return real(dirfd, path, flags, mode);
}
int open(const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap); }
  return openat(AT_FDCWD, path, flags, mode);
}
int open64(const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap); }
  return openat(AT_FDCWD, path, flags, mode);
}
int openat64(int dirfd, const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap); }
  return openat(dirfd, path, flags, mode);
}
C
  if cc -shared -fPIC -o "$dir/shim/shim.so" "$dir/shim/shim.c" -ldl 2>/dev/null; then
    printf '#!/bin/sh\nLD_PRELOAD=%s exec /bin/cp "$@"\n' "$dir/shim/shim.so" > "$dir/shim/cp"
    chmod +x "$dir/shim/cp"
    shr="$dir/src/c.tgz"; head -c 300000 /dev/urandom > "$shr"
    shc=$(sha256sum "$shr" | cut -d' ' -f1)
    entry="$dir/race-cache/$shc-c.tgz"
    "$dl" "file://$shr" "$shc" "$dir/race0" "$dir/race-cache" >/dev/null 2>&1 || fail "race setup run failed"
    cp "$entry" "$dir/race-cache/.fresh"
    if SHIM_ENTRY=$entry SHIM_FRESH=$dir/race-cache/.fresh "$dir/shim/cp" "$entry" "$dir/race-probe" 2>"$dir/probe.log" \
        && ! grep -q 'replaced while being copied' "$dir/probe.log"; then
      fail "the replace shim did not interrupt a plain cp of the entry"
    fi
    cp "$entry" "$dir/race-cache/.fresh"
    SHIM_ENTRY=$entry SHIM_FRESH=$dir/race-cache/.fresh PATH="$dir/shim:$PATH" \
      "$dl" "file://$shr" "$shc" "$dir/race1" "$dir/race-cache" > "$dir/race1.log" 2>&1 \
      || fail "a run whose entry was replaced mid-copy failed: $(cat "$dir/race1.log")"
    cmp -s "$shr" "$dir/race1" || fail "a run whose entry was replaced mid-copy wrote the wrong bytes: $(cat "$dir/race1.log")"
  else
    fail "cc cannot build the replace shim"
  fi
else
  fail "no C compiler for the replace shim"
fi

# Every GitHub release URL in a workflow goes through cached-download.sh (it is its argument,
# on the same line or within the two lines before): a step that curls one directly fails the
# job whenever GitHub answers 5xx. WORKFLOWS points the check at another tree's workflows.
workflows=${WORKFLOWS:-$here/../.github/workflows}
direct=$(python3 - "$workflows" <<'PY'
import pathlib, sys
for path in sorted(pathlib.Path(sys.argv[1]).glob("*.yml")):
    lines = path.read_text().splitlines()
    for i, line in enumerate(lines):
        if "releases/download/" in line and not any(
                "cached-download.sh" in prev for prev in lines[max(0, i - 2):i + 1]):
            print(f"{path.name}:{i + 1}: {line.strip()}")
PY
)
[ -z "$direct" ] || fail "workflow steps download GitHub release files without cached-download.sh:
$direct"

# A cache that holds cached-download.sh's files is keyed on the script too: the file names in
# it are the script's layout, and a key that outlives a layout change restores files the script
# no longer recognises, downloads again, and, being an exact hit, never saves the new copy.
stale=$(python3 - "$workflows" <<'PY'
import pathlib, re, sys
for path in sorted(pathlib.Path(sys.argv[1]).glob("*.yml")):
    lines = path.read_text().splitlines()
    for i, line in enumerate(lines):
        if not re.search(r"uses: actions/cache(/restore|/save)?@", line):
            continue
        step_indent = len(line) - len(line.lstrip(" -"))
        fields = {}
        for nxt in lines[i + 1:]:
            if nxt.strip().startswith("- ") and len(nxt) - len(nxt.lstrip(" ")) < step_indent:
                break
            m = re.match(r"\s+(path|key):\s*(.*)", nxt)
            if m:
                fields.setdefault(m.group(1), m.group(2))
        held = fields.get("path", "")
        if re.search(r"/(downloads|gradle-dist)$", held) and \
                "scripts/cached-download.sh" not in fields.get("key", ""):
            print(f"{path.name}:{i + 1}: path {held} key {fields.get('key')}")
PY
)
[ -z "$stale" ] || fail "caches of cached-download.sh files are not keyed on the script:
$stale"

[ $rc = 0 ] && echo "ok: cached-download serves warm copies without the source and publishes only verified files"
exit $rc
