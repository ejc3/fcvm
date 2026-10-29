#!/bin/bash
# Starts the phone VM for the measurement, detached; its fcvm pid goes to $B/vm.pid once healthy.
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
cd ~/fcvm-bench
F=./target/release/fcvm
IMAGE=$(./scripts/phone/run.sh --image)
AOSP=$(./scripts/phone/aosp.sh --root-path)
EMU=$(./scripts/phone/emulator.sh --path)
GPU=$(head -1 /run/fcvm-fleet/gpus | cut -d' ' -f1)
NAME=phone-bench
rm -f $B/vm.pid
nohup setsid $F podman run --name $NAME --kernel-profile phone --gpu "$GPU" --privileged --cpu 6 --mem 10240 \
  --portable-volumes --map "$AOSP:/android:ro" --map "$EMU:/emu:ro" --map "$B:/bench" \
  "$IMAGE" > $B/logs/vm.log 2>&1 < /dev/null &
for _ in $(seq 1 600); do
  pid=$($F ls --json 2>/dev/null | jq -r --arg n "$NAME" '.[] | select(.name == $n and .health_status == "healthy") | .pid' | head -1)
  [ -n "$pid" ] && { echo "$pid" > $B/vm.pid; echo "healthy pid $pid gpu $GPU"; exit 0; }
  sleep 1
done
echo "not healthy; log $B/logs/vm.log"; tail -20 $B/logs/vm.log; exit 1
