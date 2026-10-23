#!/usr/bin/env bash
# GPU VM restore latency: N cache-hit `fcvm podman run --gpu` runs, one after another,
# rotating over the host's vfio-pci bound NVIDIA GPUs.
#
#   make bench-gpu-restore N=104 [GPUS="0000:35:00.0 0000:36:00.0"]
#
# Each run is `fcvm podman run --kernel-profile gpu --gpu <sbdf> ubuntu:24.04 nvidia-smi -L`.
# One untimed run first creates the gpu profile's pre-start snapshot of ubuntu:24.04, so every
# timed run restores it. Per run it records the wall time from process start to exit, and from
# the clone's restored record (stdout) the host-observed ack_ms and the guest's gpu_ms.
set -euo pipefail

N=${N:-104}
IMAGE=${IMAGE:-ubuntu:24.04}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
FCVM=$REPO/target/release/fcvm
OUT=${OUT:-/mnt/fcvm-btrfs/gpu-restore/$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$OUT/runs"
for tool in jq python3 lspci; do
  command -v "$tool" >/dev/null || { echo "BLOCKED: $tool missing" >&2; exit 2; }
done

# NVIDIA display-class functions bound to vfio-pci, or the space-separated SBDFs in GPUS.
if [ -n "${GPUS:-}" ]; then
  read -r -a GPUS <<<"$GPUS"
else
  mapfile -t GPUS < <(for d in /sys/bus/pci/devices/*; do
  [ "$(cat "$d/vendor")" = 0x10de ] || continue
  case "$(cat "$d/class")" in 0x03*) ;; *) continue ;; esac
  [ "$(basename "$(readlink -f "$d/driver" 2>/dev/null)")" = vfio-pci ] && basename "$d"
  done)
fi
[ "${#GPUS[@]}" -gt 0 ] || { echo "no vfio-pci bound NVIDIA GPU found" >&2; exit 2; }

run_one() { # name sbdf logfile -> prints wall ms; returns fcvm's exit status
  local s e rc=0
  s=$(date +%s%N)
  "$FCVM" podman run --name "$1" --kernel-profile gpu --gpu "$2" "$IMAGE" nvidia-smi -L >"$3" 2>&1 || rc=$?
  e=$(date +%s%N)
  echo $(( (e - s) / 1000000 ))
  return "$rc"
}

run_one gpu-warm "${GPUS[0]}" "$OUT/warm.log" >/dev/null || { echo "warm-up run failed: $OUT/warm.log" >&2; exit 1; }
{
  echo "n=$N image=$IMAGE gpus=${#GPUS[@]} fcvm_commit=$(git -C "$REPO" rev-parse HEAD)"
  echo "kernel=$(uname -r) arch=$(uname -m) cpus=$(nproc) gpu_model=$(lspci -s "${GPUS[0]}" | cut -d: -f3-)"
} >"$OUT/env.txt"
echo "run,sbdf,wall_ms,exit,ack_ms,gpu_ms,backend,load1" >"$OUT/runs.csv"
for i in $(seq 1 "$N"); do
  g=${GPUS[$(( (i - 1) % ${#GPUS[@]} ))]}
  log=$OUT/runs/$i.log
  rc=0
  wall=$(run_one "gpu-$i" "$g" "$log") || rc=$?
  load1=$(cut -d' ' -f1 /proc/loadavg)
  # The restored record is the one JSON line on stdout whose status is "restored".
  rec=$(grep '^{' "$log" | jq -c 'select(.status == "restored")' 2>/dev/null | head -1)
  [ -n "$rec" ] || rec='{}'

  echo "$i,$g,$wall,$rc,$(jq -r '.ack_ms // ""' <<<"$rec"),$(jq -r '.guest_phases.gpu_ms // ""' <<<"$rec"),$(jq -r '.backend // ""' <<<"$rec"),$load1" >>"$OUT/runs.csv"
done
python3 "$HERE/summarize.py" "$OUT/runs.csv" | tee "$OUT/summary.txt"
