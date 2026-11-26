#!/bin/bash
# Tests of the search program's own gates (`lake exe breaks`, Search.lean): it must exit 1, and
# say why, when it could not establish what its output is quoted for.
#
#   state limit   a search that stops at the limit is not audited (its graph has successors for
#                 the expanded states only: the audit once indexed past them and printed "index
#                 out of bounds"), and the run fails, under the design and under any other rule
#                 book.
#   blind audit   in a copy of this directory `ownFlag` is made blind to a lost kick and a
#                 missing waker, so the audit finds dead vCPUs that no property flags. The run
#                 must fail on them. Unchanged, the same search has the same dead vCPUs, every
#                 one flagged, and exits 0.
#
# usage: ./test-search.sh          from verify/, after `lake build` (about 20 s)
set -euo pipefail

cd "$(dirname "$0")"
for tool in lake python3 mktemp grep cp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
breaks=.lake/build/bin/breaks
[ -x "$breaks" ] || { echo "BLOCKED: $breaks is not built: run lake build first" >&2; exit 2; }
tmp=$(mktemp -d "$PWD/.lake/test-search.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
book="Today: v7.3.3, Firecracker without the restore kick, no snapshot taken"

failures=0
# expect NAME WANT_STATUS FILE STATUS -- then lines of "+text" (must be there) or "-text" (must not)
expect() {
  local name=$1 want=$2 file=$3 status=$4 ok=1 line
  shift 4
  [ "$status" = "$want" ] || { echo "FAIL $name: exit $status, want $want"; ok=0; }
  for line in "$@"; do
    case "$line" in
      +*) grep -q -- "${line#+}" "$file" || { echo "FAIL $name: did not print: ${line#+}"; ok=0; } ;;
      -*) ! grep -q -- "${line#-}" "$file" || { echo "FAIL $name: printed: ${line#-}"; ok=0; } ;;
    esac
  done
  if [ "$ok" = 1 ]; then echo "ok   $name (exit $status)"; else failures=$((failures + 1)); tail -n 6 "$file" | cut -c1-200 | sed 's/^/     | /'; fi
}
run() { local out=$1 status=0; shift; "$@" > "$out" 2>&1 || status=$?; echo "$status"; }

s=$(run "$tmp/limit-design" "$breaks" 1000 --quiet "Design:")
expect "state limit, the design" 1 "$tmp/limit-design" "$s" \
  "+LIMIT REACHED: NOT EXHAUSTIVE" "+audit: not run, the search did not finish" \
  "+fcvm configuration: the search did not finish, so nothing is established" \
  "-index out of bounds" "-dead vCPUs that no property flags" "-^audit: [0-9]"
s=$(run "$tmp/limit-today" "$breaks" 1000 --quiet "$book")
expect "state limit, another rule book" 1 "$tmp/limit-today" "$s" \
  "+audit: not run, the search did not finish" \
  "+no snapshot taken: the search did not finish, so nothing is established" "-index out of bounds"

s=$(run "$tmp/control" "$breaks" 20000000 --quiet "$book")
expect "audit, unchanged" 0 "$tmp/control" "$s" \
  "+^audit: [1-9][0-9]* states in which a vCPU can never run again" "+, 0 have neither" "-FAILED"

mkdir "$tmp/blind" "$tmp/blind/.lake"
cp -r WakeVerify WakeVerify.lean Search.lean HostOps.lean lakefile.toml lake-manifest.json lean-toolchain "$tmp/blind/"
for extra in GateVerify GateVerify.lean GateSearch.lean GateSeq.lean; do
  [ ! -e "$extra" ] || cp -r "$extra" "$tmp/blind/"
done
cp -r .lake/build "$tmp/blind/.lake/build"
python3 - "$tmp/blind/Search.lean" <<'PY' || { echo "BLOCKED: the mutation of ownFlag did not apply" >&2; exit 2; }
import sys
path = sys.argv[1]
text = open(path).read()
old = "  s.lostKick i || s.orphan i || s.orphanHalt i"
assert text.count(old) == 1
open(path, "w").write(text.replace(old, "  s.orphanHalt i"))
PY
(cd "$tmp/blind" && lake build breaks) > "$tmp/blind-build" 2>&1 ||
  { echo "BLOCKED: the copy with a blind ownFlag did not build" >&2; tail -n 5 "$tmp/blind-build" >&2; exit 2; }
s=$(run "$tmp/blind-run" "$tmp/blind/$breaks" 20000000 --quiet "$book")
expect "audit, ownFlag blind to a lost kick and a missing waker" 1 "$tmp/blind-run" "$s" \
  "+^FAILED:" "+no snapshot taken: [1-9][0-9]* dead vCPUs that no property flags"

if [ "$failures" != 0 ]; then
  echo "test-search.sh: $failures case(s) failed"
  exit 1
fi
echo "test-search.sh: every case passed"
