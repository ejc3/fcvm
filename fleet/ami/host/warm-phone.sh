#!/bin/bash
# The warm snapshot every phone of a host is a clone of (flow B, docs/fleet/ARCHITECTURE.md
# "Phones mode"). Run as root after build.sh has built the phone's pieces and loaded the
# phone image into root's podman store:
#
#   warm-phone.sh <gpu> <second gpu> <aosp root> <emulator dir> <app apk> <env file>
#
# 1. Launches one phone on <gpu> exactly as the agent launches a phone slot that boots cold
#    (src/fleet/agent/vm.rs VmSpec::for_phone; `the_ami_warm_phone_is_launched_as_a_phone_
#    slot_is` pins the two together): routed at this instance's prefix, the streamer
#    published for HTTP and TLS, the agent's certificate directory mapped, `phone.sh serve`.
# 2. Waits until the phone streams (`phone.verified`: SurfaceFlinger and Robot Park render
#    on the GPU), then takes `fcvm snapshot create --gpu-warm --tag phone-warm`: the phone's
#    GPU hook releases the emulator's GPU and stops the streamer, and retakes both after.
# 3. Restores one clone of it on <second gpu> as the agent does (`fcvm snapshot run`), posts
#    it a scene, and waits for the streamer's first stamped scene frame: the snapshot works
#    before it is baked. Prints the click to that frame.
# 4. Writes <env file>: the agent's phone arguments (PHONE_AGENT_ARGS), which
#    fcvm-fleet-agent.service adds to the host's own (ejc3/aws adds `--phones N`).
set -uo pipefail
G=$1; G2=$2; AOSP=$3; EMU=$4; APK=$5; ENVF=$6
IMG=localhost/fcvm-phone:fleet
TAG=phone-warm
LOG=/var/log/fcvm-fleet-warm-phone.log
TLS_DIR=/run/fcvm-fleet/tls
WORK=$(dirname "$APK")
APKN=$(basename "$APK")
ms() { date +%s%3N; }
health() { curl -fsS -m 2 "http://127.0.0.1:$1/health" 2>/dev/null; }
install -d -m 0700 "$TLS_DIR"
python3 - "$TLS_DIR/tls.json" <<'PY' || { echo "warm-phone: cannot write the certificate"; exit 1; }
import json, os, sys, boto3
doc = json.loads(boto3.client("secretsmanager", region_name=os.environ.get("FLEET_SECRET_REGION", "us-east-2"))
                 .get_secret_value(SecretId=os.environ.get("FLEET_TLS_SECRET", "fcvm-fleet/tls-wildcard"))["SecretString"])
out = {"cert": doc["cert"], "key": doc["key"]}
# The scene token, as the agent writes it (src/fleet/agent/tls.rs): with it the streamer takes
# a scene only with `Authorization: Bearer <token>`. Its header goes to a file of its own, so
# the token is never on a command line.
if doc.get("scene_token"):
    out["scene_token"] = doc["scene_token"]
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(out, f)
fd = os.open(os.path.join(os.path.dirname(sys.argv[1]), "scene-auth"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    if doc.get("scene_token"):
        f.write("Authorization: Bearer %s\n" % doc["scene_token"])
PY
T=$(curl -fsS -X PUT -H "X-aws-ec2-metadata-token-ttl-seconds: 60" http://169.254.169.254/latest/api/token)
M=$(curl -fsS -H "X-aws-ec2-metadata-token: $T" http://169.254.169.254/latest/meta-data/mac)
P=$(curl -fsS -H "X-aws-ec2-metadata-token: $T" "http://169.254.169.254/latest/meta-data/network/interfaces/macs/$M/ipv6-prefix" | head -1)
[ -n "$P" ] || { echo "warm-phone: this instance has no IPv6 prefix"; exit 1; }
addr() { python3 -c 'import ipaddress,sys; print(ipaddress.ip_network(sys.argv[1], strict=False).network_address + int(sys.argv[2]))' "$P" "$1"; }
A=$(addr 2); A2=$(addr 3)
cleanup() { pkill -TERM -x fcvm 2>/dev/null; wait 2>/dev/null; rm -rf "$TLS_DIR"; }
trap cleanup EXIT
: > "$LOG"

# 1. the phone, launched as a phone slot is
t0=$(ms)
RUST_LOG=info setsid \
  fcvm podman run --name warm-phone --kernel-profile phone --gpu "$G" --publish 127.0.0.1:20097:8080 \
  --network routed --ipv6-prefix "$P" --ipv6-address "$A" --publish 127.0.0.1:20096:443 \
  --privileged --cpu 6 --mem 6144 --portable-volumes --map "$AOSP":/android:ro --map "$EMU":/emu:ro \
  --map "$WORK":/work:ro --env PHONE_APK=/work/"$APKN" \
  --map "$TLS_DIR":/run/fleet-tls:ro --env TLS_CERT_FILE=/run/fleet-tls/tls.json --env TLS_PORT=443 \
  "$IMG" /usr/local/bin/phone.sh serve >> "$LOG" 2>&1 < /dev/null &
wp=$!
ok=0
for _ in $(seq 1 900); do
  if health 20097 | jq -e '.phone.verified == true' > /dev/null 2>&1; then ok=1; break; fi
  health 20097 | jq -e '.error' > /dev/null 2>&1 && break
  kill -0 $wp 2>/dev/null || break
  sleep 1
done
echo "warm-phone: cold boot ok=$ok streaming_ms=$(( $(ms) - t0 )) gpu=$G"
[ $ok = 1 ] || { health 20097; tail -30 "$LOG"; exit 1; }

# 2. the warm snapshot of the running phone
pid=$(fcvm ls --json | jq -r '.[] | select(.name == "warm-phone") | .pid' | head -1)
rm -rf "/mnt/fcvm-btrfs/snapshots/$TAG"
t1=$(ms)
fcvm snapshot create --pid "$pid" --tag "$TAG" --gpu-warm >> "$LOG" 2>&1 || { echo "warm-phone: the warm snapshot failed"; tail -30 "$LOG"; exit 1; }
echo "warm-phone: warm snapshot $TAG in $(( $(ms) - t1 )) ms"
kill -TERM $wp; wait $wp

# 3. one clone, as a phone slot restores it
click=$(ms)
RUST_LOG=info setsid fcvm snapshot run --snapshot "$TAG" --name warm-phone-clone --gpu "$G2" \
  --publish 127.0.0.1:20095:8080 --ipv6-prefix "$P" --ipv6-address "$A2" --publish 127.0.0.1:20094:443 \
  --map "$TLS_DIR":/run/fleet-tls:ro >> "$LOG" 2>&1 < /dev/null &
cp=$!
posted=0 ok=0
for _ in $(seq 1 600); do
  h=$(health 20095 || true)
  if [ -n "$h" ] && [ $posted = 0 ]; then
    curl -fsS -m 2 -H @"$TLS_DIR/scene-auth" -H 'Content-Type: application/json' -d "{\"epoch_unix_ms\":$(( $(ms) - 1000 )),\"rect\":{\"x\":0,\"y\":0,\"w\":1,\"h\":1},\"image_aspect\":0.5625}" \
      http://127.0.0.1:20095/scene > /dev/null && posted=1
  fi
  [ "$(jq -r '.phone.scene_frames // 0' <<< "${h:-{\}}" 2>/dev/null)" -gt 0 ] 2>/dev/null && { ok=1; break; }
  kill -0 $cp 2>/dev/null || break
  sleep 0.1
done
echo "warm-phone: clone ok=$ok click_to_first_scene_frame_ms=$(( $(ms) - click )) gpu=$G2"
kill -TERM $cp; wait $cp
[ $ok = 1 ] || { tail -30 "$LOG"; exit 1; }

# 4. the agent's phone arguments
printf 'PHONE_AGENT_ARGS="--phone-image %s --phone-aosp %s --phone-emulator %s --phone-apk %s --phone-snapshot %s"\n' \
  "$IMG" "$AOSP" "$EMU" "$APK" "$TAG" > "$ENVF"
cat "$ENVF"
