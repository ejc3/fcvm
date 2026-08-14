#!/bin/bash
# Fail while a PR has UNRESOLVED inline review threads.
#
# This is the enforcement half of the review-comment rules in .claude/CLAUDE.md.
# A doc that says "read the inline findings before merging" cannot fire. This can.
#
# It exists because on 2026-08-08 a PR here sat at 15 green checks / 0 failures /
# MERGEABLE with 19 unresolved inline findings, and another merged carrying four
# unread Major findings behind a green `CodeRabbit pass`. CI state says nothing
# about whether a human or bot review was answered.
#
# A port of this script runs in dolphin-labs-hq/dolphin-labs; fixes have flowed
# both ways. Keep them in sync — they have drifted once already.
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
# Same idea for the reviews connection, so its paging is reachable on an ordinary PR
# instead of one with 100+ submitted reviews.
REVIEWS_PAGE_SIZE=${REVIEWS_PAGE_SIZE:-100}

fetch_payload() {
  local pr=$1 cursor=null threads='[]' reviews='[]'

  # Reviews are their OWN connection and must be paged on their OWN cursor. They used to
  # ride along inside the reviewThreads loop, which was wrong twice over: past 100 reviews
  # a defect claim in review 101 was never fetched at all (CLEAR with nothing answered),
  # and on a PR with 2+ pages of THREADS the same first review page was appended once per
  # iteration, duplicating every claim.
  local rcursor=null rafter="" rresp
  while :; do
    [ "$rcursor" != "null" ] && rafter=", after: \"$rcursor\""
    rresp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            reviews(first: $REVIEWS_PAGE_SIZE$rafter) {
              pageInfo { hasNextPage endCursor }
              nodes { author { login } state body submittedAt }
            } } } }" 2>/dev/null) || return 1
    if [ "$(jq -r '.data.repository.pullRequest // "null"' <<<"$rresp")" = "null" ]; then
      echo "verdict: BLOCKED — no pull request #$pr in $REPO_OWNER/$REPO_NAME (or it is" >&2
      echo "not visible to this token). Refusing to report CLEAR for a PR never read." >&2
      return 2
    fi
    reviews=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviews.nodes // [])' \
          <(echo "$reviews") <(echo "$rresp"))
    [ "$(jq -r '.data.repository.pullRequest.reviews.pageInfo.hasNextPage' <<<"$rresp")" = "true" ] || break
    rcursor=$(jq -r '.data.repository.pullRequest.reviews.pageInfo.endCursor' <<<"$rresp")
  done

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
                id isResolved isOutdated
                comments(first: $COMMENTS_PAGE_SIZE) {
                  totalCount
                  pageInfo { hasNextPage endCursor }
                  nodes { author { login } path line originalLine body }
                }
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
    [ "$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage' <<<"$resp")" = "true" ] || break
    cursor=$(jq -r '.data.repository.pullRequest.reviewThreads.pageInfo.endCursor' <<<"$resp")
  done

  # A thread's comments are a paged connection like any other, and a disposition is a
  # REPLY, so it lands at the far end of a long one. Two earlier versions got this wrong:
  #   - comments(first: N) alone truncated the tail, so a disposed thread reported
  #     UNDISPOSED with no way for a later reply to ever become visible;
  #   - then first:N + last:N covered both ends but never the MIDDLE, so on a thread
  #     longer than 2N a disposition in between was still invisible.
  # Page it properly instead of approximating. Pages are appended in order, so comment
  # order is preserved — which matters, because the disposition scan treats nodes[0] as
  # the original finding. (An interim version deduped with unique_by(.body), which SORTS,
  # and a reply sorting before the finding silently became nodes[0].)
  local oversized tid
  oversized=$(jq -r --argjson page "$COMMENTS_PAGE_SIZE" \
  '[.[] | select((.comments.totalCount // 0) > $page) | .id] | .[]' <<<"$threads")
  for tid in $oversized; do
    local ccursor cresp all_comments
    ccursor=$(jq -r --arg id "$tid" \
      '.[] | select(.id == $id) | .comments.pageInfo.endCursor // "null"' <<<"$threads")
    all_comments=$(jq -c --arg id "$tid" \
      '.[] | select(.id == $id) | .comments.nodes' <<<"$threads")
    while [ "$ccursor" != "null" ] && [ -n "$ccursor" ]; do
      cresp=$(gh api graphql -f query="
        { node(id: \"$tid\") { ... on PullRequestReviewThread {
            comments(first: $COMMENTS_PAGE_SIZE, after: \"$ccursor\") {
              pageInfo { hasNextPage endCursor }
              nodes { author { login } path line originalLine body } } } } }" 2>/dev/null) || break
      all_comments=$(jq -c -s '.[0] + (.[1].data.node.comments.nodes // [])' \
        <(echo "$all_comments") <(echo "$cresp"))
      if [ "$(jq -r '.data.node.comments.pageInfo.hasNextPage' <<<"$cresp")" = "true" ]; then
        ccursor=$(jq -r '.data.node.comments.pageInfo.endCursor' <<<"$cresp")
      else
        ccursor=null
      fi
    done
    threads=$(jq --arg id "$tid" --argjson c "$all_comments" \
      'map(if .id == $id then .comments.nodes = $c else . end)' <<<"$threads")
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
# Review data gets the SAME scrutiny as thread data. It did not, originally: `reviews` was
# added in the commit that hardened `threads` and simply missed the validation, so a
# malformed reviews array made the later jq fail, hit its `|| echo 0` fallback, and
# reported CLEAR. Half-applied hardening is how a gate ends up trusted and wrong.
if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$reviews"; then
  echo "verdict: BLOCKED — review data is not an array; refusing to judge input this" >&2
  echo "gate could not parse. Re-run, or re-capture the fixture." >&2
  exit 2
fi
# A body must be text, and any non-empty body must carry a timestamp — answers are matched
# to claims by time below, and an unordered claim cannot be shown to have been answered.
if ! jq -e 'all(((.body // "") | type) == "string")' >/dev/null 2>&1 <<<"$reviews"; then
  echo "verdict: BLOCKED — a review body is not text; refusing to judge it." >&2
  exit 2
fi
if ! jq -e 'all(((.body // "") | test("[^[:space:]]") | not) or ((.submittedAt | type) == "string"))' \
     >/dev/null 2>&1 <<<"$reviews"; then
  echo "verdict: BLOCKED — a non-empty review body has no submittedAt timestamp, so this" >&2
  echo "gate cannot tell whether the answer came before or after the claim." >&2
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
# A disposition must OPEN a reply of its own. Two weaker versions shipped first:
#   - a plain substring test, which a bare "RED-VERIFIED:" satisfied;
#   - then a non-empty-suffix test applied to every reply JOINED into one string, which
#     "Please add RED-VERIFIED: <test> before resolving" satisfied — counting the person
#     DEMANDING evidence as the person supplying it.
# `\A` (start of string, not start of line) anchors it to a deliberate answer. Prose with
# a marker buried in the middle does not count, in either direction.
disposition_re='\A[[:space:]]*(RED-VERIFIED|NOT-A-DEFECT|DISAGREE):[[:space:]]*[^[:space:]]'

# No `|| echo 0` here or below. That fallback is how a jq failure became "nothing to
# answer" — the gate reporting CLEAR precisely because it could not evaluate.
undisposed=$(jq -r --arg re "$disposition_re" '
  [ .[] | select(.isResolved == true)
        | select(any(.comments.nodes[1:][]; (.body // "") | test($re)) | not) ]
  | length' <<<"$threads") || {
  echo "verdict: BLOCKED — could not evaluate thread dispositions." >&2; exit 2; }

if [ "${undisposed:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$disposition_re" '.[] | select(.isResolved == true)
    | select(any(.comments.nodes[1:][]; (.body // "") | test($re)) | not)
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
#
# Review bodies answer to the SAME vocabulary as inline findings. They used to accept a
# bare "REVIEW-ACK: read", which meant an identical claim was adjudicated or waved through
# depending only on where the reviewer happened to click. A weaker rule reachable by
# accident is not a rule.
#
# And an answer only answers what came BEFORE it. Counting dispositions in aggregate ("any
# disposition exists => nothing outstanding") meant a finding posted after an earlier round
# was closed was silently treated as answered. Each claim needs a disposition NEWER than
# itself; ISO-8601 timestamps compare lexicographically, and the validation above
# guarantees every non-empty body has one.
unanswered=$(jq -r --arg re "$disposition_re" '
  [ .[] | select((.body // "") | test($re)) | .submittedAt ] as $answers
  | [ .[] | select((.body // "") | test("[^[:space:]]"))
          | select(((.body // "") | test($re)) | not)
          | . as $claim
          | select([ $answers[] | select(. > $claim.submittedAt) ] | length == 0) ]
  | length' <<<"$reviews") || {
  echo "verdict: BLOCKED — could not evaluate review bodies." >&2; exit 2; }

if [ "${unanswered:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$disposition_re" '
    [ .[] | select((.body // "") | test($re)) | .submittedAt ] as $answers
    | .[] | select((.body // "") | test("[^[:space:]]"))
    | select(((.body // "") | test($re)) | not)
    | . as $claim
    | select([ $answers[] | select(. > $claim.submittedAt) ] | length == 0)
    | "  UNANSWERED \(.author.login) (\(.state))\n    \(.body | split("\n")[0][0:150])"' <<<"$reviews"
  echo
  echo "verdict: BLOCKED — $unanswered PR-level review body/bodies carry no disposition."
  echo "A finding in a review body is not a thread and cannot be 'resolved'. Post a review"
  echo "on the PR OPENING with one of RED-VERIFIED: / NOT-A-DEFECT: / DISAGREE: — the same"
  echo "answers an inline finding requires, and dated after the finding it answers."
  rc=1
fi

if [ "$rc" -eq 0 ]; then
  echo "verdict: CLEAR — every thread resolved and disposed, every review body answered."
fi
exit $rc
