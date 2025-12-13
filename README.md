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
sudo fcvm podman run --name web1 nginx:alpine

# With port forwarding (8080 on host -> 80 in guest)
sudo fcvm podman run --name web1 --publish 8080:80 nginx:alpine

# With host directory mapping (via fuse-pipe)
sudo fcvm podman run --name web1 --map /host/data:/data nginx:alpine

# Read-only volume mapping
sudo fcvm podman run --name web1 --map /host/config:/config:ro nginx:alpine

# Custom resources
sudo fcvm podman run --name web1 --cpu 4 --mem 4096 nginx:alpine

# With environment variables and custom command
sudo fcvm podman run --name web1 --env DEBUG=1 --cmd "nginx -g 'daemon off;'" nginx:alpine

# Rootless mode (no sudo required)
fcvm podman run --name web1 --network rootless nginx:alpine

# List running VMs
fcvm ls
fcvm ls --json          # JSON output
fcvm ls --pid 12345     # Filter by PID
```

### Snapshot & Clone Workflow
```bash
# 1. Start baseline VM
sudo fcvm podman run --name baseline nginx:alpine

# 2. Create snapshot (pauses VM briefly)
sudo fcvm snapshot create baseline --tag nginx-warm
# Or by PID:
sudo fcvm snapshot create --pid <vm_pid> --tag nginx-warm

# 3. List available snapshots
fcvm snapshots

# 4. Start UFFD memory server (serves pages on-demand)
sudo fcvm snapshot serve nginx-warm

# 5. List running snapshot servers
fcvm snapshot ls

# 6. Clone from snapshot (~3ms startup)
sudo fcvm snapshot run --pid <serve_pid> --name clone1
sudo fcvm snapshot run --pid <serve_pid> --name clone2
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

## CLI Reference

### Global Options

| Option | Description |
|--------|-------------|
| `--base-dir <PATH>` | Base directory for all fcvm data (default: `/mnt/fcvm-btrfs` or `FCVM_BASE_DIR` env) |
| `--sub-process` | Running as subprocess (disables timestamp/level in logs) |

### Commands

#### `fcvm ls`
List running VMs.

| Option | Description |
|--------|-------------|
| `--json` | Output in JSON format |
| `--pid <PID>` | Filter by fcvm process PID |

#### `fcvm snapshots`
List available snapshots.

#### `fcvm podman run`
Run a container in a Firecracker VM.

| Option | Default | Description |
|--------|---------|-------------|
| `<IMAGE>` | (required) | Container image (e.g., `nginx:alpine`) or directory to build |
| `--name <NAME>` | (required) | VM name |
| `--cpu <N>` | 2 | Number of vCPUs |
| `--mem <MiB>` | 2048 | Memory in MiB |
| `--map <HOST:GUEST[:ro]>` | | Volume mapping(s), comma-separated. Append `:ro` for read-only |
| `--env <KEY=VALUE>` | | Environment variables, comma-separated or repeated |
| `--cmd <COMMAND>` | | Command to run inside container |
| `--publish <[IP:]HPORT:GPORT[/PROTO]>` | | Port forwarding, comma-separated |
| `--network <MODE>` | bridged | Network mode: `bridged` or `rootless` |
| `--health-check <URL>` | | HTTP health check URL (e.g., `http://localhost/health`) |
| `--balloon <MiB>` | (equals --mem) | Balloon target MiB |
| `--privileged` | false | Run container in privileged mode (allows mknod, device access) |

#### `fcvm snapshot create`
Create a snapshot from a running VM.

| Option | Description |
|--------|-------------|
| `<NAME>` | VM name to snapshot (mutually exclusive with `--pid`) |
| `--pid <PID>` | VM PID to snapshot (mutually exclusive with name) |
| `--tag <TAG>` | Custom snapshot name (defaults to VM name) |

#### `fcvm snapshot serve <SNAPSHOT>`
Start UFFD memory server to serve pages on-demand for cloning.

#### `fcvm snapshot run`
Run a clone from a snapshot.

| Option | Default | Description |
|--------|---------|-------------|
| `--pid <PID>` | (required) | Serve process PID to clone from |
| `--name <NAME>` | (auto) | Custom name for cloned VM |
| `--publish <[IP:]HPORT:GPORT[/PROTO]>` | | Port forwarding |
| `--network <MODE>` | bridged | Network mode: `bridged` or `rootless` |

#### `fcvm snapshot ls`
List running snapshot servers.

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
