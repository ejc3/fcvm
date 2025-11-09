#!/usr/bin/env bash
# Optional helper to create nftables table and MASQ rule (privileged mode).
set -euo pipefail

UPLINK="${1:-eth0}"

sudo nft list tables | grep -q '^table inet fcvm$' || sudo nft add table inet fcvm

for CH in prerouting output postrouting; do
  if ! sudo nft list chain inet fcvm "${CH}" >/dev/null 2>&1; then
    case "${CH}" in
      prerouting) sudo nft add chain inet fcvm prerouting '{ type nat hook prerouting priority -100; }' ;;
      output) sudo nft add chain inet fcvm output '{ type nat hook output priority -100; }' ;;
      postrouting) sudo nft add chain inet fcvm postrouting '{ type nat hook postrouting priority 100; }' ;;
    esac
  fi
done

sudo nft add rule inet fcvm postrouting oifname "${UPLINK}" masquerade
echo "[nft] table fcvm ready; MASQ via ${UPLINK} configured."
