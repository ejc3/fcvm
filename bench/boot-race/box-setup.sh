#!/bin/bash
# Set up a parallel box (pbox up N kvm-96) for a boot-race campaign: host prep from CLAUDE.md "Fresh Bare-Metal
# Box Quickstart", fcvm at a pinned commit, then the workload image. Run as ubuntu: box-setup.sh <fcvm commit>.
# Expects ~/boot-race/img/{boot-race-workload,Containerfile} copied from bench/boot-race/workload.
set -euxo pipefail
sudo usermod -aG kvm ubuntu
sudo tee /etc/sysctl.d/99-fcvm.conf >/dev/null <<'SYSCTL'
kernel.unprivileged_userns_clone=1
kernel.apparmor_restrict_unprivileged_userns=0
SYSCTL
sudo sysctl -p /etc/sysctl.d/99-fcvm.conf
grep -Eq '^[[:space:]]*user_allow_other[[:space:]]*$' /etc/fuse.conf || echo user_allow_other | sudo tee -a /etc/fuse.conf
sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=900 update -q
sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=900 install -y -q build-essential git jq qemu-utils busybox-static \
    pkg-config libssl-dev clang make podman skopeo uidmap slirp4netns fuse3 passt \
    btrfs-progs bc bison flex libelf-dev libseccomp-dev python3-boto3 python3-botocore sysstat
# The pbox bootstrap assembles the instance store as md127 (ext4 at /mnt/nvme). fcvm wants btrfs: reformat it.
if ! mountpoint -q /mnt/fcvm-btrfs; then
  [ -z "$(ls -A /mnt/nvme 2>/dev/null | grep -v lost+found)" ] || { echo "/mnt/nvme not empty"; exit 1; }
  sudo umount /mnt/nvme || true
  sudo mkfs.btrfs -f /dev/md127
  sudo mkdir -p /mnt/fcvm-btrfs && sudo mount /dev/md127 /mnt/fcvm-btrfs
  sudo chown ubuntu:ubuntu /mnt/fcvm-btrfs
fi
[ -e /dev/userfaultfd ] || sudo mknod /dev/userfaultfd c 10 126
sudo chmod 666 /dev/userfaultfd
sudo sysctl -w vm.unprivileged_userfaultfd=1
mkdir -p /mnt/fcvm-btrfs/cargo /mnt/fcvm-btrfs/rustup
[ -e ~/.cargo ] || ln -s /mnt/fcvm-btrfs/cargo ~/.cargo
[ -e ~/.rustup ] || ln -s /mnt/fcvm-btrfs/rustup ~/.rustup
[ -x ~/.cargo/bin/cargo ] || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
mkdir -p /mnt/fcvm-btrfs/src && cd /mnt/fcvm-btrfs/src
[ -d fcvm ] || git clone -q https://github.com/ejc3/fcvm.git
[ -d fuse-backend-rs ] || git clone -q https://github.com/ejc3/fuse-backend-rs.git
[ -d fuser ] || git clone -q https://github.com/ejc3/fuser.git
cd fcvm && git checkout -q "$1"

cd /mnt/fcvm-btrfs/src/fcvm
# a new login picks up the kvm group
sg kvm -c ". ~/.cargo/env && make build && make setup-fcvm"
podman build -t localhost/boot-race-workload ~/boot-race/img
echo BOX_SETUP_DONE
