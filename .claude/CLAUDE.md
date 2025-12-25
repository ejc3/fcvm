# fcvm Development Log

## Overview
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

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

### Common Commands
```bash
# Build
make build        # Build fcvm + fc-agent
make test         # Run fuse-pipe tests
make setup-fcvm   # Download kernel and create rootfs

# Run a VM (requires setup first, or use --setup flag)
sudo fcvm podman run --name my-vm --network bridged nginx:alpine

# Or run with auto-setup (first run takes 5-10 minutes)
sudo fcvm podman run --name my-vm --network bridged --setup nginx:alpine

# Snapshot workflow
fcvm snapshot create --pid <vm_pid> --tag my-snapshot
fcvm snapshot serve my-snapshot      # Start UFFD server (prints serve PID)
fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
```

### Manual E2E Testing with Claude Code

**CRITICAL: VM commands BLOCK the terminal.** You MUST use Claude's `run_in_background: true` feature.

```bash
# WRONG - This blocks forever, wastes context, and times out
sudo fcvm podman run --name test nginx:alpine

# CORRECT - Run VM in background, then use exec to test
sudo ./target/release/fcvm podman run --name test --network bridged nginx:alpine 2>&1 | tee /tmp/vm.log
# Use run_in_background: true in Bash tool call
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

### Development Workflow (PR-Based)

**Local commits are fast. Branch before push.**

1. **Commit locally to main** as you work - no interruption
2. **Before pushing**, create a branch and PR:
   ```bash
   # Create branch from current main
   git checkout -b feature/description
   git push -u origin feature/description
   gh pr create --fill
   # Go back to main for next work
   git checkout main
   ```
3. **Continue working** - more local commits to main
4. **End of session** - check CI on all PRs, merge in order:
   ```bash
   # Check all PR statuses
   gh pr list --author @me
   gh pr checks <pr-number>
   # Merge when green
   gh pr merge <pr-number> --merge --delete-branch
   git pull  # Update local main
   ```

**Why this works:**
- Feels like committing to main (local commits are instant)
- PRs provide CI validation before merge
- Can stack multiple PRs without waiting
- Merge at end when CI is green

### Commit Messages

**Detailed messages with context and testing.** Commit messages should capture the nuance from the session that created them.

**What to include:**
- **What changed** - specific files, functions, behaviors modified
- **Why it changed** - the problem being solved or feature being added
- **How it was tested** - "show don't tell" with actual commands/output

**Good example:**
```
Remove obsolete require_non_root guard function

The function was a no-op kept for "API compatibility" - exactly what
our NO LEGACY policy prohibits. Rootless tests work fine under sudo.

Removed function and all 12 call sites across test files.

Tested: make test-root FILTER=sanity (both rootless and bridged pass)
```

**Bad example:**
```
Fix tests
```

**Testing section format** - show actual commands:
```
Tested:
  make test-root FILTER=sanity            # passed
  make container-test-root FILTER=sanity  # passed
```

Not vague claims like "tested and works" or "verified manually".

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

Why: String matching breaks when JSON formatting changes (spaces, newlines, field order). Proper deserialization is robust and self-documenting.

### Test Failure Philosophy

**This project is designed for extreme scale, speed, and correctness.** Test failures are bugs, not excuses.

**NEVER dismiss failures as:**
- "Resource contention"
- "Timing issues"
- "Flaky tests"
- "Works on my machine"

**ALWAYS:**
1. Investigate the actual root cause
2. Find evidence in logs, traces, or code
3. Fix the underlying bug
4. Add regression tests if needed

If a test fails intermittently, that's a **concurrency bug** or **race condition** that must be fixed, not ignored.

### Race Condition Debugging Protocol

**Workarounds are NOT acceptable.** When a test fails due to a race condition:

1. **NEVER "fix" it with timing changes** like:
   - Increasing timeouts
   - Adding sleeps
   - Separating phases that should work concurrently
   - Reducing parallelism

2. **ALWAYS examine the actual output:**
   - Capture FULL logs from failing test runs
   - Look at what the SPECIFIC failing component did/didn't do
   - Trace timestamps to understand ordering
   - Find the EXACT operation that failed

3. **Ask the right questions:**
   - What's different about the failing component vs. successful ones?
   - What resource/state is being contended?
   - What initialization happens on first access?
   - Are there orphaned processes or stale state?

4. **Find and fix the ROOT CAUSE:**
   - If it's a lock ordering issue, fix the locking
   - If it's uninitialized state, fix the initialization
   - If it's resource exhaustion, fix the resource management
   - If it's a cleanup issue, fix the cleanup

**Example bad fix:** "Clone-0 times out while clones 1-99 succeed" → "Let's wait for all spawns before health checking"

**Correct approach:** Look at clone-0's logs to see WHY it specifically failed. What did clone-0 do differently? What resource did it touch first?

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

2. **Port conflicts**: Loopback IP allocation checks port availability before assigning
   - If orphaned processes hold ports, allocation skips those IPs
   - Implemented in `state/manager.rs::is_port_available()`

3. **Disk cleanup**: VM data directories are cleaned up on exit
   - `podman.rs` and `snapshot.rs` both delete `data_dir` on VM exit
   - Prevents disk from filling up with leftover VM directories

4. **State file cleanup**: State files are deleted when VMs exit
   - Prevents stale state from affecting IP allocation

**If tests fail in parallel but pass alone:**
- It's a resource isolation bug - FIX IT
- Check for shared state (files, ports, IPs, network namespaces)
- Add unique naming or proper cleanup

### Build and Test Rules

**CRITICAL: NEVER use `sudo cargo` or `sudo cargo test`. ALWAYS use Makefile targets.**

The Makefile uses `CARGO_TARGET_*_RUNNER='sudo -E'` to run test **binaries** with sudo, not cargo itself. Using `sudo cargo` creates root-owned files in `target/` that break subsequent non-sudo builds.

```bash
# CORRECT - always use make
make build       # Build fcvm + fc-agent (no sudo)
make test-unit   # Unit tests only, no sudo
make test-fast   # + quick VM tests, no sudo (rootless only)
make test-all    # + slow VM tests, no sudo (rootless only)
make test-root   # + privileged tests (bridged, pjdfstest), uses sudo runner
make test        # Alias for test-root

# WRONG - never do this
sudo cargo build ...        # Creates root-owned target/, breaks everything
sudo cargo test ...         # Same problem
cargo test -p fcvm ...      # Missing feature flags, setup
```

**Test tiers (additive):**
| Target | Features | Sudo | Tests |
|--------|----------|------|-------|
| test-unit | none | no | lint, cli, state manager |
| test-fast | integration-fast | no | + quick VM (rootless) |
| test-all | + integration-slow | no | + slow VM (rootless) |
| test-root | + privileged-tests | yes | + bridged, pjdfstest |

**Feature flags**: `privileged-tests` gates bridged networking tests and pjdfstest. Rootless tests compile without it. Use `FILTER=` to filter by name pattern.

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

### Monitoring Long-Running Tests

When tailing logs, check every **20 seconds** (not 5, not 60):
```bash
# Good - check every 20 seconds
sleep 20 && tail -20 /tmp/test.log

# Bad - too frequent (wastes API calls)
sleep 5 && ...

# Bad - too slow (miss important output)
```

### Preserving Logs from Failed Tests

**When a test fails, IMMEDIATELY save the log to a uniquely-named file for diagnosis:**

```bash
# Pattern: /tmp/fcvm-failed-{test_name}-{timestamp}.log
# Example after test_exec_rootless fails:
cp /tmp/test.log /tmp/fcvm-failed-test_exec_rootless-$(date +%Y%m%d-%H%M%S).log

# Then continue with other tests using a fresh log file
make test-root 2>&1 | tee /tmp/test-run2.log
```

**Why this matters:**
- Test logs get overwritten when running the suite again
- Failed test output is essential for root cause analysis
- Timestamps prevent filename collisions across sessions

**Automated approach:**
```bash
# After a test suite run, check for failures and save logs
if grep -q "FAIL\|TIMEOUT" /tmp/test.log; then
  cp /tmp/test.log /tmp/fcvm-failed-$(date +%Y%m%d-%H%M%S).log
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

### POSIX Compliance (pjdfstest)

All 8789 pjdfstest tests pass. These are gated by `#[cfg(feature = "privileged-tests")]` and run as part of `make test-root` or `make container-test-root`.

## CI and Testing Philosophy

**Use the Makefile.** All test commands are defined there. Never reimplement `podman run` commands - use the existing targets.

### Key Makefile Targets

| Target | What |
|--------|------|
| `make test` | All tests (rootless + root) |
| `make test-rootless` | Rootless tests only |
| `make test-root` | Root tests (requires sudo + KVM) |
| `make test-root FILTER=exec` | Only exec tests |
| `make container-test` | All tests in container |

### Path Overrides for CI

Makefile paths can be overridden via environment:
```bash
export FUSE_BACKEND_RS=/path/to/fuse-backend-rs
export FUSER=/path/to/fuser
make container-test
```

### CI Structure

**PR/Push (7 parallel jobs):**
- Lint, Build, Unit Tests, FUSE Integration, CLI Tests, FUSE Permissions, POSIX Compliance

**Nightly (scheduled):**
- Full benchmarks with artifact upload

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

### Test Integration

Tests spawn processes and track PIDs directly (no stdout parsing needed):

```rust
// 1. Start baseline VM
let baseline_proc = Command::new("sudo")
    .args(["fcvm", "podman", "run", ...])
    .spawn()?;
let baseline_pid = baseline_proc.id();  // fcvm process PID

// 2. Wait for healthy
poll_health_by_pid(baseline_pid).await?;

// 3. Create snapshot
Command::new("sudo")
    .args(["fcvm", "snapshot", "create", "--pid", &baseline_pid.to_string()])
    .status()?;

// 4. Start serve
let serve_proc = Command::new("sudo")
    .args(["fcvm", "snapshot", "serve", "my-snap"])
    .spawn()?;
let serve_pid = serve_proc.id();

// 5. Clone
let clone_proc = Command::new("sudo")
    .args(["fcvm", "snapshot", "run", "--pid", &serve_pid.to_string()])
    .spawn()?;

// 6. Wait for clone healthy
poll_health_by_pid(clone_proc.id()).await?;
```

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
├── common/mod.rs           # Shared test utilities (VmFixture, poll_health_by_pid)
├── test_sanity.rs          # End-to-end VM sanity tests (rootless + bridged)
├── test_state_manager.rs   # State manager unit tests
├── test_health_monitor.rs  # Health monitoring tests
├── test_fuse_posix.rs      # FUSE POSIX compliance in VM
├── test_fuse_in_vm.rs      # FUSE integration in VM
├── test_localhost_image.rs # Local image tests
└── test_snapshot_clone.rs  # Snapshot/clone workflow tests

fuse-pipe/tests/
├── integration.rs              # Basic FUSE operations (no root)
├── integration_root.rs         # FUSE operations requiring root
├── test_permission_edge_cases.rs # Permission/setattr edge cases
├── test_mount_stress.rs        # Mount/unmount stress tests
├── test_allow_other.rs         # AllowOther flag tests
├── test_unmount_race.rs        # Unmount race condition tests
├── pjdfstest_matrix.rs         # POSIX compliance (17 categories, parallel via nextest)
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
   - `--network bridged` (default): Network namespace + iptables, requires root
   - `--network rootless`: slirp4netns, no root required
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
| Bridged | `--network bridged` | Yes | Better | iptables DNAT |
| Rootless | `--network rootless` | No | Good | slirp4netns API |

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
The rootfs is named after the SHA of the setup script + kernel URL. This ensures automatic cache invalidation when:
- The init logic, install script, or setup script changes
- The kernel URL changes (different kernel version)

The initrd contains a statically-linked busybox and fc-agent binary, injected at boot before systemd.

```rust
// src/storage/disk.rs - create_cow_disk()
tokio::process::Command::new("cp")
    .arg("--reflink=always")
    .arg(&self.base_rootfs)
    .arg(&overlay_path)
```

```rust
// src/paths.rs
pub fn base_dir() -> PathBuf {
    PathBuf::from("/mnt/fcvm-btrfs")
}

pub fn vm_runtime_dir(vm_id: &str) -> PathBuf {
    base_dir().join("vm-disks").join(vm_id)
}
```

**Setup**: Run `make setup-fcvm` before tests (called automatically by `make test-root` or `make container-test-root`).

**⚠️ CRITICAL: Changing VM base image (fc-agent, rootfs)**

When you change fc-agent or setup scripts, regenerate the rootfs:
1. Delete existing rootfs: `sudo rm -f /mnt/fcvm-btrfs/rootfs/layer2-*.raw /mnt/fcvm-btrfs/initrd/fc-agent-*.initrd`
2. Run setup: `make setup-fcvm`

The rootfs is cached by SHA of setup script + kernel URL. Changes to these automatically invalidate the cache.

NEVER manually edit rootfs files. The setup script in `rootfs-plan.toml` and `src/setup/rootfs.rs` control what gets installed.

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

### Makefile Targets

Run `make help` for full list. Key targets:

#### Development
| Target | Description |
|--------|-------------|
| `make build` | Build fcvm + fc-agent |
| `make clean` | Clean build artifacts |

#### Testing
| Target | Description |
|--------|-------------|
| `make test` | All tests (rootless + root) |
| `make test-rootless` | Rootless tests only |
| `make test-root` | Root tests (requires sudo + KVM) |
| `make test-root FILTER=exec` | Only exec tests |
| `make container-test` | All tests in container |
| `make container-shell` | Interactive shell |

#### Linting
| Target | Description |
|--------|-------------|
| `make lint` | Run clippy + fmt-check |
| `make clippy` | Run cargo clippy |
| `make fmt` | Format code |
| `make fmt-check` | Check formatting |

#### Benchmarks
| Target | Description |
|--------|-------------|
| `make bench` | All benchmarks (throughput + operations + protocol) |
| `make bench-throughput` | I/O throughput benchmarks |
| `make bench-operations` | FUSE operation latency benchmarks |
| `make bench-protocol` | Wire protocol benchmarks |
| `make bench-quick` | Quick benchmarks (faster iteration) |
| `make bench-logs` | View recent benchmark logs/telemetry |
| `make bench-clean` | Clean benchmark artifacts |

#### Setup (idempotent, run automatically by tests)
| Target | Description |
|--------|-------------|
| `make setup-btrfs` | Create btrfs loopback |
| `make setup-fcvm` | Download kernel and create rootfs (runs `fcvm setup`) |

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
1. Downloads Kata kernel from URL in `rootfs-plan.toml` (~15MB, cached by URL hash)
2. Creates Layer 2 rootfs (~10GB, downloads Ubuntu cloud image, boots VM to install packages)
3. Creates fc-agent initrd (embeds statically-linked fc-agent binary)

**Kernel source**: Kata Containers kernel (6.12.47 from Kata 3.24.0 release) with `CONFIG_FUSE_FS=y` built-in.

### Data Layout
```
/mnt/fcvm-btrfs/           # btrfs filesystem (CoW reflinks work here)
├── kernels/
│   ├── vmlinux.bin        # Symlink to active kernel
│   └── vmlinux-{sha}.bin  # Kernel files (SHA of URL for cache key)
├── rootfs/
│   └── layer2-{sha}.raw   # Base Ubuntu + Podman image (~10GB, SHA of setup script)
├── initrd/
│   └── fc-agent-{sha}.initrd  # fc-agent injection initrd (SHA of binary)
├── vm-disks/
│   └── vm-{id}/
│       └── disks/rootfs.raw   # CoW reflink copy per VM
├── snapshots/             # Firecracker snapshots
├── state/                 # VM state JSON files
└── cache/                 # Downloaded cloud images
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

## fuse-pipe Testing

**Quick reference**: See `make help` for all targets.

### Quick Reference

| Command | Description |
|---------|-------------|
| `make container-test` | All tests in container |
| `make container-test-rootless` | Rootless tests in container |
| `make container-test-root` | Root tests in container |
| `make container-shell` | Interactive shell |

### Tracing Targets

| Target | Component |
|--------|-----------|
| `fuse_pipe::fixture` | Test fixture setup/teardown |
| `fuse-pipe::server` | Async server |
| `fuse-pipe::client` | FUSE client, multiplexer |
| `passthrough` | PassthroughFs operations |

### Running Tests with Tracing

```bash
# All components at info level, passthrough at debug
RUST_LOG="fuse_pipe=info,fuse-pipe=info,passthrough=debug" sudo -E cargo test --release -p fuse-pipe --test integration -- --nocapture

# Just passthrough operations
RUST_LOG="passthrough=debug" sudo -E cargo test --release -p fuse-pipe --test integration test_list_directory -- --nocapture
```

## References
- Main documentation: `README.md`
- Design specification: `DESIGN.md`
- Firecracker docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
