#!/bin/bash
# Capture a REAL review-thread response as a test fixture.
#
# Hand-written fixtures drift from the query that is supposed to produce them. Ours did:
# the fixture carried two comments per thread while the query asked for `comments(first: 1)`,
# so `a_resolved_defect_claim_with_a_red_test_is_accepted` passed against data the code could
# never return — a green test proving nothing. Capturing from a live PR makes that
# impossible by construction.
#
# Usage: capture-review-threads-fixture.sh <pr> <output.json>
set -euo pipefail
command -v gh >/dev/null || { echo "need gh" >&2; exit 2; }
command -v jq >/dev/null || { echo "need jq" >&2; exit 2; }
pr=${1:?pr number}; out=${2:?output path}
gh api graphql -f query="
  { repository(owner: \"${REPO_OWNER:-ejc3}\", name: \"${REPO_NAME:-fcvm}\") {
      pullRequest(number: $pr) {
        reviewThreads(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes {
            isResolved isOutdated
            comments(first: 100) { nodes { author { login } path line originalLine body } }
          } } } } }" | jq '.' > "$out"
echo "captured PR #$pr -> $out ($(jq '.data.repository.pullRequest.reviewThreads.nodes|length' "$out") threads)"
