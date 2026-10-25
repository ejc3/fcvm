#!/usr/bin/env bash
# After the top of a stack passes CI, mark each lower PR as tested.
#
# Walks down from this PR's base: the open PR from this repository whose head
# branch is that base, then that PR's base, and so on to the default branch.
# Each lower PR whose head commit is an ancestor of this run's head (so this run
# tested its commits) gets a "Stack CI" success status on that commit. The walk
# stops at the first PR whose head is not an ancestor: its branch moved after
# this run's head was built, so this run says nothing about it, and nothing
# below it is marked either.
#
# Environment: REPO, PR, HEAD_SHA, BASE_REF, DEFAULT_BRANCH, RUN_URL;
# GH overrides the gh command (tests).
set -euo pipefail
GH=${GH:-gh}
CONTEXT="Stack CI"

ref=$BASE_REF
marked=0
for _ in $(seq 1 50); do
  [ "$ref" != "$DEFAULT_BRANCH" ] || break
  owner=${REPO%%/*}
  lower=$("$GH" api "repos/$REPO/pulls?state=open&head=$owner:$ref&per_page=100" \
    --jq ".[] | select(.head.repo.full_name == \"$REPO\") | \"\(.number) \(.head.sha) \(.base.ref)\"")
  if [ -z "$lower" ]; then
    echo "no open PR has head $ref; the stack ends here"
    break
  fi
  if [ "$(wc -l <<< "$lower")" -ne 1 ]; then
    echo "::warning::more than one open PR has head $ref; marking nothing below #$PR"
    break
  fi
  read -r number sha base <<< "$lower"
  status=$("$GH" api "repos/$REPO/compare/$sha...$HEAD_SHA" --jq '.status')
  if [ "$status" != "ahead" ] && [ "$status" != "identical" ]; then
    echo "::warning::#$number's head ${sha:0:12} is not an ancestor of ${HEAD_SHA:0:12} ($status); marking nothing from #$number down"
    break
  fi
  "$GH" api -X POST "repos/$REPO/statuses/$sha" \
    -f state=success -f context="$CONTEXT" -f target_url="$RUN_URL" \
    -f description="Tested by #$PR's CI at ${HEAD_SHA:0:12}" > /dev/null
  echo "#$number at ${sha:0:12}: $CONTEXT success (tested by #$PR)"
  marked=$((marked + 1))
  ref=$base
done
echo "marked $marked lower PR(s)"
