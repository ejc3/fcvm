---
name: pr-workflow
description: "ALWAYS USE THIS SKILL when: creating PRs, checking PR status, reviewing PRs, merging PRs, checking CI status, fixing lint/clippy errors, running cargo fmt, or any gh pr command. Invoke with /pr-workflow"
allowed-tools: Bash, Read, Grep, Glob
user-invocable: true
---

# Pull Request Workflow for Rust Projects

## Quick Reference

| Task | Command |
|------|---------|
| CI status (non-blocking) | `gh pr view <N> --json statusCheckRollup --jq '.statusCheckRollup[] \| "\(.name): \(.conclusion // "pending")"'` |
| CI status (blocking) | `gh pr checks <N>` |
| Read PR comments (Claude) | `gh pr view <N> --json comments --jq '.comments[] \| "---\n" + .body'` |
| Read inline comments (Codex) | `gh api repos/$REPO/pulls/<N>/comments --jq '.[] \| select(.user.login \| test("codex")) \| "---\nfile: \(.path):\(.line // .original_line)\n\(.body)"'` |
| Read all inline comments | `gh api repos/$REPO/pulls/<N>/comments --jq '.[] \| "---\nfile: \(.path):\(.line // .original_line)\n\(.body)"'` |
| Create PR | `git push -u origin <branch> && gh pr create --fill` |
| Merge standalone PR | `gh pr merge <N> --merge --delete-branch` |
| Merge stacked PR (base) | `gh pr merge <N> --merge` (NO `--delete-branch`!) |
| List my PRs | `gh pr list --author @me` |

**Setup** — run this once per session to avoid placeholder issues:
```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')
```

---

## CI Failed — What Do I Do?

This is the most common use of this skill. Follow this decision tree:

### Step 1: Get the run ID and identify failures

```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')

# Get latest run ID for current branch
RUN_ID=$(gh run list --branch "$(git branch --show-current)" --limit 1 --json databaseId --jq '.[0].databaseId')

# Show all failed jobs and which step failed
gh api "repos/$REPO/actions/runs/$RUN_ID/jobs" \
  --jq '.jobs[] | select(.conclusion == "failure") | "\(.id) \(.name): \([.steps[] | select(.conclusion == "failure") | .name] | join(", "))"'
```

### Step 2: Is it your fault?

```bash
# Check if main is green
gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'
```

- **Main green, PR red** → Your PR caused it. Fix it.
- **Main also red** → Pre-existing. Note in PR, but still investigate.

### Step 3: Get logs for the failed job

`gh run view --job --log` blocks until the ENTIRE run finishes. Use the Jobs API instead:

```bash
JOB_ID=<from step 1>

# Get full log (works immediately for completed jobs)
gh api "repos/$REPO/actions/jobs/$JOB_ID/logs" 2>&1 | tail -100

# Search for errors
gh api "repos/$REPO/actions/jobs/$JOB_ID/logs" 2>&1 | grep -E "error|FAIL|panicked" | head -20

# Get context around failures
gh api "repos/$REPO/actions/jobs/$JOB_ID/logs" 2>&1 | grep -B5 "Error" | head -30
```

### Step 4: Fix and push

```bash
# Fix the code, then:
cargo fmt
cargo clippy --all-targets -- -D warnings
git add <files> && git commit -m "fix: ..."
git push
```

### Step 5: Report with evidence, not speculation

**NEVER say "likely", "probably", or "may be caused by".** Always find the actual root cause.

- Read the diff (`git diff main..HEAD`) to understand what changed
- Match log errors to specific lines in the diff
- If a CI step name is misleading (e.g., "container-test" runs host builds first), read the full log to find where the error actually occurs
- State what the error IS, not what it "might be"

### Common failure patterns

- **Short failure (<30s) in build step** → Compilation error. Get logs immediately.
- **"container-test" failure** → Check whether error is in host build (runs first) or container build/test (runs after). Read the log.
- **Test timeout (600s+)** → Test hang. Check for pipe deadlock or missing stdin EOF.

---

## Before Merging: Read ALL PR Comments

**MANDATORY before any merge, push, or PR update.** Run ALL THREE commands:

```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')

# PR-level comments (Claude review posts here)
gh pr view <N> --json comments --jq '.comments[] | "---\n" + .body'

# Inline code review comments (Codex review posts here)
gh api "repos/$REPO/pulls/<N>/comments" --jq '.[] | "---\nfile: \(.path):\(.line // .original_line)\n\(.body)"'

# PR reviews with body text (Codex review summary lives here)
gh api "repos/$REPO/pulls/<N>/reviews" --jq '.[] | select(.body != "") | "---\nauthor: \(.user.login)\n\(.body)"'
```

### Two Automated Reviewers

This repo has **two** automated code reviewers. Read findings from BOTH before merging.

**Claude Review** (`claude-claude` bot):
- Posts as a **PR-level comment** (visible via `gh pr view --json comments`)
- Runs as a GitHub Actions workflow on every push
- Severity levels: LOW, MEDIUM, HIGH

**Codex Review** (`chatgpt-codex-connector[bot]`):
- Posts a **PR review** with inline comments on specific lines
- Review summary: `gh api repos/$REPO/pulls/<N>/reviews` (look for `chatgpt-codex-connector`)
- Inline suggestions: `gh api repos/$REPO/pulls/<N>/comments` (filter by codex author)
- Priority badges: P0 (critical), P1 (important), P2 (suggestion)

**Read Codex inline comments specifically:**
```bash
# Codex inline review comments only
gh api "repos/$REPO/pulls/<N>/comments" \
  --jq '.[] | select(.user.login | test("codex")) | "---\nfile: \(.path):\(.line // .original_line)\n\(.body)"'
```

### Check for auto-fix PRs

```bash
gh pr list --search "base:$(git branch --show-current)"
```

**Decision tree for auto-fix PRs:**

```
Auto-fix PR exists?
 → Run the failing test locally WITHOUT the auto-fix
   PASSES → Close the auto-fix (your branch already fixes it)
            gh pr close <fix-pr> --comment "Not needed - <explain>"
   FAILS  → Read the auto-fix diff
            Adds skip/ignore/early-return? → Close it, find real fix
            Fixes a real gap?              → Cherry-pick, push, close auto-fix
```

Fix ALL review findings (including low severity) before merging.

---

## Merging a PR

### Standalone PR (base is main)

```bash
gh pr merge <N> --merge --delete-branch
```

### Stacked PR (base is another branch)

**NEVER use `--delete-branch` on the base PR of a stack.** It deletes the branch before
GitHub retargets dependent PRs, causing them to auto-close.

```bash
# Step 1: Merge WITHOUT --delete-branch
gh pr merge <base-pr> --merge

# Step 2: Retarget dependent PR
gh pr edit <dependent-pr> --base main

# Step 3: Verify retarget
gh pr view <dependent-pr> --json baseRefName
# Must show: {"baseRefName":"main"}

# Step 4: NOW delete the old branch
git push origin --delete <base-branch-name>
```

**Recovery if you accidentally used `--delete-branch`:**
```bash
git push origin <dependent-branch>  # Re-push (still exists locally)
gh pr create --base main --title "..." --body "..."
```

---

## Creating a Pull Request

### 1. Run pre-push checks

```bash
cargo fmt --check    # Fix: cargo fmt
cargo clippy --all-targets -- -D warnings
make test-root FILTER=sanity  # Or tests relevant to your changes
```

### 2. Review and push

```bash
git diff --stat                       # What files changed
git log main..HEAD --oneline          # All commits in this branch
git push -u origin <branch-name>
gh pr create --fill                   # For single-commit PRs
```

For multi-commit PRs, write a structured description:
```bash
gh pr create --title "..." --body "$(cat <<'EOF'
## Summary
- Change 1
- Change 2

## Test Results
$ make test-root FILTER=sanity
<actual output>
EOF
)"
```

### 3. Verify description matches ALL commits

Do this on creation AND after pushing new commits:

```bash
# For PRs targeting main:
git log --oneline origin/main..HEAD

# For stacked PRs (targeting another branch):
BASE=$(gh pr view --json baseRefName --jq '.baseRefName')
git log --oneline "origin/$BASE..HEAD"

# Update if needed:
gh pr edit <N> --body "$(cat <<'EOF'
...updated description covering ALL commits...
EOF
)"
```

**Anti-patterns:** Description only covers first/last commit, mentions reverted changes,
or (for stacked PRs) includes parent branch commits.

### 4. Wait for CI

```bash
gh pr checks <N>
# All checks must show "pass" before proceeding
```

---

## Stacked PRs (Branch of Branch)

```bash
git checkout pr1-branch
git checkout -b pr2-branch
# ... make changes ...
git push -u origin pr2-branch
gh pr create --base pr1-branch
```

### Rebasing after base PR merges

**Never use plain `git rebase origin/main`** — it replays already-merged commits, causing
false conflicts in files you didn't touch.

```bash
# Step 1: Find which commits are yours vs already-merged
git log --oneline origin/main..your-branch

# Step 2: Find the old base tip (last already-merged commit)
for commit in $(git log --format="%h" origin/main..your-branch); do
  subject=$(git log --format="%s" -1 $commit)
  match=$(git log --oneline origin/main --grep="$subject" | head -1)
  echo "$commit: $subject → ${match:-UNIQUE}"
done

# Step 3: Rebase only YOUR commits onto main
git rebase --onto origin/main <old-base-tip> your-branch

# Step 4: Force push
git push origin your-branch --force-with-lease
```

For deeper stacks, rebase bottom-up:
```bash
git rebase --onto pr2-branch <pr2-old-tip> pr3-branch
git push origin pr3-branch --force-with-lease
```

---

## CI Not Starting?

Most common cause: PR branch not based on main.

```bash
gh pr view <N> --json baseRefName           # Check target branch
git fetch origin && git rebase origin/main  # Rebase if stale
git push --force-with-lease
gh pr edit <N> --base main                  # Fix wrong target
```

---

## SSH into CI Runners

```bash
# Find runners
gh api repos/ejc3/fcvm/actions/runners \
  --jq '.runners[] | "\(.name) busy=\(.busy) labels=\(.labels | map(.name) | join(","))"'

# Get runner IPs (change architecture filter as needed: x86_64 or arm64)
aws ec2 describe-instances \
  --filters "Name=tag:Name,Values=*runner*" "Name=architecture,Values=arm64" \
  --query 'Reservations[*].Instances[*].[InstanceId,PublicIpAddress,State.Name]' --output text

# SSH in
ssh -i ~/.ssh/runner_key -o StrictHostKeyChecking=no ubuntu@<ip>
cd ~/fcvm && git fetch && git checkout <branch>
make build && sudo make test-root FILTER=<test> STREAM=1 2>&1 | tee /tmp/test.log
```

Don't interfere with busy runners (check `busy=` status first).

---

## Downloading Test Log Artifacts

CI uploads test logs as artifacts (14 day retention). Use when you need full debug logs
beyond what the job log provides.

```bash
REPO=$(gh repo view --json nameWithOwner --jq '.nameWithOwner')
RUN_ID=<run_id>

# List artifacts
gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts" \
  --jq '.artifacts[] | "\(.id) \(.name) \(.size_in_bytes)"'

# Download a specific artifact
gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts/<artifact_id>/zip" > /tmp/artifact.zip
unzip -o /tmp/artifact.zip -d /tmp/ci-logs/

# Download ALL test log artifacts
for artifact in $(gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts" \
  --jq '.artifacts[] | select(.name | startswith("test-logs")) | "\(.id):\(.name)"'); do
  ID=${artifact%%:*}; NAME=${artifact#*:}
  echo "Downloading $NAME..."
  gh api "repos/$REPO/actions/runs/$RUN_ID/artifacts/$ID/zip" > "/tmp/$NAME.zip"
  mkdir -p "/tmp/ci-logs/$NAME"
  unzip -o "/tmp/$NAME.zip" -d "/tmp/ci-logs/$NAME"
done
```

**What to look for:** Full error messages (CI truncates stderr), timestamps (compare failing
vs passing for contention), boot sequence (fc-agent startup stalls), dmesg artifacts.

## Answering review findings

`scripts/check-review-threads.sh <pr>` is the gate. It blocks on UNRESOLVED threads,
UNDISPOSED resolved threads, and UNANSWERED PR-level review bodies. A defect claim is closed
by `RED-VERIFIED: <test>` naming a test you watched **fail without the fix** — not by an
assertion that it is fixed. `NOT-A-DEFECT:` and `DISAGREE:` are equally valid dispositions;
silence is not one.

Write the disposition as the FIRST thing in a reply of its own (`RED-VERIFIED: ...`), post it
AFTER the finding it answers, and use the same three words for findings that arrive in a
review body rather than inline. All three are enforced, and all three were once holes.

Verify the gate itself with `scripts/test-check-review-threads.sh` before trusting a CLEAR.
