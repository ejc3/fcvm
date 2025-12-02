# fcvm Development Log

## Overview
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

## PID-Based Process Management (2025-11-14)

**Core Principle:** All fcvm processes store their own PID (via `std::process::id()`), not child process PIDs.

### Process Types

fcvm tracks three types of managed processes:

1. **VM processes** (`fcvm podman run`)
   - `process_type`: "vm"
   - Runs Firecracker + container
   - Health check: HTTP to guest

2. **Serve processes** (`fcvm snapshot serve`)
   - `process_type`: "serve"
   - Runs UFFD memory server
   - Health check: process existence
   - Tracks which clones connected via `serve_pid` field

3. **Clone processes** (`fcvm snapshot run`)
   - `process_type`: "clone"
   - Runs Firecracker with UFFD memory
   - Health check: HTTP to guest
   - References parent serve via `serve_pid` field

### Command Workflows

#### Run a VM
```bash
# Start baseline VM
fcvm podman run --name my-vm nginx:alpine

# Track via PID (fcvm process PID, not Firecracker PID)
fcvm ls --pid 12345
```

#### Create and Serve Snapshot
```bash
# Create snapshot from running VM (by PID or name)
fcvm snapshot create --pid 12345 --tag my-snapshot

# Start serve process (saves state, prints PID)
fcvm snapshot serve my-snapshot
# Output: Serve PID: 67890
# Socket: /mnt/fcvm-btrfs/uffd-my-snapshot-67890.sock

# List all serve processes
fcvm snapshot ls
# Shows: SERVE_ID, PID, HEALTH, SNAPSHOT, CLONES
```

#### Clone from Serve
```bash
# Clone using serve PID (not snapshot name!)
fcvm snapshot run --pid 67890 --name clone1
fcvm snapshot run --pid 67890 --name clone2

# Clones automatically track parent serve
# On serve exit, all clones are automatically killed
```

### State Management

**VmConfig fields:**
```rust
pub struct VmConfig {
    pub snapshot_name: Option<String>,  // Which snapshot
    pub process_type: Option<String>,   // "vm" | "serve" | "clone"
    pub serve_pid: Option<u32>,         // For clones: parent serve PID
    // ... other fields
}
```

**VmState fields:**
```rust
pub struct VmState {
    pub pid: Option<u32>,  // fcvm process PID (from std::process::id())
    // ... other fields
}
```

### Cleanup Architecture

**Serve process cleanup (on SIGTERM/SIGINT):**
1. Query state manager for all VMs where `serve_pid == my_pid`
2. Kill each clone process: `kill -TERM <clone_pid>`
3. Remove socket file: `/mnt/fcvm-btrfs/uffd-{snapshot}-{pid}.sock`
4. Delete serve state from state manager

**Benefits:**
- No orphaned clones when serve exits
- Explicit process ownership model
- Tests track processes via PIDs (no stdout parsing)
- Multiple serves per snapshot supported

### Test Integration

Tests spawn processes and track PIDs directly:

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
let clone_pid = clone_proc.id();

// 6. Wait for clone healthy
poll_health_by_pid(clone_pid).await?;
```

**No stdout/stderr parsing needed** - PIDs are known from process spawning!

## Architecture Decisions

### Project Structure - A+ Rust Pattern
fcvm follows industry-standard Rust CLI architecture (similar to ripgrep, fd, bat):
- **Library + Binary pattern**: src/lib.rs exports all modules, src/main.rs is thin dispatcher
- **Modular commands**: Each command in separate file under src/commands/
- **Clear separation**: CLI parsing (cli/), business logic (commands/), types (types.rs)
- **Comprehensive testing**: Unit tests in modules, integration tests in tests/

### Directory Layout
```
src/
├── types.rs          # Core shared types (Mode, MapMode)
├── lib.rs            # Module exports (public API)
├── main.rs           # 38-line CLI dispatcher
├── cli/              # Command-line parsing
│   ├── args.rs       # Clap structures
│   ├── types.rs      # Type conversions
│   └── mod.rs
├── commands/         # Command implementations
│   ├── run.rs        # fcvm run
│   ├── setup.rs      # fcvm setup
│   ├── ls.rs         # fcvm ls
│   └── mod.rs
├── state/            # VM state management
│   ├── types.rs      # VmState, VmStatus, VmConfig
│   ├── manager.rs    # StateManager (CRUD)
│   ├── utils.rs      # generate_vm_id()
│   └── mod.rs
├── firecracker/      # Firecracker API client
├── network/          # Networking layer
├── storage/          # Disk/snapshot management
├── readiness/        # Readiness gates
└── setup/            # Setup subcommands

tests/
├── common/mod.rs     # Shared test utilities
└── test_cli_parsing.rs  # Integration tests
```

### Design Principles
1. **One file per command**: Easy to find, easy to test
2. **No business logic in main.rs**: Just CLI parsing and dispatch
3. **Module re-exports**: Clean public API via lib.rs
4. **Unit tests in modules**: #[cfg(test)] blocks alongside code
5. **Integration tests separate**: tests/ directory for end-to-end scenarios

### Single Binary Design
- Main binary: `fcvm` with subcommands for all operations
- Guest agent: `fc-agent` (separate binary, runs inside VMs)
- NO standalone scripts - everything through `fcvm` subcommands

### Subcommand Structure
```
fcvm run <image>            # Run container in new VM
fcvm clone <vm-id>          # Clone from snapshot
fcvm stop <vm-id>           # Stop running VM
fcvm ls                     # List VMs
fcvm inspect <vm-id>        # Show VM details
fcvm logs <vm-id>           # Stream VM logs
fcvm top <vm-id>            # Show resource usage
fcvm setup kernel           # Download/build kernel
fcvm setup rootfs           # Create base rootfs image
fcvm setup preflight        # Check system requirements
fcvm memory-server <name>   # Start memory server for snapshot (enables sharing)
```

## Implementation Status

### ✅ Completed
1. **Core Implementation** (2025-11-09)
   - Firecracker API client using hyper + hyperlocal (Unix sockets)
   - Dual networking modes: rootless (slirp4netns) + privileged (nftables)
   - Storage layer with CoW disk management
   - Snapshot save/load/list functionality
   - VM state persistence
   - Process lifetime binding with tokio signal handlers
   - Guest agent (fc-agent) with MMDS integration

2. **Build System** (2025-11-09)
   - Successfully compiled on x86_64 Linux (Ubuntu 24.04)
   - Release build time: ~2 minutes for fcvm, ~1.5 minutes for fc-agent
   - Dependencies: 180+ crates, all resolving correctly

3. **Test Infrastructure** (2025-11-11)
   - EC2 c6g.metal instance (ARM64 bare metal with KVM)
   - Instance ID: i-05fafabbe2e064949
   - Public IP: 54.67.60.104
   - Firecracker v1.10.0 installed
   - SSH: `ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104`
   - ⚠️ OLD c5.large (54.176.90.249) does NOT have KVM - do not use!

4. **A+ Rust Refactor** (2025-11-09)
   - Restructured to industry-standard CLI pattern
   - Extracted commands to src/commands/ (8 files)
   - Split CLI into modular structure (cli/args.rs, cli/types.rs)
   - Restructured state into state/ module with unit tests
   - Reduced main.rs from 302 to 38 lines
   - Added integration test infrastructure (tests/)
   - All tests passing (24 total: 14 unit + 10 integration)

5. **Memory Sharing Architecture** (2025-11-09)
   - Embedded async UFFD server in main binary (no subprocess)
   - Two-command design: `fcvm memory-server <snapshot>` + `fcvm clone`
   - One server per snapshot, serves multiple VMs asynchronously
   - Uses tokio::select! for concurrent VM connections
   - Each VM gets async task handling page faults
   - Shared Arc<Mmap> for memory file across all VMs
   - Server auto-exits when last VM disconnects
   - Build successful on EC2: 32 seconds for release build

6. **VM Boot Success on c6g.metal** (2025-11-10)
   - Instance: c6g.metal (ARM64, 64 cores, $2.18/hr)
   - AWS vCPU quota increased from 16 to 128 (approved instantly)
   - ✅ Alpine Linux 3.19 boots successfully to login prompt
   - ✅ ARM64 kernel (4.14.174+) from Firecracker S3
   - ✅ Serial console fix: ttyS0 enabled in /etc/inittab
   - ✅ Network configured with slirp4netns (rootless mode)
   - ✅ CoW disk working (624ms copy time for 1GB rootfs)
   - Boot time: ~500ms from VM start to login prompt

7. **Snapshot/Clone Workflow COMPLETE** (2025-11-11)
   - ✅ Snapshot creation with disk copy (src/commands/snapshot.rs:77-91)
   - ✅ UFFD memory server serving multiple VMs concurrently
   - ✅ Network overrides API fixed (Vec<NetworkOverride> type)
   - ✅ Disk path symlink strategy (handles hardcoded vmstate paths)
   - ✅ Clones successfully start with unique TAP devices
   - ✅ Independent CoW disk overlays per clone
   - ✅ Memory sharing via UFFD working (3+ VMs tested)
   - ✅ VMs stay running without exit code issues
   - **Infrastructure fully operational** - snapshot/clone mechanism works

8. **End-to-End Container Execution** (2025-11-11)
   - ✅ fc-agent reads MMDS and executes container plans
   - ✅ DNS resolution working via dnsmasq forwarder
   - ✅ Container images pull from Docker Hub
   - ✅ nginx:alpine successfully starts with 2 worker processes
   - ✅ Complete workflow: `fcvm podman run` → VM boots → fc-agent pulls image → container runs
   - **Production Ready**: Full container orchestration working

9. **Snapshot/Clone Workflow COMPLETE** (2025-11-11)
   - ✅ Snapshot creation with VM resume fix (src/commands/snapshot.rs:120-127)
   - ✅ Original VM properly resumes after snapshotting and continues serving traffic
   - ✅ UFFD memory server serving multiple VMs concurrently
   - ✅ Multiple clones sharing 512 MB memory via UFFD (7000+ page faults served)

10. **btrfs CoW Reflinks** (2025-11-12)
   - ✅ Replaced fs::copy() with `cp --reflink=always` for instant disk cloning
   - ✅ Centralized paths module (src/paths.rs) to use btrfs mount
   - ✅ All data stored under `/mnt/fcvm-btrfs/` for reflink support
   - ✅ Disk copy time: **~1.5ms** (560x faster than 840ms standard copy!)
   - ✅ True CoW at block level - shared blocks until write occurs
   - ✅ Multiple VMs share same base rootfs (1GB base.ext4 shared by all VMs = 1GB on disk)
   - **Performance**: Instant VM creation with minimal disk usage

11. **Rootless Networking with Unique Subnets** (2025-11-12)
   - ✅ Each VM gets unique /30 subnet via hash of vm_id (172.16.0.0-63.0/30)
   - ✅ Eliminates routing conflicts between VMs
   - ✅ Kernel cmdline network configuration via `ip=` boot parameter
   - ✅ Static IP assignment: guest receives .202, host uses .201 as gateway
   - ✅ DNS resolution via dnsmasq on host (bind-dynamic for TAP devices)
   - ✅ Full end-to-end connectivity: VM boots → DNS works → containers pull images
   - **Example**: VM gets 172.16.0.200/30 (host: .201, guest: .202)

12. **Complete Snapshot/Clone Workflow Verified** (2025-11-12)
   - ✅ Snapshot creation: Pause VM → Create Firecracker snapshot → Resume VM
   - ✅ Memory snapshot: 512MB saved to `/mnt/fcvm-btrfs/snapshots/{name}/memory.bin`
   - ✅ Disk snapshot: CoW copy to `/mnt/fcvm-btrfs/snapshots/{name}/disk.ext4`
   - ✅ UFFD memory server: Serves pages on-demand via Unix socket
   - ✅ Clone with memory sharing: **2.3ms snapshot load time**
   - ✅ Clone disk uses btrfs reflink: **~3ms instant CoW copy**
   - ✅ Page fault handling: 3000+ pages served successfully
   - ✅ Multiple VMs share same 512MB memory via kernel page cache
   - ✅ Network isolation: Each clone gets unique /30 subnet
   - **Commands verified**:
     ```bash
     fcvm snapshot create <vm-name> --tag <snapshot-name>
     fcvm snapshot serve <snapshot-name>  # Start UFFD server
     fcvm snapshot run <snapshot-name> --name <clone-name> --mode rootless
     ```
   - **Performance**: Original VM + 2 clones = ~512MB RAM total (not 1.5GB!)

13. **Code Quality Cleanup** (2025-11-13)
   - ✅ Removed unimplemented stub commands (stop, logs, inspect, top)
   - ✅ Removed unimplemented readiness gates (vsock, log, exec)
   - ✅ Fixed duplicate imports (10x `use crate::paths;` → 1x clean import)
   - ✅ Replaced all `unreachable!()` with proper `anyhow::bail!()` errors
   - ✅ Eliminated all compiler warnings
   - ✅ Updated stress test script for self-contained lifecycle management
   - **Performance verification**: 10 VMs @ ~200ms clone time, 100% success rate

14. **Hierarchical Logging Architecture** (2025-11-15)
   - ✅ Added hierarchical target tags showing process nesting
   - ✅ Strip Firecracker timestamps and `[anonymous-instance:*]` prefixes
   - ✅ Clean log output when piped to files (no ANSI escape codes)
   - ✅ Smart color handling: parent uses colors for TTY only, subprocesses never use colors
   - ✅ Added `atty` dependency to detect when output is piped
   - **Logging hierarchy**:
     - `sanity-baseline-vm:` (test harness)
     - `sanity-baseline-vm: vm:` (VM manager)
     - `sanity-baseline-vm: firecracker:` (Firecracker process)
     - `sanity-baseline-vm: health-monitor:` (health checks)
   - **Result**: Production-ready logging that works in terminals and log files

15. **True Rootless Networking with slirp4netns** (2025-11-25)
   - ✅ Renamed `rootless.rs` → `bridged.rs` (was misleading, uses network namespaces)
   - ✅ Added `--network bridged|rootless` CLI flag
   - ✅ Implemented `SlirpNetwork` struct with slirp4netns integration
   - ✅ User namespace support via `unshare --user --map-root-user --net`
   - ✅ Added `post_start()` to `NetworkManager` trait for deferred slirp4netns startup
   - ✅ Unique loopback IPs (127.x.y.z) per VM for health checks
   - ✅ Port forwarding via slirp4netns JSON-RPC API socket
   - ✅ Health check architecture updated for rootless mode
   - ✅ Clone support for rootless VMs
   - **Key files**:
     - `src/network/slirp.rs` - SlirpNetwork implementation
     - `src/network/bridged.rs` - BridgedNetwork (renamed from rootless.rs)
     - `src/firecracker/vm.rs` - VmManager with user namespace support
     - `src/health.rs` - Dual health check modes (loopback vs veth)
   - **Usage**:
     ```bash
     # Rootless VM (no root required)
     fcvm podman run --network rootless nginx:alpine

     # Rootless clone
     fcvm snapshot run --pid <serve_pid> --network rootless
     ```

### 🚧 In Progress

None - all major features working!

### 📋 TODO
1. **Setup Subcommands**
   - `fcvm setup kernel` - Download/prepare vmlinux
   - `fcvm setup rootfs` - Create base rootfs with Podman
   - `fcvm setup preflight` - Validate system requirements

2. **Testing**
   - Test port mapping with 127.0.0.1 (localhost-only binding)
   - Test volume mounting

   **Sanity Test**:
   ```bash
   # Run sanity test (single VM, verify health checks work)
   sudo fcvm test sanity
   ```
   - Starts nginx:alpine VM
   - Verifies VM becomes healthy within 60s timeout
   - Tests networking, health checks, and cleanup
   - Exit code 0 = PASS

3. **Documentation**
   - Usage examples
   - Performance benchmarks
   - Troubleshooting guide

## Technical Findings

### Firecracker Requirements
- **Kernel**: Need vmlinux or bzImage (no modules required for basic operation)
  - Can extract from host: `/boot/vmlinuz-*`
  - Or download pre-built from Firecracker releases
  - Boot args: `console=ttyS0 reboot=k panic=1 pci=off`

- **Rootfs**: ext4 filesystem with:
  - Systemd (for init)
  - Podman + dependencies (conmon, crun, fuse-overlayfs)
  - Network tools (iproute2)
  - fc-agent installed at `/usr/local/bin/fc-agent`
  - fc-agent.service enabled

### Networking Notes

#### Network Modes (--network flag)

fcvm supports two networking modes:

- **`--network bridged`** (default): Linux bridge + network namespace + nftables
  - Requires root or CAP_NET_ADMIN
  - Better performance
  - Uses veth pairs and TAP devices
  - Port forwarding via nftables DNAT

- **`--network rootless`**: True rootless networking with slirp4netns
  - No root privileges required
  - Uses user namespace (`unshare --user --map-root-user --net`)
  - slirp4netns creates TAP device inside namespace
  - Guest IP: 10.0.2.15, Gateway: 10.0.2.2 (slirp4netns defaults)
  - Port forwarding via slirp4netns JSON-RPC API socket
  - Health checks use unique loopback IPs (127.x.y.z)

#### Rootless Networking Architecture

**Key Implementation Files:**
- `src/network/slirp.rs` - SlirpNetwork implementation
- `src/network/bridged.rs` - BridgedNetwork implementation
- `src/firecracker/vm.rs` - VmManager with user namespace support

**How slirp4netns Integration Works:**
1. Firecracker starts with `unshare --user --map-root-user --net`
2. This creates a new user namespace with root-mapped UID
3. slirp4netns connects to the namespace via PID
4. TAP device is created inside the namespace
5. Port forwarding via API socket (JSON-RPC)

**Health Check Architecture for Rootless:**
- Each rootless VM gets a unique loopback IP (127.x.y.z) derived from vm_id hash
- Port 80 is forwarded from loopback IP to guest 10.0.2.15:80
- Health monitor checks HTTP on loopback IP instead of guest IP + veth
- `NetworkConfig.loopback_ip` and `NetworkConfig.health_check_port` fields track this

**Loopback IP Generation:**
```rust
// src/network/slirp.rs - generate_loopback_ip()
// Hash vm_id to generate unique 127.x.y.z (avoiding 127.0.0.1)
fn generate_loopback_ip(vm_id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    vm_id.hash(&mut hasher);
    let hash = hasher.finish();

    let second = ((hash >> 0) & 0xFF) as u8;
    let third = ((hash >> 8) & 0xFF) as u8;
    let fourth = ((hash >> 16) & 0xFF) as u8;

    // Avoid 127.0.0.0 and 127.0.0.1
    let second = if second == 0 { 1 } else { second };
    let fourth = if second == 0 && third == 0 && fourth <= 1 { 2 } else { fourth };

    format!("127.{}.{}.{}", second, third, fourth)
}
```

### Storage Notes - btrfs CoW Reflinks

**Performance Achievement: ~1.5ms disk copy (560x faster than 840ms standard copy!)**

#### How It Works

fcvm uses **btrfs reflinks** for instant VM disk cloning with true copy-on-write:

**Architecture:**
- All fcvm data stored under `/mnt/fcvm-btrfs/` (btrfs filesystem)
- Base rootfs: `/mnt/fcvm-btrfs/rootfs/base.ext4` (~1GB Alpine + Podman)
- VM disks: `/mnt/fcvm-btrfs/vm-disks/{vm_id}/disks/rootfs.ext4`

**Disk Cloning Process:**
```rust
// src/storage/disk.rs - create_cow_disk()
tokio::process::Command::new("cp")
    .arg("--reflink=always")
    .arg(&self.base_rootfs)
    .arg(&overlay_path)
    .status()
    .await
```

**How reflinks work:**
- `cp --reflink=always` creates instant CoW copy on btrfs/xfs
- Only metadata is copied (~1.5ms), data blocks are shared
- When VM writes to disk, btrfs allocates new blocks (CoW)
- Multiple VMs share same base blocks until they write

**Key Benefits:**
- ✅ **Instant cloning**: ~1.5ms vs 840ms for full copy
- ✅ **Space efficient**: 50 VMs with 1GB rootfs = ~1GB on disk (plus deltas)
- ✅ **True CoW**: Block-level deduplication handled by filesystem
- ✅ **No runtime overhead**: Standard ext4 filesystem inside VM

**Path Centralization (src/paths.rs):**
```rust
pub fn base_dir() -> PathBuf {
    PathBuf::from("/mnt/fcvm-btrfs")  // All data on btrfs mount
}

pub fn vm_runtime_dir(vm_id: &str) -> PathBuf {
    base_dir().join("vm-disks").join(vm_id)
}
```

**Setup Requirements:**
```bash
# Create btrfs filesystem (already done on EC2)
sudo mkfs.btrfs /dev/nvme1n1
sudo mount /dev/nvme1n1 /mnt/fcvm-btrfs
sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,state,snapshots,vm-disks}
```

### FUSE Passthrough Performance (fuse-pipe)

**Benchmark Date**: 2025-12-01
**Machine**: c6g.metal (64 ARM cores, 125GB RAM)
**Test Setup**: 256 workers doing parallel file I/O, varying FUSE reader thread count

#### Parallel Reads (256 workers, 1024 files × 4KB)

| Readers | Time (ms) | vs Host | Speedup vs 1 Reader |
|---------|-----------|---------|---------------------|
| Host FS | 10.7 | 1.0x | - |
| 1 | 490.6 | 45.8x slower | 1.0x |
| 2 | 265.0 | 24.8x slower | 1.85x |
| 4 | 142.6 | 13.3x slower | 3.44x |
| 8 | 82.7 | 7.7x slower | 5.93x |
| 16 | 63.7 | 5.9x slower | 7.70x |
| 32 | 63.5 | 5.9x slower | 7.73x |
| 64 | 61.6 | 5.7x slower | 7.97x |
| 128 | 59.4 | 5.5x slower | 8.26x |
| **256** | **57.0** | **5.3x slower** | **8.61x** |
| 512 | 58.0 | 5.4x slower | 8.46x |
| 1024 | 58.0 | 5.4x slower | 8.46x |

**Key Finding**: Performance plateaus at 16 readers (~64ms), best at 256 readers (~57ms).

#### Parallel Writes (256 workers, 1024 files × 4KB, with sync_all)

| Readers | Time (s) | vs Host | Speedup vs 1 Reader |
|---------|----------|---------|---------------------|
| Host FS | 0.862 | 1.0x | - |
| 1 | 3.048 | 3.5x slower | 1.0x |
| 4 | 2.525 | 2.9x slower | 1.21x |
| 16 | 2.435 | 2.8x slower | 1.25x |
| 64 | 2.481 | 2.9x slower | 1.23x |
| **256** | **2.765** | **3.2x slower** | **1.10x** |
| 1024 | 2.955 | 3.4x slower | 1.03x |

**Key Finding**: Writes are disk I/O bound (sync_all). Performance peaks at 16 readers but 256 readers is still acceptable.

#### Recommendations

- **Default**: Use **256 readers** - good balance for mixed read/write workloads
- **FUSE Overhead**: ~5.3x for reads, ~3.2x for writes vs native filesystem
- **Scaling**: Read performance scales well with readers; write performance is bounded by disk I/O

### Memory Sharing Architecture
**Two-Command Workflow:**
1. **Start memory server** (one per snapshot, runs in foreground):
   ```bash
   fcvm memory-server nginx-base
   # Creates Unix socket at /tmp/fcvm/uffd-nginx-base.sock
   # Mmaps snapshot memory file (e.g., 512MB)
   # Waits for VM connections
   ```

2. **Clone VMs** (multiple VMs can clone from same snapshot):
   ```bash
   fcvm clone --snapshot nginx-base --name web1  # VM 1
   fcvm clone --snapshot nginx-base --name web2  # VM 2
   # Each connects to same memory server socket
   # Memory pages served on-demand via UFFD
   # True copy-on-write at 4KB page granularity
   ```

**How it works:**
- Memory server opens snapshot memory file and mmaps it (MAP_SHARED)
- Kernel automatically shares physical pages via page cache
- Server uses tokio AsyncFd to handle UFFD events non-blocking
- tokio::select! multiplexes: accept new VMs + monitor VM exits
- Each VM gets dedicated async task (JoinSet) for page faults
- All tasks share Arc<Mmap> reference to memory file
- Server exits gracefully when last VM disconnects

**Memory efficiency:**
- 50 VMs with 512MB snapshot = ~512MB physical RAM + small overhead
- NOT 50 × 512MB = 25.6GB!
- Linux kernel handles sharing via page cache automatically
- Pages only copied on write (true CoW at page level)

## Quick Start

### Prerequisites
- **CRITICAL**: Must use c6g.metal ARM instance (54.67.60.104) - has KVM support
- **DO NOT use** c5.large (54.176.90.249) - no /dev/kvm, will fail!

### Connect to ARM Instance
```bash
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104
```

### Start a VM
```bash
cd ~/fcvm
sudo ./target/release/fcvm podman run --name my-vm --mode rootless nginx:latest
```

### Test Snapshot/Clone Workflow
```bash
# 1. Start a VM
sudo ./target/release/fcvm podman run --name nginx-base --mode rootless nginx:latest

# 2. Create snapshot (in another terminal)
./target/release/fcvm snapshot create nginx-base --tag nginx-snap

# 3. Start memory server
./target/release/fcvm snapshot serve nginx-snap

# 4. Clone VMs (in another terminal)
./target/release/fcvm snapshot run nginx-snap --name clone1
./target/release/fcvm snapshot run nginx-snap --name clone2
```

## Build Instructions

### Using Makefile (Recommended)

The Makefile handles all builds on EC2 remotely. Run these commands from your **local macOS machine**:

```bash
# Standard development workflow:
make build        # Sync code to EC2 + build fcvm + build fc-agent (musl)
make test         # Run sanity test on EC2
make rebuild      # Full rebuild: sync + build + update rootfs with new fc-agent

# Individual targets:
make sync         # Just sync code to EC2 (no build)
make build-remote # Build on EC2 without syncing (use after manual changes)
make rootfs       # Update fc-agent in rootfs (mounts base.ext4, copies binary)
make deploy       # Quick deploy: copy fc-agent to rootfs (deprecated, use rootfs)

# Kernel builds (only needed once):
make kernel-setup # Clone Linux 5.10, upload FUSE-enabled config
make kernel       # Build kernel on EC2 (~10-20 min)

# Fetch artifacts to local machine:
make fetch        # Download fcvm and fc-agent binaries
make kernel-fetch # Download vmlinux

# Local builds (for IDE/linting only - won't run on macOS):
make build-local
make clean
```

**Key Makefile targets:**
| Target | Description |
|--------|-------------|
| `make build` | **Most common** - Sync + build everything on EC2 |
| `make test` | Run sanity test (starts VM, verifies health) |
| `make rebuild` | Full rebuild including rootfs update |
| `make rootfs` | Update fc-agent in existing rootfs |

### Manual Build on EC2

If you need to build directly on the EC2 instance:

```bash
# SSH to ARM instance
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104

# Build fcvm (host binary)
cd ~/fcvm
source ~/.cargo/env
cargo build --release

# Build fc-agent (guest binary, statically linked with musl)
cd fc-agent
cargo build --release --target aarch64-unknown-linux-musl

# Update fc-agent in rootfs
sudo mkdir -p /tmp/rootfs-mount
sudo mount -o loop /mnt/fcvm-btrfs/rootfs/base.ext4 /tmp/rootfs-mount
sudo cp ~/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent /tmp/rootfs-mount/usr/local/bin/
sudo umount /tmp/rootfs-mount

# Binaries at:
# ~/fcvm/target/release/fcvm
# ~/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent
```

### One-Time EC2 Setup

Only needed when setting up a fresh EC2 instance:

```bash
# Install prerequisites
sudo apt-get update
sudo apt-get install -y musl-tools gcc-aarch64-linux-gnu dnsmasq
rustup target add aarch64-unknown-linux-musl

# Configure dnsmasq for DNS forwarding
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

### rsync and File Sync Issues (2025-11-09)
- **Problem**: rsync said it synced cli.rs but the enum variant wasn't actually on the remote
- **Solution**: Always explicitly verify critical files after rsync
- **Lesson**: Don't trust rsync output - verify the actual file content changed

### Cargo Incremental Build Issues (2025-11-09)
- **Problem**: `cargo build` claimed "Finished" but binary didn't have new code
- **Root cause**: Binary was compiled BEFORE source file was properly synced
- **Solution**: After syncing source, explicitly rebuild (not `cargo clean`!)
- **Lesson**: Check file timestamps and binary timestamp to debug

### Background Command Execution (2025-11-09)
- **CRITICAL**: NEVER use `grep` or `tail` to filter cargo/test output
- **ALWAYS** use: `command 2>&1 | tee /tmp/log.txt`
- **ALWAYS** run in background: `run_in_background=True`
- **ALWAYS** check every 10 seconds (not 5, not 30!)
- **Reason**: Filtering loses context, wastes money, can't debug

### Alpine Serial Console (2025-11-10)
- **Problem**: VM booted but no output after OpenRC started
- **Root cause**: `/etc/inittab` has `ttyS0` serial console commented out by default
- **Fix**: Auto-enable during rootfs creation in setup/rootfs.rs
- **Implementation**:
  ```rust
  let inittab_fixed = inittab.replace(
      "#ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100",
      "ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100"
  );
  ```
- **Result**: VM now boots to login prompt on serial console
- **Manual fix for existing rootfs**: `sudo sed -i 's/^#ttyS0/ttyS0/' /mnt/rootfs/etc/inittab`

### DNS Resolution in VMs (2025-11-11)
- **Problem**: Container image pulls failing with DNS timeout
  ```
  dial tcp: lookup registry-1.docker.io on 8.8.8.8:53: read udp 172.16.10.2:40325->8.8.8.8:53: i/o timeout
  ```
- **Root cause**: VMs configured to use 8.8.8.8 directly but NAT wasn't forwarding DNS packets properly
- **Fix**: Install dnsmasq on host to act as DNS forwarder for all TAP interfaces
- **Implementation**:
  1. Install dnsmasq: `sudo apt-get install -y dnsmasq`
  2. Create `/etc/dnsmasq.d/fcvm.conf`:
     ```conf
     # Listen on all interfaces (including dynamically created TAP devices)
     bind-dynamic

     # Forward DNS to Google Public DNS
     server=8.8.8.8
     server=8.8.4.4

     # Don't read /etc/resolv.conf
     no-resolv

     # Cache size
     cache-size=1000
     ```
  3. Restart dnsmasq: `sudo systemctl restart dnsmasq`
- **Why bind-dynamic**: TAP devices are created dynamically after dnsmasq starts. The `bind-dynamic` option makes dnsmasq automatically listen on new interfaces without restart.
- **Result**: DNS resolution works, container images pull successfully

### Clone Network Configuration (2025-11-11)
- **Problem**: When restoring snapshots, guest OS retains original static IP (e.g., 172.16.29.2). Default network setup created TAP devices on different subnets (172.16.231.0/24, 172.16.201.0/24), causing subnet mismatch and connection failures.
- **Root cause**: Firecracker's network override only changes TAP device name, not guest IP configuration
- **Solution**: Configure TAP devices on the SAME subnet as the guest's original IP
  ```bash
  # Wrong: TAP on different subnet than guest
  ip addr add 172.16.201.1/24 dev tap-vm-c93e8  # Guest thinks it's 172.16.29.2
  # Connection fails due to subnet mismatch!

  # Correct: TAP on same subnet as guest
  ip addr add 172.16.29.1/24 dev tap-vm-c93e8   # Guest is 172.16.29.2
  # Works! Both on 172.16.29.0/24 subnet
  ```
- **Why it works**: Multiple TAP devices can have same host IP (172.16.29.1) because they're isolated L2 networks. Traffic doesn't conflict when using `--interface` flag or proper routing.
- **Implementation**: Clone network setup must extract guest IP from snapshot metadata and configure TAP on matching subnet
- **Reference**: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/network-for-clones.md
- **Alternative approaches** (not implemented):
  - Use network namespaces (more complex)
  - Use iptables NAT to translate IPs (requires routing setup)
  - Reconfigure guest via fc-agent after restore (requires guest agent changes)

### fc-agent musl Build (2025-11-11)
- **Problem**: fc-agent compiled with glibc (gnu target) doesn't work on Alpine Linux (uses musl libc)
  ```
  start-stop-daemon: failed to exec '/usr/local/bin/fc-agent': No such file or directory
  ```
- **Root cause**: Alpine Linux uses musl libc, not glibc. Binaries must be statically linked with musl.
- **Fix**: Compile fc-agent with musl target
- **Implementation**:
  1. Create `fc-agent/.cargo/config.toml`:
     ```toml
     [target.aarch64-unknown-linux-musl]
     linker = "aarch64-linux-musl-gcc"
     rustflags = ["-C", "target-feature=+crt-static"]
     ```
  2. Update `Makefile` to build with musl automatically:
     ```makefile
     build:
         cargo build --release
         cd fc-agent && cargo build --release --target aarch64-unknown-linux-musl
     ```
  3. Update `src/setup/rootfs.rs:240` to prefer musl binary:
     ```rust
     let possible_paths = vec![
         PathBuf::from("/home/ubuntu/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent"),  // musl (static)
         PathBuf::from("fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent"),  // musl relative
         // ... gnu fallbacks
     ];
     ```
- **Prerequisite**: Install musl cross-compiler on EC2:
  ```bash
  sudo apt-get install -y musl-tools gcc-aarch64-linux-gnu
  rustup target add aarch64-unknown-linux-musl
  ```
- **Result**: fc-agent runs successfully on Alpine Linux, executes container plans

### Disk Filename Consistency (2025-11-12)
- **Issue**: Code had inconsistent naming - some places used `rootfs-overlay.ext4`, others used `rootfs.ext4`
- **Root cause**: Legacy migration logic attempted to migrate from old to new naming
- **Solution**: Removed ALL migration logic, standardized on single filename: `rootfs.ext4`
- **Changes**:
  - `src/storage/disk.rs`: Removed migration logic, uses only `rootfs.ext4`
  - `src/commands/snapshot.rs`: Updated to expect `rootfs.ext4`
  - Variable names: Changed `overlay_path` → `disk_path` for clarity
- **Result**: Simple, consistent naming throughout - no migrations, no workarounds

### KVM and Nested Virtualization (2025-11-09)
- **Problem**: c5.large instance doesn't have `/dev/kvm` - nested virtualization not supported
- **Root cause**: Standard EC2 instances don't expose KVM to guest OS
- **Attempts failed**:
  - `modprobe kvm_intel` → "Operation not supported"
  - `modprobe kvm_amd` → "Operation not supported"
  - No CPU virtualization flags (vmx/svm) exposed in /proc/cpuinfo
- **Solution Options**:

  **Option 1: Metal Instances** (bare metal hardware)
  - c5.metal ($4.08/hr, 96 vCPUs, x86_64)
  - c6g.metal ($2.18/hr, 64 vCPUs, ARM64) ← cheaper!
  - **Blocker**: AWS vCPU limit is 16, metal instances need 64+
  - **Fix**: Request limit increase (takes 1-2 business days)

  **Option 2: PVM (Pagetable Virtual Machine)**
  - Enables Firecracker on regular instances WITHOUT nested virt
  - Proposed by Ant Group/Alibaba in Feb 2024
  - **Requirements**:
    - Build custom host kernel from virt-pvm/linux (Linux 6.7+)
    - Build custom guest kernel with PVM config
    - Use Firecracker fork with PVM patches (e.g., Loophole Labs)
  - **Pros**: Works on c5.large, no extra cost
  - **Cons**: Experimental, unmaintained upstream, complex setup
  - **Performance**: ~2x slower than bare metal for I/O workloads

- **Lesson**: Firecracker REQUIRES /dev/kvm or PVM - verify hardware support before testing

### Sync from Local
```bash
# From macOS - use --delete to remove old files and avoid conflicts
rsync -avz --delete --exclude 'target' --exclude '.git' \
  -e "ssh -i ~/.ssh/fcvm-ec2" \
  . ubuntu@54.176.90.249:~/fcvm/
```

## Next Steps

1. Add `fcvm setup` subcommands to main.rs
2. Implement kernel download/extraction
3. Implement rootfs creation (use debootstrap)
4. Test full workflow: setup -> run -> clone
5. Document performance results

## References
- Design doc: `/Users/ejcampbell/src/fcvm/DESIGN.md`
- Implementation summary: `/Users/ejcampbell/src/fcvm/IMPLEMENTATION_SUMMARY.md`
- Firecracker docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
