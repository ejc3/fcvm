#!/bin/bash
# A `snapshot create --gpu-warm` SIGKILLed after it parked and detached the source's GPU must
# leave the source with its GPU (#1939): within 60 s `nvidia-smi -L` in the guest names it
# again, and a following warm snapshot of the same VM succeeds.
#   FCVM=./target/release/fcvm scripts/gpu-warm-kill/run.sh <gpu pci address> [image]
# Needs a GPU on vfio-pci and the gpu profile set up (make setup-gpu). The create is held at
# the `snapshot.post_save_pre_resume` failpoint: the GPU is parked and detached and the VM
# paused, and nothing the create would do on its own way out runs after the SIGKILL.
set -uo pipefail
GPU=${1:?usage: run.sh <gpu pci address> [image]}
IMAGE=${2:-docker.io/library/nginx:alpine}
F=${FCVM:-./target/release/fcvm}
NAME=gwk-$$
TAG=gwk-$$
TAG2=gwk2-$$
LOGS=$(mktemp -d)
runner=""
create=""
cleanup() {
  [ -n "$create" ] && { kill -KILL "$create" 2>/dev/null; wait "$create" 2>/dev/null; }
  [ -n "$runner" ] && { kill -TERM "$runner" 2>/dev/null; wait "$runner" 2>/dev/null; }
  "$F" snapshots delete "$TAG" > /dev/null 2>&1 || true
  "$F" snapshots delete "$TAG2" > /dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM
fail() { echo "FAIL $*"; echo "logs: $LOGS"; exit 1; }
gpus_seen() { timeout 10 "$F" exec --pid "$pid" --vm -- nvidia-smi -L 2>&1; }

"$F" podman run --name "$NAME" --kernel-profile gpu --gpu "$GPU" --no-snapshot "$IMAGE" \
  > "$LOGS/vm.log" 2>&1 < /dev/null &
runner=$!
pid=""
for _ in $(seq 1 300); do
  pid=$("$F" ls --json 2>/dev/null | jq -r --arg n "$NAME" '.[] | select(.name == $n and .health_status == "healthy") | .pid' | head -1)
  [ -n "$pid" ] && break
  kill -0 "$runner" 2>/dev/null || break
  sleep 1
done
[ -n "$pid" ] || fail "the GPU VM never turned healthy (log $LOGS/vm.log)"
before=$(gpus_seen) || fail "nvidia-smi -L failed in the guest before the snapshot: $before"
grep -q '^GPU ' <<< "$before" || fail "the guest names no GPU before the snapshot: $before"
echo "ok   the guest names its GPU: $(head -1 <<< "$before")"

FCVM_FAILPOINT="snapshot.post_save_pre_resume:block_until_file:$LOGS/never" \
  "$F" snapshot create --pid "$pid" --tag "$TAG" --gpu-warm > "$LOGS/create.log" 2>&1 < /dev/null &
create=$!
for _ in $(seq 1 300); do
  grep -q "FAILPOINT snapshot.post_save_pre_resume reached" "$LOGS/create.log" && break
  kill -0 "$create" 2>/dev/null || fail "the create exited before its failpoint: $(tail -5 "$LOGS/create.log")"
  sleep 1
done
grep -q "FAILPOINT snapshot.post_save_pre_resume reached" "$LOGS/create.log" \
  || fail "the create never reached its failpoint"
killed=$(date +%s%3N)
kill -KILL "$create"; wait "$create" 2>/dev/null; create=""

seen=""
while [ $(( $(date +%s%3N) - killed )) -lt 60000 ]; do
  if out=$(gpus_seen) && grep -q '^GPU ' <<< "$out"; then seen=$out; break; fi
  sleep 1
done
[ -n "$seen" ] || fail "60 s after the create's SIGKILL the guest names no GPU: ${out:-}"
echo "ok   the guest names its GPU $(( $(date +%s%3N) - killed )) ms after the create's SIGKILL"

"$F" snapshot create --pid "$pid" --tag "$TAG2" --gpu-warm > "$LOGS/create2.log" 2>&1 < /dev/null \
  || fail "a following warm snapshot of the same VM failed: $(tail -5 "$LOGS/create2.log")"
echo "ok   a following warm snapshot of the same VM succeeds"
