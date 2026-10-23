#!/bin/bash
# Cross-tenant GPU memory residue test, on a host with a GPU bound to vfio-pci.
#
#   make gpu-residue-test GPU=<pci address> [RESIDUE_OUT=<dir>]
#
# For each case and each allocation mode, two VMs run one after the other on
# the same GPU:
#
#   A  fills all the GPU memory it can allocate with 16-byte records
#      (a 64-bit marker and the record's offset), then ends:
#        exit  the container exits normally and the VM shuts down;
#        kill  the container holds the memory and the fcvm process is SIGKILLed
#              (its Firecracker dies with it).
#   B  is restored on the same GPU and, before writing anything, allocates as
#      much (runtime: cudaMalloc, driver: cuMemAlloc, managed:
#      cudaMallocManaged up to the free memory) and counts the marker words.
#
# Every scan checks itself: after counting, B writes one marker into its own
# memory and fails unless a second scan finds exactly one more. And A's fill
# reads its records back before it reports FILLED.
#
# The same-vm case runs fill and then scan as two processes in one VM, with no
# reset between them: what a later process of the same tenant sees.
#
# HOST_CLEAR, if set, is a command run on the host between A and B (a
# host-side clear to compare with fc-agent's scrub); its seconds are recorded.
#
# The tool is built with nvcc in NVIDIA's CUDA devel image (rootless podman),
# and runs in an ubuntu:24.04 container, which gets libcuda from the CDI spec.
set -euo pipefail

GPU=${1:?usage: run.sh <gpu pci address> [out dir]}
OUT=${2:-/tmp/gpu-residue-$(date +%Y%m%d-%H%M%S)}
FCVM=${FCVM:-./target/release/fcvm}
CUDA_IMAGE=${CUDA_IMAGE:-docker.io/nvidia/cuda:12.6.3-devel-ubuntu24.04}
RUN_IMAGE=${RUN_IMAGE:-docker.io/library/ubuntu:24.04}
CASES=${CASES:-same-vm exit kill}
MODES=${MODES:-runtime driver managed}
HERE=$(cd "$(dirname "$0")" && pwd)

for tool in podman jq bc column; do
  command -v "$tool" >/dev/null || { echo "BLOCKED: $tool missing" >&2; exit 2; }
done
[ -x "$FCVM" ] || { echo "BLOCKED: no fcvm binary at $FCVM (make build)" >&2; exit 2; }
[ -e "/sys/bus/pci/devices/$GPU" ] || { echo "BLOCKED: no PCI device $GPU" >&2; exit 2; }
GROUP=$(basename "$(readlink "/sys/bus/pci/devices/$GPU/iommu_group")")

mkdir -p "$OUT/tools"
echo "== building gpu-residue in $CUDA_IMAGE"
podman run --rm -v "$HERE:/src:ro" -v "$OUT/tools:/out" "$CUDA_IMAGE" \
  nvcc -O2 -arch=sm_75 -o /out/gpu-residue /src/residue.cu \
  -L/usr/local/cuda/lib64/stubs -lcuda
[ -x "$OUT/tools/gpu-residue" ] || { echo "the build produced no gpu-residue" >&2; exit 1; }

# Wait until no process holds the GPU's VFIO group, so B gets the device only
# after A's Firecracker is gone. A group has one opener at a time, so an open
# that succeeds (and is closed at once) proves it is free. fuser cannot be used:
# unprivileged, it does not see the fds of a VMM inside the VM's user namespace.
wait_gpu_released() {
  for _ in $(seq 1 120); do
    if (exec 3<>"/dev/vfio/$GROUP") 2>/dev/null; then return 0; fi
    sleep 0.5
  done
  echo "the GPU's VFIO group $GROUP is still open after 60 s" >&2
  return 1
}

VM=(podman run --kernel-profile gpu --gpu "$GPU" --map "$OUT/tools:/tools")

# A background VM A (the kill case) is stopped on any exit, so a run that
# stops early does not leave it holding the GPU.
apid=
stop_a() {
  if [ -n "$apid" ] && kill -0 "$apid" 2>/dev/null; then
    kill -9 "$apid"
    wait "$apid" 2>/dev/null || true
  fi
  apid=
}
trap stop_a EXIT

printf 'case\tmode\tbytes\tmarker_words\tmarker_fraction\tnonzero_fraction\tscrub_ms\thost_clear_s\n' > "$OUT/summary.tsv"
n=0
for case in $CASES; do
  for mode in $MODES; do
    n=$((n + 1))
    tag="res-$$-$n"
    alog="$OUT/$case-$mode-A.log"
    blog="$OUT/$case-$mode-B.log"
    wait_gpu_released
    clear_s=-
    if [ "$case" = same-vm ]; then
      echo "== $case/$mode: one VM, fill then scan"
      "$FCVM" "${VM[@]}" --name "$tag-a" "$RUN_IMAGE" \
        sh -c "/tools/gpu-residue fill && /tools/gpu-residue scan $mode" > "$blog" 2>&1
      cp "$blog" "$alog"
    elif [ "$case" = exit ]; then
      "$FCVM" "${VM[@]}" --name "$tag-a" "$RUN_IMAGE" /tools/gpu-residue fill > "$alog" 2>&1
    else
      # Started directly, so $! is the fcvm process itself.
      "$FCVM" "${VM[@]}" --name "$tag-a" "$RUN_IMAGE" /tools/gpu-residue fill --hold > "$alog" 2>&1 &
      apid=$!
      for _ in $(seq 1 600); do
        grep -q FILLED "$alog" && break
        kill -0 "$apid" 2>/dev/null || break
        sleep 0.5
      done
      grep -q FILLED "$alog" || { echo "A never reported FILLED; see $alog" >&2; exit 1; }
      stop_a
    fi
    grep -o 'FILLED.*' "$alog" | head -1 || { echo "A printed no FILLED; see $alog" >&2; exit 1; }
    wait_gpu_released
    if [ "$case" != same-vm ] && [ -n "${HOST_CLEAR:-}" ]; then
      started=$(date +%s.%N)
      bash -c "$HOST_CLEAR" > "$OUT/$case-$mode-clear.log" 2>&1 || { echo "HOST_CLEAR failed; see $OUT/$case-$mode-clear.log" >&2; exit 1; }
      clear_s=$(echo "$(date +%s.%N) - $started" | bc)
      echo "   host clear: $clear_s s"
    fi
    if [ "$case" != same-vm ]; then
      echo "== $case/$mode: B scans"
      "$FCVM" "${VM[@]}" --name "$tag-b" "$RUN_IMAGE" /tools/gpu-residue scan "$mode" > "$blog" 2>&1
    fi
    result=$(grep -o 'RESULT .*' "$blog" | head -1 || true)
    [ -n "$result" ] || { echo "B printed no RESULT; see $blog" >&2; exit 1; }
    echo "   $result"
    field() { sed -n "s/.* $1=\([^ ]*\).*/\1/p" <<<"$result"; }
    # The restore ACK's gpu_steps.scrub_ms, when fc-agent scrubbed the GPU: the
    # restored record is a JSON line in B's log.
    scrub=$(jq -rR 'fromjson? | select(type == "object" and has("ack_ms"))
      | .guest_phases.gpu_steps.scrub_ms // empty' "$blog" | head -1 || true)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$case" "$mode" "$(field bytes)" \
      "$(field marker_words)" "$(field marker_fraction)" "$(field nonzero_fraction)" \
      "${scrub:--}" "$clear_s" >> "$OUT/summary.tsv"
  done
done
echo "== summary ($OUT/summary.tsv)"
column -t "$OUT/summary.tsv"
