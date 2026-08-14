#!/bin/bash
# Tests for check-review-threads.sh — one case per fail-open hole the gate has had.
#
# Every case here was written against the UNFIXED script and observed FAILING first.
# A test that has never been red proves nothing (see .claude/skills/pr-workflow).
#
# The seven holes were found by Codex review on dolphin-labs#5, which runs a port of
# this same gate; the fixes and these cases came back here. Re-verified against this
# repo's own pre-fix script: 7 red, then 15 green.
#
# Usage: test-check-review-threads.sh [path-to-check-review-threads.sh]
set -uo pipefail
GATE=${1:-"$(dirname "$0")/check-review-threads.sh"}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
pass=0; fail=0

# $1 name, $2 fixture json, $3 expected exit code, $4 substring expected in output
run_case() {
  local name=$1 json=$2 want_rc=$3 want_txt=${4:-}
  printf '%s' "$json" > "$TMP/f.json"
  local out rc
  out=$(bash "$GATE" --from-file "$TMP/f.json" 2>&1); rc=$?
  if [ "$rc" = "$want_rc" ] && { [ -z "$want_txt" ] || grep -qF "$want_txt" <<<"$out"; }; then
    echo "  PASS  $name"; pass=$((pass+1))
  else
    echo "  FAIL  $name (rc=$rc want=$want_rc)"
    sed 's/^/          /' <<<"$out" | head -4
    fail=$((fail+1))
  fi
}

wrap() { printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":%s},"reviews":{"nodes":%s}}}}}' "$1" "${2:-[]}"; }

echo "== finding 1: a missing/nonexistent PR must not read as 'no threads' =="
run_case "null pullRequest blocks" \
  '{"data":{"repository":{"pullRequest":null}}}' 2 "BLOCKED"

echo "== finding 3: isResolved must be a real boolean =="
run_case "isResolved null blocks" \
  "$(wrap '[{"isResolved":null,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this crashes"}]}}]')" \
  2 "BLOCKED"
run_case "isResolved string blocks" \
  "$(wrap '[{"isResolved":"true","comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this crashes"}]}}]')" \
  2 "BLOCKED"

echo "== finding 2: RED-VERIFIED needs an actual test name =="
run_case "bare RED-VERIFIED: blocks" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this crashes on empty input"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"RED-VERIFIED:"}]}}]')" \
  1 "BLOCKED"
run_case "RED-VERIFIED with test name clears" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this crashes on empty input"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"RED-VERIFIED: recommendation.test.ts"}]}}]')" \
  0 "CLEAR"

echo "== findings 0 + 5: every resolved thread needs an explicit disposition =="
run_case "defect wording the old regex missed still blocks" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this drops the final game from the schedule"}]}}]')" \
  1 "BLOCKED"
run_case "resolved with no reply at all blocks" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"rename this variable"}]}}]')" \
  1 "BLOCKED"
run_case "documented disagreement is a valid disposition" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this is broken and crashes"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"DISAGREE: the guard above already rejects that input"}]}}]')" \
  0 "CLEAR"
run_case "non-defect disposition clears" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"rename this variable"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"NOT-A-DEFECT: renamed in 1a2b3c4"}]}}]')" \
  0 "CLEAR"

echo "== finding 4: PR-level review bodies must be adjudicated =="
run_case "unacknowledged review body blocks" \
  "$(wrap '[]' '[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","body":"P1: this silently drops events"}]')" \
  1 "BLOCKED"
run_case "acknowledged review body clears" \
  "$(wrap '[]' '[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","body":"P1: this silently drops events"},{"author":{"login":"me"},"state":"COMMENTED","body":"REVIEW-ACK: addressed in the thread above"}]')" \
  0 "CLEAR"
run_case "empty review body needs no ack" \
  "$(wrap '[]' '[{"author":{"login":"someone"},"state":"APPROVED","body":""}]')" \
  0 "CLEAR"

echo "== finding 6: a disposition past the first comment page must be seen =="
long=$(python3 -c '
import json
n=[{"author":{"login":"bot"},"path":"a.ts","line":1,"body":"this leaks memory"}]
n+=[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"chatter %d"%i} for i in range(120)]
n+=[{"author":{"login":"y"},"path":"a.ts","line":1,"body":"RED-VERIFIED: leak.test.ts"}]
print(json.dumps([{"isResolved":True,"comments":{"nodes":n,"totalCount":len(n)}}]))')
run_case "RED-VERIFIED after 100+ comments is found" "$(wrap "$long")" 0 "CLEAR"

echo "== regression: the original behaviours still hold =="
run_case "unresolved thread blocks" \
  "$(wrap '[{"isResolved":false,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"something"}]}}]')" \
  1 "UNRESOLVED"
run_case "no threads and no reviews is clear" "$(wrap '[]')" 0 "CLEAR"

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
