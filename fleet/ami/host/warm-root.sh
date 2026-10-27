#!/bin/bash
# Start one GPU VM on GPU $1 as root, the way the fleet agent starts a slot (routed, its
# renderer published for HTTP and TLS, an address in this instance's IPv6 prefix), wait for
# its first frame, then stop it. Run as root, after root's podman store holds the renderer.
#
# It leaves the two things a host's first restores after a wake need:
# - the pre-start snapshot the agent's launches restore. warm.sh's VM runs rootless, as the
#   build user, with that user's copy of the image, so its snapshot has another key and the
#   agent never restores it: every slot of every woken host cold-booted a VM and took a
#   snapshot of its own first (8 s of the 27 to 40 s first restore, 2026-10-27).
# - $2: every file outside the store, /proc, /sys, /dev, /run and /tmp that the launch opened
#   or ran (strace), which fcvm-fleet-prefetch.service reads at boot. The root volume is
#   restored from the AMI's snapshot and reads each block from S3 the first time: the first
#   launch after a wake spent 34 s in `podman image inspect` (podman and its libraries, 130 MB
#   in 181 files, read at 4 to 9 MB/s).
#
# A slot also maps the agent's certificate directory and names the file and port in its
# environment (`fcvm fleet agent --tls-secret`, src/fleet/agent/tls.rs); the map and the
# environment are in the snapshot key, so the warm-up passes the same, with the fleet's
# certificate written there as the agent writes it. /run is not in the image.
set -uo pipefail
G=$1; LIST=$2
IMG=localhost/fcvm-renderer:fleet
LOG=/var/log/fcvm-fleet-warm-root.log
TLS_DIR=/run/fcvm-fleet/tls
command -v strace >/dev/null || { echo "warm-root: strace is not installed"; exit 1; }
install -d -m 0700 "$TLS_DIR"
python3 - "$TLS_DIR/tls.json" <<'PY' || { echo "warm-root: cannot write the renderer certificate"; exit 1; }
import json, os, sys, boto3
doc = json.loads(boto3.client("secretsmanager", region_name=os.environ.get("FLEET_SECRET_REGION", "us-east-2"))
                 .get_secret_value(SecretId=os.environ.get("FLEET_TLS_SECRET", "fcvm-fleet/tls-wildcard"))["SecretString"])
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump({"cert": doc["cert"], "key": doc["key"]}, f)
PY
T=$(curl -fsS -X PUT -H "X-aws-ec2-metadata-token-ttl-seconds: 60" http://169.254.169.254/latest/api/token)
M=$(curl -fsS -H "X-aws-ec2-metadata-token: $T" http://169.254.169.254/latest/meta-data/mac)
P=$(curl -fsS -H "X-aws-ec2-metadata-token: $T" "http://169.254.169.254/latest/meta-data/network/interfaces/macs/$M/ipv6-prefix" | head -1)
[ -n "$P" ] || { echo "warm-root: this instance has no IPv6 prefix (build-ami.sh launch host gives it one)"; exit 1; }
# The agent's slot address: the prefix with the GPU index plus two in its last group.
A=$(python3 -c 'import ipaddress,sys; print(ipaddress.ip_network(sys.argv[1], strict=False).network_address + 2)' "$P")
t0=$(date +%s%N)
setsid strace -f -qq --seccomp-bpf -e trace=openat,execve -o /tmp/warm-root.strace \
  fcvm podman run --name warm-root --kernel-profile gpu --gpu "$G" --publish 127.0.0.1:20099:8080 \
  --network routed --ipv6-prefix "$P" --ipv6-address "$A" --publish 127.0.0.1:20098:443 \
  --map "$TLS_DIR":/run/fleet-tls:ro --env TLS_CERT_FILE=/run/fleet-tls/tls.json --env TLS_PORT=443 "$IMG" \
  > "$LOG" 2>&1 < /dev/null &
WP=$!
ok=0
for _ in $(seq 1 1800); do
  if curl -fsS -m 2 http://127.0.0.1:20099/health 2>/dev/null | jq -e '.ready == true and .frame > 0' >/dev/null 2>&1; then ok=1; break; fi
  kill -0 $WP 2>/dev/null || break
  sleep 1
done
echo "warm-root: ok=$ok first_frame_ms=$(( ($(date +%s%N) - t0) / 1000000 )) gpu=$G"
pkill -TERM -x fcvm; wait $WP
rm -rf "$TLS_DIR"
[ $ok = 1 ] || { tail -20 "$LOG"; exit 1; }
grep -oE '(openat\([^"]*|execve\()"[^"]+"' /tmp/warm-root.strace | sed -E 's/.*"([^"]+)"$/\1/' |
  grep '^/' | grep -vE '^/(proc|sys|dev|run|tmp|mnt/fcvm-btrfs)/' | sort -u |
  while read -r f; do [ -f "$f" ] && echo "$f"; done > "$LIST"
rm -f /tmp/warm-root.strace
[ -s "$LIST" ] || { echo "warm-root: the launch opened no file on the root volume"; exit 1; }
echo "warm-root: $(wc -l < "$LIST") files to prefetch at boot, $(xargs -d '\n' stat -L -c %s < "$LIST" | awk '{s+=$1} END {printf "%.0f", s/1048576}') MiB"
