#!/bin/bash
# The phone inside a phone VM: an Android Emulator whose GPU rendering goes to the VM's
# NVIDIA GPU. Runs in the fleet/phone container, which fcvm starts privileged (for
# /dev/kvm and the bind mounts below) with the GPU's CDI devices, the SDK mapped at /sdk
# and the test's files at /work. scripts/phone/run.sh drives it:
#
#   phone.sh boot            cold boot; prints boot_completed_ms
#   phone.sh renderer        prints the GLES renderer SurfaceFlinger composes with
#   phone.sh probe secs apk  installs and starts the probe app, prints its log lines
#   phone.sh draws secs      the probe app's log lines over the next secs seconds
#   phone.sh holds           the emulator's /dev/nvidia* descriptors and mappings
#   phone.sh stop
set -euo pipefail
export ANDROID_SDK_ROOT=/sdk ANDROID_HOME=/sdk ANDROID_AVD_HOME=/root/.android/avd
# gfxstream's EGL-on-EGL engine: the host side renders through EGL (NVIDIA's, through glvnd)
# with no X display. Without it the emulator's "host" mode on Linux opens GLX and fails
# headless ("GlxEngine getDefaultDisplay: Failed to open display").
export ANDROID_EGL_ON_EGL=1
ADB=/sdk/platform-tools/adb
# The emulator built from source (scripts/phone/emulator.sh), mapped at /emu: the SDK's may
# not be modified, and the phone needs the console's gpu release and gpu retake.
EMU_DIR=${EMU_DIR:-/emu}
EMU=$EMU_DIR/emulator
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
  setsid "$EMU" -avd phone -no-window -no-audio -no-boot-anim -gpu host -accel on \
    -no-snapshot-load -verbose > /tmp/emulator.log 2>&1 < /dev/null &
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
stop)
  "$ADB" emu kill > /dev/null 2>&1 || true
  ;;
*)
  echo "usage: phone.sh boot|renderer|probe secs apk|draws secs|holds|stop" >&2
  exit 2
  ;;
esac
