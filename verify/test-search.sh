#!/bin/bash
# Tests of the search program's own gates (`lake exe breaks`, Search.lean): it must exit
# non-zero, and say why, when it did not establish what its output is quoted for.
#
#   state limit   a search that stops at the limit is not audited (its graph has successors for
#                 the expanded states only: the audit once indexed past them and printed "index
#                 out of bounds"), does not print "no reachable state breaks a property", and
#                 fails the run, under the design and under any other rule book.
#   arguments     a pattern that matches no search, an unknown option, and a first argument that
#                 is neither a number nor a pattern of some search each end the program with
#                 exit status 2 before anything is searched. Each once exited 0 having searched
#                 nothing.
#   blind audit   in a copy of this directory `ownFlag` is made blind to a lost kick and a
#                 missing waker, so the audit finds dead vCPUs that no property flags. The run
#                 must fail on them. Unchanged, the same search has the same dead vCPUs, every
#                 one flagged, and exits 0.
#   expected      search-expected.sh compares strictly both ways. With one search's block in a
#                 copy of search.expected changed four ways (a kind removed, so the search finds
#                 one the file does not have; a kind added; a count changed; the block dropped),
#                 each is a difference and exits 1; the file as it is agrees and exits 0.
#   panic         in a copy, `main` reads past the end of an array before it searches. By
#                 default a compiled Lean program prints a message for that and goes on with a
#                 default value, and the run exits 0. With LEAN_ABORT_ON_PANIC=1, which this
#                 script, search-expected.sh and `make verify-search` set, the process aborts.
#
# usage: ./test-search.sh          from verify/, after `lake build` (about 30 s)
set -euo pipefail

cd "$(dirname "$0")"
for tool in lake python3 mktemp grep cp tail cut sed awk cmp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
breaks=.lake/build/bin/breaks
[ -x "$breaks" ] || { echo "BLOCKED: $breaks is not built: run lake build first" >&2; exit 2; }
export LEAN_ABORT_ON_PANIC=1
tmp=$(mktemp -d "$PWD/.lake/test-search.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
book="Today: v7.3.3, Firecracker without the restore kick, no snapshot taken"

failures=0
# expect NAME WANT_STATUS FILE STATUS, then "+text" (must be printed) or "-text" (must not be).
# WANT_STATUS is a number, or "abort" for a process ended by a signal (128 or more).
expect() {
  local name=$1 want=$2 file=$3 status=$4 ok=1 line
  shift 4
  if [ "$want" = abort ]; then
    [ "$status" -ge 128 ] || { echo "FAIL $name: exit $status, want the process aborted"; ok=0; }
  else
    [ "$status" = "$want" ] || { echo "FAIL $name: exit $status, want $want"; ok=0; }
  fi
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
  "-index out of bounds" "-dead vCPUs that no property flags" "-^audit: [0-9]" \
  "-no reachable state breaks a property"
s=$(run "$tmp/limit-today" "$breaks" 1000 --quiet "$book")
expect "state limit, another rule book" 1 "$tmp/limit-today" "$s" \
  "+audit: not run, the search did not finish" \
  "+no snapshot taken: the search did not finish, so nothing is established" "-index out of bounds"

s=$(run "$tmp/no-match" "$breaks" 1000 --quiet "no search has this in its title")
expect "a pattern that matches no search" 2 "$tmp/no-match" "$s" "+no search matches" "-reachable states"
s=$(run "$tmp/one-no-match" "$breaks" 1000 --quiet "Design:" "no search has this in its title")
expect "one pattern of two matches no search" 2 "$tmp/one-no-match" "$s" "+no search matches" "-states visited"
s=$(run "$tmp/not-a-number" "$breaks" 1000x --quiet)
expect "a first argument that is not a number" 2 "$tmp/not-a-number" "$s" "+no search matches" "-reachable states"
s=$(run "$tmp/option" "$breaks" 1000 --quite "Design:")
expect "an unknown option" 2 "$tmp/option" "$s" "+unknown option --quite" "-reachable states"

s=$(run "$tmp/control" "$breaks" --quiet "$book")
expect "audit, unchanged" 0 "$tmp/control" "$s" \
  "+^audit: [1-9][0-9]* states in which a vCPU can never run again" "+, 0 have neither" "-FAILED"

# search-expected.sh on the smallest search whose block has a kind, with the file changed
small="Order: v7.3.3 with the #2130 patch, KVM_SET_MSRS BEFORE KVM_SET_LAPIC"
s=$(run "$tmp/expected-control" ./search-expected.sh only "$small")
expect "expected, the file as it is" 0 "$tmp/expected-control" "$s" "+1 searches agree"
# change_expected NAME AWK-PROGRAM: a copy of the file with that search's block changed
change_expected() {
  awk -v pat="$small" 'BEGIN { RS = ""; ORS = "\n\n" }
    { title = $0; sub(/\n.*/, "", title); hit = index(title, pat) } '"$2"' { print }' \
    WakeVerify/search.expected > "$tmp/$1.expected"
  ! cmp -s WakeVerify/search.expected "$tmp/$1.expected" ||
    { echo "BLOCKED: the change '$1' to search.expected did not apply" >&2; exit 2; }
}
change_expected kind-removed 'hit { sub(/\nkind: [^\n]*/, "") }'
change_expected kind-added 'hit { sub(/\naudit: /, "\nkind: NoHang: a kind the search does not find; 1 states; shortest run 9 steps\naudit: ") }'
change_expected count-changed 'hit { sub(/; [0-9]+ states; shortest run/, "; 1 states; shortest run") }'
change_expected block-dropped 'hit { next }'
for change in kind-removed kind-added count-changed block-dropped; do
  s=$(run "$tmp/expected-$change" env SEARCH_EXPECTED="$tmp/$change.expected" ./search-expected.sh only "$small")
  expect "expected, $change" 1 "$tmp/expected-$change" "$s" "+the search differs from"
done

# copy NAME: this directory's sources and build products under $tmp/NAME
copy() {
  mkdir "$tmp/$1" "$tmp/$1/.lake"
  local f
  for f in *.lean */ lakefile.toml lake-manifest.json lean-toolchain; do
    case "$f" in harness/) ;; *) cp -r "$f" "$tmp/$1/" ;; esac
  done
  cp -r .lake/build "$tmp/$1/.lake/build"
}
# mutate NAME OLD NEW: replace the one occurrence of OLD in the copy's Search.lean and build it
mutate() {
  python3 - "$tmp/$1/Search.lean" "$2" "$3" <<'PY' || { echo "BLOCKED: the mutation for '$1' did not apply" >&2; exit 2; }
import sys
path, old, new = sys.argv[1:4]
text = open(path).read()
assert text.count(old) == 1, (old, text.count(old))
open(path, "w").write(text.replace(old, new))
PY
  (cd "$tmp/$1" && lake build breaks) > "$tmp/$1-build" 2>&1 ||
    { echo "BLOCKED: the copy '$1' did not build" >&2; tail -n 5 "$tmp/$1-build" >&2; exit 2; }
}

copy blind
mutate blind "  s.lostKick i || s.orphan i || s.orphanHalt i" "  s.orphanHalt i"
s=$(run "$tmp/blind-run" "$tmp/blind/$breaks" --quiet "$book")
expect "audit, ownFlag blind to a lost kick and a missing waker" 1 "$tmp/blind-run" "$s" \
  "+^FAILED:" "+no snapshot taken: [1-9][0-9]* dead vCPUs that no property flags"

copy panic
mutate panic '  if labels.size > 64 then' '  IO.println s!"{(#[0] : Array Nat)[args.length + 1]!}"
  if labels.size > 64 then'
s=$(run "$tmp/panic-abort" "$tmp/panic/$breaks" --quiet "$small")
expect "a panic, with LEAN_ABORT_ON_PANIC=1" abort "$tmp/panic-abort" "$s" "-reachable states"
s=$(run "$tmp/panic-default" env -u LEAN_ABORT_ON_PANIC "$tmp/panic/$breaks" --quiet "$small")
expect "the same panic without it (why the variable is set)" 0 "$tmp/panic-default" "$s" \
  "+index out of bounds" "+reachable states"

if [ "$failures" != 0 ]; then
  echo "test-search.sh: $failures case(s) failed"
  exit 1
fi
echo "test-search.sh: every case passed"
