# fcvm-starter (pre-wired)
A ready-to-run starter kit that sets up the basics for a Rust CLI (`fcvm`) that launches Firecracker microVMs to run Podman containers, with **rootless** and **privileged** host modes.

> **What you get**
> - A cargo workspace with two crates: `fcvm/` (host CLI) and `fc-agent/` (guest agent).
> - Scripts to fetch Firecracker, build a minimal Debian rootfs with Podman, and prep kernel/rootfs.
> - Networking templates for **nftables** (privileged) and **slirp4netns** hostfwd (rootless).
> - Example MMDS plan, systemd unit for the agent, and a reference config file.

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

### 1.4 First run (rootless, egress-only, with two published ports QEMU-style)
```bash
# This doesn't start a real VM yet; the CLI is a scaffold showing the exact argument surface.
./target/release/fcvm run ghcr.io/acme/web:latest   --name web1 --mode rootless   --publish 10080:80,10443:443   --logs both
```

### 1.5 Warm snapshot flow (scaffold)
```bash
./target/release/fcvm run ghcr.io/acme/web:latest   --name warmup --mode auto   --save-snapshot warm-web   --wait-ready mode=vsock

./target/release/fcvm clone warmup --name web-a --snapshot warm-web --mode auto
```

> The code is structured so you can fill in Firecracker API calls, networking, disks, and snapshots step-by-step.

---

## 2) Repo Layout

```
fcvm-starter/
  .env
  README.md
  Makefile
  config/
    fcvm.example.yml
  templates/
    mmds-plan-example.json
  network/
    nftables-template.nft
  scripts/
    preflight.sh
    fcvm-init.sh
    create-rootfs-debian.sh
    build-kernel.sh
    setup-nftables.sh
  fcvm/           # host CLI
    Cargo.toml
    src/
      main.rs
      cli.rs
      lib.rs
  fc-agent/       # guest agent
    Cargo.toml
    src/
      main.rs
    fc-agent.service
```

---

## 3) Notes

- **Inbound is off by default**. Publishing is optional:
  - **Rootless**: `--publish` creates slirp hostfwd rules (defaults to 127.0.0.1).
  - **Privileged**: `--publish` programs nftables DNAT rules.
- **Mapping modes**:
  - **block** (default; snapshot-friendly), **sshfs** (rootless OK), **nfs** (privileged only).
- For **VM-in-VM**, ensure nested virtualization is enabled so `/dev/kvm` exists inside your outer VM.

Happy hacking!
