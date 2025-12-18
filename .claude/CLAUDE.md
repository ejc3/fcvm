# fcvm Development Log

## Overview
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

## Quick Reference

### Common Commands
```bash
# Build
make build        # Build fcvm + fc-agent
make test         # Run fuse-pipe tests
make rebuild      # Full rebuild including rootfs update

# Run a VM
sudo fcvm podman run --name my-vm --network bridged nginx:alpine

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

**NO LEGACY/BACKWARD COMPATIBILITY in our own implementation.** When we change an API, we update all callers. No deprecated functions, no compatibility shims, no `_old` suffixes. Clean breaks only.

Exception: For **forked libraries** (like fuse-backend-rs), we maintain compatibility with upstream to enable merging upstream changes.

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

**Common parallel test pitfalls and fixes:**

1. **Unique resource names**: Use `unique_names()` helper to generate timestamp+counter-based names
   ```rust
   let (baseline, clone, snapshot, serve) = unique_names("mytest");
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

**Use Makefile targets for common operations:**

```bash
# Correct - always use make
make build              # Build fcvm + fc-agent
make test               # Run fuse-pipe tests
make test-vm            # Run VM tests
make test-vm-rootless   # Run rootless VM test only
make container-test     # Run tests in container
make clean              # Clean build artifacts
```

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
sleep 60 && ...
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

### Container Testing for Full POSIX Compliance

All 8789 pjdfstest tests pass when running in a container with proper device cgroup rules. Use `make container-test-pjdfstest` for the full POSIX compliance test.

**Why containers work better**: The container runs with `sudo podman` and `--device-cgroup-rule` flags that allow mknod for block/char devices.

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
├── pjdfstest_full.rs           # Full POSIX compliance (8789 tests)
├── pjdfstest_fast.rs           # Fast POSIX subset
├── pjdfstest_stress.rs         # Parallel POSIX stress
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
- Base rootfs: `/mnt/fcvm-btrfs/rootfs/base.ext4` (~1GB Ubuntu 24.04 + Podman)
- VM disks: `/mnt/fcvm-btrfs/vm-disks/{vm_id}/disks/rootfs.ext4`

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

**Setup**: Automatic via `make test-vm` or `make container-test-vm` (idempotent btrfs loopback + kernel copy).

**⚠️ CRITICAL: Changing VM base image (fc-agent, rootfs)**

ALWAYS use Makefile commands to update the VM base:
- `make rebuild` - Rebuild fc-agent and update rootfs
- `make rootfs` - Update fc-agent in existing rootfs only

NEVER manually edit `/mnt/fcvm-btrfs/rootfs/base.ext4` or mount it directly. The Makefile handles mount/unmount correctly and ensures proper cleanup.

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
| `make test` | Run fuse-pipe tests: noroot + root |
| `make test-noroot` | Tests without root: unit + integration + stress |
| `make test-root` | Tests requiring root: integration_root + permission |
| `make test-unit` | Unit tests only |
| `make test-fuse` | All fuse-pipe tests explicitly |
| `make test-vm` | Run VM tests: rootless + bridged |
| `make test-vm-rootless` | VM test with slirp4netns (no root) |
| `make test-vm-bridged` | VM test with bridged networking |
| `make test-pjdfstest` | POSIX compliance (8789 tests) |
| `make test-all` | Everything: test + test-vm + test-pjdfstest |
| `make container-test` | Run fuse-pipe tests (in container) |
| `make container-test-vm` | Run VM tests (in container) |
| `make container-test-pjdfstest` | POSIX compliance in container |
| `make container-shell` | Interactive shell in container |

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
| `make setup-all` | Full setup: btrfs + kernel + rootfs |
| `make setup-btrfs` | Create btrfs loopback |
| `make setup-kernel` | Copy kernel to btrfs |
| `make setup-rootfs` | Create base rootfs (~90 sec first run) |

#### Rootfs Updates
| Target | Description |
|--------|-------------|
| `make rootfs` | Update fc-agent in existing rootfs |
| `make rebuild` | Build + update rootfs |

### How Setup Works

**What Makefile does (prerequisites):**
1. `setup-btrfs` - Creates 20GB btrfs loopback at `/mnt/fcvm-btrfs`
2. `setup-kernel` - Copies pre-built kernel from `~/linux-firecracker/arch/arm64/boot/Image`

**What fcvm binary does (auto on first VM start):**
1. `ensure_kernel()` - Checks for `/mnt/fcvm-btrfs/kernels/vmlinux.bin` (already copied by Makefile)
2. `ensure_rootfs()` - If missing, downloads Ubuntu 24.04 cloud image (~590MB), customizes with virt-customize, installs podman/crun/etc, embeds fc-agent binary (~90 sec)

### Data Layout
```
/mnt/fcvm-btrfs/           # btrfs filesystem (CoW reflinks work here)
├── kernels/
│   └── vmlinux.bin        # Firecracker kernel
├── rootfs/
│   └── base.ext4          # Base Ubuntu + Podman image (~10GB)
├── vm-disks/
│   └── vm-{id}/
│       └── rootfs.ext4    # CoW reflink copy per VM
├── snapshots/             # Firecracker snapshots
├── state/                 # VM state JSON files
└── cache/                 # Downloaded cloud images
```

### One-Time Setup (dnsmasq)

```bash
sudo apt-get update
sudo apt-get install -y dnsmasq

# dnsmasq for DNS forwarding to VMs (bind-dynamic listens on dynamically created TAP devices)
sudo tee /etc/dnsmasq.d/fcvm.conf > /dev/null <<EOF
bind-dynamic
server=8.8.8.8
server=8.8.4.4
no-resolv
cache-size=1000
EOF
sudo systemctl restart dnsmasq
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
- Problem: Container image pulls failing with DNS timeout
- Root cause: VMs configured to use 8.8.8.8 but NAT wasn't forwarding DNS properly
- Fix: Install dnsmasq on host with `bind-dynamic` to listen on TAP devices

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

**Quick reference**: See `README.md` for testing guide and Makefile targets.

### Quick Reference (Container - Recommended)

| Command | Description |
|---------|-------------|
| `make container-test` | Run all fuse-pipe tests |
| `make container-test-vm` | Run fcvm VM tests (rootless + bridged) |
| `make container-test-pjdfstest` | POSIX compliance (8789 tests) |
| `make container-shell` | Interactive shell for debugging |

### Quick Reference (Native)

| Command | Description |
|---------|-------------|
| `sudo cargo test --release -p fuse-pipe --test integration` | Basic FUSE ops (15 tests) |
| `sudo cargo test --release -p fuse-pipe --test test_permission_edge_cases` | Permission tests (18 tests) |
| `sudo cargo test --release -p fuse-pipe --test pjdfstest_full` | POSIX compliance (8789 tests) |
| `sudo cargo test --release -p fuse-pipe --test pjdfstest_stress` | Parallel stress (85 jobs) |
| `sudo cargo bench -p fuse-pipe --bench throughput` | I/O benchmarks |

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
