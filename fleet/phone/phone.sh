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
set -euo pipefail
# The emulator finds the system image under ANDROID_SDK_ROOT; nothing in it is from the SDK.
export ANDROID_SDK_ROOT=/android ANDROID_HOME=/android ANDROID_AVD_HOME=/root/.android/avd
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
CTL_PORT=7070
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

case "${1:-}" in
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
  echo "usage: phone.sh boot|renderer|probe secs apk|draws secs|holds|bugreports|app apk|ctl json|pull path dest|shot dest.png|applog|stop" >&2
  exit 2
  ;;
esac
