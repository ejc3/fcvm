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
wrap3() { printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":%s},"reviews":{"nodes":%s},"comments":{"nodes":%s}}}}}' "$1" "${2:-[]}" "${3:-[]}"; }
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
wrap4() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"%s"},"reviewThreads":{"nodes":%s},"reviews":{"nodes":%s},"comments":{"nodes":%s}}}}}' "$1" "$2" "${3:-[]}" "${4:-[]}"; }
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
run_case "no reviews at all is not this check's business" \
  "$(wrap5 deadbeef '[]')" 0 "CLEAR"

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
# verdict dated AFTER the head ARRIVED at GitHub covers it. Arrival is the creation of the
# head's first check suite, not the commit's own date: a commit made locally before an
# older head's verdict and pushed afterwards carries a committedDate that predates that
# verdict (codex on #873). The verdict must be the whole comment, in the shape the bot
# actually posts (the builders below copy live comments): a body carrying the phrase plus
# finding text is a finding, claimable and not coverage.
wrap6() { printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"},{"createdAt":"2026-01-02T00:31:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z","body":"","commit":{"oid":"0ldc0mm1t"}}]},"comments":{"nodes":%s}}}}}' "$1"; }
# One top-level PR comment as GraphQL returns it: login, __typename, createdAt, and a body
# that is already a JSON string literal.
cmt() { printf '{"author":{"login":"%s","__typename":"%s"},"createdAt":"%s","body":%s}' "$1" "$2" "$3" "$4"; }
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
run_case "a coderabbit full-review-finished answering a request after arrival covers it" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai full review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  0 "HEAD COVERED"
run_case "a coderabbit incremental review-finished with its note covers it" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body "$CR_NOTE")")]")" \
  0 "HEAD COVERED"
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

echo "== finding 23: a verdict is bound to the head, not merely dated after it =="
# A review of the OLD head that finishes after the new one arrives is dated after arrival
# too. Codex names the commit it reviewed, so that sha must be a prefix of the head.
# CodeRabbit names nothing, so its reply must answer a review request addressed to it and
# posted after arrival, and be its first verdict after that request.
run_case "a codex verdict naming an older commit is not coverage" \
  "$(wrap6 "[$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")]")" \
  1 "UNREVIEWED HEAD"
# Guard, green before and after: a verdict for an older commit is still a verdict, and a
# head covered by a review object does not go BLOCKED over an undisposed stale verdict.
run_case "a codex verdict naming an older commit needs no disposition" \
  "$(printf '{"data":{"repository":{"pullRequest":{"author":{"login":"me"},"headRefOid":"deadbeef","commits":{"nodes":[{"commit":{"committedDate":"2026-01-02T00:00:00Z","checkSuites":{"nodes":[{"createdAt":"2026-01-02T00:30:00Z"}]}}}]},"reviewThreads":{"nodes":[]},"reviews":{"nodes":[{"author":{"login":"codex"},"state":"COMMENTED","submittedAt":"2026-01-03T00:00:00Z","body":"","commit":{"oid":"deadbeef"}}]},"comments":{"nodes":[%s]}}}}}' "$(cmt "$CODEX" Bot 2026-01-02T01:00:00Z "$(codex_body ' Bravo.' abc123def4)")")" \
  0 "CLEAR"
run_case "a coderabbit verdict with no request after arrival is not coverage" \
  "$(wrap6 "[$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  1 "UNREVIEWED HEAD"
run_case "a request before arrival does not bind a verdict after it" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:20:00Z '"@coderabbitai full review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  1 "UNREVIEWED HEAD"
run_case "a request addressed to another bot does not bind a coderabbit verdict" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@codex review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  1 "UNREVIEWED HEAD"
run_case "a verdict before the request does not count, the one after it does" \
  "$(wrap6 "[$(cmt coderabbitai Bot 2026-01-02T00:50:00Z "$(cr_body 'Full review finished.')"),$(cmt me User 2026-01-02T00:55:00Z '"@coderabbitai full review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  0 "HEAD COVERED" "1 no-findings verdict(s)"
run_case "only the first verdict after a request answers it" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@coderabbitai full review"'),$(cmt coderabbitai Bot 2026-01-02T00:50:00Z "$(cr_body 'Full review finished.')"),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  0 "HEAD COVERED" "1 no-findings verdict(s)"
# Guard, green before and after: GitHub logins are case-insensitive, so the mention is too.
run_case "a request that mentions the bot in another case still binds" \
  "$(wrap6 "[$(cmt me User 2026-01-02T00:40:00Z '"@CodeRabbitAI review"'),$(cmt coderabbitai Bot 2026-01-02T01:00:00Z "$(cr_body 'Full review finished.')")]")" \
  0 "HEAD COVERED"
run_case "@coderabbitai full review is a trigger, not a finding" \
  "$(wrap4 me '[]' '[]' "[$(cmt anyone User 2026-01-01T00:00:00Z '"@coderabbitai full review"')]")" \
  0 "CLEAR"

echo "== regression: the original behaviours still hold =="
run_case "unresolved thread blocks" \
  "$(wrap '[{"isResolved":false,"comments":{"nodes":[{"author":{"login":"x"},"path":"a.ts","line":1,"body":"something"}]}}]')" \
  1 "UNRESOLVED"
run_case "no threads and no reviews is clear" "$(wrap '[]')" 0 "CLEAR"

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
