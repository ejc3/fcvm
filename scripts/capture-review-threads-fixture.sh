#!/bin/bash
# Capture a REAL review-gate payload as a test fixture.
#
# Hand-written fixtures drift from the query that is supposed to produce them. Ours did:
# the fixture carried two comments per thread while the query asked for `comments(first: 1)`,
# so `a_resolved_defect_claim_with_a_red_test_is_accepted` passed against data the code could
# never return — a green test proving nothing. This dumps what the gate itself assembles
# (threads, reviews, top-level comments, the head and its check suites), so a fixture cannot
# drift from the query again, and a fixture cannot omit a field the gate now reads.
#
# Usage: capture-review-threads-fixture.sh <pr> <output.json>
set -euo pipefail
command -v gh >/dev/null || { echo "need gh" >&2; exit 2; }
command -v jq >/dev/null || { echo "need jq" >&2; exit 2; }
pr=${1:?pr number}; out=${2:?output path}
"$(dirname "$0")/check-review-threads.sh" --dump-payload "$pr" | jq '.' > "$out"
jq -r --arg pr "$pr" --arg out "$out" '.data.repository.pullRequest
  | "captured PR #\($pr) -> \($out) (head \(.headRefOid[0:9]), \(.reviewThreads.nodes|length) threads, "
    + "\([.reviewThreads.nodes[]|select(.isResolved==false)]|length) unresolved, "
    + "\(.reviews.nodes|length) reviews, \(.comments.nodes|length) comments)"' "$out"
