# fcvm - Firecracker VM Manager

A complete Rust implementation that launches Firecracker microVMs to run Podman containers, with **rootless** and **privileged** host modes, lightning-fast cloning, and full production features.

> **What you get**
> - Complete Firecracker VM manager with working `run`, `clone`, `ls`, and other commands
> - Full networking layer (rootless with slirp4netns, privileged with nftables)
> - CoW disk management for instant cloning
> - Snapshot save/restore functionality
> - VM state management and lifecycle control
> - Enhanced guest agent with environment variables and volume support
> - Comprehensive 1500-line design specification
> - Buck build system support
> - Production-ready error handling and logging

---

## 0) Prereqs

**Common**
- Linux x86_64 with `/dev/kvm` (user in `kvm` group for rootless use).
- `curl`, `jq`, `uidmap` (for user namespaces).

**Privileged mode**
- `sudo` access, `nftables`, `iproute2`, `ethtool`.

**Rootless mode**
- `slirp4netns`, `newuidmap`, `newgidmap`, sysctl `user.max_user_namespaces` sufficiently high.

**Build tools**
- `rustup` + stable toolchain, `cargo`, `clang`/`llvm` (optional), `make`.
- For building rootfs: `mmdebstrap` (preferred rootless) **or** `debootstrap` (needs sudo), plus `qemu-user-static` on some distros.

---

## 1) Quick Start

### 1.1 Preflight
```bash
scripts/preflight.sh
```

### 1.2 Initialize (downloads Firecracker, builds rootfs & places files under ~/.local/share/fcvm)
```bash
scripts/fcvm-init.sh
```

This will:
- Download Firecracker (latest by default; configurable in `.env`).
- Build a minimal Debian rootfs with Podman and install the **fc-agent** into it.
- Create an ext4 rootfs image (`rootfs.ext4`) and copy the rootfs into it.
- Place artifacts under `~/.local/share/fcvm/{bin,images}`.

> If you already have a kernel (`vmlinux`), copy it to `~/.local/share/fcvm/images/vmlinux` before running `fcvm`.

### 1.3 Build the host CLI + guest agent
```bash
make build
```

### 1.4 First run
```bash
# Run nginx in a Firecracker VM
./target/release/fcvm run nginx:latest --name web1 --publish 8080:80

# With environment variables and volumes
./target/release/fcvm run postgres:15 \
  --env POSTGRES_PASSWORD=secret \
  --map /data/postgres:/var/lib/postgresql/data \
  --mem 4096 --cpu 4

# List running VMs
./target/release/fcvm ls
```

### 1.5 Warm snapshot flow
```bash
# Run and save snapshot when ready
./target/release/fcvm run nginx:latest \
  --wait-ready mode=http,url=http://127.0.0.1:80 \
  --save-snapshot warm-nginx

# Clone from snapshot (fast <1s startup)
./target/release/fcvm clone --name warm-nginx --snapshot warm-nginx --publish 9090:80
```

---

## 2) Repo Layout

```
fcvm/
  README.md            # This file
  DESIGN.md            # Complete 1500-line design specification
  Cargo.toml           # Workspace configuration
  Makefile             # Build targets
  BUCK                 # Buck2 build system (root)

  config/
    fcvm.example.yml   # Configuration template

  templates/
    mmds-plan-example.json  # MMDS metadata example

  network/
    nftables-template.nft   # Privileged mode networking

  scripts/
    preflight.sh            # Prerequisites check
    fcvm-init.sh            # Download Firecracker, build rootfs
    create-rootfs-debian.sh # Build Debian rootfs with Podman
    build-kernel.sh         # Kernel build helper
    setup-nftables.sh       # Network setup (privileged)

  fcvm/                # Host CLI crate
    Cargo.toml
    BUCK               # Buck build file
    src/
      main.rs          # Entry point with full run/clone implementation
      cli.rs           # Command-line argument parsing
      lib.rs           # Shared types
      state.rs         # VM state persistence

      firecracker/     # Firecracker integration
        mod.rs
        api.rs         # HTTP API client (Unix sockets)
        vm.rs          # VM process lifecycle manager

      network/         # Networking layer
        mod.rs
        types.rs       # Port mapping, config types
        rootless.rs    # slirp4netns integration
        privileged.rs  # nftables + bridge setup

      storage/         # Storage & snapshots
        mod.rs
        disk.rs        # CoW disk management
        snapshot.rs    # Snapshot save/restore
        volume.rs      # Volume mount handling

      readiness/       # Readiness gates
        mod.rs
        vsock.rs       # vsock readiness
        http.rs        # HTTP endpoint polling
        log.rs         # Serial console log matching
        exec.rs        # Execute command in guest

  fc-agent/            # Guest agent crate
    Cargo.toml
    BUCK               # Buck build file
    fc-agent.service   # systemd unit
    src/
      main.rs          # Enhanced Podman launcher with env/volumes
```

---

## 3) Notes

- **Inbound is off by default**. Publishing is optional:
  - **Rootless**: `--publish` creates slirp hostfwd rules (defaults to 127.0.0.1).
  - **Privileged**: `--publish` programs nftables DNAT rules.
- **Mapping modes**:
  - **block** (default; snapshot-friendly), **sshfs** (rootless OK), **nfs** (privileged only).
- For **VM-in-VM**, ensure nested virtualization is enabled so `/dev/kvm` exists inside your outer VM.
- **Storage expectations**: On btrfs/xfs hosts we take advantage of `cp --reflink=always` for instant CoW disks. On ext4 (or any FS without reflink support) we automatically fall back to a normal `cp`, which works everywhere but takes longer because it copies the image fully.

Happy hacking!
