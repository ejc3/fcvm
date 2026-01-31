---
name: pr-workflow
description: "ALWAYS USE THIS SKILL when: creating PRs, checking PR status, reviewing PRs, merging PRs, checking CI status, fixing lint/clippy errors, running cargo fmt, or any gh pr command. Invoke with /pr-workflow"
allowed-tools: Bash, Read, Grep, Glob
user-invocable: true
---

# Pull Request Workflow for Rust Projects

Follow this workflow when creating, updating, or merging pull requests.

## Before Creating/Pushing ANY Code

Run these checks locally - CI is for validation, not discovery:

```bash
# 1. Format check (REQUIRED)
cargo fmt --check
# If it fails: cargo fmt

# 2. Lint check (REQUIRED)
cargo clippy --all-targets -- -D warnings

# 3. Sanity tests (RECOMMENDED)
make test-root FILTER=sanity
# Or run tests relevant to your changes
```

## Creating a Pull Request

1. **Review your changes first**:
   ```bash
   git diff --stat              # What files changed
   git diff                     # Actual changes
   git log main..HEAD --oneline # All commits in this branch
   ```

2. **Push and create PR**:
   ```bash
   git push -u origin <branch-name>
   gh pr create --fill
   ```

3. **Wait for CI** - Do NOT proceed until all checks pass:
   ```bash
   gh pr checks <pr-number>
   # All checks must show "pass" before proceeding
   ```

## MANDATORY: Read PR Comments Before ANY PR Operation

**YOU MUST READ ALL PR COMMENTS** before:
- Checking PR status
- Pushing new commits
- Merging
- Closing
- ANY interaction with the PR

This is NOT optional. Comments contain critical information.

### Step 1: ALWAYS Read Comments First

```bash
# MANDATORY - Run this FIRST before any other PR operation
gh pr view <pr-number> --json comments --jq '.comments[] | "---\n" + .body'
```

### Step 2: Check for Auto-Fix PRs

CI may have created fix PRs targeting your branch. You MUST handle these:

```bash
# Check for fix PRs
gh pr list --search "base:<your-branch>"

# If fix PRs exist:
# 1. Review the fix
# 2. Cherry-pick: git cherry-pick <commit>
# 3. Push to your branch
# 4. Close the fix PR: gh pr close <fix-pr> --comment "Cherry-picked into PR #<your-pr>"
```

### Step 3: Address Review Findings

Comments may contain:
- **Code review findings** - Fix these before merging
- **Auto-fix PRs** - Cherry-pick or close if outdated
- **CI failure analysis** - Re-run if infra issue, fix if code issue
- **Security concerns** - Must address before merge

### Step 4: Verify CI is Green

```bash
gh pr checks <pr-number>
# ALL checks must show "pass"
# If Lint fails: run cargo fmt && cargo clippy, commit, push
```

### Then Merge

```bash
gh pr merge <pr-number> --merge --delete-branch
```

## Stacked PRs (Branch of Branch)

When your work builds on an unmerged PR:

```bash
# Create PR #2 based on PR #1's branch (not main!)
git checkout pr1-branch
git checkout -b pr2-branch
# ... make changes ...
git push -u origin pr2-branch
gh pr create --base pr1-branch  # Target the parent branch!

# Verify the chain
git log --oneline origin/main..HEAD  # Should show both PR's commits
```

**After PR #1 merges:**
1. Wait for GitHub to update PR #2's base to main
2. Verify: `gh pr view 2 --json baseRefName`
3. Only then merge PR #2

## Rust Code Quality Checklist

Before considering code "done":

- [ ] No `unwrap()` in production code (use `?` or proper error handling)
- [ ] No `clone()` without justification (prefer references)
- [ ] Feature-gated tests have matching `#[cfg(feature = "...")]` on imports
- [ ] Test names are descriptive: `test_<what>_<scenario>`
- [ ] Error messages include context (use `.context("what failed")`)
- [ ] No `println!` in library code (use `tracing::debug!` etc.)
- [ ] Public APIs have doc comments

## When Tests Fail

**NEVER skip, ignore, or weaken assertions.** Find and fix the root cause.

1. Read the error message completely
2. Check test logs: `/tmp/fcvm-test-logs/*.log`
3. Run the specific test with more output:
   ```bash
   RUST_LOG=debug make test-root FILTER=<failing_test> STREAM=1
   ```
4. Fix the CODE, not the test
