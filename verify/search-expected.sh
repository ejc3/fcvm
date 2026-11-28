#!/bin/bash
# Compares the exhaustive search with the result the documents quote, strictly both ways:
# `lake exe breaks --summary` against WakeVerify/search.expected, which holds, for every search,
# its number of states, each kind of break with its count and the length of its shortest run,
# and the audit's numbers. A kind that appears, a kind that goes, a count that moves, a search
# that is added or dropped: each is a difference, and a difference fails.
#
#   search-expected.sh quick     the 19 small searches (`breaks --quick`), against the blocks of
#                                the file marked "set: quick". About 2 min, 1.2 GB. Part
#                                of `make verify` and of the CI job.
#   search-expected.sh full      all 76 searches against the whole file. About 45 min of CPU and
#                                3.3 GB in one process; JOBS=7 runs one process a rule book (and
#                                one for the restore orders) side by side, about 8 min and
#                                3.3 GB each. `make verify-search`, and the dispatch job in CI.
#   search-expected.sh only PATTERN
#                                the searches whose title contains PATTERN, against the blocks
#                                of the file whose title contains it.
#   search-expected.sh update    run `full` and write the file. Read the diff before committing
#                                it, and update BREAKS.md with it.
#
# The file is WakeVerify/search.expected, or $SEARCH_EXPECTED (test-search.sh uses that to check
# that a changed file is a difference).
#
# Exit status: 0 the search agrees with the file; 1 it differs, or the search failed one of its
# own gates; 2 BLOCKED, the comparison could not be made.
set -Eeuo pipefail
trap 'echo "BLOCKED: search-expected.sh line $LINENO failed" >&2; exit 2' ERR

cd "$(dirname "$0")"
for tool in awk diff mktemp cat; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
breaks=.lake/build/bin/breaks
expected=${SEARCH_EXPECTED:-WakeVerify/search.expected}
[ -x "$breaks" ] || { echo "BLOCKED: $breaks is not built: run lake build first" >&2; exit 2; }
mode=${1:-}
pattern=${2:-}
case "$mode" in
  quick | full) [ -s "$expected" ] || { echo "BLOCKED: $expected is missing or empty" >&2; exit 2; } ;;
  only)
    [ -s "$expected" ] || { echo "BLOCKED: $expected is missing or empty" >&2; exit 2; }
    [ -n "$pattern" ] || { echo "usage: search-expected.sh only PATTERN" >&2; exit 2; } ;;
  update) ;;
  *) echo "usage: search-expected.sh quick | full | only PATTERN | update" >&2; exit 2 ;;
esac
# A panic in the search must end it, not go on with a default value.
export LEAN_ABORT_ON_PANIC=1
tmp=$(mktemp -d "$PWD/.lake/search-expected.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT

# Each part is one process; the parts' outputs in this order are the output of one process
# running everything. The names are the six rule books of `books` in Search.lean and the prefix
# of the restore orders: a part that matches no search exits 2, and a rule book that is in no
# part leaves its blocks out of the output, which is a difference.
parts=(
  "Today: v6.14, Firecracker without the restore kick,"
  "Today: v7.3.3, Firecracker without the restore kick,"
  "v6.14 with the #2131 restore kick,"
  "v7.3.3 with the #2131 restore kick,"
  "v7.3.3 with the #2130 patch, no restore kick,"
  "Design:"
  "Order:"
)
status=0
run_search() {   # run_search OUT ARGS...: status 0 or 1 is the search's verdict, anything else blocks
  local out=$1 rc=0
  shift
  "$breaks" --summary "$@" > "$out" || rc=$?
  [ "$rc" -le 1 ] || { echo "BLOCKED: breaks --summary $* exited $rc" >&2; return 2; }
  return "$rc"
}
if [ "$mode" = quick ]; then
  rc=0; run_search "$tmp/got" --quick || rc=$?
  [ "$rc" -le 1 ] || exit 2
  status=$rc
  # the blocks of the file that a quick run prints: paragraphs with the line "set: quick"
  awk 'BEGIN { RS = ""; ORS = "\n\n" } /\nset: quick\n/' "$expected" > "$tmp/want"
  [ -s "$tmp/want" ] || { echo "BLOCKED: $expected has no block marked 'set: quick'" >&2; exit 2; }
elif [ "$mode" = only ]; then
  rc=0; run_search "$tmp/got" "$pattern" || rc=$?
  [ "$rc" -le 1 ] || exit 2
  status=$rc
  # the blocks whose title line contains the pattern; none is an empty file, and a difference
  awk -v pat="$pattern" 'BEGIN { RS = ""; ORS = "\n\n" }
       { title = $0; sub(/\n.*/, "", title); if (index(title, pat)) print }' "$expected" > "$tmp/want"
else
  jobs=${JOBS:-1}
  case "$jobs" in '' | *[!0-9]* | 0) echo "BLOCKED: JOBS=$jobs is not a positive number" >&2; exit 2 ;; esac
  pids=()
  for i in "${!parts[@]}"; do
    ( rc=0; run_search "$tmp/part-$i" "${parts[$i]}" || rc=$?; echo "$rc" > "$tmp/rc-$i" ) &
    pids+=($!)
    if [ "${#pids[@]}" -ge "$jobs" ]; then wait "${pids[0]}" || true; pids=("${pids[@]:1}"); fi
  done
  wait || true
  : > "$tmp/got"
  for i in "${!parts[@]}"; do
    rc=$(cat "$tmp/rc-$i" 2>/dev/null || echo missing)
    case "$rc" in
      0) ;;
      1) status=1 ;;
      *) echo "BLOCKED: the search of '${parts[$i]}' did not finish (status $rc)" >&2; exit 2 ;;
    esac
    cat "$tmp/part-$i" >> "$tmp/got"
  done
  cp "$expected" "$tmp/want" 2>/dev/null || : > "$tmp/want"
fi

if [ "$mode" = update ]; then
  [ "$status" = 0 ] || { echo "search-expected.sh: the search failed a gate of its own; $expected is not written" >&2; grep -A20 '^FAILED:' "$tmp/got" >&2 || true; exit 1; }
  cp "$tmp/got" "$expected"
  echo "search-expected.sh: wrote $expected ($(grep -c '^## ' "$expected") searches)"
  exit 0
fi
rc=0
diff "$tmp/want" "$tmp/got" || rc=$?
[ "$rc" -le 1 ] || { echo "BLOCKED: diff failed (status $rc)" >&2; exit 2; }
searches=$(grep -c '^## ' "$tmp/got" || true)
if [ "$rc" = 1 ]; then
  echo "search-expected.sh $mode: the search differs from $expected ('<' expected, '>' found)" >&2
  exit 1
fi
if [ "$status" != 0 ]; then
  echo "search-expected.sh $mode: the search failed a gate of its own" >&2
  exit 1
fi
echo "search-expected.sh $mode: $searches searches agree with $expected"
