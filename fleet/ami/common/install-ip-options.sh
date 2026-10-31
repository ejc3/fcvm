#!/bin/bash
# Install the interim IPv4 TCP IP-options guard before enabling fleet listeners.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
GUARD="$HERE/cve-2026-98432-mitigation.py"
echo "3676cd1defac9288c6ca650ece15f81f8e32bc8781b9f78a8b3e7e83fa9f5b18  $GUARD" | sha256sum -c --quiet
if ! command -v nft >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get -o DPkg::Lock::Timeout=600 update
  apt-get -o DPkg::Lock::Timeout=600 install -y --no-install-recommends nftables python3
fi
python3 "$GUARD" --apply
