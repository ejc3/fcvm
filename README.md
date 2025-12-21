# fcvm - Firecracker VM Manager

A Rust implementation that launches Firecracker microVMs to run Podman containers, with lightning-fast cloning via UFFD memory sharing and btrfs CoW disk snapshots.

> **Features**
> - Run OCI containers in isolated Firecracker microVMs
> - Instant VM cloning via UFFD memory server + btrfs reflinks (~3ms)
> - Multiple VMs share memory via kernel page cache (50 VMs = ~512MB, not 25GB!)
> - Dual networking: bridged (iptables) or rootless (slirp4netns)
> - Port forwarding for both regular VMs and clones
> - FUSE-based host directory mapping via fuse-pipe
> - Container exit code forwarding

---

## Prerequisites

**Hardware**
- Linux with `/dev/kvm` (bare-metal or nested virtualization)
- For AWS: c6g.metal (ARM64) or c5.metal (x86_64) - NOT regular instances

**Runtime Dependencies**
- Rust 1.83+ with cargo (nightly for fuser crate)
- Firecracker binary in PATH
- For bridged networking: sudo, iptables, iproute2, dnsmasq
- For rootless networking: slirp4netns
- For building rootfs: virt-customize (libguestfs-tools), qemu-utils, e2fsprogs

**Storage**
- btrfs filesystem at `/mnt/fcvm-btrfs` (for CoW disk snapshots)
- Pre-built Firecracker kernel at `/mnt/fcvm-btrfs/kernels/vmlinux.bin`

---

## Test Requirements

**Container Testing (Recommended)** - All dependencies bundled:
```bash
# Just needs podman and /dev/kvm
make container-test          # fuse-pipe tests
make container-test-vm       # VM tests
make container-test-pjdfstest # POSIX compliance (8789 tests)
```

**Native Testing** - Additional dependencies required:

| Category | Packages |
|----------|----------|
| FUSE | fuse3, libfuse3-dev |
| pjdfstest build | autoconf, automake, libtool |
| pjdfstest runtime | perl |
| bindgen (userfaultfd-sys) | libclang-dev, clang |
| VM tests | iproute2, iptables, slirp4netns, dnsmasq |
| Rootfs build | qemu-utils, libguestfs-tools, e2fsprogs |
| User namespaces | uidmap (for newuidmap/newgidmap) |

**pjdfstest Setup** (for POSIX compliance tests):
```bash
git clone --depth 1 https://github.com/pjd/pjdfstest /tmp/pjdfstest-check
cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make
```

**Ubuntu/Debian Install**:
```bash
sudo apt-get update && sudo apt-get install -y \
    fuse3 libfuse3-dev \
    autoconf automake libtool perl \
    libclang-dev clang \
    iproute2 iptables slirp4netns dnsmasq \
    qemu-utils libguestfs-tools e2fsprogs \
    uidmap
```

---

## Quick Start

### Build
```bash
# Build host CLI and guest agent
cargo build --release --workspace
```

### Run a Container
```bash
# Run nginx in a Firecracker VM (using AWS ECR public registry to avoid Docker Hub rate limits)
sudo fcvm podman run --name web1 public.ecr.aws/nginx/nginx:alpine

# With port forwarding (8080 on host -> 80 in guest)
sudo fcvm podman run --name web1 --publish 8080:80 public.ecr.aws/nginx/nginx:alpine

# With host directory mapping (via fuse-pipe)
sudo fcvm podman run --name web1 --map /host/data:/data public.ecr.aws/nginx/nginx:alpine

# Read-only volume mapping
sudo fcvm podman run --name web1 --map /host/config:/config:ro public.ecr.aws/nginx/nginx:alpine

# Custom resources
sudo fcvm podman run --name web1 --cpu 4 --mem 4096 public.ecr.aws/nginx/nginx:alpine

# With environment variables and custom command
sudo fcvm podman run --name web1 --env DEBUG=1 --cmd "nginx -g 'daemon off;'" public.ecr.aws/nginx/nginx:alpine

# Rootless mode (no sudo required)
fcvm podman run --name web1 --network rootless public.ecr.aws/nginx/nginx:alpine

# List running VMs (sudo needed to read VM state files)
sudo fcvm ls
sudo fcvm ls --json          # JSON output
sudo fcvm ls --pid 12345     # Filter by PID

# Execute commands (mirrors podman exec, sudo needed)
sudo fcvm exec web1 -- cat /etc/os-release         # Run in container (default)
sudo fcvm exec web1 --vm -- hostname               # Run in VM
sudo fcvm exec web1 -- bash                        # Interactive shell (auto -it)
sudo fcvm exec web1 -it -- sh                      # Explicit interactive TTY
```

### Snapshot & Clone Workflow
```bash
# 1. Start baseline VM
sudo fcvm podman run --name baseline public.ecr.aws/nginx/nginx:alpine

# 2. Create snapshot (pauses VM briefly)
sudo fcvm snapshot create baseline --tag nginx-warm
# Or by PID:
sudo fcvm snapshot create --pid <vm_pid> --tag nginx-warm

# 3. List available snapshots
sudo fcvm snapshots

# 4. Start UFFD memory server (serves pages on-demand)
sudo fcvm snapshot serve nginx-warm

# 5. List running snapshot servers (sudo needed to read state files)
sudo fcvm snapshot ls

# 6. Clone from snapshot (~3ms startup)
sudo fcvm snapshot run --pid <serve_pid> --name clone1
sudo fcvm snapshot run --pid <serve_pid> --name clone2

# 7. Clone with port forwarding (each clone can have unique ports)
sudo fcvm snapshot run --pid <serve_pid> --name web1 --publish 8081:80
sudo fcvm snapshot run --pid <serve_pid> --name web2 --publish 8082:80
curl localhost:8081  # Reaches clone web1
curl localhost:8082  # Reaches clone web2

# 8. Clone and execute command (auto-cleans up after)
sudo fcvm snapshot run --pid <serve_pid> --exec "curl localhost"
# Clone starts → execs command in container → returns result → cleans up
```

---

## Advanced Demos

| Demo | What it proves |
|------|----------------|
| **Clone Speed** | 3ms memory restore from snapshot |
| **Memory Sharing** | 10 clones use ~1.5GB extra, not 20GB |
| **Scale-Out** | 50+ VMs with ~7GB memory, not 100GB |
| **Privileged Container** | mknod and device access work |
| **Multiple Ports** | Comma-separated port mappings |
| **Multiple Volumes** | Comma-separated volume mappings with :ro |

### Clone Speed (~3ms startup)

Demonstrate instant VM cloning from a warmed snapshot:

```bash
# Setup: Create baseline and snapshot
sudo fcvm podman run --name baseline public.ecr.aws/nginx/nginx:alpine
sudo fcvm snapshot create baseline --tag nginx-warm
sudo fcvm snapshot serve nginx-warm  # Note the serve PID

# Time a clone startup (includes exec and cleanup)
time sudo fcvm snapshot run --pid <serve_pid> --exec "echo ready"
# real 0m0.003s  ← 3ms!
```

### Memory Sharing Proof

Show that multiple clones share memory via kernel page cache:

```bash
# Check baseline memory
free -m | grep Mem

# Start 10 clones from same snapshot
for i in {1..10}; do
  sudo fcvm snapshot run --pid <serve_pid> --name clone$i &
done
wait

# Memory barely increased! 10 VMs share the same pages
free -m | grep Mem
```

### Scale-Out Demo (50 VMs in ~150ms)

Spin up a fleet of web servers instantly:

```bash
# Create warm nginx snapshot (one-time)
sudo fcvm podman run --name baseline --publish 8080:80 public.ecr.aws/nginx/nginx:alpine
# Wait for healthy, then snapshot
sudo fcvm snapshot create baseline --tag nginx-warm
sudo fcvm snapshot serve nginx-warm  # Note serve PID

# Spin up 50 nginx instances in parallel
time for i in {1..50}; do
  sudo fcvm snapshot run --pid <serve_pid> --name web$i --publish $((8080+i)):80 &
done
wait
# real 0m0.150s  ← 50 VMs in 150ms!

# Verify all running
sudo fcvm ls | wc -l  # 51 (50 clones + 1 baseline)

# Test a random clone
curl -s localhost:8090 | head -5
```

### Privileged Container (Device Access)

Run containers that need mknod or device access:

```bash
# Privileged mode allows mknod, /dev access, etc.
sudo fcvm podman run --name dev --privileged \
  --cmd "sh -c 'mknod /dev/null2 c 1 3 && ls -la /dev/null2'" \
  public.ecr.aws/docker/library/alpine:latest
# Output: crw-r--r-- 1 root root 1,3 /dev/null2
```

### Multiple Ports and Volumes

Expose multiple ports and mount multiple volumes in one command:

```bash
# Multiple port mappings (comma-separated)
sudo fcvm podman run --name multi-port \
  --publish 8080:80,8443:443 \
  public.ecr.aws/nginx/nginx:alpine

# Multiple volume mappings (comma-separated, with read-only)
sudo fcvm podman run --name multi-vol \
  --map /tmp/logs:/var/log,/tmp/data:/data:ro \
  public.ecr.aws/nginx/nginx:alpine

# Combined
sudo fcvm podman run --name full \
  --publish 8080:80,8443:443 \
  --map /tmp/html:/usr/share/nginx/html:ro \
  --env NGINX_HOST=localhost,NGINX_PORT=80 \
  public.ecr.aws/nginx/nginx:alpine
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
│   ├── src/                # Client/server for host directory sharing
│   ├── tests/              # Integration tests
│   └── benches/            # Performance benchmarks
│
└── tests/                  # Integration tests
    ├── common/mod.rs       # Shared test utilities
    ├── test_sanity.rs      # Basic VM lifecycle
    ├── test_state_manager.rs
    ├── test_health_monitor.rs
    ├── test_fuse_posix.rs
    ├── test_fuse_in_vm.rs
    ├── test_localhost_image.rs
    └── test_snapshot_clone.rs
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
| `<IMAGE>` | (required) | Container image (e.g., `nginx:alpine` or `localhost/myimage`) |
| `--name <NAME>` | (required) | VM name |
| `--cpu <N>` | 2 | Number of vCPUs |
| `--mem <MiB>` | 2048 | Memory in MiB |
| `--map <HOST:GUEST[:ro]>` | | Volume mapping(s), comma-separated. Append `:ro` for read-only |
| `--env <KEY=VALUE>` | | Environment variables, comma-separated or repeated |
| `--cmd <COMMAND>` | | Command to run inside container |
| `--publish <[IP:]HPORT:GPORT[/PROTO]>` | | Port forwarding, comma-separated |
| `--network <MODE>` | bridged | Network mode: `bridged` or `rootless` |
| `--health-check <URL>` | | HTTP health check URL. If not specified, uses container ready signal via vsock |
| `--balloon <MiB>` | (none) | Balloon device target MiB. If not specified, no balloon device is configured |
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
| `--exec <CMD>` | | Execute command in container after clone starts, then cleanup |

#### `fcvm snapshot ls`
List running snapshot servers.

#### `fcvm exec`
Execute a command in a running VM or container. Mirrors `podman exec` behavior.

| Option | Description |
|--------|-------------|
| `<NAME>` | VM name (mutually exclusive with `--pid`) |
| `--pid <PID>` | VM PID (mutually exclusive with name) |
| `--vm` | Execute in the VM instead of inside the container |
| `-i, --interactive` | Keep STDIN open |
| `-t, --tty` | Allocate pseudo-TTY |
| `-- <COMMAND>...` | Command and arguments to execute |

**Auto-detection**: When running a shell (bash, sh, zsh, etc.) with a TTY stdin, `-it` is enabled automatically.

**Examples:**
```bash
# Execute inside container (default, sudo needed to read VM state)
sudo fcvm exec my-vm -- cat /etc/os-release
sudo fcvm exec --pid 12345 -- wget -q -O - ifconfig.me

# Execute in VM (guest OS)
sudo fcvm exec my-vm --vm -- hostname
sudo fcvm exec --pid 12345 --vm -- curl -s ifconfig.me

# Interactive shell (auto-detects -it when stdin is a TTY)
sudo fcvm exec my-vm -- bash
sudo fcvm exec my-vm --vm -- bash

# Explicit TTY flags (like podman exec -it)
sudo fcvm exec my-vm -it -- sh
sudo fcvm exec my-vm --vm -it -- bash
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

## Container Behavior

### Exit Code Forwarding

When a container exits, fcvm forwards its exit code:

```bash
# Container exits with code 0 → fcvm returns 0
sudo fcvm podman run --name test --cmd "exit 0" public.ecr.aws/nginx/nginx:alpine
echo $?  # 0

# Container exits with code 42 → fcvm returns error
sudo fcvm podman run --name test --cmd "exit 42" public.ecr.aws/nginx/nginx:alpine
# ERROR fcvm: Error: container exited with code 42
echo $?  # 1
```

Exit codes are communicated from fc-agent (inside VM) to fcvm (host) via vsock status channel (port 4999).

### Container Logs

Container stdout/stderr flows through the serial console:
1. Container writes to stdout/stderr
2. fc-agent prefixes with `[ctr:out]` or `[ctr:err]` and writes to serial console
3. Firecracker sends serial output to fcvm
4. fcvm logs via tracing (visible on stderr)

Example output:
```
INFO firecracker: fc-agent[292]: [ctr:out] hello world
INFO firecracker: fc-agent[292]: [ctr:err] error message
```

### Health Checks

**Default behavior**: fcvm waits for fc-agent to signal container readiness via vsock. No HTTP polling needed.

**Custom HTTP health check**: Use `--health-check` for HTTP-based health monitoring:
```bash
sudo fcvm podman run --name web --health-check http://localhost:80/health nginx:alpine
```

With custom health checks, fcvm polls the URL until it returns 2xx status.

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `FCVM_BASE_DIR` | Base directory for all fcvm data | `/mnt/fcvm-btrfs` |
| `RUST_LOG` | Logging level and filters | `info` |

### Examples

```bash
# Use different base directory
FCVM_BASE_DIR=/data/fcvm sudo fcvm podman run ...

# Increase logging verbosity
RUST_LOG=debug sudo fcvm podman run ...

# Debug specific component
RUST_LOG=firecracker=debug,health-monitor=debug sudo fcvm podman run ...

# Silence all logs
RUST_LOG=off sudo fcvm podman run ... 2>/dev/null
```

---

## Testing

### Makefile Targets

Run `make help` for the full list. Key targets:

#### Development
| Target | Description |
|--------|-------------|
| `make build` | Build fcvm and fc-agent |
| `make clean` | Clean build artifacts |

#### Testing
| Target | Description |
|--------|-------------|
| `make test` | Run fuse-pipe tests: noroot + root |
| `make test-noroot` | Tests without root: unit + integration + stress |
| `make test-root` | Tests requiring root: integration_root + permission |
| `make test-unit` | Unit tests only (no root) |
| `make test-fuse` | All fuse-pipe tests explicitly |
| `make test-vm` | Run VM tests: rootless + bridged |
| `make test-vm-rootless` | VM test with slirp4netns (no root) |
| `make test-vm-bridged` | VM test with bridged networking |
| `make test-pjdfstest` | POSIX compliance (8789 tests) |
| `make test-all` | Everything: test + test-vm + test-pjdfstest |

#### Container Testing (Recommended)
| Target | Description |
|--------|-------------|
| `make container-test` | Run fuse-pipe tests in container |
| `make container-test-vm` | Run VM tests in container |
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

### Test Files

#### fcvm Integration Tests (`tests/`)
| File | Description |
|------|-------------|
| `test_sanity.rs` | Basic VM startup and health check (rootless + bridged) |
| `test_state_manager.rs` | State management unit tests |
| `test_health_monitor.rs` | Health monitoring tests |
| `test_fuse_posix.rs` | POSIX FUSE compliance tests |
| `test_fuse_in_vm.rs` | FUSE-in-VM integration |
| `test_localhost_image.rs` | Local image tests |
| `test_snapshot_clone.rs` | Snapshot/clone workflow, clone port forwarding |
| `test_port_forward.rs` | Port forwarding for regular VMs |

#### fuse-pipe Tests (`fuse-pipe/tests/`)
| File | Description |
|------|-------------|
| `integration.rs` | Basic FUSE operations (no root) |
| `integration_root.rs` | FUSE operations requiring root |
| `test_permission_edge_cases.rs` | Permission edge cases, setuid/setgid |
| `test_mount_stress.rs` | Mount/unmount stress tests |
| `test_allow_other.rs` | AllowOther flag tests |
| `test_unmount_race.rs` | Unmount race condition tests |
| `pjdfstest_full.rs` | Full POSIX compliance (8789 tests) |
| `pjdfstest_fast.rs` | Fast POSIX subset |
| `pjdfstest_stress.rs` | Parallel stress test |

### Running Tests

```bash
# Container testing (recommended)
make container-test      # All fuse-pipe tests
make container-test-vm   # VM tests

# Native testing
make test               # fuse-pipe tests
make test-vm            # VM tests

# Direct cargo commands (for debugging)
cargo test --release -p fuse-pipe --test integration -- --nocapture
sudo cargo test --release --test test_sanity -- --nocapture
```

### Debugging Tests

Enable tracing:
```bash
RUST_LOG="passthrough=debug,fuse_pipe=info" sudo -E cargo test ...
```

Check running VMs:
```bash
sudo fcvm ls
```

Manual cleanup:
```bash
# Kill test VMs
ps aux | grep fcvm | grep test | awk '{print $2}' | xargs sudo kill 2>/dev/null

# Remove test directories
rm -rf /tmp/fcvm-test-*

# Force unmount stale FUSE mounts
sudo fusermount3 -u /tmp/fuse-*-mount*
```

---

## Data Layout

```
/mnt/fcvm-btrfs/
├── kernels/vmlinux.bin     # Firecracker kernel
├── rootfs/base.ext4        # Base Ubuntu + Podman image
├── vm-disks/{vm_id}/       # Per-VM disk (CoW reflink)
├── snapshots/              # Firecracker snapshots
├── state/                  # VM state JSON files
└── cache/                  # Downloaded cloud images
```

---

## Setup

### dnsmasq Setup

```bash
# One-time: Install dnsmasq for DNS forwarding to VMs
sudo apt-get update && sudo apt-get install -y dnsmasq
sudo tee /etc/dnsmasq.d/fcvm.conf > /dev/null <<EOF
bind-dynamic
server=8.8.8.8
server=8.8.4.4
no-resolv
cache-size=1000
EOF
sudo systemctl restart dnsmasq
```

### btrfs Setup

```bash
# Create btrfs loopback (done automatically by make setup-btrfs)
sudo truncate -s 20G /var/fcvm-btrfs.img
sudo mkfs.btrfs /var/fcvm-btrfs.img
sudo mkdir -p /mnt/fcvm-btrfs
sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs
sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,state,snapshots,vm-disks,cache}
sudo chown -R $USER:$USER /mnt/fcvm-btrfs
```

---

## Troubleshooting

### "fcvm binary not found"
- Build fcvm first: `make build`
- Or set PATH: `export PATH=$PATH:./target/release`

### "timeout waiting for VM to become healthy"
- Check VM logs: `sudo fcvm ls --json`
- Verify kernel and rootfs exist: `ls -la /mnt/fcvm-btrfs/`
- Check dnsmasq is running: `systemctl status dnsmasq`

### Tests hang indefinitely
- VMs may not be cleaning up properly
- Manual cleanup: `ps aux | grep fcvm | grep test | awk '{print $2}' | xargs sudo kill`

### KVM not available
- Firecracker requires `/dev/kvm`
- On AWS: use c6g.metal or c5.metal (NOT c5.large or other regular instances)
- On other clouds: use bare-metal instances or hosts with nested virtualization

---

## Documentation

- `DESIGN.md` - Comprehensive design specification and architecture
- `.claude/CLAUDE.md` - Development notes, debugging tips, implementation details
- `LICENSE` - MIT License

---

## License

MIT License - see [LICENSE](LICENSE) for details.
