#!/bin/bash
# N Minecraft servers through the fleet's own path on one host: a coordinator, its local agent
# (--minecraft N, routed networking) and a client that reads the status rows as a page does
# (fleetfield.py). Run as root on a bare-metal box with fcvm set up, the tile image in root's
# podman store and IPv6 forwarding on (never on the shared dev host).
#
#   FCVM=<fcvm> fleetfield.sh N OUT IPV6_PREFIX [fleetfield.py options]
#
# OUT gets host.txt (kernel, CPUs, memory, the store's device), disk.log (the store's use and the
# host's I/O stall time every 5 s), the coordinator's log and state file, and fleetfield.py's records.
set -uo pipefail
N=$1; OUT=$2; PREFIX=$3; shift 3
F=${FCVM:?FCVM is the fcvm binary}
IMAGE=${MC_IMAGE:-localhost/fcvm-minecraft:dev}
PORT=${MC_COORD_PORT:-20000}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
export RUST_LOG=fcvm::fleet=info
ulimit -n 1048576
STORE=$(findmnt -no SOURCE,FSTYPE,SIZE /mnt/fcvm-btrfs)
{ echo "kernel $(uname -r)"; echo "cpus $(nproc)"; grep MemTotal /proc/meminfo; echo "store $STORE"; losetup -a | grep -F "$(echo "$STORE" | cut -d' ' -f1):" || true; lsblk -d -o NAME,MODEL,SIZE | grep -v loop; } > "$OUT/host.txt" 2>&1
(while true; do echo "$(date +%s) store_mib=$(df -B1M --output=used /mnt/fcvm-btrfs | tail -1 | tr -d ' ') load=$(cut -d' ' -f1-3 /proc/loadavg) fc=$(pgrep -c firecracker) io_full_avg10=$(awk '/^full/{split($2,a,"=");print a[2]}' /proc/pressure/io) io_full_total_us=$(awk '/^full/{split($5,a,"=");print a[2]}' /proc/pressure/io)"; sleep 5; done) > "$OUT/disk.log" 2>&1 &
DL=$!
setsid "$F" fleet coord --listen "127.0.0.1:$PORT" --state-file "$OUT/state.json" --ec2 fake --restart-cooldown 1 \
  --agent-cmd "$F fleet agent --stdio --host field-box --image $IMAGE --base-port $((PORT + 100)) --publish-ip 127.0.0.1 --ipv6-prefix $PREFIX --minecraft $N --minecraft-image $IMAGE" \
  > "$OUT/coord.log" 2>&1 < /dev/null &
CP=$!
# The coordinator stops its agent, and the agent its servers, when it is told to stop.
trap 'kill -TERM $CP 2>/dev/null; kill $DL 2>/dev/null' EXIT
for i in $(seq 1 120); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; kill -0 $CP 2>/dev/null || break; sleep 1; done
python3 "$HERE/fleetfield.py" "ws://127.0.0.1:$PORT/live" "$OUT" "$N" "$@" > "$OUT/fleetfield.log" 2>&1
echo "fleetfield.py exit=$?" >> "$OUT/fleetfield.log"
kill -TERM $CP; wait $CP
echo "coordinator exit=$?" >> "$OUT/fleetfield.log"
kill $DL 2>/dev/null
for i in $(seq 1 120); do [ "$(pgrep -c firecracker)" = 0 ] && break; sleep 1; done
echo "left: firecracker=$(pgrep -c firecracker)" >> "$OUT/fleetfield.log"
echo RUN_DONE >> "$OUT/fleetfield.log"
