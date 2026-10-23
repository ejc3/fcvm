#!/bin/bash
# Build the fcvm fleet GPU host AMI's contents on a g4dn.metal launched from Ubuntu 24.04
# (fleet/ami/build-ami.sh launches it and drives these phases through SSM). Run as root:
#
#   build.sh prep    packages, host settings, IOMMU on the kernel command line, boot units;
#                    the caller reboots the instance afterwards
#   build.sh build   fcvm at $FCVM_COMMIT (make build, make setup-gpu), the renderer image,
#                    one GPU VM to first frame (warms the image export and snapshot cache),
#                    and the renderer image's export at /mnt/fcvm-btrfs/renderer.oci.tar.zst
#   build.sh bake    copies the store into /var/lib/fcvm-fleet/store.tar.zst and cleans up
#                    for imaging
#
# Every phase logs to /var/log/fleet-ami/<phase>.log and leaves <phase>.done or
# <phase>.failed in /var/lib/fleet-ami.
set -euo pipefail
PHASE=$1
HERE=$(cd "$(dirname "$0")" && pwd)
S=/var/lib/fleet-ami; L=/var/log/fleet-ami; mkdir -p "$S" "$L"
[ -f "$S/env" ] && . "$S/env"
exec > >(tee -a "$L/$PHASE.log") 2>&1
# EXIT, not ERR: an ERR trap does not fire for a failure inside a function under set -e.
trap 'rc=$?; [ $rc = 0 ] || { echo "PHASE $PHASE FAILED (exit $rc)"; touch "$S/$PHASE.failed"; }' EXIT
rm -f "$S/$PHASE.done" "$S/$PHASE.failed"
echo "== $PHASE start $(date -u +%FT%TZ)"
export DEBIAN_FRONTEND=noninteractive
APT="apt-get -o DPkg::Lock::Timeout=600"
U=ubuntu; UH=/home/$U; SRC=$UH/fcvm
# Rootless podman needs the user's systemd instance and runtime directory (lingering, set in prep).
as_user() {
  local uid; uid=$(id -u "$U")
  sudo -u "$U" -H env PATH="$UH/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    XDG_RUNTIME_DIR=/run/user/$uid DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus bash -c "$1"
}

case "$PHASE" in
prep)
  # The AMI's kernel must not move under the store and the NVIDIA-free host: no unattended upgrades.
  systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer || true
  $APT update
  $APT install -y build-essential git jq qemu-utils busybox-static pkg-config libssl-dev clang make \
    podman skopeo uidmap slirp4netns fuse3 passt btrfs-progs bc bison flex libelf-dev libseccomp-dev \
    curl zstd ffmpeg python3-pil
  # AGENTS.md "Fresh Bare-Metal Box Quickstart", steps 1, 1b, 1c and 4, made persistent.
  usermod -aG kvm "$U"
  loginctl enable-linger "$U"
  cat > /etc/sysctl.d/99-fcvm.conf <<'X'
kernel.unprivileged_userns_clone=1
kernel.apparmor_restrict_unprivileged_userns=0
vm.unprivileged_userfaultfd=1
X
  sysctl -p /etc/sysctl.d/99-fcvm.conf
  grep -Eq '^[[:space:]]*user_allow_other[[:space:]]*$' /etc/fuse.conf || echo user_allow_other >> /etc/fuse.conf
  cat > /etc/udev/rules.d/70-fcvm-fleet.rules <<'X'
KERNEL=="userfaultfd", MODE="0666"
SUBSYSTEM=="vfio", GROUP="kvm", MODE="0660"
X
  printf '%s soft memlock unlimited\n%s hard memlock unlimited\n' "$U" "$U" > /etc/security/limits.d/90-vfio-memlock.conf
  # GPUs: the Intel IOMMU on (iommu=pt keeps the host's own devices untranslated), nouveau
  # never loaded, vfio-pci loaded early; fcvm-fleet-vfio.service binds every NVIDIA function.
  echo 'GRUB_CMDLINE_LINUX_DEFAULT="$GRUB_CMDLINE_LINUX_DEFAULT intel_iommu=on iommu=pt"' > /etc/default/grub.d/90-fcvm-fleet-iommu.cfg
  printf 'blacklist nouveau\noptions nouveau modeset=0\nsoftdep nouveau pre: vfio-pci\n' > /etc/modprobe.d/fcvm-fleet-vfio.conf
  echo vfio-pci > /etc/modules-load.d/vfio-pci.conf
  update-initramfs -u
  update-grub
  install -d /usr/local/lib/fcvm-fleet /var/lib/fcvm-fleet /etc/fcvm-fleet
  install -m 0755 "$HERE/bind-vfio.sh" "$HERE/store.sh" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE/fcvm-fleet-vfio.service" "$HERE/fcvm-fleet-store.service" "$HERE/fcvm-fleet-agent.service" /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable fcvm-fleet-vfio.service fcvm-fleet-store.service
  ;;
build)
  loginctl enable-linger "$U"
  for _ in $(seq 1 30); do [ -S "/run/user/$(id -u "$U")/bus" ] && break; sleep 1; done
  /usr/local/lib/fcvm-fleet/bind-vfio.sh
  /usr/local/lib/fcvm-fleet/store.sh   # no archive yet: mounts the empty NVMe store
  # VFIO maps all guest memory for DMA, which counts against the caller's memlock limit;
  # children inherit it through sudo.
  prlimit --pid $$ --memlock=unlimited:unlimited
  [ -x "$UH/.cargo/bin/cargo" ] || as_user 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  [ -d "$SRC/.git" ] || as_user "git clone -q https://github.com/ejc3/fcvm.git $SRC"
  as_user "cd $SRC && git fetch -q origin && git checkout -q --detach $FCVM_COMMIT && git log --oneline -1"
  as_user "cd $SRC && make build"
  as_user "cd $SRC && make setup-gpu"
  as_user "cd $SRC && make -C fleet/renderer image TAG=fleet"
  install -m 0755 "$(as_user "cd $SRC && readlink -f target/release/fcvm")" /usr/local/bin/fcvm
  # One GPU VM to first frame, as fleet/tools/gpurun.sh warms a host: exports the renderer
  # image into the store and leaves the pre-start snapshot that later starts restore from.
  G0=$(ls /sys/bus/pci/drivers/vfio-pci | grep -E '^[0-9a-f]{4}:' | head -1)
  install -o "$U" -g "$U" -m 0644 /dev/null "$L/warm.log"
  as_user "bash $HERE/warm.sh $G0 $L/warm.log"
  # The tile AMI loads this export; build-ami.sh uploads it with a fresh presigned URL.
  as_user "podman save --format oci-archive localhost/fcvm-renderer:fleet -o /mnt/fcvm-btrfs/renderer.oci.tar"
  zstd -q -T0 -f --rm /mnt/fcvm-btrfs/renderer.oci.tar -o /mnt/fcvm-btrfs/renderer.oci.tar.zst
  sha256sum /mnt/fcvm-btrfs/renderer.oci.tar.zst | tee "$S/renderer.sha256"
  as_user "podman image inspect localhost/fcvm-renderer:fleet --format '{{.Id}}'" | tee "$S/renderer.id"
  ;;
bake)
  pkill -TERM -x fcvm || true; sleep 2
  install -m 0755 "$HERE/warm.sh" /usr/local/lib/fcvm-fleet/
  # What a host needs to run GPU VMs without building: everything in the store except
  # per-VM and per-run state, build caches and downloads that only setup reads.
  cd /mnt/fcvm-btrfs
  du -sh --apparent-size ./* 2>/dev/null | sort -h | tee "$S/store-du.txt"
  tar -c --sparse --numeric-owner \
      --exclude=./vm-disks --exclude=./state --exclude=./cache --exclude='./cargo-target*' \
      --exclude=./renderer.oci.tar --exclude=./renderer.oci.tar.zst --exclude='./*.lock' \
      . | zstd -q -T0 -6 -o /var/lib/fcvm-fleet/store.tar.zst -f
  ls -la /var/lib/fcvm-fleet/store.tar.zst
  # The user's rootless podman storage keeps the renderer image (the store holds its export).
  rm -rf "$UH/.cargo/registry" "$UH/.cargo/git" "$UH/.rustup/toolchains"/*/share/doc
  rm -f "$SRC/target"
  $APT clean
  cloud-init clean --logs --seed
  truncate -s 0 /etc/machine-id
  rm -f /var/lib/fleet-ami/env
  sync
  ;;
*) echo "unknown phase $PHASE"; exit 2 ;;
esac
echo "== $PHASE done $(date -u +%FT%TZ)"
touch "$S/$PHASE.done"
