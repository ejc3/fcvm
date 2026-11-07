#!/bin/bash
# Build the Windows sandbox image: Windows Server 2025 Datacenter evaluation (Desktop
# Experience) installed unattended into a raw disk, booting under Cloud Hypervisor's firmware.
# QEMU is the builder only; it never runs a sandbox (docs/fleet/windows-sandboxes.md, "The
# image").
#
#   build-image.sh [OUT_ROOT]      build, or reuse, OUT_ROOT/image-<id> (default
#                                  /mnt/fcvm-btrfs/windows); prints the image directory
#
# Every input is pinned by SHA-256 below and downloaded from its publisher at build time, so
# nothing here stores Microsoft's media. <id> hashes the pins and this directory's files, so a
# change to either builds a new image. The image directory holds:
#
#   disk.raw          the installed system (40 GiB, sparse)
#   CLOUDHV.fd        the firmware it boots with
#   admin-password    the administrator's password, generated here and unused after the build
#   sandbox-password  the standard user sandbox's password, which the bridge logs on with
#                     (both mode 0600, never logged)
#   manifest.json     the inputs, the installation time and the evaluation's expiry
#   setup.log         what the guest's setup scripts printed (no secrets)
#
# Needs: qemu-system-x86_64 with KVM, OVMF, genisoimage, curl, jq, python3, openssl.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT_ROOT=${1:-/mnt/fcvm-btrfs/windows}

ISO_URL='https://go.microsoft.com/fwlink/?linkid=2293312&clcid=0x409&culture=en-us&country=us'
ISO_NAME=26100.1742.240906-0331.ge_release_svc_refresh_SERVER_EVAL_x64FRE_en-us.iso
ISO_SHA256=d0ef4502e350e3c6c53c15b1b3020d38a5ded011bf04998e950720ac8579b23d
VIRTIO_URL='https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-0.1.302-1/virtio-win-0.1.302.iso'
VIRTIO_SHA256=303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d
FW_URL='https://github.com/cloud-hypervisor/edk2/releases/download/ch-97eeb7b09/CLOUDHV.fd'
FW_SHA256=dc2fc8f0e43b96712d9fccc52a3a590769606412b3e1bc911d217addd3bef624
# install.wim index 4: "Windows Server 2025 SERVERDATACENTER", Desktop Experience.
IMAGE_INDEX=4
DISK_SIZE=40G
# The install, both reboots and the setup scripts finished in 713 s on a g4dn.metal with 8
# vCPUs (2026-11-07); three times that is a failed build.
DEADLINE_SECS=${WINDOWS_BUILD_DEADLINE_SECS:-2400}

say() { echo "build-image: $*" >&2; }
die() { say "$*"; exit 1; }
for tool in qemu-system-x86_64 genisoimage curl jq python3 openssl sha256sum; do
  command -v "$tool" >/dev/null || die "'$tool' is missing"
done
OVMF_CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
OVMF_VARS=/usr/share/OVMF/OVMF_VARS_4M.fd
[ -r "$OVMF_CODE" ] && [ -r "$OVMF_VARS" ] || die "OVMF is missing ($OVMF_CODE)"
[ -w /dev/kvm ] || die "/dev/kvm is not writable"

id=$( { echo "$ISO_SHA256 $VIRTIO_SHA256 $FW_SHA256 $IMAGE_INDEX $DISK_SIZE"
        cd "$HERE" && sha256sum autounattend.xml.in SetupComplete.cmd fcvm-setup.ps1 fcvm-agent.ps1 build-image.sh image_tool.py; } \
      | sha256sum | cut -c1-12)
IMG=$OUT_ROOT/image-$id
if [ -s "$IMG/manifest.json" ]; then
  echo "$IMG"
  exit 0
fi

SRC=$OUT_ROOT/src
mkdir -p "$SRC"
fetch() { # url sha256 file
  local url=$1 sum=$2 f=$SRC/$3
  if [ -s "$f" ] && echo "$sum  $f" | sha256sum -c --quiet 2>/dev/null; then return; fi
  say "downloading $3"
  curl -fsSL --retry 3 -o "$f.part.$$" "$url"
  echo "$sum  $f.part.$$" | sha256sum -c --quiet || { rm -f "$f.part.$$"; die "$3: SHA-256 mismatch"; }
  mv "$f.part.$$" "$f"
}
fetch "$ISO_URL" "$ISO_SHA256" "$ISO_NAME"
fetch "$VIRTIO_URL" "$VIRTIO_SHA256" virtio-win.iso
fetch "$FW_URL" "$FW_SHA256" CLOUDHV.fd

WORK=$OUT_ROOT/build-$id.$$
rm -rf "$WORK"
mkdir -p "$WORK/cfg"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

umask 077
printf 'Fc%s!9' "$(openssl rand -hex 12)" > admin-password
printf 'Sb%s!7' "$(openssl rand -hex 12)" > sandbox-password
umask 022
python3 "$HERE/image_tool.py" render "$HERE/autounattend.xml.in" cfg/autounattend.xml "$IMAGE_INDEX" admin-password sandbox-password
cp "$HERE/SetupComplete.cmd" "$HERE/fcvm-setup.ps1" "$HERE/fcvm-agent.ps1" cfg/
genisoimage -quiet -J -r -V FCVMCFG -o config.iso cfg
rm -f cfg/autounattend.xml
truncate -s "$DISK_SIZE" disk.raw
cp "$OVMF_VARS" vars.fd

say "installing (deadline ${DEADLINE_SECS} s)"
t0=$(date +%s)
qemu-system-x86_64 -name fcvm-windows-image -machine q35,accel=kvm -cpu host -smp 8 -m 8G \
  -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
  -drive if=pflash,format=raw,file=vars.fd \
  -drive file="$SRC/$ISO_NAME",media=cdrom,if=none,id=cd0,readonly=on -device ide-cd,drive=cd0,bus=ide.0,bootindex=0 \
  -drive file="$SRC/virtio-win.iso",media=cdrom,if=none,id=cd1,readonly=on -device ide-cd,drive=cd1,bus=ide.1 \
  -drive file=config.iso,media=cdrom,if=none,id=cd2,readonly=on -device ide-cd,drive=cd2,bus=ide.2 \
  -drive file=disk.raw,format=raw,if=none,id=root,cache=unsafe -device virtio-blk-pci,drive=root,disable-legacy=on,bootindex=1 \
  -device virtio-net-pci,netdev=n0,disable-legacy=on -netdev user,id=n0 \
  -vga std -display none -qmp unix:"$WORK/qmp.sock",server,nowait \
  -serial file:"$WORK/com1.log" -serial file:"$WORK/com2.log" &
qpid=$!
trap 'kill -9 $qpid 2>/dev/null || true; rm -rf "$WORK"' EXIT
qmp() { # one QMP command, its JSON as $1
  printf '{"execute":"qmp_capabilities"}\n%s\n' "$1" | timeout 10 python3 -c 'import socket,sys,time; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.stdin.buffer.read()); time.sleep(0.5)' "$WORK/qmp.sock" 2>/dev/null || true
}
# The installer's EFI loader asks "Press any key to boot from CD or DVD" for about 5 s and
# boots the empty disk when nobody does. Press Enter twice a second for the first 12 s: the
# question comes 2 to 4 s after power on, and Setup's first window, where a stray Enter would
# land, takes another 20 s to load. (Rewriting the ISO with its no-prompt loader would drop
# its UDF tree, and install.wim is larger than ISO 9660 holds in one extent.)
for _ in $(seq 1 24); do
  sleep 0.5
  [ -S "$WORK/qmp.sock" ] && qmp '{"execute":"send-key","arguments":{"keys":[{"type":"qcode","data":"ret"}]}}'
done
while kill -0 $qpid 2>/dev/null; do
  if [ $(( $(date +%s) - t0 )) -ge "$DEADLINE_SECS" ]; then
    # The screen at the deadline is the only record of where the installer stopped.
    qmp "{\"execute\":\"screendump\",\"arguments\":{\"filename\":\"$OUT_ROOT/failed-$id.ppm\"}}"
    kill -9 $qpid
    die "the install did not power off within ${DEADLINE_SECS} s; last screen: $OUT_ROOT/failed-$id.ppm"
  fi
  sleep 5
done
wait $qpid || die "QEMU exited with status $?"
say "installed in $(( $(date +%s) - t0 )) s"

# The guest's finish step writes its setup log to COM2 between two markers.
python3 "$HERE/image_tool.py" setuplog com2.log setup.log || die "no setup log from the guest"
grep -q "Product activated successfully" setup.log || { cat setup.log >&2; die "the evaluation was not activated"; }
expiry=$(python3 "$HERE/image_tool.py" expiry setup.log)

tmp=$IMG.tmp.$$
mkdir -p "$tmp"
mv disk.raw admin-password sandbox-password setup.log "$tmp/"
cp "$SRC/CLOUDHV.fd" "$tmp/"
jq -n --arg id "$id" --arg edition "Windows Server 2025 Datacenter Evaluation (Desktop Experience)" \
  --arg build "26100.1742" --arg iso "$ISO_NAME" --arg iso_sha256 "$ISO_SHA256" \
  --arg virtio_sha256 "$VIRTIO_SHA256" --arg firmware_sha256 "$FW_SHA256" \
  --arg installed_at "$(date -u -d @"$t0" +%Y-%m-%dT%H:%M:%SZ)" --arg eval_expires "$expiry" \
  '{id: $id, edition: $edition, build: $build, iso: $iso, iso_sha256: $iso_sha256,
    virtio_sha256: $virtio_sha256, firmware_sha256: $firmware_sha256,
    installed_at: $installed_at, eval_expires: $eval_expires}' > "$tmp/manifest.json"
# Two builds of one id: the first to rename publishes, the other discards its copy.
mv -T "$tmp" "$IMG" 2>/dev/null || { rm -rf "$tmp"; say "another build published $IMG first"; }
say "image $id: installed $(jq -r .installed_at "$IMG/manifest.json"), evaluation expires $expiry"
echo "$IMG"
