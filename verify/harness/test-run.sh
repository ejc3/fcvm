#!/bin/bash
# Tests of run.sh itself: it must stop as BLOCKED, and never report agreement or a passed
# self-test, when a step of the comparison could not be carried out.
#
# Each case runs run.sh with one tool replaced on PATH by a wrapper that fails in one place:
#   hostops fails    `lake exe hostops v6.14` prints its first ten rows and exits 1. run.sh must
#                    not compare the ten rows and report that v6.14 agrees.
#   diff fails       the self-test's `diff` exits 2 (an input error). run.sh must not take that
#                    for the difference the self-test has to find.
#   diff finds none  the self-test's `diff` exits 0. run.sh must say it cannot tell the two
#                    kernels apart.
#   count unreadable the `grep -c` that counts the self-test's differing operations prints
#                    nothing. run.sh must not pass the self-test on a count it could not read.
#   cc fails         the compiler exits 1. That is a step that could not be carried out (exit 2),
#                    not a kernel that disagrees with its rule book (exit 1).
#   count pinned     with one operation count in operations.expected changed, run.sh must fail:
#                    a run that compares fewer operations than the documents quote is not the
#                    run they describe.
# The self-test's diff is the fourth diff run.sh executes (one for each of the three kernels,
# then the self-test). The control case counts the calls, so a change to that order fails here.
#
# Every case runs run.sh in a build directory of its own (HARNESS_BUILD), seeded with the kernel
# source already fetched, so the cases do not disturb a run of run.sh going on beside them.
#
# usage: harness/test-run.sh       from verify/, after `lake build` (it runs run.sh eight times,
#                                  about 15 s)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
for tool in lake diff mktemp grep cc cp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
real_lake=$(command -v lake)
real_diff=$(command -v diff)
real_grep=$(command -v grep)
mkdir -p "$here/build"
tmp=$(mktemp -d "$here/build/test-run.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
export HARNESS_BUILD="$tmp/build"
mkdir "$HARNESS_BUILD"
# the kernel source another run already fetched and checked, so the cases need no network
[ ! -d "$here/build/src" ] || cp -r "$here/build/src" "$HARNESS_BUILD/src"

mkdir "$tmp/hostops-fails" "$tmp/diff-wrapped" "$tmp/grep-silent" "$tmp/cc-fails" "$tmp/expected"
cat > "$tmp/grep-silent/grep" <<WRAP
#!/bin/bash
if [ "\$1 \$2" = "-c ^<" ]; then exit 2; fi
exec "$real_grep" "\$@"
WRAP
cat > "$tmp/cc-fails/cc" <<WRAP
#!/bin/bash
echo "cc: stand-in compiler that fails" >&2
exit 1
WRAP
cat > "$tmp/hostops-fails/lake" <<WRAP
#!/bin/bash
if [ "\$1 \$2 \$3" = "exe hostops v6.14" ]; then
  "$real_lake" "\$@" | head -n 10
  exit 1
fi
exec "$real_lake" "\$@"
WRAP
cat > "$tmp/diff-wrapped/diff" <<WRAP
#!/bin/bash
n=\$(( \$(cat "$tmp/diff-calls" 2>/dev/null || echo 0) + 1 ))
echo "\$n" > "$tmp/diff-calls"
if [ "\$n" = "\${TEST_RUN_DIFF_CALL:-}" ]; then exit "\$TEST_RUN_DIFF_STATUS"; fi
exec "$real_diff" "\$@"
WRAP
chmod +x "$tmp/hostops-fails/lake" "$tmp/diff-wrapped/diff" "$tmp/grep-silent/grep" "$tmp/cc-fails/cc"

failures=0
# case NAME WANT_STATUS WANT_TEXT FORBIDDEN_TEXT -- command...
case_() {
  local name=$1 want=$2 text=$3 forbidden=$4 status=0
  shift 5
  rm -f "$tmp/diff-calls"
  "$@" > "$tmp/out" 2>&1 || status=$?
  if [ "$status" != "$want" ]; then
    echo "FAIL $name: run.sh exited $status, want $want"; failures=$((failures + 1))
  elif ! grep -q -- "$text" "$tmp/out"; then
    echo "FAIL $name: run.sh did not print: $text"; failures=$((failures + 1))
  elif [ -n "$forbidden" ] && grep -q -- "$forbidden" "$tmp/out"; then
    echo "FAIL $name: run.sh printed: $forbidden"; failures=$((failures + 1))
  else
    echo "ok   $name (exit $status)"; return 0
  fi
  sed 's/^/     | /' "$tmp/out"
}

case_ "control" 0 "self-test: v7.3.3's functions against the patched rule book differ in" "BLOCKED" -- \
  env PATH="$tmp/diff-wrapped:$PATH" "$here/run.sh"
calls=$(cat "$tmp/diff-calls" 2>/dev/null || echo 0)
if [ "$calls" != 4 ]; then
  echo "FAIL control: run.sh ran diff $calls times, and these cases take the fourth for the self-test"
  failures=$((failures + 1))
fi
case_ "hostops fails after ten rows" 2 "BLOCKED: lake exe hostops v6.14 failed" "v6.14: .* agree" -- \
  env PATH="$tmp/hostops-fails:$PATH" "$here/run.sh"
case_ "the self-test's diff exits 2" 2 "BLOCKED: diff failed" "as they must" -- \
  env PATH="$tmp/diff-wrapped:$PATH" TEST_RUN_DIFF_CALL=4 TEST_RUN_DIFF_STATUS=2 "$here/run.sh"
case_ "the self-test's diff exits 0" 2 "BLOCKED: the harness cannot tell v7.3.3 from v7.3.3 with the #2130 patch" "as they must" -- \
  env PATH="$tmp/diff-wrapped:$PATH" TEST_RUN_DIFF_CALL=4 TEST_RUN_DIFF_STATUS=0 "$here/run.sh"
case_ "a kernel's own diff exits 2" 2 "BLOCKED: diff failed" "v6.14: .* agree" -- \
  env PATH="$tmp/diff-wrapped:$PATH" TEST_RUN_DIFF_CALL=1 TEST_RUN_DIFF_STATUS=2 "$here/run.sh"
case_ "the count of differing operations cannot be read" 2 "BLOCKED: the self-test's count of differing operations" "as they must" -- \
  env PATH="$tmp/grep-silent:$PATH" "$here/run.sh"
case_ "the compiler fails" 2 "BLOCKED: harness/run.sh line" "agree with the model" -- \
  env PATH="$tmp/cc-fails:$PATH" "$here/run.sh"
sed 's/^v6\.14 .*/v6.14 7/' "$here/operations.expected" > "$tmp/expected/operations.expected"
case_ "an operation count differs from operations.expected" 1 "v6.14: .* operations, and operations.expected says 7" "" -- \
  env HARNESS_EXPECTED="$tmp/expected/operations.expected" "$here/run.sh"

if [ "$failures" != 0 ]; then
  echo "test-run.sh: $failures case(s) failed"
  exit 1
fi
echo "test-run.sh: every case passed"
