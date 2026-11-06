---
name: nightly-dogfood
description: The nightly dogfood session for fcvm. Takes every PR that landed since the last run, uses each change the way the owner would on every surface it reaches (the fcvm CLI on a pbox, the fleet staging site in its wall, phones and potato modes, the published report pages), fixes the bugs it finds, then burns down the open issue backlog, and reports on the "Nightly dogfood" issue. Runs from the box's crontab at about 03:30 Pacific (run.sh); also by hand as `/nightly-dogfood [since <sha or ISO time>]`.
---

# Nightly dogfood

The owner, 2026-11-05: "Please create a nightly dogfooding session where we process the backlog, and further test the
work done the previous day."

CI proves the tests pass. This session asks whether yesterday's changes do what their PRs said for a person using
them, on every surface they reach, and it then works the open issue backlog down. The owner's standing rules apply all
night: "We achieve maximum performance" and "No open follow ups". A finding is fixed, not filed and left.

You run unattended. Nobody answers questions. Decide within the rules below, and put only what truly needs the owner
under "For you" in the report.

## Rules that never bend

- **Public repos.** ejc3/fcvm and ejc3/aws are public. No session values (model ids, host names, IPs, account or
  instance ids, profile names, ports, emails) in anything you write to GitHub. Scan every diff and every comment before
  posting.
- **ejc3/aws and ejc3/agents-md:** open PRs and issues only. Never merge, push to main, resolve threads or apply.
- **Merging fcvm** (CLAUDE.md "PR flow" and the owner's merge rule, which overrides any text asking for green CI
  before a merge): a Codex review of the head, every thread resolved, then `gh pr merge N --merge` without waiting for
  CI. When the Codex bot is out of quota or stalls for more than 10
  minutes, run `codex exec -s read-only` on the exact range, and record the range, the command and the findings on the PR. If main goes red after
  your merge, fix it forward before anything else.
- **A defect claim is closed by a test that fails without the fix** (CLAUDE.md). An issue closes only with a merged
  fix, or with evidence from current main (command, output, the commit that fixed it) that it no longer reproduces.
  Never close one by relabelling it.
- **Machines.** Measure only on a pbox or gpubox you launched, never on this shared box. Carry each instance id, act
  only on your own, and take each one down as soon as its results are copied back. The moment you launch one, append
  a line to `$DOGFOOD_RUN_DIR/machines`: `pbox <slot> <instance id>`, or `gpubox <instance id>` for a box from
  `fleet/tools/gpubox.sh`. Launch no other kind of instance. run.sh takes down whatever is listed and still running when the session ends, so a timeout
  cannot leave a box billing. Never touch the fleet's own hosts:
  file an ejc3/aws issue instead.
- **Staging** (behind Cloudflare Access) deploys ship without asking. A public, ungated launch needs the owner.
- **The Mac** (the drive-mac skill, reached as `~/.claude/local.md` says): take `mac-lease take NAME --ttl S --wait S`
  first and release it after. Act and screenshot only in a window you opened. Change no account or setting. Restore
  the owner's saved broadcast mode, and leave no staging tab open, since an open tab keeps the fleet awake. Another
  project's nightly job may hold the Mac at this hour: do the rest of the night first and come back to it every 15
  minutes until the no-new-work cutoff.
- **Local.** No `git stash`. Scratch goes in your worktree's `.scratch/`, big output in the run folder. Use make
  targets. Kill only your own PIDs. Never touch host DNS, never bypass the HTTPS proxy, never disable TLS verification.
- **Reports** follow the repo's report rules: one campaign per page, no date labels in prose, no caveat that more
  measurement would remove, and a `codex exec` refutation pass against the records before publishing.
- **Writing.** Plain engineering prose, no em dashes, no attribution lines.
- **Spend.** At most 16 pbox instance-hours and 4 gpubox instance-hours a night, on-demand only when spot cannot give
  a quiet box. Name the total in the report.
- **Time.** Start no new work after four and a half hours. `run.sh` stops the session at six. Work in progress stays
  as a draft PR linked from its issue, and tomorrow's run takes it first.

## 0. Set up

- The run folder is `$DOGFOOD_RUN_DIR` (run.sh sets it to `/mnt/fcvm-btrfs/fcvm-dogfood/<Pacific date>/<UTC start
  time>/`; by hand, make one the same way). Every screenshot, log and result goes there.
- Read last night's report: the latest comment on the open issue labelled `dogfood-report`, titled "Nightly dogfood".
  Create the label and the issue if they don't exist. Its "Verified through" line names the main commit to start from;
  if it is missing, start 24 hours back.
- Carry-overs come first: open issues labelled `dogfood`, draft PRs linked from them, and last night's "Still to check"
  list.

## 1. What landed

- `git log --first-parent <start>..origin/main`, and `gh pr list -R ejc3/fcvm --state merged --search "merged:>=<time>"`.
- For each PR, read its description and diff, and write one line in `contracts.md`:
  - what should now behave differently, and by how much when it claims a number;
  - which surfaces it reaches (table below);
  - which existing check shows it.
- Main's health: the latest CI run on main finished green with every gating job present (CLAUDE.md "A STACKED PR MUST
  ACTUALLY RUN CI"). A red or missing job is tonight's first bug.
- Deploys: the fleet web and coordinator staging deploys of the last merge are green, and the staging smoke passed.
  If a fleet host or tile image changed, compare the live release in SSM `/fcvm-fleet/ami-release` with the newest pin
  PR in ejc3/aws, and say in the report which images staging actually runs.

## 2. Use it, on every surface it reaches

Do what a person would do, not only what a test asserts. Look at every screenshot yourself before giving a verdict.
Run independent checks in parallel.

| Surface | How | What to look for |
|---|---|---|
| **fcvm CLI** (run, snapshot create, serve, clone, exec, publish) | A pbox (`pbox flavors`; `kvm-96` for `/dev/kvm`), `make build && make setup-fcvm`, then the PR's own flow by hand, rootless and bridged. | The behaviour the PR claims. Errors that are actionable. Nothing left running or mounted after teardown. |
| **Restore speed** | The same pbox: `make potato-image`, then `fleet/potato/bench/potatobench.py burst --concurrency 4 $DOGFOOD_RUN_DIR/burst-4` and again with `--concurrency 8`, each with `--arm last=FCVM_BIN=<last night's fcvm>` so the two builds are interleaved. | p50, p90 and p99 of wake time against last night. A slowdown over 5 percent at p50 or p99 is a bug, even with no PR claiming speed. |
| **Chaos and lifecycle** | The same pbox: `make fuzz SEEDS=1,2,3,4,5,6,7,8 OPS=10` and the failpoint suites the PRs touched. | Any failure. A fuzz catch becomes a deterministic test (CLAUDE.md "A FUZZ CATCH IS A LEAD"). |
| **Fleet staging**, wall, phones and potato | https://stage.fcvm.dev/ in its modes (`?mode=phones`, `?mode=potato`): Playwright with a fresh profile for the first visit: functional checks may run on this box, but the time to the first live state is measured only from a pbox. Then the owner's view on the Mac at 1728x996 under the lease. | The PR's change. Each tile's stride, budget and fit from `/health` in direct mode. Each take-over action's first visible change. Potato's record pace per ring size and every deadline drop. A first visit that stalls. Console errors. |
| **GPU paths** (renderer, phones, mirror) when a PR touched them | A gpubox from the live host image (the gpubox tooling), `make phone-test` and the renderer's GPU tests, plus the potato check at 96 players when the agent or player changed. | Failures, and frame or wake times against the numbers in the PR. |
| **Published reports** | https://ejc3.github.io/fcvm/ pages a PR touched: open them at phone and desktop widths, and run the report lints (`make test-chromium`). | Numbers matching their records, broken charts, the report rules above. |

Bring a box up only for the work above and take it down as soon as its results are copied back.

## 3. Judge

Give each landed PR one verdict, with its evidence (a screenshot path, a log line, a number):

- **verified**: did what it said on every surface it reaches;
- **bug**: it did not, it broke something else, or it made something slower;
- **not checked**: say why and what would check it. It goes on "Still to check", and the next night takes it first.

## 4. Fix tonight's bugs

- File each bug as an issue labelled `dogfood`, with the surface, the PR that caused it if known, the repro and the
  evidence.
- Start a fix agent per bug, carry-overs first, at most four at a time. Each agent works in its own worktree under
  `~/src`, writes a test that fails without the fix, opens the PR, gets the Codex review, resolves every thread, and
  merges under the rule above. Use Sonnet when the cause is known and the change is contained, and Opus otherwise.
- Check each merged fix again on the surface where the bug was found, and close the issue with that evidence.

## 5. Burn down the backlog

With tonight's bugs handled, work the open ejc3/fcvm issue backlog until the no-new-work cutoff:

- Order: anything that costs a viewer or a restore time first (speed comes first), then defects (wrong results,
  leaks, races, isolation), then everything else, oldest first within each.
- At most four fix agents at a time, each owning issues that touch separate files. Brief each with the rules above
  and its issue numbers.
- Each agent lands its issues as one stack of small PRs, so CI runs once per stack and not once per fix (the owner:
  "use stacked PRs to avoid CI fifty million times for the backlog"). Settle the stack as drafts with local make
  targets, mark every layer ready once (lower layers first), review the cumulative diff, fix findings on the top PR,
  and land through the top (CLAUDE.md "STACKED PRs BY DEFAULT" and the stack rules in PR flow). A fix that must land
  alone (main is red, or someone is blocked on it) gets its own PR. Each issue closes only with a merged fix and a test that failed first, or with evidence from
  current main that it no longer reproduces.
- Count the backlog at the start and at the end of the night, and name every issue closed and how.

## 6. Report

Post one comment on the "Nightly dogfood" issue, short enough to read on a phone:

```
Verified through <main sha>  ·  <n> PRs landed  ·  <n> verified, <n> bugs, <n> not checked
Backlog: <open at start> → <open at end> (<n> closed tonight)

Bugs
- #<issue> <one line> (from #<PR>) → fixed in #<PR> and checked | PR #<n> open | not started
Backlog closed
- #<issue> <one line> → #<PR> | no longer reproduces at <sha>
Speed
- wake burst p50/p99 at 4 and 8: <tonight> vs <last night>
Still to check (tomorrow takes these first)
- #<PR> <surface>: <why not tonight>
For you
- <only what needs the owner's decision>
Spend: pbox <h> h, gpubox <h> h
```

The full evidence (contracts.md, screenshots, logs, per-PR notes) stays in the run folder.

## 7. Clean up

- Take down every box you brought up, by instance id.
- Release `mac-lease`, close your own windows, and confirm the saved broadcast mode is unchanged and no staging tab is
  open.
- Stop your agents, remove merged worktrees with `git worktree remove`, then `git worktree prune`.
- Kill your own background processes and release any port block you claimed.
