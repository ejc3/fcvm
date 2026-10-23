#!/bin/bash
# A routed GPU VM, then one restored from its snapshot at another address, must each hold
# the --ipv6-address it was given on eth0 and reach the internet over IPv6.
#   FCVM=./target/release/fcvm scripts/gpu-routed/run.sh <gpu pci address> [image]
# Needs root (routed mode), a GPU on vfio-pci, the gpu profile set up (make setup-gpu),
# and a routable IPv6 prefix: FCVM_IPV6_PREFIX, or the one AWS delegates to the instance.
set -euo pipefail
GPU=${1:?usage: run.sh <gpu pci address> [image]}
IMAGE=${2:-docker.io/library/nginx:alpine}
F=${FCVM:-./target/release/fcvm}
TARGET=${GPU_ROUTED_PING:-2606:4700:4700::1111}
[ "$(id -u)" = 0 ] || { echo "run.sh: routed mode needs root" >&2; exit 2; }
if [ -z "${FCVM_IPV6_PREFIX:-}" ]; then
  t=$(curl -fsS -m 2 -X PUT http://169.254.169.254/latest/api/token -H "X-aws-ec2-metadata-token-ttl-seconds: 60")
  mac=$(curl -fsS -m 2 -H "X-aws-ec2-metadata-token: $t" http://169.254.169.254/latest/meta-data/mac)
  FCVM_IPV6_PREFIX=$(curl -fsS -m 2 -H "X-aws-ec2-metadata-token: $t" \
    "http://169.254.169.254/latest/meta-data/network/interfaces/macs/$mac/ipv6-prefix" | head -1)
fi
addr() { python3 -c 'import ipaddress,sys; print(ipaddress.IPv6Network(sys.argv[1], strict=False).network_address + int(sys.argv[2]))' "$FCVM_IPV6_PREFIX" "$1"; }
LOGS=$(mktemp -d)
fail=0
check() { # name, address suffix
  local name=$1 want; want=$(addr "$2")
  "$F" podman run --name "$name" --kernel-profile gpu --gpu "$GPU" --network routed \
    --ipv6-prefix "$FCVM_IPV6_PREFIX" --ipv6-address "$want" "$IMAGE" > "$LOGS/$name.log" 2>&1 < /dev/null &
  local runner=$! pid="" i
  for i in $(seq 1 300); do
    pid=$("$F" ls --json 2>/dev/null | jq -r --arg n "$name" '.[] | select(.name == $n and .health_status == "healthy") | .pid' | head -1)
    [ -n "$pid" ] && break
    kill -0 "$runner" 2>/dev/null || break
    sleep 1
  done
  if [ -z "$pid" ]; then echo "FAIL $name: never healthy (log $LOGS/$name.log)"; fail=1; kill -TERM "$runner" 2>/dev/null; wait "$runner" || true; return; fi
  local has; has=$("$F" exec --pid "$pid" --vm -- ip -6 -o addr show dev eth0 scope global 2>&1 | awk '{print $4}' | paste -sd' ' || true)
  if [[ " $has " == *" $want/128 "* ]]; then echo "ok   $name: eth0 holds its address"; else echo "FAIL $name: eth0 holds '$has', not $want/128"; fail=1; fi
  if "$F" exec --pid "$pid" --vm -- ping -6 -c 2 -W 3 "$TARGET" > "$LOGS/$name.ping" 2>&1; then
    echo "ok   $name: reaches $TARGET over IPv6"
  else
    echo "FAIL $name: no IPv6 reply from $TARGET: $(tail -1 "$LOGS/$name.ping")"; fail=1
  fi
  kill -TERM "$runner" 2>/dev/null; wait "$runner" || true
}
# The first run creates the pre-start snapshot (or restores one); the second restores it
# at another address.
check "gpu-routed-a-$$" "$((0x20 + RANDOM % 0x60))"
check "gpu-routed-b-$$" "$((0x80 + RANDOM % 0x60))"
[ "$fail" = 0 ] && echo "gpu-routed: PASS" || { echo "gpu-routed: FAIL (logs in $LOGS)"; exit 1; }
