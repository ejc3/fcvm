# fcvm Development Log

## ALWAYS FIX FAILING TESTS. PERIOD.

**If ANY test fails, FIX THE ROOT CAUSE.** No exceptions. No workarounds. No weakening assertions.

- Never add flags like `--no-cache` to avoid failures
- Never weaken assertions to accept broken behavior
- Never skip, ignore, or comment out failing tests
- Always fix the actual bug in the code

This is non-negotiable. A test failure means the CODE is broken - fix the code, not the test.

## ZERO RACE CONDITIONS. ZERO.

**Never write code that can race.** Not "unlikely to race." Not "only races under load." ZERO.

- If two processes can touch the same file, use `flock()` or atomic rename
- If two threads can touch the same state, use a mutex or atomic
- If setup runs at shutdown, it WILL race with the shutdown sequence — handle it
- If a container build runs in parallel tests, it WILL collide — serialize or lock it
- "Works most of the time" means "broken" — races are bugs, period

The stale `db.sql` disaster happened because setup "usually" cleaned up before shutdown recreated the file. "Usually" is not "always." One race condition broke every VM boot across all CI runners.

**Before committing, ask:** "Can any other process/thread/test touch this state at the same time?" If yes, add synchronization or make it atomic. No exceptions.

## WRITE PLAINLY, ENGINEER TO ENGINEER

Commit messages, PR descriptions, code comments, docs: all of it is one engineer telling
another what changed and why. Say that and stop.

- State the change, the reason, and the evidence. Nothing else belongs there.
- No selling. Cut "worth having", "the real win", "makes it trustworthy", "elegant",
  "powerful", "seamless", "comprehensive", "robust".
- No throat-clearing. First sentence names the change. Skip the scene-setting paragraph.
- No rhetorical scaffolding: "Two things follow from that", "Four properties make this
  work", "Here's the thing". Make the point instead of announcing it.
- No staccato triples ("No traceback. No crash. No exception."). One normal sentence.
- No em dashes. Comma, parenthesis, or a new sentence.
- Don't narrate the journey. "I first tried X, then found Y" belongs nowhere. Describe the
  end state.
- Don't restate the diff in prose. If the code already says it, delete the sentence.
- A claim needs a number or a command. "Faster" is not a claim; `2.4s -> 0.3s` is.
- Read it back as if a colleague sent it to you. If it reads like marketing or an essay,
  rewrite it.

## VERIFY PLAN AND RUN NEW TESTS LOCALLY

**Before committing, ALWAYS:**

1. **Re-read the plan** — check every numbered step and verification item against what was implemented
2. **Run new tests locally** — don't just compile them, actually execute them and verify output
3. **Check for dead code** — search for old function/field names that should have been removed

Writing tests without running them is pointless. Compilation does not equal correctness. A test that passes `--no-run` might fail at runtime due to logic bugs, missing `-T` flags, wrong paths, etc.

**Anti-pattern:** "All 85 unit tests pass" + never ran the integration tests that actually exercise the new code paths.

## A DEFECT CLAIM IS CLOSED BY A RED TEST, NOT BY A FIX

**Whenever anyone — a reviewer, a bot, a teammate, you — says "this is broken", the thing that
closes it is a test that FAILS WITHOUT THE FIX.** Not the fix. Not "verified manually". Not a
green suite after the change, which proves only that the suite never covered it.

Applies to every source of a defect claim, not just review comments: a CI failure, a bug
report, a hunch you had in the shower, a comment you wrote yourself.

**The procedure, in order:**
1. Write the test. Run it against the **unfixed** tree. **Watch it fail.**
2. Apply the fix. Watch it pass.
3. Revert the fix once more and confirm it goes red again — if you skipped step 1, this is
   your last chance to learn the test was vacuous.
4. Only then resolve the thread / close the issue, citing the test by name.

A test written after the fix, never observed failing, is indistinguishable from a test that
cannot fail. This repo has repeatedly found checks that could never fire — a contention
detector matching a truncated `comm`, a leak check whose pattern never matched, a `"VM exited"`
branch logged 0 times across 137 runs, a `grep '^ *FAIL'` blind to nextest's `TRY 1 FAIL`.
Every one of them was green for its whole life.

Enforcement: `scripts/check-review-threads.sh <pr>` fails while any review thread is
unresolved, AND while any thread that *describes* broken behaviour has been resolved without a
`RED-VERIFIED: <test>` reply. CI state cannot tell you whether a finding was answered.

### A GATE MUST FAIL CLOSED — check your dependencies before you trust your verdict

A check that cannot run must **block**, never pass. Passing is a claim, and a tool that could
not evaluate anything has no basis for making it.

This bit immediately, in the gate written to enforce the rule above. `jq` is not in the CI
container. Every `jq` call failed to stderr, the counts came back empty, and the script printed
`verdict: CLEAR ... exit 0` — waving every PR through *precisely because it could not evaluate
them*. Strictly worse than having no gate, because it looks like one:

```
review threads:  total,  unresolved
verdict: CLEAR — every thread resolved, ...
check-review-threads.sh: line 49: jq: command not found
```

Any script that renders a verdict must begin by proving it can:
```bash
for tool in jq gh; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
```
And its tests must run **where CI runs it**, not only on a dev box that happens to have the
tooling. A green unit test on your laptop says nothing about the container.

### HOW TO GET CI LOGS — there is always a way; never report "no logs available"

"The logs aren't available yet" is almost always a tooling mistake, not a fact. Two independent
routes, in order of preference:

**1. Per-job API — works even while the RUN is still in progress.**
```bash
JOB=$(gh api repos/{o}/{r}/commits/<sha>/check-runs \
       --jq '.check_runs[] | select(.conclusion=="failure") | .id' | head -1)
gh api --allow-escape-sequences repos/{o}/{r}/actions/jobs/$JOB/logs > job.log
sed -i 's/\x1b\[[0-9;]*m//g' job.log
```
`--allow-escape-sequences` is **mandatory**: job logs contain ANSI, and without it `gh api`
writes nothing to stdout and puts the reason **only on stderr** — so `> job.log` yields an
empty file from a command that looks like it succeeded. That empty file is what makes people
announce there are no logs.

Note `gh run view --log` refuses while the RUN is in progress even for jobs that finished long
ago. Do not wait for the run; use the per-job API above.

**2. SSH to the self-hosted runner — it has the checkout AND the environment.**
```bash
gh api repos/{o}/{r}/actions/runners --jq '.runners[] | "\(.name)\t\(.status)\t\(.busy)"'
# names are runner-i-<instance-id>; get IPs read-only:
aws ec2 describe-instances --instance-ids i-... \
  --query 'Reservations[].Instances[].{id:InstanceId,pub:PublicIpAddress,arch:Architecture}' --output text
ssh -i ~/.ssh/runner_key ubuntu@<public-ip>
# the CI tree, exactly as the job saw it (merge commit of PR head into base):
cd /opt/actions-runner/_work/fcvm/fcvm/fcvm && sudo git log --oneline -1
```
This is how a `cargo fmt --check` failure was diagnosed in one command after the API route had
been (wrongly) given up on. `_diag/Worker_*.log` is runner METADATA — step output is not there;
either use route 1 or re-run the failing command in that checkout.

**Check `busy` first and leave a busy runner alone** — it is executing someone's job, and a
build you start competes for its cargo cache and disk.

### "It is too big / slow / expensive to test" is almost always false

That excuse is how the worst bugs stay uncovered, because expensive-to-reach paths are exactly
where nobody looks. Find the cheap equivalent:

- **Large files → sparse files.** The FUSE `remap_file_range` u32 truncation
  (`kernel/patches/0001-fuse-add-remap_file_range-support.patch`) only bites above 4 GiB,
  because `fuse_write_out.size` is a `u32` and the client saturates it — the destination inode
  records ~4 GiB and later guest reads come back short. Sounds like it needs a 4 GiB fixture.
  It does not: a sparse file costs no real blocks and on btrfs the reflink is O(1).
  `truncate -s 5G` + FICLONE reproduces it for free.
- **Slow timeouts → inject the signal.** Use the failpoint harness (`make fuzz`, `FAILPOINT`
  specs) instead of waiting out a real timeout.
- **Rare races → make the interleaving deterministic.** A seeded schedule beats hoping.
- **Huge memory → test the arithmetic.** Feed the boundary value (`u32::MAX`, `pid_max`, a
  9-digit `pid_start_time`) to the function directly rather than provisioning the machine that
  would produce it naturally.
- **Multi-hour soak → assert the invariant, not the duration.** If a leak takes 6 hours to be
  visible, count the resource instead of watching the clock.

If after genuinely trying you cannot make it cheap, say so **in the test file**, with what it
would take — never only in a commit message, where the next reader will not find it.

## STACKED PRs BY DEFAULT

**All work goes in stacked PRs.** Each new PR should be based on the previous one, not main.

```
main → PR#55 → PR#56 → PR#57  (correct)
main → PR#55, main → PR#56    (wrong - parallel branches)
```

Only branch directly from main when explicitly starting independent work.

**When base PR merges:** Your branch's merge-base with main shifts automatically. The delta shown in your PR will only be your commits (base PR's commits are now in main). Merge conflicts can arise if main got other commits touching the same files.

**CRITICAL: Verify base update before merging dependent PRs:**
```bash
# After PR #1 merges, WAIT and verify PR #2's base changed to main
gh pr view 2 --json baseRefName
# Must show: {"baseRefName":"main"}
# If it still shows the old branch name, DO NOT MERGE - wait or manually update
```

**Why this matters:** If you merge PR #2 while its base is still the old branch (not main), the commits go into the orphaned branch and never reach main. You'll lose your changes.

**PR description:** Always note `**Stacked on:** <base-branch> (PR #N)` so reviewers understand the dependency.

### A STACKED PR MUST ACTUALLY RUN CI — "no failures" is not "it ran"

**A `pull_request:` trigger's `branches:` filter matches the PR's BASE branch.** So
`branches: [main]` silently skips the whole workflow for every stacked PR — the exact PRs this
section tells you to open. GitHub then shows a check set containing whatever *other* workflows
did fire (here: `safety-check`), with zero failures. It looks green. Nothing ran.

Observed 2026-08-08: #752 (base `kernel-7.0.14`) was merged on a "no failures" reading. Its
head sha had **zero** runs of `lint`, `packaging`, `host`, `host-root`, `container` — the only
non-skipped check was `safety-check`. It carried three rustfmt violations that surfaced the
moment the change reached a PR whose base *was* main.

**Before treating any PR as green, prove the checks EXIST, then that they passed:**
```bash
SHA=$(gh api repos/{o}/{r}/pulls/<N> --jq .head.sha)
gh api repos/{o}/{r}/commits/$SHA/check-runs \
  --jq '.check_runs[] | select(.conclusion != "skipped") | "\(.name)\t\(.conclusion)"' | sort -u
# An expected job missing from this list is a FAILURE to verify, not a pass.
```
Any monitor, gate, or script that renders a verdict from `statusCheckRollup` must assert the
required jobs are **present**. Counting failures and finding none is the fail-open form of the
same bug as `jq: command not found` printing `verdict: CLEAR`, and as a `CodeRabbit  pass` that
means the reviewer never started.

Enforcement: `tests/test_ci_workflow_coverage.rs` fails if `ci.yml` reintroduces a
`branches:`/`branches-ignore:` filter on `pull_request`, or if a gating job leaves that file.

## Sending Email

Use `aws ses send-email --region us-east-1` (recipient must be verified in SES sandbox).

## UNDERSTAND BRANCH CHAINS

**ALWAYS fetch before investigating branches:**
```bash
git fetch origin
```
Branches may already be merged on remote. Don't waste time on stale local state.

**Run before starting work, committing, or opening PRs:**

```bash
git log --oneline --graph --all --decorate | head -120
```

Shows which branch you're on and what it's based on.

**Don't confuse local vs remote:** After rebasing locally, `origin/<branch>` shows the old history until you force-push. They're the same branch at different points in time.

## ALWAYS USE THE MAKEFILE

**Never run raw cargo/podman commands. Use make targets.**

```bash
# CORRECT
make test-root FILTER=sanity
make setup-fcvm
make build

# WRONG - bypasses setup, env vars, correct flags
cargo test ...
sudo cargo test ...
./target/release/fcvm setup
```

If the Makefile is missing a target or broken, **fix the Makefile** - don't work around it.

### Never share `CARGO_TARGET_DIR` across worktrees

The Makefile owns Cargo target routing. It sets `CARGO_TARGET_DIR=target`, and
`cargo-target-link` maps that path to a directory unique to the current worktree. The obsolete
shell-profile export of `/mnt/fcvm-btrfs/cargo-target` was removed; do not restore it or export
any other target directory shared by multiple worktrees. Two worktrees building the same
package can produce the same test-binary path, so a shared target lets Cargo consider a sibling
worktree's binary fresh and run code from the wrong checkout.

Observed 2026-08-08 while verifying two stacked PRs: `cargo test --test
test_ci_workflow_coverage` in worktree A printed a test that exists **only in worktree B** and
silently omitted the one under test:

```
Running .../cargo-target/debug/deps/test_ci_workflow_coverage-113926a94ccc9fad
test gh_existence_probes_do_not_discard_their_error ... ok    <- not on this branch
test result: ok. 4 passed                                     <- summary_fails never ran
```

Red/green verification is worthless under these conditions: "it went red" and "it went green"
may both be reports about code you are not editing. Use the appropriate Make target. Before
believing a result, confirm `readlink -f target` resolves to this worktree's unique target
directory and the `Running .../deps/<binary>` line points beneath it.

## NEVER ROUTE AROUND BUILD PROCESSES

**If a build fails, FIX THE BUILD. Never manually copy files.**

When a kernel, rootfs, or binary doesn't build correctly:
1. Fix the build script
2. Fix the source code
3. Fix the patches

**NEVER:**
- Manually copy files to work around naming issues
- Run build scripts directly instead of through fcvm
- Create symlinks to "fix" path mismatches

If `fcvm setup` produces wrong output, the bug is in fcvm or build.sh. Fix it there.

## Test Helper Functions (tests/common/mod.rs)

| Function | Purpose |
|----------|---------|
| `spawn_fcvm(&args)` | Spawn fcvm process, returns `(child, pid)` |
| `spawn_fcvm_with_logs(&args, name)` | Same + debug log file |
| `poll_health_by_pid(pid, timeout)` | Wait for VM healthy status |
| `poll_health_status_by_pid(pid, expected, timeout)` | Wait for specific health status |
| `poll_health(&child, timeout)` | Wait healthy (checks process exit) |
| `kill_process(pid)` | SIGTERM then SIGKILL if needed |
| `unique_names(prefix)` | Generate unique `(name, clone, snap, serve)` |
| `find_fcvm_binary()` | Locate `./target/release/fcvm` |
| `exec_in_vm(pid, &cmd)` | Run command in VM via exec |
| `exec_in_container(pid, &cmd)` | Run command in container via exec |

## Nested Test Architecture

Tests use `localhost/nested-test` container image built from `Containerfile.nested`.

**Key files:**
- `Containerfile.nested`: Container with fcvm, fc-agent, firecracker-nested, rsync
- `tests/common/mod.rs`: `ensure_nested_image()` auto-builds via podman
- `rootfs-config.toml`: VM rootfs packages (copied into container at `/etc/fcvm/`)

**Package installation locations:**
- Container packages: `Containerfile.nested` apt-get install
- VM rootfs packages: `rootfs-config.toml` [packages] section

Both need rsync for `--disk-dir` to work.

## NO HACKS

**Fix the root cause, not the symptom.** When something fails:
1. Understand WHY it's failing
2. Fix the actual problem
3. Don't hide errors, disable tests, or add workarounds

Examples of hacks to avoid:
- Gating tests behind feature flags to skip failures
- Adding sleeps or retries without understanding the race
- Clearing caches instead of updating tools
- Using `|| true` to ignore errors

## NEVER Parse JSON with Regex

**Always use `jq` to parse JSON.** Never use grep, sed, awk, or string matching on JSON.

```bash
# WRONG
grep '"health_status":"healthy"' output.json

# CORRECT
jq -r '.[] | select(.health_status == "healthy")' output.json
```

## Test Failure Investigation

**Never say "likely" - always find the actual root cause.**

When tests fail in CI or parallel runs:
1. Re-run in isolation to confirm the test itself is correct
2. Root-cause why it failed when run together
3. All tests must pass together
4. If it passes alone but fails in parallel, find and fix the race condition

## Debugging Test Hangs

**When a test hangs, look at what it's ACTUALLY DOING - don't blame "stale processes".**

```bash
# WRONG approach: blindly killing "old" processes
ps aux | grep fcvm   # "I see old processes, they must be blocking!"
sudo pkill -9 fcvm   # "Fixed it!" (No, you didn't debug anything)

# CORRECT approach: understand what the test is doing
ps aux | grep -E "fcvm|script|cat"
# See: script -q -c ./target/release/fcvm exec --pid 1083915 -t -- cat
# The test is running `cat` in TTY mode - it's waiting for input!
# The bug is in the test, not "stale processes"
```

**Common causes of hanging tests:**
- Command waiting for stdin (like `cat` without EOF signal)
- Missing Ctrl+D (0x04) in TTY mode tests
- Blocking reads without timeout
- Deadlocks in async code

**The process list tells you EXACTLY what's happening.** Read it.

## Overview
fcvm is a hypervisor-agnostic VM manager for running Podman containers in lightweight microVMs. It drives two backends behind a pluggable `Hypervisor` trait — Firecracker (default) and Cloud Hypervisor — selected via `--hypervisor firecracker|cloud-hypervisor`. This document tracks implementation findings and decisions.

## Nested Virtualization

fcvm supports running inside another fcvm VM using ARM64 FEAT_NV2.
Recursive nesting (Host → L1 → L2 → ...) is enabled via the `arm64.nv2` kernel boot parameter.

### Requirements

- **Hardware**: ARM64 with FEAT_NV2 (Graviton3+, c7g.metal)
- **Host kernel**: 6.18+ with `kvm-arm.mode=nested` AND DSB patches
- **Nested kernel**: Custom kernel with CONFIG_KVM=y (use `--kernel-profile nested`)

### Host Kernel with DSB Patches

**CRITICAL**: Both host AND guest kernels need DSB patches for cache coherency under NV2.

**Install host kernel**: `make install-host-kernel` (builds kernel, installs to /boot, updates GRUB).
Patches from `kernel/patches/` are applied automatically during the build.

**Current patches** (all apply to both host and guest kernels):
- `nv2-vsock-cache-sync.patch`: DSB SY in `kvm_nested_sync_hwstate()`
- `nv2-vsock-rx-barrier.patch`: DSB SY in `virtio_transport_rx_work()`
- `mmfr4-override.vm.patch`: ID register override for recursive nesting (guest only)

**VM Graceful Shutdown (PSCI)**:
- fc-agent uses `poweroff -f` to trigger PSCI SYSTEM_OFF (function ID 0x84000008)
- KVM forwards this to Firecracker via KVM_EXIT_SYSTEM_EVENT
- NOTE: `halt -f` does NOT trigger PSCI - it just enters a WFI loop without calling PSCI

### How It Works

1. Set `FCVM_NV2=1` environment variable (auto-set when `--kernel-profile nested` is used)
2. fcvm passes `--enable-nv2` to Firecracker, which enables `HAS_EL2` vCPU feature
3. vCPU boots at EL2h in VHE mode (E2H=1) so guest kernel sees HYP mode available
4. EL2 registers are initialized: HCR_EL2, VMPIDR_EL2, VPIDR_EL2
5. Guest kernel initializes KVM: "VHE mode initialized successfully"
6. `arm64.nv2` boot param overrides MMFR4 to advertise NV2 support
7. L1 KVM reports `KVM_CAP_ARM_EL2=1`, enabling recursive L2+ VMs

### Running Nested VMs

```bash
# Build nested kernel (first time only, ~10-20 min)
fcvm setup --kernel-profile nested --build-kernels

# Run outer VM with nested kernel profile
sudo fcvm podman run \
    --name outer \
    --network bridged \
    --kernel-profile nested \
    --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    nginx:alpine

# Inside outer VM, run inner fcvm
fcvm podman run --name inner --network bridged alpine:latest
```

### Key Firecracker Changes

Firecracker fork with NV2 support (configured in kernel profile)

- `HAS_EL2` (bit 7): Enables virtual EL2 for guest in VHE mode
- Boot at EL2h: Guest kernel must see CurrentEL=EL2 on boot
- VHE mode (E2H=1): Required for NV2 support in guest (nVHE mode doesn't support NV2)
- VMPIDR_EL2/VPIDR_EL2: Proper processor IDs for nested guests

### Tests

```bash
make test-root FILTER=kvm
```

- `test_kvm_available_in_vm`: Verifies /dev/kvm works in guest
- `test_nested_run_fcvm_inside_vm`: Full nested virtualization test

### Recursive Nesting: The ID Register Problem (Solved)

**Problem**: L1's KVM initially reported `KVM_CAP_ARM_EL2=0`, blocking L2+ VMs.

**Root cause**: ARM architecture provides no mechanism to virtualize ID registers for virtual EL2.

1. Host KVM stores correct emulated ID values in `kvm->arch.id_regs[]`
2. `HCR_EL2.TID3` controls trapping of ID register reads - but only for **EL1 reads**
3. When guest runs at virtual EL2 (with NV2), ID register reads are EL2-level accesses
4. EL2-level accesses don't trap via TID3 - they read hardware directly
5. Guest sees `MMFR4=0` (hardware), not `MMFR4=NV2_ONLY` (emulated)

**Solution**: Use kernel's ID register override mechanism with `arm64.nv2` boot parameter.

1. Added `arm64.nv2` alias for `id_aa64mmfr4.nv_frac=2` (NV2_ONLY)
2. Changed `FTR_LOWER_SAFE` to `FTR_HIGHER_SAFE` for MMFR4 to allow upward overrides
3. Kernel patch: `kernel/patches/mmfr4-override.patch`

**Why it's safe**: The host KVM *does* provide NV2 emulation - we're just fixing the guest's
view of this capability. We're not faking a feature, we're correcting a visibility issue.

**Verification**:
```
$ dmesg | grep mmfr4
CPU features: SYS_ID_AA64MMFR4_EL1[23:20]: forced to 2

$ check_kvm_caps
KVM_CAP_ARM_EL2 (cap 240) = 1
  -> Nested virtualization IS supported by KVM (VHE mode)
```

### Known NV2 Architectural Limitations

ARM's FEAT_NV2 has fundamental architectural issues acknowledged by Linux kernel maintainers.
These affect memory visibility, register access, and timer emulation under nested virtualization.

**Kernel source citations** (from `torvalds/linux` master branch):

From [`arch/arm64/kvm/nested.c`](https://github.com/torvalds/linux/blob/master/arch/arm64/kvm/nested.c):
> "In yet another example where FEAT_NV2 is fscking broken, accesses to MDSCR_EL1 are redirected to the VNCR despite having an effect at EL2."

> "One of the many architectural bugs in FEAT_NV2 is that the guest hypervisor can write to HCR_EL2 behind our back"

From [`arch/arm64/kvm/arch_timer.c`](https://github.com/torvalds/linux/blob/master/arch/arm64/kvm/arch_timer.c):
> "Paper over NV2 brokenness by publishing the interrupt status bit. This still results in a poor quality of emulation"

> "NV2 badly breaks the timer semantics by redirecting accesses to the EL1 timer state to memory"

**Impact on fcvm**: Under L2 (nested) VMs, vsock packet fragmentation can trigger memory visibility
issues due to double Stage 2 translation (L2 GPA → L1 S2 → L1 HPA → L0 S2 → physical). Large writes
that fragment into multiple vsock packets may see stale/zero data instead of actual content.

**Fix**: The DSB SY kernel patch in `kernel/patches/nv2-vsock-cache-sync.patch` fixes this issue.
The patch adds a full system data synchronization barrier in `kvm_nested_sync_hwstate()` to ensure
L2's writes are visible to L1's reads before returning from the nested guest exit handler.

With the patch applied, FUSE max_write can be unbounded (default). Without the patch, set
`FCVM_FUSE_MAX_WRITE=32768` to limit writes to 32KB as a workaround.

### L2 Cache Coherency Fix (2026-01)

**Problem**: L2 FUSE-over-FUSE corrupted with unbounded max_write (~1MB). After ~3-10MB
transferred, L1 reads all zeros where L2's data should be.

**Error pattern**:
```
STREAM CORRUPTION: zero-length message at count=67 after 10489619 bytes
peek_bytes=128 hex=00 00 00 00 00 00 00 ... (128 bytes of zeros)
```

**Data path**:
1. L2 app writes to FUSE → L2 fc-agent multiplexer → L2 vsock → virtio ring
2. L2 kicks virtio (trap to L1 KVM)
3. L1 Firecracker reads from virtio ring (mmap of guest memory)
4. L1 VolumeServer writes to L1 FUSE → Host FS

**Investigation**:
- Raw vsock works fine (2MB packets, 4480/4480 tests pass)
- Only FUSE-over-FUSE path triggers corruption (many small requests/responses)
- Corruption happens when L1 reads virtio ring and sees stale/zero data

**Root cause**: Under double Stage 2 translation, L2's writes to the virtio ring weren't
visible to L1's mmap reads due to missing cache synchronization at nested guest exit.

**Solution**: Add `dsb(sy)` in `kvm_nested_sync_hwstate()` - a full system data synchronization
barrier that ensures all L2 writes complete and are visible before returning to L1.

```c
// In arch/arm64/kvm/nested.c
dsb(sy);  // Full system barrier - ensures L2 writes visible to L1
```

**Why it works**: The DSB SY barrier forces cache coherency across the entire system, including
the mmap'd guest memory that Firecracker reads. ISH (inner-shareable) barriers weren't sufficient
because the double S2 translation creates a cross-domain cache coherency issue.

**Test results**: With the DSB SY patch, 100MB file copies through FUSE-over-FUSE complete
successfully with unbounded max_write (~1MB packets). Test: `make test-root FILTER=nested_l2_with_large`

### NV2 Snapshot Lifecycle: Timer Coherence + One Restore Path (#630, 2026-06)

The "nested tests are too slow/flaky" disables (Jan 2026) traced to TWO snapshot-lifecycle
bugs, both fixed. Neither was an inherent NV2 limitation:

**1. Timer-domain skew on restore (firecracker fork fix).** Restore replayed CNTVCT/CNTPCT
(KVM adjusts per-timer offsets, which for HAS_EL2 guests live in CNTVOFF_EL2 storage), then
replayed CNTVOFF_EL2 — clobbering the adjustment. The guest's monotonic clock jumped forward
by the host time elapsed since the snapshot, every armed timer CVAL landed in the past, and
KVM's emulated EL1 timers (NV2 traps these) re-fired in a storm (~14k kvm_timer_emulate/s
measured) that starved the vCPUs 25-300x. Fix: firecracker owns the VM-wide counter offset
via `KVM_ARM_SET_COUNTER_OFFSET` — set at boot (zero-based counters), set coherently before
the register replay on restore, and advanced by the pause duration on resume so the guest
clock FREEZES while a VM is paused (fcvm's pre-start snapshot pauses VMs for many seconds).

**2. Event-loop starvation after pause→save→resume (fcvm design fix).** Resuming the VM that
just produced a snapshot intermittently left Firecracker's single device event-loop thread
spinning (94 CPU-seconds measured), starving every virtio queue: `NETDEV WATCHDOG: transmit
queue 0 timed out` in the guest, stalled FUSE-over-vsock, apparent 100x guest slowdowns that
self-heal minutes later. Restored VMs never storm (fresh process, fresh event loop):
create+resume stormed 3/3 under load; restore was clean 12/12. Fix: the snapshot-miss path
CONVERGES on the restore path — create the pre-start snapshot, tear the throwaway VM down,
and relaunch by restoring it. Snapshot hit and miss now run the exact same flow.

**Debugging these**: guest `dd if=/dev/zero of=/dev/null` per-CPU (healthy ≈ 19 GB/s on
Graviton3; storms read 0.07-0.3 GB/s), host `ftrace` on `kvm:kvm_timer_emulate`, per-thread
CPU of the firecracker process (`/proc/<fc>/task/*/stat` field 14+15), and the fork's
env-gated `FC_DEBUG_REG_DUMP` / `FC_DEBUG_REG_DIFF` register instrumentation.

**Historical note — "L2 single vCPU requirement"**: multi-vCPU L2 VMs hitting NETDEV
WATCHDOG (TX queue not serviced) was previously attributed to NV2 cross-vCPU interrupt
issues. That symptom matches bug 2 (host event loop starvation, which always reports the
watchdog on the queue's CPU). Nested launches still use `--cpu 1` as a conservative
default; re-evaluating multi-vCPU L2 is tracked in #632.

## FUSE Performance Tracing

Enable per-operation tracing to diagnose FUSE latency issues (especially in nested VMs).

### Enabling Tracing

Set `FCVM_FUSE_TRACE_RATE=N` to trace every Nth FUSE operation:

```bash
# Trace every 100th request (recommended for benchmarks)
FCVM_FUSE_TRACE_RATE=100 fcvm podman run --name test nginx:alpine

# Trace every request (high overhead, use for debugging specific issues)
FCVM_FUSE_TRACE_RATE=1 fcvm podman run ...
```

The env var is automatically passed to the guest via kernel boot parameters (`fuse_trace_rate=N`).

### Trace Output Format

```
[TRACE     lookup] total=8940µs srv=159µs | fs=149 | to_srv=33 to_cli=1974
[TRACE      fsync] total=70000µs srv=3000µs | fs=2900 | to_srv=? to_cli=?
```

| Field | Meaning |
|-------|---------|
| `total` | End-to-end client round-trip time |
| `srv` | Server-side processing (reliable) |
| `fs` | Filesystem operation time (subset of srv) |
| `to_srv` | Network: client → server (may show `?` if clocks differ) |
| `to_cli` | Network: server → client (may show `?` if clocks differ) |

### L2 Performance Expectations

Based on FUSE-over-FUSE architecture:

| Operation | Expected L2/L1 Ratio | Notes |
|-----------|---------------------|-------|
| `stat`/metadata | ~2x | One extra FUSE layer |
| Async writes | ~3x | Data transfer overhead |
| Sync writes (fsync) | ~8-10x | fsync propagates synchronously through layers |

The fsync amplification occurs because each L2 fsync must wait for L1's fsync to complete,
which itself waits for the host disk sync. This is fundamental to FUSE-over-FUSE durability.

### Related Configuration

```bash
# Reduce FUSE readers for nested VMs (saves memory)
FCVM_FUSE_READERS=8 fcvm podman run ...  # Default: 64 readers × 8MB stack = 512MB
```

## Quick Reference

### Shell Scripts to /tmp

**Write complex shell logic to /tmp instead of fighting escaping issues:**
```bash
# BAD - escaping nightmare
for dir in ...; do count=$(grep ... | wc -l); done

# GOOD - write to file, execute
cat > /tmp/script.sh << 'EOF'
for dir in */; do
  count=$(grep -c pattern "$dir"/*.rs)
  echo "$dir: $count"
done
EOF
chmod +x /tmp/script.sh && /tmp/script.sh
```

### Streaming Test Output

**Use `STREAM=1` to see test output in real-time:**
```bash
make test-root FILTER=sanity STREAM=1              # Host tests with streaming
make container-test-root FILTER=sanity STREAM=1   # Container tests with streaming
```

Without `STREAM=1`, nextest captures output and only shows it after tests complete (better for parallel runs).

**Log levels:** Tests run with `fcvm=debug` by default (FUSE spam suppressed). Override with:
```bash
RUST_LOG=debug make test-root  # Full debug (slow, 18x more output)
```

### Debug Logs

**All tests automatically capture debug-level logs to files.**

How it works:
- `spawn_fcvm()` and `spawn_fcvm_with_logs()` always create a log file
- fcvm runs with `RUST_LOG=debug` for full debug output
- Console shows INFO/WARN/ERROR only (DEBUG filtered out)
- Log file has everything including DEBUG/TRACE
- Path printed at end: `📋 Debug log: /tmp/fcvm-test-logs/{name}-{timestamp}.log`
- CI uploads `/tmp/fcvm-test-logs/` as artifacts (7 day retention)
- Tests add `--setup` flag automatically, so missing initrd auto-creates

### Common Commands
```bash
# Build
make build        # Build fcvm + fc-agent
make build-fc-mock  # Build fc-mock (Firecracker mock for container mode)
make test         # Run fuse-pipe tests
make setup-fcvm   # Download kernel and create rootfs

# Run a VM (requires setup first, or use --setup flag)
sudo fcvm podman run --name my-vm --network bridged nginx:alpine

# With custom command (docker-style trailing args)
sudo fcvm podman run --name my-vm --network bridged alpine:latest echo "hello"

# Or using --cmd flag
sudo fcvm podman run --name my-vm --network bridged --cmd "echo hello" alpine:latest

# Or run with auto-setup (first run takes 5-10 minutes)
sudo fcvm podman run --name my-vm --network bridged --setup nginx:alpine

# With extra root disk space (default: 10G free)
sudo fcvm podman run --name my-vm --rootfs-size 50G nginx:alpine

# Snapshot workflow
fcvm snapshot create --pid <vm_pid> --tag my-snapshot
fcvm snapshot serve my-snapshot      # Start UFFD server (prints serve PID)
fcvm snapshot run --pid <serve_pid> --name clone1
```

### Local Test Containers

**Build test logic into a container, run with fcvm.** No weird feature flags or binary copying.

```bash
# Build with localhost/ prefix
podman build -t localhost/mytest -f Containerfile.mytest .

# Run with fcvm (exports via skopeo automatically)
sudo fcvm podman run --name test --network bridged \
    --map /mnt/fcvm-btrfs/test-data:/data \
    localhost/mytest
```

See `Containerfile.libfuse-remap` and `Containerfile.pjdfstest` for examples.

### Manual E2E Testing with Claude Code

**CRITICAL: VM commands BLOCK the terminal.** You MUST use Claude's `run_in_background: true` feature.

**PREFER NON-ROOT TESTING**: Run tests without sudo when possible. Rootless networking mode (`--network rootless`, the default) doesn't require sudo. Only use `sudo` for:
- `--network bridged` tests
- Operations that explicitly need root (iptables, privileged containers)

The ubuntu user has KVM access (`kvm` group), so `fcvm podman run` works without sudo in rootless mode.

```bash
# PREFERRED - Rootless mode (no sudo needed, use run_in_background: true)
./target/release/fcvm podman run --name test alpine:latest 2>&1 | tee /tmp/vm.log
# Defaults to --network rootless
# Get PID from state and use exec:
ls -t /mnt/fcvm-btrfs/state/*.json | head -1 | xargs cat | jq -r '.pid'
./target/release/fcvm exec --pid <PID> -- hostname

# ONLY WHEN NEEDED - Bridged mode (requires sudo)
sudo ./target/release/fcvm podman run --name test --network bridged nginx:alpine 2>&1 | tee /tmp/vm.log
# Then sleep and check logs:
sleep 30
grep healthy /tmp/vm.log
# Get PID from state and use exec:
sudo ls -t /mnt/fcvm-btrfs/state/*.json | head -1 | xargs sudo cat | jq -r '.pid'
sudo ./target/release/fcvm exec --pid <PID> -- curl -s ifconfig.me
```

**Testing egress connectivity:**
```bash
# VM-level egress (runs in guest OS)
fcvm exec --pid <PID> -- curl -s --max-time 10 ifconfig.me

# Container-level egress (runs inside the container)
fcvm exec --pid <PID> -c -- wget -q -O - --timeout=10 http://ifconfig.me
```

### Debugging Network Issues with Quick VMs

When debugging network issues (connectivity, DNS, routing), spawn quick one-off VMs with inline commands:

```bash
# Test basic connectivity
./target/release/fcvm podman run --name net-test-$(date +%s) --privileged alpine:latest sh -c "
echo '=== Network config ==='
ip addr show eth0
ip route
cat /etc/resolv.conf
echo ''
echo '=== Test gateway ping ==='
ping -c 2 -W 3 10.0.2.2 || echo 'gateway ping failed'
echo ''
echo '=== Test DNS ==='
nslookup example.com || echo 'DNS failed'
echo ''
echo '=== Test external ==='
wget -q -O - --timeout=10 http://ifconfig.me || echo 'external failed'
" 2>&1 &
sleep 60  # Wait for VM to boot and run commands
```

**Debugging technique:**
1. Run background (`&`) so terminal stays available
2. Sleep to let VM boot (~30-60s)
3. Check stdout for results
4. Use `--privileged` for ping (requires raw sockets)
5. Test incrementally: gateway → DNS → external

**Inspecting namespace state for running VM:**
```bash
# Get holder PID from state file
HOLDER_PID=$(cat /mnt/fcvm-btrfs/state/*.json | jq -r '.holder_pid')
# Check namespace network config
sudo nsenter --net=/proc/$HOLDER_PID/ns/net ip addr
sudo nsenter --net=/proc/$HOLDER_PID/ns/net bridge link  # Show bridge ports
sudo nsenter --net=/proc/$HOLDER_PID/ns/net ip route
```

### Code Philosophy

**NO LEGACY/BACKWARD COMPATIBILITY.** This applies to everything: code, Makefile, documentation.

- When we change an API, we update all callers
- No deprecated functions, no compatibility shims, no `_old` suffixes
- No legacy Makefile targets or aliases
- No "keep this for backwards compatibility" comments
- Clean breaks only - delete the old thing entirely

Exception: For **forked libraries** (like fuse-backend-rs), we maintain compatibility with upstream to enable merging upstream changes.

### File Operations

**Always use `git mv` when renaming files.** This preserves git history.

```bash
# CORRECT - preserves history
git mv old_name.rs new_name.rs

# WRONG - loses history
mv old_name.rs new_name.rs
```

### Development Workflow (PR-Based)

**IMPORTANT: Use `/pr-workflow` skill for ALL PR operations.** This includes:
- Creating, checking, or merging PRs (`gh pr create`, `gh pr checks`, `gh pr merge`)
- Checking CI status
- Fixing lint/clippy/format errors
- Running `cargo fmt` or `cargo clippy`

**NEVER APPLY SKIP CONDITIONS FROM AUTO-FIX PRs.** When CI creates auto-fix PRs:
- If the fix adds `#[ignore]`, early returns, or weakened assertions → CLOSE IT
- Find and apply the ACTUAL FIX that makes tests pass
- Tests must PASS, not be SKIPPED

**Main branch is protected. All changes MUST go through pull requests.**

#### Creating a PR

**TEST LOCALLY BEFORE PUSHING.** CI is for validation, not discovery.

#### Quick Reference

| Action | Command |
|--------|---------|
| Create branch | `git checkout -b branch-name` |
| **Test locally first** | `make lint && make test-root FILTER=<relevant>` |
| Push & create PR | `git push -u origin branch-name && gh pr create --fill` |
| Check CI | `gh pr checks <pr-number>` |
| Merge PR | `gh pr merge <pr-number> --merge --delete-branch` |
| List my PRs | `gh pr list --author @me` |

**Stacking PRs:** create a branch chain (not parallel branches):
```bash
git checkout -b feature-a && git push -u origin feature-a
gh pr create --base main

git checkout -b feature-b && git push -u origin feature-b
gh pr create --base feature-a

gh pr list --json number,headRefName,baseRefName
```
Merge in order (#1 first, then #2). **Never use `--delete-branch` on the base PR**: it closes dependent PRs. Merge without delete, run `gh pr edit <dep> --base main`, then delete branch.
For the full merge procedure and safety checks, see `.claude/skills/pr-workflow/SKILL.md`.

**CRITICAL: Maintain Stack Coherence.** PR #2's branch must actually be based on PR #1's branch, not just have GitHub base set.
```bash
git log --oneline origin/main..feature-b
```

If PR #2's branch is based on main instead of feature-a, tests will fail because PR #2 won't have PR #1's changes. Fix with:
```bash
git checkout -B feature-b origin/feature-a
git cherry-pick <pr2-commit>
git push origin feature-b --force
```

**One PR per concern:** Unrelated changes get separate PRs.

### Claude Review Workflow

PRs trigger an automated Claude review via GitHub Actions. After pushing:

```bash
# Wait for review check to complete
gh pr checks <pr-number>
# Look for: review  pass  4m13s  ...

# Read review comments
gh pr view <pr-number> --json comments --jq '.comments[] | .body'
```

If review finds critical issues, it may auto-create a fix PR. Cherry-pick the fix:
```bash
git fetch origin
git cherry-pick <fix-commit>
git push
gh pr close <fix-pr-number>  # Close the auto-generated PR
```

**MANDATORY before merging any PR:** Read all review comments first — and note that
`--json comments` returns **only issue comments**. Inline review comments, which is where
CodeRabbit and Codex put their actual findings, live on a different endpoint and are invisible
to that query:
```bash
gh pr view <pr-number> --json comments --jq '.comments[] | .body'   # top-level only
gh api --paginate repos/{owner}/{repo}/pulls/<pr-number>/comments \
  --jq '.[] | "=== \(.user.login) \(.path):\(.line // .original_line)\n\(.body)\n"'
```
Two things that look like details and are not:

- **`--paginate` is mandatory.** Without it you get the first page only, so a PR that has
  accumulated findings over several review rounds silently reports a subset — and the audit
  that was supposed to catch hidden findings becomes one.
- **Print `.body` whole.** A `[0:200]` preview drops the scenario, the evidence, and the
  suggested fix — the parts you need in order to decide. A truncated finding is not a finding
  you have read.

On 2026-08-08 a PR carried **four unread inline findings, two of them Major**, while the check
rendered `CodeRabbit  pass`. One was a slice index that would panic inside the fault-handler
task and hang the guest. Reading only the top-level comments would have merged all four.

**A green `CodeRabbit` check does NOT mean CodeRabbit reviewed anything.** When it hits its
rate limit it posts *"Review limit reached ... we couldn't start this review"* and the check
still renders as `CodeRabbit  pass`. "The reviewer never ran" is indistinguishable from "the
reviewer approved" — the same class of bug as a contention detector that can never fire, or a
leak check whose pattern can never match. Prove the review happened before counting it:
```bash
gh pr view <pr-number> --json comments \
  --jq '.comments[] | select(.author.login=="coderabbitai") | .body' | head -5
# "Review limit reached" / "next review in NN minutes" => NOT reviewed. Re-run before merging.
```

**GitHub re-anchors still-open review comments onto the current head.** After you push a fix,
an *old* comment's `commit_id` and `line` both change to match the new HEAD — so a finding you
already fixed reappears looking brand new, at a shifted line number. Observed live: four
comments created at `09:01:36Z` against `23558456` re-anchored onto the fix commit, with
`prefetch.rs:179 → 196` and `server.rs:1975 → 1991`, while the comment count never changed.

**`isResolved` is the ONLY field that means resolved.** Age does not:
```bash
gh api graphql -f query='{repository(owner:"O",name:"R"){pullRequest(number:N){
  reviewThreads(first:100){nodes{isResolved comments(first:100){nodes{author{login} path line body}}}}}}}' \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved==false)'
```
An earlier version of this file said "`created_at` BEFORE your fix commit ⇒ already addressed",
and that rule is **unsound** — it was wrong here for months of nothing and then wrong in
practice within a day. When one commit fixes SOME of several findings, every older comment
still predates it, **including the ones nobody fixed**, so the rule silently reclassifies
unfixed blockers as handled. `original_commit_id` + `created_at` can tell you a comment is OLD
or RE-ANCHORED; neither can tell you its concern was addressed. Only a human reading the
thread, or `isResolved`, can.

Use age for one thing only: deciding whether a finding needs re-reading after a push, never
whether it needs fixing.

**A CLOSED PR reports CI results for a commit you are no longer on.** GitHub does not advance
a closed PR's head, and `pull_request` events do not fire for one — so `git push` updates
`refs/heads/<branch>` and creates **zero check-runs**, while `gh pr checks` keeps serving the
*previous* commit's results. Same shape as the above: "CI never ran" is indistinguishable from
"CI passed", and it survives a fetch, a re-push, and a `--force`. Anyone can close a PR out
from under you (a dedupe, a bot, a stale-branch sweep), so check PR state before trusting its
checks, and bind results to a SHA rather than to the PR:
```bash
gh pr view <pr> --json state,headRefOid --jq '"\(.state) head=\(.headRefOid)"'
git rev-parse HEAD   # must equal headRefOid; if it does not, the checks are for other code
gh api repos/OWNER/REPO/commits/$(git rev-parse HEAD)/check-runs --jq '.check_runs | length'
# 0 => nothing ran for this commit. `gh pr reopen <pr>` resyncs the head and triggers CI.
```
**Reopening is not guaranteed to start CI.** It fires a `reopened` event, which a workflow
receives only if its `pull_request` trigger omits `types` (or lists `reopened`) — but even
then `paths`/`paths-ignore` still applies, so a PR whose every changed file matches an
ignored pattern gets a `reopened` event and **still runs nothing**. Docs-only and
bench-only PRs land in exactly that hole. Check, and fall back to an explicit dispatch:
```bash
gh api repos/OWNER/REPO/commits/$(git rev-parse HEAD)/check-runs --jq '.check_runs | length'
# still 0 after reopen => the workflow filtered this PR out, not a sync problem
gh workflow run ci.yml --ref "$(git branch --show-current)"
```
Do not read "0 checks" as "CI passed"; it means nothing ran, which is the same
green-by-absence trap as a reviewer that never started.

### GitHub Actions Workflow Security (claude.yml)

Jobs run with secrets, so editing `.github/workflows/claude.yml` is security-critical. Rules
(Anthropic `claude-code-action/docs/security.md` + GitHub "pwn requests"); re-run local codex
until SAFE:
- `pull_request` from forks has no secrets; `issue_comment`/`pull_request_target`/`workflow_run` do.
- Never check out/build untrusted PR head in a secret job (postinstall/build.rs → exfil).
- Gate on PR-author `author_association`, not the commenter (commit email is spoofable).
- `workflow_run`: require `head_repository.full_name == github.repository`, not branch name.
- Never interpolate `${{ github.event.* }}` into `run:` (injection) — pass via `env:`.
- `issue_comment` fires for issues too; gate on `github.event.issue.pull_request`.
- Centralize into one `eligible` allowlist every job gates on.

### PR Descriptions: Show, Don't Tell

**CRITICAL: Review commits in THIS branch before writing PR description.**

For stacked PRs (branches of branches), only describe commits in YOUR branch:
```bash
# First: identify your base branch
gh pr view --json baseRefName   # Shows what branch this PR targets

# Then: review only YOUR commits (not the whole stack)
git log --oneline origin/<base-branch>..HEAD   # Commits in THIS branch only
git log --oneline origin/main..HEAD            # Only if PR targets main directly
```

**Anti-pattern:** On stacked PRs, reviewing `main..HEAD` includes parent commits and causes incorrect claims.

**Include test evidence.** Actual output, not "tested and works."

Simple PR:
```markdown
## Fix cargo fmt scope
Changed to only check workspace packages.

Tested: cargo fmt -p fcvm -p fuse-pipe --check  # passes
```

Complex PR (kernel patches, workarounds, architectural changes):
```markdown
One-line description of what this enables.

## The Problem
- What was broken
- Root cause analysis

## The Solution
What changed and why this approach over alternatives.

## Test Results
$ actual-command-run
actual output
```

### Commit Messages

**Include what changed, why, and test evidence.**

```
Remove obsolete require_non_root guard function

The function was a no-op kept for "API compatibility" - exactly what
our NO LEGACY policy prohibits. Removed function and all 12 call sites.

Tested: make test-root FILTER=sanity (both rootless and bridged pass)
```

### JSON Parsing

**NEVER parse JSON with string matching.** Always use proper deserialization.

```rust
// BAD - Fragile, breaks with formatting changes
if stdout.contains("\"health_status\":\"healthy\"") { ... }

// GOOD - Use serde
#[derive(Deserialize)]
struct VmState { health_status: String }

let vms: Vec<VmState> = serde_json::from_str(&stdout)?;
if vms.first().map(|v| v.health_status == "healthy").unwrap_or(false) { ... }
```

### Test Failure Philosophy

Test failures are bugs. Do not dismiss them as "resource contention", "timing issues", "flaky tests", or "works on my machine".

For failures under load/parallel: use logs and timestamps, find the race/concurrency bug, fix the code, and add regression coverage.

### POSIX Compliance Testing

**fuse-pipe must pass pjdfstest** - the POSIX filesystem test suite.

When a POSIX test fails:
1. **Understand the POSIX requirement** - What behavior does the spec require?
2. **Check kernel vs userspace** - FUSE operations go through the kernel, which handles inode lifecycle. Unit tests calling PassthroughFs directly bypass this.
3. **Use integration tests for complex behavior** - Hardlinks, permissions, and refcounting require the full FUSE stack (kernel manages inodes).
4. **Unit tests for simple operations** - Single file create/read/write can be tested directly.

**Key FUSE concepts:**
- Kernel maintains `nlookup` (lookup count) for inodes
- `release()` closes file handles, does NOT decrement nlookup
- `forget()` decrements nlookup; inode removed when count reaches zero
- Hardlinks work because kernel resolves paths to inodes before calling LINK

**If a unit test works locally but fails in CI:** Add diagnostics to understand the exact failure. Don't assume - investigate filesystem type, inode tracking, and timing.

### Race Condition Debugging Protocol

**Show, don't tell. We have extensive logs - it's NEVER a guess.**

1. **NEVER "fix" with timing changes** (timeouts, sleeps, reducing parallelism)

2. **ALWAYS find the smoking gun in logs** - compare failing vs passing timestamps

3. **Real example**: Firecracker crashed in parallel tests. Logs showed: failing test took 122s to export image (lock contention), then VM crashed 24ms after spawn. Passing test took 103s. **Root cause:** thundering herd after podman lock. **Fix:** content-addressable image cache.

4. **The mantra:** What do timestamps show? What's different between failing and passing? The logs ALWAYS have the answer.

5. **Real example (cross-VM cache race, #677):** De-serialized nested tests (max-threads>1)
   failed. The lazy conclusion was "nested KVM can't take concurrency (#660)." It was not —
   the smoking gun was in the per-VM logs (`/tmp/nested-l2-*.log`), and the technique that
   caught it is reusable:

   - **Read the per-VM debug logs, not the nextest summary.** Two concurrent chains showed,
     at the same second: chain A `resize2fs: Inode checksum does not match inode` and chain B
     `renaming storage image to final path: No such file or directory (os error 2)` — both on
     the SAME content-addressed path `image-cache/<digest>.storage-v2.img.tmp`. Same file, two
     writers = race, full stop.
   - **`e2fsck -fn` the *cached artifact* to prove the race POISONED the cache.** The
     already-cached `.storage-v2.img` failed checksum ("Filesystem still has errors"). A race
     that persists a corrupt content-addressed file silently breaks every *later* run too
     (including serialized ones) — so the symptom and the cause can be in different test runs.
   - **Know when a single host CAN'T reproduce it.** The per-digest `flock` works fine within
     one kernel, so host-only concurrent processes serialize and never collide. The race only
     appears across VM boundaries: nested tests `--map` the host `/mnt/fcvm-btrfs` into every
     L1, and `flock()` over a fuse-pipe mount that negotiates no `FUSE_FLOCK_LOCKS` is granted
     LOCALLY per guest kernel — it never reaches the shared host backing store. Faithful repro =
     clear the digest's cache entry (cold cache) + run two nested tests concurrently.
   - **Fix at the right altitude:** don't try to make `flock` work over FUSE — make the writes
     not need a lock. Unique-per-builder temp names (`uuid`, NOT pid — separate PID namespaces
     reuse numbers) + atomic rename to the content-addressed final (identical bytes → the
     rename race is idempotent). See `src/commands/podman/{image.rs,mod.rs}`.

   **Generalizable rule:** any file under a `--map`'d / FUSE-shared directory written by more
   than one VM must be made safe WITHOUT relying on `flock`/`fcntl` locks — those don't cross
   the fuse-pipe boundary. Use content-addressing + unique temp + atomic rename.

### NO TEST HEDGES

**Test assertions must be DEFINITIVE.** A test either PASSES or FAILS - no middle ground.

**NEVER write hedges like:**
- "NOTE: this may not work (known limitation)"
- "We log the result but don't fail the test for now"
- "skip this assertion for now"
- "this is expected to fail sometimes"

**If a feature should work:**
- Write an assertion that FAILS if it doesn't work
- Fix the bug so the assertion passes
- If you can't fix it, file an issue and mark the test `#[ignore]` with a link

**Example of UNACCEPTABLE test code:**
```rust
// BAD - This hides bugs!
if !localhost_works {
    println!("NOTE: localhost port forwarding not working (known limitation)");
}
// BAD - Test "passes" even when feature is broken
```

**Example of CORRECT test code:**
```rust
// GOOD - This catches bugs!
assert!(localhost_works, "Localhost port forwarding should work (requires route_localnet)");
// GOOD - Test fails if feature is broken
```

### Parallel Test Isolation

**Tests MUST work when run in parallel.** Resource conflicts are bugs, not excuses.

**Test feature flags:**
- `#[cfg(feature = "privileged-tests")]`: Tests requiring sudo (iptables, root podman storage)
- No feature flag: Unprivileged tests run by default
- Features are compile-time gates - tests won't exist unless the feature is enabled
- Use `FILTER=` to further filter by name pattern: `make test-root FILTER=exec`
- For multiple tests or regex: `make test-root FILTER="-E 'test(/pattern1|pattern2/)'" STREAM=1`
- FILTER is a nextest substring match on test function name, NOT file name. Use `-E` for expressions.

**Common parallel test pitfalls and fixes:**

1. **Unique resource names**: Use `common::unique_names()` helper to generate timestamp+counter-based names
   ```rust
   let (baseline, clone, snapshot, serve) = common::unique_names("mytest");
   // Returns: mytest-base-12345-0, mytest-clone-12345-0, etc.
   ```

2. **Port forwarding**: Both networking modes use unique IPs, so same port works
   ```rust
   // BRIDGED: DNAT scoped to veth IP (172.30.x.y) - same port works across VMs
   "--publish", "8080:80"  // Test curls veth's host_ip:8080

   // ROOTLESS: each VM gets unique loopback IP (127.x.y.z) - same port works
   "--publish", "8080:80"  // Test curls loopback_ip:8080
   ```
   - Tests must curl the VM's assigned IP (veth host_ip or loopback_ip), not localhost
   - Get the IP from VM state: `config.network.host_ip` (bridged) or `config.network.loopback_ip` (rootless)

3. **Disk cleanup**: VM data directories are cleaned up on exit
   - `podman.rs` and `snapshot.rs` both delete `data_dir` on VM exit
   - Prevents disk from filling up with leftover VM directories

4. **State file cleanup**: State files are deleted when VMs exit
   - Prevents stale state from affecting IP allocation

5. **Unique ports/directories**: Tests must not share ports or temp directories
   - Use `std::process::id() % 1000` offset for ports
   - Use test name suffix for directories (e.g., `/tmp/scripts-{test_name}/`)
   - Test owns lifetime of any services it starts (kill at end)

**If tests fail in parallel but pass alone:**
- It's a resource isolation bug - FIX IT
- Check for shared state (files, ports, IPs, network namespaces)
- Add unique naming or proper cleanup

### Build and Test Rules

Do not run `sudo cargo`. Use Makefile targets so cargo runs as your user and test binaries run via `CARGO_TARGET_*_RUNNER='sudo -E'`; otherwise `target/` becomes root-owned and breaks builds.

### PROCESS TEARDOWN IS PER-HOP. EVERY HOP.

**`PR_SET_PDEATHSIG` only kills a child whose OWN pdeathsig is set. One unprotected hop
orphans the entire subtree below it.** The chain that must hold, end to end:

```
cargo-nextest (uid 1000) → sudo (ruid 1000) → test binary (uid 0) → fcvm ─┬→ firecracker
                           ^ setpriv --pdeathsig  ^ set_test_pdeathsig     ├→ holder
                                                                           └→ pasta (rootless)
                                                                              ^ see per-hop list below
```

- `scripts/root-test-runner.sh` (`ROOT_TEST_RUNNER` in the Makefile) covers the **sudo hop**.
  Never reduce the privileged runner back to a bare `sudo -E env PATH=...`.
- `tests/common/mod.rs::set_test_pdeathsig{,_std}` covers **every long-lived fcvm spawn** in
  tests. `spawn_fcvm*` applies it; a hand-rolled `Command::new(&fcvm_path)…spawn()` must too.
- `src/utils.rs::install_namespace_pre_exec` covers **every VMM spawn** (Firecracker, Cloud
  Hypervisor, and the Layer-2 setup VM in `src/setup/rootfs.rs`). It must stay the LAST
  `pre_exec` — `setns(CLONE_NEWUSER)` zeroes `pdeath_signal`, so setting it earlier is lost.
- `src/commands/common.rs::spawn_namespace_holder` covers the **holder hop** (rootless mode).
  The holder's pdeathsig is armed in its `pre_exec`, before UID/GID mappings are written.
- `src/network/pasta.rs` (in `start_pasta`) covers the **pasta hop** (rootless mode). Must be
  the last `pre_exec`; includes a `getppid` re-check to close the fork/exec race window.

Regression tests: `test_sigkill_reaps_rootless_vm_tree` asserts all three children (firecracker,
holder, pasta) die when fcvm is SIGKILL'd. `test_root_test_runner_reaps_vm_when_sudo_is_killed`
covers the sudo hop specifically.

**Why a privilege boundary is special:** the kernel refuses a signal from uid 1000 to a uid-0
process, and `killpg` still returns success when it managed to signal *any* member — so
nextest's slow-timeout kill fails **silently** against a `sudo`'d test binary. SIGKILL cannot
be caught, so `sudo` never forwards it either. On 2026-08-06 that left ~498 `firecracker`
processes alive on two ARM runners (load average 523, 103 tasks in D-state, 420 zombies) for
21 hours, and the AWS lease logic kept renewing the dead runners because GitHub still reported
them busy. Regression test: `test_root_test_runner_reaps_vm_when_sudo_is_killed`
(`tests/test_signal_cleanup.rs`). Defense in depth (NOT a substitute): `timeout-minutes` on
every self-hosted job plus the `scripts/ci-stray-vm-guard.sh` pre/post steps in `ci.yml`.

**Prefer kernel-enforced reaping over cleanup code.** A `Drop` impl, a signal handler, or an
`always()` cleanup step does not run when the process is SIGKILLed — which is exactly the case
that leaked. If teardown matters, it must survive SIGKILL.

### Container Build Rules

**Container builds work naturally with layer caching.** No workarounds needed.

- Podman caches layers based on Containerfile content
- When you modify a line, that layer and all subsequent layers rebuild automatically
- Just run `make container-build-root` and let caching work
- NEVER use `--no-cache` or add dummy comments to invalidate cache

**Symlinks for sudo access**: The Containerfile creates symlinks in `/usr/local/bin/` so that `sudo cargo` works (sudo uses secure_path which includes `/usr/local/bin`). This matches how the host is configured.

The `fuse-pipe/Cargo.toml` uses a local path dependency:
```toml
fuse-backend-rs = { path = "../../fuse-backend-rs", ... }
```

This ensures changes to fuse-backend-rs are immediately available without git commits.

### Container KVM Access (Rootless Podman)

`--device /dev/kvm` fails silently in rootless podman (ignores group membership). Use `-v` bind mount with `--group-add keep-groups` instead. See Makefile `CONTAINER_RUN` and [podman#16701](https://github.com/containers/podman/issues/16701).

### Monitoring Long-Running Tests

**Max 30 second sleeps** when waiting for results. Provide play-by-play updates as tests run.

### Preserving Logs from Failed Tests

Always include branch name in tee log filenames to avoid overwrites across branches/worktrees.

```bash
BRANCH=$(git branch --show-current)

# Standard run
make test-root 2>&1 | tee /tmp/test-${BRANCH}-root.log

# Filtered run
make test-root FILTER=<name> 2>&1 | tee /tmp/test-${BRANCH}-root-<name>.log
```

If a run fails, archive the log with test name + timestamp before re-running:

```bash
cp /tmp/test-${BRANCH}-root.log /tmp/fcvm-failed-${BRANCH}-root-test_exec_rootless-$(date +%Y%m%d-%H%M%S).log

# Continue with a fresh log file
make test-root 2>&1 | tee /tmp/test-${BRANCH}-root-run2.log
```

Optional automation:
```bash
if grep -q "FAIL\|TIMEOUT" /tmp/test-${BRANCH}-root.log; then
  cp /tmp/test-${BRANCH}-root.log /tmp/fcvm-failed-${BRANCH}-root-$(date +%Y%m%d-%H%M%S).log
fi
```

### Debugging fuse-pipe Tests

**ALWAYS run tests with debug logging enabled when debugging issues:**

```bash
# Full debug
RUST_LOG=debug make test-root FILTER=permission STREAM=1

# Component-focused
RUST_LOG="passthrough=debug,fuse_pipe=debug" make test-root FILTER=permission STREAM=1
RUST_LOG="fuse_backend_rs=debug" make test-root FILTER=permission STREAM=1
```

**Tracing targets:**
- `passthrough` - fuse-pipe passthrough operations
- `fuse_pipe` - fuse-pipe client/server
- `fuse_backend_rs` - fuse-backend-rs internals (uses `log` crate, bridged via tracing-log)

### Debugging Protocol Issues (ftruncate example)

When a FUSE operation fails unexpectedly, trace the full path from kernel to fuse-backend-rs:

1. **Add debug logging to passthrough handler** to see what parameters arrive:
   ```rust
   debug!(target: "passthrough", "setattr inode={} handle={:?} valid={:?}", inode, handle, valid);
   ```

2. **Run test with logging** to see the actual values:
   ```bash
   RUST_LOG='passthrough=debug' make test-root FILTER=permission STREAM=1
   ```

3. **Check if kernel sends parameter but protocol drops it** - e.g., `handle=None` when it should be `Some(1)` means the protocol layer isn't passing it through.

4. **Trace the path**: kernel → fuser → fuse-pipe client (`_fh` unused?) → protocol message → handler → passthrough → fuse-backend-rs

This pattern found the ftruncate bug: kernel sends `FATTR_FH` with file handle, but fuse-pipe's `VolumeRequest::Setattr` didn't have an `fh` field.

### Kernel Tracing (Ftrace)

Use `common::Ftrace` for KVM debugging:

```rust
let tracer = common::Ftrace::new()?;
tracer.enable_events(common::Ftrace::EVENTS_PSCI)?;
tracer.start()?;
// ... run VM ...
tracer.stop()?;
println!("{}", tracer.read_grep("kvm_exit", 50)?);
```

**Event sets:** `EVENTS_PSCI` (low noise), `EVENTS_INTERRUPTS`, `EVENTS_DETAILED` (noisy)

## CI and Testing

**See README.md for test categories, CI summary, and Makefile targets.** Run `make help` for full list.

Key points for development:
- CI runs on every PR: Host (bare metal) + Container (privileged)
- Manual trigger: `gh workflow run ci.yml --ref <branch>`
- Get in-progress logs: `gh api repos/OWNER/REPO/actions/runs/RUN_ID/jobs`

## PID-Based Process Management

**Core Principle:** All fcvm processes store their own PID (via `std::process::id()`), not child process PIDs.

### Process Types

1. **VM processes** (`fcvm podman run`) - `process_type`: "vm", health check: HTTP to guest
2. **Serve processes** (`fcvm snapshot serve`) - `process_type`: "serve", health check: process existence
3. **Clone processes** (`fcvm snapshot run`) - `process_type`: "clone", references parent via `serve_pid`

### State Management

```rust
pub struct VmConfig {
    pub snapshot_name: Option<String>,  // Which snapshot
    pub process_type: Option<String>,   // "vm" | "serve" | "clone"
    pub serve_pid: Option<u32>,         // For clones: parent serve PID
}

pub struct VmState {
    pub pid: Option<u32>,            // fcvm process PID (from std::process::id())
    pub pid_start_time: Option<u64>, // Process start time (clock ticks since boot)
}
```

**Concurrency model** — state files use per-VM flock for all mutations:
- `save_state()`: Overwrites entire file. Used for **initial creation** only (single writer).
  Records `pid_start_time` automatically from `/proc/<pid>/stat` field 22.
- `update_state(vm_id, |state| { ... })`: **Locked read-modify-write.** Loads current
  on-disk state, applies the closure, writes back. Used by all post-startup writers
  (health monitor, snapshot name recording) so concurrent updates are not clobbered.
  Returns `Ok(None)` if the state file was deleted (no-op, cannot resurrect).
- `delete_state()`: Acquires the per-VM flock, deletes the state file, then removes
  the lock file. A concurrent `update_state` either completes before deletion or
  finds no state file afterwards and becomes a no-op.
- `update_health_status()`: Thin wrapper around `update_state` that sets
  `health_status` and `exit_code`.

### Cleanup Architecture

On serve process exit (SIGTERM/SIGINT):
1. Query state manager for all VMs where `serve_pid == my_pid`
2. Kill each clone process: `kill -TERM <clone_pid>`
3. Remove socket file: `/mnt/fcvm-btrfs/uffd-{snapshot}-{pid}-{pid_start_time}.sock`
4. Delete serve state from state manager

### Stale State File Handling

**Problem**: State files persist when VMs crash (SIGKILL, test abort). When the OS reuses a PID, the old state file causes collisions when querying by PID.

**Solution — two layers of defense:**

1. **PID start-time identity** (`pid_start_time` field): `save_state()` records the
   process's start time from `/proc/<pid>/stat` (field 22, clock ticks since boot).
   A (pid, start_time) pair uniquely identifies a process even after PID reuse.
   `load_state_by_pid()` and `cleanup_stale_state()` verify this — if the process
   at the recorded PID has a different start time, the state file is stale.

2. **PID collision cleanup in `save_state()`**: Before saving, checks if any OTHER
   state file claims the same PID. If found, that file is stale (process is dead,
   PID was reused). Deletes the stale file with a warning log, then saves.

**State file layout**: Individual files per VM, keyed by `vm_id` (UUID):
```
/mnt/fcvm-btrfs/state/
├── vm-abc123.json    # { vm_id: "vm-abc123", pid: 5000, pid_start_time: 12345, ... }
├── vm-def456.json    # { vm_id: "vm-def456", pid: 5001, pid_start_time: 67890, ... }
└── loopback-ip.lock  # Global lock for IP allocation
```

No master state file - `list_vms()` globs all `.json` files.

## Architecture

### Project Structure
```
src/
├── lib.rs            # Module exports (public API)
├── main.rs           # CLI dispatcher
├── paths.rs          # Path utilities for btrfs layout
├── health.rs         # Health monitoring
├── kvm_trace.rs      # KVM ftrace helper for debugging snapshot restore
├── utils.rs          # Process/system utility functions
├── cli/              # Command-line parsing
│   └── args.rs       # Clap structures
├── commands/         # Command implementations
├── state/            # VM state management
├── hypervisor/       # Pluggable hypervisor (VMM) abstraction (Firecracker + Cloud Hypervisor)
├── firecracker/      # Low-level Firecracker API client (Firecracker backend impl)
├── network/          # Networking layer (bridged + pasta + routed + egress proxy)
├── storage/          # Disk/snapshot management
├── uffd/             # UFFD memory sharing
├── volume/           # FUSE volume handling
└── setup/            # Setup subcommands

tests/
├── common/mod.rs              # Shared test utilities (VmFixture, poll_health_by_pid)
├── test_sanity.rs             # End-to-end VM sanity tests (rootless + bridged)
├── test_state_manager.rs      # State manager unit tests
├── test_health_monitor.rs     # Health monitoring tests
├── test_fuse_in_vm_matrix.rs  # In-VM pjdfstest (17 categories, parallel via nextest)
├── test_localhost_image.rs    # Local image tests
├── test_snapshot_clone.rs     # Snapshot/clone workflow tests
├── test_egress.rs             # Egress proxy tests (rootless + bridged, fresh + clone)
├── test_egress_stress.rs      # Egress proxy IPv4/IPv6 stress tests
└── test_egress_proxy_bench.rs # 8000-connection concurrent benchmark

fuse-pipe/tests/
├── integration.rs              # Basic FUSE operations (no root)
├── integration_root.rs         # FUSE operations requiring root
├── test_permission_edge_cases.rs # Permission/setattr edge cases
├── test_mount_stress.rs        # Mount/unmount stress tests
├── test_allow_other.rs         # AllowOther flag tests
├── test_unmount_race.rs        # Unmount race condition tests
├── pjdfstest_matrix_root.rs    # Host-side pjdfstest (17 categories, parallel)
└── pjdfstest_common.rs         # Shared pjdfstest utilities

fuse-pipe/benches/
├── throughput.rs    # I/O throughput benchmarks
├── operations.rs    # FUSE operation latency benchmarks
└── protocol.rs      # Wire protocol benchmarks
```

### Design Principles
- **Library + Binary pattern**: src/lib.rs exports all modules, src/main.rs is thin dispatcher
- **One file per command**: Easy to find, easy to test
- **Single binary**: `fcvm` with subcommands (guest agent `fc-agent` is separate)

## Implementation Status

### ✅ Completed

1. **Core Implementation** (2025-11-09)
   - Pluggable `Hypervisor` trait (`src/hypervisor/mod.rs`) selecting the VMM behind the same orchestration, with two backends: **Firecracker (default)** and **Cloud Hypervisor**, chosen via `--hypervisor firecracker|cloud-hypervisor` (#632)
   - Firecracker backend: API client using hyper + hyperlocal (Unix sockets); boot plan via MMDS
   - Cloud Hypervisor backend: cold boot + container run, snapshot/restore/clone via `--restore` + in-process UFFD; boot plan via vsock (no MMDS)
   - Dual networking modes: bridged (iptables) + rootless (pasta)
   - Storage layer with btrfs CoW disk management
   - VM state persistence
   - Guest agent (fc-agent) receives its boot plan via MMDS (Firecracker) or over vsock (Cloud Hypervisor)

2. **Snapshot/Clone Workflow** (2025-11-11, verified 2025-11-12)
   - Pause VM → Create Firecracker snapshot → Resume VM
   - UFFD memory server serves pages on-demand via Unix socket
   - Clone disk uses btrfs reflink (~3ms instant CoW copy)
   - Clone memory load time: ~2.3ms
   - UFFD clones populate lazily (faulted pages are per-VM copies); File-backend
     clones share clean pages via the page cache (measured in #632)
   - **Performance**: Original VM + 2 idle clones ≈ ~512MB RAM total (not 1.5GB) —
     only each clone's faulted working set materializes

3. **True Rootless Networking** (2025-11-25)
   - `--network rootless` (default): pasta, no root required
   - `--network bridged`: Network namespace + iptables, requires root
   - `--network routed`: veth + IPv6 kernel routing, requires root + IPv6 host
   - User namespace via `unshare --user --net` with external UID/GID mappings
   - Health checks use unique loopback IPs (127.x.y.z) per VM

4. **Hierarchical Logging** (2025-11-15)
   - Target tags showing process nesting
   - Smart color handling: TTY gets colors, pipes don't
   - Strips Firecracker timestamps and `[anonymous-instance:*]` prefixes

5. **Container Lifecycle Management** (2025-12-08)
   - Container exit code forwarding via vsock status channel (port 4999)
   - `--privileged` mode for containers requiring device access and mknod
   - Health monitoring detects stopped containers (`HealthStatus::Stopped`)
   - `fcvm podman run` returns non-zero exit code when container fails
   - State tracking includes `exit_code` field in `VmState`

6. **Supplementary Groups Forwarding** (2025-12-08)
   - fuse-pipe forwards supplementary groups through wire protocol
   - Enables proper permission checks for remote filesystems
   - Uses raw `SYS_setgroups` syscall for per-thread credential switching
   - Critical for vsock-based FUSE where server can't read /proc

7. **Resource Limits** (2025-12-08)
   - RLIMIT_NOFILE raised to 65536 on startup (both fc-agent and fcvm)
   - Prevents EMFILE errors during parallel test execution
   - Required for large-scale POSIX compliance test suites

## Technical Reference

### Backend Requirements

fcvm runs two microVM backends behind a pluggable `Hypervisor` trait (`src/hypervisor/`), selected with `--hypervisor firecracker|cloud-hypervisor` (default: firecracker). Both cold-boot fcvm's kernel + initrd + rootfs and run a Podman container; fc-agent is injected at boot via initrd in both cases. Rootfs is Ubuntu 24.04 with systemd, Podman, and iproute2 for both.

**Firecracker** (default)
- **Kernel**: vmlinux or bzImage, boot args: `console=ttyS0 reboot=k panic=1 pci=off`
- **Rootfs**: ext4 or btrfs (via `--rootfs-type`)

**Cloud Hypervisor** (ARM64)
- **Kernel**: ARM64 `Image` (PE) format (not vmlinux/bzImage), boot args: `console=hvc0 reboot=k panic=1` (virtio console, no `pci=off`)
- **Rootfs**: declared with `image_type=Raw` (CH does not take an ext4/btrfs filesystem-type flag)

### Network Modes

| Mode | Flag | Requires Root | Performance | Port Forwarding |
|------|------|---------------|-------------|-----------------|
| Rootless (default) | `--network rootless` | No | Good | pasta CLI flags (-t/-u) |
| Bridged | `--network bridged` | Yes | Better | iptables DNAT |
| Routed | `--network routed` | Yes (+ IPv6 host) | Best (kernel line rate) | TCP proxy + loopback IP |

**Rootless Architecture:**
- Holder process starts with `unshare --user --net`, UID/GID mappings written externally
- Linux bridge (br0) connects pasta0 and tap-fc for L2 forwarding
- Bridge preserves MAC addresses for proper pasta ARP/NDP learning
- Namespace IP (10.0.2.1) on bridge enables health checks via nsenter
- Guest uses pasta network (10.0.2.100)
- Port forwarding via pasta CLI flags (-t/-u)
- IPv6 supported with `--enable-ipv6` and native DNS proxying

**Routed Architecture** (`src/network/routed.rs`):
- veth pair connects VM namespace to host with native IPv6 kernel routing (no userspace proxy)
- Each VM gets a unique IPv6 derived from host's /64 subnet via hash of vm_id
- Network namespace with bridge (br0) connecting TAP and veth for L2 forwarding
- Proxy NDP on default interface makes VM IPv6 routable from network fabric
- ip6tables MASQUERADE for AWS VPC source/dest checks (skipped when `--ipv6-prefix` is set)
- Port forwarding via built-in TCP proxy (setns + tokio relay) on unique loopback IP (same allocation as rootless)
- IPv4 stays internal to namespace (health checks only); all external traffic uses IPv6
- Egress proxy is NOT used — IPv6 goes natively through the kernel stack

**Loopback IP Allocation** (`src/state/manager.rs`):
- Sequential allocation: 127.0.0.2, 127.0.0.3, ..., 127.0.0.254, then 127.0.1.2, etc.
- Lock-protected with persistence to avoid conflicts

### btrfs CoW Reflinks

**Performance: ~1.5ms disk copy (560x faster than standard copy)**

**Architecture:**
- All data under `/mnt/fcvm-btrfs/` — native btrfs used directly if host is btrfs, otherwise a loopback image is created (size from `paths.btrfs_size` in rootfs-config.toml, default 60G)
- Base rootfs: `/mnt/fcvm-btrfs/rootfs/layer2-{sha}.raw` (~10GB raw disk with Ubuntu 24.04 + Podman)
- VM disks: `/mnt/fcvm-btrfs/vm-disks/{vm_id}/disks/rootfs.raw` (sparse, expanded per `--rootfs-size`)
- Initrd: `/mnt/fcvm-btrfs/initrd/fc-agent-{sha}.initrd` (injects fc-agent at boot)

**Per-VM Disk Sizing (`--rootfs-size`):**
- Default: `10G` — ensures at least 10G free space on root filesystem
- After btrfs reflink copy, `ensure_free_space()` auto-detects filesystem type: ext4 uses `dumpe2fs` + `truncate` + `resize2fs`; btrfs uses `btrfs dump-super` + `truncate` (guest resizes btrfs at boot)
- Sparse files: only written blocks use real disk space (50G file with 2G content uses ~2G)
- Included in `FirecrackerConfig.rootfs_size` → affects snapshot cache key (different sizes = different snapshots)
- Clones inherit parent's disk size naturally (reflink copies the already-resized file)
- Implementation: `src/storage/disk.rs::ensure_free_space()`, called from `src/commands/podman/vm_config.rs`

**Layer System:**
The rootfs is named after the SHA of a combined script that includes:
- Init script (embeds install script + setup script)
- Kernel URL
- Download script (packages + Ubuntu codename)

This ensures automatic cache invalidation when:
- The init logic, install script, or setup script changes
- The kernel URL changes (different kernel version)
- The package list or target Ubuntu version changes

**Package Download:**
Packages are downloaded using `podman run ubuntu:{codename}` with `apt-get install --download-only`.
This ensures packages match the target Ubuntu version (Noble/24.04), not the host OS.
The `codename` is specified in `rootfs-config.toml`.

**Setup Verification:**
Layer 2 setup writes a marker file `/etc/fcvm-setup-complete` and prints `FCVM_SETUP_COMPLETE` to serial console on successful completion.
After the setup VM exits, fcvm checks the serial console output for the completion marker.
If missing, setup fails with a clear error.

The initrd contains a statically-linked busybox and fc-agent binary, injected at boot before systemd.

**Setup**: Run `make setup-fcvm` before tests (called automatically by `make test-root` or `make container-test-root`).

**Content-Addressed Caching**

All assets are content-addressed - changing the input automatically creates new output:
- **Kernel**: Cached by URL hash. Different URL = new kernel.
- **Rootfs**: Cached by setup script SHA. Change script = new rootfs.
- **Initrd**: Cached by fc-agent binary SHA. Rebuild fc-agent = new initrd.

**NEVER manually delete cached assets.** Just rebuild and run `make setup-fcvm`:
```bash
# Change fc-agent code, then:
cargo build --release -p fc-agent
make setup-fcvm  # Creates new initrd with new SHA

# Change rootfs-config.toml, then:
make setup-fcvm  # Creates new rootfs with new SHA
```

**Custom Kernel (Nested Virtualization)**

Use `--kernel-profile` flag for named kernel configurations:
```bash
# Build nested kernel with CONFIG_KVM=y
fcvm setup --kernel-profile nested --build-kernels

# Run VM with nested kernel profile
sudo fcvm podman run --name my-vm --network bridged \
    --kernel-profile nested \
    nginx:alpine
```

**Kernel Build Architecture:**
- **Config is source of truth**: All kernel versions and build settings flow from `rootfs-config.toml`
- **No hardcoded versions**: Version numbers like `6.18.3` are ONLY in config, never in Rust code
- **Dynamic build scripts**: Rust generates build scripts on-the-fly (no `build.sh` or `build-host.sh` in source)
- **Config sync**: `make build` automatically syncs embedded config to `~/.config/fcvm/` via `fcvm setup --generate-config --force`
- **Content-addressed**: Kernel SHA computed from `build_inputs` patterns (config + patches)

Key config fields in `[kernel_profiles.nested.arm64]`:
```toml
kernel_version = "6.18.3"              # Version to download/build
kernel_repo = "ejc3/fcvm"           # GitHub repo for releases
build_inputs = ["kernel/nested.conf", "kernel/patches/*.patch"]  # Files for SHA
kernel_config = "kernel/nested.conf"   # Kernel .config
patches_dir = "kernel/patches"         # Directory with patches
```

**Creating/Editing Kernel Patches:**
```bash
make kernel-patch-create PROFILE=nested NAME=0004-my-fix FILE=fs/fuse/dir.c
make kernel-patch-edit PROFILE=nested PATCH=0002
make kernel-patch-validate PROFILE=nested
```

NEVER hand-write patches - the hunk counts will be wrong. Always use the helper script which generates proper `git format-patch` output.

When a patch change doesn't fix the issue, the bug is incomplete root cause analysis - not "needs a workaround". Adding workarounds (env vars, flags) masks bugs. Find and fix ALL causes.

### NEVER Assume - Always Investigate

**Disabling tests is NEVER acceptable.** When a test fails:
1. **Don't assume** the test is wrong or the limitation is fundamental
2. **Don't assume** someone else's workaround (like #[ignore]) was correct
3. **Investigate** the actual code path - read the library source
4. **Find the root cause** - there's usually a missing initialization or config

**Example anti-pattern (O_WRONLY + writeback cache):**
```
❌ WRONG: "O_WRONLY is fundamentally incompatible with writeback cache"
   → Added #[ignore] to test
   → Assumed the limitation was in FUSE kernel design

✅ CORRECT: Read fuse-backend-rs source code
   → Found get_writeback_open_flags() exists and promotes O_WRONLY → O_RDWR
   → But init() wasn't being called to enable the writeback flag
   → Fixed by calling inner.init(FsOptions::WRITEBACK_CACHE)
   → Test passes, no workaround needed
```

**The fix is almost always in the code, not in disabling tests.**

NEVER manually edit rootfs files. The setup script in `rootfs-config.toml` and `src/setup/rootfs.rs` control what gets installed.

### Memory Sharing (UFFD)

**Workflow:**
```bash
# 1. Start baseline VM
fcvm podman run --name baseline --network bridged nginx:alpine

# 2. Create snapshot from running VM
fcvm snapshot create --pid <baseline_pid> --tag my-snapshot

# 3. Start memory server (serves pages via UFFD)
fcvm snapshot serve my-snapshot    # Creates /mnt/fcvm-btrfs/uffd-my-snapshot-<pid>-<pid_start_time>.sock

# 4. Spawn clones from the memory server
fcvm snapshot run --pid <serve_pid> --name clone1
```

**How it works:**
- Memory server mmaps the snapshot file once; the page cache holds one copy
- Guest RAM is MAP_ANONYMOUS; faults are filled with UFFDIO_COPY (a private
  per-VM copy of each faulted page — lazy population, not cross-VM sharing)
- Server uses tokio AsyncFd to handle UFFD events non-blocking
- tokio::select! multiplexes: accept new VMs + monitor VM exits
- Each VM gets dedicated async task (JoinSet) for page faults
- All tasks share Arc<Mmap> reference to memory file
- Server exits gracefully when last VM disconnects

**Memory efficiency:**
- UFFD path: density comes from laziness — only each clone's faulted working
  set materializes (faulted pages are per-VM copies, #632)
- File-backend restores (`snapshot run --snapshot`): clones genuinely share
  clean pages via the page cache (MAP_PRIVATE) — measured 3x 1GiB clones
  ≈ 230MiB total PSS, with or without dirty tracking (#632)

### Working-Set Replay (`--uffd-prefetch`, default on)

Demand paging is the UFFD path's latency tax: a Chromium clone takes ~56,300 faults at
~5.6 us marginal each (+316 ms versus a file-backed restore on the same page). Clones of one
snapshot fault almost the same PAGES — pairwise Jaccard median 0.927 across 8 clones, 82.2% of
the union faulted by all 8 — but NOT in the same ORDER (only 8.6% of faults are the next
sequential page, which is why readahead and fault-around do nothing here). So the server
records the SET and replays it.

- **Record**: every demand fault marks its snapshot file offset in a 4 KiB-granular bitmap
  (32 KiB per GiB of guest RAM). On handler exit the bitmap is unioned into the serve
  process's in-memory set and scheduled onto one bounded, coalescing background writer. The
  writer publishes `<memory.bin>.working-set` beside the snapshot under an `flock` + atomic
  rename, and only writes when the union actually grew — so the steady state writes nothing,
  and a clone that was killed mid-restore gets completed by the next one instead of baking in
  a truncated set. Persistence is a performance hint: sidecar lock contention skips that
  publication attempt without delaying clone teardown, admission slots, or server shutdown;
  the in-memory union remains available for the next request. Publication also takes the
  snapshot generation lock shared from the final image-identity check through the atomic
  rename. Snapshot replacement takes that lock exclusively, so it either finishes before the
  check (the old clone declines to publish) or waits until an active publication completes; it
  cannot land in between.
- **Replay**: at handshake the recorded set is coalesced into runs, mapped into that clone's
  regions, aligned to its page size, and populated in 2 MiB `UFFDIO_COPY`/`UFFDIO_CONTINUE`
  chunks. This runs before the guest's first instruction (fcvm loads with `resume_vm: false`),
  but it is NOT a barrier — the resume comes from the clone process — so a drain of real
  faults precedes every chunk and demand always beats speculation.
- **Invalidation**: keyed by the exact `config.json` digest plus the memory image's
  (`len, mtime, ino, dev`) identity, not a memory-image content hash — SHA-256 of a 2 GiB
  image measures 1.4 s at 1.5 GB/s here, which costs more than the mis-prefetch it would
  prevent. The config digest makes atomically installed generations distinct even under inode
  reuse. Safe because a working set only says WHICH offsets to copy; the BYTES always come
  from the file being served, so a stale set wastes a copy and can never corrupt a guest.
- **Isolation**: replay touches pages the guest never asked for, so it must stay private.
  `UFFDIO_COPY` writes into the clone's own anonymous memory, and `UFFDIO_CONTINUE` installs a
  read-only PTE that copies on write. Proven by `prefetched_pages_are_private_to_each_clone`
  (mechanism level, milliseconds) and `test_snapshot_clone_working_set_replay` (two clones,
  cross-write invisibility, `memory.bin` byte-identical).
- **Measuring it**: the memory server logs `replayed recorded working set` with
  `prefetched_pages`, and `VM exited` with `fault_count`. Compare a recording clone against a
  replaying one; `--uffd-prefetch off` (or `FCVM_UFFD_PREFETCH=off`) gives an inert baseline
  arm — no recording, no replay, no files.
- **The trade is latency for eagerness, not for total memory.** A replaying clone materialises
  its working set at restore instead of over the first ~750 ms, so a clone that lives a full
  life ends up at the same footprint, just sooner; a clone that is created and destroyed
  immediately pays for pages it would not have reached. Density claims above (idle clones cost
  only what they faulted) still hold per page — replay changes WHEN, not WHAT. Turn it off for
  workloads that spawn many clones which never run.

**End of clone (`PeerVmm`)**: a userfaultfd reports nothing when the process that created it
dies — measured on this kernel, `poll` returns 0/revents=0 forever and `read` returns EAGAIN —
because the server still holds its own reference. The server therefore obtains the connecting
Firecracker's pidfd atomically from the accepted socket with `SO_PEERPIDFD`, before handshake
or admission, and selects that exact handle alongside UFFD readiness. PIDs are never
re-resolved, so process reuse cannot redirect observation or the fail-closed SIGKILL. The pidfd
edge ends a normal handler and schedules the working-set union for best-effort background
publication. Handshake and page-fault service failures follow the existing fail-closed path and
terminate the pinned VMM rather than leaving its guest wedged on an unserved fault; hint load,
speculative-population, and persistence failures degrade to ordinary demand paging because they
do not stop fault service.

### FUSE Parallelism (fuse-pipe)

**Kernel clone fd model (`FUSE_DEV_IOC_CLONE`):**

Each cloned fd is a complete request-response pipeline. A request dequeued from clone_fd_A is pinned to that fd — the response **must** be written back to clone_fd_A (`fuse_request_find()` searches only per-fd `fpq->processing`, not a global list).

| What | Serialized | Parallel |
|------|-----------|----------|
| Dequeue from `fiq->pending` | Yes — brief `fiq->lock` spinlock shared across ALL fds | N/A (one request dequeued at a time) |
| Copy request to userspace | No lock held | Yes — each fd copies independently |
| Process request | N/A | Yes — each thread processes its own request |
| Write response | Per-fd `fpq->lock` (no cross-fd contention) | Yes — each fd writes independently |
| FORGET/BATCH_FORGET | Fire-and-forget, no response written | Dequeued from shared `fiq->forget_list` |

Without clone fd, all threads share one fd and contend on both `fiq->lock` and the single `fpq->lock`.

**fuse-pipe layers:**

| Layer | Serialized | Parallel |
|-------|-----------|----------|
| Client reader threads (`mount.rs`) | Dequeue briefly serialized (kernel `fiq->lock`) | N threads with N cloned fds read/write independently |
| Multiplexer (socket) | Socket write serialized by channel | Request submission lock-free; responses routed by unique ID |
| Server socket reader (`pipelined.rs`) | Reads requests sequentially from socket | — |
| Server handler dispatch | — | Each request dispatched via `tokio::spawn` + `spawn_blocking`; concurrent on tokio blocking pool |

**Implication:** `FilesystemHandler` implementations (including `RemapFs`) must be thread-safe. Shared state requires atomic operations or lock-free data structures (e.g., `DashMap::entry()` for atomic check-and-insert).

### FUSE Passthrough Performance (fuse-pipe)

**Benchmark**: 256 workers, 1024 files × 4KB

#### Parallel Reads

| Readers | Time (ms) | vs Host | Speedup vs 1 Reader |
|---------|-----------|---------|---------------------|
| Host FS | 10.7 | 1.0x | - |
| 1 | 490.6 | 45.8x slower | 1.0x |
| 16 | 63.7 | 5.9x slower | 7.70x |
| **256** | **57.0** | **5.3x slower** | **8.61x** |

#### Parallel Writes (with sync_all)

| Readers | Time (s) | vs Host |
|---------|----------|---------|
| Host FS | 0.862 | 1.0x |
| 16 | 2.435 | 2.8x slower |
| **256** | **2.765** | **3.2x slower** |

**Recommendation**: Use 256 readers for mixed workloads.

## Build Instructions

Run `make help` for all targets. See README.md for details.

### How Setup Works

**Setup is explicit, not automatic.** VMs require kernel, rootfs, and initrd to exist before running.

**Two ways to set up:**

1. **`fcvm setup`** (explicit, works for all modes):
   - Downloads kernel and creates rootfs
   - Required before running VMs with bridged networking (root)

2. **`fcvm podman run --setup`** (rootless only):
   - Adds `--setup` flag to opt-in to auto-setup
   - Only works for rootless mode (no root)
   - Disallowed when running as root - use `fcvm setup` instead

**Without setup**, fcvm fails immediately if assets are missing:
```
ERROR fcvm: Error: setting up rootfs: Rootfs not found. Run 'fcvm setup' first, or use --setup flag.
```

**What `fcvm setup` does:**
1. Downloads the released default fcvm kernel selected by the explicit
   architecture profile (content-addressed by version, arch, and manifest SHA)
2. Downloads packages using `podman run ubuntu:noble` with `apt-get install --download-only`
   - Packages specified in `rootfs-config.toml` (podman, crun, fuse-overlayfs, skopeo, fuse3, haveged, chrony, strace)
   - Uses target Ubuntu version (noble/24.04) to get correct package versions
3. Creates Layer 2 rootfs (~10GB):
   - Downloads Ubuntu cloud image
   - Boots VM with packages embedded in initrd
   - Runs install script (dpkg) + setup script (config files, services)
   - Verifies setup completed by checking for `/etc/fcvm-setup-complete` marker file
4. Creates fc-agent initrd (embeds statically-linked fc-agent binary)

**Kernel source**: fcvm builds the pinned Linux release from an immutable
Firecracker base config. Default arm64 and amd64 fragments require FUSE plus
`CONFIG_INET_DIAG`, `CONFIG_INET_DIAG_DESTROY`, and `CONFIG_PACKET` for safe
snapshot socket cleanup.

### Data Layout

Paths are configured in `rootfs-config.toml` under `[paths]`:
- `assets_dir`: Content-addressed files (shared across nesting levels)
- `data_dir`: Mutable per-instance data (separate per nesting level)

```
assets_dir (default: /mnt/fcvm-btrfs)
├── kernels/vmlinux-{profile}-{version}-{arch}-{sha}.bin  # Released source kernel
├── rootfs/layer2-{sha}.raw       # Base image (~10GB, SHA of setup script)
├── initrd/fc-agent-{sha}.initrd  # fc-agent injection (SHA of binary)
├── image-cache/sha256:{digest}/  # Container image layers
└── cache/                        # Downloaded cloud images

data_dir (default: /mnt/fcvm-btrfs, override per nesting level)
├── vm-disks/{vm_id}/disks/       # CoW reflink copies per VM
├── state/{vm_id}.json            # VM state files
└── snapshots/{name}/             # Firecracker snapshots
```

## Key Learnings

### Serial Console
- Problem: VM booted but no output after init
- Fix: Kernel boot args include `console=ttyS0` (done automatically)

### Clone Network Configuration
- Problem: Guest retains original static IP after snapshot restore
- Root cause: Firecracker's network override only changes TAP device name, not guest IP
- Fix: Configure TAP devices on SAME subnet as guest's original IP
```bash
# Wrong: TAP on different subnet than guest
ip addr add 172.16.201.1/24 dev tap-vm-c93e8  # Guest thinks it's 172.16.29.2

# Correct: TAP on same subnet as guest
ip addr add 172.16.29.1/24 dev tap-vm-c93e8   # Guest is 172.16.29.2
```
- Reference: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/network-for-clones.md

### KVM Requirements
- Firecracker REQUIRES `/dev/kvm`
- On AWS: c6g.metal (ARM64) or c5.metal (x86_64) work; c5.large does NOT
- On other clouds: use bare-metal or hosts with nested virtualization

### DNS Resolution in VMs
- VMs use host's DNS servers directly (read from `/etc/resolv.conf`)
- For systemd-resolved hosts, falls back to `/run/systemd/resolve/resolv.conf`
- Traffic flows: Guest → NAT → Host's DNS servers
- No dnsmasq required

### Container Resource Limits (EAGAIN Debugging)

**Symptom:** Tests fail with "Resource temporarily unavailable (os error 11)" or "fork/exec: resource temporarily unavailable"

**Debugging steps:**
1. Check dmesg for cgroup rejections:
   ```bash
   sudo dmesg | grep -i "fork rejected"
   # Look for: "cgroup: fork rejected by pids controller in /machine.slice/libpod-..."
   ```

2. Check actual process/thread counts (usually much lower than limits):
   ```bash
   ps aux | wc -l          # Process count
   ps -eLf | wc -l         # Thread count
   ps -eo user,nlwp,comm --sort=-nlwp | head -20  # Top by threads
   ```

3. Check container pids limit (NOT ulimit - cgroup is separate!):
   ```bash
   sudo podman run --rm alpine cat /sys/fs/cgroup/pids.max
   # Default: 2048 (way too low for parallel VM tests)
   ```

**Root cause:** Podman sets cgroup pids limit to 2048 by default. This is NOT the same as `ulimit -u` (nproc). The cgroup pids controller limits total processes/threads in the container.

**Fix:** Use `--pids-limit=65536` in container run command (already in Makefile).

### Pipe Buffer Deadlock in Tests (CRITICAL)

**Problem:** Tests hang indefinitely when spawning fcvm with `Stdio::piped()` but not reading the pipes.

**Root cause:**
- Linux pipe buffer is 64KB
- fcvm outputs 100+ lines of Firecracker serial console logs
- When buffer fills, child process blocks on `write()` syscall
- This prevents ALL async tasks in the child (including health monitor) from running
- Result: VM never becomes "healthy", test times out

**Symptoms:**
- Test works manually with `| tee /tmp/log` (because tee consumes output)
- Test hangs when run via `cargo test`
- State file timestamp never updates (health monitor blocked)
- VM is actually running fine, just not being monitored

**Fix:** NEVER use `Stdio::piped()` unless you actively consume the output. Use the `spawn_fcvm()` helper which uses `Stdio::inherit()`:

```rust
// WRONG - will deadlock!
let child = tokio::process::Command::new(&fcvm_path)
    .args([...])
    .stdout(Stdio::piped())  // Never read = deadlock
    .stderr(Stdio::piped())  // Never read = deadlock
    .spawn()?;

// CORRECT - use the helper
let (mut child, pid) = common::spawn_fcvm(&["podman", "run", "--name", &vm_name, ...]).await?;
```

**The helper enforces:**
- `Stdio::inherit()` for stdout/stderr - output goes to parent (visible with `--nocapture`)
- No deadlock because parent's stdout/stderr handle the data
- Consistent error handling and PID extraction

## Exec Command Flags

`fcvm exec` uses `-i` and `-t` separately, matching podman/docker:
- `-t`: allocate PTY (for colors/formatting)
- `-i`: forward stdin
- `-it`: both (interactive shell)
- neither: plain exec

**NO backward compatibility wrappers.** When the API changed from `run_tty_mode(stream)` to `run_tty_mode(stream, interactive)`, all callers were updated directly - no deprecated functions or compatibility shims.

## References
- Main documentation: `README.md`
- Performance guide: `PERFORMANCE.md`
- Design specification: `DESIGN.md`
- Firecracker docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
