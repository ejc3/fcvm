#!/bin/bash
# Build the EC2 tile AMI's contents on a g4dn.xlarge launched from Ubuntu 24.04 (driven by
# fleet/ami/build-ami.sh). The tile is EC2 at its best: everything a boot could do is done
# here, and a boot only fetches the TLS certificate and starts the renderer. Run as root:
#
#   build.sh prep    packages, the NVIDIA driver (the gpu profile's 580.196.07 installer,
#                    proprietary modules with the GSP firmware off, as fcvm's guests load
#                    it), nvidia-container-toolkit and its CDI spec, the TLS front, units
#   build.sh image   loads the renderer image from $RENDERER_URL (podman save of the image
#                    the host AMI runs) and checks one start to first frame
#   build.sh bake    cleans up for imaging
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
DRIVER_URL=https://us.download.nvidia.com/tesla/580.196.07/NVIDIA-Linux-x86_64-580.196.07.run
DRIVER_SHA256=0aab0518db53cf73c412dc7f32bbc850ca95e839d8923d0de4bff42c1c9169ce
TOOLKIT_URL=https://github.com/NVIDIA/nvidia-container-toolkit/releases/download/v1.20.1/nvidia-container-toolkit_1.20.1_deb_amd64.tar.gz
TOOLKIT_SHA256=14dc22489851d4f1a6d1bbfdfb51524c8f110091738570cddaddc09778a63ad9
IMG=localhost/fcvm-renderer:fleet

case "$PHASE" in
prep)
  systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer || true
  $APT update
  $APT install -y build-essential "linux-headers-$(uname -r)" pkg-config podman zstd curl jq nginx python3-boto3
  printf 'blacklist nouveau\noptions nouveau modeset=0\n' > /etc/modprobe.d/blacklist-nouveau.conf
  # A headless renderer needs nvidia (CUDA, NVENC) and nvidia_uvm only. nvidia_drm adds
  # /dev/dri nodes owned by the video and render groups, which puts additionalGids in the
  # CDI spec, a field podman 4.9's CDI parser rejects ("unresolvable CDI devices").
  printf 'options nvidia NVreg_EnableGpuFirmware=0\ninstall nvidia_drm /bin/true\ninstall nvidia_modeset /bin/true\n' > /etc/modprobe.d/fcvm-fleet-nvidia.conf
  update-initramfs -u
  rmmod nouveau 2>/dev/null || true
  d=$(mktemp -d)
  curl -fsSL -o "$d/driver.run" "$DRIVER_URL"
  echo "$DRIVER_SHA256  $d/driver.run" | sha256sum -c -
  sh "$d/driver.run" --silent --no-questions --ui=none --kernel-module-type=proprietary --no-x-check --no-nouveau-check
  curl -fsSL -o "$d/toolkit.tgz" "$TOOLKIT_URL"
  echo "$TOOLKIT_SHA256  $d/toolkit.tgz" | sha256sum -c -
  tar -xzf "$d/toolkit.tgz" -C "$d"
  $APT install -y $(find "$d" -name '*.deb' ! -name '*dbg*' | sort)
  rm -rf "$d"
  modprobe nvidia
  cat > /etc/systemd/system/nvidia-persistenced.service <<'X'
[Unit]
Description=NVIDIA Persistence Daemon (keeps the GPU initialised between clients)
DefaultDependencies=no
After=systemd-modules-load.service systemd-udev-trigger.service
Before=fcvm-fleet-renderer.service shutdown.target
Conflicts=shutdown.target

[Service]
Type=forking
ExecStart=/usr/bin/nvidia-persistenced --user root
ExecStopPost=/bin/rm -rf /var/run/nvidia-persistenced

[Install]
WantedBy=multi-user.target
X
  # One CDI spec, made here, the same on every g4dn.xlarge: devices by index, and without
  # the apply-cuda-memory-limits hook, which names the builder's GPU by UUID (another
  # instance's GPU then fails cudaSetDevice) and limits nothing on a whole T4. The
  # toolkit's refresh units, started by udev when the module loads, would regenerate it
  # during boot (24 s, measured); masked.
  systemctl mask nvidia-cdi-refresh.path nvidia-cdi-refresh.service
  rm -f /var/run/cdi/nvidia.yaml
  nvidia-ctk cdi generate --device-name-strategy=index --disable-hook=apply-cuda-memory-limits --output=/etc/cdi/nvidia.yaml
  ! grep -q -E 'additionalGids|GPU-[0-9a-f]{8}' /etc/cdi/nvidia.yaml
  # Nothing on a tile uses podman's auto-update or restart-policy units (8.7 s each at boot).
  systemctl mask podman-auto-update.timer podman-auto-update.service podman-restart.service
  # The driver's modules load in sysinit, so nvidia-persistenced (DefaultDependencies=no)
  # initialises the GPU while cloud-init and the network come up.
  printf 'nvidia\nnvidia_uvm\n' > /etc/modules-load.d/nvidia.conf
  install -d /usr/local/lib/fcvm-fleet /etc/fcvm-fleet/tls
  install -m 0755 "$HERE/fetch-cert.py" "$HERE/first-frame.sh" /usr/local/lib/fcvm-fleet/
  install -m 0644 "$HERE"/*.service /etc/systemd/system/
  install -m 0644 "$HERE/nginx-renderer.conf" /etc/nginx/sites-available/renderer
  rm -f /etc/nginx/sites-enabled/default
  ln -sf /etc/nginx/sites-available/renderer /etc/nginx/sites-enabled/renderer
  install -d /etc/systemd/system/nginx.service.d
  printf '[Unit]\nRequires=fcvm-fleet-tls.service\nAfter=fcvm-fleet-tls.service\n' > /etc/systemd/system/nginx.service.d/fcvm-fleet.conf
  systemctl daemon-reload
  systemctl enable nvidia-persistenced.service fcvm-fleet-tls.service fcvm-fleet-renderer.service fcvm-fleet-first-frame.service nginx.service
  systemctl start nvidia-persistenced.service
  nvidia-smi -L
  ;;
image)
  curl -fsS "$RENDERER_URL" | zstd -d -c > /var/tmp/renderer.oci.tar
  podman load -i /var/tmp/renderer.oci.tar
  rm -f /var/tmp/renderer.oci.tar
  podman tag "$(podman images --format '{{.Id}}' | head -1)" "$IMG" 2>/dev/null || true
  podman image inspect "$IMG" --format '{{.Id}}' | tee "$S/renderer.id"
  # One start, renderer only (TLS needs the certificate, which a boot fetches).
  systemctl start fcvm-fleet-renderer.service
  for _ in $(seq 1 300); do
    curl -fsS -m 2 http://127.0.0.1:8080/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS -m 2 http://127.0.0.1:8080/health | jq -c '{ready, frame, gpu}'
  systemctl stop fcvm-fleet-renderer.service
  ;;
bake)
  $APT clean
  rm -f /etc/fcvm-fleet/tls/*.pem /var/log/fcvm-fleet-first-frame.log /var/run/cdi/nvidia.yaml
  cloud-init clean --logs --seed
  truncate -s 0 /etc/machine-id
  rm -f /var/lib/fleet-ami/env
  sync
  ;;
*) echo "unknown phase $PHASE"; exit 2 ;;
esac
echo "== $PHASE done $(date -u +%FT%TZ)"
touch "$S/$PHASE.done"
