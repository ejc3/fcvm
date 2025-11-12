# fcvm Development Log

## Overview
fcvm is a Firecracker VM manager for running Podman containers in lightweight microVMs. This document tracks implementation findings and decisions.

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

### 🚧 In Progress

None - all major features working!

### 📋 TODO
1. **Setup Subcommands**
   - `fcvm setup kernel` - Download/prepare vmlinux
   - `fcvm setup rootfs` - Create base rootfs with Podman
   - `fcvm setup preflight` - Validate system requirements

2. **Testing**
   - Test `fcvm run nginx:latest` end-to-end
   - Measure clone performance (<1s target)
   - Test port mapping (rootless + privileged modes)
   - Test volume mounting

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
- **Rootless mode**: slirp4netns provides userspace networking
  - Port forwarding via hostfwd
  - No root privileges required
  - Slightly lower performance than bridge mode

- **Privileged mode**: Linux bridge + nftables DNAT
  - Better performance
  - Requires root or CAP_NET_ADMIN
  - Uses nftables for port forwarding

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

### On ARM EC2 Instance (c6g.metal)
```bash
# SSH to ARM instance
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.67.60.104

# One-time setup: Install prerequisites
sudo apt-get update
sudo apt-get install -y musl-tools gcc-aarch64-linux-gnu dnsmasq
rustup target add aarch64-unknown-linux-musl

# One-time setup: Configure dnsmasq for DNS forwarding
sudo tee /etc/dnsmasq.d/fcvm.conf > /dev/null <<EOF
# Listen on all interfaces (including dynamically created TAP devices)
bind-dynamic

# Forward DNS to Google Public DNS
server=8.8.8.8
server=8.8.4.4

# Don't read /etc/resolv.conf
no-resolv

# Cache size
cache-size=1000
EOF

sudo systemctl restart dnsmasq

# Sync code from local
rsync -avz --delete --exclude 'target' --exclude '.git' \
  -e "ssh -i ~/.ssh/fcvm-ec2" \
  . ubuntu@54.67.60.104:~/fcvm/

# Build fcvm and fc-agent (use background + tee!)
cd ~/fcvm
source ~/.cargo/env
cargo build --release 2>&1 | tee /tmp/fcvm-build.log
cd fc-agent && cargo build --release --target aarch64-unknown-linux-musl 2>&1 | tee /tmp/fc-agent-build.log

# Or use Makefile (builds both automatically):
make build 2>&1 | tee /tmp/build.log

# Binaries at:
# ~/fcvm/target/release/fcvm
# ~/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent
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
