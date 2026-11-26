#!/usr/bin/env bash
# Decide what CI runs for this event, for the `changes` job in ci.yml.
#
# Writes run, reason, code, bench, fleet and web to $GITHUB_OUTPUT.
#
# A pull request runs nothing while it is a draft, or while another open PR
# from this repository that is not a draft is stacked on it (based on its branch): CI runs once, on
# the top of the stack, whose head holds every lower PR's commits. The top's run
# classifies the whole stack's diff against the default branch, not only its own
# files, so a docs-only top PR cannot skip the matrix for code changed lower down.
# `scripts/ci-pool/ci_pool.py mark` then puts a `ci/pool` status on the lower PRs' heads.
#
# Every lookup that fails runs the full matrix: a scope that cannot be computed
# must not skip anything.
#
# Environment: EVENT, REPO, PR, ACTION, DRAFT, HEAD_REPO, HEAD_REF, HEAD_SHA, BASE_REF,
# DEFAULT_BRANCH, GITHUB_OUTPUT; GH overrides the gh command (tests).
set -euo pipefail
GH=${GH:-gh}

emit() {
  # run reason code bench fleet web confine
  # confine: the change touches the VMM's confinement (the seccomp policies, their
  # generator, the launch that applies them or their gate), so a pull request also runs
  # Host-x64, whose test-confine step is the only place the x86_64 policy is exercised
  # under real confinement.
  {
    echo "run=$1"
    echo "reason=$2"
    echo "code=$3"
    echo "bench=$4"
    echo "fleet=$5"
    echo "web=$6"
    echo "confine=$7"
  } >> "$GITHUB_OUTPUT"
  echo "run=$1 code_changed=$3 bench_changed=$4 fleet_changed=$5 web_changed=$6 confine_changed=$7: $2"
}

# Only a pull_request has a well-defined changed-file set here; every
# other trigger runs the full matrix rather than guess.
# A fail-open must open EVERY gate. Writing `code=true` alone left
# `bench` unset, and bench-tests is gated on it, so the bench lints
# were skipped on push-to-main, workflow_dispatch and the Build
# Kernels workflow_run — i.e. never ran on main at all, which matters
# because the documented stacked-PR routine force-merges with --admin
# after cancelling the PR-side run. Summary counts skipped as
# non-failure, so that read as green.
if [ "$EVENT" != "pull_request" ]; then
  emit true "event $EVENT: running the full matrix" true true true true true
  exit 0
fi

# Only a PR whose branch is in this repository can be a layer of one of its
# stacks. A fork's branch can share a name with one of ours.
stackable=true
[ "$HEAD_REPO" = "$REPO" ] || stackable=false

if [ "${ACTION:-}" = "closed" ]; then
  emit false "closed: this run only cancels the PR's earlier runs" false false false false false
  exit 0
fi

if [ "$DRAFT" = "true" ]; then
  emit false "draft: CI runs when the PR is marked ready" false false false false false
  exit 0
fi

# Open PRs from this repository based on this PR's branch whose head contains this
# PR's head: their runs test this commit. A child that does not contain it yet (a fix
# pushed here before the child was rebased) defers nothing, or the commit would go
# untested. Nor does a draft child: a draft runs nothing (above), so deferring to one
# left this PR with every job skipped and a green `Summary (deferred)`. A child whose
# draft state is not the literal `false` counts as a draft. A failed lookup counts as
# no child, so this PR runs.
if [ "$stackable" = false ]; then
  echo "head branch is in $HEAD_REPO, not $REPO: not part of a stack here"
elif children=$("$GH" api --paginate "repos/$REPO/pulls?state=open&base=$HEAD_REF&per_page=100" \
    --jq ".[] | select(.head.repo.full_name == \"$REPO\") | \"\(.number) \(.head.sha) \(.draft)\""); then
  covering=""
  while read -r number sha child_draft; do
    [[ "$number" =~ ^[0-9]+$ && "$sha" =~ ^[0-9a-f]{40}$ ]] || continue
    if [ "$child_draft" != "false" ]; then
      echo "#$number is stacked here but is a draft, which runs no CI"
      continue
    fi
    status=$("$GH" api "repos/$REPO/compare/$HEAD_SHA...$sha" --jq '.status' 2>/dev/null) || status=""
    if [ "$status" = "ahead" ] || [ "$status" = "identical" ]; then
      covering="$covering${covering:+ }$number"
    else
      echo "#$number is stacked here but its head does not contain ${HEAD_SHA:0:12} (${status:-lookup failed})"
    fi
  done <<< "$children"
  if [ -n "$covering" ]; then
    emit false "stacked under #${covering// /, #}: CI runs on the top of the stack" false false false false false
    exit 0
  fi
else
  echo "could not list PRs stacked on $HEAD_REF; treating this PR as the top"
fi

# The files to classify. For a PR on the default branch, its own files; for the
# top of a stack, the stack's diff against the default branch. `.filename` is the
# POST-rename path only. `git mv src/foo.rs bench/chromium/foo.rs` would report
# one bench/ entry, leave code false, skip the whole VM matrix, and make a
# non-compiling tree mergeable behind a green Summary. `previous_filename` is
# present on exactly the renamed entries, so classifying both paths means a
# rename is judged by where it came from as well as where it went.
if [ "$BASE_REF" = "$DEFAULT_BRANCH" ]; then
  scope="PR #$PR"
  files=$("$GH" api "repos/$REPO/pulls/$PR/files" --paginate \
    --jq '.[] | .filename, (.previous_filename // empty)') || files=""
else
  scope="stack $DEFAULT_BRANCH...$HEAD_SHA"
  # The compare API lists at most 300 files and says nothing when it stops
  # there, so 300 or more means the list may be cut short: run everything.
  if compared=$("$GH" api "repos/$REPO/compare/$DEFAULT_BRANCH...$HEAD_SHA" \
      --jq '(.files | length), (.files[] | .filename, (.previous_filename // empty))'); then
    count=$(head -n1 <<< "$compared")
    if [ "$count" -ge 300 ]; then
      emit true "$scope: $count files, more than the compare API lists; running the full matrix" true true true true true
      exit 0
    fi
    files=$(tail -n +2 <<< "$compared")
  else
    files=""
  fi
fi
if [ -z "$files" ]; then
  # An empty or failed file list is unexpected, so fail toward running everything.
  emit true "$scope: no changed files reported; running the full matrix" true true true true true
  exit 0
fi

# bench/** is classified SEPARATELY, not lumped into code. The
# self-hosted matrix does not exercise the benchmark harness at all,
# so a bench-only PR was paying for the full matrix and getting no
# check of the thing it changed. It now runs bench-tests instead,
# which is the check that can actually fail for it. Skipping without
# routing would be the green-by-absence hole this file already
# documents twice.
code=false
bench=false
fleet=false
web=false
confine=false
while IFS= read -r f; do
  # `bench/*` MUST precede the docs arm. A shell `case` glob matches
  # `/`, so `*.md` swallows bench/chromium/README.md and that PR
  # reports code=false bench=false: zero gating jobs, clean Summary.
  # Meanwhile bench/chromium/test_reqbench.py carries lints that read
  # exactly those files (the Clopper-Pearson bound lint on REVIEW.md,
  # the JPEG figure and cited-path lints on AGENTS.md, the healthcheck
  # lint on README.md), so the one class of change those lints exist
  # to catch would be the one class that runs nothing.
  #
  # The `*.md` in `paths-ignore:` at the top of this file is NOT the
  # same pattern: GitHub's filter globs stop at a slash, so there it
  # means root-level markdown only. Identical spelling, different
  # matcher — which is why this was easy to miss.
  case "$f" in
    scripts/claude-assistant/*|.github/workflows/claude*.yml|.claude/*) ;;
    # Docs that TESTS READ are code, whatever their extension.
    # tests/test_documented_make_targets.rs reads README.md and
    # PERFORMANCE.md and fails when they name make targets that do not
    # exist; it runs ungated under `make test-unit` in the host job. A
    # PR editing only those files was getting zero gating jobs while
    # the very lints that check them sat unrun -- the same hole this
    # file fixes for bench/, one directory up.
    README.md|PERFORMANCE.md) code=true; echo "linted doc: $f" ;;
    # The Makefile is BOTH: ordinary code, and the subject of
    # bench/chromium/test_reqbench.py::MakefileBenchGraph, the
    # structural pin AGENTS.md cites for the bench dependency graph.
    # It runs `make -pq` against this file, so a Makefile-only change
    # is exactly the class that can break it.
    Makefile) code=true; bench=true; echo "makefile: $f" ;;
    # bench/start-rate/pieces is a workspace member: lint builds and formats it.
    bench/start-rate/pieces/*) code=true; bench=true; echo "code path (workspace member): $f" ;;
    bench/*) bench=true; echo "bench path: $f" ;;
    # fleet/core is a workspace member that fcvm builds, so it is code.
    # The rest of fleet/ (renderer, media, shots, tools, PROTOCOL.md)
    # is not built, read or run by anything the VM matrix executes, so
    # the matrix can say nothing about it. It runs fleet-renderer, the
    # renderer's host tests; fleet/web runs fleet-web instead.
    # fleet/web's tests read fleet/core/tests/vectors/** and the coordinator's view in
    # fleet/core/src/** (the fixture core/test/fixtures/coordinator.json is generated from
    # them, and the mock's shape test compares against it), so a change there runs fleet-web.
    fleet/core/*) code=true; web=true; echo "code path (fleet web reads it): $f" ;;
    # fleet/potato is a workspace member too (the potato player); lint formats it.
    fleet/potato/*) code=true; echo "code path: $f" ;;
    # The fleet web page is TypeScript with its own suite (fleet-web);
    # the VM matrix does not build or run it. Like bench/, it precedes
    # the docs arm so its README still routes to its check.
    fleet/web/*) web=true; echo "fleet web path: $f" ;;
    # The mock's shape test reads fleet/PROTOCOL.md's example.
    fleet/PROTOCOL.md) web=true; fleet=true; echo "fleet web reads it: $f" ;;
    fleet/*|.github/workflows/fleet-coordinator-deploy.yml) fleet=true; echo "fleet path, no matrix: $f" ;;
    # This file holds the fleet-renderer and fleet-web jobs, so a change here runs them too.
    .github/workflows/ci.yml) code=true; web=true; fleet=true; echo "ci workflow: $f" ;;
    docs/*|*.md) ;;
    *) code=true; echo "code path: $f" ;;
  esac
  # A second question about the same path, kept out of the arms above (which say which
  # jobs a path needs): whether it is part of the VMM's confinement.
  case "$f" in
    resources/seccomp/*|src/confine.rs|src/firecracker/vm.rs|src/hypervisor/firecracker.rs|scripts/confine-*|tests/test_confine*.rs)
      confine=true; echo "confinement path: $f" ;;
  esac
done <<< "$files"
emit true "$scope" "$code" "$bench" "$fleet" "$web" "$confine"
