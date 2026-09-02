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
#   check-review-threads.sh <pr-number>            # query GitHub
#   check-review-threads.sh --from-file <json>     # parse a saved response (tests)
#   check-review-threads.sh --dump-payload <pr>    # print that response, for fixtures
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
# And for the PR's commits, whose paging needs a PR with 100+ commits to reach.
COMMITS_PAGE_SIZE=${COMMITS_PAGE_SIZE:-100}

# Ordering timestamps. GitHub does not return them all in one shape: check-suite and
# comment timestamps are whole-second ("2026-01-02T00:30:00Z"), while the datetime Codex
# writes into a review-summary row carries a fraction ("2026-01-02T00:30:00.123456Z").
# Compared as strings, "." sorts before "Z", so a result recorded in the SAME SECOND as the
# head check suite reads as earlier than the head arrived, and the head stays uncovered for
# as long as that result is its only coverage. String order also ranks values that are not
# timestamps at all: "a while ago" sorts after every digit, so an unparsable date postdated
# everything and granted coverage.
#
# `ts` parses an ISO-8601 instant to epoch seconds, with the fraction optional and the zone
# either Z or a +HH:MM offset, and yields null for anything else. Every comparison below is
# between parsed instants. A null orders against nothing: it grants no coverage and answers
# no claim, and where the field is one the gate validates, the gate says so and exits 2.
#
# The day is checked against the month's real length before the conversion, because mktime
# NORMALISES: without that check "2026-02-31T01:00:00Z" converts to 2026-03-03T01:00:00Z and
# orders as an instant three days after anything it names. The zone offset's minutes field
# is minutes; added as seconds it puts every half- and quarter-hour zone up to 44 minutes
# out, and for a positive offset the error runs late, which is the fail-open direction.
TS_JQ='def ts:
  if type != "string" then null
  else (capture("^(?<y>[0-9]{4})-(?<mo>[0-9]{2})-(?<d>[0-9]{2})[Tt](?<h>[0-9]{2}):(?<mi>[0-9]{2}):(?<s>[0-9]{2})(\\.(?<frac>[0-9]+))?([Zz]|(?<zsign>[+-])(?<zh>[0-9]{2}):?(?<zm>[0-9]{2}))$") // null) as $c
  | if $c == null then null
    else ($c.y | tonumber) as $yy
       | ($c.mo | tonumber) as $mo | ($c.d | tonumber) as $dd
       | ($c.h | tonumber) as $hh | ($c.mi | tonumber) as $mm | ($c.s | tonumber) as $ss
       | (($c.zh // "0") | tonumber) as $zh | (($c.zm // "0") | tonumber) as $zm
       | (if $mo == 2 then (if ((($yy % 4) == 0) and ((($yy % 100) != 0) or (($yy % 400) == 0)))
                            then 29 else 28 end)
          elif ($mo == 4 or $mo == 6 or $mo == 9 or $mo == 11) then 30
          else 31 end) as $dim
       | if $mo < 1 or $mo > 12 or $dd < 1 or $dd > $dim or $hh > 23 or $mm > 59 or $ss > 60
            or $zh > 23 or $zm > 59 then null
         else ([$yy, $mo - 1, $dd, $hh, $mm, $ss, 0, 0] | mktime)
              + (if $c.frac then ("0." + $c.frac | tonumber) else 0 end)
              - (if $c.zsign == "-" then -1 else 1 end) * ($zh * 3600 + $zm * 60)
         end
    end
  end;'

# What a thread contributes to the verdict, in a form the two reads can be compared in.
# The comment bodies are IN it: they are what the disposition and unresolved-listing rules
# read, and an in-place edit moves neither the thread's flags nor its comment count. pageInfo
# is deliberately out, since a cursor is not content and one differing between two reads of
# the same threads would block on nothing.
THREAD_FP_JQ='[ .[] | {id, isResolved, isOutdated, total: (.comments.totalCount // null),
                       comments: [ .comments.nodes[]? | {author: (.author.login // null), path,
                                                         line, originalLine, body: (.body // null)} ]} ]
  | sort_by(.id | tostring)'

# Every paging loop below folds a page in with `<connection>.nodes // []` and then reads
# `hasNextPage` from that same page. Both readings turn a response the loop CANNOT READ into
# "this connection is empty and it is finished": a null `pullRequest` (a wrong number, a
# renamed repo, a token that cannot see the PR, a partial GraphQL result) contributes no
# nodes, and hasNextPage evaluates to null rather than "true", so the loop stops holding [].
# `[]` is indistinguishable from a connection that is genuinely empty, and the verdict is
# then computed from a list nobody fetched. That is the gate's recurring fail-open shape: a
# check that could not evaluate anything reporting that it found nothing. On the recheck
# comments loop it dropped every top-level claim while the run reported CLEAR (#882).
#
# So no page is folded in before this says it can be read: parsable JSON, a pullRequest that
# is there, the named connection present, a `nodes` array, and a boolean `hasNextPage`.
# Anything else blocks with a reason and returns 2, the code this gate already uses for
# "cannot evaluate". It prints to stderr because the fetchers' stdout is the payload.
require_page() {
  local resp=$1 path=$2 what=$3 pr=${4:-} why=""
  if ! jq -e 'type == "object"' >/dev/null 2>&1 <<<"$resp"; then
    why="the response is not JSON this gate can read"
  elif [[ $path == data.repository.pullRequest.* ]] \
       && [ "$(jq -r '.data.repository.pullRequest // "null"' <<<"$resp")" = "null" ]; then
    why="no pull request #$pr in $REPO_OWNER/$REPO_NAME (or it is not visible to this token)"
  elif ! jq -e --arg p "$path" 'getpath($p | split("."))
         | type == "object" and (.nodes | type) == "array"
         and (.pageInfo.hasNextPage | type) == "boolean"' >/dev/null 2>&1 <<<"$resp"; then
    why="the page carries no nodes array and no boolean hasNextPage"
  # hasNextPage without a cursor is the same hole one field over: the page says another
  # page follows and names nothing to fetch it with. `jq -r` renders a null endCursor as
  # the STRING "null", which is the outer loops' own "no cursor yet" sentinel, so the next
  # request omits `after` and asks for the FIRST page again, and the same page answers
  # forever. An empty cursor sends `after: ""`. The oversized-thread loop tests the cursor
  # for "null"/empty as its exit condition instead, so it ENDS and the thread is judged
  # from the pages that did arrive. A literal "null" is rejected for the same reason a null
  # is: the call sites cannot tell it apart from the sentinel.
  elif ! jq -e --arg p "$path" 'getpath($p | split("."))
         | .pageInfo.hasNextPage == false
           or ((.pageInfo.endCursor | type) == "string"
               and (.pageInfo.endCursor | length) > 0
               and .pageInfo.endCursor != "null")' >/dev/null 2>&1 <<<"$resp"; then
    why="the page says another page follows and names no cursor to fetch it with"
  else
    return 0
  fi
  echo "verdict: BLOCKED. Could not read $what: $why." >&2
  echo "A page this gate could not read is not an empty page. Folding one in as [] is" >&2
  echo "indistinguishable from a connection that is genuinely empty, so this run refuses" >&2
  echo "to report a verdict computed from a list nobody fetched." >&2
  return 2
}

# ONE read of the PR's review threads: every thread, every comment, with each oversized
# thread's comments paged to the end. Prints the thread array on stdout; says why and
# returns non-zero when a page could not be fetched. fetch_payload calls it TWICE, and the
# two reads must agree.
fetch_threads() {
  local pr=$1 cursor=null threads='[]'
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
    require_page "$resp" data.repository.pullRequest.reviewThreads "the review threads" "$pr" || return 2

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
    # require_page checked the reviewThreads page; this cursor comes from a comments
    # connection nested INSIDE one of its nodes, which that check does not reach. The
    # thread is here because it holds more comments than one page, so a cursor the loop
    # cannot use means the tail can never be asked for, and the loop below would simply
    # not run.
    if [ "$ccursor" = "null" ] || [ -z "$ccursor" ]; then
      echo "verdict: BLOCKED. Cannot page thread $tid: it holds more comments than one" >&2
      echo "page and its first page names no cursor to fetch the rest with. A thread judged" >&2
      echo "from the comments that did arrive is a thread nobody finished reading." >&2
      return 2
    fi
    while [ "$ccursor" != "null" ] && [ -n "$ccursor" ]; do
      cresp=$(gh api graphql -f query="
        { node(id: \"$tid\") { ... on PullRequestReviewThread {
            comments(first: $COMMENTS_PAGE_SIZE, after: \"$ccursor\") {
              pageInfo { hasNextPage endCursor }
              nodes { author { login } path line originalLine body } } } } }" 2>/dev/null) || {
        # NOT `break`. A transient API/auth/rate-limit failure mid-thread used to return
        # the pages fetched so far as though they were the whole conversation — so an
        # early disposition could certify a thread nobody finished reading.
        echo "verdict: BLOCKED — could not page comments for thread $tid." >&2
        return 1
      }
      # A node the query cannot resolve (a deleted thread, an id this token cannot see)
      # comes back as a null `node`, which folded in as an empty page and read hasNextPage
      # as null: the loop ended and the thread was judged from the pages fetched so far,
      # exactly the truncation the failure branch above refuses to accept.
      require_page "$cresp" data.node.comments "the comments page for thread $tid" || return 2
      all_comments=$(jq -c -s '.[0] + (.[1].data.node.comments.nodes // [])' \
        <(echo "$all_comments") <(echo "$cresp"))
      if [ "$(jq -r '.data.node.comments.pageInfo.hasNextPage' <<<"$cresp")" = "true" ]; then
        ccursor=$(jq -r '.data.node.comments.pageInfo.endCursor' <<<"$cresp")
      else
        ccursor=null
      fi
    done
    # Same MAX_ARG_STRLEN hazard as the final merge below: a fully paged
    # thread's comments are exactly the payload that grows past one argv
    # string, so they ride an fd too.
    threads=$(jq --arg id "$tid" --slurpfile c <(printf '%s' "$all_comments") \
      'map(if .id == $id then .comments.nodes = $c[0] else . end)' <<<"$threads")
  done
  printf '%s' "$threads"
}

fetch_payload() {
  local pr=$1 threads='[]' reviews='[]' prcomments='[]' prauthor='' headoid=''

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
            author { login }
            headRefOid
            commits(last: 1) { nodes { commit { committedDate checkSuites(first: 10) { nodes { createdAt } } } } }
            reviews(first: $REVIEWS_PAGE_SIZE$rafter) {
              pageInfo { hasNextPage endCursor }
              nodes { author { login } state body submittedAt commit { oid }
                    comments(first: $REVIEWS_PAGE_SIZE) { totalCount nodes { replyTo { id } } } }
            } } } }" 2>/dev/null) || return 1
    require_page "$rresp" data.repository.pullRequest.reviews "the reviews" "$pr" || return 2
    reviews=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviews.nodes // [])' \
          <(echo "$reviews") <(echo "$rresp"))
    prauthor=$(jq -r '.data.repository.pullRequest.author.login // ""' <<<"$rresp")
    headoid=$(jq -r '.data.repository.pullRequest.headRefOid // ""' <<<"$rresp")
    headdate=$(jq -r '.data.repository.pullRequest.commits.nodes[0].commit.committedDate // ""' <<<"$rresp")
    headsuites=$(jq -c '.data.repository.pullRequest.commits.nodes[0].commit.checkSuites.nodes // []' <<<"$rresp")
    [ "$(jq -r '.data.repository.pullRequest.reviews.pageInfo.hasNextPage' <<<"$rresp")" = "true" ] || break
    rcursor=$(jq -r '.data.repository.pullRequest.reviews.pageInfo.endCursor' <<<"$rresp")
  done

  # The PR's own commits: the universe an abbreviated sha in a review result resolves
  # against. Its own connection, paged on its own cursor to completion. It used to ride the
  # reviews query as `commits(last: 100)`, which truncates in silence: past 100 commits an
  # omitted OLDER commit sharing the head's seven-character prefix made the abbreviation
  # look unambiguous, so a result issued for that commit certified the head. totalCount
  # travels with the list, and the reader refuses a list that does not account for it, so a
  # fetch that stopped early blocks instead of resolving against a fragment.
  local pccursor=null pcafter="" pcresp prcommitcount=null prcommits='[]'
  while :; do
    [ "$pccursor" != "null" ] && pcafter=", after: \"$pccursor\""
    pcresp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            commits(first: $COMMITS_PAGE_SIZE$pcafter) {
              totalCount
              pageInfo { hasNextPage endCursor }
              nodes { commit { oid } }
            } } } }" 2>/dev/null) || return 1
    require_page "$pcresp" data.repository.pullRequest.commits "the PR's commit list" "$pr" || return 2
    prcommits=$(jq -s '.[0] + (.[1].data.repository.pullRequest.commits.nodes // [])' \
          <(echo "$prcommits") <(echo "$pcresp"))
    prcommitcount=$(jq -r '.data.repository.pullRequest.commits.totalCount // "null"' <<<"$pcresp")
    [ "$(jq -r '.data.repository.pullRequest.commits.pageInfo.hasNextPage' <<<"$pcresp")" = "true" ] || break
    pccursor=$(jq -r '.data.repository.pullRequest.commits.pageInfo.endCursor' <<<"$pcresp")
  done

  # THIRD place a finding can live: a top-level PR comment. Not a thread, not a review —
  # its own `comments` connection. This gate told people to answer with `gh pr comment`
  # while never reading what that command produces, so a defect claim posted the way the
  # docs suggest could sit on a PR that reported CLEAR.
  local ccursor2=null cafter="" cresp2
  while :; do
    [ "$ccursor2" != "null" ] && cafter=", after: \"$ccursor2\""
    cresp2=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            comments(first: $REVIEWS_PAGE_SIZE$cafter) {
              pageInfo { hasNextPage endCursor }
              nodes { author { login __typename } body createdAt updatedAt }
            } } } }" 2>/dev/null) || return 1
    require_page "$cresp2" data.repository.pullRequest.comments "the top-level comments" "$pr" || return 2
    prcomments=$(jq -s '.[0] + (.[1].data.repository.pullRequest.comments.nodes // [])' \
          <(echo "$prcomments") <(echo "$cresp2"))
    [ "$(jq -r '.data.repository.pullRequest.comments.pageInfo.hasNextPage' <<<"$cresp2")" = "true" ] || break
    ccursor2=$(jq -r '.data.repository.pullRequest.comments.pageInfo.endCursor' <<<"$cresp2")
  done

  threads=$(fetch_threads "$pr") || return $?

  # Re-read the COMMENTS after all paging, for the same reason the head is re-read below —
  # and it is not the same check. Two of the three coverage signals live in comments that
  # their bot EDITS IN PLACE: CodeRabbit's walkthrough and Codex's review summary. Both were
  # captured at the start of a run that then spent minutes paging threads, and both can go
  # from a clean result to a failed, rate-limited or in-progress one without the head moving
  # an inch. The head recheck cannot see that. This second read is compared against the
  # first below, and any difference blocks: the gate would otherwise grant coverage from a
  # comment that no longer says what it said when it was read.
  local rc2cursor=null rc2after="" rc2resp recheckcomments='[]'
  while :; do
    [ "$rc2cursor" != "null" ] && rc2after=", after: \"$rc2cursor\""
    rc2resp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            comments(first: $REVIEWS_PAGE_SIZE$rc2after) {
              pageInfo { hasNextPage endCursor }
              nodes { author { login __typename } body createdAt updatedAt }
            } } } }" 2>/dev/null) || return 1
    require_page "$rc2resp" data.repository.pullRequest.comments "the second read of the top-level comments" "$pr" || return 2
    recheckcomments=$(jq -s '.[0] + (.[1].data.repository.pullRequest.comments.nodes // [])' \
          <(echo "$recheckcomments") <(echo "$rc2resp"))
    [ "$(jq -r '.data.repository.pullRequest.comments.pageInfo.hasNextPage' <<<"$rc2resp")" = "true" ] || break
    rc2cursor=$(jq -r '.data.repository.pullRequest.comments.pageInfo.endCursor' <<<"$rc2resp")
  done

  # Re-read the REVIEWS and the THREAD LIST after all paging, for the same reason. A claim
  # can land in either while the gate is working, and neither was looked at again: a review
  # body edited to add a P1, or a whole thread opened, arrived after the only read of it and
  # went unjudged. The comments are carried into the payload so the gate judges the FINAL
  # snapshot of them; reviews and threads are compared instead, because re-paging every
  # thread body would double the cost of the slowest part of the run. Either one moving means
  # the reading was taken from data that has since changed, so it blocks.
  local rv2='[]' rv2cursor=null rv2after="" rv2resp
  while :; do
    [ "$rv2cursor" != "null" ] && rv2after=", after: \"$rv2cursor\""
    rv2resp=$(gh api graphql -f query="
      { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
          pullRequest(number: $pr) {
            reviews(first: $REVIEWS_PAGE_SIZE$rv2after) {
              pageInfo { hasNextPage endCursor }
              nodes { author { login } state body submittedAt commit { oid }
                    comments(first: $REVIEWS_PAGE_SIZE) { totalCount nodes { replyTo { id } } } }
            } } } }" 2>/dev/null) || return 1
    require_page "$rv2resp" data.repository.pullRequest.reviews "the second read of the reviews" "$pr" || return 2
    rv2=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviews.nodes // [])' \
          <(echo "$rv2") <(echo "$rv2resp"))
    [ "$(jq -r '.data.repository.pullRequest.reviews.pageInfo.hasNextPage' <<<"$rv2resp")" = "true" ] || break
    rv2cursor=$(jq -r '.data.repository.pullRequest.reviews.pageInfo.endCursor' <<<"$rv2resp")
  done
  if [ "$(jq -cS . <<<"$reviews")" != "$(jq -cS . <<<"$rv2")" ]; then
    echo "verdict: BLOCKED — the reviews on this PR changed while this gate was reading it." >&2
    echo "The reading below was taken from what they said before. Re-run against a PR that is" >&2
    echo "holding still." >&2
    return 2
  fi

  # The second read is the SAME read, comments and all. It used to fetch only id,
  # isResolved and comments.totalCount, on the theory that those are what the verdict turns
  # on. They are not: the verdict turns on the BODIES from the first read, and GitHub lets a
  # review comment be edited in place, which moves none of those three. A disposition edited
  # mid-run into something that disposes of nothing left the fingerprint identical, and the
  # gate certified the thread from a reply that no longer existed. Reading the comments twice
  # costs the slowest part of the run twice; a cheaper comparison that cannot see an edit
  # costs correctness.
  local th2
  th2=$(fetch_threads "$pr") || return $?
  if [ "$(jq -c "$THREAD_FP_JQ" <<<"$threads")" != "$(jq -c "$THREAD_FP_JQ" <<<"$th2")" ]; then
    echo "verdict: BLOCKED — the review threads on this PR changed while this gate was" >&2
    echo "reading them. The reading below was taken from what they said before. Re-run" >&2
    echo "against a PR that is holding still." >&2
    return 2
  fi

  # Re-read the head AFTER all paging. A push landing mid-evaluation would otherwise leave
  # $headoid describing the commit captured by the first query, and the coverage check
  # would happily certify a SHA the PR no longer points at.
  local headnow
  headnow=$(gh api graphql -f query="
    { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
        pullRequest(number: $pr) { headRefOid } } }" \
    --jq '.data.repository.pullRequest.headRefOid' 2>/dev/null) || return 1
  # `--jq` prints an EMPTY line when its filter lands on a null pullRequest, and the check
  # used to skip on an empty result: the one read that exists to catch a push landing
  # mid-run was disabled by precisely the response that says the gate could not see the PR.
  if [ -z "$headnow" ] || [ "$headnow" = "null" ]; then
    echo "verdict: BLOCKED. The re-read of the head named no commit, so this gate cannot" >&2
    echo "tell whether a push landed while it was reading the PR. Re-run against a PR this" >&2
    echo "token can see." >&2
    return 2
  fi
  if [ "$headnow" != "$headoid" ]; then
    echo "verdict: BLOCKED — the head moved from ${headoid:0:9} to ${headnow:0:9} while this" >&2
    echo "gate was reading the PR. Re-run against a branch that is holding still." >&2
    return 2
  fi

  # The three arrays ride file descriptors, not argv: a single argv string is
  # capped at MAX_ARG_STRLEN (128 KiB on Linux), so --argjson with a real PR's
  # thread bodies dies with "Argument list too long" and the gate fail-closes
  # on exactly the large PRs it is most needed for. printf is a bash builtin,
  # so the process substitutions never exec with the payload as an argument.
  jq -n --slurpfile t <(printf '%s' "$threads") \
        --slurpfile r <(printf '%s' "$reviews") \
        --slurpfile c <(printf '%s' "$prcomments") \
        --slurpfile rc <(printf '%s' "$recheckcomments") \
        --arg a "$prauthor" --arg h "$headoid" --arg d "$headdate" --argjson cs "${headsuites:-[]}" \
        --slurpfile pc <(printf '%s' "${prcommits:-[]}") --argjson pct "${prcommitcount:-null}" \
     '{data:{repository:{pullRequest:{author:{login:$a}, headRefOid:$h,
                                      commits:{nodes:[{commit:{committedDate:$d, checkSuites:{nodes:$cs}}}]},
                                      prcommits:{totalCount:$pct, nodes:$pc[0]},
                                      reviewThreads:{nodes:$t[0]}, reviews:{nodes:$r[0]},
                                      comments:{nodes:$c[0]},
                                      recheck:{comments:{nodes:$rc[0]}}}}}}'
}

# A fixture must be the payload the gate actually consumes, not a hand-written guess at
# it. One already drifted: it carried two comments per thread while the query asked for
# `comments(first: 1)`, so a test passed against data the code could never return. Dumping
# from fetch_payload keeps the fixtures and the query the same thing by construction.
if [ "${1:-}" = "--dump-payload" ]; then
  fetch_payload "${2:?usage: check-review-threads.sh --dump-payload <pr-number>}" || exit 2
  exit 0
fi

if [ "${1:-}" = "--from-file" ]; then
  payload=$(cat "${2:?need a json file}")
else
  pr=${1:?usage: check-review-threads.sh <pr-number> | --from-file <json>}
  payload=$(fetch_payload "$pr") || exit 2
fi
threads=$(jq '.data.repository.pullRequest.reviewThreads.nodes' <<<"$payload" 2>/dev/null)
reviews=$(jq '.data.repository.pullRequest.reviews.nodes // []' <<<"$payload" 2>/dev/null)
prcomments=$(jq '.data.repository.pullRequest.comments.nodes // []' <<<"$payload" 2>/dev/null)

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
if ! jq -e "$TS_JQ"'all(((.body // "") | test("[^[:space:]]") | not) or ((.submittedAt | ts) != null))' \
     >/dev/null 2>&1 <<<"$reviews"; then
  echo "verdict: BLOCKED — a non-empty review body has no parsable submittedAt timestamp, so" >&2
  echo "this gate cannot tell whether the answer came before or after the claim." >&2
  exit 2
fi
if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$prcomments"; then
  echo "verdict: BLOCKED — top-level PR comment data is not an array." >&2
  exit 2
fi
if ! jq -e "$TS_JQ"'all(((.body // "") | type) == "string"
                and (((.body // "") | test("[^[:space:]]") | not) or ((.createdAt | ts) != null)))' \
     >/dev/null 2>&1 <<<"$prcomments"; then
  echo "verdict: BLOCKED — a top-level PR comment has a non-text body or no parsable createdAt." >&2
  exit 2
fi
# A comment is MUTABLE and createdAt does not move when it is edited, so ordering the body
# the gate is reading by createdAt dates the wrong thing: a comment opened before a
# disposition and edited afterwards to add a defect reads as answered by it. updatedAt dates
# the current body, and a body with no parsable updatedAt cannot be ordered at all.
if ! jq -e "$TS_JQ"'all(((.body // "") | test("[^[:space:]]") | not) or ((.updatedAt | ts) != null))' \
     >/dev/null 2>&1 <<<"$prcomments"; then
  echo "verdict: BLOCKED — a top-level PR comment has no parsable updatedAt, so this gate" >&2
  echo "cannot date the body it is reading against the answers to it." >&2
  exit 2
fi
# The coverage check below certifies that the HEAD was reviewed, so a payload that names
# no head is one it cannot judge. This used to skip the check instead, which made "no
# head" and "no reviews" both read as CLEAR: nothing to answer is not nothing reviewed.
headoid=$(jq -r '.data.repository.pullRequest.headRefOid // ""' <<<"$payload" 2>/dev/null)
if [ "${REQUIRE_REVIEWED_HEAD:-1}" = "1" ] && [ -z "$headoid" ]; then
  echo "verdict: BLOCKED — the payload names no head commit, so this gate cannot tell" >&2
  echo "whether the code being merged was reviewed. Re-run, or re-capture the fixture." >&2
  exit 2
fi

# Reviews and top-level comments are two containers for the same thing: a PR-level
# statement. Judge them as one list, so an answer posted either way settles a claim posted
# either way. `at` normalises submittedAt/updatedAt: a review is dated by its submission and
# a comment by its last edit, which is when its current body came to say what it says.
#
# `claimable` — what needs an answer — took several tries, and every wrong version made
# the same mistake: inferring whether something is a FINDING from who wrote it.
#
#   v1: every non-empty body counts. Blocked on `@codex review` — the trigger this skill
#       documents — and on deploy notifications. A gate that blocks on ordinary traffic
#       gets switched off.
#   v2: exclude the PR author, and bots. But Codex IS a bot, and posts findings as plain
#       comments; and a genuine self-reported defect from the author still needs a RED
#       test, which this skill says explicitly. Silenced both.
#   v3: exclude bots that have never submitted a review. A bot whose FIRST finding is a
#       top-level comment has no review history yet — silenced exactly the case the
#       adjacent comment warned about.
#
# Authorship cannot answer the question. So this no longer asks it. Two explicit signals
# do the work, and both fail CLOSED for anything unrecognised:
#
#   - REVIEWS carry a state. GitHub's own APPROVED means "I am not asking for changes";
#     that is a semantic fact, not a guess about the prose. COMMENTED and
#     CHANGES_REQUESTED carry findings and need answers — from anyone EXCEPT the PR author.
#     A review body from the author is the answer this gate asks for two paragraphs below
#     ("Post a review on the PR OPENING with one of ..."), so counting it as a claim made
#     every acknowledgement create the obligation it was posted to discharge, and the count
#     grew by one each round: #874 carried 4 such bodies and #872 5, all the author's own
#     acks, burying the bot findings that really had no answer. The head-coverage rule
#     discounts the author for the same reason. This does NOT re-admit REVIEW-ACK as a
#     disposition: it answers nothing, it merely stops being a claim itself. And the
#     exemption belongs to a LOGIN, which is unique across GitHub accounts, so nobody else
#     can wear it; an empty or absent author matches nothing, because on a payload that
#     names no author "" == "" would exempt every unattributed body on it.
#   - TOP-LEVEL COMMENTS have no state, so everything counts unless it is on a short,
#     documented ignore list: bot logins that only ever post notifications, the literal
#     trigger phrases this skill tells you to post, and a listed reviewer's notice that it
#     did NOT review (the shapes above VERDICT_JQ). An unknown bot counts, and so
#     does the AUTHOR: a comment is where a self-reported defect lands, and a review is
#     where an answer does. That asymmetry is the whole rule.
#
# Add to IGNORED_COMMENT_AUTHORS deliberately, and never a bot that reviews.
IGNORED_COMMENT_AUTHORS=${IGNORED_COMMENT_AUTHORS:-vercel,vercel[bot],dependabot,dependabot[bot],github-actions,github-actions[bot],codecov,codecov[bot]}
# Comments that are ONLY a documented bot command: "@codex review", "@codex security
# review", or "@coderabbitai" followed by one of review, full review, resume, pause,
# ignore, resolve, summary, help, configuration. The handles are the mention names of the
# bots in VERDICT_BOTS and the commands are the ones each bot documents (Codex in its
# About block, CodeRabbit in its command reference). Case-insensitive, because GitHub
# logins are. Any other comment that opens with a mention is a finding and stays
# claimable: "@me drops records" and "@codex drops records" both need a disposition. An
# earlier version exempted any mention plus up to two words, which exempted exactly those.
TRIGGER_RE='\A[[:space:]]*@(codex[[:space:]]+(security[[:space:]]+)?review|coderabbitai[[:space:]]+(full[[:space:]]+review|review|resume|pause|ignore|resolve|summary|help|configuration))[[:space:]]*\z'
# A reviewer with nothing to say posts no review object. Codex answers with a plain
# comment: "Codex Review: Didn't find any major issues." plus one of a few short
# sign-offs, then "**Reviewed commit:** `<sha>`", then a folded "About Codex in GitHub"
# block. A CodeRabbit review that produces no comments replies "Full review finished."
# (or "Review finished." for an incremental one) folded under "Action performed". Those
# are verdicts, not findings: they need no disposition. Codex's also covers the commit it
# names; CodeRabbit's names nothing, and its coverage comes from the walkthrough comment
# instead (walkthrough_sha below, and the coverage check for how a result is bound to
# the head).
#
# walkthrough_sha is the last commit of a review that RAN CLEAN, or "". CodeRabbit edits
# one walkthrough comment in place, and its recent-review block names the range it
# reviewed after a failed review too: on #853 and #792 the range ended at the head under
# a "Review failed" caution, with no review object on the head, and the range alone read
# as coverage. So three things must hold, each failing closed: the block says "No
# actionable comments were generated" (CodeRabbit adds that line only after a review that
# finished with nothing to post); the summarize marker is the comment's ONLY
# auto-generated-comment marker (the failure, rate-limit, skip, pause and in-progress
# notices each add their own, and so would a notice this script has never seen); and the
# comment carries no blockquoted heading, which is how every notice renders its title
# (Review failed / skipped / limit reached, Reviews paused). A rate-limit or pause notice
# beside a clean block at the head (#869) also declines: the comment is in a mixed state
# from two runs, and which run wrote which block is not something this gate can read.
#
# Codex's SECOND no-findings shape, and since 2026-08-28 the only one it has posted here:
# one comment per PR carrying "<!-- codex-pull-request-review-summary -->", opened when a
# review STARTS and edited in place as reviews finish. Its table holds one row per review:
#   | Code Review | Completed <relative-time datetime="..."> | `9ebed54` | Manual request |
# The comment is a notice, never a finding. A row covers the head when it says Completed,
# names a prefix of the head, and its own datetime postdates the head's arrival. Neither
# timestamp on the comment can date a row: createdAt is written before any result exists
# (on #867, created 22:11:32Z for a review it reported at 22:15:13Z), and updatedAt only
# says when the last of them was published.
#
# summary_rows returns the {sha, at} of every row, or [] for the whole comment when
# anything in the table is not a finished review: a row still in progress, a status this
# gate has never seen, a row it cannot split into the table's four cells. A table holding a
# review that is still running is a review that has not finished, whatever sits beside it.
# A Completed row whose commit or datetime will not parse names nothing and binds nothing.
#
# The verdict must be the WHOLE comment. After dropping HTML comments, blank lines, the
# About Codex block and the one blockquote line CodeRabbit's incremental reply carries
# (matched whole; it is the only blockquote line that bot's replies on this repo have
# ever contained), the remaining lines must be exactly one of the shapes below; anything
# else is a finding and stays claimable. Dropping every line that starts with ">" was a
# hole: a bot quoting its finding, "> P1: this drops records", had posted a verdict. The
# Codex sign-off is matched against the list of ones Codex has posted on this repo, because
# an open-ended tail would accept "Didn't find any major issues. P1: drops data" as a
# verdict. An unlisted sign-off is not a verdict, so the gate blocks until someone reads
# the comment, which is the right direction; extend the list when Codex says something
# new. Only the About Codex block is stripped, and only under that exact summary text:
# a details block with any other summary is where a bot folds its findings.
# NOTICES: a listed reviewer saying that its review did NOT run. Neither a finding nor
# coverage. Both bots post one, and because both are in VERDICT_BOTS none of it fell under
# the notification-bot exemption below: every notice was a PR-level claim needing a
# disposition dated after it. On 2026-09-02 #898 carried three Codex quota notices and the
# gate reported all three UNANSWERED (aws #46 one more); the author cleared them with a
# NOT-A-DEFECT review answering text that claimed nothing. A gate that blocks on ordinary
# bot traffic gets switched off, which is the failure the withdrawn last-word rule had.
#
# A notice is matched as the WHOLE body, line for line, against the shapes the bots have
# posted here, and belongs to the bot that posts it (see CODERABBIT_LOGIN): the same words
# with anything added, or from any other account, are a comment like any other and stay
# claimable. The shapes, with where each was observed:
#   - Codex, a plain top-level comment: "You have reached your Codex usage limits for code
#     reviews. You can see your limits in the [Codex usage dashboard](...)." alone (#898 at
#     05:16Z, #842) or followed by "To continue using code reviews, add credits to your
#     account and enable them for code reviews in your [settings](...)." (#898 at 05:20Z
#     and 06:04Z, aws #46, aws #27).
#   - CodeRabbit's reply to a command it did not carry out: one of "Review rate limited.",
#     "Already reviewed.", "Already reviewed the last commit. Use `@coderabbitai full
#     review` to rerun a review of the entire changeset.", "No files to review." or "Pull
#     request is closed." folded under "⚠️ Action not completed", with or without the
#     incremental-review note (#847, #864, #846, #869, #853); or "Review rate limited." over
#     a rule and the Fair Usage Limits paragraph naming a wait (#844, #887, aws #17).
#   - CodeRabbit's chat reply "### Rate Limit Exceeded" naming the user and a wait (#893).
#   - CodeRabbit's summary comment before any review has run: the summarize marker, ONE
#     notice between its "rate limited", "skip review" or "review paused" markers, its
#     payload blockquoted under a "Review limit reached", "Review skipped" or "Reviews
#     paused" heading, the tips block, and nothing else (#874, #843, #789, #832). The
#     payload is parsed line by line against the shapes CodeRabbit posts
#     (cr_notice_payload_res), not skimmed for a leading ">": accepting any quoted line
#     made "> P1: this drops the last row" part of the notice, and a notice needs no
#     disposition. The range
#     that notice quotes is not coverage (walkthrough_sha never read it). Once the comment
#     holds a walkthrough or a recent-review block it is a walkthrough, judged as one.
#
# ACCOUNTING. A body is exempt from dispositions only if the classifier read every byte of
# it. Content removed before classification is content nobody evaluated, so a finding in a
# removed region is exempt and never answered. That is one defect class and it landed three
# times: the notice payload (any line starting with ">"), the CodeRabbit tips block, and
# the folded About Codex block, the last of which also bound no-findings COVERAGE to the
# head. Every removal now goes through strip_accounted, which deletes a region declared in
# accounted_regions only after every non-blank line inside it matches one of that region's
# shapes, and returns null otherwise, which makes the whole body claimable. The regions:
#
#   cr_tips       <!-- tips_start --> .. <!-- tips_end -->        rendered, shape-checked
#   codex_about   <details> <summary>ℹ️ About Codex ..</details>  rendered, shape-checked
#   html_comment  <!-- .. -->                                     not rendered, no shapes
#
# html_comment carries no shapes because GitHub renders none of it: nothing inside one is a
# claim a reader can see or answer. Both shape lists were read off what the bots posted on
# ejc3/fcvm #789 through #901 (13 About blocks, 17 tips blocks) and match all of it with
# nothing left over. Every shape is anchored, because an unanchored one matches the head of
# a line and lets the rest ride along. An unlisted line costs one disposition; accepting
# one costs a finding nobody has to answer.
#
# scripts/gate-discard-sites.sh enumerates every discarding call in VERDICT_JQ and blocks
# on any that is neither this primitive nor a listed normalization, so the next stripping
# step fails a test unless it declares a region. Both harnesses run it.
VERDICT_JQ='def cr_note: "> Note: CodeRabbit is an incremental review system and does not re-review already reviewed commits. This command is applicable only when automatic reviews are paused.";
def cr_tips_res:
  [ "^---$"
  , "^</?details>$"
  , "^<summary>❤️ Share</summary>$"
  , "^- \\[(X|Mastodon|Reddit|LinkedIn)\\]\\(https://[^)]*\\)$"
  , "^Thanks for using \\[CodeRabbit\\]\\(https://coderabbit\\.ai\\?utm_source=oss&utm_medium=github&utm_campaign=[^)]+\\)! It.s free for OSS, and your support helps us grow\\. If you like it, consider giving us a shout-out\\.$"
  , "^<sub>Comment `@coderabbitai help` to get the list of available commands\\.</sub>$"
  ];
def codex_about_res:
  [ "^<br/>$"
  , "^\\[Your team has set up Codex to review pull requests in this repo\\]\\(https://chatgpt\\.com/codex/cloud/settings/general\\)\\. Reviews are triggered when you$"
  , "^- Open a pull request for review$"
  , "^- Mark a draft as ready$"
  , "^- Comment \"@codex review\"( or \"@codex security review\")?\\.$"
  , "^If Codex has suggestions, it will comment; otherwise it will react with 👍\\.$"
  , "^Codex reacts with 👀 while any review is running, comments if it has suggestions, and reacts with 👍 once all reviews finish with no findings\\.$"
  , "^Codex can also answer questions or update the PR\\. Try commenting \"@codex address that feedback\"\\.$"
  ];
def accounted_regions:
  [ { name: "cr_tips", rendered: true, lines: cr_tips_res,
      re: "<!-- tips_start -->(?<inner>(.|\n)*?)<!-- tips_end -->" }
  , { name: "codex_about", rendered: true, lines: codex_about_res,
      re: "<details> <summary>ℹ️ About Codex in GitHub</summary>(?<inner>(.|\n)*?)</details>" }
  , { name: "html_comment", rendered: false, lines: null,
      re: "<!--(.|\n)*?-->" }
  ];
def strip_accounted($names):
  if ($names | any(. as $n | ([accounted_regions[] | .name] | index($n)) == null)) then null
  else reduce accounted_regions[] as $r (.;
    if . == null then null
    elif ($r.name | IN($names[]) | not) then .
    elif ($r.rendered | not) then gsub($r.re; "")
    elif (($r.lines // []) | length) == 0 then null
    elif ([match($r.re; "g")] | all(.[];
            [.captures[] | select(.name == "inner") | .string] as $c
            | ($c | length) == 1
              and (($c[0] // "") | split("\n")
                   | map(gsub("^[[:space:]]+|[[:space:]]+$"; "")) | map(select(length > 0))
                   | all(.[]; . as $line | any($r.lines[]; . as $re | $line | test($re))))))
      then gsub($r.re; "")
    else null end)
  end;
def verdict_lines:
  ((. // "") | strip_accounted(["codex_about", "html_comment"])) as $s
  | if $s == null then null
    else ($s | split("\n") | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
             | map(select(length > 0 and . != cr_note))) end;
def codex_line_re: "^Codex Review: Didn.t find any major issues\\.?( Bravo\\.| Keep it up!| Keep them coming!| Hooray!| Swish!| You.re on a roll\\.| :\\+1:| :rocket:| :tada:| Nice work[.!]| Great job[.!]| 👍)?$";
def reviewed_re: "^\\*\\*Reviewed commit:\\*\\* `(?<sha>[0-9a-f]{7,40})`$";
def is_verdict:
  verdict_lines as $l
  | ($l == ["<details>", "<summary>✅ Action performed</summary>", "Full review finished.", "</details>"])
    or ($l == ["<details>", "<summary>✅ Action performed</summary>", "Review finished.", "</details>"])
    or (($l | length) == 2 and ($l[0] | test(codex_line_re)) and ($l[1] | test(reviewed_re)));
def verdict_sha:
  verdict_lines as $l
  | if ($l | length) == 2 and ($l[1] | test(reviewed_re)) then ($l[1] | capture(reviewed_re) | .sha) else "" end;
def codex_summary_marker: "<!-- codex-pull-request-review-summary -->";
def summary_cells:
  split("|") | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
  | (if (.[0] // "") == "" then .[1:] else . end)
  | (if (.[-1] // "") == "" then .[:-1] else . end);
def summary_lines:
  ((. // "") | strip_accounted(["codex_about", "html_comment"])) as $s
  | if $s == null then null
    else ($s | split("\n") | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
             | map(select(length > 0))) end;
def is_codex_summary:
  (((. // "") | sub("^[[:space:]]+"; "")) | startswith(codex_summary_marker))
  and (summary_lines as $l
       | ($l | length) >= 5
       and ($l[0] == "## Codex Review Summary")
       and ($l[1] == "This comment shows the latest Codex review activity on this pull request.")
       and ($l[2] == "| Review | Status | Commit | Review trigger |")
       and (($l[3] | summary_cells) as $sep
            | ($sep | length) == 4 and ($sep | all(.[]; test("^:?-{3,}:?$"))))
       and ($l[4:] | all(.[]; startswith("|") and endswith("|"))));
def summary_rows:
  [ (. // "") | split("\n")[] | gsub("^[[:space:]]+|[[:space:]]+$"; "")
    | select(startswith("|")) | summary_cells ]
  | map(select((((.[0] // "") == "Review") and ((.[1] // "") == "Status")) | not))
  | map(select(((length > 0) and all(.[]; test("^:?-{3,}:?$"))) | not))
  | . as $rows
  | if ($rows | length) == 0 then []
    elif ($rows | any(length != 4)) then []
    elif ($rows | any((([.[1] | capture("\\*\\*(?<s>[^*]+)\\*\\*").s] | first) // "")
                      | gsub("^[[:space:]]+|[[:space:]]+$"; "") | . != "Completed")) then []
    else [ $rows[]
           | { sha: (([.[2] | capture("^`(?<x>[0-9a-f]{7,40})`$").x] | first) // ""),
               at: (([.[1] | capture("datetime=\"(?<d>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?Z)\"").d] | first) // "") } ]
    end;
def cr_range_re: "Reviewing files that changed from the base of the PR and between [0-9a-f]{40} and (?<sha>[0-9a-f]{40})\\.";
def cr_marker_re: "<!-- This is an auto-generated comment: [^>]*-->";
def cr_summarize_marker: "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->";
def walkthrough_sha:
  ([ (. // "")
     | select([scan(cr_marker_re)] == [cr_summarize_marker])
     | select(test("(?m)^>[[:space:]]*##") | not)
     | capture("<!-- recent_review_start -->(?<r>(.|\n)*?)<!-- recent_review_end -->") | .r
     | select(test("No actionable comments were generated in the recent review"))
     | capture(cr_range_re) | .sha ] | first) // "";
def codex_limit_line1: "You have reached your Codex usage limits for code reviews. You can see your limits in the [Codex usage dashboard](https://chatgpt.com/codex/cloud/settings/usage).";
def codex_limit_line2: "To continue using code reviews, add credits to your account and enable them for code reviews in your [settings](https://chatgpt.com/codex/cloud/settings/code-review).";
def is_codex_limit_notice:
  verdict_lines as $l | ($l == [codex_limit_line1]) or ($l == [codex_limit_line1, codex_limit_line2]);
def cr_not_done_re: "^(Review rate limited\\.|Already reviewed\\.|Already reviewed the last commit\\. Use `@coderabbitai full review` to rerun a review of the entire changeset\\.|No files to review\\.|Pull request is closed\\.)$";
def cr_fair_usage_re: "^Your included review limit is currently reached under our \\[Fair Usage Limits Policy\\]\\(https://docs\\.coderabbit\\.ai/management/plans#fair-usage-limits-policy\\)\\. This review may still proceed through usage-based billing if eligible\\. Your next included review will be available in [0-9]+ (minutes?|seconds?|hours?)\\.$";
def cr_chat_limit_re: "^`@[A-Za-z0-9-]+` have exceeded the limit for the number of chat messages per hour\\. Please wait \\*\\*[0-9]+ (minutes?|seconds?|hours?)( and [0-9]+ (minutes?|seconds?))?\\*\\* before sending another message\\.$";
def is_cr_reply_notice:
  verdict_lines as $l
  | (($l | length) == 4 and $l[0] == "<details>" and $l[1] == "<summary>⚠️ Action not completed</summary>"
     and ($l[2] | test(cr_not_done_re)) and $l[3] == "</details>")
    or (($l | length) == 6 and $l[0] == "<details>" and $l[1] == "<summary>⚠️ Action not completed</summary>"
        and $l[2] == "Review rate limited." and $l[3] == "---" and ($l[4] | test(cr_fair_usage_re))
        and $l[5] == "</details>")
    or (($l | length) == 2 and $l[0] == "### Rate Limit Exceeded" and ($l[1] | test(cr_chat_limit_re)));
def cr_notice_start_re: "^<!-- This is an auto-generated comment: (?<k>rate limited|skip review|review paused) by coderabbit\\.ai -->$";
def cr_notice_heading_re: "^## (Review limit reached|Review skipped|Reviews paused)$";
# The notice payload, one accepted shape per line, with the parts that vary between
# postings generalized: counts, durations, shas, run and org ids, backticked names, and the
# handle the rate-limit line addresses. An apostrophe is written "." so this list can live
# inside VERDICT_JQ. Every shape was read off a notice CodeRabbit posted in ejc3/fcvm or
# ejc3/aws; the 46 such bodies fetched on 2026-09-02 are matched by this list with nothing
# left over. A line outside it makes the comment claimable, which is what it was before the
# exemption existed: the cost of an unlisted shape is one disposition, and the cost of
# accepting one is a finding nobody has to answer.
def cr_notice_payload_res:
  [ "^$"
  , "^</?details>$"
  , "^<summary>(⚙️ Run configuration|📥 Commits|📒 Files selected for processing \\([0-9]+\\)|⛔ Files ignored due to path filters \\([0-9]+\\)|How can I continue\\?|How do review limits work\\?|Review details|View limit details)</summary>$"
  , "^\\* `[^`]+`( is excluded by `[^`]+`)?$"
  , "^- \\[ \\] <!-- \\{\"checkboxId\": ?\"[0-9a-f-]+\"\\} --> 🔍 Trigger review$"
  , "^\\*\\*(Configuration used|Review profile|Plan)\\*\\*: [A-Za-z0-9 +]+$"
  , "^\\*\\*Run ID\\*\\*: `[0-9a-f-]+`$"
  , "^\\*\\*Review configuration:\\*\\*$"
  , "^\\*\\*Next review available in:\\*\\* \\*\\*[0-9]+ (minutes?|seconds?|hours?)\\*\\*$"
  , "^\\*\\*Next included review available in [0-9]+ (minutes?|seconds?|hours?)\\.\\*\\*$"
  , "^\\*\\*Limit details:\\*\\* You.ve used (all [0-9]+ included review|the included review) currently available( under your plan)?\\.$"
  , "^Reviewing files that changed from the base of the PR and between [0-9a-f]{40} and [0-9a-f]{40}\\.$"
  , "^You can disable this status message by setting the `reviews\\.review_status` to `false` in the CodeRabbit configuration file\\.$"
  , "^Please check the settings in the CodeRabbit UI or the `\\.coderabbit\\.yaml` file in this repository\\. To trigger a single review, invoke the `@coderabbitai review` command\\.$"
  , "^Use the checkbox below for a quick retry:$"
  , "^`@[A-Za-z0-9-]+`, you.ve reached your PR review limit, so we couldn.t start this review\\.$"
  , "^After more reviews become available, a review can be triggered using the `@coderabbitai review` command as a PR comment\\. Alternatively, push new commits to this PR\\.$"
  , "^To avoid repeated limits, reduce automatic review volume by pausing incremental auto-reviews earlier, using label-based review opt-in, excluding WIP or generated PR titles, or requesting reviews manually when the PR is ready\\. If your team needs uninterrupted high-volume reviews, an organization admin can enable usage-based reviews\\.$"
  , "^CodeRabbit enforces per-developer PR review limits for each organization\\. Most developers receive the normal plan review availability\\.$"
  , "^For paid Pro and Pro\\+ PR reviews, CodeRabbit uses adaptive limits for sustained high-volume activity\\. When a developer.s recent PR review activity reaches the 95th percentile or higher among CodeRabbit users, additional reviews become available more gradually as earlier reviews age out of the rolling window\\.$"
  , "^Please refer \\[docs\\]\\(https://docs\\.coderabbit\\.ai/management/plans#rate-limits\\) for additional details\\.$"
  , "^You.ve used all free OSS reviews for now\\. Wait for the free limit to reset to keep reviewing this public repository\\.$"
  , "^Auto reviews are disabled on base/target branches other than the default branch\\.$"
  , "^Draft detected\\.$"
  , "^Enable \\*\\*\\[usage-based reviews\\]\\(https://app\\.coderabbit\\.ai/settings/billing\\?tab=usage&orgId=[0-9a-f-]+\\)\\*\\* in Billing to review now\\. Otherwise, wait until the next included review is available\\.$"
  , "^You.re only billed for reviews past your plan.s rate limits \\(\\$[0-9]+\\.[0-9]+/file\\)\\.$"
  , "^\\[Learn how review limits work\\]\\(https://docs\\.coderabbit\\.ai/management/plans#rate-limits\\)\\.$"
  , "^Too many files!$"
  , "^This PR contains [0-9]+ files, which is [0-9]+ over the limit of [0-9]+\\.$"
  , "^To get a review, reduce the PR to [0-9]+ files or fewer by splitting it into smaller PRs or changing its base branch\\.$"
  , "^Upgrade to a paid plan to raise the limit\\.$"
  , "^Usage-priced reviews support at most [0-9]+ files\\.$"
  , "^\\[Check out review usage here\\]\\(https://app\\.coderabbit\\.ai/dashboard/review-capacity\\?orgId=[0-9a-f-]+\\)\\.$"
  ];
# The blockquote between the notice markers: an alert marker, one of the listed headings,
# then payload. Accepting any line that merely starts with ">" made "> P1: this drops the
# last row" part of a notice, and a notice needs no disposition.
def cr_notice_payload_ok:
  . as $n
  | ($n | length) >= 2 and all($n[]; startswith(">"))
    and (($n | map(sub("^>[[:space:]]*"; ""))) as $t
         | ($t[0] | test("^\\[!(WARNING|IMPORTANT|NOTE)\\]$"))
           and ($t[1] | test(cr_notice_heading_re))
           and all($t[2:][]; . as $line | any(cr_notice_payload_res[]; . as $re | $line | test($re))));
def is_cr_summary_notice:
  (. // "") as $b
  | [$b | scan(cr_marker_re)] as $m
  | ($m | length) == 2 and $m[0] == cr_summarize_marker
    and (($m[1] | capture(cr_notice_start_re) // null) != null)
    and (($b | split($m[1])) as $p
         | ($p | length) == 2
           and (($p[1] | split("<!-- end of auto-generated comment: \($m[1] | capture(cr_notice_start_re).k) by coderabbit.ai -->")) as $q
                | ($q | length) == 2
                  and (($q[0] | split("\n") | map(gsub("^[[:space:]]+|[[:space:]]+$"; "")) | map(select(length > 0)))
                       | cr_notice_payload_ok)
                  and ((($p[0] + $q[1]) | strip_accounted(["cr_tips", "html_comment"])) as $rest
                       | $rest != null and ($rest | test("[^[:space:]]") | not))));
def is_cr_notice: is_cr_reply_notice or is_cr_summary_notice;'
# Only these bots issue verdicts, and only from the account GitHub types as a Bot.
# Anyone else posting the same words has written an ordinary comment: claimable like
# any other, never coverage. Entries are logins; their mention handles (@codex,
# @coderabbitai) are the ones TRIGGER_RE accepts.
VERDICT_BOTS=${VERDICT_BOTS:-chatgpt-codex-connector,coderabbitai}
# The review summary belongs to ONE of them. Accepting it from any listed bot made a
# CodeRabbit comment carrying the marker a Codex verdict: exempt from dispositions, with its
# table read as head coverage.
CODEX_LOGIN=${CODEX_LOGIN:-chatgpt-codex-connector}
# Each notice belongs to its bot the same way: the quota notice is Codex's, the rate-limit,
# not-completed and summary notices are CodeRabbit's. From any other account they are
# ordinary comments.
CODERABBIT_LOGIN=${CODERABBIT_LOGIN:-coderabbitai}

prauthor=$(jq -r '.data.repository.pullRequest.author.login // ""' <<<"$payload" 2>/dev/null)

# A coverage-bearing comment must still say, at the END of the run, what it said at the
# start. `bodies` above was built from the FIRST read of the comments; the walkthrough and
# the review summary are edited in place by their bots, so a clean result captured at
# 22:11 can be a "Review failed" notice by 22:15 with the head never moving. Comparing the
# two reads is the only way to notice, and the gate must block rather than certify a head
# from a comment it can no longer quote. Every path compares, including --from-file: the
# payload carries the second read under `recheck`, which is what fetch_payload fills, so a
# fixture exercises the same code the live run does.
#
# The sort_by here is the one place a timestamp is not parsed, deliberately: it puts the two
# fingerprints in one canonical order so they can be compared for EQUALITY, and never decides
# which of two instants came first. A timestamp that differs between the reads, in value or in
# shape, makes the fingerprints differ, which blocks.
COVERAGE_FP_JQ="$VERDICT_JQ"'($bots | split(",")) as $botlogins
  | [ .[] | select(((.author.__typename // "") == "Bot") and (.author.login | IN($botlogins[])))
          | select(((.body // "") | is_verdict)
                   or (((.author.login // "") == $codex) and ((.body // "") | is_codex_summary))
                   or ((.body // "") | contains(cr_summarize_marker)))
          | {login: .author.login, createdAt: (.createdAt // ""),
             updatedAt: (.updatedAt // ""), body: (.body // "")} ]
  | sort_by([.createdAt, .login, .body])'
capturedfp=$(jq -c --arg bots "$VERDICT_BOTS" --arg codex "$CODEX_LOGIN" "$COVERAGE_FP_JQ" <<<"$prcomments") || {
  echo "verdict: BLOCKED — could not read the coverage-bearing comments." >&2; exit 2; }
if [ "$capturedfp" != "[]" ]; then
  recheckcomments=$(jq -c '.data.repository.pullRequest.recheck.comments.nodes // "absent"' \
    <<<"$payload" 2>/dev/null)
  if [ "$recheckcomments" = '"absent"' ] || [ -z "$recheckcomments" ]; then
    echo "verdict: BLOCKED — this PR carries a comment that can grant head coverage, and the" >&2
    echo "payload has no second read of it to compare against. Those comments are edited in" >&2
    echo "place, so one read cannot show what they said when the run ended. Re-run the gate," >&2
    echo "or regenerate the fixture with --dump-payload." >&2
    exit 2
  fi
  if ! jq -e 'type == "array" and all(((.body // "") | type) == "string")' \
       >/dev/null 2>&1 <<<"$recheckcomments"; then
    echo "verdict: BLOCKED — the second read of the PR comments is not an array of comments." >&2
    exit 2
  fi
  recheckfp=$(jq -c --arg bots "$VERDICT_BOTS" --arg codex "$CODEX_LOGIN" "$COVERAGE_FP_JQ" <<<"$recheckcomments") || {
    echo "verdict: BLOCKED — could not read the re-fetched coverage-bearing comments." >&2; exit 2; }
  if [ "$capturedfp" != "$recheckfp" ]; then
    echo "verdict: BLOCKED — a comment that can grant head coverage changed under us while" >&2
    echo "this gate was reading the PR. Whatever it says now, the reading below was taken" >&2
    echo "from what it said before. Re-run against a PR that is holding still." >&2
    exit 2
  fi
fi

# The verdict is computed from the FINAL read of the comments, not the one the run opened
# with. The fingerprint above only covers comments that can grant coverage, so everything
# else that moved while the gate paged threads used to be invisible: a P1 posted mid-run was
# never judged, and a disposition deleted mid-run still answered its claim. Where the payload
# carries no second read there is nothing later to judge, and the first read is the final one.
finalcomments=$(jq -c '.data.repository.pullRequest.recheck.comments.nodes // "absent"' \
  <<<"$payload" 2>/dev/null)
if [ "$finalcomments" != '"absent"' ] && [ -n "$finalcomments" ]; then
  prcomments=$finalcomments
  if ! jq -e "$TS_JQ"'type == "array" and all(((.body // "") | type) == "string"
                  and (((.body // "") | test("[^[:space:]]") | not)
                       or (((.createdAt | ts) != null) and ((.updatedAt | ts) != null))))' \
       >/dev/null 2>&1 <<<"$prcomments"; then
    echo "verdict: BLOCKED — the second read of the PR comments has a comment this gate" >&2
    echo "cannot judge: a non-text body, or no parsable createdAt/updatedAt." >&2
    exit 2
  fi
fi

#
# Each top-level comment also records `reviewed_rows`: every {sha, at} pair by which a
# listed bot says it reviewed a commit, and when it said so. There are three sources, and
# each dates itself differently, which is the whole difficulty:
#   - Codex's legacy verdict names one commit and is posted once, so its createdAt is the
#     time of the result.
#   - Codex's review-summary comment names one commit per table row and is edited in place,
#     so only a row's own datetime dates that row.
#   - CodeRabbit's walkthrough names the range it reviewed and is edited in place, so its
#     updatedAt is the time of the result.
# The coverage check below asks only whether some row names the head and postdates its
# arrival, so a comment carrying several results is judged row by row.
bodies=$(jq -s --arg ignore "$IGNORED_COMMENT_AUTHORS" --arg trig "$TRIGGER_RE" --arg bots "$VERDICT_BOTS" \
   --arg codex "$CODEX_LOGIN" --arg cr "$CODERABBIT_LOGIN" --arg me "$prauthor" \
   "$VERDICT_JQ"'($ignore | split(",")) as $skip
  | ($bots | split(",")) as $botlogins
  | (.[0] | map({author, state, body, at: .submittedAt,
                 claimable: (((.state // "COMMENTED") != "APPROVED")
                             and (($me == "") or ((.author.login // "") != $me)))}))
  + (.[1] | map(. as $c
      | (($c.author.__typename // "") == "Bot" and ($c.author.login | IN($botlogins[]))) as $listed
      | ($listed and ($c.body | is_verdict)) as $v
      | ($listed and (($c.author.login // "") == $codex) and ($c.body | is_codex_summary)) as $sum
      | ($listed and ((($c.author.login // "") == $codex and ($c.body | is_codex_limit_notice))
                      or (($c.author.login // "") == $cr and ($c.body | is_cr_notice)))) as $notice
      | {author, state: "COMMENT", body, at: .updatedAt,
         verdict: ($v or $sum), notice: $notice,
         reviewed_rows: ((if $notice then []
                          elif $v then [{sha: ($c.body | verdict_sha), at: .createdAt}]
                          elif $sum then ($c.body | summary_rows)
                          elif $listed then [{sha: ($c.body | walkthrough_sha), at: (.updatedAt // "")}]
                          else [] end)
                         | map(select((.sha // "") != "" and (.at // "") != ""))),
         claimable: ((.author.login | IN($skip[]) | not)
                     and ((.body // "") | test($trig; "i") | not)
                     and (($v or $sum or $notice) | not))}))' \
         <(echo "$reviews") <(echo "$prcomments")) || {
  echo "verdict: BLOCKED — could not merge PR-level bodies." >&2; exit 2; }

# Observability seam for the probe. Paging is about what was FETCHED, and the verdict is a
# poor proxy for that — the probe authors everything it posts, and the author-exclusion rule
# above means it cannot manufacture a PR-level claim to swing the verdict with. Counting the
# fetch directly tests the thing, without a flag that disables any rule.
if [ "${GATE_DEBUG_COUNTS:-0}" = "1" ]; then
  echo "fetched: threads=$(jq 'length' <<<"$threads") reviews=$(jq 'length' <<<"$reviews") prcomments=$(jq 'length' <<<"$prcomments")"
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
# A previous version also demanded the disposition be POSITIONALLY LAST — newer than every
# non-disposition comment in the thread. That is withdrawn, deliberately. It was aimed at a
# real case (someone raises a second defect after the first is answered, and the thread gets
# resolved anyway), but the only way to implement it without guessing what a defect looks
# like was to treat every later reply as a new claim — so an ordinary "Thanks, confirmed"
# after a RED-VERIFIED reply blocked a fully adjudicated thread. It traded a rare fail-open
# for a routine fail-closed, which is the worse of the two: a gate that cries wolf on normal
# conversation gets switched off, and then it catches nothing at all.
#
# What remains is that a disposition must EXIST as a reply. Resolving is a deliberate act;
# resolving a thread whose latest message you have not answered is a human failure this
# script does not try to model.
undisposed=$(jq -r --arg re "$disposition_re" '[ .[] | select(.isResolved == true)
  | select(any(.comments.nodes[1:][]; (.body // "") | test($re)) | not) ] | length' \
  <<<"$threads") || {
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
# itself, compared as instants (`ts`): as strings, an answer posted 0.1s BEFORE a claim in
# the same second sorted after it, because "Z" sorts after ".". An answer whose timestamp
# will not parse is dropped, and a claim whose timestamp will not parse cannot be shown to
# have been answered, so it stays outstanding; the validation above blocks both shapes
# first, and this keeps the comparison itself fail-closed rather than resting on that.
unanswered=$(jq -r --arg re "$disposition_re" "$TS_JQ"'
  [ .[] | select((.body // "") | test($re)) | (.at | ts) | select(. != null) ] as $answers
  | [ .[] | select(.claimable)
          | select((.body // "") | test("[^[:space:]]"))
          | select(((.body // "") | test($re)) | not)
          | (.at | ts) as $cat
          | select($cat == null or ([ $answers[] | select(. > $cat) ] | length == 0)) ]
  | length' <<<"$bodies") || {
  echo "verdict: BLOCKED — could not evaluate PR-level bodies." >&2; exit 2; }

if [ "${unanswered:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$disposition_re" "$TS_JQ"'
    [ .[] | select((.body // "") | test($re)) | (.at | ts) | select(. != null) ] as $answers
    | .[] | select(.claimable)
    | select((.body // "") | test("[^[:space:]]"))
    | select(((.body // "") | test($re)) | not)
    | (.at | ts) as $cat
    | select($cat == null or ([ $answers[] | select(. > $cat) ] | length == 0))
    | "  UNANSWERED \(.author.login) (\(.state))\n    \(.body | split("\n")[0][0:150])"' <<<"$bodies"
  echo
  echo "verdict: BLOCKED — $unanswered PR-level review body/bodies carry no disposition."
  echo "A finding in a review body is not a thread and cannot be 'resolved'. Post a review"
  echo "on the PR OPENING with one of RED-VERIFIED: / NOT-A-DEFECT: / DISAGREE: — the same"
  echo "answers an inline finding requires, and dated after the finding it answers."
  rc=1
fi

# Answering every finding proves nothing if nobody has reviewed the CODE YOU ARE MERGING.
# This gate shipped with five open findings for exactly that reason: the branch was pushed,
# the previous round's threads were answered, the gate went CLEAR, and it merged 36 seconds
# later — while the reviewer was still working. Its findings arrived five minutes after the
# squash. Nothing was bypassed; the gate simply had no notion of review COVERAGE.
#
# Skipped only when REQUIRE_REVIEWED_HEAD=0. It used to skip as well when no review
# object had ever been submitted, on the theory that a PR nobody has reviewed is the
# required-checks ruleset's business. That was the fail-open: a no-findings verdict
# leaves no review object, so a PR whose only review ever was a clean Codex pass on an
# OLDER commit had no review objects at all, and after a push it went CLEAR with nothing
# reviewing the head. The head must be covered whether or not any review object exists.
if [ "${REQUIRE_REVIEWED_HEAD:-1}" = "1" ]; then
  # Only SOMEONE ELSE's review is coverage. Counting any review on the head meant the
  # author's own `gh pr review --comment` disposition — posted after the push, as the
  # workflow requires — marked the commit reviewed. The check certified precisely the
  # race it was built to catch.
  #
  # And a review OBJECT on the head is not the same thing as a review of it. GitHub mints
  # an empty COMMENTED review as the CONTAINER for every reply to an inline thread, bound
  # to whatever the head is at the time. #897 merged on head 27b1b943 with its last two
  # commits reviewed by nobody: CodeRabbit reviewed the previous head 34f66d47, posted two
  # findings, was refused under Fair Usage on every request after that, and the two
  # containers minted when it replied in the answered threads
  #     [coderabbitai] COMMENTED body_len=0 14:16:37Z commit=27b1b943
  #     [coderabbitai] COMMENTED body_len=0 14:16:41Z commit=27b1b943
  # counted as coverage. The gate said CLEAR and the merge went through 12 minutes later.
  # #901 merged the same way 40 minutes after that, on head e35ff1a2. This is the same
  # fail-open as the missing `jq` and the rate-limited CodeRabbit check: "the reviewer never
  # ran" reading as "the reviewer approved".
  #
  # So an object counts only when it carries something that could ONLY exist because a
  # review ran. Three things can, and a reply container has none of them:
  #   - a non-empty BODY. The reviewer wrote a summary. A container's body is "".
  #   - a state of APPROVED or CHANGES_REQUESTED. GitHub mints those only from the review
  #     form; a reply container is always COMMENTED. DISMISSED is a review withdrawn and
  #     PENDING one never submitted, so neither covers on its own.
  #   - an inline comment of its own that is not a reply, which is a finding this review
  #     placed. No review on ejc3/fcvm has that shape today: every bot review here writes a
  #     summary body (checked across #844, #853, #867, #872, #874, #887, #893, #897, #901).
  #     It is admitted because POST /pulls/N/reviews takes `event: COMMENT` with `comments`
  #     and no `body`, so a reviewer who only comments inline submits it, and refusing that
  #     would leave the head uncoverable once its threads were answered. A gate that cries
  #     wolf on a real review gets switched off, and then it catches nothing.
  # Emptiness is judged on the object, not on who wrote it. Authorship has decided nothing
  # in this gate since the claimable rule was rewritten, and a human's reply container is
  # as empty as a bot's: #897 carried two of each.
  covering_jq='def covers:
      (((.body // "") | type) == "string" and ((.body // "") | test("[^[:space:]]")))
      or (((.state // "") | type) == "string" and ((.state // "") | IN("APPROVED", "CHANGES_REQUESTED")))
      or (any(.comments.nodes[]?; has("replyTo") and .replyTo == null));'
  headreviews=$(jq -c --arg h "$headoid" --arg me "$prauthor" \
             '[ .[] | select((.commit.oid // "") == $h) | select(.author.login != $me) ]' \
             <<<"$reviews") || {
    echo "verdict: BLOCKED — could not evaluate head-commit review coverage." >&2; exit 2; }
  # A review object this gate cannot read is not one it may wave through as coverage, and
  # not one it may silently drop either. `state` is what separates a submitted verdict from
  # a reply container, so a non-string one is unreadable and blocks.
  if ! jq -e 'all(((.state // null) | type) == "string")' >/dev/null 2>&1 <<<"$headreviews"; then
    echo "verdict: BLOCKED — a review object on the head has no readable state, so this" >&2
    echo "gate cannot tell a submitted review from the empty container GitHub mints for a" >&2
    echo "reply to a comment thread. Re-run, or re-capture the fixture." >&2
    exit 2
  fi
  if ! jq -e 'all((has("comments") | not)
                  or (((.comments | type) == "object") and ((.comments.nodes | type) == "array")))' \
       >/dev/null 2>&1 <<<"$headreviews"; then
    echo "verdict: BLOCKED — a review object on the head has an unreadable comments" >&2
    echo "connection, so this gate cannot tell whether that review placed a finding." >&2
    exit 2
  fi
  # A comments connection that does not account for every comment it reports cannot answer
  # "did this review place a finding of its own": the one non-reply may be the comment that
  # was not fetched. Only asked of an object that is not already coverage by body or state.
  # An ABSENT connection is a different thing and declines rather than blocking: it is the
  # shape of every fixture captured before this gate read review comments, and no evidence
  # is not evidence of a review.
  truncated=$(jq -r "$covering_jq"'[ .[] | select(covers | not) | select(has("comments"))
    | select((((.comments.totalCount | type) == "number")
              and (.comments.totalCount == (.comments.nodes | length))) | not) ] | length' \
    <<<"$headreviews") || {
    echo "verdict: BLOCKED — could not evaluate the head reviews' comment connections." >&2; exit 2; }
  if [ "${truncated:-0}" -gt 0 ]; then
    echo "verdict: BLOCKED — $truncated review object(s) on the head report more inline" >&2
    echo "comments than were fetched, so this gate cannot tell whether they placed a" >&2
    echo "finding or only replied to one. Re-run, or re-capture the fixture." >&2
    exit 2
  fi
  reviewed=$(jq -r "$covering_jq"'[ .[] | select(covers) ] | length' <<<"$headreviews") || {
    echo "verdict: BLOCKED — could not evaluate head-commit review coverage." >&2; exit 2; }
  # A no-findings result leaves no review object (see VERDICT_JQ), so it carries no commit
  # of its own. It counts for THIS head only when the bot itself names the head. A result
  # merely dated after the head arrived is not bound: a review of the old head that
  # finishes after the push is dated the same way. What names the head, per bot:
  #   - Codex writes the commit it reviewed into the verdict. That sha must NAME the head.
  #     A verdict naming an older commit was issued for that commit and is ignored here (it
  #     still needs no disposition).
  #   - Codex's review-summary comment (the shape it has posted since 2026-08-28) names a
  #     commit per table row, and a row counts when it says Completed, names the head, and
  #     its own datetime postdates arrival. A table holding any row that is not a finished
  #     Completed review covers nothing at all, because a review still running is one whose
  #     findings have not landed. When Codex does have findings it
  #     posts a review object, which `reviewed` counts, and those findings answer to
  #     dispositions exactly as they did before.
  #   - CodeRabbit's "Full review finished." reply names nothing, so it never covers a
  #     head on its own: it is a notice, exempt from dispositions and nothing more. What
  #     names the head is the walkthrough comment CodeRabbit edits in place. After a
  #     review that finished with no comments it carries, between "<!-- recent_review_start -->"
  #     and "<!-- recent_review_end -->", the line "No actionable comments were generated"
  #     and the line "Reviewing files that changed from the base of the PR and between
  #     <sha> and <sha>". The second sha is the last commit reviewed and must be the head,
  #     and the comment's updatedAt must postdate the head's arrival. (A review that did
  #     produce comments leaves a review object on the head, which `reviewed` counts.)
  #     Only that block, in a comment carrying no notice, counts (walkthrough_sha): the
  #     same comment quotes an identical range line inside its "Review limit reached"
  #     notice for a review that did NOT run (on #872 that notice named head 181fcbbb,
  #     which no CodeRabbit review ever covered), and after a FAILED review the block
  #     itself names the range under a "Review failed" caution (#853, #792).
  # An earlier version bound CodeRabbit's reply to the latest review request posted after
  # arrival. That is timestamp ordering, not binding: a review of the old head that
  # finished after a new request was counted as that request's answer. Withdrawn.
  # Arrival is the creation of the head's earliest check suite, not committedDate: a
  # commit made locally before an older head's verdict and pushed afterwards would
  # otherwise read as covered. Without a check suite nothing can be dated after arrival
  # and the head stays uncovered. Earliest is by INSTANT: as strings, a whole-second
  # timestamp sorts after a fractional one in the same second, so the string minimum can
  # name a later suite than the one that actually created first. And an arrival that will
  # not parse dates nothing, which is a block: as a string it still ranked against every
  # verdict, and one sorting below them certified the head.
  # A sha a bot names is usually ABBREVIATED (Codex writes 7 characters), and a prefix is
  # not an identity: reviewed commit `deadbee0` and head `deadbeef` share `deadbee`, so
  # matching the head with startswith let a result for the old head certify the new one.
  # An abbreviation is resolved against the PR's own commits, which is the only universe a
  # review row can be naming, and it covers the head only when it resolves to exactly one
  # commit and that commit IS the head. A full sha is compared as an identity and needs no
  # resolution. An ambiguous or unresolvable abbreviation names nothing and binds nothing.
  # A FRAGMENT of the commit list is worse than none of it: `deadbee` is ambiguous across
  # the PR's real commits and unique across the hundred that happened to be fetched, and the
  # second reading certifies a head nobody reviewed. The list is paged to completion above
  # and carries the totalCount GitHub reported, so a payload whose list does not account for
  # every commit blocks. An ABSENT list is a different thing and stays allowed: no universe
  # resolves no abbreviation, which is the fail-closed direction, and it is the shape of
  # every fixture captured before this gate read the commits at all.
  if [ "$(jq -r '.data.repository.pullRequest | has("prcommits")' <<<"$payload" 2>/dev/null)" = "true" ] \
     && ! jq -e '.data.repository.pullRequest.prcommits
                 | (.totalCount | type) == "number" and (.totalCount == (.nodes | length))' \
          >/dev/null 2>&1 <<<"$payload"; then
    echo "verdict: BLOCKED — the PR's commit list does not account for every commit on the" >&2
    echo "PR, so an abbreviated sha in a review result would resolve against a fragment of" >&2
    echo "it and could name the head by accident. Re-run, or re-capture the fixture." >&2
    exit 2
  fi
  proids=$(jq -c '[.data.repository.pullRequest.prcommits.nodes[]?.commit.oid // empty]' \
    <<<"$payload" 2>/dev/null) || {
    echo "verdict: BLOCKED — could not read the PR's commit list." >&2; exit 2; }
  if ! jq -e 'type == "array" and all(type == "string")' >/dev/null 2>&1 <<<"$proids"; then
    echo "verdict: BLOCKED — the PR's commit list is not a list of object ids, so an" >&2
    echo "abbreviated sha in a review result cannot be resolved to a commit." >&2
    exit 2
  fi
  suitetimes=$(jq -c '[.data.repository.pullRequest.commits.nodes[0].commit.checkSuites.nodes[]?.createdAt // empty]' \
    <<<"$payload" 2>/dev/null) || {
    echo "verdict: BLOCKED — could not read the head commit's check suites." >&2; exit 2; }
  if ! jq -e "$TS_JQ"'type == "array" and all(ts != null)' >/dev/null 2>&1 <<<"$suitetimes"; then
    echo "verdict: BLOCKED — a check-suite timestamp on the head will not parse, so this gate" >&2
    echo "cannot tell when the head arrived, and cannot date any review result against it." >&2
    exit 2
  fi
  arrived=$(jq -r "$TS_JQ"'min_by(ts) // ""' <<<"$suitetimes") || {
    echo "verdict: BLOCKED — could not date the head's arrival." >&2; exit 2; }
  verdicts=$(jq -r --arg me "$prauthor" --arg hd "$arrived" --arg h "$headoid" \
             --argjson oids "$proids" "$TS_JQ"'
    ($hd | ts) as $hat
    | ($h | ascii_downcase) as $hl
    | def names_head($sha):
        ($sha | ascii_downcase) as $s
        | if $s == "" then false
          elif $s == $hl then true
          else ([ $oids[] | ascii_downcase | select(startswith($s)) ] | unique) as $cand
               | ($cand | length) == 1 and ($cand[0] == $hl)
          end;
      [ .[] | select(.state == "COMMENT" and .author.login != $me)
        | select(any(.reviewed_rows[]?; . as $row
                     | ($row.at | ts) as $rat
                     | ($row.sha // "") != "" and names_head($row.sha)
                     and $hat != null and $rat != null and $rat > $hat)) ]
    | length' <<<"$bodies") || {
    echo "verdict: BLOCKED — could not evaluate no-findings verdicts." >&2; exit 2; }
  # A head is covered by a review object on it from someone other than the author, or by
  # a verdict bound to it. Nothing else counts, and in particular the existence of review
  # objects on OTHER commits does not: the blocking branch used to require one, so a PR
  # with no review objects and an unbound verdict printed neither line and went CLEAR.
  if [ "${reviewed:-0}" -eq 0 ] && [ "${verdicts:-0}" -gt 0 ]; then
    echo
    echo "  HEAD COVERED  ${headoid:0:9}: no review object, but $verdicts no-findings verdict(s) bound to it (arrived $arrived)"
  elif [ "${reviewed:-0}" -eq 0 ]; then
    echo
    echo "  UNREVIEWED HEAD  ${headoid:0:9} — no review of this commit from anyone but the author (an empty review object is a reply container, not a review), and no no-findings verdict bound to it"
    echo
    echo "verdict: BLOCKED — the head commit has not been reviewed."
    echo "Every finding raised so far is answered, but the code being merged is not the code"
    echo "anyone reviewed. Wait for a review of ${headoid:0:9}, or re-request one."
    rc=1
  fi
fi

if [ "$rc" -eq 0 ]; then
  echo "verdict: CLEAR — every thread resolved and disposed, every review body answered."
fi
exit $rc
