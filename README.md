# fcvm - Firecracker VM Manager

A Rust implementation that launches Firecracker microVMs to run Podman containers, with lightning-fast cloning via UFFD memory sharing and btrfs CoW disk snapshots.

> **Features**
> - Run OCI containers in isolated Firecracker microVMs
> - **~6x faster startup** with container image cache (540ms vs 3100ms)
> - Fast VM cloning via UFFD memory server + btrfs reflinks (~10ms restore, ~610ms with exec)
> - Multiple VMs share memory via kernel page cache (50 VMs = ~512MB, not 25GB!)
> - Dual networking: bridged (iptables) or rootless (slirp4netns)
> - Port forwarding for both regular VMs and clones
> - FUSE-based host directory mapping via fuse-pipe
> - Container exit code forwarding
> - Interactive shell support (`-it`) with full TTY (vim, editors, colors)

---

## Prerequisites

**Hardware**
- Linux with `/dev/kvm` (bare-metal or nested virtualization)
- For AWS: c6g.metal (ARM64) or c5.metal (x86_64) - NOT regular instances

**Runtime Dependencies**
- Rust 1.83+ with cargo and musl target ([rustup.rs](https://rustup.rs), then `rustup target add $(uname -m)-unknown-linux-musl`)
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
make container-test  # All tests in container (just needs podman + /dev/kvm)
```

See [CLAUDE.md](.claude/CLAUDE.md#makefile-targets) for all Makefile targets.

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

**Complete prerequisites**: See [`Containerfile`](Containerfile) for the full list of dependencies used in CI. This includes additional packages for kernel builds, container runtime, and testing. Running fcvm inside a VM (nested virtualization) is experimental.

**Host system configuration**:
```bash
# KVM access
sudo chmod 666 /dev/kvm

# Userfaultfd for snapshot cloning
sudo mknod /dev/userfaultfd c 10 126 2>/dev/null || true
sudo chmod 666 /dev/userfaultfd
sudo sysctl -w vm.unprivileged_userfaultfd=1

# FUSE allow_other
echo "user_allow_other" | sudo tee -a /etc/fuse.conf

# Ubuntu 24.04+: allow unprivileged user namespaces
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0

# IP forwarding for container networking (e.g., podman builds)
sudo sysctl -w net.ipv4.conf.all.forwarding=1
sudo sysctl -w net.ipv4.conf.default.forwarding=1

# Bridged networking only (not needed for --network rootless):
sudo mkdir -p /var/run/netns
sudo iptables -P FORWARD ACCEPT
# NAT rule is set up automatically by fcvm

# If running fcvm inside a container, set NAT on the HOST (container iptables don't persist):
# sudo iptables -t nat -A POSTROUTING -s 172.30.0.0/16 -o eth0 -j MASQUERADE
```

---

## Quick Start

fcvm runs containers inside Firecracker microVMs:

```
You → fcvm → Firecracker VM → Podman → Container
```

Each `podman run` boots a VM, pulls the image, and starts the container with full VM isolation. First run takes ~3s; subsequent runs with the same image take ~540ms (cached).

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install musl toolchain (for static linking fc-agent binary)
sudo apt install musl-tools
rustup target add $(uname -m)-unknown-linux-musl

# Clone and build fcvm + fc-agent binaries (~2 min)
git clone https://github.com/ejc3/fcvm
cd fcvm
make build
# → "Finished release profile [optimized] target(s)"

# Create symlink for convenience (works with sudo)
ln -sf target/release/fcvm ./fcvm

# Download kernel + build rootfs (~5 min first time, then cached)
sudo ./fcvm setup
# → "Setup complete"

# One-shot command (runs, prints output, exits)
./fcvm podman run --name hello alpine:latest -- echo "Hello from microVM"
# → Hello from microVM

# Run a long-lived service (stays in foreground, or add & to background)
./fcvm podman run --name web nginx:alpine
# → Logs show VM booting, then "healthy" when nginx is ready

# In another terminal:
./fcvm ls
# → Shows "web" with PID, health status, network info

./fcvm exec --name web -- cat /etc/os-release
# → Shows Alpine Linux info

# Bridged networking (for full network access, requires sudo)
sudo ./fcvm podman run --name web-bridged --network bridged nginx:alpine
```

### Container Image Cache (~6x Faster Startup)

fcvm automatically caches container images after the first pull. On subsequent runs with the same image, startup is **~6x faster** (540ms vs 3100ms).

```bash
# First run: pulls image, creates cache (~3s)
./fcvm podman run --name web1 nginx:alpine
# → Cache created for nginx:alpine

# Second run: restores from cache (~540ms)
./fcvm podman run --name web2 nginx:alpine
# → Restored from snapshot

# Disable snapshot for testing
./fcvm podman run --name web3 --no-snapshot nginx:alpine
```

**How it works:**
1. First run: fc-agent pulls image, host takes Firecracker snapshot
2. Cache key: SHA256 of (image, tag, cmd, env, config)
3. Subsequent runs: Restore snapshot, fc-agent starts container (image already pulled)

The snapshot captures VM state **after image pull but before container start**. On restore, fc-agent runs `podman run` with the already-pulled image, skipping the slow pull/export step.

### Two-Tier Snapshot System

fcvm uses a two-tier snapshot system for optimal startup performance:

| Snapshot | When Created | Content | Size |
|----------|--------------|---------|------|
| **Pre-start** | After image pull, before container runs | VM with image loaded | Full (~2GB) |
| **Startup** | After HTTP health check passes | VM with container fully initialized | Diff (~50MB) |

**How diff snapshots work:**
1. **First snapshot (pre-start)**: Creates a full memory snapshot (~2GB)
2. **Subsequent snapshots (startup)**: Copies parent's memory.bin via reflink (CoW, instant), creates diff with only changed pages, merges diff onto base
3. **Result**: Each snapshot ends up with a **complete memory.bin** - equivalent to a full snapshot, but created much faster

**Key insight**: We use reflink copy + diff merge, not persistent diff chains. The reflink copy is instant (btrfs CoW), and the diff contains only ~2% of pages (those changed during container startup). After merging, you have a complete memory.bin that can be restored without any dependency on parent snapshots.

The startup snapshot is triggered by `--health-check <url>`. When the health check passes, fcvm creates a diff snapshot of the fully-initialized application. Second run restores from the startup snapshot, skipping container initialization entirely.

```bash
# First run: Creates pre-start (full) + startup (diff, merged)
./fcvm podman run --name web --health-check http://localhost/ nginx:alpine
# → Pre-start snapshot: 2048MB (full)
# → Startup snapshot: ~50MB (diff) → merged onto base

# Second run: Restores from startup snapshot (~100ms faster)
./fcvm podman run --name web2 --health-check http://localhost/ nginx:alpine
# → Restored from startup snapshot (application already running)
```

**Parent lineage**: User snapshots from clones automatically use their source snapshot as a parent, enabling diff-based optimization across the entire snapshot chain.

### More Options

```bash
# Port forwarding (8080 on host -> 80 in container)
./fcvm podman run --name web --publish 8080:80 nginx:alpine
# In rootless: curl the assigned loopback IP (e.g., curl 127.0.0.2:8080)
# In bridged: curl the veth host IP (see ./fcvm ls --json)

# Mount host directory into container
./fcvm podman run --name app --map /host/data:/data alpine:latest

# Custom CPU/memory
./fcvm podman run --name big --cpu 4 --mem 4096 alpine:latest

# Interactive shell (-it like docker/podman)
./fcvm podman run --name shell -it alpine:latest sh

# JSON output for scripting
./fcvm ls --json
./fcvm ls --pid 12345    # Filter by PID

# Execute in guest VM instead of container
./fcvm exec --name web --vm -- hostname

# Interactive shell in container
./fcvm exec --name web -it -- sh

# TTY for colors (no stdin)
./fcvm exec --name web -t -- ls -la --color=always
```

### Snapshot & Clone Workflow

Two modes for restoring from snapshots:
- **UFFD mode** (`--pid`): Memory served on-demand via UFFD server. Best for many concurrent clones sharing memory.
- **Direct mode** (`--snapshot`): Memory loaded directly from file. Simpler, no server needed.

```bash
# 1. Start baseline VM (using bridged, or omit --network for rootless)
sudo ./fcvm podman run --name baseline --network bridged public.ecr.aws/nginx/nginx:alpine

# 2. Create snapshot (pauses VM briefly, then resumes)
sudo ./fcvm snapshot create baseline --tag nginx-warm

# === Direct Mode (simpler, for single clones) ===
# Clone directly from snapshot files - no server needed
sudo ./fcvm snapshot run --snapshot nginx-warm --name clone1 --network bridged

# === UFFD Mode (for multiple concurrent clones) ===
# 3. Start UFFD memory server (serves pages on-demand, memory shared via page cache)
sudo ./fcvm snapshot serve nginx-warm

# 4. Clone from snapshot (~10ms restore, ~610ms with exec)
sudo ./fcvm snapshot run --pid <serve_pid> --name clone1 --network bridged
sudo ./fcvm snapshot run --pid <serve_pid> --name clone2 --network bridged

# 5. Clone with port forwarding (each clone can have unique ports)
sudo ./fcvm snapshot run --pid <serve_pid> --name web1 --network bridged --publish 8081:80
sudo ./fcvm snapshot run --pid <serve_pid> --name web2 --network bridged --publish 8082:80
# Get the host IP from fcvm ls --json, then curl it:
#   curl $(./fcvm ls --json | jq -r '.[] | select(.name=="web1") | .config.network.host_ip'):8081

# 6. Clone and execute command (auto-cleans up after)
sudo ./fcvm snapshot run --pid <serve_pid> --network bridged --exec "curl localhost"
# Or in direct mode:
sudo ./fcvm snapshot run --snapshot nginx-warm --network bridged --exec "curl localhost"
```

---

## Advanced Demos

| Demo | What it proves |
|------|----------------|
| **Clone Speed** | ~10ms memory restore, ~610ms full cycle |
| **Memory Sharing** | 10 clones use ~1.5GB extra, not 20GB |
| **Scale-Out** | 50+ VMs with ~7GB memory, not 100GB |
| **Privileged Container** | mknod and device access work |
| **Multiple Ports** | Comma-separated port mappings |
| **Multiple Volumes** | Comma-separated volume mappings with :ro |

### Clone Speed Breakdown

Clone timing measured on c7g.metal ARM64 with `RUST_LOG=debug`:

| Step | Time | Description |
|------|------|-------------|
| State lookup | ~1ms | Find serve process |
| Namespace spawn | ~6ms | `unshare --user --map-root-user --net` |
| CoW disk reflink | ~31ms | btrfs instant copy |
| Network setup | ~35ms | TAP device, iptables rules |
| Firecracker spawn | ~6ms | Start VM process |
| **Snapshot load (UFFD)** | **~9ms** | Load memory from server |
| Disk patch | <1ms | Point to CoW disk |
| **VM resume** | **<1ms** | Resume vCPUs |
| fc-agent recovery | ~100ms | ARP flush, kill stale TCP |
| Exec connect | ~20ms | Connect to guest vsock |
| Command + cleanup | ~300ms | Run echo + shutdown |
| **Total** | **~610ms** | Full clone cycle with exec |

The **core VM restore** (snapshot load + resume) is just **~10ms**. The remaining time is network setup, guest agent recovery, and cleanup.

#### 10-Clone Test Results

Validated with 10 sequential clones from the same memory server:

| Metric | Average | Range |
|--------|---------|-------|
| **Snapshot load (UFFD)** | 9.08ms | 8.76-9.56ms |
| **VM resume** | 0.48ms | 0.44-0.56ms |
| **Core VM restore** | ~9.5ms | — |
| **Full clone cycle** | 611ms | 587-631ms |

Individual clone times: 631, 599, 611, 611, 615, 618, 618, 622, 587, 599ms

#### 10-Clone Parallel Test Results

All 10 clones launched simultaneously:

| Metric | Value |
|--------|-------|
| **Wall clock time** | **1.03s** |
| **Snapshot load (UFFD)** | 9-11ms (consistent under load) |
| **Individual clone times** | 743-1024ms |

**Key findings**:
- **Core restore is fast**: ~10ms regardless of sequential or parallel execution
- **UFFD scales well**: Single memory server handles 10 concurrent clones with minimal overhead
- **Parallelism works**: 10 VMs in 1.03s (not 10× sequential time)
- **Bottleneck is cleanup**: Network teardown and state deletion add latency under contention

**Demo: Time a clone cycle**

```bash
# Setup: Create baseline and snapshot (rootless mode)
./fcvm podman run --name baseline nginx:alpine
./fcvm snapshot create baseline --tag nginx-warm
./fcvm snapshot serve nginx-warm  # Note the serve PID

# Time a clone startup (includes exec and cleanup)
time ./fcvm snapshot run --pid <serve_pid> --exec "echo ready"
# real 0m0.610s  ← 610ms total, ~10ms for VM restore
```

### Memory Sharing Proof

Show that multiple clones share memory via kernel page cache:

```bash
# Check baseline memory
free -m | grep Mem

# Start 10 clones from same snapshot
for i in {1..10}; do
  ./fcvm snapshot run --pid <serve_pid> --name clone$i &
done
wait

# Memory barely increased! 10 VMs share the same pages
free -m | grep Mem
```

### Scale-Out Demo (50 VMs in ~3s)

Spin up a fleet of web servers quickly:

```bash
# Create warm nginx snapshot (one-time, in another terminal)
./fcvm podman run --name baseline --publish 8080:80 nginx:alpine
# Once healthy, in another terminal:
./fcvm snapshot create baseline --tag nginx-warm
./fcvm snapshot serve nginx-warm  # Note serve PID

# Spin up 50 nginx instances in parallel
time for i in {1..50}; do
  ./fcvm snapshot run --pid <serve_pid> --name web$i --publish $((8080+i)):80 &
done
wait
# real 0m3.1s  ← 50 VMs in ~3 seconds

# Verify all running
./fcvm ls | wc -l  # 51 (50 clones + 1 baseline)

# Test a clone (use loopback IP from ./fcvm ls --json)
curl -s 127.0.0.10:8090 | head -5
```

### Privileged Container (Device Access)

Run containers that need mknod or device access:

```bash
# Privileged mode allows mknod, /dev access, etc.
sudo ./fcvm podman run --name dev --privileged \
  --cmd "sh -c 'mknod /dev/null2 c 1 3 && ls -la /dev/null2'" \
  public.ecr.aws/docker/library/alpine:latest
# Output: crw-r--r-- 1 root root 1,3 /dev/null2
```

### Multiple Ports and Volumes

Expose multiple ports and mount multiple volumes in one command:

```bash
# Multiple port mappings (comma-separated)
./fcvm podman run --name multi-port \
  --publish 8080:80,8443:443 \
  nginx:alpine

# Multiple volume mappings (comma-separated, with read-only)
./fcvm podman run --name multi-vol \
  --map /tmp/logs:/logs,/tmp/data:/data:ro \
  nginx:alpine

# Combined
./fcvm podman run --name full \
  --publish 8080:80,8443:443 \
  --map /tmp/html:/usr/share/nginx/html:ro \
  --env NGINX_HOST=localhost,NGINX_PORT=80 \
  nginx:alpine
```

---

## Interactive Mode & TTY

fcvm supports full interactive terminal sessions, matching docker/podman's `-i` and `-t` flags:

| Flag | Meaning | Use Case |
|------|---------|----------|
| `-i` | Keep stdin open | Pipe data to container |
| `-t` | Allocate pseudo-TTY | Colors, line editing |
| `-it` | Both | Interactive shell |

### Interactive Shell Examples

```bash
# Run interactive shell in container
./fcvm podman run --name shell -it alpine:latest sh

# Run vim (full TTY - arrow keys, escape sequences work)
./fcvm podman run --name editor -it alpine:latest vi /tmp/test.txt

# Run shell in existing VM
./fcvm exec --name web1 -it -- sh

# Pipe data (use -i without -t)
echo "hello" | ./fcvm podman run --name pipe -i alpine:latest cat
```

### How It Works

1. **Host side**: Sets terminal to raw mode, captures all input
2. **Protocol**: Binary framed protocol over vsock (handles escape sequences, control chars)
3. **Guest side**: Allocates PTY, connects container stdin/stdout

**Supported**:
- Escape sequences (colors, cursor movement)
- Control characters (Ctrl+C, Ctrl+D, Ctrl+Z)
- Line editing in shells
- Full-screen apps (vim, htop, less)

**Not yet implemented**:
- Window resize (SIGWINCH) - terminal size is fixed at session start

---

## Nested Virtualization

> ⚠️ **Experimental Feature**: Nested virtualization (L2+) is experimental. While basic functionality works, there are known stability issues under high I/O load. See [Known Issues](#known-issues-nested) below.

fcvm supports running VMs inside VMs using ARM64 FEAT_NV2 nested virtualization. Currently **one level of nesting works**: Host → L1 VM with full KVM support.

```
┌─────────────────────────────────────────────────────────┐
│  Host (bare metal c7g.metal)                            │
│  ┌───────────────────────────────────────────────────┐  │
│  │  L1 VM (fcvm + nested kernel profile)             │  │
│  │  ┌─────────────────────────────────────────────┐  │  │
│  │  │  L2 VM (fcvm inside L1)                     │  │  │
│  │  │  - Runs containers                          │  │  │
│  │  │  - Full VM isolation                        │  │  │
│  │  └─────────────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

**What Works**: Host → L1 → L2 nesting is fully functional. The `arm64.nv2` kernel boot parameter enables recursive KVM (`KVM_CAP_ARM_EL2=1`).

**Limitation**: L3+ nesting (L1 → L2 → L3...) is blocked by FUSE-over-FUSE latency. Each nesting level adds ~3-5 seconds per filesystem request due to the multi-hop FUSE chain. See `.claude/CLAUDE.md` for technical details.

### Requirements

| Requirement | Details |
|-------------|---------|
| **Hardware** | ARM64 with FEAT_NV2 (Graviton3+: c7g.metal, c7gn.metal, r7g.metal) |
| **Host kernel** | 6.18+ with `kvm-arm.mode=nested` boot parameter |
| **Nested kernel** | Pre-built from releases or `fcvm setup --kernel-profile nested --build-kernels` |
| **Firecracker** | Fork with NV2 support (configured via kernel profile) |

### Setting Up an EC2 Instance for Nested Virtualization

**Step 1: Launch a metal instance**

```bash
# Must be a metal instance for FEAT_NV2 hardware support
# Recommended: c7g.metal, m7g.metal, r7g.metal (Graviton3)
aws ec2 run-instances \
    --instance-type c7g.metal \
    --image-id ami-0xyz...  # Ubuntu 24.04 ARM64
```

**Step 2: Install fcvm and set up host kernel**

```bash
# Install fcvm (or build from source)
cargo install fcvm

# Download nested kernel profile and install as host kernel
# This also configures GRUB with kvm-arm.mode=nested
sudo ./fcvm setup --kernel-profile nested --install-host-kernel

# Reboot into the new kernel
sudo reboot
```

**Step 3: Verify nested KVM is enabled**

```bash
# Check kernel version
uname -r  # Should show 6.18-nested

# Check nested mode is enabled
cat /sys/module/kvm/parameters/mode  # Should show "nested"

# Verify KVM works
ls -la /dev/kvm
```

<details>
<summary>Manual kernel build (alternative)</summary>

If you prefer to build the host kernel manually:

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

# Configure GRUB
sudo sed -i 's/GRUB_CMDLINE_LINUX_DEFAULT="/GRUB_CMDLINE_LINUX_DEFAULT="kvm-arm.mode=nested /' /etc/default/grub
sudo update-grub
sudo reboot
```

</details>

### Getting the Nested Kernel

> **Note**: If you followed "Setting Up an EC2 Instance" above, the kernel is already downloaded. This section is for users who already have a host with nested KVM enabled.

```bash
# Download pre-built kernel from GitHub releases (~20MB)
./fcvm setup --kernel-profile nested

# Kernel will be at /mnt/fcvm-btrfs/kernels/vmlinux-nested-6.18-aarch64-*.bin
```

Or build locally (takes 10-20 minutes):
```bash
./fcvm setup --kernel-profile nested --build-kernels
```

The nested kernel (6.18) includes:
- **CONFIG_KVM=y** - KVM hypervisor for nested virtualization
- **EL2 support** - ARM Exception Level 2 (hypervisor mode)
- **MMFR4 patch** - Enables `arm64.nv2` boot param for NV2 capability
- **FUSE** - For volume mounts between host and guest
- **Networking** - TUN/VETH/netfilter for bridged networking in nested VMs

### Running Nested VMs

**Step 1: Start outer VM with nested kernel profile**
```bash
# Uses nested kernel profile from rootfs-config.toml
sudo ./fcvm podman run \
    --name outer-vm \
    --network bridged \
    --kernel-profile nested \
    --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map /path/to/fcvm/binary:/opt/fcvm \
    nginx:alpine
```

**Step 2: Verify nested KVM works**
```bash
# Check guest sees HYP mode
./fcvm exec --pid <outer_pid> --vm -- dmesg | grep -i kvm
# Should show: "kvm [1]: VHE mode initialized successfully"

# Verify /dev/kvm is accessible
./fcvm exec --pid <outer_pid> --vm -- ls -la /dev/kvm
```

**Step 3: Run inner VM**
```bash
# Inside outer VM (via exec or SSH)
cd /mnt/fcvm-btrfs
/opt/fcvm/fcvm podman run --name inner-vm --network bridged alpine:latest echo "Hello from nested VM!"
```

### How It Works

1. **FCVM_NV2=1** environment variable (auto-set when `--kernel-profile nested` is used) triggers fcvm to pass `--enable-nv2` to Firecracker
2. **HAS_EL2 + HAS_EL2_E2H0** vCPU features are enabled
   - HAS_EL2 (bit 7): Enables virtual EL2 for guest
   - HAS_EL2_E2H0 (bit 8): Forces nVHE mode (avoids timer trap storm)
3. **vCPU boots at EL2h** so guest kernel's `is_hyp_mode_available()` returns true
4. **EL2 registers initialized**: HCR_EL2, CNTHCTL_EL2, VMPIDR_EL2, VPIDR_EL2
5. Guest kernel initializes KVM: "CPU: All CPU(s) started at EL2"
6. Nested fcvm creates VMs using the guest's KVM

### Testing Nested Virtualization

```bash
# Run nested virtualization tests
make test-root FILTER=kvm

# Tests:
# - test_kvm_available_in_vm: Verifies /dev/kvm works in guest with nested profile
# - test_nested_run_fcvm_inside_vm: Full test of running fcvm inside fcvm
# - test_nested_l2: Full L1→L2 nesting with benchmarks at each level
```

### Nested Performance Benchmarks

Performance at each nesting level (measured on c7g.metal, ARM64 Graviton3):

| Metric | L1 (Host→VM) | L2 (VM→VM) | Overhead |
|--------|-------------|------------|----------|
| **Egress (curl)** | ✓ | ✓ | — |
| **Local Write** (10MB sync) | 4ms | 16ms | 4x |
| **Local Read** (10MB) | 2ms | 14ms | 7x |
| **FUSE Write** (10MB sync) | 83ms | 295ms | 3.6x |
| **FUSE Read** (10MB) | 45ms | 226ms | 5x |
| **FUSE Stat** (per-op) | 1.1ms | 5.3ms | 4.8x |
| **Copy TO FUSE** (100MB) | 1078ms (92 MB/s) | 7789ms (12 MB/s) | **7.2x** |
| **Copy FROM FUSE** (100MB) | 398ms (250 MB/s) | 2227ms (44 MB/s) | **5.6x** |
| **Memory Used** | 399MB | 341MB | — |

**Key observations:**
- **~5-7x FUSE overhead** at L2 due to FUSE-over-FUSE chaining (L2 → L1 → Host)
- **Large copies** show sustained throughput: 92 MB/s at L1, 12 MB/s at L2 (write) / 44 MB/s (read)
- **Local disk** overhead is lower (~4-7x) since it only traverses the virtio block device
- **Memory** is similar at each level (~350-400MB for the nested container image)

**Why L3+ is blocked:** Each additional nesting level adds another FUSE hop. At L3, a single stat() would take ~25ms (5x × 5x = 25x overhead), making container startup take 10+ minutes.

#### Network Performance (iperf3)

Egress/ingress throughput measured with iperf3 (3-second tests, various block sizes and parallelism):

| Direction | Block Size | Streams | L1 | L2 | Overhead |
|-----------|------------|---------|----|----|----------|
| **Egress** (VM→Host) | 128K | 1 | 42.4 Gbps | 11.0 Gbps | 3.9x |
| | 128K | 4 | 38.0 Gbps | 12.8 Gbps | 3.0x |
| | 1M | 1 | 43.1 Gbps | 9.0 Gbps | 4.8x |
| | 1M | 8 | 33.1 Gbps | 12.3 Gbps | 2.7x |
| **Ingress** (Host→VM) | 128K | 1 | 48.7 Gbps | 8.4 Gbps | 5.8x |
| | 128K | 4 | 44.3 Gbps | 8.6 Gbps | 5.2x |
| | 1M | 1 | 53.4 Gbps | 11.7 Gbps | 4.6x |
| | 1M | 8 | 43.0 Gbps | 10.4 Gbps | 4.1x |

**Network observations:**
- **L1 achieves 40-53 Gbps** - excellent virtio-net performance
- **L2 achieves 8-13 Gbps** - ~4-5x overhead from double NAT chain
- **Single stream often outperforms parallel** - likely virtio queue contention
- **Egress slightly faster than ingress at L2** - asymmetric NAT path

### Limitations

- ARM64 only (x86_64 nested virt uses different mechanism)
- Requires bare-metal instance (c7g.metal) or host with nested virt enabled
- L3+ nesting blocked by FUSE-over-FUSE latency (~5x per level)

### L2 Cache Coherency Fix

**Background**: Under NV2 nested virtualization, L2 FUSE writes could corrupt when using large packet sizes (~1MB). The root cause was missing cache synchronization at nested guest exit - L2's writes to the virtio ring weren't visible to L1's mmap reads.

**Solution**: A kernel patch adds a DSB SY (Data Synchronization Barrier) in `kvm_nested_sync_hwstate()` to ensure L2's writes are visible to L1 before returning from the nested guest exit handler.

The patch is at `kernel/patches/nv2-vsock-cache-sync.patch` and is automatically applied when building the nested kernel.

**Test**: 100MB file copies through FUSE-over-FUSE complete successfully with unbounded max_write:
```bash
make test-root FILTER=nested_l2_with_large
```

### Known Issues (Nested) {#known-issues-nested}

- **L3+ nesting**: Blocked by FUSE-over-FUSE latency (~5x per level). Each additional nesting level adds 3-5 seconds per filesystem request.
- **Nested tests disabled**: L2/L3 nested tests are currently disabled in CI due to timing sensitivity and flakiness under NV2. The tests pass individually but are slow (~5 min each) and occasionally timeout. Run manually with `make test-root FILTER=nested` if needed.

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
| `fcvm snapshot run` | Clone from snapshot (`--pid` for UFFD, `--snapshot` for direct) |
| `fcvm snapshots` | List available snapshots |

See [DESIGN.md](DESIGN.md#cli-interface) for architecture and design decisions.

### Key Options

**`fcvm podman run`** - Essential options:
```
--name <NAME>       VM name (required)
--network <MODE>    rootless (default) or bridged (needs sudo)
--publish <H:G>     Port forward host:guest (e.g., 8080:80)
--map <H:G[:ro]>    Volume mount host:guest (optional :ro for read-only)
--env <K=V>         Environment variable
-i, --interactive   Keep stdin open (for piping input)
-t, --tty           Allocate pseudo-TTY (for vim, colors, etc.)
--setup             Auto-setup if kernel/rootfs missing (rootless only)
--no-snapshot       Disable automatic snapshot creation (for testing)
```

**`fcvm exec`** - Execute in VM/container:
```bash
./fcvm exec --name my-vm -- cat /etc/os-release     # In container
./fcvm exec --name my-vm --vm -- curl -s ifconfig.me # In guest OS
./fcvm exec --name my-vm -it -- bash                 # Interactive shell
```

---

## Network Modes

| Mode | Flag | Root | Notes |
|------|------|------|-------|
| Rootless | `--network rootless` (default) | No | slirp4netns, no root needed |
| Bridged | `--network bridged` | Yes | iptables NAT |

### Host Service Access (Rootless Mode)

In rootless mode, VMs can reach services on the host via slirp4netns gateways:

| Host Address | VM Uses | Description |
|--------------|---------|-------------|
| `127.0.0.1` | `10.0.2.2` | IPv4 loopback gateway |
| `::1` | `fd00::2` | IPv6 loopback gateway |

#### IPv6 from Inside VMs

VMs have full IPv6 support via slirp4netns. To reach host services bound to `::1`:

```bash
# From inside the VM/container, use fd00::2 to reach host's ::1
wget http://[fd00::2]:8080/    # Reaches host's [::1]:8080
curl http://[fd00::2]:3000/    # Reaches host's [::1]:3000
```

The VM's internal IPv6 address is `fd00:1::2` on the `fd00:1::/64` network.

#### Using HTTP Proxies

**Automatic Proxy Passthrough**: fcvm automatically forwards `http_proxy` and `https_proxy`
environment variables from the host to the VM via MMDS. The VM's podman process inherits
these settings for image pulls:

```bash
# Set proxy on host - fcvm passes it to VM automatically
export http_proxy=http://[fd00::2]:8080
export https_proxy=http://[fd00::2]:8080
fcvm podman run --name myvm alpine:latest
# Image pulls inside VM will use the proxy
```

**Manual Proxy Configuration**: You can also configure proxies inside the VM manually.
The proxy binds to the host's loopback, and the VM connects via the gateway address:

```bash
# On host: start proxy listening on ::1:8080 (or 127.0.0.1:8080)

# Inside VM: configure proxy using gateway address
export http_proxy=http://[fd00::2]:8080   # For IPv6 proxy
export http_proxy=http://10.0.2.2:8080    # For IPv4 proxy

# Now HTTP requests go through the proxy
wget http://example.com/
```

**Note**: The VM uses `fd00::2` or `10.0.2.2` (gateway addresses), not `::1` or `127.0.0.1`
(which would be the VM's own loopback).

See [DESIGN.md](DESIGN.md#networking) for architecture details.

---

## Container Behavior

- **Exit codes**: Container exit code forwarded to host via vsock
- **Logs**: Container stdout goes to host stdout, stderr to host stderr (clean output for scripting)
- **Health**: Default uses vsock ready signal; optional `--health-check` for HTTP

See [DESIGN.md](DESIGN.md#guest-agent) for details.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `FCVM_BASE_DIR` | `/mnt/fcvm-btrfs` | Base directory for all data |
| `RUST_LOG` | `warn` | Logging level (quiet by default; use `info` or `debug` for verbose) |
| `FCVM_NO_SNAPSHOT` | unset | Set to `1` to disable automatic snapshot creation (same as `--no-snapshot` flag) |
| `FCVM_NO_WRITEBACK_CACHE` | unset | Set to `1` to disable FUSE writeback cache (see below) |
| `FCVM_NO_XATTR_FASTPATH` | unset | Set to `1` to disable security.capability xattr fast path |

### FUSE Writeback Cache

FUSE writeback cache is **enabled by default** for ~9x write performance improvement. The kernel batches writes and flushes them asynchronously, dramatically improving throughput for workloads with many small writes.

**Known POSIX edge cases** (disabled in pjdfstest):

| Test | Issue | Workaround |
|------|-------|------------|
| `open` (3/144 fail) | O_WRONLY promoted to O_RDWR, requires read permission | Use `0644` instead of `0200` for write-only files |
| `utimensat` (1/122 fail) | Needs kernel patch with `default_permissions` | Use nested kernel profile which has the patch |

To disable writeback cache for debugging:
```bash
FCVM_NO_WRITEBACK_CACHE=1 ./fcvm podman run --name test alpine:latest
```

---

## Testing

### CI Summary

Every CI run exercises the full stack:

| Metric | Count |
|--------|-------|
| **Total Tests** | 9,290 |
| **Nextest Functions** | 501 |
| **POSIX Compliance (pjdfstest)** | 8,789 |
| **VMs Spawned** | 331 (92 base + 239 clones) |
| **UFFD Memory Servers** | 28 |
| **pjdfstest Categories** | 17 |

Performance (on c7g.metal ARM64):
- **Clone to healthy**: 0.67s average (see [Clone Speed Breakdown](#clone-speed-breakdown))
- **Snapshot creation**: 40.7s average
- **Total test time**: ~13 minutes (parallel jobs)

### Test Categories

| Category | Description | VMs | Tests |
|----------|-------------|-----|-------|
| **Unit Tests** | CLI parsing, state manager, protocol serialization | 0 | ~50 |
| **FUSE Tests** | fuse-pipe passthrough, permissions, mount/unmount | 0 | ~80 |
| **VM Sanity** | Basic VM lifecycle, networking, exec | ~20 | ~30 |
| **Snapshot/Clone** | UFFD memory sharing, btrfs reflinks, 100-clone scaling | ~230 | ~20 |
| **pjdfstest** | POSIX filesystem compliance in VMs | 17 | 8,789 |
| **Egress/Port Forward** | Network connectivity, port mapping | ~30 | ~40 |
| **Disk Mounts** | RO/RW disks, directory mapping, NFS | ~10 | ~15 |
| **Nested KVM** | L1→L2 virtualization (ARM64 NV2) | 2 | ~5 |

### Test Tiers

Tests are organized into tiers by privilege requirements:

```bash
make test-unit   # Unit tests only (no VMs, no sudo)
make test-fast   # + quick VM tests (rootless, no sudo)
make test-all    # + slow VM tests (rootless, no sudo)
make test-root   # + privileged tests (bridged, pjdfstest, sudo)
make test        # Alias for test-root
```

Container equivalents:
```bash
make container-test-unit   # Unit tests in container
make container-test        # All tests in container (recommended)
```

### Running Tests

```bash
# Build first
make build

# Run all tests (requires sudo + KVM)
make test-root

# Filter by name pattern
make test-root FILTER=exec

# Live output (stream as tests run)
make test-root FILTER=sanity STREAM=1

# Single test with debug logging
RUST_LOG=debug make test-root FILTER=test_exec_basic STREAM=1
```

### CI Workflow

Tests run automatically on PRs and pushes to main:

| Job | Runner | Tests |
|-----|--------|-------|
| **Host** | Self-hosted ARM64 | Unit tests, quick VM tests (rootless) |
| **Host-Root-SnapshotDisabled** | Self-hosted ARM64 | Privileged tests with `FCVM_NO_SNAPSHOT=1` |
| **Host-Root-SnapshotEnabled** | Self-hosted ARM64 | Privileged tests run **twice** to verify snapshot hit |
| **Container** | Self-hosted ARM64 | All tests in container |

The **SnapshotEnabled** job runs the full test suite twice on the same runner:
- **Run 1**: Creates snapshots (cache miss path)
- **Run 2**: Uses existing snapshots (cache hit path - should be faster)

This validates the complete snapshot lifecycle: creation, persistence, and restoration.

Latest results: [CI Workflow](.github/workflows/ci.yml) → Actions tab

Analyze any CI run locally:
```bash
python3 scripts/analyze_ci_vms.py              # Latest run
python3 scripts/analyze_ci_vms.py <run_id>     # Specific run
```

### Debugging Tests

Enable tracing:
```bash
RUST_LOG="passthrough=debug,fuse_pipe=info" sudo -E cargo test ...
```

Check running VMs:
```bash
./fcvm ls
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

## Kernels and Base Images

fcvm uses a config-driven approach for kernels and base images. All configuration is in `rootfs-config.toml`.

### Default Kernel

The default kernel is from [Kata Containers](https://github.com/kata-containers/kata-containers):

| Property | Value |
|----------|-------|
| **Version** | 6.12.47 |
| **Source** | Kata 3.24.0 release |
| **Key Config** | `CONFIG_FUSE_FS=y` (required for volume mounts) |
| **Architectures** | arm64, amd64 |

The kernel is downloaded automatically during `fcvm setup` and cached by URL hash. Changing the URL in config triggers a re-download.

### Base Image

The guest OS is Ubuntu 24.04 LTS (Noble Numbat):

| Property | Value |
|----------|-------|
| **Version** | 24.04 LTS |
| **Source** | Ubuntu cloud images |
| **Packages** | podman, crun, fuse-overlayfs, skopeo, fuse3, haveged, chrony |

The rootfs is built automatically during `fcvm setup` and cached by script SHA. Changing packages, services, or files in config triggers a rebuild.

### Kernel Profiles

For advanced use cases (like nested virtualization), fcvm supports **kernel profiles**. Profiles define:

- Custom kernel with specific configuration
- Optional custom Firecracker binary
- Boot arguments and runtime settings

**Current profiles:**

| Profile | Architecture | Description |
|---------|--------------|-------------|
| `nested` | arm64 | Nested virtualization (NV2) with CONFIG_KVM=y |

Usage:
```bash
# Download/build kernel for profile
./fcvm setup --kernel-profile nested

# Run VM with profile
sudo ./fcvm podman run --name vm1 --kernel-profile nested --privileged nginx:alpine
```

### Adding a New Kernel Profile

To add a custom kernel profile, edit `rootfs-config.toml`:

```toml
# Example: Add a minimal kernel profile for amd64
[kernel_profiles.minimal.amd64]
description = "Minimal kernel for fast boot"
kernel_version = "6.12"
kernel_repo = "your-org/your-kernel-repo"

# Files that determine kernel SHA (supports globs)
# When any of these change, kernel is rebuilt
build_inputs = [
    "kernel/minimal.conf",
    "kernel/patches/*.patch",
]

# Build paths (relative to repo root)
kernel_config = "kernel/minimal.conf"
patches_dir = "kernel/patches"

# Optional: Custom Firecracker binary
# firecracker_bin = "/usr/local/bin/firecracker-custom"

# Optional: Extra boot arguments
boot_args = "quiet"
```

**Key fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `kernel_version` | Yes | Kernel version (e.g., "6.18.3") |
| `kernel_repo` | Yes | GitHub repo for releases (e.g., "ejc3/firepod") |
| `build_inputs` | Yes | Files to hash for kernel SHA (supports globs) |
| `kernel_config` | No | Kernel .config file path |
| `patches_dir` | No | Directory containing kernel patches |
| `firecracker_bin` | No | Custom Firecracker binary path |
| `firecracker_args` | No | Extra Firecracker CLI args |
| `boot_args` | No | Extra kernel boot parameters |

**How it works:**

1. **Config is source of truth**: All kernel versions and build configuration flow from `rootfs-config.toml`
2. **SHA computation**: fcvm hashes all files matching `build_inputs` patterns
3. **Download first**: Tries to download from `kernel_repo` releases with tag `kernel-{profile}-{version}-{arch}-{sha}`
4. **Dynamic build scripts**: If download fails and `--build-kernels` is set, Rust generates build scripts on-the-fly (no shell scripts in source control)
5. **Config sync**: `make build` automatically syncs embedded config to `~/.config/fcvm/` so runtime matches compile-time config

### Customizing the Base Image

The rootfs is built from `rootfs-config.toml` sections:

```toml
[base]
version = "24.04"
codename = "noble"

[packages]
runtime = ["podman", "crun", "fuse-overlayfs", "skopeo"]
fuse = ["fuse3"]
system = ["haveged", "chrony"]
debug = ["strace"]

[services]
enable = ["haveged", "chrony", "systemd-networkd"]
disable = ["snapd", "cloud-init"]

[files."/etc/myconfig"]
content = """
my custom config
"""
```

After changing the config, run `fcvm setup` to rebuild the rootfs with the new SHA.

---

## Troubleshooting

### "fcvm binary not found"
- Build fcvm first: `make build`
- Or set PATH: `export PATH=$PATH:./target/release`

### "timeout waiting for VM to become healthy"
- Check VM logs: `./fcvm ls --json`
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

## CI Infrastructure

CI runs on self-hosted ARM64 runners (c7g.metal spot instances) managed by [ejc3/aws-setup](https://github.com/ejc3/aws-setup).

- **Auto-scaling**: Runners launch on demand, stop after 30 mins idle
- **Hardware**: c7g.metal with /dev/kvm for VM tests
- **Cost**: ~$0.50/hr spot pricing, $0 when idle

### Claude Code Review

PRs are automatically reviewed by Claude. Reviews are blocking if critical issues are found.

| Trigger | Description |
|---------|-------------|
| **Auto** | PRs from org members are reviewed automatically |
| `/claude-review` | Comment on any PR to trigger manual review |
| `@claude ...` | Ask Claude questions in PR comments |

Reviews check for security issues, bugs, and breaking changes. Issues prefixed with `BLOCKING:` will fail the status check.

---

## License

MIT License - see [LICENSE](LICENSE) for details.
