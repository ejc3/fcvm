#!/usr/bin/env bash
set -euo pipefail

echo "[preflight] checking /dev/kvm ..."
if [[ ! -e /dev/kvm ]]; then
  echo "ERROR: /dev/kvm not found. Enable KVM (and nested virtualization if running inside a VM)." >&2
  exit 10
fi

echo "[preflight] checking group membership ..."
if id -nG "$USER" | tr ' ' '\n' | grep -q '^kvm$'; then
  echo "OK: user is in 'kvm' group (rootless use of /dev/kvm possible)."
else
  echo "WARN: user not in 'kvm' group. Rootless may fail. You can run: sudo usermod -aG kvm $USER && newgrp kvm"
fi

echo "[preflight] checking slirp4netns (for rootless) ..."
if command -v slirp4netns >/dev/null 2>&1; then
  echo "OK: slirp4netns present."
else
  echo "WARN: slirp4netns not found. Rootless networking will be unavailable."
fi

echo "[preflight] checking nftables/iproute2 (for privileged) ..."
if command -v nft >/dev/null 2>&1 && command -v ip >/dev/null 2>&1; then
  echo "OK: nftables + iproute2 present."
else
  echo "WARN: nftables/iproute2 missing. Privileged networking (bridge + DNAT) will be unavailable."
fi

echo "[preflight] done."
