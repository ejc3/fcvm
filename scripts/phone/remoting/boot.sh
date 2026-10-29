#!/bin/bash
# boot.sh <mode> [emulator args...]: (re)boots the phone with the counters on, into /bench/out/<mode>
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
X=$B/scripts/x.sh
mode=$1; shift
mkdir -p $B/out/$mode
$X /usr/local/bin/phone.sh stop > /dev/null 2>&1
for _ in $(seq 1 60); do $X pgrep -f qemu-system > /dev/null 2>&1 || break; sleep 1; done
$X env PHONE_RAM_MIB=4096 PHONE_EMU_ARGS="-grpc 8554 $*" GFXSTREAM_STATS=/bench/out/$mode/stats.jsonl \
  GFXSTREAM_STATS_DUMP=/bench/out/$mode/dump.bin GFXSTREAM_STATS_DUMP_BYTES=1500000000 /usr/local/bin/phone.sh boot
A=/android/platform-tools/adb
$X $A shell settings put global stay_on_while_plugged_in 7
$X $A shell settings put system screen_off_timeout 1800000
$X $A shell settings put secure immersive_mode_confirmations confirmed
$X $A shell getprop debug.hwui.renderer
$X sh -c 'grep -aE "gfxstream|Vulkan|GLES" /tmp/emulator.log | head -5'
