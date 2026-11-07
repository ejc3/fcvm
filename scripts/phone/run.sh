#!/bin/bash
# A phone VM renders on its GPU and survives a warm snapshot onto another GPU: an fcvm VM of
# the phone profile with one GPU, restored from its pre-start snapshot as every GPU VM is,
# runs an Android Emulator built from source (scripts/phone/emulator.sh) whose GPU rendering
# goes to that GPU. Checks, each failing the run:
#   1. the VM was restored from a snapshot, and KVM runs a VM inside it (fleet/phone/kvmprobe.c):
#      a restored guest keeps its nested virtualization state only with the phone profile's
#      Firecracker (KVM_GET/SET_NESTED_STATE; with the gpu profile's commit the guest's
#      kvm_intel oopses in kvm_spurious_fault and the probe dies);
#   2. Android boots (sys.boot_completed) with -gpu host, and the phone takes no bug
#      report (one, which a failing device such as UWB triggers, takes a core for ~30 s);
#   3. SurfaceFlinger composes with GLES on that GPU: its renderer names the GPU nvidia-smi
#      reports in the VM;
#   4. an app (fleet/phone/probe) gets a GLES 3 context on that GPU and draws: its renderer
#      names the GPU and it reports at least 30 frames a second;
#   5. the running phone lets go of the GPU (the container's GPU hook, `gpu release` on the
#      emulator's console): the emulator then holds no /dev/nvidia* descriptor or mapping;
#      and takes it up again (`gpu retake`): the app draws 30 frames a second again. A retake
#      that cannot create a Vulkan instance (the ICD manifest moved away) fails and leaves the
#      phone running released, and the next retake takes the GPU up; a release while a guest
#      memory dump runs (which paused the guest itself) is refused, the dump completes, and
#      the phone keeps its GPU;
#   6. a warm snapshot of the running phone (`fcvm snapshot create --gpu-warm`, whose park
#      runs the hook) is taken and the source phone keeps drawing; a clone restored from it
#      with the second GPU (`fcvm snapshot run --gpu`) runs the same phone on that GPU:
#      nvidia-smi names a different GPU than the source's, SurfaceFlinger's renderer names
#      it, the emulator holds it open, and the app draws 30 frames a second there. Prints the
#      time from the restore command to the phone's first frame on the clone's GPU.
#   7. the web browser (scripts/phone/browser.sh) runs through all of it: it opens
#      www.nvidia.com through the phone web proxy before the release checks, which a guest
#      Vulkan instance would refuse; the proxy refuses a name off its list; and on the clone,
#      `phone.sh action open_browser` brings the page to the front within 1 s and draws it.
#   8. Robot Park (fleet/phone-app), last, on the source phone: seams, surface loss and
#      actions (scripts/phone/scene-test.sh); and the phone took no bug report during
#      the whole run (phone.sh turns UWB off, whose service otherwise asks for one a minute
#      or two after boot).
#
#   FCVM=./target/release/fcvm scripts/phone/run.sh <gpu pci address> <second gpu pci address>
# PHONE_PROFILE names another kernel profile to run the same checks on (default phone).
# `run.sh --image` prints the image tag it builds: fleet/phone's contents and the renderer
# code its streamer builds with name it (fleet/phone/Makefile), so
# concurrent runs from checkouts with other sources never replace each other's image.
# Needs two GPUs on vfio-pci, the phone profile set up (make setup-phone), the host's
# kvm_intel loaded with nested=1, the system image, adb and probe app from
# scripts/phone/aosp.sh, the emulator from scripts/phone/emulator.sh and Robot Park from
# scripts/phone/app.sh.
SRC=$(cd "$(dirname "$0")/../.." && pwd)
. "$SRC/scripts/phone/checks.sh"
IMAGE=localhost/fcvm-phone:$(make -s -C "$SRC/fleet/phone" image-id)
if [ "${1:-}" = --image ]; then echo "$IMAGE"; exit 0; fi
GPU=${1:?usage: run.sh <gpu pci address> <second gpu pci address>}
GPU2=${2:?usage: run.sh <gpu pci address> <second gpu pci address>}
[ "$GPU" != "$GPU2" ] || { echo "run.sh: the clone needs a second GPU" >&2; exit 2; }
F=${FCVM:-./target/release/fcvm}
AOSP=$("$(dirname "$0")/aosp.sh" --root-path)
APK=$(ANDROID_AOSP="$AOSP" "$(dirname "$0")/aosp.sh" --probe-path)
APP=$("$(dirname "$0")/app.sh" --apk-path)
BROWSER=$("$(dirname "$0")/browser.sh" --apk-path)
WORK=$(dirname "$APK")
EMU=$("$(dirname "$0")/emulator.sh" --path)
NAME=phone-test-$$
TAG=phone-test-$$-warm
[ -f "$APK" ] || { echo "run.sh: run scripts/phone/aosp.sh first" >&2; exit 2; }
[ -x "$EMU/emulator" ] || { echo "run.sh: run scripts/phone/emulator.sh first" >&2; exit 2; }
[ -f "$APP" ] && [ "$(dirname "$APP")" = "$WORK" ] || { echo "run.sh: run scripts/phone/app.sh first" >&2; exit 2; }
[ -f "$BROWSER" ] && [ "$(dirname "$BROWSER")" = "$WORK" ] || { echo "run.sh: run scripts/phone/browser.sh first" >&2; exit 2; }
[ "$(cat /sys/module/kvm_intel/parameters/nested 2>/dev/null)" = Y ] || { echo "run.sh: the host's kvm_intel needs nested=1" >&2; exit 2; }
LOGS=$(mktemp -d)
mkdir -p "$LOGS/out"
make -s -C "$SRC/fleet/phone" image IMAGE="$IMAGE" > "$LOGS/build.log" 2>&1 || { cat "$LOGS/build.log"; exit 1; }
runner=""
clone=""
snapshot_dir=""
web_proxy=""
stop_runner() {
  local r
  for r in "$clone" "$runner" "$web_proxy"; do
    [ -n "$r" ] || continue
    kill -TERM "$r" 2>/dev/null || true
    wait "$r" 2>/dev/null || true
  done
  runner=""
  clone=""
  web_proxy=""
  # The warm snapshot is this run's alone (its tag names the run).
  if [ -n "$snapshot_dir" ]; then
    rm -rf "$snapshot_dir" "$snapshot_dir.lock"
    snapshot_dir=""
  fi
}
finished=0
trap 'finish_run stop_runner' EXIT
trap 'exit 130' INT TERM
# The phone web proxy, as an agent with phones serves it: the browser's only way out.
"$F" fleet phone-web-proxy --listen 127.0.0.1:3128 > "$LOGS/web-proxy.log" 2>&1 &
web_proxy=$!
for _ in $(seq 1 50); do (exec 3<> /dev/tcp/127.0.0.1/3128) 2> /dev/null && break; kill -0 "$web_proxy" 2> /dev/null || break; sleep 0.1; done
kill -0 "$web_proxy" 2> /dev/null || { echo "run.sh: the phone web proxy did not start: $(cat "$LOGS/web-proxy.log")" >&2; exit 2; }
t0=$(date +%s%3N)
"$F" podman run --name "$NAME" --kernel-profile "${PHONE_PROFILE:-phone}" --gpu "$GPU" --privileged --cpu 6 --mem 6144 \
  --portable-volumes --map "$AOSP:/android:ro" --map "$WORK:/work:ro" --map "$EMU:/emu:ro" --map "$LOGS/out:/out" --forward-localhost 3128 \
  "$IMAGE" > "$LOGS/vm.log" 2>&1 < /dev/null &
runner=$!
pid=$(WAIT_HEALTHY_WHILE=$runner wait_healthy "$NAME" 600 1) || pid=""
[ -n "$pid" ] || { echo "FAIL the phone VM never became healthy: $(ls_error) (log $LOGS/vm.log)"; exit 1; }
ack=$(grep -m1 '"ack_ms"' "$LOGS/vm.log" || true)
status=$(jq -r '.status // empty' <<< "$ack" 2>/dev/null || true)
echo "phone VM healthy after $(( $(date +%s%3N) - t0 )) ms; restore ack_ms=$(jq -r '.ack_ms' <<< "$ack") gpu_ms=$(jq -r '.guest_phases.gpu_ms' <<< "$ack")"
[ "$status" = restored ] && ok "the phone VM was restored from its snapshot" || bad "the phone VM did not report a snapshot restore (status '${status}')"
X() { "$F" exec --pid "$pid" -- "$@"; }
gpu_name=$(X /usr/bin/nvidia-smi --query-gpu=name --format=csv,noheader 2>&1 | head -1) || gpu_name=""
[ -n "$gpu_name" ] && ok "the VM has GPU '$gpu_name'" || bad "nvidia-smi names no GPU"
rc=0; probe=$(timeout 30 "$F" exec --pid "$pid" -- /usr/local/bin/kvmprobe 2>&1) || rc=$?
[ "$probe" = "kvm ok" ] && ok "KVM runs a VM inside the restored phone VM" || bad "KVM in the restored VM: exit $rc, '${probe}'"

in_place_release_and_retake() {
  local out holds tls
  if out=$(X /usr/local/bin/gpu-hook release 2>&1); then
    holds=$(X /usr/local/bin/phone.sh holds 2>&1 || true)
    [ "$holds" = "fds=0 mappings=0" ] && ok "the running phone released the GPU ($(grep -o 'released_ms=[0-9]*' <<< "$out")): $holds" \
      || bad "the emulator still holds the GPU after the release: $holds"
    # the driver's TLS library stays loaded through the release (phone.sh preloads it), so
    # the retakes never run glibc's static TLS out
    tls=$(X sh -c 'grep -c libnvidia-tls /proc/$(pgrep -f qemu-system-x86_64 | head -1)/maps' 2>&1 || true)
    [[ "$tls" =~ ^[1-9] ]] && ok "the released emulator keeps the driver's TLS library loaded" \
      || bad "the released emulator unloaded libnvidia-tls ($tls mappings): each retake takes static TLS again"
  else
    bad "the phone did not release the GPU: $out"
  fi
  if out=$(X /usr/local/bin/gpu-hook retake 2>&1); then
    ok "the phone retook the GPU: $(grep -o -E '(retaken|first_frame)_ms=[0-9]*' <<< "$out" | paste -sd' ')"
  else
    bad "the phone did not retake the GPU: $out"
  fi
  check_draws "$pid" "after the retake"
}

# A retake whose Vulkan instance cannot be created leaves the phone released, not dead: the
# loader finds no driver while the ICD manifest is moved away, as it found none in a retake
# that aborted the emulator (gfxstream then loaded render threads that need Vulkan). With the
# manifest back, a second retake takes the GPU up and the app draws.
retake_without_vulkan() {
  local out holds icd=/usr/share/vulkan/icd.d/nvidia_icd.json
  if ! out=$(X /usr/local/bin/gpu-hook release 2>&1); then
    bad "the phone did not release the GPU: $out"
    return
  fi
  X mv "$icd" "$icd.away"
  if out=$(X /usr/local/bin/gpu-hook retake 2>&1); then
    bad "a retake with no Vulkan driver succeeded: $out"
  else
    [[ "$out" == *"Vulkan did not come back"* ]] && ok "a retake with no Vulkan driver fails and says why" \
      || bad "a retake with no Vulkan driver failed for another reason: $out"
  fi
  X mv "$icd.away" "$icd"
  holds=$(X /usr/local/bin/phone.sh holds 2>&1 || true)
  [ "$holds" = "fds=0 mappings=0" ] && ok "the phone is still running, released: $holds" \
    || { bad "after the failed retake the phone is not running released: $holds"; keep_phone_logs "$pid" retake-without-vulkan; }
  if out=$(X /usr/local/bin/gpu-hook retake 2>&1); then
    ok "the next retake took the GPU up: $(grep -o -E '(retaken|first_frame)_ms=[0-9]*' <<< "$out" | paste -sd' ')"
  else
    bad "the next retake failed too: $out"
  fi
  check_draws "$pid" "after a retake without Vulkan and one with it"
}

# QMP command $1 (JSON without an id) on the phone's emulator (phone.sh boot serves QEMU's
# monitor on the container's 127.0.0.1:4444); prints the reply to it, the line whose id is the
# one sent, so events QEMU sends between (DUMP_COMPLETED among them) are never taken for it.
qmp() {
  X bash -c 'exec 3<> /dev/tcp/127.0.0.1/4444 || exit 1
    IFS= read -r -t 5 l <&3
    printf "%s\n" "{\"execute\":\"qmp_capabilities\",\"id\":\"cap\"}" "$(jq -c ". + {id: \"cmd\"}" <<< "$1")" >&3
    while IFS= read -r -t 30 l <&3; do
      [ "$(jq -r ".id // empty" <<< "$l" 2> /dev/null)" = cmd ] && { echo "$l"; exit 0; }
    done
    exit 1' _ "$1" 2>&1 | tr -d '\r'
}
dump_status() { qmp '{"execute":"query-dump"}' | jq -r '.return.status // empty' 2> /dev/null || true; }

# A GPU release while a detached guest memory dump runs is refused: the dump stopped the
# guest itself and starts it again when it ends, so a release that believed it had paused
# the guest would leave it running without its GPU. The dump writes into a FIFO whose reader
# reads nothing until /tmp/dump.go exists, so it is still running for both refusals, and once
# let go it must complete: the emulator's dump failed on every phone, into a FIFO or a file,
# until it left out the address space device's user-backed area ("failed to save memory: Bad
# address"). A release right after the dump completes leaves the guest paused (see below),
# and after a retake the phone holds its GPU and the app draws. A dump that never ends leaves
# the guest paused, so the run ends there.
release_during_dump() {
  local r out st i rc
  X sh -c 'rm -f /tmp/dump.fifo /tmp/dump.go && mkfifo /tmp/dump.fifo && { setsid sh -c "exec 3< /tmp/dump.fifo; i=0; while [ ! -e /tmp/dump.go ] && [ \$i -lt 1800 ]; do sleep 0.1; i=\$((i + 1)); done; cat <&3 > /dev/null" > /dev/null 2>&1 < /dev/null & }' \
    || { bad "could not make the dump's FIFO"; return; }
  r=$(qmp '{"execute":"dump-guest-memory","arguments":{"paging":false,"protocol":"file:/tmp/dump.fifo","detach":true}}' || true)
  if [ "$(jq -c '.return // empty' <<< "$r" 2> /dev/null)" != "{}" ]; then
    bad "a detached guest memory dump did not start: '$r'"
    X touch /tmp/dump.go || true
    return
  fi
  st=$(dump_status)
  [ "$st" = active ] || bad "the dump is not running when the release asks: '$st'"
  # the console's own pause refuses too, and answers (a refusal with no answer left the
  # client waiting)
  rc=0; r=$(X sh -c 'timeout 10 /android/platform-tools/adb emu avd pause' 2>&1 | tr -d '\r') || rc=$?
  [ "$rc" = 0 ] && [[ "$r" == *"KO: could not pause the guest"* ]] && ok "'avd pause' during a guest memory dump answers KO" \
    || bad "'avd pause' during a guest memory dump: exit $rc, '$r'"
  if out=$(X /usr/local/bin/gpu-hook release 2>&1); then
    bad "a GPU release during a guest memory dump succeeded: $(grep -o 'released_ms=[0-9]*' <<< "$out" || true)"
  else
    [[ "$out" == *"could not pause the guest"* ]] && ok "a GPU release during a guest memory dump is refused: $(grep -m1 KO <<< "$out" | tr -d '\r' || true)" \
      || bad "a GPU release during a guest memory dump failed for another reason: $out"
  fi
  st=$(dump_status)
  [ "$st" = active ] || bad "the dump ended before its reader was let go: '$st'"
  X touch /tmp/dump.go || true
  for i in $(seq 1 120); do
    st=$(dump_status)
    [ "$st" = active ] || break
    sleep 1
  done
  if [ "$st" = active ]; then
    bad "the dump still runs 120 s after its reader was let go, and the guest stays paused: ending the run"
    exit 1
  fi
  [ "$st" = completed ] && ok "the dump completed" || bad "the dump did not complete: '$st'"
  # The dump's end and the guest's restart are one step: phone.sh boot widens the gap
  # between them to 5 s (FCVM_DUMP_RESUME_DELAY_MS), and a release right after the dump
  # reads complete must leave the guest paused. Ending first and restarting later, the dump
  # started the released guest again 5 s after the release.
  if out=$(X /usr/local/bin/gpu-hook release 2>&1); then
    sleep 7
    st=$(qmp '{"execute":"query-status"}' | jq -r '.return.status // empty' 2> /dev/null || true)
    [ "$st" = paused ] && ok "a release right after the dump ends leaves the guest paused" \
      || bad "a release right after the dump ended: the guest is '$st' 7 s later, its GPU released"
    out=$(X /usr/local/bin/gpu-hook retake 2>&1) || bad "the retake after the dump failed: $out"
  else
    bad "a release right after the dump ended failed: $out"
  fi
  st=$(X sh -c '/android/platform-tools/adb emu gpu status | head -1' 2>&1 | tr -d '\r' || true)
  if [ "$st" = held ]; then
    ok "after the dump the phone holds its GPU"
  else
    bad "after the dump the phone's GPU is '$st'"
    X /usr/local/bin/gpu-hook retake > /dev/null 2>&1 || true
  fi
  check_draws "$pid" "after a release refused during a guest memory dump"
}

# VM $1's phone shows the browser in front on the NVIDIA page, after `phone.sh action
# open_browser`; $2 says where. Prints how long the browser took to come to the front.
browser_in_front() {
  local out top title green shown
  if ! out=$("$F" exec --pid "$1" -- /usr/local/bin/phone.sh action open_browser 2>&1); then
    bad "open_browser failed $2: $out"
    return
  fi
  top=$("$F" exec --pid "$1" -- sh -c '/android/platform-tools/adb shell dumpsys activity activities | grep -m1 topResumedActivity' 2>&1 || true)
  [[ "$top" == *org.cromite.cromite/* ]] && ok "the browser is in front $2 ($(grep -o 'shown_ms=[0-9]*' <<< "$out"))" \
    || bad "the browser is not in front $2: $top"
  # Its share groups were restored before it was asked for (gfxstream restores a retake's
  # share groups once the first frame is posted): its first show after a restore took 2.7
  # to 3.5 s while its first frame waited for them, and takes 0.07 to 0.6 s otherwise.
  shown=$(grep -o 'shown_ms=[0-9]*' <<< "$out" | cut -d= -f2 || true)
  [ -n "$shown" ] && [ "$shown" -lt 1000 ] && ok "the browser came to the front $2 in ${shown} ms, under 1 s" \
    || bad "the browser took ${shown:-?} ms to come to the front $2 (1 s at most)"
  title=$("$F" exec --pid "$1" -- /usr/local/bin/phone.sh page 20 2>&1) && ok "the browser shows '$title' $2" \
    || bad "the browser shows no NVIDIA page $2: $title"
  # NVIDIA's green (#76b900) is on screen within 10 s: the page is drawn, not only loaded (the
  # action reloads it, so the first screenshots can catch it between documents)
  local t0 i
  t0=$(date +%s%3N)
  for i in $(seq 1 20); do
    green=$("$F" exec --pid "$1" -- sh -c '/android/platform-tools/adb exec-out screencap' 2> /dev/null | python3 -c '
import sys
d = sys.stdin.buffer.read()[16:]
print(sum(1 for i in range(0, len(d) - 3, 4) if abs(d[i] - 0x76) < 24 and abs(d[i + 1] - 0xb9) < 24 and d[i + 2] < 40))' || echo 0)
    [ "${green:-0}" -ge 400 ] && break
    [ $(( $(date +%s%3N) - t0 )) -lt 10000 ] || break
  done
  [ "${green:-0}" -ge 400 ] && ok "the page is drawn $2: $green pixels of NVIDIA green" || bad "the page is not drawn $2 within 10 s: $green pixels of NVIDIA green"
}
# The proxy the phone reaches at the container's 127.0.0.1:3128 (through the VM's
# --forward-localhost) tunnels to www.nvidia.com and refuses a name off its list.
proxy_boundary() {
  local a b
  a=$(X bash -c 'exec 3<> /dev/tcp/127.0.0.1/3128; printf "CONNECT www.nvidia.com:443 HTTP/1.1\r\nHost: www.nvidia.com\r\n\r\n" >&3; IFS= read -r -t 15 l <&3; echo "$l"' 2>&1 | tr -d '\r' || true)
  b=$(X bash -c 'exec 3<> /dev/tcp/127.0.0.1/3128; printf "CONNECT example.org:443 HTTP/1.1\r\nHost: example.org\r\n\r\n" >&3; IFS= read -r -t 15 l <&3; echo "$l"' 2>&1 | tr -d '\r' || true)
  [[ "$a" == "HTTP/1.1 200"* ]] && ok "the phone web proxy tunnels to www.nvidia.com" || bad "the phone web proxy did not tunnel to www.nvidia.com: '$a'"
  [[ "$b" == "HTTP/1.1 403"* ]] && ok "the phone web proxy refuses a name off its list" || bad "the phone web proxy answered '$b' for example.org"
}

warm_snapshot_onto_the_second_gpu() {
  local source_uuid cpid click ack first clone_uuid clone_name holds renderer
  source_uuid=$(X /usr/bin/nvidia-smi --query-gpu=uuid --format=csv,noheader 2>&1 | head -1) \
    || { bad "nvidia-smi gave no GPU UUID on the source phone: '$source_uuid'"; source_uuid=""; }
  local started=$(date +%s%3N)
  if ! "$F" snapshot create --pid "$pid" --tag "$TAG" --gpu-warm > "$LOGS/snapshot.log" 2>&1; then
    bad "the warm snapshot of the running phone failed: $(grep -a -m1 -E 'Error|error' "$LOGS/snapshot.log") (log $LOGS/snapshot.log)"
    return
  fi
  snapshot_dir=$(grep -a -o -E "/[^ \",]*/snapshots/$TAG" "$LOGS/snapshot.log" | head -1)
  ok "a warm snapshot of the running phone in $(( $(date +%s%3N) - started )) ms"
  check_draws "$pid" "on the source phone after the snapshot"
  click=$(date +%s%3N)
  "$F" snapshot run --snapshot "$TAG" --gpu "$GPU2" --name "$NAME-clone" > "$LOGS/clone.log" 2>&1 < /dev/null &
  clone=$!
  cpid=$(WAIT_HEALTHY_WHILE=$clone wait_healthy "$NAME-clone" 1200 0.1) || cpid=""
  if [ -z "$cpid" ]; then
    bad "the clone on $GPU2 never became healthy: $(ls_error) (log $LOGS/clone.log)"
    return
  fi
  ack=$(grep -a -m1 '"ack_ms"' "$LOGS/clone.log" || true)
  # The hook waited for the phone's first frame on the clone's GPU before the restore was
  # acknowledged; the clone's clock was stepped to the host's at restore. Its log starts with
  # the source phone's, which the snapshot carries up to the release: only a first frame
  # after that release is the clone's. The run keeps the clone's log.
  "$F" exec --pid "$cpid" -- cat /tmp/gpu-hook.log > "$LOGS/out/clone-gpu-hook.log" 2>&1 || true
  first=$(first_frame_after_release "$(cat "$LOGS/out/clone-gpu-hook.log")")
  if [ -n "$first" ]; then
    echo "flow B: click to the app's first frame on the clone's GPU $(( first - click )) ms; restore ack_ms=$(jq -r '.ack_ms' <<< "$ack") gpu_ms=$(jq -r '.guest_phases.gpu_ms' <<< "$ack")"
  fi
  if [ -n "$first" ] && [ "$first" -ge "$click" ]; then
    ok "the clone's phone posted a frame on its GPU before the restore was acknowledged"
  else
    bad "the clone's GPU hook recorded no first frame after the restore (its log: $LOGS/out/clone-gpu-hook.log)"
    keep_phone_logs "$cpid" clone-first-frame
  fi
  clone_uuid=$("$F" exec --pid "$cpid" -- /usr/bin/nvidia-smi --query-gpu=uuid --format=csv,noheader 2>&1 | head -1) || clone_uuid=""
  [ -n "$source_uuid" ] && [ -n "$clone_uuid" ] && [ "$clone_uuid" != "$source_uuid" ] && ok "the clone has another GPU ($clone_uuid, the source $source_uuid)" \
    || bad "the clone's GPU '$clone_uuid' is not another GPU than the source's '$source_uuid'"
  holds=$("$F" exec --pid "$cpid" -- /usr/local/bin/phone.sh holds 2>&1 || true)
  [[ "$holds" =~ ^fds=[1-9] ]] && ok "the clone's emulator holds its GPU: $holds" || bad "the clone's emulator does not hold its GPU: $holds"
  # the clone's own GPU name: its GPU may be another model than the source's
  clone_name=$("$F" exec --pid "$cpid" -- /usr/bin/nvidia-smi --query-gpu=name --format=csv,noheader 2>&1 | head -1) || clone_name=""
  renderer=$("$F" exec --pid "$cpid" -- /usr/local/bin/phone.sh renderer 2>&1 || true)
  [ -n "$clone_name" ] && [[ "$renderer" == *"$clone_name"* ]] && ok "the clone's SurfaceFlinger: $renderer" \
    || bad "the clone's SurfaceFlinger does not render on its GPU '$clone_name': $renderer"
  check_draws "$cpid" "on the clone's GPU"
  # the browser, started before the snapshot, comes to the front in the clone
  browser_in_front "$cpid" "on the clone"
  "$F" exec --pid "$cpid" -- /usr/local/bin/phone.sh stop > /dev/null 2>&1 || true
}

if boot=$(timeout 400 "$F" exec --pid "$pid" -- /usr/local/bin/phone.sh boot 2>&1); then
  ok "Android booted with the host GPU: $boot"
  renderer=$(X /usr/local/bin/phone.sh renderer 2>&1 || true)
  [[ "$renderer" == *"$gpu_name"* ]] && ok "SurfaceFlinger: $renderer" || bad "SurfaceFlinger does not render on '$gpu_name': $renderer"
  out=$(timeout 120 "$F" exec --pid "$pid" -- /usr/local/bin/phone.sh probe 6 "/work/$(basename "$APK")" 2>&1 || true)
  echo "$out" > "$LOGS/probe.log"
  app_renderer=$(grep -m1 -o 'renderer=.* vendor=' <<< "$out" || true)
  [[ "$app_renderer" == *"$gpu_name"* ]] && ok "the app's GLES context: $app_renderer" || bad "the app's renderer does not name '$gpu_name': ${app_renderer:-none} (log $LOGS/probe.log)"
  fps=$(lowest_fps_in "$out" 8)
  at_least_30 "$fps" \
    && ok "the app draws: lowest of its last 8 fps samples $fps" || bad "the app does not draw 30 fps: lowest '${fps:-none}' (log $LOGS/probe.log)"
  reports=$(X /usr/local/bin/phone.sh bugreports 2>&1 || true)
  [ "$reports" = 0 ] && ok "the phone took no bug report" || bad "the phone took $reports bug report(s) (a device failed at boot; dumpstate.last_id)"
  # the browser runs from here on: the releases, the warm snapshot and the clone below all
  # carry it, and a release refuses while a guest holds a Vulkan instance
  if title=$(timeout 120 "$F" exec --pid "$pid" -- /usr/local/bin/phone.sh browser "/work/$(basename "$BROWSER")" 2>&1); then
    ok "the browser opened www.nvidia.com through the phone web proxy: '$title'"
  else
    bad "the browser did not open www.nvidia.com: $title"
  fi
  proxy_boundary
  # the probe app in front again, drawing before the release checks start
  X sh -c '/android/platform-tools/adb shell am start -W -n dev.fcvm.probe/.MainActivity' > /dev/null 2>&1 || true
  check_draws "$pid" "with the browser behind it"
  in_place_release_and_retake
  retake_without_vulkan
  release_during_dump
  warm_snapshot_onto_the_second_gpu
  "$(dirname "$0")/scene-test.sh" "$F" "$pid" "/work/$(basename "$APP")" "$LOGS/out" "$gpu_name" || { fail=1; keep_phone_logs "$pid" scene-test; }
  # The UWB service asked for its bug report a minute or two after boot, after the check
  # above: count again once every check has run.
  reports=$(X /usr/local/bin/phone.sh bugreports 2>&1 || true)
  [ "$reports" = 0 ] && ok "the phone took no bug report during the run" \
    || { bad "the phone took $reports bug report(s) during the run (dumpstate.last_id; logcat)"; keep_phone_logs "$pid" bugreport; }
else
  bad "Android did not boot: $boot"
  X sh -c 'grep -aE "ERROR|FATAL" /tmp/emulator.log | head -20' || true
fi
finished=1
X /usr/local/bin/phone.sh stop > /dev/null 2>&1 || true
stop_runner
echo "logs: $LOGS"
exit $fail
