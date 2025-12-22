# FCVM - Firecracker VM Manager Design Specification

## Table of Contents
1. [Overview](#overview)
2. [Requirements](#requirements)
3. [Architecture](#architecture)
4. [Core Components](#core-components)
5. [Networking](#networking)
6. [Storage & Cloning](#storage--cloning)
7. [VM Lifecycle](#vm-lifecycle)
8. [Guest Agent](#guest-agent)
9. [CLI Interface](#cli-interface)
10. [Implementation Details](#implementation-details)

---

## Overview

**fcvm** is a Firecracker VM manager designed to run Podman containers inside lightweight microVMs with lightning-fast cloning capabilities. It provides a simple CLI interface for spinning up isolated container environments with:

- **Full-featured VMs**: Filesystem access, outbound networking, port forwarding
- **Fast cloning**: Clone running VMs in <1s using snapshots and CoW disks
- **Flexible networking**: Both rootless and privileged modes
- **Process lifetime binding**: VM lifetime tied to controlling process
- **Resource configuration**: Configurable vCPU/memory with overcommit support

**Target Platform**: Linux only (requires KVM)

---

## Requirements

### Functional Requirements

1. **`fcvm podman run` Command**
   - Takes a Docker/Podman container image
   - Spins up a Firecracker VM running the container
   - Supports volume mounts via FUSE passthrough (host → guest)
   - Supports port forwarding (host → guest)
   - Process blocks until VM exits (hanging/foreground mode)
   - VM dies when process is killed (lifetime binding)

2. **`fcvm snapshot` Commands**
   - `fcvm snapshot create`: Create snapshot from running VM
   - `fcvm snapshot serve`: Start UFFD memory server for cloning
   - `fcvm snapshot run`: Spawn clone from memory server
   - Lightning-fast clone startup (<1 second)
   - Shares memory via UFFD page fault handler
   - Creates independent VM with its own networking

3. **Networking Modes**
   - **Rootless**: Works without root privileges using slirp4netns
   - **Privileged**: Uses nftables + bridge for better performance
   - **Port mapping**: `[HOSTIP:]HOSTPORT:GUESTPORT[/PROTO]` syntax
   - Support multiple ports, TCP/UDP protocols

4. **Volume Mounting**
   - Map local directories to guest filesystem
   - Support block devices, sshfs, and NFS modes
   - Read-only and read-write mounts

5. **Resource Configuration**
   - vCPU overcommit (more vCPUs than physical cores)
   - Memory overcommit with balloon device
   - Configurable memory ballooning

6. **Snapshot & Clone**
   - Save VM state at "warm" checkpoint (after container ready)
   - Fast restore from snapshot
   - CoW disks for instant cloning
   - Identity patching (MAC addresses, hostnames)

### Non-Functional Requirements

- **Performance**: Clone startup <1s
- **Isolation**: Full VM isolation via Firecracker
- **Compatibility**: Works with rootless Podman in guest
- **Portability**: Runs on bare metal or nested VMs (VM-in-VM)
- **Reliability**: Clean shutdown, resource cleanup

---

## Architecture

### High-Level Design

```
┌──────────────────────────────────────────────────────┐
│                  fcvm CLI (Host)                      │
│  ┌────────────┐  ┌──────────────┐  ┌─────────────┐  │
│  │ Networking │  │  Firecracker │  │  Storage &  │  │
│  │  Manager   │  │  API Client  │  │  Snapshots  │  │
│  └────────────┘  └──────────────┘  └─────────────┘  │
│         │                │                 │          │
│         └────────────────┴─────────────────┘          │
│                          │                            │
└──────────────────────────┼────────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Firecracker Process   │
              │  (microVM)             │
              │  ┌──────────────────┐  │
              │  │   Linux Kernel   │  │
              │  │  ┌────────────┐  │  │
              │  │  │ fc-agent   │  │  │
              │  │  │     │      │  │  │
              │  │  │  Podman    │  │  │
              │  │  │     │      │  │  │
              │  │  │ Container  │  │  │
              │  │  └────────────┘  │  │
              │  └──────────────────┘  │
              └────────────────────────┘
```

### Component Breakdown

1. **fcvm CLI** (Rust)
   - Command-line interface
   - Orchestrates VM lifecycle
   - Manages networking, storage, snapshots
   - Streams logs and handles signals

2. **Firecracker** (External binary)
   - Runs the microVM
   - Provides REST API over Unix socket
   - Manages VM resources (vCPU, memory, drives, network)

3. **fc-agent** (Rust, runs in guest)
   - Fetches container configuration from MMDS
   - Launches Podman with correct parameters
   - Streams container logs to serial console
   - Signals readiness to host

---

## Core Components

### 1. Firecracker API Client

**Location**: `fcvm/src/firecracker/api.rs`

Provides Rust interface to Firecracker REST API over Unix socket using `hyper` + `hyperlocal`.

**Key Functions**:
- `set_boot_source()` - Configure kernel + boot args
- `set_machine_config()` - Set vCPU, memory, SMT
- `add_drive()` - Attach rootfs and data disks
- `add_network_interface()` - Setup networking
- `set_mmds_config()` - Configure metadata service
- `put_mmds()` - Provide container plan to guest
- `create_snapshot()` - Save VM state
- `load_snapshot()` - Restore from snapshot
- `set_balloon()` - Configure memory balloon

**API Structures**:
```rust
struct BootSource {
    kernel_image_path: String,
    initrd_path: Option<String>,
    boot_args: Option<String>,
}

struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u32,
    smt: Option<bool>,
    track_dirty_pages: Option<bool>,
}

struct Drive {
    drive_id: String,
    path_on_host: String,
    is_root_device: bool,
    is_read_only: bool,
}

struct NetworkInterface {
    iface_id: String,
    host_dev_name: String,  // TAP device
    guest_mac: Option<String>,
}
```

### 2. VM Manager

**Location**: `fcvm/src/firecracker/vm.rs`

Manages Firecracker process lifecycle.

**Responsibilities**:
- Spawn Firecracker process with correct args
- Wait for API socket to be ready
- Stream stdout/stderr to tracing logs
- Handle graceful shutdown
- Clean up resources (socket, processes)

**Key Functions**:
```rust
impl VmManager {
    async fn start(&mut self, firecracker_bin, config) -> Result<()>
    async fn wait(&mut self) -> Result<ExitStatus>
    async fn kill(&mut self) -> Result<()>
    async fn stream_console(&self, console_path) -> Result<Receiver<String>>
    fn client(&self) -> Result<&FirecrackerClient>
}
```

### 3. Networking Managers

**Location**: `fcvm/src/network/`

Two implementations based on execution mode.

#### Rootless Networking (`slirp.rs`)

Uses `slirp4netns` for userspace networking.

**Features**:
- No root privileges required
- Port forwarding via `slirp4netns --port`
- Default guest IP: `10.0.2.15`
- Default host IP: `10.0.2.2`

**Implementation**:
```rust
struct RootlessNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,
    slirp_process: Option<Child>,
}

async fn setup() -> Result<NetworkConfig> {
    // TAP device created by Firecracker
    // slirp4netns started after VM boots
    // Port forwarding configured via hostfwd
}
```

#### Privileged Networking (`bridged.rs`)

Uses Linux bridge + nftables for native performance.

**Features**:
- Requires root or CAP_NET_ADMIN
- Better performance than rootless
- Uses DNAT for port forwarding
- Bridge networking for VM isolation

**Implementation**:
```rust
struct PrivilegedNetwork {
    vm_id: String,
    tap_device: String,
    bridge: String,
    guest_ip: String,
    host_ip: String,
    port_mappings: Vec<PortMapping>,
}

async fn setup() -> Result<NetworkConfig> {
    create_tap_device(tap_name)
    add_to_bridge(tap_name, bridge)
    for mapping in port_mappings {
        setup_nat_rule(mapping, guest_ip)
    }
}
```

**NAT Rule Example**:
```bash
nft add rule ip nat PREROUTING tcp dport 8080 dnat to 172.16.0.10:80
```

#### Port Mapping Format

**Grammar**: `[HOSTIP:]HOSTPORT:GUESTPORT[/PROTO]`

**Examples**:
```
8080:80              # TCP port 8080 → guest:80
127.0.0.1:8080:80    # Bind to localhost only
8080:80/udp          # UDP protocol
0.0.0.0:53:53/udp    # DNS forwarding
```

**Parsing Logic** (`network/types.rs`):
```rust
impl PortMapping {
    pub fn parse(s: &str) -> Result<Self> {
        // Split on ':'
        // Extract optional host IP
        // Extract protocol suffix (/tcp or /udp)
        // Default to TCP if not specified
    }
}
```

---

## Storage & Cloning

### Disk Layout

Each VM has:
1. **Kernel**: Shared across all VMs (read-only)
2. **Base rootfs**: Shared base image with Podman + fc-agent
3. **CoW overlay**: Per-VM writable layer (using btrfs reflinks)
4. **Volume mounts**: Optional host directory mounts

```
/mnt/fcvm-btrfs/               # btrfs filesystem (CoW reflinks work here)
├── kernels/
│   └── vmlinux.bin            # Shared kernel
├── rootfs/
│   └── base.ext4              # Base rootfs image (~1GB Ubuntu + Podman)
├── vm-disks/
│   └── vm-{id}/
│       └── rootfs.ext4        # CoW reflink copy per VM
├── snapshots/
│   └── {snapshot-name}/
│       ├── vmstate.snap       # VM memory snapshot
│       ├── disk.snap          # Disk snapshot
│       └── config.json        # VM configuration
├── state/                     # VM state JSON files
└── cache/                     # Downloaded cloud images
```

### Copy-on-Write (CoW) Strategy

**Goal**: Share base rootfs across VMs, only store deltas per-VM.

**Options**:

1. **overlayfs** (preferred for simplicity)
   ```bash
   mount -t overlay overlay \
     -o lowerdir=/base/rootfs,upperdir=/vm/upper,workdir=/vm/work \
     /vm/merged
   ```

2. **qcow2** (better for snapshots)
   ```bash
   qcow2-img create -f qcow2 -b base.ext4 vm-overlay.qcow2
   ```

**Benefits**:
- Instant cloning (no disk copy)
- Shared memory pages across VMs
- Fast snapshot restore

### Snapshot Format

**Memory Snapshot**: Firecracker native format
```json
{
  "snapshot_path": "/snapshots/warm/disk.snap",
  "mem_file_path": "/snapshots/warm/memory.snap",
  "snapshot_type": "Full"
}
```

**Clone Process**:
1. Load snapshot via Firecracker API
2. Create new CoW overlay disk
3. Patch identity (MAC address, hostname, VM ID)
4. Setup new networking (TAP device, ports)
5. Resume VM

**Identity Patching**:
- Generate new MAC address
- Update hostname in guest
- Regenerate machine IDs
- Update MMDS with new config

---

## Networking

### Rootless Mode (slirp4netns with Dual-TAP Architecture)

**Key Insight**: slirp4netns and Firecracker CANNOT share a TAP device (both need exclusive access).
**Solution**: Use two TAP devices with IP forwarding between them inside a user namespace.

**Topology**:
```
Host                     │ User Namespace (unshare --user --map-root-user --net)
                         │
slirp4netns <────────────┼── slirp0 (10.0.2.100/24)
  (userspace NAT)        │        │
                         │        │ IP forwarding + iptables NAT
                         │        ▼
                         │   tap0 (192.168.1.1/24)
                         │        │
                         │        ▼
                         │   Firecracker VM
                         │     eth0: 192.168.1.2
```

**Setup Sequence** (3-phase with nsenter):
1. Spawn holder process: `unshare --user --map-root-user --net -- sleep infinity`
2. Run setup via nsenter: create TAPs, iptables, enable IP forwarding
3. Start slirp4netns attached to holder's namespace
4. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
5. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`

**Network Setup Script** (executed via nsenter):
```bash
# Create slirp0 TAP for slirp4netns connectivity
ip tuntap add slirp0 mode tap
ip addr add 10.0.2.100/24 dev slirp0
ip link set slirp0 up
ip route add default via 10.0.2.2 dev slirp0

# Create tap0 for Firecracker (guest uses 192.168.1.2)
ip tuntap add tap0 mode tap
ip addr add 192.168.1.1/24 dev tap0
ip link set tap0 up

# Enable IP forwarding
echo 1 > /proc/sys/net/ipv4/ip_forward

# Allow forwarding between slirp0 and FC TAP
iptables -A FORWARD -i slirp0 -o tap0 -j ACCEPT
iptables -A FORWARD -i tap0 -o slirp0 -j ACCEPT

# NAT guest traffic (192.168.x.x) to slirp0's address (10.0.2.100)
iptables -t nat -A POSTROUTING -s 192.168.1.0/24 -o slirp0 -j MASQUERADE
```

**Port Forwarding** (unique loopback IPs):
```bash
# Each VM gets a unique loopback IP (127.x.y.z) for port forwarding
# No IP aliasing needed - Linux routes all 127.0.0.0/8 to loopback
slirp4netns \
  --configure \
  --mtu=65520 \
  --api-socket /tmp/slirp-{vm_id}.sock \
  <holder-pid> \
  slirp0

# Port forwarding via JSON-RPC API:
echo '{"execute":"add_hostfwd","arguments":{"proto":"tcp","host_addr":"127.0.0.2","host_port":8080,"guest_addr":"10.0.2.100","guest_port":8080}}' | nc -U /tmp/slirp-{vm_id}.sock
```

**Traffic Flow** (VM to Internet):
```
Guest (192.168.1.2) → tap0 → iptables MASQUERADE → slirp0 (10.0.2.100) → slirp4netns → Host → Internet
```

**Traffic Flow** (Host to VM port forward):
```
Host (127.0.0.2:8080) → slirp4netns → slirp0 (10.0.2.100:8080) → IP forward → tap0 → Guest (192.168.1.2:80)
```

**Characteristics**:
- No root required (runs entirely in user namespace)
- Isolated 192.168.1.0/24 subnet per VM (no conflicts)
- Unique loopback IP per VM enables same port on multiple VMs
- Slightly slower than bridged (~10-20% overhead)
- Works in nested VMs and restricted environments
- Fully compatible with rootless Podman in guest

### Privileged Mode (nftables + bridge)

**Topology**:
```
┌───────────────────────────────────────┐
│ Host                                   │
│  ┌─────────┐                          │
│  │ fcvmbr0 │ (172.16.0.1)             │
│  └────┬────┘                          │
│       │                                │
│  ┌────┴─────┐                         │
│  │ tap-vm1  │ ← connected to VM       │
│  └──────────┘                         │
│                                        │
│  nftables DNAT rules:                 │
│    tcp dport 8080 → 172.16.0.10:80   │
└───────────────────────────────────────┘
          │
          ▼
    ┌──────────────┐
    │ Firecracker  │
    │  eth0:       │
    │  172.16.0.10 │
    └──────────────┘
```

**Bridge Setup**:
```bash
ip link add fcvmbr0 type bridge
ip addr add 172.16.0.1/24 dev fcvmbr0
ip link set fcvmbr0 up
```

**TAP Device**:
```bash
ip tuntap add tap-vm1 mode tap
ip link set tap-vm1 master fcvmbr0
ip link set tap-vm1 up
```

**nftables Rules**:
```bash
# Create NAT table
nft add table ip nat

# DNAT for port forwarding
nft add rule ip nat PREROUTING tcp dport 8080 dnat to 172.16.0.10:80

# MASQUERADE for outbound
nft add rule ip nat POSTROUTING oifname "eth0" masquerade
```

**IP Allocation**:
- Bridge: `172.16.0.1/24`
- VMs: `172.16.0.10`, `172.16.0.11`, ... (incrementing)

---

## VM Lifecycle

### `fcvm podman run` Flow

```
┌─────────────────────────────────────────────────────────┐
│ 1. Parse CLI arguments                                   │
│    - Image, vCPU, memory, ports, volumes, snapshot name │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 2. Detect execution mode (auto/rootless/privileged)     │
│    - Check for root privileges                          │
│    - Check for /dev/kvm access                          │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 3. Setup networking                                      │
│    - Create TAP device (privileged) or prepare slirp    │
│    - Parse port mappings                                │
│    - Generate MAC address                               │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 4. Prepare disks                                         │
│    - Create CoW overlay from base rootfs                │
│    - Setup volume mounts (block/sshfs/nfs)              │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 5. Start Firecracker process                            │
│    - Spawn with Unix socket API                         │
│    - Wait for socket ready                              │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 6. Configure VM via API                                  │
│    - set_boot_source (kernel)                           │
│    - set_machine_config (vCPU, memory)                  │
│    - add_drive (rootfs)                                 │
│    - add_network_interface (TAP device)                 │
│    - set_mmds_config (metadata service)                 │
│    - put_mmds (container plan)                          │
│    - set_balloon (memory balloon if configured)         │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 7. Start VM                                              │
│    - put_action(InstanceStart)                          │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 8. Stream serial console logs                           │
│    - Open serial console device                         │
│    - Stream to stdout/file based on --logs flag         │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 9. Wait for readiness (if --wait-ready specified)       │
│    - vsock: Wait for guest connection                   │
│    - http: Poll HTTP endpoint                           │
│    - log: Search serial console for pattern             │
│    - exec: Execute command in guest                     │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 10. Save snapshot (if --save-snapshot specified)        │
│     - create_snapshot(memory + disk)                    │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 11. Setup signal handlers                               │
│     - SIGINT/SIGTERM → graceful shutdown                │
│     - SIGCHLD → detect VM exit                          │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 12. Wait for VM exit or signal                          │
│     - Process blocks here (hanging mode)                │
│     - VM lifetime = process lifetime                    │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 13. Cleanup                                              │
│     - Kill Firecracker process                          │
│     - Remove TAP device                                 │
│     - Remove NAT rules                                  │
│     - Clean up temp files                               │
└─────────────────────────────────────────────────────────┘
```

### `fcvm snapshot` Flow (Create → Serve → Run)

**Step 1: Create Snapshot** (`fcvm snapshot create`)
```
┌─────────────────────────────────────────────────────────┐
│ 1. Pause the running VM                                  │
│    - Firecracker API: pause                             │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 2. Create Firecracker snapshot                          │
│    - Snapshot memory to file                            │
│    - Snapshot disk state                                │
│    - Save VM configuration                              │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 3. Resume the original VM                               │
│    - VM continues running                               │
└─────────────────────────────────────────────────────────┘
```

**Step 2: Start Memory Server** (`fcvm snapshot serve`)
```
┌─────────────────────────────────────────────────────────┐
│ 1. Load snapshot memory file (mmap, MAP_SHARED)         │
│    - Kernel shares physical pages via page cache        │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 2. Create Unix socket for clone connections             │
│    - /mnt/fcvm-btrfs/uffd-{snapshot}-{pid}.sock         │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 3. Register state in state manager                      │
│    - process_type: "serve"                              │
│    - snapshot_name                                      │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 4. Wait for clone connections (async)                   │
│    - Handle UFFD page faults from clones                │
│    - Serve memory pages on-demand                       │
└─────────────────────────────────────────────────────────┘
```

**Step 3: Spawn Clone** (`fcvm snapshot run`)
```
┌─────────────────────────────────────────────────────────┐
│ 1. Create CoW overlay disk (btrfs reflink)              │
│    - cp --reflink=always (~1.5ms)                       │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 2. Setup new networking                                  │
│    - Generate new MAC address                           │
│    - Create TAP device (bridged) or slirp (rootless)    │
│    - Allocate loopback IP for health checks             │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 3. Start Firecracker with UFFD backend                  │
│    - Connect to memory server's Unix socket             │
│    - Firecracker fetches pages via UFFD on access       │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 4. Load snapshot via Firecracker API                    │
│    - enable_diff_snapshots = true                       │
│    - resume_vm = true                                   │
└────────────────┬────────────────────────────────────────┘
                 ▼
┌─────────────────────────────────────────────────────────┐
│ 5. VM resumes (< 1 second total startup)                │
│    - Memory pages loaded on-demand                      │
│    - Shared pages via kernel page cache                 │
└─────────────────────────────────────────────────────────┘
```

### Signal Handling (Process Lifetime Binding)

**Goal**: VM dies when `fcvm podman run` process exits.

**Implementation** (using `tokio::signal`):
```rust
use tokio::signal::unix::{signal, SignalKind};

async fn main() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Start VM
    let mut vm = VmManager::new(...);
    vm.start().await?;

    // Wait for signal or VM exit
    tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down");
            vm.kill().await?;
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down");
            vm.kill().await?;
        }
        status = vm.wait() => {
            info!("VM exited with status: {:?}", status);
        }
    }

    // Cleanup
    network.cleanup().await?;
    Ok(())
}
```

**Graceful Shutdown**:
1. Receive SIGTERM/SIGINT
2. Send shutdown signal to Firecracker
3. Wait up to 10 seconds for graceful exit
4. Force kill if timeout
5. Clean up network resources
6. Remove temporary files

---

## Guest Agent

### fc-agent Architecture

**Location**: `fc-agent/src/main.rs`

Runs inside the Firecracker VM as a systemd service.

**Responsibilities**:
1. Fetch container plan from MMDS (Metadata Service)
2. Launch Podman with correct configuration
3. Stream container logs to serial console
4. Signal readiness to host (via vsock)
5. Handle container lifecycle

### MMDS (Metadata Service)

Firecracker provides a metadata service accessible at `http://169.254.169.254/`.

**Container Plan Format**:
```json
{
  "image": "nginx:latest",
  "env": {
    "KEY": "VALUE",
    "DB_HOST": "localhost"
  },
  "cmd": ["/bin/sh", "-c", "nginx -g 'daemon off;'"],
  "volumes": [
    {
      "host": "/data",
      "guest": "/mnt/data",
      "readonly": false
    }
  ],
  "podman": {
    "rootless": true,
    "network": "host",
    "privileged": false
  },
  "readiness": {
    "mode": "http",
    "url": "http://127.0.0.1:80/health"
  },
  "logs": {
    "mode": "stream"
  }
}
```

### fc-agent Implementation

```rust
#[tokio::main]
async fn main() -> Result<()> {
    // 1. Fetch plan from MMDS
    let plan = fetch_mmds_plan().await?;

    // 2. Build Podman command
    let mut cmd = Command::new("podman");
    cmd.arg("run").arg("--rm");

    // Network mode
    if plan.podman.network == "host" {
        cmd.arg("--network=host");
    }

    // Environment variables
    for (key, val) in plan.env {
        cmd.arg("-e").arg(format!("{}={}", key, val));
    }

    // Volume mounts
    for vol in plan.volumes {
        let mount = if vol.readonly {
            format!("{}:{}:ro", vol.guest, vol.guest)
        } else {
            format!("{}:{}", vol.guest, vol.guest)
        };
        cmd.arg("-v").arg(mount);
    }

    // Image
    cmd.arg(&plan.image);

    // Command override
    if let Some(cmd_override) = plan.cmd {
        cmd.args(cmd_override);
    }

    // 3. Spawn container
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // 4. Stream logs to serial console
    stream_to_console(child.stdout.take(), "stdout").await;
    stream_to_console(child.stderr.take(), "stderr").await;

    // 5. Signal readiness (vsock or log marker)
    if let Some(readiness) = plan.readiness {
        signal_ready(readiness).await?;
    }

    // 6. Wait for container exit
    let status = child.wait().await?;
    eprintln!("[agent] container exited: {}", status);

    Ok(())
}

async fn fetch_mmds_plan() -> Result<Plan> {
    loop {
        match reqwest::get("http://169.254.169.254/").await {
            Ok(resp) => return resp.json().await.context("parsing MMDS"),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
```

### Rootless Podman Support

The guest is configured to support rootless Podman:

1. **User setup** (`create-rootfs-debian.sh`):
   ```bash
   # Create podman user
   useradd -m -s /bin/bash podman

   # Setup subuid/subgid ranges
   echo "podman:100000:65536" >> /etc/subuid
   echo "podman:100000:65536" >> /etc/subgid
   ```

2. **Podman configuration**:
   ```bash
   # Enable unprivileged port binding
   sysctl -w net.ipv4.ip_unprivileged_port_start=0

   # Use crun runtime (faster than runc)
   podman --runtime=crun
   ```

3. **fc-agent runs as**:
   - Root: When `podman.rootless = false`
   - podman user: When `podman.rootless = true`

---

## CLI Interface

### Commands

#### `fcvm podman run`

**Purpose**: Launch a container in a new Firecracker VM.

**Usage**:
```bash
fcvm podman run --name <NAME> [OPTIONS] <IMAGE>
```

**Arguments**:
- `IMAGE` - Container image (e.g., `nginx:alpine`, `ghcr.io/org/app:v1.0`)

**Options**:
```
--name <NAME>              VM name (required)
--cpu <COUNT>              vCPU count (default: 2)
--mem <MB>                 Memory in MiB (default: 2048)
--network <MODE>           Network mode: bridged|rootless (default: bridged)
--map <HOST:GUEST[:ro]>    Volume mount via FUSE (can specify multiple)
--env <KEY=VALUE>          Environment variable (can specify multiple)
--cmd <COMMAND>            Container command override
--publish <MAPPING>        Port publish (can specify multiple)
--balloon <MB>             Memory balloon target
--health-check <URL>       HTTP health check URL
--privileged               Run container in privileged mode
```

**Examples**:
```bash
# Simple nginx (bridged networking, requires sudo)
sudo fcvm podman run --name my-nginx nginx:alpine

# Rootless mode (no sudo required)
fcvm podman run --name my-nginx --network rootless nginx:alpine

# With port forwarding
sudo fcvm podman run --name web --publish 8080:80 nginx:alpine

# With volumes and environment
sudo fcvm podman run \
  --name db \
  --map /host/data:/data \
  --env DB_HOST=localhost \
  postgres:15

# With health check
sudo fcvm podman run \
  --name web \
  --health-check http://localhost/health \
  nginx:alpine

# High CPU/memory with balloon
sudo fcvm podman run \
  --name ml \
  --cpu 8 \
  --mem 8192 \
  --balloon 4096 \
  ml-training:latest
```

#### `fcvm snapshot create`

**Purpose**: Create a snapshot from a running VM.

**Usage**:
```bash
fcvm snapshot create [--pid <PID> | <VM_NAME>] [--tag <TAG>]
```

**Options**:
```
--pid <PID>               fcvm process PID to snapshot
--tag <TAG>               Snapshot name (defaults to VM name)
<VM_NAME>                 VM name to snapshot (alternative to --pid)
```

**Examples**:
```bash
# Create snapshot by PID
fcvm snapshot create --pid 12345 --tag my-snapshot

# Create snapshot by name
fcvm snapshot create my-vm --tag warm-nginx
```

#### `fcvm snapshot serve`

**Purpose**: Start a UFFD memory server for cloning.

**Usage**:
```bash
fcvm snapshot serve <SNAPSHOT_NAME>
```

The memory server:
- Loads the snapshot's memory file
- Listens for clone connections via Unix socket
- Serves memory pages on-demand via UFFD (userfaultfd)
- Enables sharing physical pages across multiple clones

**Example**:
```bash
# Start memory server (blocks, keeps running)
fcvm snapshot serve my-snapshot
```

#### `fcvm snapshot run`

**Purpose**: Spawn a clone VM from a running memory server.

**Usage**:
```bash
fcvm snapshot run --pid <SERVE_PID> [OPTIONS]
```

**Options**:
```
--pid <SERVE_PID>         Memory server PID (required)
--name <NAME>             Clone VM name (auto-generated if not provided)
--network <MODE>          Network mode: bridged|rootless
--publish <MAPPING>       Port mappings (can differ from original)
```

**Examples**:
```bash
# Spawn a clone
fcvm snapshot run --pid 12345 --name clone1 --network bridged

# Multiple clones in parallel
for i in {1..10}; do
  fcvm snapshot run --pid 12345 --name clone$i --publish $((8000+i)):80 &
done
wait  # Lightning fast: all start in <1 second each
```

#### `fcvm snapshot ls`

**Purpose**: List running memory servers.

```bash
fcvm snapshot ls
```

#### `fcvm ls`

**Purpose**: List running VMs.

**Usage**:
```bash
fcvm ls [--json] [--pid <PID>]
```

**Options**:
```
--json                    Output in JSON format
--pid <PID>               Filter by fcvm process PID
```

**Example output**:
```
NAME           PID     STATUS    HEALTH    NETWORK   IMAGE
my-nginx       12345   running   healthy   bridged   nginx:alpine
clone-1        12350   running   healthy   rootless  (clone)
```

#### `fcvm snapshots`

**Purpose**: List available snapshots.

```bash
fcvm snapshots
```

---

## Implementation Details

### Directory Structure

```
fcvm/
├── Cargo.toml              # Workspace manifest
├── DESIGN.md               # This document
├── README.md               # User-facing documentation
├── Makefile                # Build and test commands
├── Containerfile           # Test container definition
│
├── src/                    # Host CLI (fcvm binary)
│   ├── main.rs             # Entry point
│   ├── lib.rs              # Module exports
│   ├── paths.rs            # Path utilities for btrfs layout
│   ├── health.rs           # Health monitoring
│   │
│   ├── cli/                # Command-line parsing
│   │   ├── mod.rs
│   │   └── args.rs         # Clap structures
│   │
│   ├── commands/           # CLI command implementations
│   │   ├── mod.rs
│   │   ├── ls.rs           # fcvm ls
│   │   ├── podman.rs       # fcvm podman run
│   │   ├── snapshot.rs     # fcvm snapshot {create,serve,run}
│   │   ├── snapshots.rs    # fcvm snapshots
│   │   ├── setup.rs        # fcvm setup
│   │   ├── memory_server.rs # UFFD memory server subprocess
│   │   └── common.rs       # Shared utilities
│   │
│   ├── firecracker/        # Firecracker integration
│   │   ├── mod.rs
│   │   ├── api.rs          # API client (hyper + hyperlocal)
│   │   └── vm.rs           # VM manager
│   │
│   ├── network/            # Networking
│   │   ├── mod.rs
│   │   ├── bridged.rs      # Bridged networking (iptables)
│   │   ├── slirp.rs        # Rootless networking (slirp4netns)
│   │   ├── namespace.rs    # Network namespace management
│   │   ├── veth.rs         # Veth pair management
│   │   ├── types.rs        # Network types
│   │   └── portmap.rs      # Port mapping utilities
│   │
│   ├── storage/            # Storage & snapshots
│   │   ├── mod.rs
│   │   ├── disk.rs         # btrfs CoW disk management
│   │   ├── snapshot.rs     # Snapshot management
│   │   └── volume.rs       # Volume handling
│   │
│   ├── state/              # VM state management
│   │   ├── mod.rs
│   │   ├── types.rs        # VmState, VmConfig
│   │   ├── manager.rs      # StateManager (CRUD + loopback IPs)
│   │   └── utils.rs        # State utilities
│   │
│   ├── uffd/               # UFFD memory server
│   │   ├── mod.rs
│   │   ├── server.rs       # Userfaultfd page handler
│   │   └── handler.rs      # UFFD event handler
│   │
│   ├── volume/             # FUSE volume handling
│   │   └── mod.rs          # Host → guest filesystem mapping
│   │
│   └── setup/              # Setup utilities
│       ├── mod.rs
│       ├── preflight.rs    # Pre-flight checks
│       ├── kernel.rs       # Kernel setup
│       ├── kernel_build.rs # Kernel build
│       └── rootfs.rs       # Rootfs setup
│
├── fc-agent/               # Guest agent crate
│   ├── Cargo.toml
│   └── src/
│       └── main.rs         # MMDS + Podman orchestration
│
├── fuse-pipe/              # FUSE passthrough library
│   ├── Cargo.toml
│   ├── src/
│   │   ├── client/         # FUSE client (mounts in VM)
│   │   ├── server/         # Async server (runs on host)
│   │   ├── protocol/       # Wire protocol (request/response)
│   │   └── transport/      # vsock/Unix socket transport
│   ├── tests/              # Integration tests
│   └── benches/            # Performance benchmarks
│
└── tests/                  # fcvm integration tests
    ├── common/mod.rs       # Shared test utilities
    ├── test_sanity.rs      # VM sanity tests
    ├── test_state_manager.rs
    ├── test_health_monitor.rs
    ├── test_fuse_posix.rs
    ├── test_fuse_in_vm.rs
    ├── test_localhost_image.rs
    └── test_snapshot_clone.rs
```

### Dependencies

**fcvm** (`Cargo.toml`):
```toml
[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.11", features = ["json", "rustls-tls"] }
which = "6"
nix = { version = "0.29", features = ["user", "process", "signal", "ioctl", "net"] }
uuid = { version = "1", features = ["v4", "serde"] }
sha2 = "0.10"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
libc = "0.2"
hex = "0.4"
chrono = { version = "0.4", features = ["serde"] }
tempfile = "3"
rand = "0.8"
async-trait = "0.1"
hyper = { version = "0.14", features = ["client", "http1"] }
hyperlocal = "0.8"
```

**fc-agent** (`fc-agent/Cargo.toml`):
```toml
[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "io-util"] }
reqwest = { version = "0.11", features = ["json", "rustls-tls"] }
```

### Build System (Makefile)

All builds are done via the root Makefile.

**Key targets**:
```bash
# Development
make build         # Build fcvm + fc-agent
make clean         # Clean build artifacts

# Testing
make test          # Run fuse-pipe tests (noroot + root)
make test-vm       # Run VM tests (rootless + bridged)
make test-all      # Everything: test + test-vm + test-pjdfstest

# Linting
make lint          # Run clippy + fmt-check
make fmt           # Format code

# Container testing
make container-test    # fuse-pipe tests in container
make container-test-vm # VM tests in container
make container-shell   # Interactive shell
```

See `make help` for the complete list of targets.

### Configuration File

**Location**: `~/.config/fcvm/config.yml` or `/etc/fcvm/config.yml`

**Format**:
```yaml
# Data directory for VM state
data_dir: /var/lib/fcvm

# Firecracker binary path
firecracker_bin: /usr/local/bin/firecracker

# Kernel image
kernel_path: /var/lib/fcvm/kernels/vmlinux.bin

# Base rootfs image
rootfs_path: /var/lib/fcvm/rootfs/base.ext4

# Default settings
defaults:
  mode: auto
  vcpu: 2
  memory_mib: 2048
  map_mode: block
  logs: stream

# Network configuration
network:
  mode: auto
  bridge: fcvmbr0
  subnet: 172.16.0.0/24
  guest_ip_start: 172.16.0.10

# Logging
logging:
  level: info
  format: json
```

### State Persistence

**VM State** (`~/.local/share/fcvm/vms/<vm-id>/state.json`):
```json
{
  "vm_id": "abc123",
  "name": "my-nginx",
  "status": "running",
  "pid": 12345,
  "created_at": "2025-01-09T12:00:00Z",
  "config": {
    "image": "nginx:latest",
    "vcpu": 2,
    "memory_mib": 2048,
    "network": {
      "mode": "rootless",
      "tap_device": "tap-abc123",
      "guest_mac": "02:aa:bb:cc:dd:ee",
      "guest_ip": "10.0.2.15",
      "port_mappings": [
        {"host_port": 8080, "guest_port": 80, "proto": "tcp"}
      ]
    },
    "disks": [
      {
        "path": "/var/lib/fcvm/vms/abc123/rootfs.ext4",
        "is_root": true
      }
    ],
    "volumes": [
      {"host": "/data", "guest": "/mnt/data", "readonly": false}
    ]
  }
}
```

### Error Handling

**Strategy**: Use `anyhow::Result` everywhere, with context.

**Example**:
```rust
use anyhow::{Context, Result, bail};

async fn setup_network() -> Result<NetworkConfig> {
    create_tap_device("tap0")
        .await
        .context("creating TAP device for VM network")?;

    add_to_bridge("tap0", "fcvmbr0")
        .await
        .context("adding TAP to bridge")?;

    Ok(NetworkConfig { ... })
}
```

**User-facing errors**:
```rust
// In main.rs
if let Err(e) = run().await {
    eprintln!("Error: {:#}", e);  // Pretty print error chain
    std::process::exit(1);
}
```

### Logging

**Setup** (in `main.rs`):
```rust
use tracing_subscriber::{fmt, EnvFilter};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    // ...
}
```

**Usage**:
```rust
use tracing::{info, warn, error, debug};

info!(vm_id = %vm.id(), "starting VM");
warn!(tap = "tap0", "TAP device already exists");
error!(error = %e, "failed to start Firecracker");
debug!(config = ?config, "loaded configuration");
```

**Environment**:
```bash
# Set log level
export RUST_LOG=fcvm=debug

# Run with debug logs
RUST_LOG=trace fcvm run nginx:latest
```

---

## Testing Strategy

### Test Infrastructure

**Network Mode Guards**: The fcvm binary enforces proper network mode usage:
- **Bridged without root**: Fails with helpful error message suggesting `sudo` or `--network rootless`
- **Rootless with root**: Runs but prints warning that bridged would be faster

**Test Isolation**: All tests use unique resource names to enable parallel execution:
- `unique_names()` helper generates timestamp+counter-based names
- PID-based naming for additional uniqueness
- Automatic cleanup on test exit

**Dynamic NBD Device Selection**: When creating rootfs (extracting qcow2 images):
- Scans `/dev/nbd0` through `/dev/nbd15` to find a free device
- Checks `/sys/block/nbdN/pid` to detect in-use devices
- Includes retry logic for race conditions during parallel execution

**Privileged/Unprivileged Test Organization**:
- Tests requiring sudo use `#[cfg(feature = "privileged-tests")]`
- Unprivileged tests run by default (no feature flag needed)
- Privileged tests: Need sudo for iptables, root podman storage
- Unprivileged tests: Run without sudo, use slirp4netns networking
- Makefile uses `--features` for selection: `make test-vm FILTER=exec` runs all exec tests
- Container tests: Use appropriate container run configurations (CONTAINER_RUN_FCVM vs CONTAINER_RUN_UNPRIVILEGED)

### Unit Tests

Test individual components in isolation:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_port_mapping() {
        let pm = PortMapping::parse("8080:80").unwrap();
        assert_eq!(pm.host_port, 8080);
        assert_eq!(pm.guest_port, 80);
        assert_eq!(pm.proto, Protocol::Tcp);
    }

    #[tokio::test]
    async fn test_firecracker_client() {
        // Mock Firecracker API
        // Test API calls
    }
}
```

### Integration Tests

Test full workflows:

```bash
#!/bin/bash
# tests/integration/test_run.sh

# Test rootless mode (no sudo required)
fcvm podman run --name test-nginx --network rootless nginx:alpine &
PID=$!
sleep 5
kill $PID

# Test bridged mode with port forwarding (requires sudo for iptables/TAP)
sudo fcvm podman run --name web --network bridged --publish 8080:80 nginx:alpine &
PID=$!
sleep 5
curl http://localhost:8080  # Should return nginx page
kill $PID

# Test snapshot & clone (rootless)
fcvm podman run --name baseline --network rootless nginx:alpine &
BASELINE_PID=$!
sleep 5  # Wait for VM to be healthy

# Create snapshot
fcvm snapshot create --pid $BASELINE_PID --tag warm

# Start memory server
fcvm snapshot serve warm &
SERVE_PID=$!
sleep 2

# Spawn clone
fcvm snapshot run --pid $SERVE_PID --name clone1 --network rootless --publish 9090:80 &
CLONE_PID=$!
sleep 2
curl http://localhost:9090  # Should return nginx page in <2s

kill $CLONE_PID $SERVE_PID $BASELINE_PID
```

**Note**: `--network rootless` uses slirp4netns (no root required). `--network bridged` (default) uses iptables/TAP devices (requires sudo).

---

## Performance Targets

### Clone Speed

**Goal**: <1 second from `fcvm clone` to ready

**Breakdown**:
- Snapshot load: ~200ms
- Network setup: ~100ms
- Identity patching: ~50ms
- VM resume: ~300ms
- Container ready: ~300ms
- **Total**: ~950ms

**Optimizations**:
- Pre-warmed snapshot (container already running)
- CoW disks (no disk copy)
- Shared memory pages
- Fast network setup (TAP device creation)

### Resource Efficiency

**Memory**:
- Base VM: ~100MB overhead
- Shared kernel + rootfs: ~200MB (shared across all VMs)
- Per-VM: Container memory + ~100MB overhead

**Example**: 10 nginx VMs
- Traditional VMs: 10 × 512MB = 5GB
- fcvm with cloning: 200MB (shared) + 10 × 150MB = 1.7GB
- **Savings**: ~66%

**CPU**:
- Support vCPU overcommit (e.g., 32 vCPUs on 8 cores)
- KVM handles scheduling efficiently
- Minimal overhead when VMs are idle

---

## Security Considerations

### Isolation

- **VM-level isolation**: Full hardware virtualization via KVM
- **No shared kernel**: Each VM has its own kernel
- **No container escape**: Podman runs inside VM, not on host

### Rootless Mode

- **No root required**: Entire stack runs as regular user
- **User namespaces**: slirp4netns uses user namespaces
- **No privileged operations**: No sudo, no CAP_NET_ADMIN

### Privileged Mode

- **Requires CAP_NET_ADMIN**: For TAP/bridge/nftables setup
- **Minimal privileges**: Only for network setup, not VM execution
- **Firecracker jailer**: Can use jailer for additional sandboxing (future)

### Snapshot Security

- **Snapshot contains full VM state**: Including memory (may have secrets)
- **Encrypt snapshots**: Option to encrypt at rest (future)
- **Access control**: Snapshots stored in user-owned directories

---

## Future Enhancements

### Phase 2 (Post-MVP)

1. **Persistent volumes**:
   - Support Docker volumes API
   - Persistent storage across clones

2. **Custom networks**:
   - User-defined networks
   - VM-to-VM communication

3. **Resource limits**:
   - CPU pinning
   - Memory limits (cgroups)
   - I/O throttling

4. **Metrics & monitoring**:
   - Prometheus exporter
   - Real-time resource graphs

5. **Snapshot encryption**:
   - Encrypt memory snapshots
   - Key management

6. **Jailer integration**:
   - Use Firecracker jailer for additional sandboxing
   - chroot, cgroups, seccomp

7. **Multi-host support**:
   - Distribute VMs across multiple hosts
   - Remote snapshots

### Phase 3 (Advanced Features)

1. **Live migration**:
   - Migrate running VMs between hosts
   - Zero-downtime updates

2. **GPU passthrough**:
   - vGPU support for ML workloads

3. **Kubernetes integration**:
   - Run as CRI runtime
   - Pod → Firecracker VM

---

## Glossary

- **Firecracker**: Lightweight VMM (Virtual Machine Monitor) from AWS
- **microVM**: Minimalistic virtual machine with fast boot times
- **KVM**: Kernel-based Virtual Machine, Linux's hypervisor
- **MMDS**: Micro Metadata Service, Firecracker's metadata API
- **TAP device**: Virtual network interface (TUN/TAP)
- **slirp4netns**: User-mode networking for rootless containers
- **CoW**: Copy-on-Write, disk strategy for fast cloning
- **nftables**: Linux firewall/NAT configuration tool
- **vsock**: Virtual socket for host-guest communication
- **Balloon device**: Memory reclamation mechanism for VMs

---

## References

- [Firecracker Documentation](https://github.com/firecracker-microvm/firecracker/tree/main/docs)
- [Firecracker API Specification](https://github.com/firecracker-microvm/firecracker/blob/main/src/api_server/swagger/firecracker.yaml)
- [Podman Documentation](https://docs.podman.io/)
- [slirp4netns](https://github.com/rootless-containers/slirp4netns)
- [nftables Wiki](https://wiki.nftables.org/)
- [KVM Documentation](https://www.linux-kvm.org/page/Documents)

---

**End of Design Specification**

*Version: 2.1*
*Date: 2025-12-21*
*Author: fcvm project*
