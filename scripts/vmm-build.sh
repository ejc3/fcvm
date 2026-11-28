#!/bin/bash
# vmm.yml's build step: build fcvm and every pinned VMM from source, and write the binaries and
# their manifest.json to /out/builds (fcvm setup --vmm-artifacts).
# usage: vmm-build.sh host|container <arch>
#   host       on the runner, with Ubuntu 24.04's glibc (the self-hosted Host and Host-Root lanes')
#   container  inside the test image built from Containerfile (the Container lanes' libc)
set -euo pipefail
libc=$1 arch=$2
# The store goes under /out: the runner has no btrfs, and a build's name does not depend on
# where the store is.
sudo mkdir -p /out && sudo chown "$(id -u):$(id -g)" /out
sed -e 's#^assets_dir = .*#assets_dir = "/out/store"#' -e 's#^data_dir = .*#data_dir = "/out/data"#' \
  rootfs-config.toml > /out/vmm-config.toml
grep -q '^assets_dir = "/out/store"' /out/vmm-config.toml || { echo "rootfs-config.toml has no assets_dir line to point at /out" >&2; exit 1; }
build='make build && ./target/release/fcvm setup --vmm-artifacts /out/builds --config /out/vmm-config.toml'
case "$libc" in
  host)
    sudo apt-get update -q
    sudo apt-get install -y -q build-essential clang libclang-dev pkg-config libssl-dev libfuse3-dev musl-tools gperf
    bash -c "$build"
    ;;
  container)
    docker build -t fcvm-vmm -f Containerfile --build-arg "ARCH=$arch" .
    docker run --rm -v "$PWD:/workspace/fcvm" -v /out:/out -w /workspace/fcvm fcvm-vmm bash -c "$build"
    ;;
  *) echo "usage: $0 host|container <arch>" >&2; exit 2 ;;
esac
test -s /out/builds/manifest.json
