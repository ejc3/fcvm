# fcvm Development Log

## Overview
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

## Quick Reference

### EC2 Test Instance
- **Instance**: c6g.metal (ARM64 bare metal with KVM)
- **Instance ID**: i-05fafabbe2e064949
- **IP**: 54.67.60.104
- **SSH**: `ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104`
- **Firecracker**: v1.10.0
- **WARNING**: Do NOT use c5.large instances - no /dev/kvm support

### Common Commands
```bash
# Build and deploy
make sync         # Sync fcvm + fuse-backend-rs to EC2 (ALWAYS use this, not git)
make build        # Sync + build fcvm + fc-agent on EC2
make test         # Run sanity test
make rebuild      # Full rebuild including rootfs update

# Run a VM
sudo fcvm podman run --name my-vm --network bridged nginx:alpine

# Snapshot workflow
fcvm snapshot create --pid <vm_pid> --tag my-snapshot
fcvm snapshot serve my-snapshot      # Start UFFD server (prints serve PID)
fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
```

### Code Philosophy

**NO LEGACY/BACKWARD COMPATIBILITY in our own implementation.** When we change an API, we update all callers. No deprecated functions, no compatibility shims, no `_old` suffixes. Clean breaks only.

Exception: For **forked libraries** (like fuse-backend-rs), we maintain compatibility with upstream to enable merging upstream changes.

### Code Sync Strategy

**ALWAYS use `make sync` to sync code to EC2.** Never use git pull on EC2.

The Makefile syncs both:
1. `fcvm` - the main project
2. `fuse-backend-rs` - the FUSE backend library (local fork at `../fuse-backend-rs`)

The `fuse-pipe/Cargo.toml` uses a local path dependency:
```toml
fuse-backend-rs = { path = "../../fuse-backend-rs", ... }
```

This ensures changes to fuse-backend-rs are immediately available without git commits.

### Monitoring Long-Running Tests

When tailing logs from EC2, check every **20 seconds** (not 5, not 60):
```bash
# Good - check every 20 seconds
sleep 20 && ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104 "tail -20 /tmp/test.log"

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

**Why containers work better**: The container runs with `sudo podman` and `--device-cgroup-rule` flags that allow mknod for block/char devices. Native EC2 testing may have permission issues with supplementary groups and device node creation.

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
├── types.rs          # Core shared types (Mode, MapMode)
├── lib.rs            # Module exports (public API)
├── main.rs           # CLI dispatcher
├── cli/              # Command-line parsing
│   ├── args.rs       # Clap structures
│   └── types.rs      # Type conversions
├── commands/         # Command implementations
├── state/            # VM state management
│   ├── types.rs      # VmState, VmStatus, VmConfig
│   ├── manager.rs    # StateManager (CRUD)
│   └── utils.rs      # generate_vm_id()
├── firecracker/      # Firecracker API client
├── network/          # Networking layer
│   ├── slirp.rs      # SlirpNetwork (rootless)
│   └── bridged.rs    # BridgedNetwork
├── storage/          # Disk/snapshot management
├── health.rs         # Health monitoring
├── uffd/             # UFFD memory sharing
└── setup/            # Setup subcommands

tests/
├── common/mod.rs     # Shared test utilities
├── test_sanity.rs    # End-to-end sanity tests
└── test_state_manager.rs
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

**Setup**: Automatic via `make test-sanity` or `make container-test-fcvm-sanity` (idempotent btrfs loopback + kernel copy).

**⚠️ CRITICAL: Changing VM base image (fc-agent, rootfs)**

ALWAYS use Makefile commands to update the VM base:
- `make rebuild` - Rebuild fc-agent and update rootfs
- `make rootfs` - Update fc-agent in existing rootfs only

NEVER manually edit `/mnt/fcvm-btrfs/rootfs/base.ext4` or mount it directly. The Makefile handles mount/unmount correctly and ensures proper cleanup.

### Memory Sharing (UFFD)

**Two-command workflow:**
```bash
fcvm memory-server nginx-base    # Start server, creates /tmp/fcvm/uffd-nginx-base.sock
fcvm clone --snapshot nginx-base --name web1  # Connects to server
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

**Benchmark**: c6g.metal, 256 workers, 1024 files × 4KB

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

### Makefile Targets (from local macOS)

Run `make help` for full list. Key targets:

#### Development
| Target | Description |
|--------|-------------|
| `make build` | Sync + build fcvm + fc-agent on EC2 |
| `make sync` | Just sync code (no build) |
| `make clean` | Clean build artifacts |

#### Testing
| Target | Description |
|--------|-------------|
| `make test` | Run all tests (native on EC2) |
| `make test-sanity` | Run VM sanity test (native on EC2) |
| `make container-test` | Run fuse-pipe tests (in container) |
| `make container-test-fcvm` | Run fcvm VM tests (in container) |
| `make container-shell` | Interactive shell in container |

#### Setup (idempotent, run automatically by tests)
| Target | Description |
|--------|-------------|
| `make setup-all` | Full setup (btrfs + kernel + rootfs) |
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

### One-Time EC2 Setup (dnsmasq)

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
- Firecracker REQUIRES `/dev/kvm` - only available on bare metal instances
- c6g.metal works ($2.18/hr, 64 vCPUs ARM64)
- c5.metal works ($4.08/hr, 96 vCPUs x86_64)
- c5.large does NOT work (no nested virtualization)
- AWS vCPU limit increase needed for metal instances (64+ vCPUs)

### DNS Resolution in VMs
- Problem: Container image pulls failing with DNS timeout
- Root cause: VMs configured to use 8.8.8.8 but NAT wasn't forwarding DNS properly
- Fix: Install dnsmasq on host with `bind-dynamic` to listen on TAP devices

## fuse-pipe Testing

**Full documentation**: See `fuse-pipe/TESTING.md` for complete testing guide.

### Quick Reference (Container - Recommended)

| Command | Description |
|---------|-------------|
| `make container-test` | Run all fuse-pipe tests |
| `make container-test-fcvm` | Run fcvm VM sanity tests |
| `make container-shell` | Interactive shell for debugging |

### Quick Reference (Native EC2)

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
- Design doc: `/Users/ejcampbell/src/fcvm/DESIGN.md`
- Firecracker docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
