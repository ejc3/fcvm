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
# Thread shape: finding, filler, disposition — the disposition last, because it must
# follow the last outstanding comment. At page size 1 it lands on page 3.
#   A. first page only    -> must MISS the disposition (the original truncation)
#   C. full cursor paging -> must SEE it               (shipping behaviour)
#   D/E. the same question for the separate REVIEWS connection, where missing a claim is
#        fail-OPEN rather than fail-closed.
#
# Creates a draft PR and closes it on exit.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
GATE="$HERE/check-review-threads.sh"
[ -x "$GATE" ] || { echo "no gate at $GATE" >&2; exit 2; }

REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || exit 2
OWNER=${REPO%%/*}; NAME=${REPO##*/}
# Unique per invocation. A fixed name meant two concurrent probes force-pushed over each
# other, and teardown could delete a branch the other one was still using — or clobber an
# unrelated branch that happened to share the name.
BRANCH="scratch/gate-pagination-probe-$$-$(date +%s)"
PRNUM=""
# Record BOTH: `git rev-parse --abbrev-ref HEAD` yields the literal "HEAD" when detached,
# and `git checkout HEAD` on the way out would then leave the caller sitting on the scratch
# branch — which also makes deleting that branch fail silently.
START_BRANCH=$(git symbolic-ref -q --short HEAD || true)
START_COMMIT=$(git rev-parse HEAD)

# Never fold the caller's work into the probe commit. `git commit` records the whole index,
# so staged changes would ride along into a branch this script later DELETES — recoverable
# only via reflog. Refuse to run rather than put someone's staged work at risk.
if [ -e PAGINATION_PROBE.md ]; then
  echo "PROBE ABORTED: PAGINATION_PROBE.md already exists. This probe writes, commits and" >&2
  echo "then deletes that path; it will not overwrite your file. Move it first." >&2
  exit 2
fi
if ! git diff --cached --quiet 2>/dev/null; then
  echo "PROBE ABORTED: you have staged changes. This probe commits and then deletes a" >&2
  echo "scratch branch; it will not touch your index. Commit or stash them first." >&2
  exit 2
fi

cleanup() {
  echo "=== teardown ==="
  [ -n "$PRNUM" ] && gh pr close "$PRNUM" --repo "$REPO" --delete-branch >/dev/null 2>&1 \
    && echo "closed PR #$PRNUM, branch deleted"
  if [ -n "$START_BRANCH" ]; then git checkout -q "$START_BRANCH" 2>/dev/null
  else git checkout -q --detach "$START_COMMIT" 2>/dev/null; fi
  git branch -qD "$BRANCH" 2>/dev/null
  rm -f "${GATE_SNAPSHOT:-}" /tmp/gate.A.firstonly.sh /tmp/gate.D.reviewsfirst.sh
}
die() { echo "PROBE ABORTED: $*" >&2; exit 1; }
trap cleanup EXIT

# Copy the shipping gate somewhere stable BEFORE any checkout. Run from the PR that
# introduces or edits this skill, `git checkout origin/main` removes $GATE from the working
# tree, and the verification this script advertises cannot run at all.
GATE_SNAPSHOT=$(mktemp)
cp "$GATE" "$GATE_SNAPSHOT" || die "could not snapshot the gate"
GATE="$GATE_SNAPSHOT"

python3 - "$GATE" <<'PY' || die "could not build comparison gates"
import sys
s = open(sys.argv[1]).read()

# A: never page past the first page.
a = s.replace("for tid in $oversized; do", "for tid in ; do", 1)
assert a != s, "paging loop not found"
open("/tmp/gate.A.firstonly.sh", "w").write(a)

PY

git fetch -q origin main && git checkout -qB "$BRANCH" origin/main || die branch
printf 'scratch\n' > PAGINATION_PROBE.md
# `--only` cannot commit a path git has never seen, so stage it first — safe, because the
# clean-index check above already refused to run on top of anyone's staged work. `--only`
# then guarantees this commit carries that one file and nothing else.
git add PAGINATION_PROBE.md || die "stage probe file"
git commit -q --only PAGINATION_PROBE.md -m "scratch: pagination probe" || die commit
# No --force: the branch name is unique, so a rejected push means a real collision worth
# hearing about rather than something to bulldoze.
git push -q origin "$BRANCH" || die push
# Check the exit status, and only then parse a URL. Piping stderr into `grep -oE '[0-9]+$'`
# meant a FAILED create whose message merely ended in digits (an HTTP status, say) yielded a
# plausible "PR number" — and teardown would then close and delete somebody else's PR.
PR_URL=$(gh pr create --repo "$REPO" --draft --base main --head "$BRANCH" \
  --title "scratch: gate pagination probe (auto-closed)" \
  --body "Throwaway PR verifying thread-comment pagination. Closed automatically.") \
  || die "gh pr create failed: $PR_URL"
PRNUM=$(grep -oE "/pull/[0-9]+$" <<<"$PR_URL" | grep -oE '[0-9]+$')
[ -n "$PRNUM" ] || die "could not parse a PR number from: $PR_URL"
echo "scratch PR #$PRNUM"

CID=$(gh api "repos/$REPO/pulls/$PRNUM/comments" -f body="P1 probe finding: this corrupts output." \
  -f commit_id="$(git rev-parse HEAD)" -f path=PAGINATION_PROBE.md -F line=1 -f side=RIGHT --jq .id) \
  || die "root comment"
# The disposition must come after the last outstanding comment, so: finding, filler,
# disposition. With COMMENTS_PAGE_SIZE=1 the disposition sits on page 3 — invisible to a
# gate that reads only page 1.
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" \
  -f body="filler reply, no disposition" --jq .id >/dev/null || die "filler"
gh api "repos/$REPO/pulls/$PRNUM/comments/$CID/replies" \
  -f body="RED-VERIFIED: scripts/test-check-review-threads.sh" --jq .id >/dev/null || die "disposition"

read -r TID TOTAL < <(gh api graphql -f query="{repository(owner:\"$OWNER\",name:\"$NAME\"){pullRequest(number:$PRNUM){reviewThreads(first:10){nodes{id comments{totalCount}}}}}}" \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[0] | "\(.id) \(.comments.totalCount)"') || die "thread query"
# Assert the setup EXISTS. The first version of this probe skipped the check and
# silently tested a 39-comment thread it believed had 106.
[ "$TOTAL" = "3" ] || die "expected 3 comments, got $TOTAL — replies failed to post"
gh api graphql -f query="mutation{resolveReviewThread(input:{threadId:\"$TID\"}){thread{isResolved}}}" \
  >/dev/null || die resolve
echo "thread of $TOTAL comments, resolved, disposition last (page 3 at size 1)"
echo

rc_all=0
check() { # name, gate, expected exit
  local out rc
  out=$(COMMENTS_PAGE_SIZE=1 REVIEWS_PAGE_SIZE=1 bash "$2" "$PRNUM" 2>&1); rc=$?
  printf '  %-26s exit=%s ' "$1" "$rc"
  if [ "$rc" = "$3" ]; then echo "PASS"; else echo "FAIL (want $3)"; rc_all=1; fi
}
# NOTE: there is deliberately no first-page+last-page variant here any more. Now that a
# disposition must follow the last outstanding comment, the decisive comment is always on
# the LAST page — so first+last and full paging return the same verdict on every input, and
# a check comparing them could never fail. That is the trap this whole suite exists to
# avoid, so it is documented rather than kept as a green-forever test.
echo "comment paging — finding, filler, then the disposition last:"
check "A first page only"    /tmp/gate.A.firstonly.sh 1
check "C full cursor paging" "$GATE"                  0

# The reviews connection is separate and pages separately. Post a defect claim as a REVIEW
# BODY with no disposition: it sits after the (empty-bodied) review that carried the inline
# comment, so a gate that only ever reads the first review page cannot see it.
echo
echo "reviews paging — unanswered claim in a review body on page 2:"
gh pr review "$PRNUM" --repo "$REPO" --comment \
  -b "P1 probe finding posted in a REVIEW BODY, deliberately left unanswered." >/dev/null \
  || die "review body"
python3 - "$GATE" <<'PY' || die "could not build reviews variant"
import sys
s = open(sys.argv[1]).read()
old = """    [ "$(jq -r '.data.repository.pullRequest.reviews.pageInfo.hasNextPage' <<<"$rresp")" = "true" ] || break"""
assert old in s, "reviews paging break not found"
open("/tmp/gate.D.reviewsfirst.sh", "w").write(s.replace(old, "    break", 1))
PY
check "D reviews unpaged"    /tmp/gate.D.reviewsfirst.sh 0
check "E reviews paged"      "$GATE"                     1

echo
[ "$rc_all" = 0 ] \
  && echo "VERIFIED: only a fully paged thread sees a disposition past page 1, and only a" \
  && [ "$rc_all" = 0 ] && echo "separately paged reviews connection sees a claim past the first review page." \
  || echo "pagination NOT verified"
exit $rc_all
