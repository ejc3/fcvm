#!/usr/bin/env bash
# The confinement gate (make test-confine): runs a test lane with every VMM confined and its
# syscall allowlist enforcing, then checks what the lane's exit status does not show.
#
#   confine-gate.sh MIN_TESTS LANE_COMMAND...
#
# The lane runs with FCVM_CONFINE=required, FCVM_CONFINE_SECCOMP=enforce and
# NEXTEST_RETRIES=0: with a retry, a VMM the policy stopped on one try of two passes as flaky.
# tests/common writes three records for the gate while the lane runs, each to the file an
# environment variable names:
#   FCVM_CONFINE_LOG_LIST    test <TAB> path, for every per-test log a test opens;
#   FCVM_CONFINE_ASSERT_LOG  test <TAB> VMM pid <TAB> healthy|seen, each time a test read a
#                            VMM as confined from /proc (assert_vmm_confined_under_the_gate
#                            when a VM turns healthy; `seen` for a VMM read while its VM was
#                            still starting);
#   FCVM_CONFINE_API_LOG     ok|dead <TAB> test, for the API call kill_process makes on a
#                            VMM before it stops it.
# After the lane the gate fails when
#   1. the lane failed, or retried a test;
#   2. a VMM was stopped by the policy: fcvm's report or Firecracker's own line, in the
#      lane's output, in a per-test log the lane's tests listed, or in any other file
#      written to the log directory during the lane (a test that keeps a log of its own).
#      Every test that passed must have listed a log: the directory is shared, so a file
#      in it does not show that this lane wrote one;
#   3. the lane ran fewer than MIN_TESTS tests (a filter that stopped matching), or not all
#      of them passed;
#   4. a test that passed has no `healthy` record: it did not assert a VMM confined once
#      its VM was up. A test whose VM is meant to fail its boot needs a `seen` record and
#      its name in CONFINE_GATE_BOOT_FAILS_TESTS; a test that starts no VMM through the
#      helpers needs its name in CONFINE_GATE_NO_VMM_TESTS (both space separated);
#   5. a VMM's API stopped answering while its process ran on. A syscall trapped while a
#      Firecracker thread is writing a log line hangs that thread: no line is written and
#      the process does not exit, so check 2 cannot see it.
# What the records prove: that the assertion ran in that test against a VMM that was
# confined then. They do not prove a test asserted every VMM it started; the verdict line
# counts the VMMs asserted and the API calls answered.
# A check that cannot be made fails: no lane output, no summary line, a test with no log, a
# log that cannot be read, no API call recorded.
# CONFINE_GATE_LOG_DIR overrides the log directory (tests of this script).
set -uo pipefail

blocked() {
  echo "confine gate: BLOCKED: $*" >&2
  exit 2
}
for tool in awk comm cut date find grep mktemp rm sed sort tail tee touch tr wc; do
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
: > "$work/api"
: > "$work/logs"

export FCVM_CONFINE=required FCVM_CONFINE_SECCOMP=enforce NEXTEST_RETRIES=0
export FCVM_CONFINE_ASSERT_LOG="$work/asserted"
export FCVM_CONFINE_API_LOG="$work/api"
export FCVM_CONFINE_LOG_LIST="$work/logs"
"$@" 2>&1 | tee "$work/lane.log"
lane=${PIPESTATUS[0]}

failed=0
fail() {
  echo "confine gate: FAIL: $*" >&2
  failed=1
}
names() { tr ' ' '\n' <<< "$1" | grep -v '^$' | sort -u; }
sed 's/\x1b\[[0-9;]*m//g' "$work/lane.log" > "$work/plain.log"
[ -s "$work/plain.log" ] || blocked "the lane printed nothing"
[ "$lane" -eq 0 ] || fail "the lane exited $lane"

# 3. The lane ran the tests the gate is pinned to, once each, and all of them passed.
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
awk '$1 == "PASS" && $2 ~ /^\[/ { print $NF }' "$work/plain.log" | sort -u > "$work/passed"
[ "$(wc -l < "$work/passed")" -eq "$passed" ] || fail "the lane's PASS lines name $(wc -l < "$work/passed") tests and its summary says $passed passed"
names "${CONFINE_GATE_NO_VMM_TESTS:-}" > "$work/no-vmm"
names "${CONFINE_GATE_BOOT_FAILS_TESTS:-}" > "$work/boot-fails"
comm -23 "$work/passed" "$work/no-vmm" > "$work/with-vmm"

# 2. No VMM was stopped by the policy, in a test that passed as much as in one that failed.
cut -f1 "$work/logs" | sort -u > "$work/logged"
comm -23 "$work/with-vmm" "$work/logged" > "$work/unlogged"
if [ -s "$work/unlogged" ]; then
  fail "these tests passed and opened no per-test log, so a stopped VMM cannot be ruled out for them:"
  sed 's/^/  /' "$work/unlogged" >&2
fi
stop='stopped by its confinement policy|intercepting a bad syscall'
: > "$work/stops"
: > "$work/unreadable"
{
  cut -f2- "$work/logs"
  [ ! -d "$log_dir" ] || find "$log_dir" -type f -newer "$work/marker"
} | sort -u > "$work/scanned"
{
  echo "$work/plain.log"
  cat "$work/scanned"
} | while IFS= read -r file; do
  grep -aHnE -m 3 "$stop" "$file" >> "$work/stops" 2>/dev/null
  # 0: a stop line; 1: none; anything else: the file was not read.
  [ $? -le 1 ] || echo "$file" >> "$work/unreadable"
done
if [ -s "$work/unreadable" ]; then
  fail "these per-test logs could not be read, so a stopped VMM cannot be ruled out:"
  sed 's/^/  /' "$work/unreadable" >&2
fi
if [ -s "$work/stops" ]; then
  fail "a VMM was stopped by the confinement policy:"
  sed "s|^$work/plain.log:|lane output:|" "$work/stops" | cut -c1-400 >&2
fi

# 4. Every test that passed asserted a VMM confined once its VM was up.
awk -F'\t' '$3 == "healthy" { print $1 }' "$work/asserted" | sort -u > "$work/healthy"
awk -F'\t' '$3 == "seen" { print $1 }' "$work/asserted" | sort -u > "$work/seen"
comm -12 "$work/boot-fails" "$work/seen" | sort -u - "$work/healthy" > "$work/asserted-tests"
comm -23 "$work/with-vmm" "$work/asserted-tests" > "$work/unasserted"
if [ -s "$work/unasserted" ]; then
  fail "these tests passed without asserting their VMM confined (tests/common assert_vmm_confined_under_the_gate):"
  sed 's/^/  /' "$work/unasserted" >&2
fi
vmms=$(awk -F'\t' '$3 == "healthy" || $3 == "seen" { print $1 "\t" $2 }' "$work/asserted" | sort -u | wc -l)

# 5. No VMM was left hung: every API call made before a test stopped its VM was answered.
if grep -aq '^dead' "$work/api"; then
  fail "a VMM's API stopped answering while its process ran on (a syscall trapped inside Firecracker's logging does this):"
  grep -a '^dead' "$work/api" | cut -f2- | cut -c1-400 | sed 's/^/  /' >&2
fi
api_checks=$(grep -ac '^ok' "$work/api")
if [ "$api_checks" -eq 0 ] && ! grep -aq '^dead' "$work/api"; then
  fail "no VMM's API was called before its test stopped it (tests/common kill_process), so a hung VMM cannot be ruled out"
fi

if [ "$failed" -ne 0 ]; then
  exit 1
fi
echo "confine gate: CLEAR: $ran tests ran once and passed; $(wc -l < "$work/with-vmm") asserted a VMM confined ($vmms VMMs; $(wc -l < "$work/no-vmm") tests start none); no VMM was stopped by the policy in the lane's output or the $(wc -l < "$work/scanned") logs written during it; $api_checks VMMs answered their API before they were stopped"
