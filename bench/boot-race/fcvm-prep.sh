#!/bin/bash
# Make the fcvm lanes' snapshot: boot the workload image in a VM sized like the EC2 lanes' c8g.medium
# (1 vCPU, 2 GiB), wait until it answers, snapshot it, stop it, and start the memory server the UFFD lane uses.
#   fcvm-prep.sh <fcvm binary> <results dir>
set -euo pipefail
FCVM=$1 RESULTS=$2
mkdir -p "$RESULTS"
TAG=boot-race
PORT=20999
"$FCVM" podman run --name boot-race-base --cpu 1 --mem 2048 --publish "127.0.0.1:$PORT:8080" \
  localhost/boot-race-workload > "$RESULTS/fcvm-base.log" 2>&1 &
BASE=$!
for _ in $(seq 1 600); do
  curl -sf --noproxy '*' -m 1 "http://127.0.0.1:$PORT/" >/dev/null && break
  kill -0 $BASE 2>/dev/null || { echo "base VM exited"; tail -30 "$RESULTS/fcvm-base.log"; exit 1; }
  sleep 0.5
done
ANSWER=$(curl -sf --noproxy '*' -m 1 "http://127.0.0.1:$PORT/")
echo "$ANSWER"
# race.py refuses an fcvm sample whose answer is not from this process (cfg.json fcvm_proc_id).
echo "fcvm_proc_id=$(jq -r .proc_id <<<"$ANSWER")"
"$FCVM" snapshot create --pid "$BASE" --tag "$TAG" 2>&1 | tail -5
kill -TERM $BASE; wait $BASE || true
setsid "$FCVM" snapshot serve "$TAG" > "$RESULTS/fcvm-serve.log" 2>&1 < /dev/null &
echo "serve_pid=$!"
