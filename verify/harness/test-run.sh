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
# The self-test's diff is the fourth diff run.sh executes (one for each of the three kernels,
# then the self-test). The control case counts the calls, so a change to that order fails here.
#
# usage: harness/test-run.sh       from verify/, after `lake build` (it runs run.sh five times)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
for tool in lake diff mktemp grep; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
real_lake=$(command -v lake)
real_diff=$(command -v diff)
mkdir -p "$here/build"
tmp=$(mktemp -d "$here/build/test-run.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT

mkdir "$tmp/hostops-fails" "$tmp/diff-wrapped"
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
chmod +x "$tmp/hostops-fails/lake" "$tmp/diff-wrapped/diff"

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

if [ "$failures" != 0 ]; then
  echo "test-run.sh: $failures case(s) failed"
  exit 1
fi
echo "test-run.sh: every case passed"
