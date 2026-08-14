#!/bin/bash
# Live verification for the ONE part of check-review-threads.sh that fixtures cannot
# reach: the oversized-thread re-fetch. `--from-file` cases already contain every
# comment inline, so they pass with or without pagination — they document intent, not
# a regression anyone watched fail.
#
# Reproducing an oversized thread honestly costs 100+ comment-creation calls, which
# GitHub secondary-rate-limits into SILENT partial failure (a first attempt asked for
# 105 replies, got 39, and "passed" for the wrong reason). So shrink the page instead:
# with COMMENTS_PAGE_SIZE=1 a three-comment thread takes the same code path.
#
# Three gates, one variable at a time:
#   A. no tail re-fetch     -> must MISS the disposition   (the re-fetch is load-bearing)
#   B. re-fetch + unique_by -> must MISS it too            (unique_by SORTS, so a reply
#                                                           that sorts before the finding
#                                                           becomes nodes[0] and is skipped)
#   C. re-fetch + stable    -> must SEE it                 (shipping behaviour)
#
# Creates a draft PR and closes it on exit. Usage: probe-review-gate-pagination.sh
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
GATE="$HERE/check-review-threads.sh"
[ -x "$GATE" ] || { echo "no gate at $GATE" >&2; exit 2; }

REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || exit 2
OWNER=${REPO%%/*}; NAME=${REPO##*/}
BRANCH="scratch/gate-pagination-probe"
PRNUM=""

cleanup() {
  echo "=== teardown ==="
  [ -n "$PRNUM" ] && gh pr close "$PRNUM" --repo "$REPO" --delete-branch >/dev/null 2>&1 \
    && echo "closed PR #$PRNUM, branch deleted"
  git checkout -q "$START_REF" 2>/dev/null
  git branch -qD "$BRANCH" 2>/dev/null
}
die() { echo "PROBE ABORTED: $*" >&2; exit 1; }

START_REF=$(git rev-parse --abbrev-ref HEAD)
trap cleanup EXIT

python3 - "$GATE" <<'PY' || die "could not build comparison gates"
import sys
s = open(sys.argv[1]).read()
a = s.replace("for tid in $oversized; do", "for tid in ; do", 1)
assert a != s, "re-fetch loop not found"
open("/tmp/gate.A.nopage.sh", "w").write(a)
old = """'def dedupe_stable: reduce .[] as $c ([]; if any(.[]; . == $c) then . else . + [$c] end);
       map(if .id == $id
           then .comments.nodes = ((.comments.nodes + $tail) | dedupe_stable)
           else . end)'"""
new = "'map(if .id == $id then .comments.nodes = (.comments.nodes + $tail | unique_by(.body)) else . end)'"
assert old in s, "stable merge not found"
open("/tmp/gate.B.uniqueby.sh", "w").write(s.replace(old, new, 1))
PY

git fetch -q origin main && git checkout -qB "$BRANCH" origin/main || die "branch"
printf 'scratch\n' > PAGINATION_PROBE.md && git add PAGINATION_PROBE.md
git commit -qm "scratch: pagination probe" || die "commit"
git push -qf origin "$BRANCH" || die "push"
PRNUM=$(gh pr create --repo "$REPO" --draft --base main --head "$BRANCH" \
  --title "scratch: gate pagination probe (auto-closed)" \
  --body "Throwaway PR verifying thread-comment pagination. Closed automatically." \
  2>&1 | grep -oE '[0-9]+$')
[ -n "$PRNUM" ] || die "pr create"
echo "scratch PR #$PRNUM"

CID=$(gh api "repos/$REPO/pulls/$PRNUM/comments" -f body="P2 probe finding: pagination check." \
  -f commit_id="$(git rev-parse HEAD)" -f path=PAGINATION_PROBE.md -F line=1 -f side=RIGHT --jq .id) \
  || die "root comment"
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" -f body="filler reply, no disposition" \
  --jq .id >/dev/null || die "filler reply"
# Sorts BEFORE the finding ("N" < "P") — the input that exposes the unique_by reorder.
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" \
  -f body="NOT-A-DEFECT: probe disposition, deliberately last and alphabetically first." \
  --jq .id >/dev/null || die "disposition reply"

read -r TID TOTAL < <(gh api graphql -f query="{repository(owner:\"$OWNER\",name:\"$NAME\"){pullRequest(number:$PRNUM){reviewThreads(first:10){nodes{id comments{totalCount}}}}}}" \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[0] | "\(.id) \(.comments.totalCount)"') || die "thread query"
# Assert the setup EXISTS. The first version of this probe skipped the check and
# silently tested a 39-comment thread it believed had 106.
[ "$TOTAL" = "3" ] || die "expected 3 comments, got $TOTAL — replies failed to post"
gh api graphql -f query="mutation{resolveReviewThread(input:{threadId:\"$TID\"}){thread{isResolved}}}" \
  >/dev/null || die "resolve"
echo "thread of $TOTAL comments, resolved, disposition last"
echo

rc_all=0
check() { # name, gate, expected exit
  local out rc
  out=$(COMMENTS_PAGE_SIZE=1 bash "$2" "$PRNUM" 2>&1); rc=$?
  printf '  %-24s exit=%s ' "$1" "$rc"
  if [ "$rc" = "$3" ]; then echo "PASS"; else echo "FAIL (want $3)"; rc_all=1; fi
}
check "A no re-fetch"     /tmp/gate.A.nopage.sh   1
check "B unique_by merge" /tmp/gate.B.uniqueby.sh 1
check "C stable merge"    "$GATE"                 0
echo
[ "$rc_all" = 0 ] && echo "pagination VERIFIED: only the shipping merge sees the disposition" \
                  || echo "pagination NOT verified"
exit $rc_all
