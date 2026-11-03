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
#   phone.sh bugreports      how many bug reports the phone has taken since boot
#   phone.sh app apk         installs and starts Robot Park (fleet/phone-app), forwards
#                            127.0.0.1:7070 in this container to its control socket, and waits
#                            for its first frame; prints its renderer line
#   phone.sh browser apk     installs the web browser and opens www.nvidia.com in it, through
#                            the phone web proxy; waits for the page's title; prints it
#   phone.sh page [secs]     the NVIDIA page's title once the browser shows it
#   phone.sh ctl json        one request on the app's control socket; prints the reply line
#   phone.sh pull path dest  copies a file off the phone
#   phone.sh shot dest.png   a screenshot of the phone's screen
#   phone.sh applog          the app's log lines so far
#   phone.sh stop
#
# A fleet phone slot runs the container's default command instead:
#
#   phone.sh serve           the phone streamer (fleet/phone/streamer) on :8080, and :443 with
#                            TLS_CERT_FILE, then a cold boot, the browser on www.nvidia.com
#                            ($PHONE_BROWSER_APK, when set) and Robot Park ($PHONE_APK); the
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
# The web browser (docs/fleet/ARCHITECTURE.md, "The browser"): Cromite, a Chromium build for
# x86_64 phones (scripts/phone/browser.sh), on the NVIDIA home page. Its only way out is the
# phone web proxy on the host (src/fleet/agent/webproxy.rs), which the VM forwards to this
# container's loopback and the emulator to the phone's 10.0.2.2. Vulkan off: a guest Vulkan
# instance makes `gpu release` refuse, so no warm snapshot could be taken, and the mirror
# replays GL only.
BROWSER_PKG=org.cromite.cromite
BROWSER_URL=https://www.nvidia.com/
BROWSER_FLAGS="--disable-fre --no-first-run --no-default-browser-check --proxy-server=http://10.0.2.2:${PHONE_WEB_PROXY_PORT:-3128} --disable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE"
DEVTOOLS_PORT=${PHONE_DEVTOOLS_PORT:-9222}
STREAMER_PORT=${PHONE_STREAMER_PORT:-8080}
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
# The NVIDIA driver's thread-local storage library, which the emulator preloads. Every gpu
# retake loads the driver's libraries again, and a library loaded at run time takes its
# initial-exec TLS from glibc's small static TLS surplus, which an unload gives back only when
# it was the last block handed out: on five phones the 29th retake could not load the Vulkan
# driver ("libnvidia-tls.so...: cannot allocate memory in static TLS block"), and the
# emulator aborted. Preloaded, its TLS is in the startup block, and a release never unloads it.
nvidia_tls() {
  local f
  for f in "${PHONE_NVIDIA_LIB_DIR:-/usr/lib/x86_64-linux-gnu}"/libnvidia-tls.so.*; do
    if [ -e "$f" ]; then echo "$f"; return 0; fi
  done
  return 0
}
# UWB off, once the phone has booted. The image declares UWB, so its service turns UWB on at
# boot, but -feature -Uwb gives the phone no UWB HAL: each attempt waits 20 s for the HAL,
# the service retries every 10 s, and a later failure asks for a bug report ("UWB Bugreport:
# error enabling UWB"). dumpstate then takes the phone's CPU for about 30 s, a minute or two
# after boot, and the app's frames drop with it. Off, the service stops trying.
uwb_off() { "$ADB" shell cmd uwb disable-uwb > /dev/null 2>&1; }
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
# One GET on a loopback HTTP port of this container; prints the body of a 200. The response
# is read to its Content-Length: the browser's DevTools server answers HTTP/1.1 only and keeps
# the connection open after it.
http_get() { # port path
  local line len=""
  exec 3<> "/dev/tcp/127.0.0.1/$1" || return 1
  printf 'GET %s HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' "$2" >&3
  IFS= read -r -t 5 line <&3 && [[ "$line" == "HTTP/1."?" 200"* ]] || { exec 3>&-; return 1; }
  while IFS= read -r -t 5 line <&3; do
    line=${line%$'\r'}
    [ -n "$line" ] || break
    case "${line,,}" in content-length:*) len=${line#*:}; len=${len// /} ;; esac
  done
  if [ -n "$len" ]; then timeout 5 head -c "$len" <&3; else timeout 5 cat <&3; fi
  exec 3>&-
}
# The browser's DevTools targets (its remote debugging socket, forwarded into this container).
browser_pages() {
  "$ADB" forward tcp:"$DEVTOOLS_PORT" localabstract:chrome_devtools_remote > /dev/null 2>&1 || return 1
  http_get "$DEVTOOLS_PORT" /json/list | jq -c '[.[] | select(.type == "page")]'
}
# The browser is on www.nvidia.com and the page has a title: prints it.
browser_title() {
  browser_pages 2> /dev/null | jq -er --arg u "$BROWSER_URL" '[.[] | select((.url | startswith($u)) and (.title | test("NVIDIA")))][0].title' 2> /dev/null
}
# The page's title within $1 seconds, or a failure naming what the browser shows.
wait_browser_page() {
  local t
  for _ in $(seq 1 $(( ${1:-60} * 5 ))); do
    t=$(browser_title) && { echo "$t"; return 0; }
    sleep 0.2
  done
  echo "the browser shows no NVIDIA page: $(browser_pages 2> /dev/null | jq -c '[.[] | {url, title}]' 2> /dev/null)"
  return 1
}
# The browser installed with its command line, open on www.nvidia.com; prints its title.
start_browser() {
  "$ADB" install -r -g "$1" > /dev/null 2>&1 || { echo "adb could not install $1"; return 1; }
  # a userdebug image reads Chromium's command line from this file
  "$ADB" shell "echo '_ $BROWSER_FLAGS' > /data/local/tmp/chrome-command-line" || { echo "could not write the browser's command line"; return 1; }
  open_page || return 1
  wait_browser_page "${PHONE_BROWSER_WAIT:-60}"
}
# The browser started on www.nvidia.com in a tab of its own: the other tabs are closed first,
# so a restart does not add one.
open_page() {
  local out id
  for id in $(browser_pages 2> /dev/null | jq -r '.[].id' 2> /dev/null); do
    http_get "$DEVTOOLS_PORT" "/json/close/$id" > /dev/null 2>&1 || true
  done
  "$ADB" shell am force-stop "$BROWSER_PKG" > /dev/null 2>&1 || true
  out=$("$ADB" shell am start -W -a android.intent.action.VIEW -d "$BROWSER_URL" "$BROWSER_PKG" 2>&1 | tr -d '\r') || true
  grep -q '^Status: ok' <<< "$out" || { echo "am start of the browser: $(grep -m2 -E 'Error|Status' <<< "$out" | tr '\n' ' ')"; return 1; }
  echo browser > "$RUN/front"
  sed -n 's/^TotalTime: */shown_ms=/p' <<< "$out" > "$RUN/shown"
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
  tls=$(nvidia_tls)
  [ -n "$tls" ] || fail "no libnvidia-tls among the GPU driver's libraries"
  start_emulator() {
    LD_PRELOAD=$tls GFXSTREAM_MIRROR_SOCKET=${MIRROR_SOCKET:-/tmp/gfxstream-mirror.sock} GFXSTREAM_MAX_GLES_VERSION=${PHONE_MAX_GLES:-3.0} \
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
  uwb_off || fail "could not turn UWB off (cmd uwb disable-uwb)"
  echo "phone.sh: booted in $(( $(ms) - t0 )) ms"
  why=$(renderer_on_gpu "$gpu") || fail "$why"
  # the browser first, then Robot Park in front of it: every clone of the warm snapshot has
  # the page loaded, and showing it is an activity coming to the front
  if [ -n "${PHONE_BROWSER_APK:-}" ]; then
    why=$(start_browser "$PHONE_BROWSER_APK") || fail "the browser did not start: $why"
    echo "phone.sh: the browser shows '$why'"
  fi
  why=$(start_app "$PHONE_APK" "$gpu") || fail "the app did not start: $why"
  echo app > "$RUN/front"
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
  # Which app is in front, Robot Park or the browser: an action for Robot Park brings it to
  # the front first.
  front() { cat "$RUN/front" 2> /dev/null || echo app; }
  to_app() {
    [ "$(front)" = browser ] || return 0
    "$ADB" shell am start -W -n dev.fcvm.robotpark/.MainActivity 2>&1 | tr -d '\r' | grep -q '^Status: ok' \
      || { echo "phone.sh: Robot Park did not come to the front" >&2; exit 1; }
    echo app > "$RUN/front"
  }
  has_browser() { "$ADB" shell pm path "$BROWSER_PKG" 2> /dev/null | grep -q '^package:' || { echo "phone.sh: this phone has no browser" >&2; exit 1; }; }
  # While the viewer's GPU mirror is open, a window must be drawn by a process that started
  # after the mirror's copy did (docs/fleet/ARCHITECTURE.md, "The GPU mirror", point 3): the
  # browser starts again instead of coming to the front.
  mirror_open() { http_get "$STREAMER_PORT" /health 2> /dev/null | jq -e '.mirror.open == true' > /dev/null 2>&1; }
  restart_browser() { open_page > "$RUN/why" || { echo "phone.sh: $(cat "$RUN/why")" >&2; exit 1; }; }
  case "${2:-}" in
  switch_screen) to_app; toggle screen scene info ;;
  rotate) to_app; app_action rotate ;;
  dark_mode) to_app; toggle night light dark ;;
  dance|wave) to_app; app_action "$2" ;;
  gpu_benchmark) to_app; app_action benchmark ;;
  # the app in front exits and starts again; a restarted Robot Park shows its scene in its own
  # light or dark again
  restart_app)
    if [ "$(front)" = browser ]; then restart_browser; else app_action restart; rm -f "$RUN/screen" "$RUN/night"; fi ;;
  open_browser)
    has_browser
    if mirror_open; then
      restart_browser
    else
      # the page loaded when the phone started comes to the front at once, and loads again
      # (Ctrl+R) while it shows
      out=$("$ADB" shell am start -W -a android.intent.action.MAIN -c android.intent.category.LAUNCHER "$BROWSER_PKG" 2>&1 | tr -d '\r')
      grep -q '^Status: ok' <<< "$out" || { echo "phone.sh: the browser did not come to the front: $out" >&2; exit 1; }
      echo browser > "$RUN/front"
      sed -n 's/^TotalTime: */shown_ms=/p' <<< "$out" > "$RUN/shown"
      "$ADB" shell input keycombination 113 46 > /dev/null 2>&1 || { echo "phone.sh: the page was not reloaded" >&2; exit 1; }
    fi
    echo "ok open_browser $(cat "$RUN/shown" 2> /dev/null)"
    exit 0
    ;;
  open_app) to_app ;;
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
  # app's frames dropping under 30 a second). Without the device the UWB service still
  # tries: uwb_off stops it.
  tls=$(nvidia_tls)
  [ -n "$tls" ] || { echo "no libnvidia-tls among the GPU driver's libraries"; exit 1; }
  LD_PRELOAD=$tls setsid "$EMU" -avd phone -no-window -no-audio -no-boot-anim -gpu host -accel on \
    -no-snapshot-load -feature -Uwb -verbose > "$RUN/emulator.log" 2>&1 < /dev/null &
  echo $! > "$RUN/emulator.pid"
  timeout "${BOOT_TIMEOUT:-300}" "$ADB" wait-for-device
  until [ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do
    kill -0 "$(cat "$RUN/emulator.pid")" 2>/dev/null || { echo "the emulator exited:"; grep -aE 'ERROR|FATAL' "$RUN/emulator.log" | head -20; exit 1; }
    [ $(( $(ms) - t0 )) -lt $(( ${BOOT_TIMEOUT:-300} * 1000 )) ] || { echo "no boot_completed within ${BOOT_TIMEOUT:-300} s"; exit 1; }
    sleep 0.2
  done
  uwb_off || { echo "could not turn UWB off (cmd uwb disable-uwb)"; exit 1; }
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
  # dumpstate numbers each bug report it takes since boot (dumpstate.last_id, unset before the
  # first), however it was asked for. The events log shows the shell app starting for one,
  # which it does not when the app is already running. A phone adb cannot ask fails.
  id=$("$ADB" shell getprop dumpstate.last_id) || { echo "phone.sh: adb could not read dumpstate.last_id" >&2; exit 1; }
  id=$(tr -d '\r' <<< "$id")
  echo "${id:-0}"
  ;;
app)
  # a fresh install: another build host signs with another debug key
  "$ADB" uninstall dev.fcvm.robotpark > /dev/null 2>&1 || true
  "$ADB" install "${2:?usage: phone.sh app apk}" > /dev/null
  # the app hides the system bars; without this Android covers the scene with a one-time hint
  "$ADB" shell settings put secure immersive_mode_confirmations confirmed
  "$ADB" logcat -c
  "$ADB" shell am start -W -n dev.fcvm.robotpark/.MainActivity | tr -d '\r' | grep -E '^(Status|TotalTime)'
  echo app > "$RUN/front"
  "$ADB" forward tcp:"$CTL_PORT" localabstract:dev.fcvm.robotpark > /dev/null
  for _ in $(seq 1 100); do
    line=$("$ADB" logcat -d -s ROBOTPARK:I | tr -d '\r' | grep -m1 'fps=' || true)
    [ -n "$line" ] && break
    sleep 0.2
  done
  [ -n "$line" ] || { echo "Robot Park drew no frame in 20 s:"; "$ADB" logcat -d | tr -d '\r' | grep -E 'ROBOTPARK|AndroidRuntime|FATAL' | tail -30; exit 1; }
  "$ADB" logcat -d -s ROBOTPARK:I | tr -d '\r' | grep -m1 -o 'renderer=.*'
  ;;
browser)
  start_browser "${2:?usage: phone.sh browser apk}"
  ;;
page)
  # the title of the NVIDIA page the browser shows, or a failure naming what it shows
  wait_browser_page "${2:-10}"
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
  echo "usage: phone.sh serve|action NAME|boot|renderer|probe secs apk|draws secs|holds|bugreports|app apk|browser apk|page [secs]|ctl json|pull path dest|shot dest.png|applog|stop" >&2
  exit 2
  ;;
esac
