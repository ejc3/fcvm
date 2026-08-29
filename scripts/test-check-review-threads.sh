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

# $1 name, $2 fixture json, $3 expected exit code, $4 and $5 substrings expected in output
run_case() {
  local name=$1 json=$2 want_rc=$3 want_txt=${4:-} want_txt2=${5:-}
  printf '%s' "$json" > "$TMP/f.json"
  local out rc
  out=$(bash "$GATE" --from-file "$TMP/f.json" 2>&1); rc=$?
  if [ "$rc" = "$want_rc" ] && { [ -z "$want_txt" ] || grep -qF "$want_txt" <<<"$out"; } \
     && { [ -z "$want_txt2" ] || grep -qF "$want_txt2" <<<"$out"; }; then
    echo "  PASS  $name"; pass=$((pass+1))
  else
    echo "  FAIL  $name (rc=$rc want=$want_rc)"
    sed 's/^/          /' <<<"$out" | head -4
    fail=$((fail+1))
  fi
}

# Every case runs under the full ruleset, head coverage included (finding 29 closed the
# skip for a PR with no review objects). The builders for cases about dispositions put
# the head on a covered commit: a stranger's APPROVED review of deadbeef, spliced in front
# of the reviews the case supplies. A non-array passes through untouched so the
# validation cases still reach the parser with it. Cases about coverage use wrap5-8.
COVER='{"author":{"login":"reviewer"},"state":"APPROVED","submittedAt":"2025-12-31T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}'
covered() { if jq -e 'type == "array"' >/dev/null 2>&1 <<<"${1:-[]}"; then jq -c --argjson c "$COVER" '[$c] + .' <<<"${1:-[]}"; else printf '%s' "$1"; fi; }
wrap() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":%s},"reviews":{"nodes":%s}}}}}' "$1" "$(covered "${2:-[]}")"; }

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
run_case "undisposed review body blocks" \
  "$(wrap '[]' '[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops events"}]')" \
  1 "BLOCKED"
run_case "disposed review body clears" \
  "$(wrap '[]' '[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops events"},{"author":{"login":"me"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"DISAGREE: the guard above already rejects that input"}]')" \
  0 "CLEAR"
run_case "empty review body needs no disposition" \
  "$(wrap '[]' '[{"author":{"login":"someone"},"state":"APPROVED","body":""}]')" \
  0 "CLEAR"

echo "== finding 8: a reply ASKING for proof is not a disposition =="
# "Please add RED-VERIFIED: <test>" contains the marker followed by non-whitespace, so an
# unanchored substring test counted the person DEMANDING evidence as the one supplying it.
run_case "reply requesting proof blocks" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this corrupts the output"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"Please add RED-VERIFIED: test-name before resolving this."}]}}]')" \
  1 "BLOCKED"
run_case "disposition buried after prose blocks" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this corrupts the output"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"we talked about this and\nNOT-A-DEFECT: it is fine"}]}}]')" \
  1 "BLOCKED"
run_case "disposition opening the reply clears" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this corrupts the output"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"NOT-A-DEFECT: renamed only, no behaviour change"}]}}]')" \
  0 "CLEAR"

echo "== finding 9: an answer must be NEWER than the claim it answers =="
# One disposition used to zero the whole count, so a review body posted AFTER it was
# silently treated as answered.
run_case "claim posted after the disposition blocks" \
  "$(wrap '[]' '[{"author":{"login":"me"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"DISAGREE: answered the first round"},{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-05T00:00:00Z","body":"P1: new finding nobody has answered"}]')" \
  1 "BLOCKED"
run_case "two claims, one late disposition clears both" \
  "$(wrap '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: first finding"},{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"P2: second finding"},{"author":{"login":"me"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"RED-VERIFIED: covers both, tests/foo.rs"}]')" \
  0 "CLEAR"

echo "== finding 10: review data must be validated like thread data =="
run_case "reviews.nodes as an object exits 2" \
  "$(wrap '[]' '{"malformed":true}')" 2 "BLOCKED"
run_case "reviews.nodes as a string exits 2" \
  "$(wrap '[]' '"nonsense"')" 2 "BLOCKED"
run_case "review body of the wrong type exits 2" \
  "$(wrap '[]' '[{"author":{"login":"x"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":42}]')" \
  2 "BLOCKED"
run_case "non-empty review body with no timestamp exits 2" \
  "$(wrap '[]' '[{"author":{"login":"x"},"state":"COMMENTED","body":"P1: this drops events"}]')" \
  2 "BLOCKED"

echo "== finding 11: PR-level claims answer to the same vocabulary as inline ones =="
# A bare "REVIEW-ACK: read" used to clear a PR-level defect claim that, posted inline,
# would have demanded a real disposition. Same finding, two standards.
run_case "PR-level claim + bare ack blocks" \
  "$(wrap '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops the last record"},{"author":{"login":"me"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"REVIEW-ACK: read"}]')" \
  1 "BLOCKED"

echo "== finding 6: a disposition past the first comment page must be seen =="
long=$(python3 -c '
import json
n=[{"author":{"login":"bot"},"path":"a.ts","line":1,"body":"this leaks memory"}]
n+=[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"chatter %d"%i} for i in range(120)]
n+=[{"author":{"login":"y"},"path":"a.ts","line":1,"body":"RED-VERIFIED: leak.test.ts"}]
print(json.dumps([{"isResolved":True,"comments":{"nodes":n,"totalCount":len(n)}}]))')
run_case "RED-VERIFIED after 100+ comments is found" "$(wrap "$long")" 0 "CLEAR"

echo "== finding 14: ordinary chatter after a disposition must NOT re-block =="
# A positional "the disposition must be last" rule was tried and withdrawn: it could not
# tell a new defect claim from "Thanks, confirmed", so it blocked adjudicated threads.
run_case "reviewer confirmation after a disposition clears" \
  "$(wrap '[{"isResolved":true,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"this drops a record"},{"author":{"login":"y"},"path":"a.ts","line":1,"body":"RED-VERIFIED: a.test.ts"},{"author":{"login":"x"},"path":"a.ts","line":1,"body":"Thanks, confirmed."}]}}]')" \
  0 "CLEAR"

echo "== finding 13: findings in top-level PR comments count too =="
wrap3() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","reviewThreads":{"nodes":%s},"reviews":{"nodes":%s},"comments":{"nodes":%s}}}}}' "$1" "$(covered "${2:-[]}")" "${3:-[]}"; }
run_case "undisposed top-level PR comment blocks" \
  "$(wrap3 '[]' '[]' '[{"author":{"login":"codex"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops the last row"}]')" \
  1 "BLOCKED"
run_case "disposed top-level PR comment clears" \
  "$(wrap3 '[]' '[]' '[{"author":{"login":"codex"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops the last row"},{"author":{"login":"me"},"createdAt":"2026-01-02T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","body":"DISAGREE: the guard above rejects that input"}]')" \
  0 "CLEAR"
run_case "a PR comment can answer a review body" \
  "$(wrap3 '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: drops a row"}]' '[{"author":{"login":"me"},"createdAt":"2026-01-02T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","body":"RED-VERIFIED: row.test.ts"}]')" \
  0 "CLEAR"
run_case "malformed PR comment data exits 2" \
  "$(wrap3 '[]' '[]' '"nonsense"')" 2 "BLOCKED"

echo "== finding 15: only real claimants raise PR-level claims =="
wrap4() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"%s"},"headRefOid":"deadbeef","reviewThreads":{"nodes":%s},"reviews":{"nodes":%s},"comments":{"nodes":%s}}}}}' "$1" "$2" "$(covered "${3:-[]}")" "${4:-[]}"; }
# The command this skill documents — posted by the PR author — is not a finding.
run_case "author's own @codex review comment does not block" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"me","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"@codex review"}]')" \
  0 "CLEAR"
run_case "deploy bot notification does not block" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"vercel","__typename":"Bot"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"Deployment ready at https://preview.example"}]')" \
  0 "CLEAR"
run_case "a human finding in a top-level comment still blocks" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"reviewer","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"P1: this drops the last row"}]')" \
  1 "BLOCKED"
run_case "a reviewing bot's top-level comment still blocks" \
  "$(wrap4 me '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":""}]' '[{"author":{"login":"codex","__typename":"Bot"},"createdAt":"2026-01-02T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","body":"P1: this drops the last row"}]')" \
  1 "BLOCKED"
run_case "a bot REVIEW body still blocks" \
  "$(wrap4 me '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: this drops the last row"}]' '[]')" \
  1 "BLOCKED"

echo "== finding 16: the head commit must actually have been reviewed =="
wrap5() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"%s","reviewThreads":{"nodes":[]},"reviews":{"nodes":%s},"comments":{"nodes":[]}}}}}' "$1" "$2"; }
run_case "reviews exist but none cover the head blocks" \
  "$(wrap5 deadbeef '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]')" \
  1 "UNREVIEWED HEAD"
run_case "a review covering the head clears" \
  "$(wrap5 deadbeef '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]')" \
  0 "CLEAR"
# Flipped by finding 29: this case expected CLEAR, which was the fail-open itself.
run_case "no reviews at all is an unreviewed head, not a clear one (was CLEAR)" \
  "$(wrap5 deadbeef '[]')" 1 "UNREVIEWED HEAD"

echo "== finding 17: authorship does not decide what is a finding =="
run_case "author's own defect report is claimable" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"me","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"P1: this drops records"}]')" \
  1 "BLOCKED"
run_case "a standalone bot finding with no review history is claimable" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"codex","__typename":"Bot"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"P1: this drops the last row"}]')" \
  1 "BLOCKED"
run_case "an APPROVED review body is not a finding" \
  "$(wrap4 me '[]' '[{"author":{"login":"reviewer"},"state":"APPROVED","submittedAt":"2026-01-01T00:00:00Z","body":"LGTM, nice work"}]' '[]')" \
  0 "CLEAR"
run_case "a COMMENTED review body still is" \
  "$(wrap4 me '[]' '[{"author":{"login":"reviewer"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"LGTM, nice work"}]' '[]')" \
  1 "BLOCKED"
run_case "a bare @codex trigger is not a finding" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"anyone","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"@codex review"}]')" \
  0 "CLEAR"
run_case "a finding that merely mentions @codex still blocks" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"anyone","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","body":"@codex review this: P1 it drops rows"}]')" \
  1 "BLOCKED"

echo "== finding 18: the author reviewing their own push is not coverage =="
run_case "only the author reviewed the head" \
  "$(wrap5 deadbeef '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}},{"author":{"login":"me"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"NOT-A-DEFECT: fixed","commit":{"oid":"deadbeef"}}]')" \
  1 "UNREVIEWED HEAD"
run_case "someone else reviewed the head" \
  "$(wrap5 deadbeef '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]')" \
  0 "CLEAR"

echo "== finding 19: a reviewer with nothing to say posts no review object =="
# Codex answers "Didn't find any major issues" as a plain comment; CodeRabbit's full
# review with no comments replies "Full review finished." and creates no review. A head
# that no bot has anything to say about must not sit BLOCKED forever: a bot's no-findings
# result that names the head, dated AFTER the head ARRIVED at GitHub, covers it (finding
# 26 below has the CodeRabbit side). Arrival is the creation of the head's first check
# suite, not the commit's own date: a commit made locally before an older head's verdict
# and pushed afterwards carries a committedDate that predates that verdict (codex on
# #873). The verdict must be the whole comment, in the shape the bot actually posts (the
# builders below copy live comments): a body carrying the phrase plus finding text is a
# finding, claimable and not coverage.
wrap6() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"},{"createdAt":"2026-01-02T00:31:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":%s},"recheck":{"comments":{"nodes":%s}}}}}}' "$1" "${2:-$1}"; }
# One top-level PR comment as GraphQL returns it: login, __typename, createdAt, a body
# that is already a JSON string literal, and updatedAt ($5, defaulting to createdAt as
# GitHub does for a comment nobody has edited).
cmt() { printf '{"author":{"login":"%s","__typename":"%s"},"createdAt":"%s","updatedAt":"%s","body":%s}' "$1" "$2" "$3" "${5:-$3}" "$4"; }
# Every payload carries the gate's SECOND read of the comments under `recheck` — what
# fetch_payload fetches after all paging, and compares against the first read (finding 31).
# The builders default it to the first read: a fixture models a run in which nothing changed,
# unless the case is about something that did.
#
# wrap9 is the wrap6 head with a review object already on it, for cases about whether a
# comment needs a disposition rather than about what covers the head.
wrap9() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]},"comments":{"nodes":[%s]},"recheck":{"comments":{"nodes":[%s]}}}}}}' "$1" "${2:-$1}"; }
# Codex's no-findings comment as posted: the phrase with sign-off $1, the reviewed commit
# $2, and the folded About Codex block. Emits a JSON string literal.
codex_body() { jq -n --arg s "$1" --arg sha "$2" '"Codex Review: Didn\u0027t find any major issues.\($s)\n\n**Reviewed commit:** `\($sha)`\n\n<details> <summary>ℹ️ About Codex in GitHub</summary>\n<br/>\n\n[Your team has set up Codex to review pull requests in this repo](https://chatgpt.com/codex/cloud/settings/general). Reviews are triggered when you\n- Open a pull request for review\n- Mark a draft as ready\n- Comment \"@codex review\".\n\nIf Codex has suggestions, it will comment; otherwise it will react with 👍.\n\n\n\n\nCodex can also answer questions or update the PR. Try commenting \"@codex address that feedback\".\n            \n</details>"'; }
# CodeRabbit's command acknowledgement as posted, folding $1 under "Action performed".
cr_body() { jq -n --arg t "$1" '"<!-- This is an auto-generated reply by CodeRabbit -->\n<!-- CodeRabbit review command invocation: 8fd6df2f-253d-45d8-a76b-9078ed862337 -->\n<details>\n<summary>✅ Action performed</summary>\n\n\($t)\n\n</details>"'; }
CODEX=chatgpt-codex-connector
CR_NOTE=$'Review finished.\n\n> Note: CodeRabbit is an incremental review system and does not re-review already reviewed commits. This command is applicable only when automatic reviews are paused.'
run_case "a codex no-findings comment after the head arrived covers it" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")]")" \
  0 "HEAD COVERED"
# The two CodeRabbit replies are notices: exempt from dispositions, never coverage. They
# used to cover the head when they answered a request posted after arrival; finding 26
# withdraws that rule, so both expect UNREVIEWED HEAD.
run_case "a coderabbit full-review-finished reply is not coverage, even after a request" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai full review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  1 "UNREVIEWED HEAD" "the head commit has not been reviewed"
run_case "a coderabbit review-finished reply with its note is not coverage either" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body "$CR_NOTE")")]")" \
  1 "UNREVIEWED HEAD" "the head commit has not been reviewed"
run_case "a coderabbit reply with its note needs no disposition" \
  "$(wrap9 "$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body "$CR_NOTE")")")" \
  0 "CLEAR"
run_case "a verdict dated after the commit but before the head arrived is not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T00:10:00Z "$(codex_body '' deadbeef)")]")" \
  1 "UNREVIEWED HEAD"
run_case "the author saying no findings is not coverage" \
  "$(wrap6 "[$(cmt me User 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")]")" \
  1 "UNREVIEWED HEAD"
run_case "a verdict cannot cover a head that has no check suite (arrival unknown)" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":[%s]},"recheck":{"comments":{"nodes":[%s]}}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
run_case "a no-findings verdict is not a finding that needs a disposition" \
  "$(wrap9 "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")")" \
  0 "CLEAR"
run_case "the verdict phrase beside finding text is a finding, not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body $'\n\nOne thing though: the lease is dropped before the rename, which races the pruner.' deadbeef)")]")" \
  1 "carry no disposition"
run_case "a coderabbit ack with extra review text is a finding, not coverage" \
  "$(wrap6 "[$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body $'Full review finished.\n\nActionable comments posted: 1\n\nThe stall gate is never armed.')")]")" \
  1 "carry no disposition"

echo "== finding 21: a verdict is a fixed shape, not a phrase with an open tail =="
# "[^\n]{0,40}" after the Codex phrase accepted a P1 on the same line as a verdict, and a
# one-line phrase with no reviewed-commit line, which Codex never posts. The sign-off is
# now one of an enumerated list; the reviewed-commit line is required; and only the About
# Codex details block is stripped, since any other details block is where findings fold.
for signoff in ' :rocket:' ' Keep it up!' " You're on a roll."; do
  run_case "the codex sign-off '$signoff' is a verdict" \
    "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body "$signoff" deadbeef)")]")" \
    0 "HEAD COVERED"
done
run_case "the codex phrase with a P1 on the same line is not a verdict" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z '"Codex Review: Didn'"'"'t find any major issues. P1: drops data"')]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "the codex phrase alone, without the reviewed-commit line, is not a verdict" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z '"Codex Review: Didn'"'"'t find any major issues."')]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "a P1 on the codex phrase line, in the posted shape, is claimable, not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' P1: drops data' deadbeef)")]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "a details block other than About Codex is not stripped" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body $'\n\n<details><summary>Findings</summary>\nP1: drops data\n</details>' deadbeef)")]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"

echo "== finding 22: only a listed review bot can issue a verdict =="
# The verdict words from anyone but the Bot account of a listed reviewer are an ordinary
# comment: claimable like any other, and never coverage. Both fixtures name the head in
# the reviewed-commit line, so only the author decides the outcome.
run_case "a human posting the codex verdict is claimable, not coverage" \
  "$(wrap6 "[$(cmt helpful-human User 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "an unlisted bot posting the codex verdict is claimable, not coverage" \
  "$(wrap6 "[$(cmt some-other-bot Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"

echo "== finding 23: a codex verdict is bound to the commit it names, not merely dated =="
# A review of the OLD head that finishes after the new one arrives is dated after arrival
# too. Codex names the commit it reviewed, so that sha must be a prefix of the head.
run_case "a codex verdict naming an older commit is not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")]")" \
  1 "UNREVIEWED HEAD"
# Guard, green before and after: a verdict for an older commit is still a verdict, and a
# head covered by a review object does not go BLOCKED over an undisposed stale verdict.
run_case "a codex verdict naming an older commit needs no disposition" \
  "$(wrap9 "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")")" \
  0 "CLEAR"

echo "== finding 24: a trigger is a documented bot command, nothing else =="
# TRIGGER_RE accepted any mention plus up to two words, so "@me drops records" was exempt
# from claimable and the gate reported CLEAR with nothing answered.
run_case "a mention plus two words is a claim, not a trigger" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@me drops records"')]")" \
  1 "BLOCKED" "carry no disposition"
run_case "a listed bot's mention with a non-command is a claim" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@codex drops records"')]")" \
  1 "BLOCKED" "carry no disposition"
# Guards, green before and after: the documented commands stay exempt, in any case,
# because GitHub logins are case-insensitive.
run_case "@coderabbitai full review is a trigger, not a finding" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@coderabbitai full review"')]")" \
  0 "CLEAR"
run_case "@coderabbitai resume is a trigger, not a finding" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@coderabbitai resume"')]")" \
  0 "CLEAR"
run_case "@codex security review is a trigger, not a finding" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@codex security review"')]")" \
  0 "CLEAR"
run_case "a trigger in another case is still a trigger" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@CodeRabbitAI review"')]")" \
  0 "CLEAR"

echo "== finding 25: only CodeRabbit's known note is stripped before the shape check =="
# Every line starting with ">" was dropped before the whole-comment check, so a bot that
# quoted its finding, "> P1: this drops records", had posted a verdict and covered the head.
run_case "a codex verdict quoting a finding is claimable, not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body $'\n\n> P1: this drops records' deadbeef)")]")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "a coderabbit reply quoting a finding is claimable" \
  "$(wrap6 "[$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body $'Full review finished.\n\n> P1: this drops records')")]")" \
  1 "carry no disposition"
# The live note shape stays exempt: "a coderabbit reply with its note needs no disposition".

echo "== finding 26: a CodeRabbit result is bound to the head by the commit it names =="
# CodeRabbit's reply names nothing. Binding it to the latest review request posted after
# arrival was timestamp ordering: a review of the OLD head that finished after a new
# request counted as that request's answer. The walkthrough comment CodeRabbit edits in
# place names the range it reviewed after a review with no comments, so that is the
# signal (bodies copied from #815 and #872), and the reply is a notice.
run_case "an old review finishing after a new request is not coverage" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:20:00Z '"@coderabbitai review"'),$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  1 "UNREVIEWED HEAD"
HEAD40=5f8a63e13cd9fdc777d23165ecd5f149fb93f848
BASE40=1aa0e96276e5500e60fc93f56d3c74298b4a2ba3
OLD40=eb7fa388b1685edcb70514be82f072d7d6525819
# wrap6 with a full-length head, which the walkthrough range names in full.
wrap7() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"%s","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":%s},"recheck":{"comments":{"nodes":%s}}}}}}' "$HEAD40" "$1" "${2:-$1}"; }
# CodeRabbit's walkthrough after a review that produced no comments (#815): the block
# between the recent_review markers names the range reviewed, from $1 to $2.
cr_walkthrough() { jq -n --arg from "$1" --arg to "$2" '"<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- recent_review_start -->\n\nNo actionable comments were generated in the recent review. 🎉\n\n<details>\n<summary>ℹ️ Recent review info</summary>\n\n<details>\n<summary>⚙️ Run configuration</summary>\n\n**Configuration used**: defaults\n\n**Review profile**: CHILL\n\n**Plan**: Pro Plus\n\n**Run ID**: `80692642-637e-46d1-bd3e-dea6682a1c78`\n\n</details>\n\n<details>\n<summary>📥 Commits</summary>\n\nReviewing files that changed from the base of the PR and between \($from) and \($to).\n\n</details>\n\n<details>\n<summary>📒 Files selected for processing (1)</summary>\n\n* `scripts/check-review-threads.sh`\n\n</details>\n\n</details>\n\n---\n\n\n\n<!-- recent_review_end -->\n<!-- walkthrough_start -->\n\n<details>\n<summary>📝 Walkthrough</summary>\n\n## Walkthrough\n\nThe review gate binds a CodeRabbit result to the head it names.\n\n</details>\n\n<!-- walkthrough_end -->"'; }
# The same comment while CodeRabbit is rate limited (#872): the notice quotes the range a
# review WOULD cover, and no review ran.
cr_walkthrough_limited() { jq -n --arg from "$1" --arg to "$2" '"<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- This is an auto-generated comment: rate limited by coderabbit.ai -->\n\n> [!WARNING]\n> ## Review limit reached\n> \n> **Next included review available in 51 minutes.**\n> \n> <details>\n> <summary>View limit details</summary>\n> \n> **Limit details:** You have used the included review currently available.\n> \n> **Review configuration:**\n> \n> <details>\n> <summary>📥 Commits</summary>\n> \n> Reviewing files that changed from the base of the PR and between \($from) and \($to).\n> \n> </details>\n> \n> </details>\n\n<!-- end of auto-generated comment: rate limited by coderabbit.ai -->\n<!-- walkthrough_start -->\n\n## Walkthrough\n\nThe review gate binds a CodeRabbit result to the head it names.\n\n<!-- walkthrough_end -->"'; }
# The walkthrough is a bot comment and claimable like any other, so each case answers it.
ANSWER=$(cmt me User 2026-01-02T02:00:00Z '"NOT-A-DEFECT: the walkthrough is a summary, not a finding"')
run_case "a walkthrough whose recent review ends at the head covers it" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walkthrough "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  0 "HEAD COVERED"
# Guards, green before and after.
run_case "a walkthrough whose recent review ends at an older commit is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walkthrough "$BASE40" "$OLD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a range quoted in the rate-limit notice is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walkthrough_limited "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a walkthrough edited before the head arrived is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walkthrough "$BASE40" "$HEAD40")" 2026-01-02T00:10:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a walkthrough from an unlisted bot is not coverage" \
  "$(wrap7 "[$(cmt some-other-bot Bot 2026-01-01T00:00:00Z "$(cr_walkthrough "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"

echo "== finding 27: a verdict covers a head that has no review object at all =="
# HEAD COVERED required anyreview > 0, but a no-findings verdict has no review object, so
# a head whose only review result was a valid Codex verdict could not be reported covered.
VERDICT_CMT=$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")
run_case "a codex verdict is coverage when no review objects exist" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[]},"comments":{"nodes":[%s]},"recheck":{"comments":{"nodes":[%s]}}}}}}' "$VERDICT_CMT" "$VERDICT_CMT")" \
  0 "HEAD COVERED"

echo "== finding 28: a walkthrough after a review that did not run clean is not coverage =="
# The recent-review block names its range after a FAILED review too. On #853 and #792 it
# ended at the head, carried no "No actionable comments were generated" line, sat under a
# "Review failed" caution, and no review object existed on the head; the gate printed
# HEAD COVERED. The range alone proves nothing. A walkthrough now covers only when its
# recent-review block says no actionable comments were generated, the summarize marker is
# its only auto-generated-comment marker, and it carries no blockquoted heading (every
# CodeRabbit notice title is one: Review failed / skipped / limit reached, Reviews paused;
# the in-progress notice has a marker and no heading). Notice bodies copied from #853,
# #869, #874, #837 and #872; the rest of the comment is the #853 body.
NOTICE_FAILED=$'<!-- This is an auto-generated comment: failure by coderabbit.ai -->\n\n> [!CAUTION]\n> ## Review failed\n> \n> The pull request is closed.\n\n<!-- end of auto-generated comment: failure by coderabbit.ai -->\n\n'
NOTICE_LIMITED=$'<!-- This is an auto-generated comment: rate limited by coderabbit.ai -->\n\n> [!WARNING]\n> ## Review limit reached\n> \n> **Next included review available in 27 minutes.**\n> \n> <details>\n> <summary>View limit details</summary>\n> \n> **Limit details:** You’ve used the included review currently available.\n> \n> </details>\n\n<!-- end of auto-generated comment: rate limited by coderabbit.ai -->\n'
NOTICE_SKIPPED=$'<!-- This is an auto-generated comment: skip review by coderabbit.ai -->\n\n> [!IMPORTANT]\n> ## Review skipped\n> \n> Auto reviews are disabled on base/target branches other than the default branch.\n> \n> Please check the settings in the CodeRabbit UI or the `.coderabbit.yaml` file in this repository. To trigger a single review, invoke the `@coderabbitai review` command.\n\n<!-- end of auto-generated comment: skip review by coderabbit.ai -->\n\n'
NOTICE_PAUSED=$'<!-- This is an auto-generated comment: review paused by coderabbit.ai -->\n\n> [!NOTE]\n> ## Reviews paused\n> \n> It looks like this branch is under active development. To avoid overwhelming you with review comments due to an influx of new commits, CodeRabbit has automatically paused this review.\n\n<!-- end of auto-generated comment: review paused by coderabbit.ai -->\n'
NOTICE_PROGRESS=$'<!-- This is an auto-generated comment: review in progress by coderabbit.ai -->\n\n> [!NOTE]\n> Currently processing new changes in this PR. This may take a few minutes, please wait...\n\n<!-- end of auto-generated comment: review in progress by coderabbit.ai -->\n'
# A heading with no marker: not observed live, pinned so the two rules are independent.
NOTICE_HEADING_ONLY=$'\n> [!CAUTION]\n> ## Review failed\n> \n> The pull request is closed.\n\n'
# The #853 walkthrough with notice $1 above a recent-review block from $3 to $4; $2 is
# "clean" for the no-actionable line CodeRabbit adds after a review with no comments, or
# "failed" for the block as a failed review leaves it. "none" for $1 or $4 omits that part.
cr_walk() { jq -n --arg notice "$1" --arg clean "$2" --arg from "$3" --arg to "$4" '
  "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!-- review_stack_entry_start -->\n\n[![Review Change Stack](https://storage.googleapis.com/coderabbit_public_assets/review-stack-in-coderabbit-ui.svg)](https://app.coderabbit.ai/change-stack/ejc3/fcvm/pull/853)\n\n<!-- review_stack_entry_end -->\n"
  + (if $notice == "none" then "" else $notice end)
  + (if $to == "none" then "" else
      "<!-- recent_review_start -->\n\n"
      + (if $clean == "clean" then "No actionable comments were generated in the recent review. 🎉\n\n" else "" end)
      + "<details>\n<summary>ℹ️ Recent review info</summary>\n\n<details>\n<summary>⚙️ Run configuration</summary>\n\n**Configuration used**: defaults\n\n**Review profile**: CHILL\n\n**Plan**: Pro Plus\n\n**Run ID**: `e2d05d3e-3f22-4137-a868-25f7f24b39df`\n\n</details>\n\n<details>\n<summary>📥 Commits</summary>\n\nReviewing files that changed from the base of the PR and between \($from) and \($to).\n\n</details>\n\n<details>\n<summary>📒 Files selected for processing (1)</summary>\n\n* `tests/test_ci_workflow_coverage.rs`\n\n</details>\n\n</details>\n\n---\n\n\n\n<!-- recent_review_end -->\n" end)
  + "<!-- walkthrough_start -->\n\n<details>\n<summary>📝 Walkthrough</summary>\n\n## Walkthrough\n\nThe CI workflow updates path classification, renamed-file handling, and fail-open gate outputs.\n\n</details>\n\n<!-- walkthrough_end -->"'; }
run_case "the #853 shape: a failed review whose range ends at the head is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_FAILED" failed "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a range line without the no-actionable line is not coverage, even with no notice" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk none failed "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a rate-limit notice beside a clean recent review at the head is not coverage (#869)" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_LIMITED" clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a skipped-review notice beside a clean recent review at the head is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_SKIPPED" clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a paused-reviews notice beside a clean recent review at the head is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_PAUSED" clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "an in-progress notice (marker, no heading) beside a clean recent review is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_PROGRESS" clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "a Review failed heading with no marker is still a notice, not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_HEADING_ONLY" clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
# Guards, green before and after: the live skipped and in-progress shapes carry no
# recent-review block at all, and the clean shape built here still covers.
run_case "the live skipped shape (#874), with no recent review block, is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_SKIPPED" clean "$BASE40" none)" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "the live in-progress shape (#872), with no recent review block, is not coverage" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_PROGRESS" clean "$BASE40" none)" 2026-01-02T01:00:00Z),$ANSWER]")" \
  1 "UNREVIEWED HEAD"
run_case "the #853 body with a clean recent review at the head and no notice covers" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk none clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  0 "HEAD COVERED"

echo "== finding 29: a head with no review result of any kind is unreviewed, not clear =="
# Finding 27 let a bound verdict cover a head with no review objects, but the blocking
# branch still required a review object to exist ("reviews exist, none on this commit").
# So reviews [] plus a verdict that does NOT bind (an older sha; the head's sha but dated
# before arrival; no check suite to date it against) printed neither HEAD COVERED nor
# UNREVIEWED HEAD and exited CLEAR: a PR whose only review ever was a clean pass on an
# older commit went CLEAR after a push, the race the coverage rule exists for. The head
# must be covered whether or not any review object exists, a PR with no review at all is
# an unreviewed head, and a payload that names no head cannot be judged (regression case
# below).
# The PR's own commits, which is the universe an abbreviated sha is resolved against
# (finding 37). $4 overrides it, as the whole connection: the nodes plus the totalCount
# that says how many there are (finding 39). By default the head is the only commit.
PRCOMMITS_HEAD='{"totalCount":1,"nodes":[{"commit":{"oid":"deadbeef"}}]}'
wrap8() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":%s}}}]},"prcommits":%s,"reviewThreads":{"nodes":[]},"reviews":{"nodes":[]},"comments":{"nodes":[%s]},"recheck":{"comments":{"nodes":[%s]}}}}}}' "$1" "${4:-$PRCOMMITS_HEAD}" "$2" "${3:-$2}"; }
SUITE='[{"createdAt":"2026-01-02T00:30:00Z"}]'
run_case "reviews [] and a codex verdict naming an older sha is an unreviewed head" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")")" \
  1 "UNREVIEWED HEAD" "the head commit has not been reviewed"
run_case "reviews [] and a head-sha verdict dated before arrival is an unreviewed head" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:10:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD" "the head commit has not been reviewed"
run_case "reviews [] and a head-sha verdict with no check suite (arrival unknown) is an unreviewed head" \
  "$(wrap8 '[]' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD" "the head commit has not been reviewed"
# Guard, green before and after: the bound verdict from finding 27 still covers.
run_case "reviews [] and a head-sha verdict dated after arrival still covers" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"

echo "== finding 30: Codex's review-summary comment is a notice, and a sha-bound verdict =="
# Since 2026-08-28 ~21:25Z Codex no longer posts "Didn't find any major issues" per review.
# It opens ONE comment per PR carrying <!-- codex-pull-request-review-summary --> when a
# review starts and EDITS that comment in place as reviews finish, so its createdAt predates
# every result in it and only a row's own datetime dates that row. Bodies copied from
# #867/#872/#873/#874, whose rows all read: Completed, a 7-char commit, "Manual request".
# The comment is a notice — never a finding — and it covers the head when a row is Completed,
# names a prefix of the head and is dated after the head arrived. Anything else in the table
# (a review still running, a status this gate has never seen, a row it cannot parse) makes
# the whole comment cover nothing, because a table with a review still in it is a review
# that has not finished.
#
# $1 status, $2 datetime, $3 commit, $4 review name. The live status cell reads
# "✅ **Completed**" and an unfinished one is not observed here, so the icon is fixed and
# the rule keys on the bolded word.
sum_row() { printf '| 📝 **%s** | ✅ **%s** <relative-time datetime="%s">%s</relative-time> | `%s` | Manual request |' "${4:-Code Review}" "$1" "$2" "$2" "$3"; }
codex_summary() { jq -n --arg rows "$1" '"<!-- codex-pull-request-review-summary -->\n\n## Codex Review Summary\n\nThis comment shows the latest Codex review activity on this pull request.\n\n| Review | Status | Commit | Review trigger |\n| --- | --- | --- | --- |\n\($rows)\n\n\n\n<details> <summary>ℹ️ About Codex in GitHub</summary>\n<br/>\n\n[Your team has set up Codex to review pull requests in this repo](https://chatgpt.com/codex/cloud/settings/general). Reviews are triggered when you\n- Open a pull request for review\n- Mark a draft as ready\n- Comment \"@codex review\" or \"@codex security review\".\n\nCodex reacts with 👀 while any review is running, comments if it has suggestions, and reacts with 👍 once all reviews finish with no findings.\n\n</details>"'; }
# The #867 shape: the comment was created at 00:20, BEFORE the head arrived at 00:30, and
# edited at 01:00 when the review finished. createdAt cannot date this result; the row can.
run_case "the live #867 shape: a summary row Completed at the head covers it" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  0 "HEAD COVERED"
run_case "a summary comment needs no disposition" \
  "$(wrap9 "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  0 "CLEAR"
# The rest answer the summary with $ANSWER, so coverage is the only thing left to decide.
run_case "a summary row naming an older commit is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z abc123def4)")" 2026-01-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a summary row dated before the head arrived is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T00:10:00.123456Z deadbee)")" 2026-01-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a summary row still in progress is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row 'In progress' 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "one Completed row at the head beside one still in progress is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)
$(sum_row 'In progress' 2026-01-02T01:02:00.123456Z deadbee 'Security Review')")" 2026-01-02T01:02:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a Completed row with an unparsable datetime is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 'a while ago' deadbee)")" 2026-01-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a summary table row this gate cannot parse is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary '| 📝 **Code Review** | ✅ **Completed** <relative-time datetime="2026-01-02T01:00:00.123456Z">t</relative-time> |')" 2026-01-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a human posting the summary body is claimable, not coverage" \
  "$(wrap8 "$SUITE" "$(cmt helpful-human User 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "an unlisted bot posting the summary body is claimable, not coverage" \
  "$(wrap8 "$SUITE" "$(cmt some-other-bot Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
# The legacy comment may still appear, and both shapes are Codex verdicts: a summary that
# binds nothing does not take away what the legacy verdict covers.
run_case "the legacy verdict still covers beside a summary that binds nothing" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)"),$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row 'In progress' 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z),$ANSWER")" \
  0 "HEAD COVERED"

echo "== finding 31: a coverage-bearing comment must still say what it said =="
# The gate reads the PR comments FIRST and pages threads afterwards, which on a real PR is
# long enough for a bot to edit what it just read. Two of the three coverage signals are one
# comment their bot edits in place: CodeRabbit's walkthrough and Codex's review summary. The
# final consistency check re-read only headRefOid, so a walkthrough that went from a clean
# review to a failed one, with the head standing still, still granted coverage from a body
# captured minutes earlier — the gate certifying a head from a comment it could no longer
# quote. fetch_payload now re-reads the comments after all paging and the gate compares the
# two reads; the payload carries the second one under `recheck`, so these fixtures exercise
# the same comparison a live run does.
CLEAN_WALK=$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk none clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z)
FAILED_WALK=$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk "$NOTICE_FAILED" failed "$BASE40" "$HEAD40")" 2026-01-02T01:20:00Z)
run_case "a walkthrough that goes clean -> failed between the two reads blocks" \
  "$(wrap7 "[$CLEAN_WALK,$ANSWER]" "[$FAILED_WALK,$ANSWER]")" \
  2 "changed under us"
SUM_DONE=$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)
SUM_RUNNING=$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row 'In progress' 2026-01-02T01:10:00.123456Z deadbee)")" 2026-01-02T01:10:05Z)
run_case "a summary that goes Completed -> in progress between the two reads blocks" \
  "$(wrap8 "$SUITE" "$SUM_DONE,$ANSWER" "$SUM_RUNNING,$ANSWER")" \
  2 "changed under us"
# A payload that grants coverage from an editable comment and carries no second read of it
# cannot be judged: one read cannot show what a comment said when the run ended.
run_case "a coverage-bearing comment with no second read at all blocks" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":%s}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[]},"comments":{"nodes":[%s]}}}}}' "$SUITE" "$SUM_DONE,$ANSWER")" \
  2 "no second read"
# Guards, green before and after: an unchanged second read changes nothing, and a comment
# that cannot grant coverage is not compared, so ordinary traffic arriving mid-run does not
# block. (Every other case in this file is also a guard for the unchanged path, since the
# builders default the second read to the first.)
run_case "an unchanged second read still covers" \
  "$(wrap8 "$SUITE" "$SUM_DONE,$ANSWER" "$SUM_DONE,$ANSWER")" \
  0 "HEAD COVERED"
run_case "a comment that cannot grant coverage may differ between the reads" \
  "$(wrap7 "[$CLEAN_WALK,$(cmt onlooker User 2026-01-02T00:50:00Z '"first wording"'),$ANSWER]" \
           "[$CLEAN_WALK,$(cmt onlooker User 2026-01-02T00:50:00Z '"edited wording"' 2026-01-02T01:30:00Z),$ANSWER]")" \
  0 "HEAD COVERED"

# Every gh shim below answers the PR's commit list from this file: the two heads the live
# cases use, and the totalCount that says the list is whole (finding 39). A case about
# resolving an abbreviation overwrites it.
PRCOMMITS_LIVE=$(printf '{"data":{"repository":{"pullRequest":{"commits":{"totalCount":2,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"oid":"deadbeef"}},{"commit":{"oid":"%s"}}]}}}}}' "$HEAD40")

echo "== finding 31: the LIVE path takes the second read =="
# --from-file can only prove the comparison. That the gate actually re-fetches is a property
# of fetch_payload, so it is tested through the gh shim: the second `comments(first:` query
# is answered with an edited walkthrough, and the run must block. If the gate never issued
# that query, comments2.json would go unread and this case would pass with CLEAR.
mkdir -p "$TMP/bin"
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/threads.json"
printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"%s","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' "$HEAD40" > "$TMP/reviews.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[%s,%s]}}}}}' "$CLEAN_WALK" "$ANSWER" > "$TMP/comments.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[%s,%s]}}}}}' "$FAILED_WALK" "$ANSWER" > "$TMP/comments2.json"
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
case "$*" in
  *"commits(first"*)   cat "$GATE_TEST_DIR/prcommits.json" ;;
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)
    n=$(cat "$GATE_TEST_DIR/ccount" 2>/dev/null || echo 0); n=$((n+1))
    printf '%s' "$n" > "$GATE_TEST_DIR/ccount"
    if [ "$n" -le 1 ]; then cat "$GATE_TEST_DIR/comments.json"; else cat "$GATE_TEST_DIR/comments2.json"; fi ;;
  *headRefOid*)        printf '%s\n' "$GATE_TEST_HEAD" ;;  # the recheck passes --jq
esac
SHIM
printf '%s' "$PRCOMMITS_LIVE" > "$TMP/prcommits.json"
chmod +x "$TMP/bin/gh"
rm -f "$TMP/ccount"
out=$(GATE_TEST_DIR="$TMP" GATE_TEST_HEAD="$HEAD40" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
if [ "$rc" = 2 ] && grep -qF "changed under us" <<<"$out" && [ "$(cat "$TMP/ccount")" -ge 2 ]; then
  echo "  PASS  the live path re-reads the comments and blocks when one changed"; pass=$((pass+1))
else
  echo "  FAIL  the live path re-reads the comments and blocks when one changed (rc=$rc, comment fetches=$(cat "$TMP/ccount" 2>/dev/null))"
  sed 's/^/          /' <<<"$out" | head -4
  fail=$((fail+1))
fi

echo "== finding 32: timestamps are ordered as instants, not as strings =="
# Every ordering decision here compares two GitHub timestamps, and they do not all arrive
# in one shape. Check-suite and comment timestamps come back whole-second
# ("2026-01-02T00:30:00Z"); the datetime Codex writes into a review-summary row carries a
# fraction ("2026-01-02T00:30:00.123456Z"). Compared as strings, "." sorts before "Z", so a
# row completed in the same second as the head's check suite reads as EARLIER than it and
# the head stays uncovered for as long as that row is the only coverage. String order also
# accepts anything that is not a timestamp at all: "a while ago" sorts after every digit, so
# a walkthrough or an answer with an unparsable date used to postdate everything.
run_case "a summary row completed in the same second as the head arrival covers it" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T00:30:00.100Z deadbee)")" 2026-01-02T00:30:05Z)")" \
  0 "HEAD COVERED"
run_case "a disposition posted in the same second as the claim answers it" \
  "$(wrap4 me '[]' '[]' "[$(cmt codex Bot 2026-01-02T00:30:00Z '"P1: this drops the last row"'),$(cmt me User 2026-01-02T00:30:00.100Z '"RED-VERIFIED: tests/row.rs"')]")" \
  0 "CLEAR"
run_case "a disposition 0.1s BEFORE the claim does not answer it" \
  "$(wrap4 me '[]' '[]' "[$(cmt codex Bot 2026-01-02T00:30:00.100Z '"P1: this drops the last row"'),$(cmt me User 2026-01-02T00:30:00Z '"RED-VERIFIED: tests/row.rs"')]")" \
  1 "carry no disposition"
run_case "a verdict timestamped in a non-UTC offset is ordered as the instant it names" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00+01:00 "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
run_case "arrival is the earliest check suite by instant, not by string" \
  "$(wrap8 '[{"createdAt":"2026-01-02T00:30:00.900Z"},{"createdAt":"2026-01-02T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2026-01-02T00:30:00.500Z "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"
# An unparsable timestamp orders against nothing, so it grants no coverage and answers no
# claim. Every field the gate orders by is validated, so it says so and exits 2 rather than
# carrying an unordered value into a comparison. updatedAt joined that list with finding 36.
run_case "a check-suite timestamp that will not parse blocks" \
  "$(wrap8 '[{"createdAt":"2026-01-02T00:30"}]' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  2 "check-suite timestamp"
run_case "a review body whose timestamp will not parse blocks" \
  "$(wrap '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"a while ago","body":"P1: this drops the last row"}]')" \
  2 "no parsable submittedAt"
run_case "an answer whose timestamp will not parse answers nothing" \
  "$(wrap4 me '[]' '[]' "[$(cmt codex Bot 2026-01-02T00:30:00Z '"P1: this drops the last row"'),$(cmt me User yesterday '"RED-VERIFIED: tests/row.rs"')]")" \
  2 "no parsable createdAt"
run_case "a walkthrough whose updatedAt will not parse blocks" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk none clean "$BASE40" "$HEAD40")" 'a while ago'),$ANSWER]")" \
  2 "no parsable updatedAt"
# Guard: the parse is UTC (jq mktime is timegm), so a verdict must not depend on where the
# runner sits. The same fixture under a non-UTC zone reaches the same verdict.
export TZ=America/New_York
run_case "the same-second verdict holds under a non-UTC host timezone" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T00:30:00.100Z deadbee)")" 2026-01-02T00:30:05Z)")" \
  0 "HEAD COVERED"
unset TZ

echo "== finding 33: a zone offset's minutes are minutes, not seconds =="
# `ts` built the offset as $zh * 3600 + $zm, adding the MINUTES field as seconds. Z, +00:00
# and every whole-hour offset are exact, which is why the +01:00 case above passes; a
# half- or quarter-hour zone is off by $zm * 59 seconds, up to 44 minutes. The direction is
# fail-open for positive offsets on both ordering sites: the instant reads LATER than it is,
# so a verdict recorded before the head arrived covers it, and a claim edited after its
# answer reads as though it came first.
# Head arrival is 2026-01-02T00:30:00Z in every wrap8 case below.
run_case "a +05:30 verdict 5 minutes before arrival is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T05:55:00+05:30 "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
run_case "a +09:30 verdict a minute before arrival is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T09:59:00+09:30 "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
run_case "a +12:45 verdict a minute before arrival is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T13:14:00+12:45 "$(codex_body ' Bravo.' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
# A negative half-hour offset reads EARLIER than it is, which fails open on the claim side:
# the claim's true instant 04:05:00Z is after the answer's 04:00:00Z, so it is unanswered.
run_case "a -03:30 claim posted after its answer is not answered by it" \
  "$(wrap4 me '[]' '[]' "[$(cmt codex Bot 2026-01-02T00:35:00-03:30 '"P1: this drops the last row"'),$(cmt me User 2026-01-02T04:00:00Z '"RED-VERIFIED: tests/row.rs"')]")" \
  1 "carry no disposition"
# Guards, green before and after: a half-hour verdict that really is after arrival covers,
# and whole-hour offsets were never affected.
run_case "a +09:30 verdict 5 minutes after arrival covers the head" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T10:05:00+09:30 "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"

echo "== finding 34: only Codex, in the whole shape it posts, writes a review summary =="
# is_codex_summary was `contains("<!-- codex-pull-request-review-summary -->")` applied to
# any listed bot, so two things followed. CodeRabbit carrying that marker became a Codex
# summary: exempt from dispositions, and its table read as coverage. And a real finding from
# Codex that merely QUOTED the marker stopped being claimable, which is a P1 exiting CLEAR
# with nothing answered. The marker must open the comment, the comment must be the whole
# posted shape (heading, the one-line explanation, the four-column table header, its
# separator, and table rows to the end), and the account must be Codex.
codex_summary_plus() { jq -n --argjson b "$(codex_summary "$1")" --arg x "$2" '$b + "\n\n" + $x'; }
run_case "a coderabbit comment in the codex summary shape is a finding, not coverage" \
  "$(wrap8 "$SUITE" "$(cmt coderabbitai Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "a codex finding that quotes the summary marker is claimable" \
  "$(wrap9 "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(jq -n '"P1: this drops the last row.\n\nThe run is in the summary comment (<!-- codex-pull-request-review-summary -->)."')")")" \
  1 "carry no disposition"
run_case "a codex summary with a finding appended after the table is claimable, not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary_plus "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)" 'P1: this drops the last row')" 2026-01-02T01:00:05Z)")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
run_case "a codex summary missing the table header is claimable, not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(jq -n --arg r "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)" '"<!-- codex-pull-request-review-summary -->\n\n\($r)"')" 2026-01-02T01:00:05Z)")" \
  1 "carry no disposition" "UNREVIEWED HEAD"
# Guards, green before and after: the shape Codex actually posts still covers and still needs
# no disposition, and CodeRabbit's own walkthrough is judged by its own rule.
run_case "the posted codex summary shape still covers the head" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z)")" \
  0 "HEAD COVERED"
run_case "a coderabbit walkthrough is still judged as a walkthrough" \
  "$(wrap7 "[$(cmt coderabbitai Bot 2026-01-01T00:00:00Z "$(cr_walk none clean "$BASE40" "$HEAD40")" 2026-01-02T01:00:00Z),$ANSWER]")" \
  0 "HEAD COVERED"

echo "== finding 35: the gate judges the FINAL snapshot, not the one it opened with =="
# The second read existed only to fingerprint the comments that can grant coverage, and the
# verdict was still computed from the FIRST read. Everything else that arrived or vanished
# while the gate paged threads was invisible: a P1 posted mid-run went unjudged, and a
# disposition deleted mid-run still answered its claim. The PR-level bodies are now taken
# from the final read of the comments.
COV_C=$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")
LATE_CLAIM=$(cmt reviewer User 2026-01-02T01:10:00Z '"P1: this drops the last row"')
LATE_DISP=$(cmt me User 2026-01-02T01:20:00Z '"NOT-A-DEFECT: the guard above rejects that input"')
NEW_CLAIM=$(cmt reviewer User 2026-01-02T01:30:00Z '"P1: this one landed while the gate was paging"')
run_case "a disposition deleted between the two reads leaves its claim unanswered" \
  "$(wrap8 "$SUITE" "$COV_C,$LATE_CLAIM,$LATE_DISP" "$COV_C,$LATE_CLAIM")" \
  1 "carry no disposition"
run_case "a claim that lands between the two reads is judged, not missed" \
  "$(wrap8 "$SUITE" "$COV_C" "$COV_C,$NEW_CLAIM")" \
  1 "carry no disposition"
# Guards, green before and after: an unchanged second read reaches the same verdict, and a
# disposition present in both still answers.
run_case "an unchanged second read reaches the same verdict" \
  "$(wrap8 "$SUITE" "$COV_C,$LATE_CLAIM,$LATE_DISP" "$COV_C,$LATE_CLAIM,$LATE_DISP")" \
  0 "HEAD COVERED"

echo "== finding 35: reviews and threads are consistency-checked too =="
# Comments are re-read and re-judged; reviews and threads are re-read and COMPARED, because
# re-paging every thread body would double the cost of the slowest part of the run. Either
# one moving means the reading was taken from data that has since changed, so it blocks.
# Both are live-path properties, so both go through the gh shim: with only one read of each,
# the second file is never fetched and the run reports CLEAR.
mkdir -p "$TMP/bin"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/comments.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/comments2.json"
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/threads.json"
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":false,"comments":{"totalCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"reviewer"},"path":"a.ts","line":1,"body":"P1: this landed while the gate was paging"}]}}]}}}}}' > "$TMP/threads2.json"
review_payload() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"prcommits":{"totalCount":1,"nodes":[{"commit":{"oid":"deadbeef"}}]},"reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"reviewer"},"state":"COMMENTED","submittedAt":"2026-01-02T01:00:00Z","body":%s,"commit":{"oid":"deadbeef"}}]}}}}}' "$1"; }
review_payload '""' > "$TMP/reviews.json"
review_payload '"P1: this landed while the gate was paging"' > "$TMP/reviews2.json"
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
nth() { local f=$GATE_TEST_DIR/$1.count n; n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); printf '%s' "$n" > "$f"; printf '%s' "$n"; }
case "$*" in
  *"commits(first"*)   cat "$GATE_TEST_DIR/prcommits.json" ;;
  *reviewThreads*)
    if [ "$(nth threads)" -le 1 ]; then cat "$GATE_TEST_DIR/threads.json"; else cat "$GATE_TEST_DIR/${GATE_TEST_THREADS2:-threads}.json"; fi ;;
  *"reviews(first"*)
    if [ "$(nth reviews)" -le 1 ]; then cat "$GATE_TEST_DIR/reviews.json"; else cat "$GATE_TEST_DIR/${GATE_TEST_REVIEWS2:-reviews}.json"; fi ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq
esac
SHIM
printf '%s' "$PRCOMMITS_LIVE" > "$TMP/prcommits.json"
chmod +x "$TMP/bin/gh"
live_case() {
  local name=$1 want_rc=$2 want_txt=$3 out rc
  rm -f "$TMP/threads.count" "$TMP/reviews.count"
  out=$(GATE_TEST_DIR="$TMP" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
  if [ "$rc" = "$want_rc" ] && grep -qF "$want_txt" <<<"$out"; then
    echo "  PASS  $name"; pass=$((pass+1))
  else
    echo "  FAIL  $name (rc=$rc want=$want_rc)"; sed 's/^/          /' <<<"$out" | head -4; fail=$((fail+1))
  fi
}
GATE_TEST_REVIEWS2=reviews2 live_case "a review body edited between the two reads blocks" 2 "reviews on this PR changed"
GATE_TEST_THREADS2=threads2 live_case "a thread opened between the two reads blocks" 2 "review threads on this PR changed"
# Guard, green before and after: unchanged reviews and threads reach a verdict.
live_case "unchanged reviews and threads still reach a verdict" 0 "CLEAR"

echo "== finding 36: a comment is dated by its last edit, not by its creation =="
# Comments are mutable and GitHub keeps createdAt fixed across edits. Ordering claims and
# answers by createdAt therefore reads the CURRENT body against the ORIGINAL time: a comment
# opened Jan 1, edited Jan 3 to add a defect, counted as answered by a Jan 2 disposition.
# updatedAt dates the body the gate is actually reading, and is validated like createdAt.
run_case "a comment edited after its answer is not answered by it" \
  "$(wrap4 me '[]' '[]' "[$(cmt reviewer User 2026-01-01T00:00:00Z '"P1: this drops the last row"' 2026-01-03T00:00:00Z),$(cmt me User 2026-01-02T00:00:00Z '"RED-VERIFIED: tests/row.rs"')]")" \
  1 "carry no disposition"
run_case "a disposition edited after the claim answers it" \
  "$(wrap4 me '[]' '[]' "[$(cmt reviewer User 2026-01-02T00:00:00Z '"P1: this drops the last row"'),$(cmt me User 2026-01-01T00:00:00Z '"RED-VERIFIED: tests/row.rs"' 2026-01-03T00:00:00Z)]")" \
  0 "CLEAR"
run_case "a PR comment whose updatedAt will not parse blocks" \
  "$(wrap4 me '[]' '[]' "[$(cmt reviewer User 2026-01-02T00:30:00Z '"P1: this drops the last row"' 'a while ago')]")" \
  2 "no parsable updatedAt"
# Guards, green before and after: an unedited comment orders the same either way, and an
# edit that lands before the answer is still answered.
run_case "a comment edited before its answer is still answered" \
  "$(wrap4 me '[]' '[]' "[$(cmt reviewer User 2026-01-01T00:00:00Z '"P1: this drops the last row"' 2026-01-02T00:00:00Z),$(cmt me User 2026-01-03T00:00:00Z '"RED-VERIFIED: tests/row.rs"')]")" \
  0 "CLEAR"

echo "== finding 37: a seven-character prefix is not a commit identity =="
# Coverage matched the sha a bot named against the head with `startswith`, so any commit
# whose abbreviation prefixes the head could cover it: a result for the OLD head, named as
# `deadbee`, certified the NEW head `deadbeef`. An abbreviation now has to resolve to
# exactly one of the PR's commits and that commit has to be the head; a full sha still
# matches by identity.
# deadbee abbreviates both of these, so on this PR it names neither.
PRCOMMITS_AMBIG='{"totalCount":2,"nodes":[{"commit":{"oid":"deadbee0"}},{"commit":{"oid":"deadbeef"}}]}'
# The head is not in this list, so deadbee resolves to a commit that is not the head.
PRCOMMITS_OTHER='{"totalCount":1,"nodes":[{"commit":{"oid":"deadbee0"}}]}'
run_case "a summary row naming a prefix two PR commits share is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-01-02T01:00:00.123456Z deadbee)")" 2026-01-02T01:00:05Z),$ANSWER" "" "$PRCOMMITS_AMBIG")" \
  1 "UNREVIEWED HEAD"
run_case "a codex verdict naming a prefix two PR commits share is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")" "" "$PRCOMMITS_AMBIG")" \
  1 "UNREVIEWED HEAD"
run_case "an abbreviation that resolves to a commit other than the head is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")" "" "$PRCOMMITS_OTHER")" \
  1 "UNREVIEWED HEAD"
# Guards, green before and after: an abbreviation unique among the PR's commits still
# resolves to the head, and a full sha matches by identity without resolving anything.
run_case "an abbreviation unique among the PR's commits still covers" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")")" \
  0 "HEAD COVERED"
run_case "the full head sha still covers" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"
run_case "an abbreviation of a sibling commit is not coverage" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee0)")" "" "$PRCOMMITS_AMBIG")" \
  1 "UNREVIEWED HEAD"

echo "== finding 38: a date that does not exist is not a date =="
# mktime NORMALISES out-of-range components, so "2026-02-31T01:00:00Z" converts to
# 2026-03-03T01:00:00Z and orders as a real instant three days later. Every ordering site
# then treats it as a timestamp, and a summary row or a check suite carrying one can grant
# coverage. The day is now checked against the month's real length, leap years included.
SUITE_MAR='[{"createdAt":"2026-03-02T00:30:00Z"}]'
run_case "a summary row dated 2026-02-31 is not coverage" \
  "$(wrap8 "$SUITE_MAR" "$(cmt "$CODEX" Bot 2026-03-02T00:20:00Z "$(codex_summary "$(sum_row Completed 2026-02-31T01:00:00.123456Z deadbee)")" 2026-03-02T01:00:05Z),$ANSWER")" \
  1 "UNREVIEWED HEAD"
run_case "a check-suite timestamp of 2026-02-31 blocks" \
  "$(wrap8 '[{"createdAt":"2026-02-31T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2026-03-04T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  2 "check-suite timestamp"
run_case "a check-suite timestamp of 2026-02-29 blocks (2026 is not a leap year)" \
  "$(wrap8 '[{"createdAt":"2026-02-29T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2026-03-04T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  2 "check-suite timestamp"
run_case "a check-suite timestamp of 2026-04-31 blocks (April has 30 days)" \
  "$(wrap8 '[{"createdAt":"2026-04-31T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2026-05-04T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  2 "check-suite timestamp"
run_case "a review submittedAt of 2026-02-31 blocks" \
  "$(wrap '[]' '[{"author":{"login":"x"},"state":"COMMENTED","submittedAt":"2026-02-31T00:00:00Z","body":"P1: this drops the last row"}]')" \
  2 "no parsable submittedAt"
# Guards, green before and after: real dates still parse, including a leap day.
run_case "2028-02-29 is a real date and still orders" \
  "$(wrap8 '[{"createdAt":"2028-02-29T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2028-02-29T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"
run_case "2026-02-28 is a real date and still orders" \
  "$(wrap8 '[{"createdAt":"2026-02-28T00:30:00Z"}]' "$(cmt "$CODEX" Bot 2026-02-28T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
  0 "HEAD COVERED"

echo "== regression: the original behaviours still hold =="
run_case "unresolved thread blocks" \
  "$(wrap '[{"isResolved":false,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"something"}]}}]')" \
  1 "UNRESOLVED"
# This case expected CLEAR until finding 29: a payload with no threads, no reviews and no
# head names nothing this gate can certify as reviewed, so it is BLOCKED, not CLEAR. The
# literal is what wrap used to build before it put the head on a covered commit.
run_case "no threads, no reviews and no head is BLOCKED, not CLEAR (was the fail-open)" \
  '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},"reviews":{"nodes":[]}}}}}' \
  2 "BLOCKED"

echo "== finding 19: a 128 KiB thread body must not kill the live-path merge =="
# fetch_payload's final jq used --argjson, putting each array in ONE argv string;
# Linux caps a single argv string at MAX_ARG_STRLEN (128 KiB), so any real PR whose
# accumulated bodies pass that died with "Argument list too long", payload came back
# empty, and the gate fail-closed FOREVER on exactly the big PRs it exists for.
# Reached through the live path with a gh shim, since --from-file skips the merge.
mkdir -p "$TMP/bin"
big_body=$(printf 'x%.0s' $(seq 1 200000))
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":true,"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"databaseId":1,"author":{"login":"reviewer"},"path":"a.ts","line":1,"createdAt":"2026-01-01T00:00:00Z","body":"%s"},{"databaseId":2,"author":{"login":"me"},"path":"a.ts","line":1,"createdAt":"2026-01-02T00:00:00Z","body":"NOT-A-DEFECT: fixture padding, not a finding"}]}}]}}}}}' "$big_body" > "$TMP/threads.json"
printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"reviewer"},"state":"COMMENTED","submittedAt":"2026-01-02T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]}}}}}' > "$TMP/reviews.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/comments.json"
# Quoted delimiter: nothing in the shim expands at write time; GATE_TEST_DIR
# arrives via the environment when the gate execs gh.
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
case "$*" in
  *"commits(first"*)   cat "$GATE_TEST_DIR/prcommits.json" ;;
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq; apply it here
esac
SHIM
printf '%s' "$PRCOMMITS_LIVE" > "$TMP/prcommits.json"
chmod +x "$TMP/bin/gh"
out=$(GATE_TEST_DIR="$TMP" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
if [ "$rc" = 0 ] && grep -qF "CLEAR" <<<"$out" && ! grep -qi "argument list too long" <<<"$out"; then
  echo "  PASS  oversized thread body evaluates instead of dying on argv"; pass=$((pass+1))
else
  echo "  FAIL  oversized thread body evaluates instead of dying on argv (rc=$rc)"
  sed 's/^/          /' <<<"$out" | head -4
  fail=$((fail+1))
fi

echo "== finding 20: an OVERSIZED thread's paged comments ride an fd too =="
# The pagination path rebuilt the thread with --argjson c "$all_comments" — the
# same MAX_ARG_STRLEN hazard as finding 19, reachable only when one thread's
# totalCount exceeds COMMENTS_PAGE_SIZE. Shrunk to 2 here so three comments,
# one of them ~200 KiB, walk the paging loop.
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":true,"comments":{"totalCount":3,"pageInfo":{"hasNextPage":true,"endCursor":"CUR1"},"nodes":[{"databaseId":1,"author":{"login":"reviewer"},"path":"a.ts","line":1,"createdAt":"2026-01-01T00:00:00Z","body":"%s"},{"databaseId":2,"author":{"login":"someone"},"path":"a.ts","line":1,"createdAt":"2026-01-01T01:00:00Z","body":"discussion"}]}}]}}}}}' "$big_body" > "$TMP/threads.json"
printf '{"data":{"node":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"me"},"path":"a.ts","line":1,"body":"NOT-A-DEFECT: fixture padding, not a finding"}]}}}}' > "$TMP/threadpage.json"
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
case "$*" in
  *"commits(first"*)   cat "$GATE_TEST_DIR/prcommits.json" ;;
  *"node(id"*)         cat "$GATE_TEST_DIR/threadpage.json" ;;
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq; apply it here
esac
SHIM
printf '%s' "$PRCOMMITS_LIVE" > "$TMP/prcommits.json"
chmod +x "$TMP/bin/gh"
out=$(COMMENTS_PAGE_SIZE=2 GATE_TEST_DIR="$TMP" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
if [ "$rc" = 0 ] && grep -qF "CLEAR" <<<"$out" && ! grep -qi "argument list too long" <<<"$out"; then
  echo "  PASS  oversized paged thread evaluates instead of dying on argv"; pass=$((pass+1))
else
  echo "  FAIL  oversized paged thread evaluates instead of dying on argv (rc=$rc)"
  sed 's/^/          /' <<<"$out" | head -4
  fail=$((fail+1))
fi

echo "== finding 39: an abbreviation resolves against the WHOLE commit list, or none =="
# The PR's commits arrived as `commits(last: 100)`, and names_head resolves an abbreviated
# sha against that list. Past 100 commits an omitted OLDER commit sharing the head's
# seven-character prefix is invisible: the abbreviation resolves to the head alone, reads
# as unambiguous, and a result issued for the old commit certifies the head. The connection
# is now paged to completion and carries the count of what it should hold, so a list that
# does not account for every commit resolves nothing.
PRCOMMITS_TRUNC='{"totalCount":2,"nodes":[{"commit":{"oid":"deadbeef"}}]}'
PRCOMMITS_NOCOUNT='{"nodes":[{"commit":{"oid":"deadbeef"}}]}'
run_case "a commit list holding fewer commits than it says blocks" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")" "" "$PRCOMMITS_TRUNC")" \
  2 "does not account for every commit"
run_case "a commit list that says nothing about its size blocks" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")" "" "$PRCOMMITS_NOCOUNT")" \
  2 "does not account for every commit"
# Guards, green before and after: a complete list still resolves an abbreviation, and a
# payload with no commit list at all is no universe, so an abbreviation resolves to nothing
# while a full sha still matches by identity.
run_case "a complete commit list still resolves the abbreviation" \
  "$(wrap8 "$SUITE" "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")")" \
  0 "HEAD COVERED"
run_case "no commit list at all resolves no abbreviation" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")]")" \
  1 "UNREVIEWED HEAD"
run_case "no commit list at all still matches a full sha by identity" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")]")" \
  0 "HEAD COVERED"

echo "== finding 39: the LIVE path pages the commit list to completion =="
# --from-file can only prove the completeness check. That the gate FETCHES every commit is
# a property of fetch_payload, so it goes through the gh shim: 150 commits whose oldest
# shares the head's seven-character prefix. One `commits(last: 100)` omits that commit, the
# verdict's `deadbee` resolves to the head alone, and the head reads as covered by a result
# bound to nothing. With both pages fetched it is ambiguous, and it covers nothing.
mkdir -p "$TMP/bin"
HEAD150=deadbeef00000000000000000000000000000000
python3 - "$TMP" "$HEAD150" <<'COMMITS'
import json, sys
tmp, head = sys.argv[1:3]
old = "deadbee0" + "0" * 32                       # shares deadbee with the head
oids = [old] + ["%040x" % (0xaaa0000 + i) for i in range(148)] + [head]
assert len(oids) == 150 and len(set(oids)) == 150
def page(nodes, has_next, cursor):
    return {"data": {"repository": {"pullRequest": {"commits": {
        "totalCount": len(oids),
        "pageInfo": {"hasNextPage": has_next, "endCursor": cursor},
        "nodes": [{"commit": {"oid": o}} for o in nodes]}}}}}
json.dump(page(oids[:100], True, "C1"), open(tmp + "/prcommits.json", "w"))
json.dump(page(oids[100:], False, None), open(tmp + "/prcommits2.json", "w"))
# What `commits(last: 100)` returned: the NEWEST 100, without the old commit that shares
# the head's prefix. An unpaged gate reads this and resolves deadbee to the head alone.
json.dump({"data": {"repository": {"pullRequest": {
    "author": {"login": "me"}, "headRefOid": head,
    "commits": {"nodes": [{"commit": {"committedDate": "2026-01-02T00:00:00Z",
        "checkSuites": {"nodes": [{"createdAt": "2026-01-02T00:30:00Z"}]}}}]},
    "prcommits": {"nodes": [{"commit": {"oid": o}} for o in oids[50:]]},
    "reviews": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": []}}}}},
    open(tmp + "/reviews.json", "w"))
COMMITS
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/threads.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[%s]}}}}}' \
  "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbee)")" > "$TMP/comments.json"
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
case "$*" in
  *"commits(first"*)
    n=$(cat "$GATE_TEST_DIR/pcount" 2>/dev/null || echo 0); n=$((n+1))
    printf '%s' "$n" > "$GATE_TEST_DIR/pcount"
    if [ "$n" -le 1 ]; then cat "$GATE_TEST_DIR/prcommits.json"; else cat "$GATE_TEST_DIR/prcommits2.json"; fi ;;
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf '%s\n' "$GATE_TEST_HEAD" ;;  # the recheck passes --jq
esac
SHIM
chmod +x "$TMP/bin/gh"
rm -f "$TMP/pcount"
out=$(GATE_TEST_DIR="$TMP" GATE_TEST_HEAD="$HEAD150" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
if [ "$rc" = 1 ] && grep -qF "UNREVIEWED HEAD" <<<"$out" && [ "$(cat "$TMP/pcount" 2>/dev/null || echo 0)" -ge 2 ]; then
  echo "  PASS  the live path pages every commit, so a shared prefix stays ambiguous"; pass=$((pass+1))
else
  echo "  FAIL  the live path pages every commit, so a shared prefix stays ambiguous (rc=$rc, commit fetches=$(cat "$TMP/pcount" 2>/dev/null || echo 0))"
  sed 's/^/          /' <<<"$out" | head -4
  fail=$((fail+1))
fi

echo "== finding 40: the thread comparison must read the comment BODIES =="
# The second read of the threads fetched only id, isResolved and comments.totalCount, and
# the verdict is computed from the first read's BODIES. GitHub lets a review comment be
# edited in place, which moves none of those three: a disposition edited mid-run into
# something that disposes of nothing leaves the fingerprint identical, and the gate certifies
# a thread from a reply that no longer exists. Both reads now fetch the comments, paging the
# oversized ones exactly as the first read does, and any difference blocks.
mkdir -p "$TMP/bin"
thread_read() {  # $1 the reply on the thread, $2 totalCount, $3.. the page-1 comments
  local reply=$1
  printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":true,"isOutdated":false,"comments":{"totalCount":2,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"reviewer"},"path":"a.ts","line":1,"originalLine":1,"body":"this drops the last row"},{"author":{"login":"me"},"path":"a.ts","line":1,"originalLine":1,"body":%s}]}}]}}}}}' "$reply"
}
thread_read '"NOT-A-DEFECT: renamed only, no behaviour change"' > "$TMP/threads.json"
thread_read '"Actually the rename changed behaviour, this still drops the row"' > "$TMP/threads2.json"
printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"reviewer"},"state":"COMMENTED","submittedAt":"2026-01-02T01:00:00Z","body":"","commit":{"oid":"deadbeef"}}]}}}}}' > "$TMP/reviews.json"
printf '{"data":{"repository":{"pullRequest":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}' > "$TMP/comments.json"
# An oversized thread: page 1 holds the finding and one line of chatter, page 2 holds the
# reply, and the reply is what gets edited between the reads.
printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T1","isResolved":true,"isOutdated":false,"comments":{"totalCount":3,"pageInfo":{"hasNextPage":true,"endCursor":"CUR1"},"nodes":[{"author":{"login":"reviewer"},"path":"a.ts","line":1,"originalLine":1,"body":"this drops the last row"},{"author":{"login":"onlooker"},"path":"a.ts","line":1,"originalLine":1,"body":"discussion"}]}}]}}}}}' > "$TMP/paged.json"
threadpage() { printf '{"data":{"node":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"author":{"login":"me"},"path":"a.ts","line":1,"originalLine":1,"body":%s}]}}}}' "$1"; }
threadpage '"NOT-A-DEFECT: renamed only, no behaviour change"' > "$TMP/threadpage.json"
threadpage '"Actually the rename changed behaviour, this still drops the row"' > "$TMP/threadpage2.json"
printf '%s' "$PRCOMMITS_LIVE" > "$TMP/prcommits.json"
cat > "$TMP/bin/gh" <<'SHIM'
#!/bin/bash
nth() { local f=$GATE_TEST_DIR/$1.count n; n=$(cat "$f" 2>/dev/null || echo 0); n=$((n+1)); printf '%s' "$n" > "$f"; printf '%s' "$n"; }
case "$*" in
  *"commits(first"*)   cat "$GATE_TEST_DIR/prcommits.json" ;;
  *"node(id"*)
    if [ "$(nth page)" -le 1 ]; then cat "$GATE_TEST_DIR/threadpage.json"
    else cat "$GATE_TEST_DIR/${GATE_TEST_PAGE2:-threadpage}.json"; fi ;;
  *reviewThreads*)
    if [ "$(nth threads)" -le 1 ]; then cat "$GATE_TEST_DIR/${GATE_TEST_THREADS1:-threads}.json"
    else cat "$GATE_TEST_DIR/${GATE_TEST_THREADS2:-${GATE_TEST_THREADS1:-threads}}.json"; fi ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq
esac
SHIM
chmod +x "$TMP/bin/gh"
edit_case() {
  local name=$1 want_rc=$2 want_txt=$3 out rc
  rm -f "$TMP/threads.count" "$TMP/page.count"
  out=$(GATE_TEST_DIR="$TMP" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
  if [ "$rc" = "$want_rc" ] && grep -qF "$want_txt" <<<"$out"; then
    echo "  PASS  $name"; pass=$((pass+1))
  else
    echo "  FAIL  $name (rc=$rc want=$want_rc)"; sed 's/^/          /' <<<"$out" | head -4; fail=$((fail+1))
  fi
}
GATE_TEST_THREADS2=threads2 \
  edit_case "a disposition edited between the two reads blocks" 2 "review threads on this PR changed"
COMMENTS_PAGE_SIZE=2 GATE_TEST_THREADS1=paged GATE_TEST_PAGE2=threadpage2 \
  edit_case "a disposition edited on a comment PAGE blocks" 2 "review threads on this PR changed"
# Guards, green before and after: two identical reads reach a verdict, paged or not.
edit_case "two identical thread reads still reach a verdict" 0 "CLEAR"
COMMENTS_PAGE_SIZE=2 GATE_TEST_THREADS1=paged \
  edit_case "two identical paged thread reads still reach a verdict" 0 "CLEAR"

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
