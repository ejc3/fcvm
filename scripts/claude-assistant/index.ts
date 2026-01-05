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
import { mkdirSync, writeFileSync } from "fs";
import { join } from "path";

const MAX_LOG_SIZE = 100_000;
const LOGS_DIR = "/tmp/ci-logs";

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
  failedRunId?: string;
  failedRunUrl?: string;
  workflowName?: string;
  commentBody?: string;
}

function log(msg: string): void {
  console.log(`[driver] ${msg}`);
}

function run(cmd: string, args: string[]): { stdout: string; ok: boolean } {
  log(`$ ${cmd} ${args.join(" ")}`);
  const result = spawnSync(cmd, args, { encoding: "utf-8", maxBuffer: 10 * 1024 * 1024 });
  return { stdout: result.stdout?.trim() ?? "", ok: result.status === 0 };
}

function git(...args: string[]): boolean {
  return run("git", args).ok;
}

function gh(...args: string[]): { stdout: string; ok: boolean } {
  return run("gh", args);
}

function setupFixBranch(ctx: Context): void {
  log(`Creating fix branch: ${ctx.fixBranch}`);
  git("config", "user.name", "claude[bot]");
  git("config", "user.email", "claude[bot]@users.noreply.github.com");
  git("checkout", "-b", ctx.fixBranch);
}

function downloadLogs(ctx: Context): string {
  mkdirSync(LOGS_DIR, { recursive: true });
  if (!ctx.failedRunId) return LOGS_DIR;

  const result = gh(
    "api", `repos/${ctx.repository}/actions/runs/${ctx.failedRunId}/jobs`,
    "--jq", '.jobs[] | select(.conclusion == "failure") | {id: .id, name: .name}'
  );

  if (result.ok && result.stdout) {
    for (const line of result.stdout.split("\n")) {
      if (!line.trim()) continue;
      try {
        const job = JSON.parse(line);
        log(`Downloading: ${job.name}`);
        const logResult = gh("api", `repos/${ctx.repository}/actions/jobs/${job.id}/logs`);
        if (logResult.ok) {
          let content = logResult.stdout;
          if (content.length > MAX_LOG_SIZE) content = content.slice(0, MAX_LOG_SIZE) + "\n...truncated";
          writeFileSync(join(LOGS_DIR, `job-${job.id}.log`), content);
        }
      } catch { continue; }
    }
  }

  const summary = gh("run", "view", ctx.failedRunId, "--repo", ctx.repository, "--log-failed");
  if (summary.ok) {
    let content = summary.stdout;
    if (content.length > MAX_LOG_SIZE) content = content.slice(0, MAX_LOG_SIZE) + "\n...truncated";
    writeFileSync(join(LOGS_DIR, "run-summary.log"), content);
  }

  log(`Logs saved to ${LOGS_DIR}`);
  return LOGS_DIR;
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
Your current branch is \`${ctx.fixBranch}\`, branched from \`${ctx.headBranch}\`.

## DESIGN

1. **Review Phase**: Analyze the PR diff, identify issues
2. **Comment Phase**: Post review to PR #${ctx.prNumber}
3. **Fix Phase** (if needed): Edit files, commit, push, create stacked PR

All GitHub operations target **PR #${ctx.prNumber}** in **${ctx.repository}**.

---

## STEP 1: GET THE DIFF

\`\`\`bash
git diff origin/${ctx.baseBranch}...origin/${ctx.headBranch}
\`\`\`

## STEP 2: ANALYZE

Categorize issues:
- \`[CRITICAL]\` - Security holes, data loss, crashes, breaking changes
- \`[MEDIUM]\` - Bugs, logic errors, race conditions, missing validation
- \`[LOW]\` - Style, naming, minor improvements

## STEP 3: POST REVIEW

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

## STEP 4: FIX (only if [MEDIUM] or [CRITICAL] issues found)

### 4a. Edit files
Make the minimum changes needed to fix the issues.

### 4b. Commit
\`\`\`bash
git add -A
git commit -m "fix: <concise description>"
\`\`\`

### 4c. Push
\`\`\`bash
git push origin ${ctx.fixBranch}
\`\`\`

### 4d. Create stacked PR

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

### 4e. Update original PR

Post to **PR #${ctx.prNumber}**:

\`\`\`bash
gh pr comment ${ctx.prNumber} --repo ${ctx.repository} --body "## 🔧 Auto-Fix Created

I found issues and created a fix PR: <URL from 4d>

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

**BEGIN**: Run the diff command.`;
}

function ciFixPrompt(ctx: Context, logsDir: string): string {
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

Logs are in \`${logsDir}/\`:
- \`run-summary.log\` - Failed run overview
- \`job-*.log\` - Individual failed job logs

\`\`\`bash
cat ${logsDir}/run-summary.log
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
  for await (const message of query({
    prompt,
    options: {
      allowedTools: ["Read", "Write", "Edit", "Bash", "Glob", "Grep"],
      permissionMode: "acceptEdits",
      maxTurns: 30,
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

async function main(): Promise<void> {
  if (!process.env.ANTHROPIC_API_KEY) {
    console.error("ANTHROPIC_API_KEY is required");
    process.exit(1);
  }

  const mode = (process.env.MODE ?? "review") as Mode;
  const runId = process.env.RUN_ID!;

  const ctx: Context = {
    mode,
    repository: process.env.GITHUB_REPOSITORY!,
    prNumber: parseInt(process.env.PR_NUMBER ?? "0", 10),
    headBranch: process.env.HEAD_BRANCH!,
    headSha: process.env.HEAD_SHA!,
    baseBranch: process.env.BASE_BRANCH ?? "main",
    runId,
    runUrl: process.env.RUN_URL ?? "",
    fixBranch: `claude/fix-${runId}`,
    failedRunId: process.env.FAILED_RUN_ID,
    failedRunUrl: process.env.FAILED_RUN_URL,
    workflowName: process.env.WORKFLOW_NAME,
    commentBody: process.env.COMMENT_BODY,
  };

  log(`Mode: ${mode}, Repo: ${ctx.repository}, PR: #${ctx.prNumber}`);

  setupFixBranch(ctx);

  let prompt: string;
  switch (mode) {
    case "review":
      prompt = reviewPrompt(ctx);
      break;
    case "ci-fix":
      prompt = ciFixPrompt(ctx, downloadLogs(ctx));
      break;
    case "respond":
      prompt = respondPrompt(ctx);
      break;
  }

  await runClaude(prompt);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
