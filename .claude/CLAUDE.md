# fcvm Development Log

## ALWAYS FIX FAILING TESTS. PERIOD.

**If ANY test fails, FIX THE ROOT CAUSE.** No exceptions. No workarounds. No weakening assertions.

- Never add flags like `--no-cache` to avoid failures
- Never weaken assertions to accept broken behavior
- Never skip, ignore, or comment out failing tests
- Always fix the actual bug in the code

This is non-negotiable. A test failure means the CODE is broken - fix the code, not the test.

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
1. Re-running in isolation to verify the test itself is correct is fine
2. But you MUST root cause why it failed when run together
3. All tests must pass together - that's the point of parallel testing
4. If a test passes alone but fails in parallel, there's a race condition - find it

**The pattern:**
```
Test failed in parallel run
  → Re-run alone: passes
  → "Probably resource contention" ← WRONG, this is speculation
  → Look at actual error message ← CORRECT
  → Find the race condition ← REQUIRED
```

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
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

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

### L2 Single vCPU Requirement (2026-01)

**Problem**: L2 VMs with 2+ vCPUs hit `NETDEV WATCHDOG: CPU: 1: transmit queue 0 timed out`
around 23-29 seconds after boot. The virtio-net TX queue stops being serviced.

**Symptoms**:
- fc-agent reaches "configuring DNS from kernel cmdline" then hangs
- NETDEV WATCHDOG fires with 5600ms timeout
- L2 network becomes unresponsive

**Root cause**: Multi-vCPU nested VMs under NV2 have interrupt delivery issues between vCPUs.
The virtio-net driver on one vCPU puts packets on the TX queue, but the notification/interrupt
path to L1's Firecracker isn't processed correctly with multiple vCPUs.

**Solution**: Use single vCPU for L2+ VMs (`--cpu 1`). The test framework automatically
applies this for nested VM launches.

**Why it works**: With a single vCPU, there's no cross-vCPU interrupt path to go wrong.
All virtio notifications go through the same vCPU, avoiding the NV2 multi-vCPU issues.

**Impact**: L2 VMs are limited to 1 vCPU. This is a performance tradeoff but enables
reliable L2 operation until ARM/kernel developers fix the underlying NV2 issues.

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

# Snapshot workflow
fcvm snapshot create --pid <vm_pid> --tag my-snapshot
fcvm snapshot serve my-snapshot      # Start UFFD server (prints serve PID)
fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
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

**Stacking PRs:** When work builds on unmerged PRs, create a chain:
```bash
# PR #1 is on main
git checkout -b feature-a && git push -u origin feature-a
gh pr create --base main

# PR #2 builds on PR #1
git checkout -b feature-b && git push -u origin feature-b
gh pr create --base feature-a  # Not main!

# Verify the chain
gh pr list --json number,headRefName,baseRefName
```
Merge in order (#1 first, then #2). After merging #1, GitHub auto-updates #2's base to main.

**CRITICAL: Maintain Stack Coherence.** When PRs are stacked, the branch for PR #2 MUST actually be based on PR #1's branch - not just have the GitHub base set correctly. Verify with:
```bash
# PR #2's commits should include PR #1's commits
git log --oneline origin/main..feature-b
# Should show: PR #2 commits THEN PR #1 commits
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

**MANDATORY before merging any PR:** Read all review comments first:
```bash
gh pr view <pr-number> --json comments --jq '.comments[] | .body'
```

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

**Anti-pattern:** For a stacked PR, reviewing `main..HEAD` includes commits from parent branches. This causes incorrect claims like "X was never enabled on main" when X was enabled in THIS branch's commits.

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

**This project is designed for extreme scale, speed, and correctness.** Test failures are bugs, not excuses.

**NEVER dismiss failures as:**
- "Resource contention" - **This is NEVER the answer. It's always a race condition.**
- "Timing issues" - **This means there's a race condition. Find and fix it.**
- "Flaky tests" - **No such thing. The test found a bug. Fix the bug.**
- "Works on my machine" - **Your machine just got lucky. The bug is real.**

**"Resource contention" is a lie you tell yourself to avoid finding the real bug.** When a test fails under load:
1. The test is correct - it found a bug
2. The bug only manifests under certain timing conditions
3. This is called a **race condition**
4. You MUST find the race and fix it

**ALWAYS:**
1. **Look at the logs** - The answer is always there
2. Investigate the actual root cause with evidence
3. Find the race condition - there IS one
4. Fix the underlying bug
5. Add regression tests if needed

If a test fails intermittently or only under parallel execution, that's a **concurrency bug** or **race condition** that must be fixed, not ignored. The test passed in isolation? Great - that narrows down the timing window where the race occurs.

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

**NEVER use `sudo cargo`. ALWAYS use Makefile targets.**

The Makefile uses `CARGO_TARGET_*_RUNNER='sudo -E'` to run test binaries with sudo, not cargo itself. Using `sudo cargo` creates root-owned files in `target/` that break subsequent builds.

See README.md for test tiers and Makefile targets.

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

**CRITICAL: ALWAYS include branch name in tee log filenames.**

Without the branch name, logs from different branches overwrite each other and you lose the ability to compare results or diagnose issues. This is especially important when:
- Working on stacked PRs (branch A depends on branch B)
- Developing two features in parallel
- Switching between branches to compare behavior
- Using multiple worktrees

```bash
# ALWAYS get branch name first
BRANCH=$(git branch --show-current)

# Run full test suite - include branch AND target
make test-root 2>&1 | tee /tmp/test-${BRANCH}-root.log

# Run filtered tests - include branch, target, AND filter
make test-root FILTER=exec 2>&1 | tee /tmp/test-${BRANCH}-root-exec.log
make test-root FILTER=sanity 2>&1 | tee /tmp/test-${BRANCH}-root-sanity.log
```

**When a test fails, IMMEDIATELY save the log to a uniquely-named file for diagnosis:**

```bash
# Pattern: /tmp/fcvm-failed-{branch}-{target}-{test_name}-{timestamp}.log
BRANCH=$(git branch --show-current)

# Example after test_exec_rootless fails in test-root run:
cp /tmp/test-${BRANCH}-root.log /tmp/fcvm-failed-${BRANCH}-root-test_exec_rootless-$(date +%Y%m%d-%H%M%S).log

# Then continue with other tests using a fresh log file
make test-root 2>&1 | tee /tmp/test-${BRANCH}-root-run2.log
```

**Automated approach:**
```bash
# After a test suite run, check for failures and save logs
BRANCH=$(git branch --show-current)
if grep -q "FAIL\|TIMEOUT" /tmp/test-${BRANCH}-root.log; then
  cp /tmp/test-${BRANCH}-root.log /tmp/fcvm-failed-${BRANCH}-root-$(date +%Y%m%d-%H%M%S).log
  echo "Saved failed test log"
fi
```

### Debugging fuse-pipe Tests

**ALWAYS run tests with debug logging enabled when debugging issues:**

```bash
# Run single test with debug logging
sudo RUST_LOG=debug cargo test --release -p fuse-pipe --test test_permission_edge_cases test_write_clears_suid -- --nocapture

# Run all permission tests with debug logging
sudo RUST_LOG=debug cargo test --release -p fuse-pipe --test test_permission_edge_cases -- --nocapture --test-threads=1

# Filter to specific components
sudo RUST_LOG="passthrough=debug,fuse_pipe=debug" cargo test ...

# Debug fuse-backend-rs internals
sudo RUST_LOG="fuse_backend_rs=debug" cargo test ...
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
   RUST_LOG='passthrough=debug' sudo -E cargo test ... -- --nocapture
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
- Always use `make test-root FILTER=<pattern>` - never raw cargo commands
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
    pub pid: Option<u32>,  // fcvm process PID (from std::process::id())
}
```

### Cleanup Architecture

On serve process exit (SIGTERM/SIGINT):
1. Query state manager for all VMs where `serve_pid == my_pid`
2. Kill each clone process: `kill -TERM <clone_pid>`
3. Remove socket file: `/mnt/fcvm-btrfs/uffd-{snapshot}-{pid}.sock`
4. Delete serve state from state manager

### Stale State File Handling

**Problem**: State files persist when VMs crash (SIGKILL, test abort). When the OS reuses a PID, the old state file causes collisions when querying by PID.

**Solution**: `StateManager::save_state()` automatically cleans up stale state files:
- Before saving, checks if any OTHER state file claims the same PID
- If found, that file is stale (the process is dead, PID was reused)
- Deletes the stale file with a warning log
- Then saves the new state

**Why it works**: If process A has PID 5000 and we're saving state for process B with PID 5000, process A must be dead (OS wouldn't reuse the PID otherwise). So A's state file is safe to delete.

**State file layout**: Individual files per VM, keyed by `vm_id` (UUID):
```
/mnt/fcvm-btrfs/state/
├── vm-abc123.json    # { vm_id: "vm-abc123", pid: 5000, ... }
├── vm-def456.json    # { vm_id: "vm-def456", pid: 5001, ... }
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
├── cli/              # Command-line parsing
│   └── args.rs       # Clap structures
├── commands/         # Command implementations
├── state/            # VM state management
├── firecracker/      # Firecracker API client
├── network/          # Networking layer (bridged + slirp)
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
└── test_snapshot_clone.rs     # Snapshot/clone workflow tests

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
   - Firecracker API client using hyper + hyperlocal (Unix sockets)
   - Dual networking modes: bridged (iptables) + rootless (slirp4netns)
   - Storage layer with btrfs CoW disk management
   - VM state persistence
   - Guest agent (fc-agent) with MMDS integration

2. **Snapshot/Clone Workflow** (2025-11-11, verified 2025-11-12)
   - Pause VM → Create Firecracker snapshot → Resume VM
   - UFFD memory server serves pages on-demand via Unix socket
   - Clone disk uses btrfs reflink (~3ms instant CoW copy)
   - Clone memory load time: ~2.3ms
   - Multiple VMs share same memory via kernel page cache
   - **Performance**: Original VM + 2 clones = ~512MB RAM total (not 1.5GB!)

3. **True Rootless Networking** (2025-11-25)
   - `--network rootless` (default): slirp4netns, no root required
   - `--network bridged`: Network namespace + iptables, requires root
   - User namespace via `unshare --user --map-root-user --net`
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

### Firecracker Requirements
- **Kernel**: vmlinux or bzImage, boot args: `console=ttyS0 reboot=k panic=1 pci=off`
- **Rootfs**: ext4 with Ubuntu 24.04, systemd, Podman, iproute2, fc-agent at `/usr/local/bin/fc-agent`

### Network Modes

| Mode | Flag | Requires Root | Performance | Port Forwarding |
|------|------|---------------|-------------|-----------------|
| Rootless (default) | `--network rootless` | No | Good | slirp4netns API |
| Bridged | `--network bridged` | Yes | Better | iptables DNAT |

**Rootless Architecture:**
- Firecracker starts with `unshare --user --map-root-user --net`
- slirp4netns connects to the namespace via PID, creates TAP device
- Dual-TAP design: slirp0 (10.0.2.x) for slirp4netns, tap0 (192.168.x.x) for Firecracker
- Port forwarding via slirp4netns JSON-RPC API socket
- Health checks use unique loopback IPs (127.x.y.z) per VM

**Loopback IP Allocation** (`src/state/manager.rs`):
- Sequential allocation: 127.0.0.2, 127.0.0.3, ..., 127.0.0.254, then 127.0.1.2, etc.
- Lock-protected with persistence to avoid conflicts

### btrfs CoW Reflinks

**Performance: ~1.5ms disk copy (560x faster than standard copy)**

**Architecture:**
- All data under `/mnt/fcvm-btrfs/` (btrfs filesystem)
- Base rootfs: `/mnt/fcvm-btrfs/rootfs/layer2-{sha}.raw` (~10GB raw disk with Ubuntu 24.04 + Podman)
- VM disks: `/mnt/fcvm-btrfs/vm-disks/{vm_id}/disks/rootfs.raw`
- Initrd: `/mnt/fcvm-btrfs/initrd/fc-agent-{sha}.initrd` (injects fc-agent at boot)

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
Layer 2 setup writes a marker file `/etc/fcvm-setup-complete` on successful completion.
After the setup VM exits, fcvm mounts the rootfs and verifies this marker exists.
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
fcvm snapshot serve my-snapshot    # Creates /mnt/fcvm-btrfs/uffd-my-snapshot-<pid>.sock

# 4. Spawn clones from the memory server
fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
```

**How it works:**
- Memory server mmaps snapshot file (MAP_SHARED)
- Kernel shares physical pages via page cache
- Server uses tokio AsyncFd to handle UFFD events non-blocking
- tokio::select! multiplexes: accept new VMs + monitor VM exits
- Each VM gets dedicated async task (JoinSet) for page faults
- All tasks share Arc<Mmap> reference to memory file
- Server exits gracefully when last VM disconnects

**Memory efficiency:**
- 50 VMs with 512MB snapshot = ~512MB physical RAM (not 25.6GB)
- Pages only copied on write (true CoW at page level)

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
1. Downloads Kata kernel from URL in `rootfs-config.toml` (~15MB, cached by URL hash)
2. Downloads packages using `podman run ubuntu:noble` with `apt-get install --download-only`
   - Packages specified in `rootfs-config.toml` (podman, crun, fuse-overlayfs, skopeo, fuse3, haveged, chrony, strace)
   - Uses target Ubuntu version (noble/24.04) to get correct package versions
3. Creates Layer 2 rootfs (~10GB):
   - Downloads Ubuntu cloud image
   - Boots VM with packages embedded in initrd
   - Runs install script (dpkg) + setup script (config files, services)
   - Verifies setup completed by checking for `/etc/fcvm-setup-complete` marker file
4. Creates fc-agent initrd (embeds statically-linked fc-agent binary)

**Kernel source**: Kata Containers kernel (6.12.47 from Kata 3.24.0 release) with `CONFIG_FUSE_FS=y` built-in.

### Data Layout

Paths are configured in `rootfs-config.toml` under `[paths]`:
- `assets_dir`: Content-addressed files (shared across nesting levels)
- `data_dir`: Mutable per-instance data (separate per nesting level)

```
assets_dir (default: /mnt/fcvm-btrfs)
├── kernels/vmlinux-{sha}.bin     # Kernel (SHA of URL)
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

## fuse-pipe Debugging

**Tracing targets** for debugging FUSE issues:
- `passthrough` - PassthroughFs operations (most useful)
- `fuse_pipe` - fuse-pipe client/server
- `fuse_backend_rs` - fuse-backend-rs internals

```bash
RUST_LOG="passthrough=debug" make test-root FILTER=permission -- --nocapture
```

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
