#!/bin/bash
# Fail while a PR has UNRESOLVED inline review threads.
#
# This is the enforcement half of the review-comment rules in .claude/CLAUDE.md.
# A doc that says "read the inline findings before merging" cannot fire. This can.
#
# It exists because on 2026-08-08 a PR sat at 15 green checks / 0 failures /
# MERGEABLE with 19 unresolved inline findings, and another merged carrying four
# unread Major findings behind a green `CodeRabbit pass`. CI state says nothing
# about whether a human or bot review was answered.
#
# Why GraphQL and not `created_at` heuristics: an earlier version of the docs
# said "comment older than your fix commit => already addressed". That is wrong
# whenever a fix addresses SOME of several findings — every older comment still
# predates it, including the unfixed ones, so unfixed blockers get classified as
# handled. `isResolved` is the only field that means resolved.
#
# Usage:
#   check-review-threads.sh <pr-number>          # query GitHub
#   check-review-threads.sh --from-file <json>   # parse a saved response (tests)
set -uo pipefail

# A GATE MUST FAIL CLOSED. Without this check the script degraded silently when `jq`
# was absent (it is not in the CI container): every `jq` call errored to stderr, the
# counts came back empty, and it printed "verdict: CLEAR ... exit 0" — a merge gate
# waving everything through precisely because it could not run. That is strictly worse
# than no gate, because it looks like one.
for tool in jq gh; do
  # gh is only needed for the live query; --from-file parsing needs jq alone.
  if [ "$tool" = "gh" ] && [ "${1:-}" = "--from-file" ]; then continue; fi
  command -v "$tool" >/dev/null 2>&1 || {
    echo "verdict: BLOCKED — '$tool' is not installed, so this gate cannot evaluate" >&2
    echo "review threads. Refusing to report CLEAR for a check that did not run." >&2
    exit 2
  }
done

REPO_OWNER=${REPO_OWNER:-ejc3}
REPO_NAME=${REPO_NAME:-fcvm}

fetch_threads() {
  local pr=$1 cursor=null all='[]'
  while :; do
    local after="" resp
    # `\"` here, NOT `\\\"`: the latter puts a literal backslash into the GraphQL
    # argument and the query fails to parse. Only reachable on page 2+, which is why
    # single-page fixtures never caught it.
    [ "$cursor" != "null" ] && after=", after: \"$cursor\""
    resp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            reviewThreads(first: 100$after) {
              pageInfo { hasNextPage endCursor }
              nodes {
                isResolved isOutdated
                comments(first: 100) { nodes { author { login } path line originalLine body } }
              } } } } }" 2>/dev/null) || return 1
    all=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviewThreads.nodes // [])' \
          <(echo "$all") <(echo "$resp"))
    [ "$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage' <<<"$resp")" = "true" ] || break
    cursor=$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.endCursor' <<<"$resp")
  done
  echo "$all"
}

if [ "${1:-}" = "--from-file" ]; then
  threads=$(jq '.data.repository.pullRequest.reviewThreads.nodes' "${2:?need a json file}")
else
  pr=${1:?usage: check-review-threads.sh <pr-number> | --from-file <json>}
  threads=$(fetch_threads "$pr") || { echo "could not query review threads for #$pr" >&2; exit 2; }
fi

total=$(jq 'length' <<<"$threads")
unresolved=$(jq '[.[] | select(.isResolved == false)] | length' <<<"$threads")

echo "review threads: $total total, $unresolved unresolved"
rc=0

if [ "$unresolved" -gt 0 ]; then
  echo
  jq -r '.[] | select(.isResolved == false) | .comments.nodes[0] |
    "  UNRESOLVED  \(.author.login)  \(.path):\(.line // .originalLine // "?")\n    \(.body | split("\n")[0][0:150])"' \
    <<<"$threads"
  echo
  echo "verdict: BLOCKED — resolve or explicitly answer each thread before merging."
  echo "An unresolved thread is a finding nobody has answered, not a finding that is wrong."
  rc=1
fi

# A finding that claims BROKEN BEHAVIOUR cannot be closed by asserting it is
# fixed. Closing it requires a test that fails without the fix — otherwise the
# only evidence the bug is gone is the same judgement that shipped it.
#
# Mark such a resolution by replying in the thread with:
#     RED-VERIFIED: <test name or path>
# and having actually watched it fail with the fix reverted.
#
# `defect_re` deliberately errs toward over-matching. A false positive costs one
# reply naming the test; a false negative closes a real bug on an assertion.
defect_re='panic|crash|hang|deadlock|leak|corrupt|race|overflow|truncat|silently|wrong|incorrect|breaks|broken|fails|unsound|use-after|out of bounds|off-by-one'

needs_proof=$(jq -r --arg re "$defect_re" '
  [ .[] | select(.isResolved == true)
        | select([.comments.nodes[].body] | join(" ") | test($re; "i"))
        | select(([.comments.nodes[1:][].body] | join(" ") | test("RED-VERIFIED:"; "i")) | not) ]
  | length' <<<"$threads" 2>/dev/null || echo 0)

if [ "${needs_proof:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$defect_re" '.[] | select(.isResolved == true)
    | select([.comments.nodes[].body] | join(" ") | test($re; "i"))
    | select(([.comments.nodes[1:][].body] | join(" ") | test("RED-VERIFIED:"; "i")) | not)
    | .comments.nodes[0]
    | "  UNPROVEN   \(.author.login)  \(.path):\(.line // .originalLine // "?")\n    \(.body | split("\n")[0][0:150])"' \
    <<<"$threads"
  echo
  echo "verdict: BLOCKED — $needs_proof resolved thread(s) describe broken behaviour with no test cited."
  echo "Reply in the thread with 'RED-VERIFIED: <test>' after watching that test fail"
  echo "with the fix reverted. A test that has never been red proves nothing."
  rc=1
fi

if [ "$rc" -eq 0 ]; then
  echo "verdict: CLEAR — every thread resolved, every defect claim backed by a red-verified test."
fi
exit $rc
