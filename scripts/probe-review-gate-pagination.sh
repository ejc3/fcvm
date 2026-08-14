#!/bin/bash
# Live verification for the ONE part of check-review-threads.sh that fixtures cannot
# reach: paging a thread's comments. `--from-file` cases already contain every comment
# inline, so they pass with or without pagination — they document intent, not a
# regression anyone watched fail.
#
# Reproducing a long thread honestly costs 100+ comment-creation calls, which GitHub
# secondary-rate-limits into SILENT partial failure (a first attempt asked for 105
# replies, got 39, and "passed" for the wrong reason). So shrink the page instead: with
# COMMENTS_PAGE_SIZE=1 a three-comment thread takes the same code path.
#
# The disposition sits at comment #2 of 3 — the MIDDLE — which is the case both earlier
# implementations got wrong. Three strategies, one variable at a time:
#   A. first page only       -> must MISS  (the original truncation)
#   B. first page + last page-> must MISS  (the first:N+last:N approximation: with page
#                                           size 1 it fetches #1 and #3, never #2)
#   C. full cursor paging    -> must SEE   (shipping behaviour)
#
# Creates a draft PR and closes it on exit.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
GATE="$HERE/check-review-threads.sh"
[ -x "$GATE" ] || { echo "no gate at $GATE" >&2; exit 2; }

REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || exit 2
OWNER=${REPO%%/*}; NAME=${REPO##*/}
BRANCH="scratch/gate-pagination-probe"
PRNUM=""
START_REF=$(git rev-parse --abbrev-ref HEAD)

cleanup() {
  echo "=== teardown ==="
  [ -n "$PRNUM" ] && gh pr close "$PRNUM" --repo "$REPO" --delete-branch >/dev/null 2>&1 \
    && echo "closed PR #$PRNUM, branch deleted"
  git checkout -q "$START_REF" 2>/dev/null
  git branch -qD "$BRANCH" 2>/dev/null
}
die() { echo "PROBE ABORTED: $*" >&2; exit 1; }
trap cleanup EXIT

python3 - "$GATE" <<'PY' || die "could not build comparison gates"
import sys
s = open(sys.argv[1]).read()

# A: never page past the first page.
a = s.replace("for tid in $oversized; do", "for tid in ; do", 1)
assert a != s, "paging loop not found"
open("/tmp/gate.A.firstonly.sh", "w").write(a)

# B: the old first:N + last:N approximation — one extra fetch of the TAIL, then stop.
b = s.replace('comments(first: $COMMENTS_PAGE_SIZE, after: \\"$ccursor\\")',
              'comments(last: $COMMENTS_PAGE_SIZE)', 1)
assert b != s, "inner comment query not found"
b2 = b.replace("""if [ "$(jq -r '.data.node.comments.pageInfo.hasNextPage' <<<"$cresp")" = "true" ]; then""",
               "if false; then", 1)
assert b2 != b, "hasNextPage branch not found"
open("/tmp/gate.B.firstlast.sh", "w").write(b2)
PY

git fetch -q origin main && git checkout -qB "$BRANCH" origin/main || die branch
printf 'scratch\n' > PAGINATION_PROBE.md && git add PAGINATION_PROBE.md
git commit -qm "scratch: pagination probe" || die commit
git push -qf origin "$BRANCH" || die push
PRNUM=$(gh pr create --repo "$REPO" --draft --base main --head "$BRANCH" \
  --title "scratch: gate pagination probe (auto-closed)" \
  --body "Throwaway PR verifying thread-comment pagination. Closed automatically." \
  2>&1 | grep -oE '[0-9]+$')
[ -n "$PRNUM" ] || die "pr create"
echo "scratch PR #$PRNUM"

CID=$(gh api "repos/$REPO/pulls/$PRNUM/comments" -f body="P1 probe finding: this corrupts output." \
  -f commit_id="$(git rev-parse HEAD)" -f path=PAGINATION_PROBE.md -F line=1 -f side=RIGHT --jq .id) \
  || die "root comment"
# #2 is the disposition — the middle. #3 is filler posted after it.
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" \
  -f body="RED-VERIFIED: scripts/test-check-review-threads.sh" --jq .id >/dev/null || die "disposition"
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" \
  -f body="filler reply posted after the disposition" --jq .id >/dev/null || die "filler"

read -r TID TOTAL < <(gh api graphql -f query="{repository(owner:\"$OWNER\",name:\"$NAME\"){pullRequest(number:$PRNUM){reviewThreads(first:10){nodes{id comments{totalCount}}}}}}" \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[0] | "\(.id) \(.comments.totalCount)"') || die "thread query"
# Assert the setup EXISTS. The first version of this probe skipped the check and
# silently tested a 39-comment thread it believed had 106.
[ "$TOTAL" = "3" ] || die "expected 3 comments, got $TOTAL — replies failed to post"
gh api graphql -f query="mutation{resolveReviewThread(input:{threadId:\"$TID\"}){thread{isResolved}}}" \
  >/dev/null || die resolve
echo "thread of $TOTAL comments, resolved, disposition is comment #2 (the middle)"
echo

rc_all=0
check() { # name, gate, expected exit
  local out rc
  out=$(COMMENTS_PAGE_SIZE=1 bash "$2" "$PRNUM" 2>&1); rc=$?
  printf '  %-26s exit=%s ' "$1" "$rc"
  if [ "$rc" = "$3" ]; then echo "PASS"; else echo "FAIL (want $3)"; rc_all=1; fi
}
check "A first page only"   /tmp/gate.A.firstonly.sh 1
check "B first page + last"  /tmp/gate.B.firstlast.sh 1
check "C full cursor paging" "$GATE"                  0
echo
[ "$rc_all" = 0 ] && echo "pagination VERIFIED: only full paging sees a disposition in the middle" \
                  || echo "pagination NOT verified"
exit $rc_all
