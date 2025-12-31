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
- For bridged networking: sudo, iptables, iproute2
- For rootless networking: slirp4netns
- For building rootfs: qemu-utils, e2fsprogs

**Storage**
- btrfs filesystem at `/mnt/fcvm-btrfs` (for CoW disk snapshots)
- Kernel auto-downloaded from Kata Containers release on first run

---

## Test Requirements

**Container Testing (Recommended)** - All dependencies bundled:
```bash
# Just needs podman and /dev/kvm
make container-test-unit             # Unit tests (no VMs)
make container-test-integration-fast # Quick VM tests (<30s each)
make container-test-root             # All tests including pjdfstest
```

**Native Testing** - Additional dependencies required:

| Category | Packages |
|----------|----------|
| FUSE | fuse3, libfuse3-dev |
| pjdfstest build | autoconf, automake, libtool |
| pjdfstest runtime | perl |
| bindgen (userfaultfd-sys) | libclang-dev, clang |
| VM tests | iproute2, iptables, slirp4netns |
| Rootfs build | qemu-utils, e2fsprogs |
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
    iproute2 iptables slirp4netns \
    qemu-utils e2fsprogs \
    uidmap
```

---

## Quick Start

### Build
```bash
# Build host CLI and guest agent
cargo build --release --workspace
```

### Setup (First Time)
```bash
# Create btrfs filesystem
make setup-btrfs

# Download kernel and create rootfs (takes 5-10 minutes first time)
fcvm setup
```

**What `fcvm setup` does:**
1. Downloads Kata kernel (~15MB, cached by URL hash)
2. Downloads packages via `podman run ubuntu:noble` (ensures correct Ubuntu 24.04 versions)
3. Creates Layer 2 rootfs (~10GB): boots VM, installs packages, writes config files
4. Verifies setup completed successfully (checks marker file)
5. Creates fc-agent initrd

Subsequent runs are instant - everything is cached by content hash.

**Alternative: Auto-setup on first run (rootless only)**
```bash
# Skip explicit setup - does it automatically on first run
fcvm podman run --name web1 --network rootless --setup nginx:alpine
```
The `--setup` flag triggers setup if kernel/rootfs are missing. Only works with `--network rootless` to avoid file ownership issues when running as root.

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

## Nested Virtualization (Inception)

fcvm supports running VMs inside VMs using ARM64 FEAT_NV2 nested virtualization. Currently **one level of nesting works**: Host → L1 VM with full KVM support.

```
┌─────────────────────────────────────────────────────────┐
│  Host (bare metal c7g.metal)                            │
│  ┌───────────────────────────────────────────────────┐  │
│  │  Level 1 VM (fcvm + inception kernel)             │  │
│  │  - KVM works (/dev/kvm accessible)                │  │
│  │  - Can run Firecracker VMs                        │  │
│  │  - Nested VMs run containers                      │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

**Limitation**: Recursive nesting (L1 → L2 → L3...) is currently blocked. L1's KVM reports `KVM_CAP_ARM_EL2=0` because NV2's E2H0 flag forces nVHE mode, but `kvm-arm.mode=nested` requires VHE mode. See `.claude/CLAUDE.md` for technical details.

### Requirements

| Requirement | Details |
|-------------|---------|
| **Hardware** | ARM64 with FEAT_NV2 (Graviton3+: c7g.metal, c7gn.metal, r7g.metal) |
| **Host kernel** | 6.18+ with `kvm-arm.mode=nested` boot parameter |
| **Inception kernel** | Pre-built from [releases](https://github.com/ejc3/firepod/releases) or build with `kernel/build.sh` |
| **Firecracker** | Fork with NV2 support: `ejc3/firecracker:nv2-inception` |

### Setting Up an EC2 Instance for Inception

**Step 1: Launch a metal instance**

```bash
# Must be a metal instance for FEAT_NV2 hardware support
# Recommended: c7g.metal, m7g.metal, r7g.metal (Graviton3)
aws ec2 run-instances \
    --instance-type c7g.metal \
    --image-id ami-0xyz...  # Ubuntu 24.04 ARM64
```

**Step 2: Build and install kernel 6.18+ with nested KVM**

```bash
# Install build dependencies
sudo apt-get update
sudo apt-get install -y build-essential flex bison bc libelf-dev libssl-dev

# Download kernel source
cd /tmp
wget https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.2.tar.xz
tar xf linux-6.18.2.tar.xz
cd linux-6.18.2

# Configure for ARM64 with KVM
make defconfig
./scripts/config --enable VIRTUALIZATION
./scripts/config --enable KVM
./scripts/config --enable CONFIG_FUSE_FS

# Build and install (~10-20 minutes on metal)
make -j$(nproc)
sudo make modules_install
sudo make install
```

**Step 3: Configure GRUB for nested KVM**

```bash
# Add kvm-arm.mode=nested to kernel boot parameters
sudo sed -i 's/GRUB_CMDLINE_LINUX_DEFAULT="/GRUB_CMDLINE_LINUX_DEFAULT="kvm-arm.mode=nested /' /etc/default/grub
sudo update-grub

# Reboot into new kernel
sudo reboot
```

**Step 4: Verify nested KVM is enabled**

```bash
# Check kernel version
uname -r  # Should show 6.18.2 or higher

# Check nested mode is enabled
cat /sys/module/kvm/parameters/mode  # Should show "nested"

# Verify KVM works
ls -la /dev/kvm
```

### Getting the Inception Kernel

```bash
# Download pre-built kernel from GitHub releases (~20MB)
fcvm setup --inception

# Kernel will be at /mnt/fcvm-btrfs/kernels/vmlinux-inception-6.18-aarch64-*.bin
```

Or build locally (takes 10-20 minutes):
```bash
fcvm setup --inception --build-kernels
# Or manually: ./kernel/build.sh
```

The inception kernel (6.18) includes:
- **CONFIG_KVM=y** - KVM hypervisor for nested virtualization
- **EL2 support** - ARM Exception Level 2 (hypervisor mode)
- **MMFR4 patch** - Enables `arm64.nv2` boot param for NV2 capability
- **FUSE** - For volume mounts between host and guest
- **Networking** - TUN/VETH/netfilter for bridged networking in nested VMs

### Running Inception

**Step 1: Start outer VM with inception kernel**
```bash
# FCVM_NV2=1 is auto-set when --kernel flag is used
sudo fcvm podman run \
    --name outer-vm \
    --network bridged \
    --kernel /mnt/fcvm-btrfs/kernels/vmlinux-inception-6.18-aarch64-*.bin \
    --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map /path/to/fcvm/binary:/opt/fcvm \
    nginx:alpine
```

**Step 2: Verify nested KVM works**
```bash
# Check guest sees HYP mode
fcvm exec --pid <outer_pid> --vm -- dmesg | grep -i kvm
# Should show: "kvm [1]: Hyp nVHE mode initialized successfully"

# Verify /dev/kvm is accessible
fcvm exec --pid <outer_pid> --vm -- ls -la /dev/kvm
```

**Step 3: Run inner VM**
```bash
# Inside outer VM (via exec or SSH)
cd /mnt/fcvm-btrfs
/opt/fcvm/fcvm podman run --name inner-vm --network bridged alpine:latest echo "Hello from inception!"
```

### How It Works

1. **FCVM_NV2=1** environment variable (auto-set when `--kernel` is used) triggers fcvm to pass `--enable-nv2` to Firecracker
2. **HAS_EL2 + HAS_EL2_E2H0** vCPU features are enabled
   - HAS_EL2 (bit 7): Enables virtual EL2 for guest
   - HAS_EL2_E2H0 (bit 8): Forces nVHE mode (avoids timer trap storm)
3. **vCPU boots at EL2h** so guest kernel's `is_hyp_mode_available()` returns true
4. **EL2 registers initialized**: HCR_EL2, CNTHCTL_EL2, VMPIDR_EL2, VPIDR_EL2
5. Guest kernel initializes KVM: "CPU: All CPU(s) started at EL2"
6. Nested fcvm creates VMs using the guest's KVM

### Testing Inception

```bash
# Run inception tests
make test-root FILTER=inception

# Tests:
# - test_kvm_available_in_vm: Verifies /dev/kvm works in guest
# - test_inception_run_fcvm_inside_vm: Full inception (fcvm inside fcvm)
```

### Limitations

- ARM64 only (x86_64 nested virt uses different mechanism)
- Requires bare-metal instance (c7g.metal) or host with nested virt enabled
- Recursive nesting (L2+) blocked - see `.claude/CLAUDE.md` for details

---

## Project Structure

```
fcvm/
├── src/           # Host CLI (fcvm binary)
├── fc-agent/      # Guest agent (runs inside VM)
├── fuse-pipe/     # FUSE passthrough library
└── tests/         # Integration tests (16 files)
```

See [DESIGN.md](DESIGN.md#directory-structure) for detailed structure.

---

## CLI Reference

Run `fcvm --help` or `fcvm <command> --help` for full options.

### Commands

| Command | Description |
|---------|-------------|
| `fcvm setup` | Download kernel (~15MB) and create rootfs (~10GB). Takes 5-10 min first run |
| `fcvm podman run` | Run container in Firecracker VM |
| `fcvm exec` | Execute command in running VM/container |
| `fcvm ls` | List running VMs (`--json` for JSON output) |
| `fcvm snapshot create` | Create snapshot from running VM |
| `fcvm snapshot serve` | Start UFFD memory server for cloning |
| `fcvm snapshot run` | Spawn clone from memory server |
| `fcvm snapshots` | List available snapshots |

See [DESIGN.md](DESIGN.md#commands) for full option reference.

### Key Options

**`fcvm podman run`** - Essential options:
```
--name <NAME>       VM name (required)
--network <MODE>    bridged (default, needs sudo) or rootless
--publish <H:G>     Port forward host:guest (e.g., 8080:80)
--map <H:G[:ro]>    Volume mount host:guest (optional :ro for read-only)
--env <K=V>         Environment variable
--setup             Auto-setup if kernel/rootfs missing (rootless only)
```

**`fcvm exec`** - Execute in VM/container:
```bash
sudo fcvm exec my-vm -- cat /etc/os-release     # In container
sudo fcvm exec my-vm --vm -- curl -s ifconfig.me # In guest OS
sudo fcvm exec my-vm -- bash                     # Interactive shell
```

---

## Network Modes

| Mode | Flag | Root | Notes |
|------|------|------|-------|
| Bridged | `--network bridged` | Yes | iptables NAT, better performance |
| Rootless | `--network rootless` | No | slirp4netns, works without root |

See [DESIGN.md](DESIGN.md#networking) for architecture details.

---

## Container Behavior

- **Exit codes**: Container exit code forwarded to host via vsock
- **Logs**: Container stdout/stderr prefixed with `[ctr:out]`/`[ctr:err]`
- **Health**: Default uses vsock ready signal; optional `--health-check` for HTTP

See [DESIGN.md](DESIGN.md#guest-agent) for details.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `FCVM_BASE_DIR` | `/mnt/fcvm-btrfs` | Base directory for all data |
| `RUST_LOG` | `info` | Logging level (e.g., `debug`, `firecracker=debug`) |

---

## Testing

```bash
# Quick start
make build                           # Build fcvm + fc-agent
make test-root                       # Run all tests (requires sudo + KVM)

# Test tiers
make test-unit                       # Unit tests only (no VMs)
make test-integration-fast           # Quick VM tests (<30s each)
make test-root                       # All tests including pjdfstest

# Container testing (recommended - all deps bundled)
make container-test-root             # All tests in container

# Options
make test-root FILTER=exec           # Filter by name
make test-root STREAM=1              # Live output
make test-root LIST=1                # List without running
```

See [DESIGN.md](DESIGN.md#test-infrastructure) for test architecture and file listing.

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

All data stored under `/mnt/fcvm-btrfs/` (btrfs for CoW reflinks). See [DESIGN.md](DESIGN.md#data-directory) for details.

```bash
# Setup btrfs (done automatically by make setup-btrfs)
make setup-btrfs
make setup-fcvm   # Download kernel, create rootfs
```

---

## Troubleshooting

### "fcvm binary not found"
- Build fcvm first: `make build`
- Or set PATH: `export PATH=$PATH:./target/release`

### "timeout waiting for VM to become healthy"
- Check VM logs: `sudo fcvm ls --json`
- Verify kernel and rootfs exist: `ls -la /mnt/fcvm-btrfs/`
- Check networking: VMs use host DNS servers directly (no dnsmasq needed)

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
- `PERFORMANCE.md` - Performance benchmarks, tuning guide, and tracing
- `.claude/CLAUDE.md` - Development notes, debugging tips, implementation details
- `LICENSE` - MIT License

---

## License

MIT License - see [LICENSE](LICENSE) for details.
