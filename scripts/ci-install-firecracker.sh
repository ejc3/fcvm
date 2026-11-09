#!/bin/bash
# Install upstream Firecracker and its jailer into /usr/local/bin for the CI jobs, from the
# release archive pinned below by version and checksum. The archive comes through
# cached-download.sh, so a runner whose cache holds it makes no request to GitHub.
set -euo pipefail
FC_VERSION="1.14.0"
ARCH=$(uname -m)
# SHA256 pins the release archive; FC_SHA256 and JAILER_SHA256 pin the two binaries in it.
case "$ARCH" in
  aarch64)
    SHA256=aa942421588108dced1de57639c4f6ddd063c3063c2f0bba9cd01be1ca8ba3bf
    FC_SHA256=275c6d7a2568fb77dc6873a87ac3605313fb506907dccf1b804475bf0a187187
    JAILER_SHA256=d66828982d2c0a43c6f4f432b41b2533b947142f44218d823889f478db031a86 ;;
  x86_64)
    SHA256=b0c60364661f396b92e350cb9d03e2e0c2140c4c24b1e48830eb7f79eb73e153
    FC_SHA256=34237ad1a6fcec150a85786488a64acc3b8f419057136113a9858c8bea68bf88
    JAILER_SHA256=a8683b88775f4d95802fb6f4db8ee5fba22aca31e0e9cfa99560608c560cdfef ;;
  *) echo "ci-install-firecracker: no pinned archive for $ARCH" >&2; exit 2 ;;
esac
# A fresh runner is a new instance, so the download cache on its instance-store btrfs is
# empty unless the host is being reused. The runner image (scripts/build-ami.sh) installs the
# same release, and a reused host keeps the previous job's install, so binaries that already
# match the pins need nothing, and no request is made.
BIN_DIR=${FC_BIN_DIR:-/usr/local/bin}
if echo "$FC_SHA256  $BIN_DIR/firecracker" | sha256sum -c --status - 2>/dev/null &&
  echo "$JAILER_SHA256  $BIN_DIR/jailer" | sha256sum -c --status - 2>/dev/null; then
  echo "ci-install-firecracker: $BIN_DIR has Firecracker v$FC_VERSION ($ARCH) already, no download"
  exit 0
fi
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
"$here/cached-download.sh" \
  "https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz" \
  "$SHA256" "$work/firecracker.tgz"
tar -xzf "$work/firecracker.tgz" -C "$work" --strip-components=1 \
  "release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH}" \
  "release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH}"
echo "$FC_SHA256  $work/firecracker-v${FC_VERSION}-${ARCH}" | sha256sum -c --quiet -
echo "$JAILER_SHA256  $work/jailer-v${FC_VERSION}-${ARCH}" | sha256sum -c --quiet -
sudo install -m 755 "$work/firecracker-v${FC_VERSION}-${ARCH}" "$BIN_DIR/firecracker"
sudo install -m 755 "$work/jailer-v${FC_VERSION}-${ARCH}" "$BIN_DIR/jailer"
