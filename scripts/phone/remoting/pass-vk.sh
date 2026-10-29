#!/bin/bash
# pass-vk.sh: after post.sh glforced, a boot with HWUI on Vulkan and the HWUI workloads
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
mode=hwuivk
while ! grep -q "POST DONE" $B/logs/post-glforced.log 2>/dev/null; do sleep 10; done
mkdir -p $B/out/$mode
exec > $B/logs/pass-$mode.log 2>&1
cp $B/scripts/wl.sh $B/wl-$mode.sh
$B/scripts/boot.sh "$mode" -prop debug.hwui.renderer=skiavk
$B/scripts/x.sh /android/platform-tools/adb shell setprop debug.hwui.renderer skiavk
# processes started before the property: restart SystemUI and the launcher
$B/scripts/x.sh /android/platform-tools/adb shell pkill -f com.android.systemui
sleep 20
for w in launcher settings shade wikipedia; do
  echo "$(date +%T) $w load $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a $B/out/$mode/load.txt
  $B/scripts/x.sh env DUMP=/bench/out/$mode/dump.bin /bench/wl-$mode.sh run "$w" "/bench/out/$mode"
done
$B/scripts/x.sh sync
echo PASS DONE
