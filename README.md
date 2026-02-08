# fcvm - Firecracker VM Manager

Run Podman containers in Firecracker microVMs with fast cloning via UFFD memory sharing and btrfs CoW snapshots.

> **Features**
> - Run OCI containers in isolated Firecracker microVMs
> - **~6x faster startup** with container image cache (540ms vs 3100ms)
> - VM cloning via UFFD memory server + btrfs reflinks (~10ms restore, ~610ms with exec)
> - Multiple VMs share memory via kernel page cache (50 VMs = ~512MB, not 25GB!)
> - Dual networking: bridged (iptables) or rootless (slirp4netns)
> - Port forwarding for both regular VMs and clones
> - FUSE-based host directory mapping via fuse-pipe
> - Container exit code forwarding
> - Interactive shell support (`-it`) with full TTY (vim, editors, colors)
> - HTTP API server (`fcvm serve`) — ComputeSDK-compatible gateway for programmatic sandbox management

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
- btrfs filesystem at `/mnt/fcvm-btrfs` (native btrfs used directly; loopback created on non-btrfs hosts)
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

See [`Containerfile`](Containerfile) for the full dependency list used in CI.

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

No persistent diff chains — reflink copy is instant (btrfs CoW), diff contains ~2% of pages, merged onto base. Each snapshot has a complete memory.bin with no parent dependency.

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

Clone snapshots automatically use their source as parent, enabling diff-based optimization across the chain.

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

Core VM restore (snapshot load + resume) is ~10ms. The rest is network setup, agent recovery, and cleanup. 10 parallel clones complete in ~1s wall clock (not 10x sequential). See [PERFORMANCE.md](PERFORMANCE.md) for detailed clone benchmarks.

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

fcvm supports interactive terminal sessions, matching docker/podman's `-i` and `-t` flags:

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

fcvm supports running VMs inside VMs using ARM64 FEAT_NV2. Host → L1 → L2 nesting works. L3+ is blocked by FUSE-over-FUSE latency (~5x per level).

| Requirement | Details |
|-------------|---------|
| **Hardware** | ARM64 with FEAT_NV2 (Graviton3+: c7g.metal) |
| **Host kernel** | 6.18+ with `kvm-arm.mode=nested` |
| **Nested kernel** | `fcvm setup --kernel-profile nested` |

```bash
# Setup host kernel (one-time)
sudo ./fcvm setup --kernel-profile nested --install-host-kernel
sudo reboot

# Start outer VM with nested kernel
sudo ./fcvm podman run \
    --name outer --network bridged \
    --kernel-profile nested --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    nginx:alpine

# Run inner VM (inside outer)
./fcvm exec --pid <outer_pid> --vm -- \
    /opt/fcvm/fcvm podman run --name inner --network bridged alpine:latest echo "nested!"
```

**Performance**: ~5-7x FUSE overhead at L2, local disk ~4x. L2 VMs limited to single vCPU (NV2 multi-vCPU interrupt issue). See [PERFORMANCE.md](PERFORMANCE.md#nested-virtualization) for benchmarks and [NESTED.md](NESTED.md) for setup details.

```bash
make test-root FILTER=kvm   # Run nested virtualization tests
```

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
| `fcvm serve` | Start HTTP API server (ComputeSDK gateway) |
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

## ComputeSDK API (`fcvm serve`)

`fcvm serve` starts an HTTP server that speaks the ComputeSDK gateway + sandbox daemon protocol. This lets the TypeScript `computesdk` package (or any HTTP client) manage sandboxes programmatically.

```bash
# Start the API server
./fcvm serve --port 8090
```

### TypeScript SDK

```typescript
import { ComputeSDK } from 'computesdk';

const sdk = new ComputeSDK({
  provider: 'fcvm',
  apiKey: 'local',
  gatewayUrl: 'http://localhost:8090'
});

const sandbox = await sdk.sandbox.create({ runtime: 'python' });
const result = await sandbox.runCode('print("hello")');
console.log(result.output);  // "hello\n"
await sandbox.destroy();
```

### API Endpoints

**Gateway** (sandbox lifecycle):

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/sandboxes` | Create sandbox (`{ runtime: "python" }`) |
| `GET` | `/v1/sandboxes` | List all sandboxes |
| `GET` | `/v1/sandboxes/{id}` | Get sandbox details |
| `DELETE` | `/v1/sandboxes/{id}` | Destroy sandbox |

**Sandbox daemon** (per-sandbox operations):

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/s/{id}/health` | Health check |
| `GET` | `/s/{id}/ready` | Readiness check |
| `POST` | `/s/{id}/run/code` | Run code (`{ code, language? }`) |
| `POST` | `/s/{id}/run/command` | Run shell command (`{ command, cwd?, env? }`) |
| `GET` | `/s/{id}/files?path=` | List directory |
| `POST` | `/s/{id}/files` | Create file (`{ path, content }`) |
| `GET` | `/s/{id}/files/*path` | Read file |
| `HEAD` | `/s/{id}/files/*path` | Check file exists |
| `DELETE` | `/s/{id}/files/*path` | Delete file |

### curl Examples

```bash
# Create a Python sandbox
curl -s -X POST localhost:8090/v1/sandboxes \
  -H 'Content-Type: application/json' \
  -d '{"runtime":"python"}' | jq .

# Run code (use sandboxId from create response)
curl -s -X POST localhost:8090/s/<id>/run/code \
  -H 'Content-Type: application/json' \
  -d '{"code":"print(42)"}' | jq .

# Run a shell command
curl -s -X POST localhost:8090/s/<id>/run/command \
  -H 'Content-Type: application/json' \
  -d '{"command":"ls -la /"}' | jq .

# Write and read a file
curl -s -X POST localhost:8090/s/<id>/files \
  -H 'Content-Type: application/json' \
  -d '{"path":"/tmp/hello.txt","content":"hello world"}'
curl -s localhost:8090/s/<id>/files/tmp/hello.txt | jq .

# Destroy sandbox
curl -s -X DELETE localhost:8090/v1/sandboxes/<id> | jq .
```

### Supported Runtimes

| Runtime | Image |
|---------|-------|
| `python` | `python:3.12-slim` |
| `node` | `node:22-slim` |
| `ruby` | `ruby:3.3-slim` |
| `go` | `golang:1.23-alpine` |
| Custom | Pass any image name directly |

---

## Network Modes

| Mode | Flag | Root | Notes |
|------|------|------|-------|
| Rootless | `--network rootless` (default) | No | slirp4netns with bridge, IPv6 support |
| Bridged | `--network bridged` | Yes | iptables NAT, better performance |

**Rootless architecture**: Uses a Linux bridge (br0) for L2 forwarding between slirp4netns and Firecracker.
The bridge preserves MAC addresses for proper ARP/NDP learning, enabling IPv6 support.

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

fcvm forwards `http_proxy` and `https_proxy` from host to VM via MMDS:

```bash
# Set proxy on host - fcvm passes it to VM automatically
export http_proxy=http://[fd00::2]:8080
export https_proxy=http://[fd00::2]:8080
fcvm podman run --name myvm alpine:latest
# Image pulls inside VM will use the proxy
```

Manual configuration (proxy on host loopback, VM connects via gateway):

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


### FUSE Writeback Cache

FUSE writeback cache is **enabled by default** for ~9x write performance. The kernel batches writes and flushes asynchronously.

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

fcvm supports custom kernel profiles for advanced use cases (e.g., nested virtualization). Profiles define a kernel config, optional Firecracker binary, and boot arguments. Currently: `nested` (arm64, CONFIG_KVM=y).

```bash
./fcvm setup --kernel-profile nested                     # Download pre-built
./fcvm setup --kernel-profile nested --build-kernels     # Or build locally
sudo ./fcvm podman run --name vm1 --kernel-profile nested --privileged nginx:alpine
```

To add custom profiles or customize the base image, edit `rootfs-config.toml`. See [DESIGN.md](DESIGN.md#kernel-profiles) for profile configuration reference.

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

### Debugging Network Issues

Spawn quick one-off VMs with inline commands to diagnose network problems:

```bash
# Test connectivity incrementally: gateway → DNS → external
./target/release/fcvm podman run --name net-debug-$(date +%s) --privileged alpine:latest sh -c "
echo '=== Network config ==='
ip addr show eth0
ip route
cat /etc/resolv.conf
echo ''
echo '=== Gateway ==='
ping -c 2 -W 3 10.0.2.2 || echo 'gateway failed'
echo ''
echo '=== DNS ==='
nslookup example.com || echo 'DNS failed'
echo ''
echo '=== External ==='
wget -q -O - --timeout=10 http://ifconfig.me || echo 'external failed'
" 2>&1 &
sleep 60  # Wait for VM to boot and run commands
```

**Inspect namespace for running VM:**
```bash
HOLDER_PID=$(cat /mnt/fcvm-btrfs/state/*.json | jq -r '.holder_pid')
sudo nsenter --net=/proc/$HOLDER_PID/ns/net ip addr
sudo nsenter --net=/proc/$HOLDER_PID/ns/net bridge link  # Show bridge ports
```

### KVM not available
- Firecracker requires `/dev/kvm`
- On AWS: use c6g.metal or c5.metal (NOT c5.large or other regular instances)
- On other clouds: use bare-metal instances or hosts with nested virtualization

---

## Documentation

- `DESIGN.md` - Architecture, configuration reference, design decisions
- `PERFORMANCE.md` - Benchmarks, tuning, and tracing
- `NESTED.md` - Nested virtualization setup and details
- `.claude/CLAUDE.md` - Development notes, debugging tips
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
