#!/bin/bash
# Canonical nextest log scanner. Use this instead of hand-rolled greps.
#
# Every field here exists because a hand-rolled grep got it wrong and turned a
# real failure into a green report. See tests/test_log_scan.rs for the fixture
# that pins each one; those tests go RED if these patterns regress.
#
#   try_fail   nextest writes retries as "TRY 1 FAIL", so a `grep '^ *FAIL'`
#              matches NONE of them. A test that failed and passed on retry
#              then vanishes into a green total.
#   pass       counting `grep -c 'PASS'` also matches test-internal "PASSED:"
#              lines the test bodies print themselves. Measured once at 136 vs
#              nextest's real 119. Anchor on the verdict form "PASS [".
#   summary    the run's own summary line is the only trustworthy total. A
#              tally reconstructed from greps is not evidence.
#   ansi       logs arrive with real ESC bytes (local runs) OR the literal
#              two-character sequence "^[" (gh run view --log). Strip both.
#
# Usage: scan-test-log.sh <logfile>
set -uo pipefail

log=${1:?usage: scan-test-log.sh <logfile>}
[ -r "$log" ] || { echo "cannot read $log" >&2; exit 2; }

clean=$(mktemp)
trap 'rm -f "$clean"' EXIT
# Real ESC sequences, then the literal caret-bracket form.
sed -e 's/\x1b\[[0-9;]*m//g' -e 's/\^\[\[[0-9;]*m//g' "$log" > "$clean"

# `grep -c` already prints 0 when it matches nothing AND exits 1, so a
# `|| echo 0` fallback prints a SECOND zero on its own line. Swallow the exit
# status instead of appending to the output.
count() { grep -acE "$1" "$clean" 2>/dev/null; true; }

# Verdicts are anchored on "<KIND> [" — the bracket separates a nextest verdict
# from prose containing the same word.
pass=$(count '(^|[^A-Z])PASS \[')
# A hard FAIL must exclude retry lines: "TRY 1 FAIL [" also ends in " FAIL [",
# so counting it here would double-report every retry as a hard failure.
fail=$(grep -aE '(^|[^A-Z])FAIL \[' "$clean" | grep -acvE 'TRY [0-9]+ FAIL'; true)
try_fail=$(count 'TRY [0-9]+ FAIL')
timeout=$(count '(^|[^A-Z])TIMEOUT \[')
leak=$(count '(^|[^A-Z])LEAK \[')
# The per-test "FLAKY [" verdict and the summary's "(N flaky)" describe the SAME
# retries; count only the per-test verdict so a flake is not reported twice.
flaky=$(count '(^|[^A-Z])FLAKY \[')

summary=$(grep -aE 'Summary \[.*tests run:' "$clean" | tail -1 | sed 's/^[[:space:]]*//')

# Parse the summary's own failure count. The summary is the only trustworthy total
# (lines 14-15), yet until now the verdict relied entirely on grep-counted verdict
# lines. When nextest crashes before printing individual FAIL [ lines — setup script
# failure, killed test binary, internal error — the summary records the failure but
# no verdict line exists to grep, and the scanner reported CLEAN.
summary_failed=0
if [ -n "$summary" ]; then
  # Match patterns like "0 passed, 1 failed" or "1 failed"
  summary_failed=$(echo "$summary" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo 0)
  [ -z "$summary_failed" ] && summary_failed=0
fi

echo "summary: ${summary:-<none found>}"
echo "pass: $pass"
echo "fail: $fail"
echo "try_fail: $try_fail"
echo "timeout: $timeout"
echo "leak: $leak"
echo "flaky: $flaky"

# A run is clean only if the summary says so AND nothing was retried. A retry
# that passed is a flake, not a pass, and must not be absorbed into a total.
if [ -z "$summary" ]; then
  echo "verdict: UNKNOWN (no summary line — did the run finish?)"
  exit 3
elif [ "$fail" -gt 0 ] || [ "$timeout" -gt 0 ] || [ "$leak" -gt 0 ]; then
  echo "verdict: FAILED"
  exit 1
elif [ "$summary_failed" -gt 0 ]; then
  echo "verdict: FAILED (summary reports $summary_failed failed but no verdict lines found)"
  exit 1
elif [ "$try_fail" -gt 0 ] || [ "$flaky" -gt 0 ]; then
  echo "verdict: FLAKY (passed only on retry — name it, do not absorb it)"
  exit 1
else
  echo "verdict: CLEAN"
  exit 0
fi
