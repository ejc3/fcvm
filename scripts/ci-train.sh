#!/usr/bin/env bash
# ci-train.sh - pooled CI ("merge train") for batches of independent PRs.
#
# Instead of running the full self-hosted CI matrix once per PR, batch k
# low-risk PRs onto one branch (default: ci-train), run the matrix ONCE on
# the combined tree, and land every PR in the batch when it goes green.
# On red, bisect the batch in halves. See docs/ci-train.md for the protocol,
# the cost math, and when pooling is (in)appropriate.
#
# Usage:
#   scripts/ci-train.sh [--branch <name>] <subcommand> [args]
#
# Subcommands:
#   create <pr#> [<pr#>...]   Reset <branch> to origin/main and merge each
#                             PR's head branch in order. Stops on conflict
#                             for MANUAL resolution (never auto-resolves).
#   create --continue         Resume merging after a manual conflict
#                             resolution (reads the recorded manifest).
#   dispatch                  Push <branch> and dispatch one ci.yml run.
#   status                    Show the recorded train run's state.
#   land                      After a green run: merge every batch PR via
#                             gh pr merge, in batch order.
#   bisect                    On a red run: split the batch into
#                             <branch>-a / <branch>-b and dispatch both.
#
# Batch state (PR list + head SHAs + run id) is recorded in
# <git-dir>/<branch>-manifest.json (.git/ci-train-manifest.json for the
# default branch) so land/bisect are deterministic.
#
# Safety rails:
#   - never touches main directly (branch must start with "ci-train")
#   - refuses to run with uncommitted changes in the invoking checkout
#   - land refuses unless the green run's head SHA is the current train tip
#     AND every PR's current head SHA is still what was batched

set -euo pipefail

WORKFLOW="ci.yml"
BRANCH="ci-train"

die() {
    echo "ci-train: error: $*" >&2
    exit 1
}

note() {
    echo "ci-train: $*"
}

usage() {
    # Print the header comment block (everything between the shebang and
    # the first non-comment line) as the help text.
    awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
    exit "${1:-0}"
}

require_cmds() {
    local cmd
    for cmd in git gh jq; do
        command -v "$cmd" >/dev/null 2>&1 || die "required command not found: $cmd"
    done
}

# Refuse to operate in a checkout with uncommitted changes (staged or
# unstaged). Untracked files are ignored: they do not affect merges here
# and blocking on stray logs would make the tool unusable.
require_clean_tree() {
    local dirty
    dirty=$(git status --porcelain --untracked-files=no)
    [ -z "$dirty" ] || die "working tree has uncommitted changes - commit or stash first:
$dirty"
}

manifest_path() {
    local git_dir
    git_dir=$(git rev-parse --absolute-git-dir)
    echo "$git_dir/$BRANCH-manifest.json"
}

require_manifest() {
    MANIFEST=$(manifest_path)
    [ -f "$MANIFEST" ] || die "no batch manifest at $MANIFEST - run 'create' first"
}

write_manifest() {
    local content=$1
    MANIFEST=$(manifest_path)
    printf '%s\n' "$content" >"$MANIFEST.tmp"
    mv "$MANIFEST.tmp" "$MANIFEST"
}

print_manifest() {
    echo ""
    echo "Batch manifest ($MANIFEST):"
    jq . "$MANIFEST"
}

# Merge every not-yet-merged PR from the manifest into HEAD, in batch order.
# Merges the RECORDED head SHA (not the branch tip) so the batch is
# deterministic even if a PR is force-pushed afterwards.
merge_batch() {
    local row number ref sha
    while IFS= read -r row; do
        number=$(jq -r '.number' <<<"$row")
        ref=$(jq -r '.headRefName' <<<"$row")
        sha=$(jq -r '.headSha' <<<"$row")
        if git merge-base --is-ancestor "$sha" HEAD; then
            note "PR #$number already merged ($sha) - skipping"
            continue
        fi
        note "merging PR #$number ($ref @ $sha)"
        if ! git merge --no-ff --no-edit -m "ci-train: merge PR #$number ($ref)" "$sha"; then
            cat >&2 <<EOF

ci-train: MERGE CONFLICT while merging PR #$number ($ref @ $sha).

The working tree has been left mid-merge for MANUAL resolution
(ci-train never auto-resolves). To proceed:

  1. Inspect:   git status
  2. Resolve the conflicted files by hand, then:
                git add <files> && git commit --no-edit
  3. Resume the remaining merges:
                scripts/ci-train.sh --branch $BRANCH create --continue

To give up instead: git merge --abort

If this conflict means the batched PRs are entangled (they edit the same
code, not just adjacent lines), do NOT pool them - abort and run them as
individual or stacked PRs. See docs/ci-train.md.
EOF
            exit 1
        fi
    done < <(jq -c '.prs[]' "$MANIFEST")
}

cmd_create() {
    require_clean_tree

    if [ "${1:-}" = "--continue" ]; then
        [ $# -eq 1 ] || die "create --continue takes no other arguments"
        require_manifest
        local current
        current=$(git rev-parse --abbrev-ref HEAD)
        [ "$current" = "$BRANCH" ] || die "create --continue must run on branch $BRANCH (currently on $current)"
        if git rev-parse -q --verify MERGE_HEAD >/dev/null; then
            die "a merge is still in progress - resolve it and 'git commit --no-edit' first"
        fi
        merge_batch
        print_manifest
        note "batch complete - next: make train-dispatch"
        return
    fi

    [ $# -ge 1 ] || die "usage: create <pr#> [<pr#>...]"
    local pr
    for pr in "$@"; do
        case "$pr" in
            *[!0-9]*|'') die "not a PR number: $pr" ;;
        esac
    done

    note "fetching origin"
    git fetch origin

    # Validate every PR and record its fetched head SHA BEFORE touching
    # any branch, so a bad batch fails fast with the tree untouched.
    local base manifest info state base_ref cross ref title sha
    base=$(git rev-parse refs/remotes/origin/main)
    manifest=$(jq -n \
        --arg branch "$BRANCH" \
        --arg base "$base" \
        --arg created "$(date -u +%FT%TZ)" \
        '{branch: $branch, base: $base, created_at: $created, prs: [], run_id: null, run_url: null, run_sha: null}')
    for pr in "$@"; do
        info=$(gh pr view "$pr" --json number,title,state,headRefName,headRefOid,baseRefName,isCrossRepository) \
            || die "gh pr view $pr failed - does the PR exist?"
        state=$(jq -r '.state' <<<"$info")
        base_ref=$(jq -r '.baseRefName' <<<"$info")
        cross=$(jq -r '.isCrossRepository' <<<"$info")
        ref=$(jq -r '.headRefName' <<<"$info")
        title=$(jq -r '.title' <<<"$info")
        [ "$state" = "OPEN" ] || die "PR #$pr is $state, not OPEN"
        [ "$base_ref" = "main" ] || die "PR #$pr targets '$base_ref', not main - stacked PRs cannot be pooled"
        [ "$cross" = "false" ] || die "PR #$pr comes from a fork - only same-repo branches can be pooled"
        sha=$(git rev-parse "refs/remotes/origin/$ref" 2>/dev/null) \
            || die "PR #$pr head branch origin/$ref not found after fetch"
        manifest=$(jq \
            --argjson number "$pr" \
            --arg ref "$ref" \
            --arg sha "$sha" \
            --arg title "$title" \
            '.prs += [{number: $number, headRefName: $ref, headSha: $sha, title: $title}]' \
            <<<"$manifest")
    done

    local original
    original=$(git rev-parse --abbrev-ref HEAD)
    note "resetting branch $BRANCH to origin/main ($base); you were on '$original'"
    git checkout -B "$BRANCH" "$base"

    # Manifest is written before merging so a conflict stop is resumable
    # with 'create --continue'.
    write_manifest "$manifest"
    merge_batch
    print_manifest
    note "batch complete - next: make train-dispatch"
}

# Push $BRANCH and dispatch one workflow run on it; record the run in the
# manifest. Used by both 'dispatch' and 'bisect'.
push_and_dispatch() {
    local tip since run
    tip=$(git rev-parse "refs/heads/$BRANCH") || die "local branch $BRANCH does not exist - run 'create' first"

    # Every batched PR must already be merged (a conflict stop leaves the
    # batch incomplete - dispatching that would validate the wrong tree).
    local row number sha
    while IFS= read -r row; do
        number=$(jq -r '.number' <<<"$row")
        sha=$(jq -r '.headSha' <<<"$row")
        git merge-base --is-ancestor "$sha" "$tip" \
            || die "PR #$number ($sha) is not merged into $BRANCH - run 'create --continue'"
    done < <(jq -c '.prs[]' "$MANIFEST")

    note "pushing $BRANCH ($tip)"
    git push --force-with-lease origin "refs/heads/$BRANCH:refs/heads/$BRANCH"

    since=$(date -u -d '-10 seconds' +%FT%TZ)
    note "dispatching $WORKFLOW on $BRANCH"
    gh workflow run "$WORKFLOW" --ref "$BRANCH"

    # The run takes a moment to appear; poll briefly to record its id.
    local attempt=0
    run=""
    while [ "$attempt" -lt 10 ]; do
        attempt=$((attempt + 1))
        sleep 3
        run=$(gh run list --workflow "$WORKFLOW" --branch "$BRANCH" --event workflow_dispatch \
            --limit 10 --json databaseId,headSha,url,createdAt \
            | jq -c --arg tip "$tip" --arg since "$since" \
                'map(select(.headSha == $tip and .createdAt >= $since)) | .[0] // empty')
        [ -n "$run" ] && break
    done
    if [ -z "$run" ]; then
        note "WARNING: dispatched, but the run has not appeared yet; re-run 'dispatch' or check manually:"
        note "  gh run list --workflow $WORKFLOW --branch $BRANCH"
        return
    fi

    local run_id run_url
    run_id=$(jq -r '.databaseId' <<<"$run")
    run_url=$(jq -r '.url' <<<"$run")
    write_manifest "$(jq \
        --argjson run_id "$run_id" \
        --arg run_url "$run_url" \
        --arg run_sha "$tip" \
        '.run_id = $run_id | .run_url = $run_url | .run_sha = $run_sha' \
        "$MANIFEST")"
    note "train run dispatched: $run_url"
}

cmd_dispatch() {
    require_clean_tree
    require_manifest
    push_and_dispatch
    note "next: make train-status (then train-land when green)"
}

cmd_status() {
    require_manifest
    local run_id run
    run_id=$(jq -r '.run_id // empty' "$MANIFEST")

    echo "Train branch: $BRANCH"
    echo "Batch:"
    jq -r '.prs[] | "  #\(.number) \(.title) (\(.headSha[0:9]))"' "$MANIFEST"

    if [ -z "$run_id" ]; then
        note "no run recorded in the manifest - run 'dispatch'"
        exit 1
    fi

    run=$(gh run view "$run_id" --json status,conclusion,headSha,url) \
        || die "gh run view $run_id failed"
    echo "Run:    $(jq -r '.url' <<<"$run")"
    echo "Status: $(jq -r '.status' <<<"$run") $(jq -r '.conclusion // ""' <<<"$run")"

    local run_sha tip
    run_sha=$(jq -r '.headSha' <<<"$run")
    if tip=$(git rev-parse -q --verify "refs/heads/$BRANCH"); then
        if [ "$tip" != "$run_sha" ]; then
            note "WARNING: local $BRANCH ($tip) no longer matches the run's head SHA ($run_sha)"
            note "         re-run 'dispatch' before trusting this run"
        fi
    fi
}

cmd_land() {
    require_clean_tree
    require_manifest

    local run_id run status conclusion run_sha run_url
    run_id=$(jq -r '.run_id // empty' "$MANIFEST")
    [ -n "$run_id" ] || die "no train run recorded - run 'dispatch' first"

    note "fetching origin"
    git fetch origin

    run=$(gh run view "$run_id" --json status,conclusion,headSha,url) \
        || die "gh run view $run_id failed"
    status=$(jq -r '.status' <<<"$run")
    conclusion=$(jq -r '.conclusion' <<<"$run")
    run_sha=$(jq -r '.headSha' <<<"$run")
    run_url=$(jq -r '.url' <<<"$run")
    [ "$status" = "completed" ] || die "train run is $status, not completed: $run_url"
    [ "$conclusion" = "success" ] || die "train run concluded '$conclusion', not success - run 'bisect': $run_url"

    # The green run must have validated exactly the tree we are about to
    # converge main onto: refuse if the branch tip moved after dispatch.
    local tip remote_tip
    if tip=$(git rev-parse -q --verify "refs/heads/$BRANCH"); then
        [ "$tip" = "$run_sha" ] || die "local $BRANCH tip ($tip) != run head SHA ($run_sha) - re-dispatch"
    fi
    if remote_tip=$(git rev-parse -q --verify "refs/remotes/origin/$BRANCH"); then
        [ "$remote_tip" = "$run_sha" ] || die "origin/$BRANCH tip ($remote_tip) != run head SHA ($run_sha) - re-dispatch"
    fi

    # Every PR's CURRENT head must still be the SHA the train validated.
    # A force-push after batching invalidates the whole batch.
    local row number sha ref cur state landable=""
    while IFS= read -r row; do
        number=$(jq -r '.number' <<<"$row")
        sha=$(jq -r '.headSha' <<<"$row")
        ref=$(jq -r '.headRefName' <<<"$row")
        cur=$(gh pr view "$number" --json state,headRefOid) || die "gh pr view $number failed"
        state=$(jq -r '.state' <<<"$cur")
        case "$state" in
            OPEN) ;;
            MERGED)
                note "PR #$number is already merged - skipping"
                continue
                ;;
            *) die "PR #$number is $state - re-create the train without it" ;;
        esac
        cur=$(jq -r '.headRefOid' <<<"$cur")
        if [ "$cur" != "$sha" ]; then
            die "PR #$number head moved after batching ($sha -> $cur) - the train did not validate the current PR. Re-create the train."
        fi
        git merge-base --is-ancestor "$sha" "$run_sha" \
            || die "PR #$number ($sha) is not contained in the validated train tip $run_sha - re-create the train"
        landable="$landable $number"
    done < <(jq -c '.prs[]' "$MANIFEST")
    [ -n "$landable" ] || { note "nothing to land - all batch PRs already merged"; return; }

    local batch_list base
    base=$(jq -r '.base' "$MANIFEST")
    batch_list=$(jq -r '.prs[] | "- #\(.number) \(.title) (`\(.headSha[0:9])`)"' "$MANIFEST")

    for number in $landable; do
        note "landing PR #$number"
        gh pr comment "$number" --body "Landed by the CI merge train (\`scripts/ci-train.sh\`).

Validation for this PR came from one full CI matrix run on the **combined tree of the whole batch** (branch \`$BRANCH\` @ \`$run_sha\`), not from a per-PR run:

- Train run: $run_url (success)
- Batch, merged in order onto \`origin/main\` @ \`$base\`:
$batch_list

Landing merges each batch PR in the same order, so \`main\` converges on the tree the train validated. See \`docs/ci-train.md\` for the protocol."

        # After each merge, GitHub re-evaluates the next PR against the
        # moved base; brief retries absorb the recompute window.
        local attempt=0 merged=""
        while [ "$attempt" -lt 5 ]; do
            attempt=$((attempt + 1))
            if gh pr merge "$number" --merge --delete-branch --admin; then
                merged=1
                break
            fi
            note "merge of PR #$number failed (attempt $attempt/5); retrying in 5s"
            sleep 5
        done
        if [ -z "$merged" ]; then
            cat >&2 <<EOF

ci-train: FAILED to merge PR #$number after 5 attempts.

PRs earlier in the batch have already been merged; PRs from #$number on
have NOT. If GitHub reports a merge conflict, this PR conflicts with a
batch-mate that just landed - the conflict was resolved by hand on
$BRANCH, but GitHub cannot reuse that resolution per-PR. Fix by updating
the PR branch with the same resolution:

  git fetch origin
  git checkout $ref
  git merge origin/main       # re-apply the resolution used on $BRANCH
  git push

then merge it normally (its own CI run) or re-create a train from the
remaining PRs.
EOF
            exit 1
        fi
    done

    write_manifest "$(jq --arg at "$(date -u +%FT%TZ)" '.landed_at = $at' "$MANIFEST")"
    note "all batch PRs landed"
    note "optional cleanup: git push origin --delete $BRANCH && git branch -D $BRANCH"
}

cmd_bisect() {
    require_clean_tree
    require_manifest

    local run_id run status conclusion
    run_id=$(jq -r '.run_id // empty' "$MANIFEST")
    [ -n "$run_id" ] || die "no train run recorded - run 'dispatch' first"
    run=$(gh run view "$run_id" --json status,conclusion,url) || die "gh run view $run_id failed"
    status=$(jq -r '.status' <<<"$run")
    conclusion=$(jq -r '.conclusion' <<<"$run")
    [ "$status" = "completed" ] || die "train run is still $status - wait for it before bisecting"
    if [ "$conclusion" = "success" ]; then
        die "train run is green - nothing to bisect; run 'land'"
    fi

    local count
    count=$(jq '.prs | length' "$MANIFEST")
    [ "$count" -ge 2 ] || die "batch has a single PR - the red run already isolates it; fix or drop PR #$(jq -r '.prs[0].number' "$MANIFEST")"

    local mid parent_manifest parent_branch
    mid=$(((count + 1) / 2))
    parent_manifest=$MANIFEST
    parent_branch=$BRANCH

    local suffix range half_prs base created
    base=$(jq -r '.base' "$parent_manifest")
    created=$(date -u +%FT%TZ)
    for suffix in a b; do
        if [ "$suffix" = "a" ]; then
            range="0:$mid"
        else
            range="$mid:$count"
        fi
        half_prs=$(jq -c ".prs[$range]" "$parent_manifest")

        # Each half is a full train in its own right: same recorded base,
        # same recorded PR head SHAs, own branch + manifest + run.
        BRANCH="$parent_branch-$suffix"
        note "building half '$suffix': branch $BRANCH with PRs $(jq -r 'map("#\(.number)") | join(" ")' <<<"$half_prs")"
        git checkout -B "$BRANCH" "$base"
        write_manifest "$(jq -n \
            --arg branch "$BRANCH" \
            --arg base "$base" \
            --arg created "$created" \
            --arg parent "$parent_branch" \
            --argjson prs "$half_prs" \
            '{branch: $branch, base: $base, created_at: $created, parent: $parent, prs: $prs, run_id: null, run_url: null, run_sha: null}')"
        merge_batch
        push_and_dispatch
    done
    BRANCH=$parent_branch
    MANIFEST=$parent_manifest

    cat <<EOF

Bisect protocol - two half-trains are running:

  make train-status TRAIN=$parent_branch-a
  make train-status TRAIN=$parent_branch-b

When both complete:
  - GREEN half           -> land it:      make train-land TRAIN=$parent_branch-<x>
  - RED half, one PR     -> culprit found: fix/drop that PR, land the rest
  - RED half, >1 PR      -> recurse:       make train-bisect TRAIN=$parent_branch-<x>
  - BOTH halves green    -> the failure needs the combination: the PRs are
    entangled - do not pool them; run them as individual/stacked PRs
EOF
}

main() {
    require_cmds

    if [ "${1:-}" = "--branch" ]; then
        [ $# -ge 2 ] || die "--branch requires a name"
        BRANCH=$2
        shift 2
    fi
    case "$BRANCH" in
        ci-train|ci-train-*) ;;
        *) die "train branch must be 'ci-train' or 'ci-train-*' (never main): $BRANCH" ;;
    esac

    git rev-parse --show-toplevel >/dev/null 2>&1 || die "not inside a git checkout"

    local cmd=${1:-}
    [ $# -ge 1 ] && shift
    case "$cmd" in
        create)   cmd_create "$@" ;;
        dispatch) [ $# -eq 0 ] || die "dispatch takes no arguments"; cmd_dispatch ;;
        status)   [ $# -eq 0 ] || die "status takes no arguments"; cmd_status ;;
        land)     [ $# -eq 0 ] || die "land takes no arguments"; cmd_land ;;
        bisect)   [ $# -eq 0 ] || die "bisect takes no arguments"; cmd_bisect ;;
        -h|--help|help) usage 0 ;;
        '')       usage 1 ;;
        *)        die "unknown subcommand: $cmd (try --help)" ;;
    esac
}

main "$@"
