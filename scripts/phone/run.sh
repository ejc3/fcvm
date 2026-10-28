#!/bin/bash
# A phone VM renders on its GPU and can let go of it and take it up again: an fcvm VM of the
# phone profile with one GPU, restored from its pre-start snapshot as every GPU VM is,
# runs an Android Emulator built from source (scripts/phone/emulator.sh) whose GPU rendering
# goes to that GPU. Checks, each failing the run:
#   1. the VM was restored from a snapshot, and KVM runs a VM inside it (fleet/phone/kvmprobe.c):
#      a restored guest keeps its nested virtualization state only with the phone profile's
#      Firecracker (KVM_GET/SET_NESTED_STATE; with the gpu profile's commit the guest's
#      kvm_intel oopses in kvm_spurious_fault and the probe dies);
#   2. Android boots (sys.boot_completed) with -gpu host;
#   3. SurfaceFlinger composes with GLES on that GPU: its renderer names the GPU nvidia-smi
#      reports in the VM;
#   4. an app (fleet/phone/probe) gets a GLES 3 context on that GPU and draws: its renderer
#      names the GPU and it reports at least 30 frames a second;
#   5. the running phone lets go of the GPU (the container's GPU hook, `gpu release` on the
#      emulator's console): the emulator then holds no /dev/nvidia* descriptor or mapping;
#      and takes it up again (`gpu retake`): the app draws 30 frames a second again.
#
#   FCVM=./target/release/fcvm scripts/phone/run.sh <gpu pci address>
# PHONE_PROFILE names another kernel profile to run the same checks on (default phone).
# `run.sh --image` prints the image tag it builds: fleet/phone's contents name it, so
# concurrent runs from checkouts with other sources never replace each other's image.
# Needs a GPU on vfio-pci, the phone profile set up (make setup-phone), the host's
# kvm_intel loaded with nested=1, the SDK and probe app from scripts/phone/sdk.sh and the
# emulator from scripts/phone/emulator.sh.
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)
IMAGE=localhost/fcvm-phone:$(cd "$SRC/fleet/phone" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
if [ "${1:-}" = --image ]; then echo "$IMAGE"; exit 0; fi
GPU=${1:?usage: run.sh <gpu pci address>}
F=${FCVM:-./target/release/fcvm}
SDK=$("$(dirname "$0")/sdk.sh" --sdk-path)
APK=$(ANDROID_SDK="$SDK" "$(dirname "$0")/sdk.sh" --probe-path)
WORK=$(dirname "$APK")
EMU=$("$(dirname "$0")/emulator.sh" --path)
NAME=phone-test-$$
[ -f "$APK" ] || { echo "run.sh: run scripts/phone/sdk.sh first" >&2; exit 2; }
[ -x "$EMU/emulator" ] || { echo "run.sh: run scripts/phone/emulator.sh first" >&2; exit 2; }
[ "$(cat /sys/module/kvm_intel/parameters/nested 2>/dev/null)" = Y ] || { echo "run.sh: the host's kvm_intel needs nested=1" >&2; exit 2; }
LOGS=$(mktemp -d)
podman build -q -t "$IMAGE" "$SRC/fleet/phone" > "$LOGS/build.log" 2>&1 || { cat "$LOGS/build.log"; exit 1; }
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
runner=""
stop_runner() {
  [ -n "$runner" ] || return 0
  kill -TERM "$runner" 2>/dev/null || true
  wait "$runner" 2>/dev/null || true
  runner=""
}
trap stop_runner EXIT
trap 'exit 130' INT TERM
t0=$(date +%s%3N)
"$F" podman run --name "$NAME" --kernel-profile "${PHONE_PROFILE:-phone}" --gpu "$GPU" --privileged --cpu 6 --mem 6144 \
  --map "$SDK:/sdk:ro" --map "$WORK:/work:ro" --map "$EMU:/emu:ro" \
  "$IMAGE" > "$LOGS/vm.log" 2>&1 < /dev/null &
runner=$!
pid=""
for _ in $(seq 1 600); do
  pid=$("$F" ls --json 2>/dev/null | jq -r --arg n "$NAME" '.[] | select(.name == $n and .health_status == "healthy") | .pid' | head -1)
  [ -n "$pid" ] && break
  kill -0 "$runner" 2>/dev/null || break
  sleep 1
done
[ -n "$pid" ] || { echo "FAIL the phone VM never became healthy (log $LOGS/vm.log)"; exit 1; }
ack=$(grep -m1 '"ack_ms"' "$LOGS/vm.log" || true)
status=$(jq -r '.status // empty' <<< "$ack" 2>/dev/null || true)
echo "phone VM healthy after $(( $(date +%s%3N) - t0 )) ms; restore ack_ms=$(jq -r '.ack_ms' <<< "$ack") gpu_ms=$(jq -r '.guest_phases.gpu_ms' <<< "$ack")"
[ "$status" = restored ] && ok "the phone VM was restored from its snapshot" || bad "the phone VM did not report a snapshot restore (status '${status}')"
X() { "$F" exec --pid "$pid" -- "$@"; }
gpu_name=$(X /usr/bin/nvidia-smi --query-gpu=name --format=csv,noheader 2>&1 | head -1)
[ -n "$gpu_name" ] && ok "the VM has GPU '$gpu_name'" || bad "nvidia-smi names no GPU"
rc=0; probe=$(timeout 30 "$F" exec --pid "$pid" -- /usr/local/bin/kvmprobe 2>&1) || rc=$?
[ "$probe" = "kvm ok" ] && ok "KVM runs a VM inside the restored phone VM" || bad "KVM in the restored VM: exit $rc, '${probe}'"

# The lowest of the app's fps samples over the next 4 seconds on VM $1 ("" when it logged none).
lowest_fps() {
  "$F" exec --pid "$1" -- /usr/local/bin/phone.sh draws 4 2>&1 | grep -o 'fps=[0-9.]*' | cut -d= -f2 | sort -n | head -1
}
at_least_30() { python3 -c 'import sys; sys.exit(0 if sys.argv[1] and float(sys.argv[1]) >= 30 else 1)' "${1:-}"; }

in_place_release_and_retake() {
  local out holds fps
  if out=$(X /usr/local/bin/gpu-hook release 2>&1); then
    holds=$(X /usr/local/bin/phone.sh holds 2>&1 || true)
    [ "$holds" = "fds=0 mappings=0" ] && ok "the running phone released the GPU ($(grep -o 'released_ms=[0-9]*' <<< "$out")): $holds" \
      || bad "the emulator still holds the GPU after the release: $holds"
  else
    bad "the phone did not release the GPU: $out"
  fi
  if out=$(X /usr/local/bin/gpu-hook retake 2>&1); then
    ok "the phone retook the GPU: $(grep -o -E '(retaken|first_frame)_ms=[0-9]*' <<< "$out" | paste -sd' ')"
  else
    bad "the phone did not retake the GPU: $out"
  fi
  fps=$(lowest_fps "$pid")
  at_least_30 "$fps" && ok "the app draws after the retake: lowest fps sample $fps" \
    || bad "the app does not draw 30 fps after the retake: lowest '${fps:-none}'"
}

if boot=$(timeout 400 "$F" exec --pid "$pid" -- /usr/local/bin/phone.sh boot 2>&1); then
  ok "Android booted with the host GPU: $boot"
  renderer=$(X /usr/local/bin/phone.sh renderer 2>&1 || true)
  [[ "$renderer" == *"$gpu_name"* ]] && ok "SurfaceFlinger: $renderer" || bad "SurfaceFlinger does not render on '$gpu_name': $renderer"
  out=$(timeout 120 "$F" exec --pid "$pid" -- /usr/local/bin/phone.sh probe 6 "/work/$(basename "$APK")" 2>&1 || true)
  echo "$out" > "$LOGS/probe.log"
  app_renderer=$(grep -m1 -o 'renderer=.* vendor=' <<< "$out" || true)
  [[ "$app_renderer" == *"$gpu_name"* ]] && ok "the app's GLES context: $app_renderer" || bad "the app's renderer does not name '$gpu_name': ${app_renderer:-none} (log $LOGS/probe.log)"
  fps=$(grep -o 'fps=[0-9.]*' <<< "$out" | tail -8 | cut -d= -f2 | sort -n | head -1)
  python3 -c 'import sys; sys.exit(0 if sys.argv[1] and float(sys.argv[1]) >= 30 else 1)' "${fps:-}" \
    && ok "the app draws: lowest of its last 8 fps samples $fps" || bad "the app does not draw 30 fps: lowest '${fps:-none}' (log $LOGS/probe.log)"
  in_place_release_and_retake
else
  bad "Android did not boot: $boot"
  X sh -c 'grep -aE "ERROR|FATAL" /tmp/emulator.log | head -20' || true
fi
X /usr/local/bin/phone.sh stop > /dev/null 2>&1 || true
stop_runner
echo "logs: $LOGS"
exit $fail
