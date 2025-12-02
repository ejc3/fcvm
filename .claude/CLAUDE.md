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
├── readiness/        # Readiness gates
└── setup/            # Setup subcommands

tests/
├── common/mod.rs     # Shared test utilities
└── test_cli_parsing.rs
```

### Design Principles
- **Library + Binary pattern**: src/lib.rs exports all modules, src/main.rs is thin dispatcher
- **One file per command**: Easy to find, easy to test
- **Single binary**: `fcvm` with subcommands (guest agent `fc-agent` is separate)

## Implementation Status

### ✅ Completed

1. **Core Implementation** (2025-11-09)
   - Firecracker API client using hyper + hyperlocal (Unix sockets)
   - Dual networking modes: bridged (nftables) + rootless (slirp4netns)
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
   - `--network bridged` (default): Linux bridge + nftables, requires root
   - `--network rootless`: slirp4netns, no root required
   - User namespace via `unshare --user --map-root-user --net`
   - Health checks use unique loopback IPs (127.x.y.z) per VM

4. **Hierarchical Logging** (2025-11-15)
   - Target tags showing process nesting
   - Smart color handling: TTY gets colors, pipes don't
   - Strips Firecracker timestamps and `[anonymous-instance:*]` prefixes

### 📋 TODO
1. `fcvm setup kernel` - Download/prepare vmlinux
2. `fcvm setup rootfs` - Create base rootfs with Podman
3. `fcvm setup preflight` - Validate system requirements

## Technical Reference

### Firecracker Requirements
- **Kernel**: vmlinux or bzImage, boot args: `console=ttyS0 reboot=k panic=1 pci=off`
- **Rootfs**: ext4 with Ubuntu 24.04, systemd, Podman, iproute2, fc-agent at `/usr/local/bin/fc-agent`

### Network Modes

| Mode | Flag | Requires Root | Performance | Port Forwarding |
|------|------|---------------|-------------|-----------------|
| Bridged | `--network bridged` | Yes | Better | nftables DNAT |
| Rootless | `--network rootless` | No | Good | slirp4netns API |

**Rootless Architecture:**
- Firecracker starts with `unshare --user --map-root-user --net`
- slirp4netns connects to the namespace via PID, creates TAP device
- Guest IP: 10.0.2.15, Gateway: 10.0.2.2 (slirp4netns defaults)
- Port forwarding via slirp4netns JSON-RPC API socket
- Health checks use unique loopback IPs (127.x.y.z) per VM

```rust
// src/network/slirp.rs - generate_loopback_ip()
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

**Setup (one-time):**
```bash
sudo mkfs.btrfs /dev/nvme1n1
sudo mount /dev/nvme1n1 /mnt/fcvm-btrfs
sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,state,snapshots,vm-disks}
```

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

| Target | Description |
|--------|-------------|
| `make build` | Sync + build fcvm + fc-agent on EC2 |
| `make test` | Run sanity test on EC2 |
| `make rebuild` | Full rebuild including rootfs update |
| `make rootfs` | Update fc-agent in rootfs only |
| `make sync` | Just sync code (no build) |
| `make kernel` | Build kernel on EC2 (~10-20 min) |
| `make fetch` | Download binaries to local |

### Manual Build on EC2

```bash
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104
cd ~/fcvm && source ~/.cargo/env

# Build fcvm
cargo build --release

# Build fc-agent
cd fc-agent && cargo build --release

# Update rootfs
sudo mkdir -p /tmp/rootfs-mount
sudo mount -o loop /mnt/fcvm-btrfs/rootfs/base.ext4 /tmp/rootfs-mount
sudo cp ~/fcvm/fc-agent/target/release/fc-agent /tmp/rootfs-mount/usr/local/bin/
sudo umount /tmp/rootfs-mount
```

### One-Time EC2 Setup

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

## References
- Design doc: `/Users/ejcampbell/src/fcvm/DESIGN.md`
- Firecracker docs: https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md
