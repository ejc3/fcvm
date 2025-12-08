# fcvm - Firecracker VM Manager

A Rust implementation that launches Firecracker microVMs to run Podman containers, with lightning-fast cloning via UFFD memory sharing and btrfs CoW disk snapshots.

> **Features**
> - Run OCI containers in isolated Firecracker microVMs
> - Instant VM cloning via UFFD memory server + btrfs reflinks (~3ms)
> - Multiple VMs share memory via kernel page cache (50 VMs = ~512MB, not 25GB!)
> - Dual networking: bridged (iptables) or rootless (slirp4netns)
> - FUSE-based host directory mapping via fuse-pipe
> - Container exit code forwarding

---

## Prerequisites

**Hardware**
- Linux with `/dev/kvm` (bare-metal or nested virtualization)
- For EC2: c6g.metal (ARM64) or c5.metal (x86_64) - NOT regular instances

**Software**
- Rust 1.70+ with cargo
- For bridged networking: sudo access, iptables, iproute2, dnsmasq
- For rootless networking: slirp4netns
- For building rootfs: virt-customize (libguestfs-tools)

**Storage**
- btrfs filesystem at `/mnt/fcvm-btrfs` (for CoW disk snapshots)

---

## Quick Start

### Build
```bash
# Build host CLI and guest agent
cargo build --release --workspace
```

### Run a Container
```bash
# Run nginx in a Firecracker VM
sudo fcvm podman run --name web1 --network bridged nginx:alpine

# With port forwarding
sudo fcvm podman run --name web1 --network bridged --publish 8080:80 nginx:alpine

# With host directory mapping (via fuse-pipe)
sudo fcvm podman run --name web1 --network bridged --map /host/data:/data nginx:alpine

# List running VMs
fcvm ls
```

### Snapshot & Clone Workflow
```bash
# 1. Start baseline VM
sudo fcvm podman run --name baseline --network bridged nginx:alpine

# 2. Create snapshot (pauses VM briefly)
sudo fcvm snapshot create --pid <vm_pid> --tag nginx-warm

# 3. Start UFFD memory server (serves pages on-demand)
sudo fcvm snapshot serve nginx-warm

# 4. Clone from snapshot (~3ms startup)
sudo fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
sudo fcvm snapshot run --pid <serve_pid> --name clone2 --network bridged
```

---

## Project Structure

```
fcvm/
├── src/                    # Host CLI
│   ├── main.rs             # Entry point
│   ├── cli/                # Command-line parsing
│   ├── commands/           # Command implementations (podman, snapshot, ls)
│   ├── firecracker/        # Firecracker API client
│   ├── network/            # Networking (bridged, slirp)
│   ├── storage/            # Disk/snapshot management
│   ├── state/              # VM state persistence
│   ├── health.rs           # Health monitoring
│   ├── uffd/               # UFFD memory sharing
│   └── volume/             # Volume/FUSE mount handling
│
├── fc-agent/               # Guest agent
│   └── src/main.rs         # Container orchestration inside VM
│
├── fuse-pipe/              # FUSE passthrough library
│   └── src/                # Client/server for host directory sharing
│
└── tests/                  # Integration tests
    ├── test_sanity.rs      # Basic VM lifecycle
    ├── test_snapshot_clone.rs
    └── test_fuse_in_vm.rs  # POSIX compliance (8789 tests)
```

---

## Network Modes

| Mode | Flag | Root Required | Performance |
|------|------|---------------|-------------|
| Bridged | `--network bridged` | Yes | Better |
| Rootless | `--network rootless` | No | Good |

**Bridged**: Uses iptables NAT, requires sudo. Port forwarding via DNAT rules.

**Rootless**: Uses slirp4netns in user namespace. Port forwarding via slirp4netns API.

---

## Testing

```bash
# Quick sanity test
make test-sanity

# Full fuse-pipe POSIX compliance (8789 tests)
make container-test

# Run tests in container (recommended)
make container-shell
```

See `fuse-pipe/TESTING.md` for comprehensive test documentation.

---

## Data Layout

```
/mnt/fcvm-btrfs/
├── kernels/vmlinux.bin     # Firecracker kernel
├── rootfs/base.ext4        # Base Ubuntu + Podman image
├── vm-disks/{vm_id}/       # Per-VM disk (CoW reflink)
├── snapshots/              # Firecracker snapshots
└── state/                  # VM state JSON files
```

---

## Documentation

- `.claude/CLAUDE.md` - Detailed development notes and implementation status
- `fuse-pipe/TESTING.md` - Test infrastructure documentation
- `DESIGN.md` - Original design specification
