# fcvm

fcvm runs Podman containers inside microVMs, with a pluggable hypervisor backend — Firecracker (default) or Cloud Hypervisor, selected with `--hypervisor firecracker|cloud-hypervisor`. You use the same images and registries as plain Podman, and each container gets its own kernel, so isolation comes from a VM boundary rather than from namespaces sharing the host kernel.

fcvm avoids paying the VM boot cost on every run: the first run of an image boots cold and saves a snapshot once the image is loaded, later runs restore from that snapshot, and a running VM can be cloned in milliseconds with copy-on-write memory and disk. `fcvm serve` exposes the same workflow over an HTTP API for programs that create sandboxes on demand.

- Cached startup takes ~540ms via snapshot restore, compared to ~3.1s for a cold boot (see [Container Image Cache](#container-image-cache))
- Cloning a running VM takes ~10ms using UFFD memory sharing and btrfs reflinks
- 50 clones of a 512MB VM share physical pages through the kernel page cache, using ~512MB total rather than 25GB
- Three network modes: rootless (no root required), bridged, and routed (native IPv6 at kernel line rate)
- Interactive containers behave like local ones: `-it`, colors, Ctrl+C, vim

Benchmarks were measured on c7g.metal ARM64. See [PERFORMANCE.md](PERFORMANCE.md) for methodology and results.

---

## Quick Start

```bash
# Build (~2 min)
git clone https://github.com/ejc3/fcvm && cd fcvm
make build

# Download kernel + build rootfs (~5 min first time, then cached)
sudo ./fcvm setup

# Run a container in a microVM
./fcvm podman run --name hello alpine:latest -- echo "Hello from microVM"

# Run a long-lived service
./fcvm podman run --name web nginx:alpine
# → VM boots, image loads, nginx starts. Ctrl+C to stop.

# In another terminal:
./fcvm ls                                        # List running VMs
./fcvm exec --name web -- cat /etc/os-release    # Exec into container
```

### Network Modes

```bash
# Rootless (default) — no sudo needed
./fcvm podman run --name web nginx:alpine

# Bridged — better performance, requires sudo
sudo ./fcvm podman run --name web --network bridged nginx:alpine

# Routed — IPv6 native at kernel line rate, requires sudo + IPv6 host
sudo ./fcvm podman run --name web --network routed nginx:alpine
```

| Mode | Flag | Root | How It Works |
|------|------|------|-------------|
| Rootless | `--network rootless` (default) | No | pasta L4 translation with bridge |
| Bridged | `--network bridged` | Yes | iptables NAT, network namespace |
| Routed | `--network routed` | Yes | veth + IPv6 routing, no userspace proxy |

### Common Options

```bash
# Port forwarding (host:guest)
./fcvm podman run --name web --publish 8080:80 nginx:alpine

# Mount host directory
./fcvm podman run --name app --map /host/data:/data alpine:latest

# Custom CPU/memory
./fcvm podman run --name big --cpu 4 --mem 4096 alpine:latest

# Multiple ports and volumes
./fcvm podman run --name full \
  --publish 8080:80,8443:443 \
  --map /tmp/html:/usr/share/nginx/html:ro \
  --env NGINX_HOST=localhost \
  nginx:alpine

# JSON output for scripting
./fcvm ls --json
```

---

## Snapshot & Clone Workflow

Snapshot a running VM and restore clones from it. Two modes: UFFD (memory server, many concurrent clones) or direct (simpler, single clone from file).

```bash
# 1. Start a baseline VM
./fcvm podman run --name baseline nginx:alpine

# 2. Snapshot it (pauses briefly, then resumes)
./fcvm snapshot create baseline --tag nginx-warm

# 3. Start memory server (serves pages on demand)
./fcvm snapshot serve nginx-warm

# 4. Clone — each takes ~10ms for VM restore, ~610ms end-to-end
./fcvm snapshot run --pid <serve_pid> --name clone1
./fcvm snapshot run --pid <serve_pid> --name clone2

# Or clone directly from file (simpler, no server needed)
./fcvm snapshot run --snapshot nginx-warm --name clone3

# One-shot: clone, run command, cleanup
./fcvm snapshot run --pid <serve_pid> --exec "curl localhost"
```

### Disk-Only (Cold-Boot) Clones

`--disk-only` captures just the disk — no vCPU pause, no memory image. The guest
filesystem is frozen (`fsfreeze`) for crash consistency, the disk is reflinked
(~ms), and the guest resumes. Clones cold-boot fresh from the captured disk: the
container storage and writable layer are preserved, the container restarts, and
per-machine identity (machine-id, SSH host keys) is regenerated.

```bash
./fcvm snapshot create --pid <vm_pid> --tag my-app --disk-only
./fcvm snapshot run --snapshot my-app --name clone1   # cold boot, no memory restore
```

This is also the stop→start lifecycle: capture, shut the VM down, start again later
from the captured disk. The sequence behaves identically to an in-place reboot —
same guest path (preserve storage, restart container, regenerate identity).

### In-Place Reboot

A guest `reboot` does not terminate the VM. The host relaunches the configured hypervisor backend
against the same disk, network, and listeners — the fcvm process and its PID stay
stable — and the guest comes back exactly like a disk-only clone cold boot:
storage preserved, container restarted, identity regenerated. A guest `poweroff`
still terminates the VM normally.

### Container Image Cache

After the first run with a given image, fcvm snapshots the VM state post-image-pull. Subsequent runs restore from snapshot instead of re-pulling — ~6x faster (540ms vs 3100ms).

```bash
./fcvm podman run --name web1 nginx:alpine   # First run: pulls image, creates cache
./fcvm podman run --name web2 nginx:alpine   # Second run: restores from cache
```

### Startup Snapshots

Use `--health-check` to snapshot the fully initialized application, not just the pulled image:

```bash
./fcvm podman run --name web --health-check http://localhost/ nginx:alpine
# First run: waits for health check, then snapshots the warm app
# Second run: restores with app already running
```

---

## Interactive Mode & TTY

fcvm supports `-i` and `-t` flags just like docker/podman:

```bash
./fcvm podman run --name shell -it alpine:latest sh          # Interactive shell
./fcvm podman run --name editor -it alpine:latest vi /tmp/x   # Full-screen apps work
./fcvm exec --name web -it -- bash                            # Shell in running VM
echo "data" | ./fcvm podman run --name pipe -i alpine:latest cat  # Pipe stdin
```

---

## Nested Virtualization

fcvm supports VMs inside VMs on ARM64 with FEAT_NV2 (Graviton3+).

```bash
# Setup (one-time)
sudo ./fcvm setup --kernel-profile nested --install-host-kernel && sudo reboot

# Outer VM with nested kernel
sudo ./fcvm podman run --name outer --network bridged \
    --kernel-profile nested --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map "$(pwd)":/opt/fcvm nginx:alpine

# Inner VM (inside outer)
./fcvm exec --pid <outer_pid> --vm -- \
    /opt/fcvm/fcvm podman run --name inner --network bridged alpine:latest echo "nested!"
```

L2 VMs have ~5-7x FUSE overhead and are limited to one vCPU. See [NESTED.md](NESTED.md) for details.

---

## ComputeSDK API

`fcvm serve` starts an HTTP server implementing the ComputeSDK gateway protocol:

```bash
./fcvm serve --port 8090
```

```typescript
import { ComputeSDK } from 'computesdk';

const sdk = new ComputeSDK({ provider: 'fcvm', apiKey: 'local', gatewayUrl: 'http://localhost:8090' });
const sandbox = await sdk.sandbox.create({ runtime: 'python' });
const result = await sandbox.runCode('print("hello")');
await sandbox.destroy();
```

<details>
<summary>API endpoints and curl examples</summary>

**Gateway** (sandbox lifecycle):

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/sandboxes` | Create sandbox |
| `GET` | `/v1/sandboxes` | List sandboxes |
| `GET` | `/v1/sandboxes/{id}` | Get sandbox |
| `DELETE` | `/v1/sandboxes/{id}` | Destroy sandbox |

**Sandbox daemon** (per-sandbox):

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/s/{id}/run/code` | Run code |
| `POST` | `/s/{id}/run/command` | Run shell command |
| `GET/POST/DELETE` | `/s/{id}/files/*` | File operations |
| `POST` | `/s/{id}/terminals` | Create terminal |
| `GET` | `/s/{id}` | WebSocket terminal |

```bash
# Create sandbox, run code, destroy
curl -s -X POST localhost:8090/v1/sandboxes -H 'Content-Type: application/json' -d '{"runtime":"python"}' | jq .
curl -s -X POST localhost:8090/s/<id>/run/code -H 'Content-Type: application/json' -d '{"code":"print(42)"}' | jq .
curl -s -X DELETE localhost:8090/v1/sandboxes/<id> | jq .
```

Supported runtimes: `python`, `node`, `ruby`, `go`, or any custom image name.

</details>

---

## Network Details

### Rootless Architecture

Uses [pasta](https://passt.top/) (from the passt project) with a Linux bridge for L2 forwarding between pasta and the VM. Pasta uses splice(2) zero-copy L4 translation. IPv6 is enabled automatically when the host has a global IPv6 address (auto-detected; no flag needed).

Host services are reachable from VMs via pasta gateways: `10.0.2.2` (IPv4) and `fd00::2` (IPv6).

### Routed Architecture

Connects VMs directly to the host via a veth pair with native IPv6 routing — no userspace proxy. Each VM gets a unique IPv6 derived from the host's /64 subnet. Requires a host with a global IPv6 address (e.g., AWS VPC with IPv6 enabled).

```
Host                            Namespace
eth0 (/64 subnet)
  |
veth-host ←──veth pair──→ veth-ns
  (proxy NDP)                 |
                           br0 (10.0.2.1/24, fd00::1/64)
                               |
                           tap-vm → microVM (10.0.2.100, unique IPv6)
```

### Egress Proxy

In rootless mode, outbound IPv4 TCP goes through a transparent egress proxy that multiplexes all connections over a single vsock. No configuration needed. In routed mode, all traffic goes native IPv6 — no proxy.

### Host Service Access

VMs can reach host services via gateway addresses (not `127.0.0.1`, which is the VM's own loopback):

| From VM | Reaches Host | Mode |
|---------|-------------|------|
| `10.0.2.2` | `127.0.0.1` (IPv4) | Rootless |
| `fd00::2` | `::1` (IPv6) | Rootless |

fcvm auto-forwards `http_proxy`/`https_proxy` from host to VM as part of the boot plan, delivered via the configured backend's metadata transport (Firecracker: MMDS; Cloud Hypervisor: the host-served vsock boot-plan on port 4995).

### Port Forwarding

`--publish` binds to a per-VM IP, not `0.0.0.0`. Use `./fcvm ls --json` to find the IP:
- **Rootless**: `config.network.loopback_ip` (e.g., `curl 127.0.0.2:8080`)
- **Bridged**: `config.network.host_ip` (e.g., `curl 172.30.0.1:8080`)
- **Routed**: `config.network.loopback_ip` (same as rootless)

---

## Prerequisites

**Hardware:**
- Linux with `/dev/kvm` — bare-metal or nested virtualization
- AWS: c6g.metal (ARM64) or c5.metal (x86_64)

**Dependencies:**
- Rust 1.75+ with musl target: `rustup target add $(uname -m)-unknown-linux-musl`
- Firecracker binary in PATH (default backend)
- Cloud Hypervisor (only if using `--hypervisor cloud-hypervisor`): build the pinned fork with `fcvm setup --cloud-hypervisor`, or set `FCVM_CLOUD_HYPERVISOR_BIN`, or put `cloud-hypervisor` in PATH (resolved in that order)
- For rootless: `passt` package (provides `pasta`)
- For bridged: sudo, iptables, iproute2
- For routed: sudo, iproute2, host with global IPv6 /64 (ip6tables also needed unless `--ipv6-prefix` is set)
- For rootfs build: qemu-utils, e2fsprogs

**Storage:** btrfs at `/mnt/fcvm-btrfs` (auto-created as loopback on non-btrfs hosts)

<details>
<summary>Full setup for Ubuntu/Debian</summary>

```bash
# Install dependencies
sudo apt-get update && sudo apt-get install -y \
    fuse3 libfuse3-dev libclang-dev clang musl-tools \
    iproute2 iptables passt qemu-utils e2fsprogs uidmap

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add $(uname -m)-unknown-linux-musl

# System configuration
sudo chmod 666 /dev/kvm
sudo sysctl -w vm.unprivileged_userfaultfd=1
echo "user_allow_other" | sudo tee -a /etc/fuse.conf
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0  # Ubuntu 24.04+
sudo sysctl -w net.ipv4.conf.all.forwarding=1

# For bridged networking only:
sudo mkdir -p /var/run/netns
sudo iptables -P FORWARD ACCEPT
```

See [`Containerfile`](Containerfile) for the complete dependency list used in CI.

</details>

---

## CLI Reference

| Command | Description |
|---------|-------------|
| `fcvm setup` | Download kernel and create rootfs (5-10 min first run, then cached) |
| `fcvm setup --cloud-hypervisor` | Build the Cloud Hypervisor backend binary (content-addressed, on demand) |
| `fcvm podman run` | Run container in a microVM (backend selected with `--hypervisor`, default `firecracker`) |
| `fcvm exec` | Execute command in running VM/container |
| `fcvm ls` | List running VMs (`--json` for JSON) |
| `fcvm snapshot create` | Snapshot a running VM |
| `fcvm snapshot serve` | Start UFFD memory server for cloning |
| `fcvm snapshot run` | Clone from snapshot |
| `fcvm serve` | Start HTTP API server |

**Key `podman run` flags:**
```
--name <NAME>         VM name (required)
--network <MODE>      rootless (default), bridged, or routed
--publish <H:G>       Port forward (e.g., 8080:80)
--map <H:G[:ro]>      Volume mount (e.g., /data:/data:ro)
--env <K=V>           Environment variable
-i / -t / -it         Interactive / TTY / both
--setup               Auto-setup if assets missing (rootless only)
--health-check <URL>  Create startup snapshot after health passes
--cpu <N> --mem <MB>  CPU count and memory
--hugepages           Use 2MB hugepages (pre-allocate pool first)
--privileged          Allow device access and mknod in container
--image-mode <MODE>   overlay (default), btrfs, or archive
--portable-volumes    Path-hash inodes for cross-machine snapshot/restore
--rootfs-size <SIZE>  Minimum free space on rootfs (default: 10G)
--no-snapshot         Disable automatic snapshot creation
--hypervisor <VMM>    firecracker (default) or cloud-hypervisor (#632)
--ipv6-prefix <PREFIX>  Use explicit /64 prefix for routed mode (skips auto-detect and MASQUERADE)
```

Run `fcvm --help` or `fcvm <command> --help` for full options.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `FCVM_DATA_DIR` | `/mnt/fcvm-btrfs` | Base directory for all data |
| `FCVM_NO_SNAPSHOT` | unset | `1` to disable snapshot creation (same as `--no-snapshot`) |
| `FCVM_NO_WRITEBACK_CACHE` | unset | `1` to disable FUSE writeback cache |
| `FCVM_SNAPSHOT_CONCURRENCY` | `10` | Max concurrent snapshot creations |
| `FCVM_CLOUD_HYPERVISOR_BIN` | unset | Path to `cloud-hypervisor` binary (falls back to PATH) |
| `RUST_LOG` | `warn` | Logging level (`info`, `debug` for verbose) |

### Image Delivery Modes

| Mode | Flag | Description |
|------|------|-------------|
| **Overlay** (default) | `--image-mode overlay` | Pre-built ext4 image, mounted as `additionalImageStore` |
| **Btrfs** | `--image-mode btrfs` | Native btrfs subvolumes, reflink-copied per VM |
| **Archive** | `--image-mode archive` | Docker tar archive, `podman load` at boot (slowest) |

Btrfs mode requires a btrfs kernel profile: `./fcvm setup --kernel-profile btrfs --build-kernels`

### Guest OS and Kernel

Guest VMs run Ubuntu 24.04 LTS with Podman, crun, and fuse-overlayfs. The default kernel is from [Kata Containers](https://github.com/kata-containers/kata-containers) (6.12.x, `CONFIG_FUSE_FS=y`). Both are built during `fcvm setup` and content-addressed — changing config triggers a rebuild. Custom kernels via `--kernel-profile`; see [DESIGN.md](DESIGN.md#kernel-profiles).

---

## Testing

```bash
make test-root                       # All tests (requires sudo + KVM)
make test-root FILTER=sanity         # Filter by name
make test-root FILTER=exec STREAM=1  # Live output
make container-test                  # All tests in container (just needs podman + KVM)
```

<details>
<summary>Test tiers and CI details</summary>

```bash
make test-unit       # Unit tests (no VMs, no sudo)
make test-fast       # + quick VM tests (rootless)
make test-all        # + slow VM tests (rootless)
make test-root       # + privileged tests (bridged, pjdfstest, sudo)
make test-fc-mock    # Container mode tests (no KVM, uses fc-mock)
```

CI runs on every PR across ARM64 and x64 with both snapshot-enabled and snapshot-disabled modes. Tests include POSIX compliance (pjdfstest), VM lifecycle, networking, snapshot/clone workflows, and egress connectivity.

</details>

---

## Project Structure

```
fcvm/
├── src/           # Host CLI (fcvm binary)
├── fc-agent/      # Guest agent (runs inside VM)
├── fuse-pipe/     # FUSE passthrough library
└── tests/         # Integration tests
```

## Documentation

| Document | Content |
|----------|---------|
| [DESIGN.md](DESIGN.md) | Architecture, internals, configuration reference |
| [Durable remote workloads](docs/durable-workloads.md) | Proposed persistent, cell-based remote execution architecture |
| [PERFORMANCE.md](PERFORMANCE.md) | Benchmarks, tuning, tracing |
| [NESTED.md](NESTED.md) | Nested virtualization setup |

---

## Troubleshooting

**VM won't start?** Check `./fcvm ls --json` for logs, verify `/mnt/fcvm-btrfs/` exists with kernel and rootfs.

**Tests hang?** Kill test VMs: `ps aux | grep fcvm | grep test | awk '{print $2}' | xargs sudo kill`

**KVM not available?** Both Firecracker and Cloud Hypervisor require bare-metal KVM or nested virtualization. On AWS, use `.metal` instances.

**Network issues?** Test incrementally inside a VM:
```bash
./fcvm exec --name web -- ping -c1 10.0.2.2        # Gateway
./fcvm exec --name web -- nslookup example.com      # DNS
./fcvm exec --name web -- wget -qO- http://ifconfig.me  # External
```

---

## License

MIT — see [LICENSE](LICENSE).
