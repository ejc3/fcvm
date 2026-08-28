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

fetch_payload() {
  local pr=$1 cursor=null threads='[]' reviews='[]' prcomments='[]' prauthor='' headoid=''

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
              nodes { author { login } state body submittedAt commit { oid } }
            } } } }" 2>/dev/null) || return 1
    if [ "$(jq -r '.data.repository.pullRequest // "null"' <<<"$rresp")" = "null" ]; then
      echo "verdict: BLOCKED — no pull request #$pr in $REPO_OWNER/$REPO_NAME (or it is" >&2
      echo "not visible to this token). Refusing to report CLEAR for a PR never read." >&2
      return 2
    fi
    reviews=$(jq -s '.[0] + (.[1].data.repository.pullRequest.reviews.nodes // [])' \
          <(echo "$reviews") <(echo "$rresp"))
    prauthor=$(jq -r '.data.repository.pullRequest.author.login // ""' <<<"$rresp")
    headoid=$(jq -r '.data.repository.pullRequest.headRefOid // ""' <<<"$rresp")
    headdate=$(jq -r '.data.repository.pullRequest.commits.nodes[0].commit.committedDate // ""' <<<"$rresp")
    headsuites=$(jq -c '.data.repository.pullRequest.commits.nodes[0].commit.checkSuites.nodes // []' <<<"$rresp")
    [ "$(jq -r '.data.repository.pullRequest.reviews.pageInfo.hasNextPage' <<<"$rresp")" = "true" ] || break
    rcursor=$(jq -r '.data.repository.pullRequest.reviews.pageInfo.endCursor' <<<"$rresp")
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
    prcomments=$(jq -s '.[0] + (.[1].data.repository.pullRequest.comments.nodes // [])' \
          <(echo "$prcomments") <(echo "$cresp2"))
    [ "$(jq -r '.data.repository.pullRequest.comments.pageInfo.hasNextPage' <<<"$cresp2")" = "true" ] || break
    ccursor2=$(jq -r '.data.repository.pullRequest.comments.pageInfo.endCursor' <<<"$cresp2")
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
              nodes { author { login } path line originalLine body } } } } }" 2>/dev/null) || {
        # NOT `break`. A transient API/auth/rate-limit failure mid-thread used to return
        # the pages fetched so far as though they were the whole conversation — so an
        # early disposition could certify a thread nobody finished reading.
        echo "verdict: BLOCKED — could not page comments for thread $tid." >&2
        return 1
      }
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
    recheckcomments=$(jq -s '.[0] + (.[1].data.repository.pullRequest.comments.nodes // [])' \
          <(echo "$recheckcomments") <(echo "$rc2resp"))
    [ "$(jq -r '.data.repository.pullRequest.comments.pageInfo.hasNextPage' <<<"$rc2resp")" = "true" ] || break
    rc2cursor=$(jq -r '.data.repository.pullRequest.comments.pageInfo.endCursor' <<<"$rc2resp")
  done

  # Re-read the head AFTER all paging. A push landing mid-evaluation would otherwise leave
  # $headoid describing the commit captured by the first query, and the coverage check
  # would happily certify a SHA the PR no longer points at.
  local headnow
  headnow=$(gh api graphql -f query="
    { repository(owner: \"$REPO_OWNER\", name: \"$REPO_NAME\") {
        pullRequest(number: $pr) { headRefOid } } }" \
    --jq '.data.repository.pullRequest.headRefOid' 2>/dev/null) || return 1
  if [ -n "$headnow" ] && [ "$headnow" != "$headoid" ]; then
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
     '{data:{repository:{pullRequest:{author:{login:$a}, headRefOid:$h,
                                      commits:{nodes:[{commit:{committedDate:$d, checkSuites:{nodes:$cs}}}]},
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
if ! jq -e 'all(((.body // "") | test("[^[:space:]]") | not) or ((.submittedAt | type) == "string"))' \
     >/dev/null 2>&1 <<<"$reviews"; then
  echo "verdict: BLOCKED — a non-empty review body has no submittedAt timestamp, so this" >&2
  echo "gate cannot tell whether the answer came before or after the claim." >&2
  exit 2
fi
if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$prcomments"; then
  echo "verdict: BLOCKED — top-level PR comment data is not an array." >&2
  exit 2
fi
if ! jq -e 'all(((.body // "") | type) == "string"
                and (((.body // "") | test("[^[:space:]]") | not) or ((.createdAt | type) == "string")))' \
     >/dev/null 2>&1 <<<"$prcomments"; then
  echo "verdict: BLOCKED — a top-level PR comment has a non-text body or no createdAt." >&2
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
# either way. `at` normalises submittedAt/createdAt.
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
#     CHANGES_REQUESTED carry findings and need answers — from anyone, author included.
#   - TOP-LEVEL COMMENTS have no state, so everything counts unless it is on a short,
#     documented ignore list: bot logins that only ever post notifications, and the
#     literal trigger phrases this skill tells you to post. An unknown bot counts.
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
VERDICT_JQ='def cr_note: "> Note: CodeRabbit is an incremental review system and does not re-review already reviewed commits. This command is applicable only when automatic reviews are paused.";
def verdict_lines:
  ((. // "") | gsub("<!--(.|\n)*?-->"; "")
    | gsub("<details> <summary>ℹ️ About Codex in GitHub</summary>(.|\n)*?</details>"; "")
    | split("\n") | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
    | map(select(length > 0 and . != cr_note)));
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
def is_codex_summary: ((. // "") | contains(codex_summary_marker));
def summary_cells:
  split("|") | map(gsub("^[[:space:]]+|[[:space:]]+$"; ""))
  | (if (.[0] // "") == "" then .[1:] else . end)
  | (if (.[-1] // "") == "" then .[:-1] else . end);
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
     | capture(cr_range_re) | .sha ] | first) // "";'
# Only these bots issue verdicts, and only from the account GitHub types as a Bot.
# Anyone else posting the same words has written an ordinary comment: claimable like
# any other, never coverage. Entries are logins; their mention handles (@codex,
# @coderabbitai) are the ones TRIGGER_RE accepts.
VERDICT_BOTS=${VERDICT_BOTS:-chatgpt-codex-connector,coderabbitai}

prauthor=$(jq -r '.data.repository.pullRequest.author.login // ""' <<<"$payload" 2>/dev/null)
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
   "$VERDICT_JQ"'($ignore | split(",")) as $skip
  | ($bots | split(",")) as $botlogins
  | (.[0] | map({author, state, body, at: .submittedAt,
                 claimable: ((.state // "COMMENTED") != "APPROVED")}))
  + (.[1] | map(. as $c
      | (($c.author.__typename // "") == "Bot" and ($c.author.login | IN($botlogins[]))) as $listed
      | ($listed and ($c.body | is_verdict)) as $v
      | ($listed and ($c.body | is_codex_summary)) as $sum
      | {author, state: "COMMENT", body, at: .createdAt,
         verdict: ($v or $sum),
         reviewed_rows: ((if $v then [{sha: ($c.body | verdict_sha), at: .createdAt}]
                          elif $sum then ($c.body | summary_rows)
                          elif $listed then [{sha: ($c.body | walkthrough_sha), at: (.updatedAt // "")}]
                          else [] end)
                         | map(select((.sha // "") != "" and (.at // "") != ""))),
         claimable: ((.author.login | IN($skip[]) | not)
                     and ((.body // "") | test($trig; "i") | not)
                     and (($v or $sum) | not))}))' \
         <(echo "$reviews") <(echo "$prcomments")) || {
  echo "verdict: BLOCKED — could not merge PR-level bodies." >&2; exit 2; }

# A coverage-bearing comment must still say, at the END of the run, what it said at the
# start. `bodies` above was built from the FIRST read of the comments; the walkthrough and
# the review summary are edited in place by their bots, so a clean result captured at
# 22:11 can be a "Review failed" notice by 22:15 with the head never moving. Comparing the
# two reads is the only way to notice, and the gate must block rather than certify a head
# from a comment it can no longer quote. Every path compares, including --from-file: the
# payload carries the second read under `recheck`, which is what fetch_payload fills, so a
# fixture exercises the same code the live run does.
COVERAGE_FP_JQ="$VERDICT_JQ"'($bots | split(",")) as $botlogins
  | [ .[] | select(((.author.__typename // "") == "Bot") and (.author.login | IN($botlogins[])))
          | select(((.body // "") | is_verdict) or ((.body // "") | is_codex_summary)
                   or ((.body // "") | contains(cr_summarize_marker)))
          | {login: .author.login, createdAt: (.createdAt // ""),
             updatedAt: (.updatedAt // ""), body: (.body // "")} ]
  | sort_by([.createdAt, .login, .body])'
capturedfp=$(jq -c --arg bots "$VERDICT_BOTS" "$COVERAGE_FP_JQ" <<<"$prcomments") || {
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
  recheckfp=$(jq -c --arg bots "$VERDICT_BOTS" "$COVERAGE_FP_JQ" <<<"$recheckcomments") || {
    echo "verdict: BLOCKED — could not read the re-fetched coverage-bearing comments." >&2; exit 2; }
  if [ "$capturedfp" != "$recheckfp" ]; then
    echo "verdict: BLOCKED — a comment that can grant head coverage changed under us while" >&2
    echo "this gate was reading the PR. Whatever it says now, the reading below was taken" >&2
    echo "from what it said before. Re-run against a PR that is holding still." >&2
    exit 2
  fi
fi

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
# itself; ISO-8601 timestamps compare lexicographically, and the validation above
# guarantees every non-empty body has one.
unanswered=$(jq -r --arg re "$disposition_re" '
  [ .[] | select((.body // "") | test($re)) | .at ] as $answers
  | [ .[] | select(.claimable)
          | select((.body // "") | test("[^[:space:]]"))
          | select(((.body // "") | test($re)) | not)
          | . as $claim
          | select([ $answers[] | select(. > $claim.at) ] | length == 0) ]
  | length' <<<"$bodies") || {
  echo "verdict: BLOCKED — could not evaluate PR-level bodies." >&2; exit 2; }

if [ "${unanswered:-0}" -gt 0 ]; then
  echo
  jq -r --arg re "$disposition_re" '
    [ .[] | select((.body // "") | test($re)) | .at ] as $answers
    | .[] | select(.claimable)
    | select((.body // "") | test("[^[:space:]]"))
    | select(((.body // "") | test($re)) | not)
    | . as $claim
    | select([ $answers[] | select(. > $claim.at) ] | length == 0)
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
  reviewed=$(jq -r --arg h "$headoid" --arg me "$prauthor" \
             '[ .[] | select((.commit.oid // "") == $h) | select(.author.login != $me) ] | length' \
             <<<"$reviews") || {
    echo "verdict: BLOCKED — could not evaluate head-commit review coverage." >&2; exit 2; }
  # A no-findings result leaves no review object (see VERDICT_JQ), so it carries no commit
  # of its own. It counts for THIS head only when the bot itself names the head. A result
  # merely dated after the head arrived is not bound: a review of the old head that
  # finishes after the push is dated the same way. What names the head, per bot:
  #   - Codex writes the commit it reviewed into the verdict. That sha must be a prefix
  #     of the head. A verdict naming an older commit was issued for that commit and is
  #     ignored here (it still needs no disposition).
  #   - Codex's review-summary comment (the shape it has posted since 2026-08-28) names a
  #     commit per table row, and a row counts when it says Completed, names a prefix of
  #     the head, and its own datetime postdates arrival. A table holding any row that is
  #     not a finished Completed review covers nothing at all, because a review still
  #     running is one whose findings have not landed. When Codex does have findings it
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
  # and the head stays uncovered.
  arrived=$(jq -r '[.data.repository.pullRequest.commits.nodes[0].commit.checkSuites.nodes[]?.createdAt // empty] | min // ""' <<<"$payload" 2>/dev/null)
  verdicts=$(jq -r --arg me "$prauthor" --arg hd "$arrived" --arg h "$headoid" '
    [ .[] | select(.state == "COMMENT" and .author.login != $me)
        | select(any(.reviewed_rows[]?; . as $row
                     | ($row.sha // "") != "" and ($h | startswith($row.sha))
                     and $hd != "" and (($row.at // "") > $hd))) ]
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
    echo "  UNREVIEWED HEAD  ${headoid:0:9} — no review object on this commit from anyone but the author, and no no-findings verdict bound to it"
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
