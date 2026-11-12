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
