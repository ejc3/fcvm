#!/usr/bin/env tsx
/**
 * Claude Assistant - Unified PR Review & Auto-Fix
 *
 * Driver responsibilities:
 * - Create fix branch (claude/fix-{runId})
 * - Configure git user
 * - Download CI logs (ci-fix mode)
 * - Launch Claude Code with detailed instructions
 *
 * Claude Code responsibilities:
 * - Read code, run commands, analyze
 * - Edit files to make fixes
 * - git add, commit, push
 * - gh pr create, gh pr comment
 */

import { query } from "@anthropic-ai/claude-agent-sdk";
import { spawnSync } from "child_process";
import { mkdirSync, writeFileSync, existsSync } from "fs";
import { join, resolve } from "path";
import { tmpdir } from "os";

// Configurable limits with sensible defaults
const MAX_LOG_SIZE = parseInt(process.env.CLAUDE_MAX_LOG_SIZE ?? "100000", 10);
const MAX_TURNS = parseInt(process.env.CLAUDE_MAX_TURNS ?? "75", 10);

// Branch naming convention for Claude-generated fix branches
const FIX_BRANCH_PREFIX = "claude/fix-";

type Mode = "review" | "ci-fix" | "respond";

interface Context {
  mode: Mode;
  repository: string;
  prNumber: number;
  headBranch: string;
  headSha: string;
  baseBranch: string;
  runId: string;
  runUrl: string;
  fixBranch: string;
  logsDir: string;
  failedRunId?: string;
  failedRunUrl?: string;
  workflowName?: string;
  commentBody?: string;
  memberPRs?: string;  // Open PRs from org members
}

function log(msg: string): void {
  console.log(`[driver] ${msg}`);
}

function logError(msg: string): void {
  console.error(`[driver] ERROR: ${msg}`);
}

function run(cmd: string, args: string[]): { stdout: string; stderr: string; ok: boolean } {
  log(`$ ${cmd} ${args.join(" ")}`);
  const result = spawnSync(cmd, args, { encoding: "utf-8", maxBuffer: 10 * 1024 * 1024 });
  if (result.status !== 0 && result.stderr) {
    logError(`Command failed: ${result.stderr.trim()}`);
  }
  return {
    stdout: result.stdout?.trim() ?? "",
    stderr: result.stderr?.trim() ?? "",
    ok: result.status === 0,
  };
}

function git(...args: string[]): boolean {
  const result = run("git", args);
  return result.ok;
}

function gitOrFail(...args: string[]): void {
  const result = run("git", args);
  if (!result.ok) {
    throw new Error(`git ${args[0]} failed: ${result.stderr}`);
  }
}

function gh(...args: string[]): { stdout: string; ok: boolean } {
  const result = run("gh", args);
  return { stdout: result.stdout, ok: result.ok };
}

function branchExists(branchName: string): boolean {
  const result = run("git", ["rev-parse", "--verify", branchName]);
  return result.ok;
}

/**
 * Fetch open PRs from org members or bots only.
 * Filters out PRs from external contributors to avoid prompt injection.
 */
function fetchMemberPRs(repository: string): string {
  log("Fetching open PRs from members/bots");
  const result = gh(
    "api", `repos/${repository}/pulls`,
    "--jq", `.[] | select(.author_association == "OWNER" or .author_association == "MEMBER" or .author_association == "COLLABORATOR" or .user.type == "Bot") | "#\(.number) \(.title) (@\(.user.login))"`
  );
  if (!result.ok || !result.stdout) {
    return "Failed to fetch PRs";
  }
  return result.stdout || "No open PRs from members";
}

function setupFixBranch(ctx: Context): void {
  log(`Setting up fix branch: ${ctx.fixBranch}`);

  // Configure git user (only if not already set)
  const nameResult = run("git", ["config", "user.name"]);
  if (!nameResult.ok || !nameResult.stdout) {
    gitOrFail("config", "user.name", "claude[bot]");
  }

  const emailResult = run("git", ["config", "user.email"]);
  if (!emailResult.ok || !emailResult.stdout) {
    gitOrFail("config", "user.email", "claude[bot]@users.noreply.github.com");
  }

  // Fetch latest to ensure we have the PR branch
  gitOrFail("fetch", "origin", ctx.headBranch);

  // Check if branch already exists
  if (branchExists(ctx.fixBranch)) {
    log(`Branch ${ctx.fixBranch} already exists, checking out and updating`);
    gitOrFail("checkout", ctx.fixBranch);
    gitOrFail("reset", "--hard", `origin/${ctx.headBranch}`);
  } else {
    // Create fix branch FROM the PR branch, not from main
    // This ensures Claude can read PR files locally with the Read tool
    log(`Creating new branch ${ctx.fixBranch} from origin/${ctx.headBranch}`);
    gitOrFail("checkout", "-b", ctx.fixBranch, `origin/${ctx.headBranch}`);
  }
}

function downloadLogs(ctx: Context): void {
  mkdirSync(ctx.logsDir, { recursive: true });
  if (!ctx.failedRunId) {
    logError("No failed run ID provided - cannot download logs");
    return;
  }

  log(`Downloading logs to ${ctx.logsDir}`);

  const result = gh(
    "api", `repos/${ctx.repository}/actions/runs/${ctx.failedRunId}/jobs`,
    "--jq", '.jobs[] | select(.conclusion == "failure") | {id: .id, name: .name}'
  );

  if (!result.ok) {
    logError(`Failed to fetch job list for run ${ctx.failedRunId}`);
    writeFileSync(join(ctx.logsDir, "error.log"), `Failed to fetch job list: ${result.stdout}`);
    return;
  }

  let jobsDownloaded = 0;
  if (result.stdout) {
    for (const line of result.stdout.split("\n")) {
      if (!line.trim()) continue;
      try {
        const job = JSON.parse(line);
        log(`Downloading: ${job.name}`);
        const logResult = gh("api", `repos/${ctx.repository}/actions/jobs/${job.id}/logs`);
        if (logResult.ok) {
          let content = logResult.stdout;
          if (content.length > MAX_LOG_SIZE) content = content.slice(0, MAX_LOG_SIZE) + "\n...truncated";
          writeFileSync(join(ctx.logsDir, `job-${job.id}.log`), content);
          jobsDownloaded++;
        } else {
          logError(`Failed to download logs for job ${job.id}`);
        }
      } catch (e) {
        logError(`Failed to parse job data: ${e}`);
        continue;
      }
    }
  }

  const summary = gh("run", "view", ctx.failedRunId, "--repo", ctx.repository, "--log-failed");
  if (summary.ok) {
    let content = summary.stdout;
    if (content.length > MAX_LOG_SIZE) content = content.slice(0, MAX_LOG_SIZE) + "\n...truncated";
    writeFileSync(join(ctx.logsDir, "run-summary.log"), content);
  } else {
    logError(`Failed to download run summary for run ${ctx.failedRunId}`);
  }

  if (jobsDownloaded === 0 && !existsSync(join(ctx.logsDir, "run-summary.log"))) {
    logError("CRITICAL: No logs were successfully downloaded. Claude may not have sufficient context to fix the issue.");
    writeFileSync(join(ctx.logsDir, "error.log"), "Failed to download any logs. This may prevent accurate diagnosis.");
  }

  log(`Logs saved to ${ctx.logsDir} (${jobsDownloaded} job logs downloaded)`);
}

function reviewPrompt(ctx: Context): string {
  return `# Claude Code Review Task

## CONTEXT - READ CAREFULLY

| Field | Value |
|-------|-------|
| **Repository** | ${ctx.repository} |
| **PR Number** | #${ctx.prNumber} |
| **PR Branch** | ${ctx.headBranch} |
| **Base Branch** | ${ctx.baseBranch} |
| **Your Branch** | ${ctx.fixBranch} (already checked out) |
| **Run URL** | ${ctx.runUrl} |

You are reviewing **PR #${ctx.prNumber}** in **${ctx.repository}**.
Your current branch is \`${ctx.fixBranch}\`, branched from \`origin/${ctx.headBranch}\`.
**The PR's code is already checked out locally** - you can use the Read tool directly on any file.

### Open PRs (from members/bots only)
\`\`\`
${ctx.memberPRs}
\`\`\`

## DESIGN

1. **Review Phase**: Analyze the PR diff, identify issues
2. **Comment Phase**: Post review to PR #${ctx.prNumber}
3. **Fix Phase** (if needed): Edit files, commit, push, create stacked PR

All GitHub operations target **PR #${ctx.prNumber}** in **${ctx.repository}**.

---

## CRITICAL: IF YOU MAKE CHANGES

When creating fix PRs, you MUST follow the complete workflow in STEP 5, including:
- **Run lint locally BEFORE committing** (step 5b)

**Never push code that doesn't pass lint.**

---

## STEP 1: UNDERSTAND THE FULL SCOPE

### 1a. Get PR context (title, description, comments)
\`\`\`bash
gh pr view ${ctx.prNumber} --repo ${ctx.repository}
gh pr view ${ctx.prNumber} --repo ${ctx.repository} --comments
\`\`\`

### 1b. Get the commit history
\`\`\`bash
git log origin/${ctx.baseBranch}..origin/${ctx.headBranch} --oneline
\`\`\`

Review each commit message to understand the progression of changes.

### 1d. Get the complete diff
\`\`\`bash
git diff origin/${ctx.baseBranch}...origin/${ctx.headBranch}
\`\`\`

### 1e. If diff output is large or appears truncated
The diff output may be truncated for large changes. If the diff ends mid-line or seems incomplete:
- Use \`git diff --stat\` to see which files changed
- Read individual files directly with the Read tool (PR code is checked out locally)
- **NEVER assume a file is incomplete based on truncated diff output**

## STEP 2: CHECK PREVIOUS REVIEWS

Look at the comments from step 1a. If there are previous Claude reviews:
- **[LOW] issues**: Do NOT mention again. Skip entirely.
- **[MEDIUM]/[CRITICAL] issues that are STILL UNFIXED**: Reference with a link, do NOT repeat the analysis.

When referencing a previous comment, get the comment URL:
\`\`\`bash
gh api repos/${ctx.repository}/issues/${ctx.prNumber}/comments --jq '.[] | select(.user.login == "claude[bot]") | {id: .id, url: .html_url}'
\`\`\`

Then link to it: "As noted in [previous review](URL), the PATH inconsistency remains unfixed."

**CRITICAL**: If an issue was already reported, do NOT repeat the full analysis. One-line summary with link only.

## STEP 3: ANALYZE

Categorize issues:
- \`[CRITICAL]\` - Security holes, data loss, crashes, breaking changes
- \`[MEDIUM]\` - Bugs, logic errors, race conditions, missing validation
- \`[LOW]\` - Style, naming, minor improvements (only if not previously mentioned)

## STEP 4: POST REVIEW

Post to **PR #${ctx.prNumber}**:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## 🔍 Claude Review

SEVERITY: <critical|medium|low|none>

### Findings

<list each issue with [CRITICAL], [MEDIUM], or [LOW] prefix>

### Summary

<1-2 sentence summary>

---
_Review by Claude | [Run](${ctx.runUrl})_"
\`\`\`

## STEP 5: DECIDE WHETHER TO FIX OR BLOCK

When you find [MEDIUM] or [CRITICAL] issues, decide:

### Option A: BLOCK (if fix would revert the PR's changes)

If fixing the issue means undoing/reverting what the PR did, **do NOT create a fix PR**.
Instead, comment that the PR should not be merged:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## ⛔ Do Not Merge

This PR has critical issues that require changes by the author before merging.

### Issues Requiring Author Action
<list the issues>

### Why No Auto-Fix
The fix would essentially revert this PR's changes. The author should address these issues directly.

---
_Review by Claude | [Run](${ctx.runUrl})_"
\`\`\`

Then **STOP** - do not create any fix PR.

### Option B: FIX (if you're adding improvements on top)

Only create a fix PR if you're **adding to** the PR's changes, not undoing them.
Good examples: adding missing error handling, fixing a typo, adding a missing test.
Bad examples: reverting a permission change, removing a feature the PR added.

### 5a. Edit files
Make the minimum changes needed to fix the issues.

**IMPORTANT: Edit files directly in the repo. NEVER:**
- Copy files to /tmp and edit there
- Create temp files and copy back
- Use intermediate files

Just use the Edit tool directly on the source files.

### 5b. Run lint locally
**BEFORE committing**, verify your changes pass lint:

\`\`\`bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
\`\`\`

If lint fails, fix the issues before proceeding. **NEVER commit code that doesn't pass lint.**

### 5c. Commit
\`\`\`bash
git add -A
git commit -m "fix: <concise description>"
\`\`\`

### 5d. Push
\`\`\`bash
git push origin ${ctx.fixBranch}
\`\`\`

### 5e. Create stacked PR

Target: \`${ctx.headBranch}\` (the original PR branch)

\`\`\`bash
gh pr create --repo ${ctx.repository} \\
  --base ${ctx.headBranch} \\
  --head ${ctx.fixBranch} \\
  --title "fix: <description>" \\
  --body "## Auto-Fix for PR #${ctx.prNumber}

### Issues Fixed
<list what you fixed>

### Changes
<brief description>

---
_Generated by Claude | [Review Run](${ctx.runUrl})_"
\`\`\`

### 5f. Update original PR

Post to **PR #${ctx.prNumber}**:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## 🔧 Auto-Fix Created

I found issues and created a fix PR: <URL from 5f>

Please review and merge the fix PR first, then this PR.

[View Claude Run](${ctx.runUrl})"
\`\`\`

---

## RULES

1. All \`gh pr comment\` commands MUST use \`--repo ${ctx.repository}\` and PR number \`${ctx.prNumber}\`
2. The fix PR base MUST be \`${ctx.headBranch}\` (stacked PR pattern)
3. If no issues found, post review and STOP
4. If issues found but unfixable, explain in review and STOP
5. Never skip steps
6. **Do NOT repeat [LOW] issues** already mentioned in previous reviews - no value added

**BEGIN**: Get the PR context (step 1a).`;
}

function ciFixPrompt(ctx: Context): string {
  return `# Claude CI Fix Task

## CONTEXT - READ CAREFULLY

| Field | Value |
|-------|-------|
| **Repository** | ${ctx.repository} |
| **Workflow** | ${ctx.workflowName} |
| **Failed Run** | [#${ctx.failedRunId}](${ctx.failedRunUrl}) |
| **Branch** | ${ctx.headBranch} |
| **Your Branch** | ${ctx.fixBranch} (already checked out) |
${ctx.prNumber ? `| **Related PR** | #${ctx.prNumber} |` : ""}
| **Run URL** | ${ctx.runUrl} |

You are fixing a CI failure for **${ctx.workflowName}** on branch **${ctx.headBranch}**.
Your current branch is \`${ctx.fixBranch}\`, branched from \`${ctx.headBranch}\`.

## DESIGN

1. **Diagnose Phase**: Read logs, identify root cause
2. **Fix Phase** (if possible): Edit files, commit, push, create PR
3. **Report Phase**: Post status to ${ctx.prNumber ? `PR #${ctx.prNumber}` : "explain findings"}

---

## STEP 1: READ LOGS

Logs are in \`${ctx.logsDir}/\`:
- \`run-summary.log\` - Failed run overview
- \`job-*.log\` - Individual failed job logs

\`\`\`bash
cat ${ctx.logsDir}/run-summary.log
\`\`\`

Then read specific job logs as needed.

## STEP 2: DIAGNOSE

Determine:
- What exactly failed?
- Root cause?
- Fixable in code, or environmental (flaky test, network, infra)?

## STEP 3: FIX (if code issue)

### 3a. Edit files
Make the minimum changes to fix the failure.

### 3b. Commit
\`\`\`bash
git add -A
git commit -m "fix: <what you fixed>"
\`\`\`

### 3c. Push
\`\`\`bash
git push origin ${ctx.fixBranch}
\`\`\`

### 3d. Create PR

\`\`\`bash
gh pr create --repo ${ctx.repository} \\
  --base ${ctx.headBranch} \\
  --head ${ctx.fixBranch} \\
  --title "fix: <description>" \\
  --body "## CI Fix

Fixes [${ctx.workflowName} #${ctx.failedRunId}](${ctx.failedRunUrl})

### Problem
<what was broken>

### Solution
<what you changed>

---
_Generated by Claude | [Fix Run](${ctx.runUrl})_"
\`\`\`

${ctx.prNumber ? `### 3e. Update PR #${ctx.prNumber}

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## 🔧 CI Auto-Fix

Created fix PR: <URL from 3d>

[View Claude Run](${ctx.runUrl})"
\`\`\`` : ""}

## STEP 4: IF NOT FIXABLE

${ctx.prNumber ? `Post to **PR #${ctx.prNumber}**:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## CI Failure Analysis

I analyzed [${ctx.workflowName} #${ctx.failedRunId}](${ctx.failedRunUrl}) but couldn't create an automatic fix.

### Diagnosis
<what you found>

### Why Not Fixable
<explanation - e.g., flaky test, network issue, needs human decision>

[View Claude Run](${ctx.runUrl})"
\`\`\`` : "Print your analysis explaining why it's not fixable."}

---

## RULES

1. Read logs FIRST before any other action
2. All \`gh\` commands MUST include \`--repo ${ctx.repository}\`
${ctx.prNumber ? `3. Status updates go to PR #${ctx.prNumber}` : ""}
4. Fix PR base MUST be \`${ctx.headBranch}\`
5. If environmental issue, report and STOP (don't make code changes)

**BEGIN**: Read the run summary log.`;
}

function respondPrompt(ctx: Context): string {
  return `# Claude Response Task

## CONTEXT - READ CAREFULLY

| Field | Value |
|-------|-------|
| **Repository** | ${ctx.repository} |
| **PR Number** | #${ctx.prNumber} |
| **PR Branch** | ${ctx.headBranch} |
| **Your Branch** | ${ctx.fixBranch} (already checked out) |
| **Run URL** | ${ctx.runUrl} |

Someone mentioned @claude on **PR #${ctx.prNumber}** in **${ctx.repository}**.

## THE COMMENT

\`\`\`
${ctx.commentBody}
\`\`\`

## DESIGN

1. Understand the request
2. Research if needed (read files, run commands)
3. Respond OR make changes + create PR

All responses go to **PR #${ctx.prNumber}**.

---

## OPTION A: JUST RESPOND

If they asked a question or want explanation:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "<your response>"
\`\`\`

## OPTION B: MAKE CHANGES

If they want code changes:

### 1. Edit files

### 2. Commit and push
\`\`\`bash
git add -A
git commit -m "fix: <description>"
git push origin ${ctx.fixBranch}
\`\`\`

### 3. Create PR
\`\`\`bash
gh pr create --repo ${ctx.repository} \\
  --base ${ctx.headBranch} \\
  --head ${ctx.fixBranch} \\
  --title "<description>" \\
  --body "Requested by @user in PR #${ctx.prNumber}

<description of changes>

---
_Generated by Claude_"
\`\`\`

### 4. Respond
\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "Done! Created PR: <URL>

[View Claude Run](${ctx.runUrl})"
\`\`\`

---

## RULES

1. All \`gh pr comment\` MUST use \`--repo ${ctx.repository}\` and PR \`${ctx.prNumber}\`
2. Always acknowledge the request
3. Be concise but helpful

**BEGIN**: Read the comment and determine what they want.`;
}

async function runClaude(prompt: string): Promise<void> {
  log(`Starting Claude Code with maxTurns=${MAX_TURNS}`);
  for await (const message of query({
    prompt,
    options: {
      model: "claude-opus-4-6",
      allowedTools: ["Read", "Write", "Edit", "Bash", "Glob", "Grep"],
      permissionMode: "acceptEdits",
      maxTurns: MAX_TURNS,
    },
  })) {
    if (message.type === "assistant" && "message" in message && message.message?.content) {
      for (const block of message.message.content) {
        if (block.type === "text") console.log(block.text);
        else if (block.type === "tool_use") log(`Tool: ${block.name}`);
      }
    } else if (message.type === "result" && "result" in message) {
      console.log(`\nResult: ${message.result}`);
    }
  }
}

function validateEnvironment(): void {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.CLAUDE_CODE_OAUTH_TOKEN) {
    throw new Error("ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN is required");
  }

  const repo = process.env.GITHUB_REPOSITORY;
  if (!repo) {
    throw new Error("GITHUB_REPOSITORY is required");
  }
  // Validate owner/repo format
  if (!/^[a-zA-Z0-9_-]+\/[a-zA-Z0-9_.-]+$/.test(repo)) {
    throw new Error(`GITHUB_REPOSITORY has invalid format: ${repo}. Expected owner/repo`);
  }

  if (!process.env.HEAD_BRANCH) {
    throw new Error("HEAD_BRANCH is required");
  }

  const runId = process.env.RUN_ID;
  if (!runId) {
    throw new Error("RUN_ID is required");
  }
  // Validate RUN_ID is numeric
  if (!/^\d+$/.test(runId)) {
    throw new Error(`RUN_ID has invalid format: ${runId}. Expected numeric value`);
  }
}

function validatePrNumber(prNumber: number, mode: Mode): void {
  if (mode === "review" || mode === "respond") {
    if (!prNumber || prNumber <= 0 || !Number.isInteger(prNumber)) {
      throw new Error(`Invalid PR number: ${prNumber}. Expected positive integer.`);
    }
  }
  // ci-fix mode allows prNumber=0 for main branch failures
}

function sanitizeCommentBody(body: string | undefined): string | undefined {
  if (!body) return undefined;
  // Limit size to prevent prompt injection attacks
  const MAX_COMMENT_SIZE = 10000;
  if (body.length > MAX_COMMENT_SIZE) {
    // Keep first 60% and last 30% to preserve both context and recent content
    const keepStart = Math.floor(MAX_COMMENT_SIZE * 0.6);
    const keepEnd = Math.floor(MAX_COMMENT_SIZE * 0.3);
    const start = body.slice(0, keepStart);
    const end = body.slice(-keepEnd);
    return `${start}\n\n...[truncated ${body.length - MAX_COMMENT_SIZE} chars]...\n\n${end}`;
  }
  return body;
}

async function main(): Promise<void> {
  try {
    validateEnvironment();
  } catch (e) {
    console.error((e as Error).message);
    process.exit(1);
  }

  const mode = (process.env.MODE ?? "review") as Mode;
  const runId = process.env.RUN_ID!;
  const prNumber = parseInt(process.env.PR_NUMBER ?? "0", 10);

  try {
    validatePrNumber(prNumber, mode);
  } catch (e) {
    console.error((e as Error).message);
    process.exit(1);
  }

  // Use system temp dir instead of hardcoded path
  const logsDir = join(tmpdir(), `claude-ci-logs-${runId}`);

  const repository = process.env.GITHUB_REPOSITORY!;
  const ctx: Context = {
    mode,
    repository,
    prNumber,
    headBranch: process.env.HEAD_BRANCH!,
    headSha: process.env.HEAD_SHA ?? "",
    baseBranch: process.env.BASE_BRANCH ?? "main",
    runId,
    runUrl: process.env.RUN_URL ?? "",
    fixBranch: `${FIX_BRANCH_PREFIX}${runId}`,
    logsDir,
    failedRunId: process.env.FAILED_RUN_ID,
    failedRunUrl: process.env.FAILED_RUN_URL,
    workflowName: process.env.WORKFLOW_NAME,
    commentBody: sanitizeCommentBody(process.env.COMMENT_BODY),
    memberPRs: fetchMemberPRs(repository),
  };

  log(`Mode: ${mode}, Repo: ${ctx.repository}, PR: #${ctx.prNumber}`);

  setupFixBranch(ctx);

  let prompt: string;
  switch (mode) {
    case "review":
      prompt = reviewPrompt(ctx);
      break;
    case "ci-fix":
      downloadLogs(ctx);
      prompt = ciFixPrompt(ctx);
      break;
    case "respond":
      prompt = respondPrompt(ctx);
      break;
  }

  await runClaude(prompt);

  // Print completion marker - if this doesn't appear in logs, the job was truncated/killed
  console.log("\n=== CLAUDE_ASSISTANT_COMPLETE ===\n");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
