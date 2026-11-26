#!/bin/bash
# Build pages mode's host image (docs/fleet/ARCHITECTURE.md, "Pages mode", "The renders host
# image") on an m8gd.metal-24xl launched from Ubuntu 24.04 arm64. The image factory
# (pipeline.py, track `renders`) drives these phases through SSM. Run as root:
#
#   build.sh prep    packages, host settings, the boot units (the 2 MiB pool, the store, the
#                    agent), and fcvm's host kernel at $FCVM_COMMIT as GRUB's default with the
#                    reference's boot line; the caller reboots the instance afterwards
#   build.sh build   on that kernel only: fcvm at $FCVM_COMMIT (make build, make setup-default),
#                    the bench image, Kitesurf at renders.conf's RENDERS_KITESURF_REV (the
#                    recorded campaign's commit) and its production bundle, the agent's environment, and one run of the agent check, which
#                    makes the module's snapshot
#   build.sh bake    packs the store into /var/lib/fcvm-fleet/store/ (host/store-pack.py), checks
#                    the host and cleans up for imaging
#
# The store is the GPU host's (host/store.sh and fcvm-fleet-store.service): the instance-store
# NVMe as btrfs at /mnt/fcvm-btrfs, restored at every boot from the asset bucket or the image's
# own copy. Every phase logs to /var/log/fleet-ami/<phase>.log and leaves <phase>.done or
# <phase>.failed in /var/lib/fleet-ami.
set -euo pipefail
PHASE=$1
HERE=$(cd "$(dirname "$0")" && pwd)
U=ubuntu; UH=/home/$U; SRC=$UH/fcvm
S=/var/lib/fleet-ami; L=/var/log/fleet-ami
install -d -m 0700 "$S"
install -d -o root -g "$U" -m 0710 "$L"
[ -f "$S/env" ] && . "$S/env"
# shellcheck source=renders.conf
. "$HERE/renders.conf"
exec > >(tee -a "$L/$PHASE.log") 2>&1
# Kitesurf's build runs as a transient systemd unit of a fixed name (bg_start), beside fcvm's own
# build: stopping it stops its whole cgroup (sudo, pnpm, Bazel's server), and every way out of a
# phase stops it, so a failed phase leaves nothing writing to the store while it is retried.
BG_UNITS="fleet-ami-kitesurf"
bg_stop() { systemctl stop $BG_UNITS 2>/dev/null || true; systemctl reset-failed $BG_UNITS 2>/dev/null || true; }
# bg_start UNIT LOG [systemd-run options] -- COMMAND...: run COMMAND as UNIT, its output in LOG.
bg_start() {
  local unit=$1 log=$2; shift 2
  local opts=()
  while [ "$1" != -- ]; do opts+=("$1"); shift; done; shift
  : > "$log"
  systemd-run --quiet --unit="$unit" --property=RemainAfterExit=yes --property=TimeoutStopSec=30 \
    --property=KillMode=control-group --property=StandardOutput="append:$log" \
    --property=StandardError="append:$log" "${opts[@]}" "$@"
}
# bg_wait UNIT LOG: until UNIT's command has ended; fails, with LOG's tail, unless it exited 0.
bg_wait() {
  while [ "$(systemctl show -p SubState --value "$1")" = running ]; do sleep 5; done
  if [ "$(systemctl show -p Result --value "$1")" != success ] ||
     [ "$(systemctl show -p ExecMainStatus --value "$1")" != 0 ]; then
    tail -n 40 "$2"; echo "$1 failed"; return 1
  fi
}
# shellcheck disable=SC2154 # rc is assigned inside the trap string
trap 'rc=$?; bg_stop; [ $rc = 0 ] || { echo "PHASE $PHASE FAILED (exit $rc)"; touch "$S/$PHASE.failed"; }' EXIT
bg_stop
rm -f "$S/$PHASE.done" "$S/$PHASE.failed"
echo "== $PHASE start $(date -u +%FT%TZ)"
export DEBIAN_FRONTEND=noninteractive
APT="apt-get -o DPkg::Lock::Timeout=600"
# The store as this boot's fcvm-fleet-store.service leaves it (host/build.sh store_settled,
# ejc3/fcvm#1825): wait while the unit restores a base image's store, fail if it failed, then
# store.sh mounts the empty store when the unit did not run.
store_settled() {
  local unit=fcvm-fleet-store.service state
  while state=$(systemctl is-active "$unit" 2>/dev/null); [ "$state" = activating ]; do sleep 5; done
  if [ "$state" = failed ]; then
    systemctl status --no-pager "$unit" | tail -n 20 || true
    echo "this boot's store restore ($unit) failed"; return 1
  fi
  /usr/local/lib/fcvm-fleet/store.sh
}
# The image keeps its packed store on the root as the S3 restore's fallback; a host still writes
# there after the image is made (journals, packages). The bake fails unless ROOT_FREE_MIN_GIB stay
# free (host/build.sh root_has_room).
root_has_room() {
  local avail min=$(( ${ROOT_FREE_MIN_GIB:-8} * 1024 * 1024 * 1024 ))
  avail=$(df --output=avail -B1 / | tail -n 1 | tr -d ' ')
  [[ "$avail" =~ ^[0-9]+$ ]] || { echo "could not read the root's free space: '$avail'"; return 1; }
  [ "$avail" -ge "$min" ] && return 0
  echo "the root has $(( avail / 1024 / 1024 )) MiB free once the image is baked; ${ROOT_FREE_MIN_GIB:-8} GiB must stay free"
  df -h /
  return 1
}
# Rootless podman needs the user's systemd instance and runtime directory (lingering, set in prep).
as_user() {
  local uid; uid=$(id -u "$U")
  sudo -u "$U" -H env PATH="$UH/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    XDG_RUNTIME_DIR="/run/user/$uid" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" bash -c "$1"
}

case "$PHASE" in
prep)
  bash "$HERE/../common/install-ip-options.sh"
  # The image's kernel must not move under the store: no unattended upgrades.
  systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer || true
  $APT update
  $APT install -y build-essential git jq qemu-utils busybox-static pkg-config libssl-dev clang make \
    podman skopeo uidmap slirp4netns fuse3 passt btrfs-progs bc bison flex libelf-dev libseccomp-dev \
    curl zstd xz-utils python3 python3-boto3 rsync sysstat iproute2 util-linux
  # The agent's unit names the user by uid (its runtime directory) and runs as it.
  [ "$(id -u "$U")" = "$RENDERS_UID" ] || { echo "$U's uid is $(id -u "$U"), not $RENDERS_UID (renders.conf)"; exit 1; }
  usermod -aG kvm "$U"
  loginctl enable-linger "$U"
  # The recorded campaign's host settings (AGENTS.md "Fresh Bare-Metal Box Quickstart", steps 1b,
  # 1c and 4): the module's replays listen on ports 80, 443 and 53 of loopback as this user, and
  # its engines and clones open many files and watches.
  cat > /etc/sysctl.d/99-fcvm.conf <<'X'
kernel.unprivileged_userns_clone=1
kernel.apparmor_restrict_unprivileged_userns=0
vm.unprivileged_userfaultfd=1
net.ipv4.ip_unprivileged_port_start=0
net.core.somaxconn=65535
fs.inotify.max_user_instances=65536
fs.inotify.max_user_watches=4194304
X
  # -e: fcvm's host kernel, which a revised image already boots, lacks Ubuntu's apparmor key and
  # Debian's userns_clone key; both only open user namespaces, checked here.
  sysctl -e -p /etc/sysctl.d/99-fcvm.conf
  runuser -u "$U" -- unshare -U -r true
  grep -Eq '^[[:space:]]*user_allow_other[[:space:]]*$' /etc/fuse.conf || echo user_allow_other >> /etc/fuse.conf
  echo 'KERNEL=="userfaultfd", MODE="0666"' > /etc/udev/rules.d/70-fcvm-fleet.rules
  printf '* soft nofile 1048576\n* hard nofile 1048576\n' > /etc/security/limits.d/99-fcvm.conf
  # ENA, the instance's NIC driver, is a module of the arm64 host kernel: its config is the
  # running Ubuntu kernel's, which has it =m, and kernel/host-arm64.conf does not pin it. The image
  # boots Ubuntu's initrdless path, as the recorded campaign's host did. A PCI driver binds a
  # device that is present inside its module load unless its probe is asynchronous: ENA's
  # pci_driver sets no probe type (drivers/net/ethernet/amazon/ena/ena_netdev.c, 7.3.3), and
  # drivers/base/dd.c makes it asynchronous only through driver_async_probe=, ena.async_probe or
  # module.async_probe on the boot line, which host-check.py boot refuses (their keys are outside
  # renders.conf), or an `options ena async_probe` line in modprobe.d, which this recipe does not
  # write. systemd-modules-load.service finishes before sysinit.target, and neither it nor
  # cloud-init-local.service has default dependencies, so loading ENA there and ordering
  # cloud-init-local after it binds the NIC before cloud-init-local looks for one. host-check.py
  # boot checks both files, that the loader ran, and the order systemd resolved; the launch-path
  # proof checks the order the boot took, on every validation boot.
  echo ena > /etc/modules-load.d/fcvm-fleet-nic.conf
  install -d /etc/systemd/system/cloud-init-local.service.d
  printf '[Unit]\nAfter=systemd-modules-load.service\n' > /etc/systemd/system/cloud-init-local.service.d/10-fcvm-fleet-nic.conf
  install -d /usr/local/lib/fcvm-fleet /var/lib/fcvm-fleet /etc/fcvm-fleet
  install -m 0644 "$HERE/renders.conf" /etc/fcvm-fleet/renders.conf
  install -m 0755 "$HERE/../host/store.sh" "$HERE/../host/s3-cat.py" "$HERE/hugepages.sh" "$HERE/host-check.py" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE/../host/fcvm-fleet-store.service" "$HERE/fcvm-fleet-hugepages.service" \
    "$HERE/fcvm-fleet-renders-agent.service" /etc/systemd/system/
  systemctl daemon-reload
  # The pool at the next boot's start, the store after it, and the agent where a host's boot wrote
  # agent.env.
  systemctl enable fcvm-fleet-hugepages.service fcvm-fleet-store.service fcvm-fleet-renders-agent.service
  # Units a host does not use (host/build.sh prep's list).
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
  # fcvm's host kernel ([kernel_profiles.nested.arm64.host_kernel]), installed as GRUB's default
  # with the profile's boot_args, which are renders.conf's RENDERS_BOOT_PARAMS (test_pipeline.py).
  # Built on the instance store, booted by the reboot after prep.
  $APT install -y libdw-dev debhelper kmod cpio patch
  store_settled
  [ -x "$UH/.cargo/bin/cargo" ] || as_user 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  [ -d "$SRC/.git" ] || as_user "git clone -q https://github.com/ejc3/fcvm.git $SRC"
  as_user "cd $SRC && git fetch -q origin && git checkout -q -f --detach $FCVM_COMMIT && git clean -q -fd && git log --oneline -1"
  as_user "cd $SRC && make setup-btrfs"
  as_user "cd $SRC && make install-host-kernel"
  # The release GRUB boots next, which the build phase and every check compare the running kernel to.
  release=$(sed -n 's/^GRUB_DEFAULT="Advanced options for Ubuntu>Ubuntu, with Linux \(.*\)"$/\1/p' /etc/default/grub)
  case "$release" in *-fcvm-*) ;; *) echo "GRUB's default is not fcvm's host kernel: '$release'"; exit 1 ;; esac
  echo "$release" > /etc/fcvm-fleet/renders-kernel
  echo "next boot: $release"
  ;;
build)
  # prep installed fcvm's host kernel with the reference's boot line; on anything else the build
  # stops here, before it makes the snapshot the image carries.
  python3 "$HERE/host-check.py" boot
  loginctl enable-linger "$U"
  for _ in $(seq 1 30); do [ -S "/run/user/$(id -u "$U")/bus" ] && break; sleep 1; done
  store_settled
  # A builder from a base image carries that image's packed store on its root; the boot restore
  # has unpacked it and bake rewrites it.
  if [ -d /var/lib/fcvm-fleet/store ]; then find /var/lib/fcvm-fleet/store -maxdepth 1 -name '*.tar.zst' -delete; fi
  [ -x "$UH/.cargo/bin/cargo" ] || as_user 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  [ -d "$SRC/.git" ] || as_user "git clone -q https://github.com/ejc3/fcvm.git $SRC"
  as_user "cd $SRC && git fetch -q origin && git checkout -q -f --detach $FCVM_COMMIT && git clean -q -fd && git log --oneline -1"
  as_user "cd $SRC && make setup-btrfs"
  # Snapshots restored from a base image's store were made by its fcvm: start the cache empty, so
  # the agent check below makes the module's snapshot with this build's.
  rm -rf /mnt/fcvm-btrfs/snapshots/*
  # Kitesurf (kitesurf_build.sh at RENDERS_KITESURF_REV, then the production bundle) depends on
  # nothing else this phase builds: it runs beside fcvm's build, as the build user, on the store.
  install -d -o "$U" -g "$U" /mnt/fcvm-btrfs/kitesurf
  bg_start fleet-ami-kitesurf "$L/kitesurf.log" -- sudo -u "$U" -H env PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    bash "$HERE/kitesurf-dist.sh" "$SRC" "$RENDERS_KITESURF" "$RENDERS_DIST" "$RENDERS_KITESURF_REV"
  as_user "cd $SRC && make build"
  as_user "cd $SRC && make setup-default"
  # The module's snapshot is made from this image (`fcvm podman prepare` of localhost/chromium-bench-req).
  as_user "cd $SRC && make bench-chromium-request-build"
  # fcvm finds fc-agent (the guest agent, musl) beside its own executable.
  for b in fcvm fc-agent; do install -m 0755 "$(as_user "cd $SRC && readlink -f target/release/$b")" /usr/local/bin/$b; done
  bg_wait fleet-ami-kitesurf "$L/kitesurf.log"
  # What the build made (renders-build.json), then the agent's environment from it and renders.conf.
  python3 "$HERE/host-check.py" record
  python3 "$HERE/host-check.py" agent-env > /etc/fcvm-fleet/renders.env
  cat /etc/fcvm-fleet/renders.env
  # The agent check's running case (make test-fleet-renders-agent): the agent with --renders, a
  # hello, renders on both sides, an overlay and a stop with nothing left. Its module makes the
  # snapshot the image carries, as a host's first module would. The validation runs every case.
  install -d -o "$U" -g "$U" /mnt/fcvm-btrfs/renders
  as_user "cd $SRC && make test-fleet-renders-agent OUT=/mnt/fcvm-btrfs/renders/build-check CASES=running FCVM=/usr/local/bin/fcvm TAG=$RENDERS_SNAPSHOT_TAG KITESURF=$RENDERS_KITESURF DIST=$RENDERS_DIST MAX_FCVM=$RENDERS_CHECK_MAX_FCVM MAX_ENGINES=$RENDERS_CHECK_MAX_ENGINES"
  jq -e '[.rows[].case] == ["hello","renders","overlay","stop"] and .failed == []' /mnt/fcvm-btrfs/renders/build-check.agent-check.json
  python3 "$HERE/host-check.py" host
  ;;
bake)
  bash "$HERE/../common/install-ip-options.sh"
  systemctl stop fcvm-renders.service 2>/dev/null || true
  pkill -TERM -x fcvm || true; sleep 2
  install -m 0644 "$HERE/renders.conf" /etc/fcvm-fleet/renders.conf
  install -m 0755 "$HERE/../host/store.sh" "$HERE/../host/s3-cat.py" "$HERE/hugepages.sh" "$HERE/host-check.py" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE/../host/fcvm-fleet-store.service" "$HERE/fcvm-fleet-hugepages.service" \
    "$HERE/fcvm-fleet-renders-agent.service" /etc/systemd/system/
  systemctl daemon-reload
  [ -s /etc/fcvm-fleet/renders.env ] || { echo "no /etc/fcvm-fleet/renders.env: run the build phase"; exit 1; }
  [ -s /etc/fcvm-fleet/renders-build.json ] || { echo "no /etc/fcvm-fleet/renders-build.json: run the build phase"; exit 1; }
  # Where the store's parts are fetched at boot; they are copied there when the image is published.
  if [ -n "${STORE_S3_PREFIX:-}" ]; then
    printf 'STORE_S3_REGION=%s\nSTORE_S3_BUCKET=%s\nSTORE_S3_PREFIX=%s\n' "$STORE_S3_REGION" "$STORE_S3_BUCKET" "$STORE_S3_PREFIX" > /etc/fcvm-fleet/store.env
  fi
  # The build's agent check records go; the module's snapshot stays.
  rm -rf /mnt/fcvm-btrfs/renders
  cd /mnt/fcvm-btrfs
  du -sh --apparent-size ./* 2>/dev/null | sort -h | tee "$S/store-du.txt"
  du -sh --apparent-size ./kitesurf/* 2>/dev/null | sort -h | tee "$S/kitesurf-du.txt"
  as_user "podman image prune -f >/dev/null"
  rm -rf "$UH/.cache/sccache"
  df -h / | tee "$S/root-before-pack.txt"
  rm -rf /var/lib/fcvm-fleet/store
  # Kitesurf's tarballs, npm and pnpm caches, Bazel's download cache and Bazelisk are the build's
  # alone; the engines run from src (its node_modules hold their own files) and the bundle from
  # Bazel's output root, which stays whole.
  python3 "$HERE/../host/store-pack.py" /mnt/fcvm-btrfs /var/lib/fcvm-fleet/store "${STORE_PARTS:-16}" \
      --exclude=./vm-disks --exclude=./state --exclude=./cache --exclude='./cargo-target*' \
      --exclude='./*.lock' --exclude=./renders \
      --exclude=./kitesurf/downloads --exclude=./kitesurf/npm-cache --exclude=./kitesurf/pnpm-store \
      --exclude=./kitesurf/cache --exclude=./kitesurf/bazelisk
  ls -la /var/lib/fcvm-fleet/store
  for f in /var/lib/fcvm-fleet/store/*.tar.zst; do
    [ "$(stat -c %s "$f")" -lt $((5 * 1024 * 1024 * 1024)) ] || { echo "$f is 5 GiB or more: one PUT cannot upload it"; exit 1; }
  done
  # The image's pieces are all still in place once the build-only state is gone.
  rm -rf "$UH/.cargo" "$UH/.rustup"
  rm -f "$SRC/target"
  python3 "$HERE/host-check.py" host
  journalctl --rotate >/dev/null 2>&1 || true
  journalctl --vacuum-time=1s >/dev/null 2>&1 || true
  $APT clean
  cloud-init clean --logs --seed
  truncate -s 0 /etc/machine-id
  rm -f /var/lib/fleet-ami/env
  find /tmp /var/tmp -mindepth 1 -delete
  sync
  root_has_room
  ;;
*) echo "unknown phase $PHASE"; exit 2 ;;
esac
echo "== $PHASE done $(date -u +%FT%TZ)"
touch "$S/$PHASE.done"
