#!/usr/bin/env bash
# Fail-closed preflight for `make install-host-kernel`.
#
# Checks, before anything is compiled, that the packages `make bindeb-pkg`
# needs are installed and that the build directory has room. Prints the build
# directory to use on stdout; every diagnostic goes to stderr. Exit 2 on any
# failure.
#
# The build directory is, in order:
#   1. $FCVM_HOST_KERNEL_BUILD_DIR, used as given;
#   2. $FCVM_HOST_KERNEL_SCRATCH/kernel-build-host (default scratch directory
#      /mnt/fcvm-btrfs), when the default directory is on the same device as /
#      and the scratch directory is on another one;
#   3. $FCVM_HOST_KERNEL_DEFAULT_DIR (default /tmp/kernel-build-host).
#
# $FCVM_HOST_KERNEL_MIN_FREE_GIB (default 40) is the free space required on
# the chosen directory's filesystem. A build on a 29 GB root disk ran out of
# space part way through. When that filesystem is on a loop device (the
# setup-btrfs image, a sparse file on the root disk), the image grows into the
# filesystem holding its backing file as the build writes, so that filesystem
# needs the same room.
set -euo pipefail

# What `make bindeb-pkg` and the generated build script use: the compiler
# toolchain, the kernel's own build dependencies (bc, bison, flex, libssl,
# libelf, libdw), the deb packaging tools (debhelper, dpkg-dev via
# build-essential), and tools the script calls (rsync, kmod, cpio, patch).
PACKAGES=(build-essential bc bison flex libssl-dev libelf-dev libdw-dev debhelper rsync kmod cpio patch)

missing=()
for pkg in "${PACKAGES[@]}"; do
  status=$(dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null || true)
  [[ "$status" == "install ok installed" ]] || missing+=("$pkg")
done
if ((${#missing[@]})); then
  echo "check-host-kernel-build: missing packages: ${missing[*]}" >&2
  echo "  install them with: sudo apt-get install -y ${missing[*]}" >&2
  exit 2
fi

min_free_gib=${FCVM_HOST_KERNEL_MIN_FREE_GIB:-40}
default_dir=${FCVM_HOST_KERNEL_DEFAULT_DIR:-/tmp/kernel-build-host}
scratch=${FCVM_HOST_KERNEL_SCRATCH:-/mnt/fcvm-btrfs}

# The deepest directory of $1 that exists: the build directory is created later.
existing_ancestor() {
  local p=$1
  while [[ ! -d "$p" && "$p" != / && "$p" != . ]]; do p=$(dirname "$p"); done
  printf '%s\n' "$p"
}
device_of() { stat -c %d "$(existing_ancestor "$1")"; }

if [[ -n "${FCVM_HOST_KERNEL_BUILD_DIR:-}" ]]; then
  build_dir=$FCVM_HOST_KERNEL_BUILD_DIR
elif [[ -d "$scratch" && -w "$scratch" ]] \
  && [[ "$(device_of "$default_dir")" == "$(device_of /)" ]] \
  && [[ "$(device_of "$scratch")" != "$(device_of /)" ]]; then
  build_dir=$scratch/kernel-build-host
else
  build_dir=$default_dir
fi

# Free GiB on the filesystem holding $1 (an existing directory), or empty.
free_gib_of() { df --output=avail -B1G "$1" | tail -n 1 | tr -d ' '; }

anc=$(existing_ancestor "$build_dir")
free_gib=$(free_gib_of "$anc")
if ! [[ "$free_gib" =~ ^[0-9]+$ ]] || ((free_gib < min_free_gib)); then
  echo "check-host-kernel-build: ${build_dir} has ${free_gib:-unknown} GiB free on its filesystem (${anc}); the host kernel build needs ${min_free_gib} GiB." >&2
  echo "  Set FCVM_HOST_KERNEL_BUILD_DIR to a directory on a larger disk (for example one on the scratch disk), or mount one at ${scratch}." >&2
  exit 2
fi

# Follow loop devices down to real storage: each backing file's filesystem must
# have the room too. Bounded, so a loop chain cannot spin.
dir=$anc
for _ in 1 2 3 4 5 6 7 8; do
  # A lookup that fails says nothing about whether the device is a loop, so it
  # fails the check rather than passing it as "not loop-backed".
  if ! source_dev=$(findmnt --noheadings --nofsroot --output SOURCE --target "$dir" 2>/dev/null) \
    || ! source_dev=$(printf '%s\n' "$source_dev" | head -n 1) || [[ -z "$source_dev" ]]; then
    echo "check-host-kernel-build: could not tell which device holds ${dir} (findmnt --target ${dir}), so whether ${build_dir} is loop-backed, and how much room the disk behind it has, is unknown." >&2
    echo "  Install util-linux's findmnt, or set FCVM_HOST_KERNEL_BUILD_DIR to a directory findmnt can resolve." >&2
    exit 2
  fi
  [[ "$source_dev" == /dev/loop* ]] || break
  backing=$(losetup --list --noheadings --output BACK-FILE "$source_dev" 2>/dev/null | head -n 1 || true)
  backing=${backing%% (deleted)}
  backing=$(printf '%s' "$backing" | sed 's/[[:space:]]*$//')
  if [[ -z "$backing" ]]; then
    echo "check-host-kernel-build: ${build_dir} is on loop device ${source_dev}, and its backing file could not be read (losetup --list -O BACK-FILE ${source_dev}), so the free space of the disk it grows into is unknown." >&2
    echo "  Set FCVM_HOST_KERNEL_BUILD_DIR to a directory on a disk that is not loop-backed." >&2
    exit 2
  fi
  dir=$(existing_ancestor "$(dirname "$backing")")
  free_gib=$(free_gib_of "$dir")
  if ! [[ "$free_gib" =~ ^[0-9]+$ ]] || ((free_gib < min_free_gib)); then
    echo "check-host-kernel-build: ${build_dir} is on loop device ${source_dev}, backed by ${backing}, whose filesystem (${dir}) has ${free_gib:-unknown} GiB free; the image grows into it as the build writes, and the host kernel build needs ${min_free_gib} GiB." >&2
    echo "  Free space there, or set FCVM_HOST_KERNEL_BUILD_DIR to a directory on a disk that is not loop-backed." >&2
    exit 2
  fi
done

printf '%s\n' "$build_dir"
