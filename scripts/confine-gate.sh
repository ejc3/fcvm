#!/usr/bin/env bash
# The confinement gate (make test-confine): runs a test lane with every VMM confined and its
# syscall allowlist enforcing, then checks what the lane's exit status does not show.
#
#   confine-gate.sh MIN_TESTS LANE_COMMAND...
#
# The lane runs with FCVM_CONFINE=required, FCVM_CONFINE_SECCOMP=enforce and
# NEXTEST_RETRIES=0: with a retry, a VMM the policy stopped on one try of two passes as flaky.
# After it, the gate fails when
#   - the lane failed, or retried a test;
#   - any VMM was stopped by the policy: fcvm's report or Firecracker's own line, in the
#     lane's output or in a per-test log written during the lane;
#   - the lane ran fewer than MIN_TESTS tests (a filter that stopped matching), or not all
#     of them passed;
#   - a test that passed did not assert its VMM confined: tests/common's helpers append each
#     test's name to FCVM_CONFINE_ASSERT_LOG when the assertion holds. Tests named in
#     CONFINE_GATE_NO_VMM_TESTS (space separated) start no VMM through those helpers.
# A check that cannot be made blocks: no lane output, no summary line, no per-test logs.
# CONFINE_GATE_LOG_DIR overrides where per-test logs are read from (tests of this script).
set -uo pipefail

blocked() {
  echo "confine gate: BLOCKED: $*" >&2
  exit 2
}
for tool in awk comm date find grep mktemp sed sort tee touch; do
  command -v "$tool" >/dev/null 2>&1 || blocked "'$tool' is missing"
done
[ $# -ge 2 ] || blocked "usage: confine-gate.sh MIN_TESTS LANE_COMMAND..."
min=$1
shift
case "$min" in
  '' | *[!0-9]*) blocked "MIN_TESTS must be a number, got '$min'" ;;
esac
log_dir=${CONFINE_GATE_LOG_DIR:-/tmp/fcvm-test-logs}
work=$(mktemp -d) || blocked "mktemp failed"
trap 'rm -rf "$work"' EXIT
# One second back: find -newer is strict, and a log written in the lane's first instant has
# the marker's own timestamp.
touch -d "@$(($(date +%s) - 1))" "$work/marker" || blocked "cannot write $work"
: > "$work/asserted"

export FCVM_CONFINE=required FCVM_CONFINE_SECCOMP=enforce NEXTEST_RETRIES=0
export FCVM_CONFINE_ASSERT_LOG="$work/asserted"
"$@" 2>&1 | tee "$work/lane.log"
lane=${PIPESTATUS[0]}

failed=0
fail() {
  echo "confine gate: FAIL: $*" >&2
  failed=1
}
sed 's/\x1b\[[0-9;]*m//g' "$work/lane.log" > "$work/plain.log"
[ -s "$work/plain.log" ] || blocked "the lane printed nothing"
[ "$lane" -eq 0 ] || fail "the lane exited $lane"

# 1. No VMM was stopped by the policy, in a test that passed as much as in one that failed.
find "$log_dir" -type f -newer "$work/marker" 2>/dev/null > "$work/logs"
if [ ! -s "$work/logs" ]; then
  fail "no per-test log was written under $log_dir during the lane, so a stopped VMM cannot be ruled out"
fi
stop='stopped by its confinement policy|intercepting a bad syscall'
{
  echo "$work/plain.log"
  cat "$work/logs"
} | while IFS= read -r file; do
  grep -aHnE -m 3 "$stop" "$file" 2>/dev/null
done > "$work/stops"
if [ -s "$work/stops" ]; then
  fail "a VMM was stopped by the confinement policy:"
  sed "s|^$work/plain.log:|lane output:|" "$work/stops" | cut -c1-400 >&2
fi

# 2. The lane ran the tests the gate is pinned to, once each, and all of them passed.
summary=$(grep -aE 'Summary \[.*\] +[0-9]+ tests? run: +[0-9]+ passed' "$work/plain.log" | tail -1)
if [ -z "$summary" ]; then
  fail "the lane printed no nextest summary line, so its tests cannot be counted"
  ran=0
  passed=0
else
  ran=$(sed -E 's/.*\] +([0-9]+) tests? run:.*/\1/' <<< "$summary")
  passed=$(sed -E 's/.*run: +([0-9]+) passed.*/\1/' <<< "$summary")
  [ "$ran" -ge "$min" ] || fail "the lane ran $ran tests and the gate is pinned to at least $min: its filter matches fewer tests than it did"
  [ "$passed" -eq "$ran" ] || fail "$passed of $ran tests passed"
fi
if grep -aqE '^ +(FLAKY|TRY [0-9]+) ' "$work/plain.log"; then
  fail "a test was retried; the gate runs each test once"
fi

# 3. Every test that passed asserted its VMM confined.
awk '$1 == "PASS" && $2 ~ /^\[/ { print $NF }' "$work/plain.log" | sort -u > "$work/passed"
[ "$(wc -l < "$work/passed")" -eq "$passed" ] || fail "the lane's PASS lines name $(wc -l < "$work/passed") tests and its summary says $passed passed"
sort -u "$work/asserted" > "$work/asserted.sorted"
tr ' ' '\n' <<< "${CONFINE_GATE_NO_VMM_TESTS:-}" | grep -v '^$' | sort -u > "$work/no-vmm"
comm -23 "$work/passed" "$work/asserted.sorted" | comm -23 - "$work/no-vmm" > "$work/unasserted"
if [ -s "$work/unasserted" ]; then
  fail "these tests passed without asserting their VMM confined (tests/common assert_vmm_confined_under_the_gate):"
  sed 's/^/  /' "$work/unasserted" >&2
fi

if [ "$failed" -ne 0 ]; then
  exit 1
fi
echo "confine gate: CLEAR: $ran tests ran once and passed, each asserted its VMM confined ($(wc -l < "$work/no-vmm") start none), and no VMM was stopped by the policy"
