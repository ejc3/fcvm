#!/bin/bash
# Build the fcvm fleet GPU host AMI's contents on a g4dn.metal launched from Ubuntu 24.04
# (fleet/ami/build-ami.sh launches it and drives these phases through SSM). Run as root:
#
#   build.sh prep    packages, host settings, IOMMU on the kernel command line, boot units;
#                    the caller reboots the instance afterwards
#   build.sh build   fcvm at $FCVM_COMMIT (make build, make setup-gpu), the renderer image,
#                    one GPU VM to first frame (warms the image export and snapshot cache),
#                    and the renderer image's export at /mnt/fcvm-btrfs/renderer.oci.tar.zst;
#                    then the phones: the phone profile, the emulator and system image built
#                    from source, Robot Park, the phone image, and the warm snapshot of a
#                    running phone every phone of a host is a clone of (warm-phone.sh)
#   build.sh bake    packs the store into /var/lib/fcvm-fleet/store/ (store-pack.py) and cleans up
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
# shellcheck disable=SC2154 # rc is assigned inside the trap string
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
    curl zstd ffmpeg python3-pil python3-boto3 strace openjdk-17-jdk-headless
  # AGENTS.md "Fresh Bare-Metal Box Quickstart", steps 1, 1b, 1c and 4, made persistent.
  usermod -aG kvm "$U"
  loginctl enable-linger "$U"
  cat > /etc/sysctl.d/99-fcvm.conf <<'X'
kernel.unprivileged_userns_clone=1
kernel.apparmor_restrict_unprivileged_userns=0
vm.unprivileged_userfaultfd=1
# Routed VMs: the host forwards every packet between a VM's veth and the uplink, and the
# kernel forwards IPv6 only with this set. systemd-networkd handles the uplink's router
# advertisements itself (the kernel's accept_ra is 0 there), so its default route stays.
net.ipv6.conf.all.forwarding=1
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
  # Phones run an Android Emulator on KVM inside their VMs: nested virtualization on (the
  # kernel's default since 4.20, set here so a host never depends on it).
  echo 'options kvm_intel nested=1' > /etc/modprobe.d/fcvm-fleet-nested.conf
  echo vfio-pci > /etc/modules-load.d/vfio-pci.conf
  update-initramfs -u
  update-grub
  install -d /usr/local/lib/fcvm-fleet /var/lib/fcvm-fleet /etc/fcvm-fleet
  install -m 0755 "$HERE/bind-vfio.sh" "$HERE/store.sh" "$HERE/s3-cat.py" "$HERE/prefetch.sh" "$HERE/warm-root.sh" "$HERE/warm-phone.sh" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE/fcvm-fleet-vfio.service" "$HERE/fcvm-fleet-store.service" "$HERE/fcvm-fleet-agent.service" \
    "$HERE/fcvm-fleet-prefetch.service" /etc/systemd/system/
  systemctl daemon-reload
  # The agent starts once the store is restored, on a host whose boot wrote agent.env. The
  # prefetch reads the agent's launch files from the root volume first (warm-root.sh).
  systemctl enable fcvm-fleet-vfio.service fcvm-fleet-store.service fcvm-fleet-agent.service fcvm-fleet-prefetch.service
  # Units a GPU host does not use. At the first boot each reads its files from the lazily
  # loaded root volume while the store is restored (podman's own units, apport, fwupd,
  # ModemManager, udisks2 and the rest started within 1 s of the store on a cold boot).
  # podman runs inside the guests; the host only exports and loads images.
  unused=(podman.service podman.socket podman-auto-update.timer podman-auto-update.service \
    podman-restart.service podman-clean-transient.service udisks2.service ModemManager.service \
    apport.service apport-autoreport.path apport-autoreport.timer apport-forward.socket fwupd-refresh.timer \
    multipathd.service multipathd.socket open-iscsi.service iscsid.socket lxd-installer.socket \
    open-vm-tools.service vgauth.service pollinate.service ubuntu-advantage.service ua-reboot-cmds.service \
    ua-timer.timer motd-news.timer update-notifier-download.timer update-notifier-motd.timer man-db.timer \
    hibinit-agent.service lvm2-monitor.service dm-event.socket lvm2-lvmpolld.socket mdcheck_continue.timer \
    mdcheck_start.timer mdmonitor-oneshot.timer snapd.autoimport.service e2scrub_all.timer e2scrub_reap.service \
    dpkg-db-backup.timer)
  mapfile -t unused < <(systemctl list-unit-files --no-legend --plain "${unused[@]}" | awk '{print $1}')
  [ "${#unused[@]}" = 0 ] || systemctl disable "${unused[@]}"
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
  # The tree must be exactly $FCVM_COMMIT. A builder from a BASE_AMI keeps the previous
  # build's checkout, whose untracked files can block the checkout or be built: force the
  # checkout over them and remove untracked files (ignored ones, the cargo target, stay).
  as_user "cd $SRC && git fetch -q origin && git checkout -q -f --detach $FCVM_COMMIT && git clean -q -fd && git log --oneline -1"
  as_user "cd $SRC && make build"
  as_user "cd $SRC && make setup-gpu"
  as_user "cd $SRC && make -C fleet/renderer image TAG=fleet"
  # fcvm finds fc-agent (the guest agent, musl) beside its own executable.
  for b in fcvm fc-agent; do install -m 0755 "$(as_user "cd $SRC && readlink -f target/release/$b")" /usr/local/bin/$b; done
  # One GPU VM to first frame, as fleet/tools/gpurun.sh warms a host: exports the renderer
  # image into the store and leaves the pre-start snapshot that later starts restore from.
  G0=
  for d in /sys/bus/pci/drivers/vfio-pci/[0-9a-f][0-9a-f][0-9a-f][0-9a-f]:*; do G0=${d##*/}; break; done
  install -o "$U" -g "$U" -m 0644 /dev/null "$L/warm.log"
  # Snapshots restored from a BASE_AMI's store are keyed by its fcvm and boot arguments, so a
  # revision would carry them unused and grow the archive: start the snapshot cache empty.
  rm -rf /mnt/fcvm-btrfs/snapshots/*
  as_user "bash $HERE/warm.sh $G0 $L/warm.log"
  # The tile AMI loads this export; build-ami.sh uploads it with a fresh presigned URL.
  as_user "podman save --format oci-archive localhost/fcvm-renderer:fleet -o /mnt/fcvm-btrfs/renderer.oci.tar"
  # The agent runs as root (routed VMs): root's podman store needs the image, and root's fcvm
  # the same config as the build user's.
  podman load -q -i /mnt/fcvm-btrfs/renderer.oci.tar
  install -D -m 0644 "$UH/.config/fcvm/rootfs-config.toml" /root/.config/fcvm/rootfs-config.toml
  # One GPU VM as root, launched as the agent launches a slot: the pre-start snapshot the
  # agent's restores hit, and the list of root volume files the prefetch reads at boot.
  bash "$HERE/warm-root.sh" "$G0" /etc/fcvm-fleet/prefetch.list
  # Phones (docs/fleet/ARCHITECTURE.md, "Phones mode"): the phone profile, the emulator and the
  # system image built from source, and Robot Park. Robot Park builds against the SDK's API 35
  # platform (scripts/phone/sdk.sh): the SDK is used here to build the app and is left out of
  # the store (bake), so nothing of Google's SDK is on a host.
  as_user "cd $SRC && make setup-phone && make phone-emulator && ./scripts/phone/aosp.sh"
  as_user "cd $SRC && ACCEPT_ANDROID_SDK_LICENSE=1 ./scripts/phone/sdk.sh && ./scripts/phone/app.sh"
  as_user "cd $SRC && make -C fleet/phone image TAG=fleet"
  as_user "podman save --format oci-archive localhost/fcvm-phone:fleet -o /mnt/fcvm-btrfs/phone.oci.tar"
  podman load -q -i /mnt/fcvm-btrfs/phone.oci.tar
  rm -f /mnt/fcvm-btrfs/phone.oci.tar
  G1=
  for d in /sys/bus/pci/drivers/vfio-pci/[0-9a-f][0-9a-f][0-9a-f][0-9a-f]:*; do [ "${d##*/}" = "$G0" ] || { G1=${d##*/}; break; }; done
  bash "$HERE/warm-phone.sh" "$G0" "$G1" "$(as_user "cd $SRC && ./scripts/phone/aosp.sh --root-path")" \
    "$(as_user "cd $SRC && ./scripts/phone/emulator.sh --path")" "$(as_user "cd $SRC && ./scripts/phone/app.sh --apk-path")" \
    /etc/fcvm-fleet/phone.env
  zstd -q -T0 -f --rm /mnt/fcvm-btrfs/renderer.oci.tar -o /mnt/fcvm-btrfs/renderer.oci.tar.zst
  sha256sum /mnt/fcvm-btrfs/renderer.oci.tar.zst | tee "$S/renderer.sha256"
  as_user "podman image inspect localhost/fcvm-renderer:fleet --format '{{.Id}}'" | tee "$S/renderer.id"
  ;;
bake)
  pkill -TERM -x fcvm || true; sleep 2
  install -m 0755 "$HERE/warm.sh" "$HERE/store.sh" "$HERE/s3-cat.py" "$HERE/prefetch.sh" "$HERE/warm-root.sh" "$HERE/warm-phone.sh" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE/fcvm-fleet-prefetch.service" "$HERE/fcvm-fleet-agent.service" /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable fcvm-fleet-prefetch.service
  [ -s /etc/fcvm-fleet/prefetch.list ] || { echo "no /etc/fcvm-fleet/prefetch.list: run the build phase (warm-root.sh)"; exit 1; }
  [ -s /etc/fcvm-fleet/phone.env ] || { echo "no /etc/fcvm-fleet/phone.env: run the build phase (warm-phone.sh)"; exit 1; }
  # Where the store's parts are fetched at boot (<prefix><part>.tar.zst); they are uploaded
  # there after bake (fleet/ami/README.md).
  if [ -n "${STORE_S3_PREFIX:-}" ]; then
    printf 'STORE_S3_REGION=%s\nSTORE_S3_BUCKET=%s\nSTORE_S3_PREFIX=%s\n' "$STORE_S3_REGION" "$STORE_S3_BUCKET" "$STORE_S3_PREFIX" > /etc/fcvm-fleet/store.env
  fi
  # What a host needs to run GPU VMs without building: everything in the store except
  # per-VM and per-run state, build caches and downloads that only setup reads.
  cd /mnt/fcvm-btrfs
  du -sh --apparent-size ./* 2>/dev/null | sort -h | tee "$S/store-du.txt"
  # STORE_PARTS parts, extracted at once at boot (store.sh). store-pack.py writes them with
  # --format=posix, which keeps modification times to the nanosecond. fcvm keys the snapshot
  # cache by each driver and overlay image's SHA-256 and caches that digest in
  # version-cache/ under the image's path, mtime and length, so with whole-second mtimes a
  # host would hash the images again on its first GPU VM (#1219).
  rm -rf /var/lib/fcvm-fleet/store
  python3 "$HERE/store-pack.py" /mnt/fcvm-btrfs /var/lib/fcvm-fleet/store "${STORE_PARTS:-16}" \
      --exclude=./vm-disks --exclude=./state --exclude=./cache --exclude='./cargo-target*' \
      --exclude=./renderer.oci.tar --exclude=./renderer.oci.tar.zst --exclude='./*.lock' \
      --exclude=./android-sdk --exclude='./android-emulator/*/src' --exclude='./android-aosp/*/.downloads'
  ls -la /var/lib/fcvm-fleet/store
  # Each part goes to S3 in one presigned PUT, which S3 refuses above 5 GiB; a host whose
  # S3 restore fails falls back to the AMI's copy, which took 1006 s against 78 s (2026-10-24).
  for f in /var/lib/fcvm-fleet/store/*.tar.zst; do
    [ "$(stat -c %s "$f")" -lt $((5 * 1024 * 1024 * 1024)) ] || { echo "$f is 5 GiB or more: one PUT cannot upload it"; exit 1; }
  done
  # The user's rootless podman storage keeps the renderer image (the store holds its export).
  rm -rf "$UH/.cargo/registry" "$UH/.cargo/git" "$UH/.rustup/toolchains"/*/share/doc
  rm -f "$SRC/target"
  $APT clean
  cloud-init clean --logs --seed
  truncate -s 0 /etc/machine-id
  rm -f /var/lib/fleet-ami/env
  # Every boot empties /tmp (systemd-tmpfiles-setup, `D /tmp`), and the store waits for it
  # (nss-lookup.target after systemd-resolved after tmpfiles-setup), on a root volume whose
  # blocks come from S3 on first read. The 67d383ea image kept the gpu kernel's build tree
  # there (/tmp/kernel-build-gpu-*, 210,696 files): 40.7 s of every boot, against 1.7 s for
  # the same pass over the 7f5fb7e6 image's volume. Empty both here, where it costs nothing.
  find /tmp /var/tmp -mindepth 1 -delete
  sync
  ;;
*) echo "unknown phase $PHASE"; exit 2 ;;
esac
echo "== $PHASE done $(date -u +%FT%TZ)"
touch "$S/$PHASE.done"
