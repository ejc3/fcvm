#!/usr/bin/env bash
set -euo pipefail
set -o pipefail

ROOT="${HOME}/.local/share/fcvm"
BIN="${ROOT}/bin"
IMG="${ROOT}/images"

mkdir -p "${BIN}" "${IMG}"

source "$(dirname "$0")/../.env"

echo "[init] ROOT=${ROOT}"
echo "[init] downloading Firecracker (${FC_RELEASE_CHANNEL}) ..."

if [[ "${FC_RELEASE_CHANNEL}" == "latest" ]]; then
  TAG="$(curl -s https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | jq -r .tag_name)"
else
  TAG="${FC_RELEASE_CHANNEL}"
fi

FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${TAG}/firecracker-${FC_ARCH}"
JAILER_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${TAG}/jailer-${FC_ARCH}"

curl -L --fail -o "${BIN}/firecracker" "${FC_URL}"
curl -L --fail -o "${BIN}/jailer" "${JAILER_URL}"
chmod +x "${BIN}/firecracker" "${BIN}/jailer"
echo "[init] firecracker installed to ${BIN}"

# Build guest agent and copy into rootfs later
echo "[init] building guest agent ..."
( cd "$(dirname "$0")/.."; cargo build --release -p fc-agent )
AGENT_BIN="$(dirname "$0")/../target/release/fc-agent"

# Build rootfs (Debian + Podman) and pack into ext4 image
echo "[init] building Debian rootfs with Podman ..."
"$(dirname "$0")/create-rootfs-debian.sh" "${IMG}/rootfs" "${AGENT_BIN}"

echo "[init] creating ext4 image of size ${ROOTFS_SIZE_GB}G ..."
truncate -s "${ROOTFS_SIZE_GB}G" "${IMG}/rootfs.ext4"
mkfs.ext4 -F "${IMG}/rootfs.ext4" >/dev/null

TMPMNT="$(mktemp -d)"
sudo mount -o loop "${IMG}/rootfs.ext4" "${TMPMNT}"
sudo rsync -aHAX "${IMG}/rootfs"/ "${TMPMNT}/"
sudo umount "${TMPMNT}"
rmdir "${TMPMNT}"

echo "[init] NOTE: provide a kernel image at ${IMG}/${KERNEL_IMAGE_NAME} (vmlinux)."
echo "[init] done."
