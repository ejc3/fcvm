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
  "$(wrap3 '[]' '[]' '[{"author":{"login":"codex"},"createdAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops the last row"}]')" \
  1 "BLOCKED"
run_case "disposed top-level PR comment clears" \
  "$(wrap3 '[]' '[]' '[{"author":{"login":"codex"},"createdAt":"2026-01-01T00:00:00Z","body":"P1: this silently drops the last row"},{"author":{"login":"me"},"createdAt":"2026-01-02T00:00:00Z","body":"DISAGREE: the guard above rejects that input"}]')" \
  0 "CLEAR"
run_case "a PR comment can answer a review body" \
  "$(wrap3 '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"P1: drops a row"}]' '[{"author":{"login":"me"},"createdAt":"2026-01-02T00:00:00Z","body":"RED-VERIFIED: row.test.ts"}]')" \
  0 "CLEAR"
run_case "malformed PR comment data exits 2" \
  "$(wrap3 '[]' '[]' '"nonsense"')" 2 "BLOCKED"

echo "== finding 15: only real claimants raise PR-level claims =="
wrap4() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"%s"},"headRefOid":"deadbeef","reviewThreads":{"nodes":%s},"reviews":{"nodes":%s},"comments":{"nodes":%s}}}}}' "$1" "$2" "$(covered "${3:-[]}")" "${4:-[]}"; }
# The command this skill documents — posted by the PR author — is not a finding.
run_case "author's own @codex review comment does not block" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"me","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","body":"@codex review"}]')" \
  0 "CLEAR"
run_case "deploy bot notification does not block" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"vercel","__typename":"Bot"},"createdAt":"2026-01-01T00:00:00Z","body":"Deployment ready at https://preview.example"}]')" \
  0 "CLEAR"
run_case "a human finding in a top-level comment still blocks" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"reviewer","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","body":"P1: this drops the last row"}]')" \
  1 "BLOCKED"
run_case "a reviewing bot's top-level comment still blocks" \
  "$(wrap4 me '[]' '[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":""}]' '[{"author":{"login":"codex","__typename":"Bot"},"createdAt":"2026-01-02T00:00:00Z","body":"P1: this drops the last row"}]')" \
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
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"me","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","body":"P1: this drops records"}]')" \
  1 "BLOCKED"
run_case "a standalone bot finding with no review history is claimable" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"codex","__typename":"Bot"},"createdAt":"2026-01-01T00:00:00Z","body":"P1: this drops the last row"}]')" \
  1 "BLOCKED"
run_case "an APPROVED review body is not a finding" \
  "$(wrap4 me '[]' '[{"author":{"login":"reviewer"},"state":"APPROVED","submittedAt":"2026-01-01T00:00:00Z","body":"LGTM, nice work"}]' '[]')" \
  0 "CLEAR"
run_case "a COMMENTED review body still is" \
  "$(wrap4 me '[]' '[{"author":{"login":"reviewer"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"LGTM, nice work"}]' '[]')" \
  1 "BLOCKED"
run_case "a bare @codex trigger is not a finding" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"anyone","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","body":"@codex review"}]')" \
  0 "CLEAR"
run_case "a finding that merely mentions @codex still blocks" \
  "$(wrap4 me '[]' '[]' '[{"author":{"login":"anyone","__typename":"User"},"createdAt":"2026-01-01T00:00:00Z","body":"@codex review this: P1 it drops rows"}]')" \
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
wrap6() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"},{"createdAt":"2026-01-02T00:31:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":%s}}}}}' "$1"; }
# One top-level PR comment as GraphQL returns it: login, __typename, createdAt, a body
# that is already a JSON string literal, and updatedAt ($5, defaulting to createdAt as
# GitHub does for a comment nobody has edited).
cmt() { printf '{"author":{"login":"%s","__typename":"%s"},"createdAt":"%s","updatedAt":"%s","body":%s}' "$1" "$2" "$3" "${5:-$3}" "$4"; }
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
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]},"comments":{"nodes":[%s]}}}}}' "$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body "$CR_NOTE")")")" \
  0 "CLEAR"
run_case "a verdict dated after the commit but before the head arrived is not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T00:10:00Z "$(codex_body '' deadbeef)")]")" \
  1 "UNREVIEWED HEAD"
run_case "the author saying no findings is not coverage" \
  "$(wrap6 "[$(cmt me User 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")]")" \
  1 "UNREVIEWED HEAD"
run_case "a verdict cannot cover a head that has no check suite (arrival unknown)" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":[%s]}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")")" \
  1 "UNREVIEWED HEAD"
run_case "a no-findings verdict is not a finding that needs a disposition" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]},"comments":{"nodes":[%s]}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body '' deadbeef)")")" \
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
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]},"comments":{"nodes":[%s]}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")")" \
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
wrap7() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"%s","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":%s}}}}}' "$HEAD40" "$1"; }
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
run_case "a codex verdict is coverage when no review objects exist" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[]},"comments":{"nodes":[%s]}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' deadbeef)")")" \
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
wrap8() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":%s}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[]},"comments":{"nodes":[%s]}}}}}' "$1" "$2"; }
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
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq; apply it here
esac
SHIM
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
  *"node(id"*)         cat "$GATE_TEST_DIR/threadpage.json" ;;
  *reviewThreads*)     cat "$GATE_TEST_DIR/threads.json" ;;
  *"reviews(first"*)   cat "$GATE_TEST_DIR/reviews.json" ;;
  *"comments(first"*)  cat "$GATE_TEST_DIR/comments.json" ;;
  *headRefOid*)        printf 'deadbeef\n' ;;  # the recheck passes --jq; apply it here
esac
SHIM
chmod +x "$TMP/bin/gh"
out=$(COMMENTS_PAGE_SIZE=2 GATE_TEST_DIR="$TMP" PATH="$TMP/bin:$PATH" bash "$GATE" 1 2>&1); rc=$?
if [ "$rc" = 0 ] && grep -qF "CLEAR" <<<"$out" && ! grep -qi "argument list too long" <<<"$out"; then
  echo "  PASS  oversized paged thread evaluates instead of dying on argv"; pass=$((pass+1))
else
  echo "  FAIL  oversized paged thread evaluates instead of dying on argv (rc=$rc)"
  sed 's/^/          /' <<<"$out" | head -4
  fail=$((fail+1))
fi

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
