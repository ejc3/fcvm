#!/bin/bash
# The viewer-path acceptance of a host image, on a host booted from it (a gpubox, or the image
# factory's validator): every mode's tiles, published as the fleet publishes them, must answer a
# viewer over IPv4 and over IPv6 (docs/fleet/ARCHITECTURE.md, "When a mode counts as working").
#
#   sudo fleet/tools/viewer-acceptance.sh [OUT]
#
# It makes a throwaway CA and a certificate for the launch names of a test zone, starts a
# coordinator on loopback that names every launch in that zone and serves the zone's DNS itself,
# with this host's agent as its agent: the image's renderer, phones, Windows sandboxes, Minecraft
# servers and potato players, the TLS ports published on the host's public IPv4 address from
# fleet/published-ports.json (--tls-publish-base) and each place's own IPv6 address on 443. Then
# fleet/coord/stage-smoke.py checks every tile through the coordinator's names, once per family,
# trusting only the throwaway CA. The viewer is this host, and it reaches the host's public IPv4
# address on its private one (SMOKE_HOST_LOCAL): what leaves the instance crosses its security
# group, which is not the image's to check (the image factory's builder group admits no inbound
# port at all; the staging smoke and ejc3/aws's test of the fleet's range cover that path), and
# IPv6 to the places' own addresses stays on the host. Runs as root (the agent's routed VMs need it), from the fcvm
# checkout the image was built from. OUT (default a new directory under /mnt/fcvm-btrfs) keeps
# the logs and the smoke's lines; the CA's key never leaves it and the directory is removed on a
# pass unless KEEP=1.
#
# Exit status: 0 when both families pass, 1 when a tile fails, 2 when the check cannot run.
set -uo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)
F=${FCVM:-$(command -v fcvm)}
MODES=${MODES:-wall,phones,windows,minecraft,potato}
ZONE=${ZONE:-fleet.test}
SUB=tiles.$ZONE
OUT=${1:-$(mktemp -d /mnt/fcvm-btrfs/viewer-acceptance.XXXXXX)}
mkdir -p "$OUT" && chmod 700 "$OUT"
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$OUT/progress.log" >&2; }
need() { command -v "$1" >/dev/null 2>&1 || { log "BLOCKED: $1 is not installed"; exit 2; }; }
for tool in openssl python3 ffmpeg jq; do need "$tool"; done
[ -x "$F" ] || { log "BLOCKED: no fcvm binary"; exit 2; }
[ "$(id -u)" = 0 ] || { log "BLOCKED: run as root (routed VMs)"; exit 2; }

# A port for a moment, from the kernel.
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }

log "throwaway CA and a certificate for *.$SUB"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
  -subj "/CN=fcvm viewer-path acceptance CA" -keyout "$OUT/ca.key" -out "$OUT/ca.pem" 2>>"$OUT/openssl.log"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$SUB" \
  -keyout "$OUT/leaf.key" -out "$OUT/leaf.csr" 2>>"$OUT/openssl.log"
printf 'subjectAltName=DNS:*.%s\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' "$SUB" > "$OUT/leaf.ext"
openssl x509 -req -in "$OUT/leaf.csr" -CA "$OUT/ca.pem" -CAkey "$OUT/ca.key" -CAcreateserial -days 1 \
  -extfile "$OUT/leaf.ext" -out "$OUT/leaf.pem" 2>>"$OUT/openssl.log"
python3 - "$OUT" <<'PY'
import json, os, secrets, sys
d = sys.argv[1]
doc = {"cert": open(f"{d}/leaf.pem").read(), "key": open(f"{d}/leaf.key").read(),
       "scene_token": secrets.token_hex(32)}
fd = os.open(f"{d}/tls.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
os.write(fd, json.dumps(doc).encode())
os.close(fd)
PY
rm -f "$OUT/ca.key" "$OUT/leaf.csr"

# The host's agent arguments, as the fleet's launch template writes them (ejc3/aws
# fcvm_fleet_host_agent_args), with the throwaway certificate and stdio to a local coordinator.
PHONE_AGENT_ARGS="" WINDOWS_AGENT_ARGS=""
# shellcheck disable=SC1091
[ -r /etc/fcvm-fleet/phone.env ] && . /etc/fcvm-fleet/phone.env
# shellcheck disable=SC1091
[ -r /etc/fcvm-fleet/windows.env ] && . /etc/fcvm-fleet/windows.env
BASE=$(jq -r .gpu_host_ipv4.base "$SRC/fleet/published-ports.json")
agent="$F fleet agent --stdio --host viewer-acceptance --image localhost/fcvm-renderer:fleet --base-port 20100"
agent+=" --publish-ip 0.0.0.0 --tls-file $OUT/tls.json --ipv6-prefix-from-imds"
agent+=" --frame-zone $SUB --tls-publish-base $BASE --public-ipv4-from-imds"
case ",$MODES," in *,phones,*) agent+=" --phones 2 $PHONE_AGENT_ARGS" ;; esac
case ",$MODES," in *,windows,*) agent+=" --windows 4 $WINDOWS_AGENT_ARGS" ;; esac
case ",$MODES," in *,minecraft,*) agent+=" --minecraft 4" ;; esac
case ",$MODES," in *,potato,*) agent+=" --potato-players 4" ;; esac

# The host's public IPv4 address, which every -v4 launch name resolves to, and its private one.
imds() {
  local token
  token=$(curl -sf -m 3 -X PUT -H "X-aws-ec2-metadata-token-ttl-seconds: 60" http://169.254.169.254/latest/api/token) &&
    curl -sf -m 3 -H "X-aws-ec2-metadata-token: $token" "http://169.254.169.254/latest/meta-data/$1"
}
PUBLIC4=$(imds public-ipv4) && PRIVATE4=$(imds local-ipv4) || { log "BLOCKED: no IPv4 addresses from IMDS"; exit 2; }

LIVE=$(free_port); DNS=$(free_port)
log "coordinator on 127.0.0.1:$LIVE, the zone's DNS on 127.0.0.1:$DNS"
RUST_LOG=fcvm::fleet=info setsid "$F" fleet coord --listen "127.0.0.1:$LIVE" --state-file "$OUT/state.json" \
  --dns-zone "$ZONE" --dns-subzone "$SUB" --dns-subzone-ns "ns1.$ZONE" --dns-listen "127.0.0.1:$DNS" \
  --agent-cmd "$agent" > "$OUT/coord.log" 2>&1 < /dev/null &
CP=$!
stop() {
  if kill -0 "$CP" 2>/dev/null; then
    log "stopping the coordinator (its agent stops every VM)"
    kill -TERM "$CP"
    wait "$CP"
  fi
}
trap stop EXIT
# shellcheck source=fleet/tools/lib.sh
. "$SRC/fleet/tools/lib.sh"
wait_listening 127.0.0.1 "$LIVE" 120 "$CP" || { log "BLOCKED: the coordinator did not listen"; exit 2; }

# Both families borrow a Minecraft server from the same client address, and a client waits
# minecraft_claim_cooldown_secs (fleet/core/src/state.rs) after a return to borrow again.
COOLDOWN=$(grep -oE 'minecraft_claim_cooldown_secs: [0-9]+' "$SRC/fleet/core/src/state.rs" | grep -oE '[0-9]+$')
[ -n "$COOLDOWN" ] || { log "BLOCKED: no Minecraft claim cooldown in fleet/core/src/state.rs"; exit 2; }
rc=0
for family in v4 v6; do
  [ "$family" = v4 ] || sleep "$((COOLDOWN + 5))"
  log "viewer path over $family"
  SMOKE_LIVE_URL="ws://127.0.0.1:$LIVE/live" SMOKE_RESOLVERS="127.0.0.1:$DNS" SMOKE_ZONE="$ZONE" \
    SMOKE_HOST_LOCAL="$PUBLIC4=$PRIVATE4" \
    SMOKE_CA_FILE="$OUT/ca.pem" python3 -B "$SRC/fleet/coord/stage-smoke.py" --family "$family" \
    --modes "$MODES" --takeover > "$OUT/smoke-$family.jsonl" 2>>"$OUT/smoke-$family.err"
  status=$?
  log "viewer path over $family: exit $status, $(jq -sc 'map(select(.ok == false)) | length' "$OUT/smoke-$family.jsonl") failing checks"
  [ "$status" = 0 ] || rc=1
done
stop
trap - EXIT
if [ "$rc" = 0 ] && [ "${KEEP:-0}" != 1 ]; then
  rm -rf "$OUT"
fi
exit "$rc"
