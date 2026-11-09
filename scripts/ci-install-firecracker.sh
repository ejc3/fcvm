#!/bin/bash
# Install upstream Firecracker and its jailer into /usr/local/bin for the CI jobs, from the
# release archive pinned below by version and checksum. The archive comes through
# cached-download.sh, so a runner whose cache holds it makes no request to GitHub.
set -euo pipefail
FC_VERSION="1.14.0"
ARCH=$(uname -m)
case "$ARCH" in
  aarch64) SHA256=aa942421588108dced1de57639c4f6ddd063c3063c2f0bba9cd01be1ca8ba3bf ;;
  x86_64) SHA256=b0c60364661f396b92e350cb9d03e2e0c2140c4c24b1e48830eb7f79eb73e153 ;;
  *) echo "ci-install-firecracker: no pinned archive for $ARCH" >&2; exit 2 ;;
esac
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
"$here/cached-download.sh" \
  "https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${ARCH}.tgz" \
  "$SHA256" "$work/firecracker.tgz"
tar -xzf "$work/firecracker.tgz" -C "$work" --strip-components=1 \
  "release-v${FC_VERSION}-${ARCH}/firecracker-v${FC_VERSION}-${ARCH}" \
  "release-v${FC_VERSION}-${ARCH}/jailer-v${FC_VERSION}-${ARCH}"
sudo install -m 755 "$work/firecracker-v${FC_VERSION}-${ARCH}" /usr/local/bin/firecracker
sudo install -m 755 "$work/jailer-v${FC_VERSION}-${ARCH}" /usr/local/bin/jailer
