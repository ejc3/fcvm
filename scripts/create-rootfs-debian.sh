#!/usr/bin/env bash
# Build a minimal Debian rootfs with Podman and install the fc-agent.
# Usage: create-rootfs-debian.sh <OUT_DIR> <FC_AGENT_BIN>
set -euo pipefail

OUT_DIR="${1:-}"
AGENT_BIN="${2:-}"

if [[ -z "${OUT_DIR}" || -z "${AGENT_BIN}" ]]; then
  echo "usage: $0 <OUT_DIR> <FC_AGENT_BIN>" >&2
  exit 1
fi

SUITE="${DEBIAN_SUITE:-bookworm}"

mkdir -p "${OUT_DIR}"
echo "[rootfs] target: ${OUT_DIR}"

build_with_mmdebstrap() {
  echo "[rootfs] using mmdebstrap (rootless)"
  mmdebstrap --variant=minbase "${SUITE}" "${OUT_DIR}"     "deb http://deb.debian.org/debian ${SUITE} main"     --include=systemd,podman,conmon,crun,fuse-overlayfs,iproute2,curl,jq
}

build_with_debootstrap() {
  echo "[rootfs] using debootstrap (requires sudo)"
  sudo debootstrap --variant=minbase "${SUITE}" "${OUT_DIR}" http://deb.debian.org/debian
  sudo chroot "${OUT_DIR}" /bin/sh -c "apt-get update && apt-get install -y podman conmon crun fuse-overlayfs iproute2 curl jq"
}

if command -v mmdebstrap >/dev/null 2>&1; then
  build_with_mmdebstrap
else
  build_with_debootstrap
fi

# Configure Podman rootless user (podman)
echo "[rootfs] creating user 'podman'"
sudo chroot "${OUT_DIR}" /usr/sbin/useradd -m -s /bin/bash podman || true
echo "podman:100000:65536" | sudo tee -a "${OUT_DIR}/etc/subuid" >/dev/null
echo "podman:100000:65536" | sudo tee -a "${OUT_DIR}/etc/subgid" >/dev/null

# Install fc-agent
echo "[rootfs] installing fc-agent"
sudo install -D -m 0755 "${AGENT_BIN}" "${OUT_DIR}/usr/local/bin/fc-agent"

# systemd unit
cat <<'UNIT' | sudo tee "${OUT_DIR}/etc/systemd/system/fc-agent.service" >/dev/null
[Unit]
Description=Firecracker Guest Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/fc-agent
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT

# Enable on boot
sudo chroot "${OUT_DIR}" systemctl enable fc-agent.service || true

echo "[rootfs] done."
