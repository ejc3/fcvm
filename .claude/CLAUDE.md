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

3. **Test Infrastructure** (2025-11-09)
   - EC2 c5.large instance (KVM/nested virt support)
   - Instance ID: i-0c23eceda148fdd60
   - Public IP: 54.176.90.249
   - Firecracker v1.10.0 installed
   - Podman 4.9.3, slirp4netns 1.2.1 installed
   - SSH access configured

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

### 🚧 In Progress
1. **Guest Environment Setup**
   - Need to add `fcvm setup` subcommands
   - Kernel: Download pre-built or extract from host
   - Rootfs: Create Debian-based image with Podman + fc-agent

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

### Storage Notes
- Base rootfs is read-only, shared across VMs
- Each VM gets CoW overlay for writes
- Snapshots capture memory + disk state
- Clone creates new VM from snapshot (<1s target)

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

## Build Instructions

### On EC2 Instance
```bash
# SSH to instance
ssh -i ~/.ssh/fcvm-ec2 ubuntu@54.176.90.249

# Build fcvm (use background + tee!)
cd ~/fcvm
source ~/.cargo/env
cargo build --release 2>&1 | tee /tmp/fcvm-build.log

# Build fc-agent
cd ~/fcvm/fc-agent
cargo build --release 2>&1 | tee /tmp/fc-agent-build.log

# Binaries at:
# ~/fcvm/target/release/fcvm
# ~/fcvm/fc-agent/target/release/fc-agent
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
