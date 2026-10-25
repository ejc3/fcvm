#!/bin/bash
# Bind every NVIDIA PCI function to vfio-pci and check each one is passable: on vfio-pci,
# with its /dev/vfio group node, in a group holding nothing a host driver owns (VFIO opens a
# group only when every member is unbound or on vfio-pci, pci-stub or pcieport). Writes one
# line per GPU to /run/fcvm-fleet/gpus. Exits non-zero when any GPU is not passable, so the
# unit fails and the agent (which requires it) does not start on a half-bound host.
set -uo pipefail
mkdir -p /run/fcvm-fleet
modprobe vfio-pci
out=/run/fcvm-fleet/gpus; : > "$out.tmp"; bad=0; n=0
for d in /sys/bus/pci/devices/*; do
  [ "$(cat "$d/vendor")" = 0x10de ] || continue
  dev=${d##*/}; n=$((n + 1))
  echo vfio-pci > "$d/driver_override"
  cur=$(readlink "$d/driver" 2>/dev/null); cur=${cur##*/}
  if [ "$cur" != vfio-pci ]; then
    [ -n "$cur" ] && echo "$dev" > "$d/driver/unbind"
    echo "$dev" > /sys/bus/pci/drivers_probe
  fi
done
udevadm settle
for d in /sys/bus/pci/devices/*; do
  [ "$(cat "$d/vendor")" = 0x10de ] || continue
  dev=${d##*/}; drv=$(readlink "$d/driver" 2>/dev/null); grp=$(readlink "$d/iommu_group" 2>/dev/null)
  echo "$dev driver=${drv##*/} iommu_group=${grp##*/}" >> "$out.tmp"
  if [ "${drv##*/}" != vfio-pci ] || [ -z "$grp" ] || [ ! -c "/dev/vfio/${grp##*/}" ]; then bad=1; continue; fi
  for m in "/sys/kernel/iommu_groups/${grp##*/}/devices"/*; do
    md=$(readlink "$m/driver" 2>/dev/null); md=${md##*/}
    case "$md" in ""|vfio-pci|pci-stub|pcieport) ;; *) echo "$dev: group ${grp##*/} also holds ${m##*/} on $md" >> "$out.tmp"; bad=1 ;; esac
  done
done
mv "$out.tmp" "$out"; cat "$out"
# Each GPU's lock file, readable by every user. A VM's shared lease on it keeps
# `fcvm gpu scrub` off a GPU the VM holds, and a rootless run cannot create it here.
# Opened for append, never replaced: a launch may already hold its lease on the file's
# inode, and a new inode under the same name would let a scrub lock it.
make_lock_file() { : >> "$1" && chmod 0644 "$1"; }
install -d -m 0755 /var/lib/fcvm/gpu || bad=1
for d in /sys/bus/pci/devices/*; do
  [ "$(cat "$d/vendor")" = 0x10de ] || continue
  make_lock_file "/var/lib/fcvm/gpu/${d##*/}.lock" || bad=1
done
grep -qw intel_iommu=on /proc/cmdline || { [ "$(uname -m)" != x86_64 ] || { echo "kernel booted without intel_iommu=on"; bad=1; }; }
[ "$n" -gt 0 ] || { echo "no NVIDIA function found"; bad=1; }
echo "fcvm-fleet-vfio: $n NVIDIA functions, passable=$([ $bad = 0 ] && echo yes || echo no)"
exit $bad
