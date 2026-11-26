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

# Routed at a chosen address in the prefix in use, instead of one derived from the VM id
sudo ./fcvm podman run --name slot-3 --network routed --ipv6-address 2001:db8:0:1::3 nginx:alpine
```

A routed VM's address is derived from its random VM id unless `--ipv6-address` names one.
That address must lie inside the prefix in use (`--ipv6-prefix`, `FCVM_IPV6_PREFIX`, or the
detected /64) and must not be its address 0 or 1. An address another live VM holds is
refused, naming that VM, and so is one the host already routes when no live VM holds it
(a VM that exited without cleanup leaves its route). The flag is refused with any other
network mode.

VM names are unique among running VMs: `podman run --name X` and `snapshot run --name X`
are refused, naming the VM's pid, while a live VM is named X. A VM whose process exited
frees its name, SIGKILL included, so `exec --name X` always finds at most one VM.

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

A `--map` with `:ro` is mounted read-only in the guest, for the guest OS and the container alike, and follows the host: a file the host rewrites or replaces shows its new size, mtime and content in the guest about a second later, in a running VM and in a clone restored from a snapshot. A map without `:ro` is read-write and follows the host the same way for every file no guest process has open for writing; a file a guest process has open for writing keeps the guest's own size and mtime until it is closed. [FUSE Volume Cache Coherency](DESIGN.md#fuse-volume-cache-coherency) says what the host may change under each.

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

A clone inherits the snapshot's port mappings, host address included. An address and
port belong to one VM at a time, so a clone of a VM published on a host address fails
to start with "Address already in use" while its source or another clone runs.
`snapshot run --publish` replaces the mappings with the ones given, in the grammar of
`podman run --publish`:

```bash
# The baseline publishes guest port 80
./fcvm podman run --name web --publish 8080:80 nginx:alpine
./fcvm snapshot create web --tag web-warm

# Inherits 8080:80, on the clone's own per-VM IP
./fcvm snapshot run --snapshot web-warm --name clone1
# Publishes guest port 80 on 127.0.0.1:9090 and nowhere else
./fcvm snapshot run --snapshot web-warm --name clone2 --publish 127.0.0.1:9090:80
```

Each mapping's guest port and protocol must be one the snapshot published, because a
guest restored from memory keeps the port setup it booted with. Anything else is refused
before the clone starts, with the snapshot's guest ports in the error. A bridged clone
listens on its veth address and ignores an IPv4 HOSTIP, as `podman run` does. `fcvm ls --json` shows the
mappings a clone uses in `config.port_mappings`. The flag is in no snapshot key: with
`--network routed`, one snapshot serves a clone on `--publish '[::]:80:80'` (port 80 on
every host address) and clones on their per-VM IPs.

A routed clone's IPv6 address comes from the host that runs it, as for `podman run`:
from `snapshot run --ipv6-prefix` (also read from `FCVM_IPV6_PREFIX`), in the forms
`podman run --ipv6-prefix` takes, and otherwise from the host's own /64. The prefix the
snapshot was created under is never used, so a clone on another host, or on a host whose
routed prefix changed, gets an address the network routes to it. On a host with no
routable /64 of its own, set the variable. Other network modes ignore it.
`snapshot run --ipv6-address` gives a routed clone that address instead of a derived one,
under the rules `podman run --ipv6-address` follows; fc-agent swaps it in for the
snapshot's address on restore. The address is in no snapshot key.

A clone also takes the restoring host's identity before its `--on-restore` hook runs.
`snapshot run --host-ipv6 <ADDR>` (also read from `FCVM_HOST_IPV6`) replaces the host
identity address the guest was booted with (`podman run --env HOST_IPV6=<ADDR>`) on lo,
and on eth0 outside routed mode; without it the guest holds none. A disk-only snapshot's
cold boot, and the relaunch of a clone whose guest reboots, get the same address as their
`HOST_IPV6`. A `podman run` cache hit passes its own `HOST_IPV6`, which is not in the
pre-start snapshot key, and the container the clone launches is given that value too. A
startup snapshot's container already runs with one, so the startup snapshot's name
carries it. The guest's resolv.conf is rewritten with the resolvers and search domains a
boot on the restoring host would give it, unless the snapshot was made with `--dns`; a
restore that cannot write it fails.

### Building a Snapshot Without Leaving a VM Behind

`podman prepare` takes the same arguments as `podman run`, boots one disposable VM,
waits for the container to report healthy, installs the startup snapshot, and reaps
every host resource. It prints one JSON line and exits; there is no VM to clean up.

```bash
# Build (or verify) the snapshot, then address it by name
./fcvm podman prepare --name golden --tag nginx-warm nginx:alpine
{"status":"prepared","cache":"created","snapshot_key":"nginx-warm",
 "content_key":"a1b2c3d4e5f6-startup","snapshot_type":"user",
 "generation_id":"...","config_digest":"..."}

./fcvm snapshot serve nginx-warm
./fcvm snapshot run --pid <serve_pid> --name clone1
```

The snapshot is keyed by the content of the arguments. Change the image or the VM
shape and the next `prepare` rebuilds; change the image behind a tag that keeps the
same reference and use `--force`. Without `--tag` the snapshot is installed under its
content key as cache, which `snapshots prune` reclaims; with `--tag` it is a user
snapshot that survives a prune.

Before it prints its record, `prepare` restores a snapshot that has no recorded working
set once through a memory server and tears the clone down, which records the pages a
restore touches. The first restore that follows replays them instead of faulting each one
in. `--no-record-working-set` skips that restore.

A `prepare` whose image import is already in the cache, from a `podman run` of the same
image and VM shape, restores that pre-start snapshot instead of booting cold, then waits
for healthy as a cold `prepare` does. `--health-check` is not part of that snapshot's
key, only of the startup snapshot's. `--force` always boots cold. A `prepare` that boots
cold takes no pre-start snapshot of its own.

Before any VM starts, `prepare` checks that the snapshot filesystem has room for the
snapshot it will take: `--mem` MiB for a cold boot's full snapshot, a tenth of that for
the diff on a restored pre-start snapshot. It refuses when it does not, and so does a
cold `--gpu` run for its pre-start snapshot. The check is repeated when the snapshot is
taken, because the VM's own disk and other writers use space meanwhile; a refusal there
says how far free space fell since the start. A snapshot that `--force` or `--tag`
replaces keeps its space until the new one is installed, and both refusals say how much
it holds.

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

A run can snapshot its running container the first time it is healthy, so the next
identical run (same image, command, environment and volumes; the name and labels may differ)
restores the container already running instead of starting it with `podman run`. A run asks
for this with `--startup-snapshot`, or with `--health-check`, which also decides when it is
healthy. Otherwise healthy is podman's healthcheck for an image that defines one, and the
container's start for one that does not:

```bash
./fcvm podman run --name web1 --startup-snapshot nginx:alpine
# First run: starts the container, then snapshots it running
./fcvm podman run --name web2 --startup-snapshot nginx:alpine
# Second run: restores with the container already running

./fcvm podman run --name web --health-check http://localhost/ nginx:alpine
# Snapshots once the app answers, not just once the container started
```

It is off for a plain `podman run` because every run restored from the snapshot continues
the same container: whatever the app made when it started (keys, IDs, random seeds, a
registration with a server) is shared by all of them, and output written before the snapshot
is not repeated. A run that does not ask also does not restore a startup snapshot that another
run left under the same key. `fcvm serve` passes `--startup-snapshot`, since its sandboxes of
one image are identical by design.

With `--balloon`, the startup snapshot is kept per target, because the app initialized under that target. Runs that differ only in the target still share the pre-start snapshot. A run whose target `fcvm balloon` changed before the app turned healthy saves no startup snapshot, and `podman prepare` fails.

---

## Interactive Mode & TTY

fcvm supports `-i` and `-t` flags just like docker/podman:

```bash
./fcvm podman run --name shell -it alpine:latest sh          # Interactive shell
./fcvm podman run --name editor -it alpine:latest vi /tmp/x   # Full-screen apps work
./fcvm exec --name web -it -- bash                            # Shell in running VM
echo "data" | ./fcvm podman run --name pipe -i alpine:latest cat  # Pipe stdin
```

`fcvm exec` follows `podman exec`: byte-exact output with stdout and stderr kept apart, the command's exit code (128+N for a signal, 127 for a missing command, 126 for one that cannot run, 125 when fcvm itself fails), end of input on `-i`, and a PTY that tracks the terminal's size on `-t`.

```bash
tar c src | ./fcvm exec --name web -i -- tar x -C /tmp         # Pipe into a command; it sees end of input
./fcvm exec --name web -e KEY=value -w /tmp -u nobody -- env     # Environment, directory, user
./fcvm exec --name web --env-file prod.env -- printenv           # KEY=VALUE lines; -e wins over the file
./fcvm exec --name web -d -- long-running-job                    # Start it and return
```

Where fcvm differs on purpose, such as killing a guest command when its `fcvm exec` dies, [DESIGN.md](DESIGN.md#differences-from-podman-exec) lists it.

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

L2 VMs have ~5-7x FUSE overhead. See [NESTED.md](NESTED.md) for details.

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

#### Recycled namespaces (`fcvm netpool serve`)

Building and destroying a rootless namespace holds the kernel's `rtnl_mutex`, one lock for the whole host, for milliseconds. A host that starts and stops many clones can keep a pool of namespaces instead: a clone's start then takes a namespace in under a millisecond, and the cleaning happens after the clone has stopped. The pool cleans 114 to 120 namespaces a second (`bench/start-rate/pieces/RESULTS.md`, changelog 7), so that is the rate of starts and stops it sustains; it does not yet raise the rate above building namespaces afresh.

```bash
fcvm netpool serve --slots 2000 &     # one per user and data directory; SIGTERM stops it
fcvm snapshot run --snapshot my-snapshot --name clone1   # takes a pooled namespace when one is free
fcvm netpool stats                    # {"ready":1999,"leased":1,"grants":1,"recycled_grants":0,...}
```

- A restored clone with no published port takes a pooled namespace when a pool runs and one is free, and gives it back when its VMM has exited. The pool has the namespace's pasta drop the VM's flows, removes what the VM left, and checks the namespace against a fingerprint taken when it was built; one that differs is destroyed and built again.
- A pooled namespace's TAP stays attached by the pool, and the clone's Firecracker uses it through a descriptor it inherits, so a start and a stop change no link. That takes the Firecracker `fcvm setup` builds; a snapshot that restores with an older binary, and a Cloud Hypervisor clone, are not pooled.
- A clone with `--publish`, a cold boot (`podman run`), and any clone when no pool runs or none is free build their own namespace, as before.
- Every holder and pasta of a pool is the pool's child. If the pool dies they die with it, and the clones in its namespaces exit with an error.
- A clone's state records its namespace as `netpool: {slot, generation}`; a generation of 2 or more is a recycled namespace.

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

A mapping can name the host address to listen on instead: `--publish 127.0.0.1:8080:80`,
or `--publish '[::]:80:80'` for every address of the host. An IPv6 address goes in
brackets, quoted for the shell. `config.port_mappings[].host_ip` records the address.
- **Rootless** and **Routed** listen on that address. In routed mode `[::]` accepts IPv4
  and IPv6 clients; in rootless mode pasta's IPv6 listeners accept IPv6 only, and an IPv6
  HOSTIP needs a global IPv6 address on the host: without one the guest has no IPv6
  address and the mapping is refused. A routed VM whose mappings all name an address has
  no `loopback_ip`.
- **Bridged** always listens on the VM's veth address: it ignores an IPv4 HOSTIP and
  rejects an IPv6 one. Two mappings with the same host port and protocol are one socket
  there whatever their HOSTIPs, and are refused.
- A listener on routed `[::]` or on `0.0.0.0` holds its port on every 127.x.y.z address
  too. While it runs, a VM that publishes the same port on its per-VM IP fails to start
  with "Address already in use", and the reverse.

---

## Prerequisites

**Hardware:**
- Linux with `/dev/kvm` — bare-metal or nested virtualization
- AWS: c6g.metal (ARM64) or c5.metal (x86_64)

**Dependencies:**
- Rust 1.93 (pinned by `rust-toolchain.toml`) with musl target: `rustup target add $(uname -m)-unknown-linux-musl`
- Firecracker binary in PATH (default backend)
- Cloud Hypervisor (only if using `--hypervisor cloud-hypervisor`): build the pinned fork with `fcvm setup --cloud-hypervisor`, or set `FCVM_CLOUD_HYPERVISOR_BIN`, or put `cloud-hypervisor` in PATH (resolved in that order)
- For rootless: `passt` package (provides `pasta`)
- For bridged: sudo, iptables, iproute2
- For routed: sudo, iproute2, host with global IPv6 /64 (ip6tables also needed unless `--ipv6-prefix` is set), and `net.ipv6.conf.all.forwarding=1` (fcvm refuses routed mode without it)
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
# Routed networking. If the kernel itself takes the uplink's IPv6 default route from
# router advertisements (accept_ra=1), turning forwarding on makes it ignore them and
# the route expires, so set accept_ra=2 on the uplink first:
#   sudo sysctl -w net.ipv6.conf.<uplink>.accept_ra=2
# systemd-networkd handles advertisements itself (the kernel's accept_ra is 0) and keeps
# its route either way.
sudo sysctl -w net.ipv6.conf.all.forwarding=1

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
| `fcvm podman prepare` | Build and verify a startup snapshot, then reap the VM that produced it (`--tag`, `--force`) |
| `fcvm exec` | Execute command in running VM/container |
| `fcvm balloon` | Print a VM's balloon target and size as one JSON line; with a MIB argument, set the target first |
| `fcvm ls` | List running VMs (`--json` for JSON) |
| `fcvm snapshot create` | Snapshot a running VM (a clone only once its restore is acknowledged; `--await-restore` waits for that) |
| `fcvm snapshot serve` | Start UFFD memory server for cloning |
| `fcvm snapshot run` | Clone from snapshot (`--publish` chooses where its ports listen) |
| `fcvm snapshot export` | Write a Firecracker snapshot as one archive, with the image disk and the pmem images it reads, to stdout or `--output <FILE>` |
| `fcvm snapshot import` | Install the snapshot in an archive that `snapshot export` wrote, and the image disk and pmem images this host lacks, from stdin or `--input <FILE>` |
| `fcvm serve` | Start HTTP API server |
| `fcvm netpool serve` | Keep `--slots` rootless network namespaces and lend them to restored clones, one after another |
| `fcvm netpool stats` | Print the running pool's namespaces by state and its counters as one JSON line |

**Key `podman run` flags:**
```
--name <NAME>         VM name (required; unique among running VMs)
--network <MODE>      rootless (default), bridged, or routed
--publish <[IP:]H:G>  Port forward (e.g., 8080:80, '[::]:80:80')
--map <H:G[:ro]>      Volume mount (e.g., /data:/data:ro); :ro is read-only in the guest and follows the host
--env <K=V>           Environment variable
-i / -t / -it         Interactive / TTY / both
--setup               Auto-setup if assets missing (rootless only)
--health-check <URL>  Create startup snapshot after health passes
--startup-snapshot    Snapshot the running container at its first Healthy (off by default)
--cpu <N> --mem <MB>  CPU count and memory
--hugepages           Use 2MB hugepages (pre-allocate pool first)
--guest-thp <MODE>    Guest transparent huge pages: always, madvise or never (default: the guest kernel's); applied at boot and at every restore, not in the snapshot key, inherited by `snapshot run`
--balloon <MIB>       Balloon device target; runs that differ only in the target share the pre-start snapshot
--free-page-reporting Guest reports freed memory and the host takes it back (needs --balloon, Firecracker only, not with --hugepages; the run fails if the guest refuses it; in the snapshot key)
--privileged          Allow device access and mknod in container
--image-mode <MODE>   overlay (default), btrfs, or archive
--portable-volumes    Path-hash inode numbers in the guest (snapshots restore held inodes by path either way)
--rootfs-size <SIZE>  Minimum free space on rootfs (default: 10G)
--no-snapshot         Disable automatic snapshot creation
--hypervisor <VMM>    firecracker (default) or cloud-hypervisor (#632)
--ipv6-prefix <PREFIX>  Use explicit /64 prefix for routed mode (skips auto-detect and MASQUERADE; also read from FCVM_IPV6_PREFIX)
--ipv6-address <ADDR>   Routed mode only: the VM's address, inside the prefix in use, instead of a derived one
--publish-limit-per-source <N>  Routed mode only: close a published port's client, without a byte, while its source (IPv4 address or IPv6 /64) holds N connections on the VM's published ports; the host's loopback, private and link-local sources are not counted (also `snapshot run`)
```

Run `fcvm --help` or `fcvm <command> --help` for full options.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `FCVM_DATA_DIR` | `/mnt/fcvm-btrfs` | Base directory for all data |
| `FCVM_NO_SNAPSHOT` | unset | `1` to disable snapshot creation (same as `--no-snapshot`) |
| `FCVM_NO_WRITEBACK_CACHE` | unset | `1` to mount read-write `--map` volumes without the FUSE writeback cache, so every write goes to the host as it is made and the guest takes a file's size and mtime from the host even while a guest process has it open for writing. Read-only maps never use that cache, and a run with no read-write map is the same run with or without it |
| `FCVM_SNAPSHOT_CONCURRENCY` | `10` | Max concurrent snapshot creations |
| `FCVM_UFFD_MAX_CLONES` | `256` | Clones one `fcvm snapshot serve` will accept. They share that server's failure and fairness domain, so raising it widens the blast radius. Running two serve processes with half the clones each halves what one crashed or overloaded handler takes down |
| `FCVM_CGROUP_PARENT` | `fcvm.slice` | Parent, below the cgroup2 root, of the cgroup each VM and memory server runs in; `none` keeps fcvm in the cgroup it was started in. See [A VM's own cgroup](#a-vms-own-cgroup) |
| `FCVM_CLOUD_HYPERVISOR_BIN` | unset | Path to `cloud-hypervisor` binary (falls back to PATH) |
| `FCVM_FIRECRACKER_LOG_LEVEL` | `Info` | Level of Firecracker's own per-VM `firecracker.log`: `Error`, `Warning`, `Info` or `Debug`. `Debug` adds one line per vsock packet, which is gigabytes an hour under FUSE-over-vsock volumes. Teardown copies the log's last 16 MiB to `/tmp/fcvm-firecracker-<vm_id>.log` |
| `FCVM_FIRECRACKER_RESOLVE_TTL_SECS` | `3600` | How long a cached firecracker-fork resolution stays valid before a launch re-queries the remote; `0` forces a query every launch |
| `FCVM_CONFINE` | config | `required` confines every VMM this host starts (see [Confining the VMM](#confining-the-vmm---confine)); it can tighten the config's `[confine] mode`, never loosen it |
| `FCVM_CONFINE_SECCOMP` | `enforce` | `log` allows a confined VMM the syscalls its allowlist does not list and has the kernel record each one, to check a workload before enforcing. It restricts nothing, so `fcvm ls` shows the layer as `seccomp-log`, every such start warns, and a host whose config has `[confine] mode = "required"` refuses it unless the config also has `allow_seccomp_log = true` |
| `RUST_LOG` | `warn` | Logging level (`info`, `debug` for verbose) |

**Firecracker fork resolution.** When a kernel profile builds firecracker from a
fork branch, the branch head is resolved with `git ls-remote` — a network round
trip that used to run on *every* VM launch and snapshot clone. The result is now
cached in `assets_dir/firecracker/<profile>.resolved.json` and reused for
`FCVM_FIRECRACKER_RESOLVE_TTL_SECS`. `fcvm setup` always re-queries and rewrites
that file, so rebuilding the fork takes effect for launches immediately rather
than after the TTL. `<binary> --version` probes are cached the same way, keyed by
the binary's path + mtime + size, so a rebuilt binary is always re-checked.

### Confining the VMM (`--confine`)

`--confine` on `podman run` or `snapshot run` (or, for every start on a host,
`[confine] mode = "required"` in the config or `FCVM_CONFINE=required`) runs the VM's
Firecracker with two layers:

- **Process:** no capabilities and none regained by its exec, `no_new_privs`, no core dumps.
- **Syscalls:** an allowlist per Firecracker thread
  (`resources/seccomp/firecracker-confined-<arch>.json`): Firecracker's own shipped filter
  for the pinned commit, less io_uring, plus the calls glibc makes where musl makes others
  and `fstatfs`, which that Firecracker calls and its filter omits. It has no process
  creation, exec, ptrace, mount, bpf, unshare or setns. The VMM runs with
  `MALLOC_ARENA_MAX=1`, so that glibc grows the heap with `brk`, which the allowlist has,
  and not a thread's own arena with `mprotect`, which two of its three classes lack; and
  with `TZ=<UTC>0`, so that glibc reads no time zone file at the first log line, which the
  API and vCPU threads could not do. Firecracker's own log is therefore stamped in UTC.

A start that cannot be confined is refused, never run unconfined. Confinement needs
rootless networking, the Firecracker backend, the default build of Firecracker, and a
kernel with Landlock ABI 9 or later. A kernel profile that pins its own Firecracker is
refused. `fcvm setup` compiles the allowlist beside each Firecracker it builds from the
commit the allowlist is derived from, and records the compiled file's SHA-256 beside it
(`<filter>.sha256`). A confined start opens the filter without following a symbolic link
and refuses one that is not a regular file owned by the user (or root, or the owner of
the Firecracker binary), that its group or other users can write, that sits under a
directory someone else owns or its group or other users can write (a sticky directory
such as `/tmp` excepted), or whose bytes are not the recorded ones. It checks before
anything is created, so a host that was upgraded and not set up again, or whose
Firecracker binary is not there, is told to run `fcvm setup` with nothing started.
`fcvm setup` rebuilds a filter that does not match its record, writes the filter and the
record with mode 0644 whatever the umask, and takes group and other write off the
directory of the Firecracker it builds. The launch checks again and hands the VMM the bytes it
checked, in a sealed memory file. Another binary (`--firecracker-bin`, or the one an
older snapshot recorded) has no recorded filter and is refused the same way. Once
Firecracker has built the VM, fcvm reads every thread of the VMM from `/proc` and
requires each to run under a seccomp filter of its own: more filters than fcvm's own
threads have, since a VMM inherits whatever fcvm runs under (a systemd unit's
`SystemCallFilter=`, a container's default profile). Otherwise it kills the VMM and fails
the start naming the threads, because Firecracker accepts a filter file that installs
nothing. A restore is checked before its guest resumes; a boot within 5 s of its guest's
start (the check waited 0 to 6 ms in the test suites). `fcvm ls` shows the layers a VM
runs with.

The `[confine]` table takes `mode` and `allow_seccomp_log` and refuses any other key, so
a misspelled `mode` is an error and not `off`. When `fcvm setup` cannot build the
allowlist's compiler it warns, removes the build's checkout and records the failure
beside the Firecracker binary (`<firecracker>.seccompiler-bin.build-failed`); it does not
build again for a day or until that file is removed, and a confined start is refused
meanwhile.

**What the two layers do not stop.** The record and the filter belong to the same user
as fcvm, so the digest catches a damaged, truncated or half-replaced filter and not a
process that already runs as that user. A confined VMM is such a process, and the
allowlist still lets it open and write any file its uid can (`openat`, `write`), connect
to any Unix socket its uid can reach (`socket`, `connect`), and signal any process of its
uid, fcvm included (`tgkill`, `tkill`). A compromised confined VMM can therefore replace
the filter, its record or the Firecracker binary for later starts, or stop the fcvm that
watches it. Closing that needs the file and socket layer (Landlock: the paths the VMM may
read and write, the Unix sockets it may reach, signals scoped to itself), which fcvm does
not apply yet. Until it does, confinement narrows what a compromised VMM can ask the
kernel for; it is not a boundary between the VMM and the rest of the user's files.

**A syscall outside the allowlist kills the VMM, and the guest with it.** fcvm logs at ERROR
which Firecracker thread made which syscall. One case is known where the thread hangs
instead: Firecracker's handler for the trap writes a log line, and a syscall trapped while
that thread was already writing one (glibc reading the time zone file for the first line)
left the API thread stopped, the guest running and nothing reported. `TZ=<UTC>0` removes
that call; a trap in the same place from another cause would do the same. The allowlist covers what fcvm's test suites
ask of Firecracker; a workload that takes Firecracker down a path they do not can hit a
syscall it does not list. Check a workload first with `FCVM_CONFINE_SECCOMP=log`: a syscall
outside the allowlist is then allowed and the kernel records it as an audit record of type
`SECCOMP` that names the thread (`comm=`, in hex when the name holds a space, as a vCPU
thread's `fc_vcpu 0` does) and the syscall number (`syscall=`):

```bash
FCVM_CONFINE_SECCOMP=log fcvm podman run --confine --name check my-image
sudo grep -h 'type=SECCOMP' /var/log/audit/audit.log* | grep 'exe="[^"]*firecracker'
```

(The kernel writes `exe=` as hex when the path holds a space, which this pattern misses;
`scripts/confine-seccomp-audit.sh` decodes those.) Log mode restricts nothing:
every start in it warns, `fcvm ls` lists the layer as `seccomp-log` and not `seccomp`, and
a host whose config sets `[confine] mode = "required"` refuses it unless the config also
sets `allow_seccomp_log = true`. Where only `FCVM_CONFINE=required` in the environment
requires confinement, log mode is accepted, since whoever sets that variable can leave it
unset.

Without auditd the records go to the kernel log, rate-limited, so a run can lose some.
auditd rotates its log (by default it keeps five files of 8 MB), so a syscall made in a
loop can push earlier records out of all of them: read every file, and raise
`max_log_file` in `/etc/audit/auditd.conf` if they filled. No record over a representative
run means enforce mode would not have stopped it.

A syscall made through another architecture's entry (on x86_64, `int 0x80`) is not trapped:
libseccomp's default for it, which Firecracker's compiler does not change, kills the
calling thread and leaves the process. That is the same under Firecracker's own filters.

### A VM's own cgroup

Run as root, `podman run`, `podman prepare`, `snapshot run` and `snapshot serve` move the
fcvm process into `/sys/fs/cgroup/fcvm.slice/fcvm-<pid>-<starttime>.scope` before they
start anything. Firecracker, the namespace holder, pasta, the memory server and the FUSE
servers start inside it, so guest memory and the memory server's page cache are charged to
the VM's cgroup and not to the session that ran fcvm, and a session that is killed for
memory does not take the VM with it. `FCVM_CGROUP_PARENT` moves the scopes under another
parent (for instance a slice your oomd watches), or `none` keeps fcvm where it was started;
to run a VM in a systemd unit instead, start fcvm in one with
`systemd-run --scope [-p MemorySwapMax=0] env FCVM_CGROUP_PARENT=none fcvm ...`.
fcvm removes its scope when it exits; a scope whose fcvm was killed is removed by the next
fcvm that starts. An unprivileged fcvm stays in its own cgroup.

`--no-swap` (on all four commands) sets `memory.swap.max` to 0 on that cgroup before the
VMM starts. It needs the cgroup, so fcvm refuses the flag when it cannot create one with a
memory controller: unprivileged, with `FCVM_CGROUP_PARENT=none`, or on a host without
cgroup2.

### Image Delivery Modes

| Mode | Flag | Description |
|------|------|-------------|
| **Overlay** (default) | `--image-mode overlay` | Pre-built ext4 image, mounted as `additionalImageStore` |
| **Btrfs** | `--image-mode btrfs` | Native btrfs subvolumes, reflink-copied per VM |
| **Archive** | `--image-mode archive` | Docker tar archive, `podman load` at boot (slowest) |

Btrfs mode requires a btrfs kernel profile: `./fcvm setup --kernel-profile btrfs --build-kernels`

### Guest OS and Kernel

Guest VMs run Ubuntu 24.04 LTS with Podman, crun, and fuse-overlayfs. fcvm publishes its default kernel for arm64 and amd64 with FUSE plus the socket-diagnostic options required for safe snapshot cleanup. `fcvm setup` downloads the content-addressed artifact; release jobs build it from the pinned profile recipe. The release workflow also builds and publishes the `nested` and `btrfs` profiles for both architectures. Custom kernels use `--kernel-profile`; see [DESIGN.md](DESIGN.md#kernel-profiles).

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

**CI merge train:** batches of independent low-risk PRs (dependabot bumps, docs, isolated fixes) can share ONE full CI matrix instead of one per PR — `make train-create PRS="689 690" && make train-dispatch`, then `make train-land` on green or `make train-bisect` on red. See [docs/ci-train.md](docs/ci-train.md) for the protocol, the cost math, and when pooling is (in)appropriate.

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
| [CI merge train](docs/ci-train.md) | Pooled CI: one full matrix per batch of low-risk PRs |
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
