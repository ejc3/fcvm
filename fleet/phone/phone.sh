#!/bin/bash
# The phone inside a phone VM: an Android Emulator whose GPU rendering goes to the VM's
# NVIDIA GPU. Runs in the fleet/phone container, which fcvm starts privileged (for
# /dev/kvm and the bind mounts below) with the GPU's CDI devices, the system image and adb
# built from AOSP source mapped at /android (scripts/phone/aosp.sh) and the test's files at
# /work. scripts/phone/run.sh drives it:
#
#   phone.sh boot            cold boot; prints boot_completed_ms
#   phone.sh renderer        prints the GLES renderer SurfaceFlinger composes with
#   phone.sh probe secs apk  installs and starts the probe app, prints its log lines
#   phone.sh draws secs      the probe app's log lines over the next secs seconds
#   phone.sh holds           the emulator's /dev/nvidia* descriptors and mappings
#   phone.sh bugreports      how many bug reports the system has asked for since boot
#   phone.sh app apk         installs and starts Robot Park (fleet/phone-app), forwards
#                            127.0.0.1:7070 in this container to its control socket, and waits
#                            for its first frame; prints its renderer line
#   phone.sh ctl json        one request on the app's control socket; prints the reply line
#   phone.sh pull path dest  copies a file off the phone
#   phone.sh shot dest.png   a screenshot of the phone's screen
#   phone.sh applog          the app's log lines so far
#   phone.sh stop
#
# A fleet phone slot runs the container's default command instead:
#
#   phone.sh serve           the phone streamer (fleet/phone/streamer) on :8080, and :443 with
#                            TLS_CERT_FILE, then a cold boot and Robot Park ($PHONE_APK); the
#                            streamer streams once SurfaceFlinger and the app render on the
#                            VM's GPU. Exits when the emulator does. A fleet slot runs it once,
#                            for the warm snapshot every phone is a clone of (flow B).
set -euo pipefail
# The emulator finds the system image under ANDROID_SDK_ROOT; nothing in it is from the SDK.
export ANDROID_SDK_ROOT=/android ANDROID_HOME=/android ANDROID_AVD_HOME=${PHONE_AVD_HOME:-/root/.android/avd}
# gfxstream's EGL-on-EGL engine: the host side renders through EGL (NVIDIA's, through glvnd)
# with no X display. Without it the emulator's "host" mode on Linux opens GLX and fails
# headless ("GlxEngine getDefaultDisplay: Failed to open display").
export ANDROID_EGL_ON_EGL=1
# scripts/phone/test-phone-sh.sh points PHONE_ADB at a stub.
ADB=${PHONE_ADB:-/android/platform-tools/adb}
# The emulator built from source (scripts/phone/emulator.sh), mapped at /emu: the phone needs
# the console's gpu release and gpu retake.
EMU_DIR=${EMU_DIR:-/emu}
EMU=$EMU_DIR/emulator
CTL_PORT=${PHONE_CTL_PORT:-7070}
# serve's programs and files (scripts/phone/test-phone-sh.sh points them at stand-ins)
STREAMER=${PHONE_STREAMER:-/usr/local/bin/phone-streamer}
KVM=${PHONE_KVM:-/dev/kvm}
# the emulator's pid and log, and the streamer's hold file: the GPU hook reads them here
RUN=${PHONE_RUN:-/tmp}
# The streamer streams nothing until this file exists: serve writes it once the phone
# renders on the VM's GPU, so a phone rendering in software is never shown.
export PHONE_GO_FILE=${PHONE_GO_FILE:-/run/phone-verified}
AVD=$ANDROID_AVD_HOME/phone.avd
ms() { date +%s%3N; }

make_avd() {
  mkdir -p "$AVD"
  printf 'avd.ini.encoding=UTF-8\npath=%s\ntarget=android-35\n' "$AVD" > "$ANDROID_AVD_HOME/phone.ini"
  cat > "$AVD/config.ini" <<CONFIG
avd.ini.encoding=UTF-8
abi.type=x86_64
hw.cpu.arch=x86_64
hw.cpu.ncore=4
hw.ramSize=${PHONE_RAM_MIB:-2048}
image.sysdir.1=system-images/android-35/default/x86_64/
tag.id=default
hw.gpu.enabled=yes
hw.gpu.mode=host
hw.lcd.width=720
hw.lcd.height=1280
hw.lcd.density=320
hw.keyboard=yes
disk.dataPartition.size=6G
PlayStore.enabled=no
CONFIG
}

# The emulator dlopens libEGL.so and libGLESv2.so from its own SwiftShader directory
# before the system's, so EGL-on-EGL would render in software. Masking those three files
# for this container (a bind mount of /dev/null, the mapped build is not changed) makes the
# lookup fall through to libEGL.so.1 and libGLESv2.so.2, which glvnd sends to NVIDIA.
use_system_egl() {
  local dir=$EMU_DIR/lib64/gles_swiftshader lib
  for lib in libEGL.so libGLESv2.so libGLES_CM.so; do
    [ -e "$dir/$lib" ] || continue
    mountpoint -q "$dir/$lib" || mount --bind /dev/null "$dir/$lib"
  done
}

gpu_name() { nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1; }
# SurfaceFlinger composes with GLES on the GPU named $1.
renderer_on_gpu() {
  local r
  r=$("$ADB" shell dumpsys SurfaceFlinger 2>/dev/null | tr -d '\r' | grep -m1 '^GLES:' || true)
  [[ -n "$1" && "$r" == *"$1"* ]] || { echo "SurfaceFlinger does not render on ${1:-the GPU}: ${r:-no GLES line}"; return 1; }
}
# Robot Park installed and started, its control socket forwarded to 127.0.0.1:$CTL_PORT, and
# its first frame drawn with a renderer on the GPU named $2; prints why not.
start_app() {
  local out line
  "$ADB" uninstall dev.fcvm.robotpark > /dev/null 2>&1 || true
  "$ADB" install "$1" > /dev/null 2>&1 || { echo "adb could not install $1"; return 1; }
  # the app hides the system bars; without this Android covers the scene with a one-time hint
  "$ADB" shell settings put secure immersive_mode_confirmations confirmed > /dev/null 2>&1 || true
  "$ADB" logcat -c > /dev/null 2>&1 || true
  out=$("$ADB" shell am start -W -n dev.fcvm.robotpark/.MainActivity 2>&1 | tr -d '\r') || true
  grep -q '^Status: ok' <<< "$out" || { echo "am start: $(grep -m2 -E 'Error|Status' <<< "$out" | tr '\n' ' ')"; return 1; }
  "$ADB" forward tcp:"$CTL_PORT" localabstract:dev.fcvm.robotpark > /dev/null || { echo "adb forward failed"; return 1; }
  for _ in $(seq 1 100); do
    line=$("$ADB" logcat -d -s ROBOTPARK:I 2>/dev/null | tr -d '\r' | grep -m1 -o 'renderer=.*' || true)
    [ -n "$line" ] && break
    sleep 0.2
  done
  [ -n "$line" ] || { echo "Robot Park drew no frame in 20 s"; return 1; }
  [[ "$line" == *"$2"* ]] || { echo "Robot Park does not render on $2: $line"; return 1; }
}
# The streamer, restarted whenever it exits, unless the GPU hook holds it (while the phone's
# GPU is released for a warm snapshot, nothing in the container may hold the GPU). The hold
# check and the start happen under streamer.lock, which the hook takes to set the hold, so
# the hook either sees the streamer's pid in streamer.pid or the loop sees the hold.
streamer_loop() {
  local pid
  exec 8> "$RUN/streamer.lock"
  while :; do
    flock 8
    if [ -e "$RUN/streamer.hold" ]; then
      flock -u 8
      sleep 0.1
      continue
    fi
    "$STREAMER" serve 8>&- &
    pid=$!
    echo "$pid" > "$RUN/streamer.pid"
    flock -u 8
    wait "$pid" || true
    sleep 0.2
  done
}
# A process that is running: a zombie (exited, not yet reaped, which happens to an orphan in
# a container whose first process reaps nothing) is not.
alive() { local st; st=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1) && [ -n "$st" ] && [ "$st" != Z ]; }
any_alive() { local p; for p in $(pgrep -x "$1"); do alive "$p" && return 0; done; return 1; }
loop_pid=""
stop_streamer() {
  [ -n "$loop_pid" ] || return 0
  kill "$loop_pid" 2>/dev/null || true
  wait "$loop_pid" 2>/dev/null || true
  loop_pid=""
  local pid
  pid=$(cat "$RUN/streamer.pid" 2>/dev/null || true)
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  pkill -x "$(basename "$STREAMER")" 2>/dev/null || true
  # its listeners are closed before failure mode binds them
  while { [ -n "$pid" ] && alive "$pid"; } || any_alive "$(basename "$STREAMER")"; do sleep 0.05; done
}
# The emulator this serve started, stopped: a phone reported failed holds no GPU.
stop_emulator() {
  local p
  p=$(cat "$RUN/emulator.pid" 2>/dev/null) || return 0
  kill "$p" 2>/dev/null || return 0
  for _ in $(seq 1 100); do kill -0 "$p" 2>/dev/null || return 0; sleep 0.1; done
  kill -9 "$p" 2>/dev/null || true
}
# A phone that cannot render on its GPU is reported failed, never shown in software: the
# streamer is replaced by one that serves only a /health naming the failure.
fail() {
  echo "phone.sh: $*" >&2
  stop_emulator
  stop_streamer
  exec "$STREAMER" fail "$*"
}

case "${1:-}" in
serve)
  t0=$(ms)
  rm -f "$PHONE_GO_FILE" "$RUN/streamer.hold" "$RUN/emulator.pid" "$RUN/streamer.pid"
  streamer_loop &
  loop_pid=$!
  trap stop_streamer EXIT
  [ -c "$KVM" ] || fail "no /dev/kvm in the phone container"
  gpu=$(gpu_name || true)
  [ -n "$gpu" ] || fail "nvidia-smi names no GPU"
  [ -n "${PHONE_APK:-}" ] || fail "no app: PHONE_APK is not set"
  make_avd || fail "could not write the AVD under $ANDROID_AVD_HOME"
  use_system_egl || fail "could not mask the emulator's software EGL"
  "$ADB" start-server > /dev/null 2>&1 || true
  # -grpc: the emulator's controller on this container's loopback, whose screen stream the
  # streamer reads. -feature -Uwb: as boot. The GPU mirror (fleet/PROTOCOL.md, "Phone
  # streamer", GET /mirror): gfxstream copies the guest's command stream to the streamer on
  # this socket while a mirror is open, and offers the guest no more than GLES 3.0, which is
  # what the viewer's WebGL2 can replay. The guest reads its version at boot, so the warm
  # snapshot every phone is a clone of carries the cap.
  start_emulator() {
    GFXSTREAM_MIRROR_SOCKET=${MIRROR_SOCKET:-/tmp/gfxstream-mirror.sock} GFXSTREAM_MAX_GLES_VERSION=${PHONE_MAX_GLES:-3.0} \
    setsid "$EMU" -avd phone -no-window -no-audio -no-boot-anim -gpu host -accel on -grpc 8554 \
      -no-snapshot-save -feature -Uwb -verbose "$@" > "$RUN/emulator.log" 2>&1 < /dev/null &
    echo $! > "$RUN/emulator.pid"
  }
  wait_booted() {
    local t1
    t1=$(ms)
    until [ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do
      kill -0 "$(cat "$RUN/emulator.pid")" 2>/dev/null || fail "the emulator exited: $(grep -aE 'ERROR|FATAL' "$RUN/emulator.log" | tail -3 | tr '\n' ' ')"
      [ $(( $(ms) - t1 )) -lt $(( ${BOOT_TIMEOUT:-300} * 1000 )) ] || fail "no boot_completed within ${BOOT_TIMEOUT:-300} s"
      sleep 0.2
    done
  }
  start_emulator -no-snapshot-load
  wait_booted
  echo "phone.sh: booted in $(( $(ms) - t0 )) ms"
  why=$(renderer_on_gpu "$gpu") || fail "$why"
  why=$(start_app "$PHONE_APK" "$gpu") || fail "the app did not start: $why"
  # the phone renders on its GPU: the streamer may stream it
  touch "$PHONE_GO_FILE"
  echo "phone.sh: streaming $(( $(ms) - t0 )) ms after the container started"
  while kill -0 "$(cat "$RUN/emulator.pid")" 2>/dev/null; do sleep 1; done
  rm -f "$PHONE_GO_FILE"
  echo "phone.sh: the emulator exited" >&2
  grep -aE 'ERROR|FATAL' "$RUN/emulator.log" | tail -20 >&2
  exit 1
  ;;
action)
  # The take-over actions (fleet/PROTOCOL.md, "Phones mode"): each name maps to one fixed
  # request on Robot Park's control socket, and nothing a viewer sends reaches the phone but
  # the name. Which screen shows and whether the phone is dark are this container's to
  # remember: the actions are toggles.
  app_action() { # name: one request; fails unless the app answers a JSON object with no "error"
    local reply
    reply=$("$0" ctl "{\"type\":\"action\",\"name\":\"$1\"}") || { echo "phone.sh: the app did not answer" >&2; exit 1; }
    jq -e 'type == "object" and (has("error") | not)' > /dev/null 2>&1 <<< "$reply" \
      || { echo "phone.sh: the app refused $1: $reply" >&2; exit 1; }
  }
  toggle() { # file, first value, second value: sends the other value, kept once the app took it
    local next
    if [ "$(cat "$RUN/$1" 2>/dev/null || echo "$2")" = "$2" ]; then next=$3; else next=$2; fi
    app_action "$next"
    echo "$next" > "$RUN/$1"
  }
  case "${2:-}" in
  switch_screen) toggle screen scene info ;;
  rotate) app_action rotate ;;
  dark_mode) toggle night light dark ;;
  dance|wave) app_action "$2" ;;
  gpu_benchmark) app_action benchmark ;;
  # a restarted app shows its scene in its own light or dark again
  restart_app) app_action restart; rm -f "$RUN/screen" "$RUN/night" ;;
  *)
    echo "phone.sh: unknown action '${2:-}'" >&2
    exit 2
    ;;
  esac
  echo "ok ${2}"
  ;;
boot)
  make_avd
  use_system_egl
  "$ADB" start-server > /dev/null 2>&1
  t0=$(ms)
  # -feature -Uwb: the AOSP image turns UWB on (its advancedFeatures.ini), and without an
  # emulated UWB peer the guest's UWB stack reports a device error at boot, which makes
  # system_server take a full bug report (dumpstate at 100% of a core for about 30 s, the
  # app's frames dropping under 30 a second).
  setsid "$EMU" -avd phone -no-window -no-audio -no-boot-anim -gpu host -accel on \
    -no-snapshot-load -feature -Uwb -verbose > /tmp/emulator.log 2>&1 < /dev/null &
  echo $! > /tmp/emulator.pid
  timeout "${BOOT_TIMEOUT:-300}" "$ADB" wait-for-device
  until [ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do
    kill -0 "$(cat /tmp/emulator.pid)" 2>/dev/null || { echo "the emulator exited:"; grep -aE 'ERROR|FATAL' /tmp/emulator.log | head -20; exit 1; }
    [ $(( $(ms) - t0 )) -lt $(( ${BOOT_TIMEOUT:-300} * 1000 )) ] || { echo "no boot_completed within ${BOOT_TIMEOUT:-300} s"; exit 1; }
    sleep 0.2
  done
  echo "boot_completed_ms=$(( $(ms) - t0 ))"
  ;;
renderer)
  "$ADB" shell dumpsys SurfaceFlinger | tr -d '\r' | grep -m1 '^GLES:'
  ;;
probe)
  "$ADB" install -r "${3:?usage: phone.sh probe secs apk}" > /dev/null
  "$ADB" logcat -c
  "$ADB" shell am start -W -n dev.fcvm.probe/.MainActivity | tr -d '\r' | grep -E '^(Status|TotalTime)'
  sleep "${2:-5}"
  "$ADB" logcat -d -s PHONEPROBE:I | tr -d '\r' | grep PHONEPROBE
  ;;
draws)
  "$ADB" logcat -c
  sleep "${2:-4}"
  "$ADB" logcat -d -s PHONEPROBE:I | tr -d '\r' | grep PHONEPROBE
  ;;
holds)
  q=$(pgrep -f qemu-system-x86_64 | head -1)
  [ -n "$q" ] || { echo "no emulator running"; exit 1; }
  echo "fds=$(find /proc/"$q"/fd -lname '/dev/nvidia*' 2> /dev/null | wc -l) mappings=$(grep -c /dev/nvidia /proc/"$q"/maps || true)"
  ;;
bugreports)
  # A log that cannot be read fails; a log with no bug report prints 0.
  log=$("$ADB" logcat -d -b events) || { echo "phone.sh: adb could not read the events log" >&2; exit 1; }
  tr -d '\r' <<< "$log" | grep -c 'BugreportRequestedReceiver' || true
  ;;
app)
  # a fresh install: another build host signs with another debug key
  "$ADB" uninstall dev.fcvm.robotpark > /dev/null 2>&1 || true
  "$ADB" install "${2:?usage: phone.sh app apk}" > /dev/null
  # the app hides the system bars; without this Android covers the scene with a one-time hint
  "$ADB" shell settings put secure immersive_mode_confirmations confirmed
  "$ADB" logcat -c
  "$ADB" shell am start -W -n dev.fcvm.robotpark/.MainActivity | tr -d '\r' | grep -E '^(Status|TotalTime)'
  "$ADB" forward tcp:"$CTL_PORT" localabstract:dev.fcvm.robotpark > /dev/null
  for _ in $(seq 1 100); do
    line=$("$ADB" logcat -d -s ROBOTPARK:I | tr -d '\r' | grep -m1 'fps=' || true)
    [ -n "$line" ] && break
    sleep 0.2
  done
  [ -n "$line" ] || { echo "Robot Park drew no frame in 20 s:"; "$ADB" logcat -d | tr -d '\r' | grep -E 'ROBOTPARK|AndroidRuntime|FATAL' | tail -30; exit 1; }
  "$ADB" logcat -d -s ROBOTPARK:I | tr -d '\r' | grep -m1 -o 'renderer=.*'
  ;;
ctl)
  # The control protocol is one JSON line each way (fleet/PROTOCOL.md, "Phone app").
  exec 3<> "/dev/tcp/127.0.0.1/$CTL_PORT"
  printf '%s\n' "${2:?usage: phone.sh ctl json}" >&3
  IFS= read -r -t "${CTL_TIMEOUT:-60}" reply <&3 || { echo "no reply from the app" >&2; exit 1; }
  exec 3>&-
  echo "$reply"
  ;;
pull)
  "$ADB" pull "${2:?usage: phone.sh pull path dest}" "${3:?usage: phone.sh pull path dest}" > /dev/null
  ;;
shot)
  "$ADB" exec-out screencap -p > "${2:?usage: phone.sh shot dest.png}"
  ;;
applog)
  "$ADB" logcat -d -s ROBOTPARK:I | tr -d '\r' | grep ROBOTPARK
  ;;
stop)
  "$ADB" emu kill > /dev/null 2>&1 || true
  ;;
*)
  echo "usage: phone.sh serve|action NAME|boot|renderer|probe secs apk|draws secs|holds|bugreports|app apk|ctl json|pull path dest|shot dest.png|applog|stop" >&2
  exit 2
  ;;
esac
