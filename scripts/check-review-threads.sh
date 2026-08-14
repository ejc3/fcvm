#!/bin/bash
# Fail while a PR has UNRESOLVED inline review threads.
#
# This is the enforcement half of the review rules in CLAUDE.md.
# A doc that says "read the inline findings before merging" cannot fire. This can.
#
# It exists because CI state says nothing about whether a review was ANSWERED.
# Ported from ejc3/fcvm, where a PR sat at 15 green checks / 0 failures /
# MERGEABLE with 19 unresolved inline findings, and another merged carrying four
# unread Major findings behind a green `CodeRabbit pass`. Codex reviews this repo,
# so the same shape is available here the moment a review has findings.
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

# The GitHub page size for comments within one thread. Only tests override it:
# reproducing an oversized thread honestly costs 100+ comment-creation API calls,
# which GitHub secondary-rate-limits into silent partial failure. Shrinking the page
# makes the same code path reachable with three comments.
COMMENTS_PAGE_SIZE=${COMMENTS_PAGE_SIZE:-100}

fetch_payload() {
  local pr=$1 cursor=null threads='[]' reviews='[]'
  while :; do
    local after="" resp
    # `\"` here, NOT `\\\"`: the latter puts a literal backslash into the GraphQL
    # argument and the query fails to parse. Only reachable on page 2+, which is why
    # single-page fixtures never caught it.
    [ "$cursor" != "null" ] && after=", after: \"$cursor\""
    resp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            reviews(first: 100) { nodes { author { login } state body } }
            reviewThreads(first: 100$after) {
              pageInfo { hasNextPage endCursor }
              nodes {
                id isResolved isOutdated
                comments(first: $COMMENTS_PAGE_SIZE) { totalCount nodes { author { login } path line originalLine body } }
              } } } } }" 2>/dev/null) || return 1

    # A wrong PR number, a renamed repo, or a permissions problem all return a NULL
    # pullRequest with no GraphQL error. Coalescing that to [] turned "I could not find
    # this PR" into "this PR has nothing to answer" and exited CLEAR. A gate that cannot
    # find its subject must block, never bless it.
    if [ "$(jq -r '.data.repository.pullRequest // "null"' <<<"$resp")" = "null" ]; then
      echo "verdict: BLOCKED — no pull request #$pr in $REPO_OWNER/$REPO_NAME (or it is" >&2
      echo "not visible to this token). Refusing to report CLEAR for a PR never read." >&2
      return 2
    fi

    threads=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviewThreads.nodes // [])' \
          <(echo "$threads") <(echo "$resp"))
    reviews=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviews.nodes // [])' \
          <(echo "$reviews") <(echo "$resp"))
    [ "$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage' <<<"$resp")" = "true" ] || break
    cursor=$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.endCursor' <<<"$resp")
  done

  # A disposition is a REPLY, so it lands at the END of a long thread. comments(first:100)
  # silently truncated exactly there: a thread with 100+ comments reported UNPROVEN even
  # though its RED-VERIFIED reply existed, with no way for a later reply to become
  # visible. Re-fetch the tail of any oversized thread and merge it in.
  local oversized
  oversized=$(jq -r --argjson page "$COMMENTS_PAGE_SIZE" \
  '[.[] | select((.comments.totalCount // 0) > $page) | .id] | .[]' <<<"$threads")
  local tid
  for tid in $oversized; do
    local tail
    tail=$(gh api graphql -f query="
      { node(id: \"$tid\") { ... on PullRequestReviewThread {
          comments(last: $COMMENTS_PAGE_SIZE) { nodes { author { login } path line originalLine body } } } } }" 2>/dev/null) || continue
    threads=$(jq --arg id "$tid" --argjson tail "$(jq '.data.node.comments.nodes // []' <<<"$tail")" \
      'def dedupe_stable: reduce .[] as $c ([]; if any(.[]; . == $c) then . else . + [$c] end);
       map(if .id == $id
           then .comments.nodes = ((.comments.nodes + $tail) | dedupe_stable)
           else . end)' \
      <<<"$threads")
  done

  jq -n --argjson t "$threads" --argjson r "$reviews" \
     '{data:{repository:{pullRequest:{reviewThreads:{nodes:$t}, reviews:{nodes:$r}}}}}'
}

if [ "${1:-}" = "--from-file" ]; then
  payload=$(cat "${2:?need a json file}")
else
  pr=${1:?usage: check-review-threads.sh <pr-number> | --from-file <json>}
  payload=$(fetch_payload "$pr") || exit 2
fi
threads=$(jq '.data.repository.pullRequest.reviewThreads.nodes' <<<"$payload" 2>/dev/null)
reviews=$(jq '.data.repository.pullRequest.reviews.nodes // []' <<<"$payload" 2>/dev/null)

# Prove the payload is what we think before counting it. `jq` emits `null` for a missing
# path and an empty string on a parse error, and `[ "" -gt 0 ]` is a shell error, not a
# block — so malformed input previously slid through to a CLEAR verdict. Same failure
# shape as the missing-jq case: a gate that cannot read its input must not bless it.
if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$threads"; then
  echo "verdict: BLOCKED — review-thread data is not an array; refusing to judge input" >&2
  echo "this gate could not parse. Re-run, or re-capture the fixture." >&2
  exit 2
fi
# has("isResolved") was presence-only: `null` and the STRING "true" both satisfied it,
# then matched neither the resolved nor the unresolved selector and vanished from both
# counts into a CLEAR verdict. Require the actual type.
if ! jq -e 'all((.isResolved | type) == "boolean" and (.comments.nodes | type == "array"))' \
     >/dev/null 2>&1 <<<"$threads"; then
  echo "verdict: BLOCKED — a thread has a non-boolean isResolved or a bad comments array." >&2
  echo "The only field that means 'resolved' is isResolved; a null or string value is not" >&2
  echo "an answer, and a thread that matches neither selector must not slip through." >&2
  exit 2
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

# EVERY resolved thread must carry an explicit disposition reply. The previous version
# only demanded proof when a regex spotted defect wording, which failed both ways:
#   - ordinary phrasing slipped past it ("this drops the final game", "this omits an
#     event" match none of panic|crash|leak|...), so a real defect closed on an assertion;
#   - and a defect claim that was WRONG could never be closed at all, because only a RED
#     marker counted, leaving no way to record a reasoned disagreement.
# Guessing which findings are defects is the wrong job. Requiring an answer to each one
# is the right one, and it is regex-free.
#
#   RED-VERIFIED: <test>    a defect claim, closed by a test watched failing without the fix
#   NOT-A-DEFECT: <reason>  not a defect (naming, docs, style) — say what you did
#   DISAGREE: <reason>      a defect claim you are rejecting, with the reasoning
#
# `[^[:space:]]` after the colon is required: a bare "RED-VERIFIED:" — or a comment
# merely ASKING someone for proof — previously satisfied a plain substring test.
disposition_re='(RED-VERIFIED|NOT-A-DEFECT|DISAGREE):[[:space:]]*[^[:space:]]'

undisposed=$(jq -r --arg re "$disposition_re" '
  [ .[] | select(.isResolved == true)
        | select(([.comments.nodes[1:][].body] | join(" ") | test($re)) | not) ]
  | length' <<<"$threads" 2>/dev/null || echo 0)

if [ "${undisposed:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$disposition_re" '.[] | select(.isResolved == true)
    | select(([.comments.nodes[1:][].body] | join(" ") | test($re)) | not)
    | .comments.nodes[0]
    | "  UNDISPOSED \(.author.login)  \(.path):\(.line // .originalLine // "?")\n    \(.body | split("\n")[0][0:150])"' \
    <<<"$threads"
  echo
  echo "verdict: BLOCKED — $undisposed resolved thread(s) carry no disposition reply."
  echo "Reply in the thread with one of:"
  echo "  RED-VERIFIED: <test>    after watching that test fail with the fix reverted"
  echo "  NOT-A-DEFECT: <reason>  for naming/docs/style findings"
  echo "  DISAGREE: <reason>      to reject a defect claim, with the reasoning"
  echo "Resolving without replying closes a finding without answering it."
  rc=1
fi

# A defect claim can also arrive in a PR-LEVEL REVIEW BODY, which is not a thread and has
# no isResolved. Those never appeared in reviewThreads at all, so a review whose entire
# content was "P1: this silently drops events" left the gate reporting 0 threads / CLEAR.
# Require an explicit acknowledgement comment for each non-empty review body.
# Count only bodies that are NOT themselves acknowledgements. An ACK reply carries a
# body too, and counting it as one more thing needing acknowledgement made a correctly
# acked PR block forever — the fail-closed-permanently twin of the bug this replaced.
unacked=$(jq -r '
  ([ .[] | select((.body // "") | test("[^[:space:]]"))
          | select((.body // "") | test("REVIEW-ACK:") | not) ] | length) as $needing
  | ([ .[] | select((.body // "") | test("REVIEW-ACK:[[:space:]]*[^[:space:]]")) ] | length) as $acks
  | if $acks > 0 then 0 else $needing end' <<<"$reviews" 2>/dev/null || echo 0)

if [ "${unacked:-0}" -gt 0 ]; then
  echo
  jq -r '.[] | select((.body // "") | test("[^[:space:]]"))
    | select((.body // "") | test("REVIEW-ACK:") | not)
    | "  UNACKED    \(.author.login) (\(.state))\n    \(.body | split("\n")[0][0:150])"' <<<"$reviews"
  echo
  echo "verdict: BLOCKED — $unacked PR-level review body/bodies not acknowledged."
  echo "A finding in a review body is not a thread and cannot be 'resolved'. Reply on the"
  echo "PR with 'REVIEW-ACK: <what you did>' once you have read and answered it."
  rc=1
fi

if [ "$rc" -eq 0 ]; then
  echo "verdict: CLEAR — every thread resolved and disposed, every review body acknowledged."
fi
exit $rc
