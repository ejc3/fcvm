#!/bin/bash
# validate.sh <outdir> [secs]: an independent count of Robot Park's traffic, from inside the guest.
# The app is stopped (SIGSTOP), strace attaches to every thread, the app runs for <secs>
# under strace, and is stopped again; strace records every read and write on its
# goldfish pipe descriptors. The host's counters for the app's puid over the same span are
# compared offline (validate.py). Markers go to <outdir>/validate.markers.jsonl.
set -uo pipefail
ADB=${ADB:-/android/platform-tools/adb}
OUT=${1:?outdir}; SECS=${2:-30}
M=$OUT/validate.markers.jsonl; : > "$M"
S() { "$ADB" shell "$@" | tr -d '\r'; }
ms() { date +%s%3N; }
mark() { printf '{"ev":"%s","t_ms":%s%s}\n' "$1" "$(ms)" "${2:+,$2}" >> "$M"; }
"$ADB" root > /dev/null; sleep 2; "$ADB" wait-for-device
S which strace || { echo "no strace in the guest"; exit 1; }
pid=$(S pidof dev.fcvm.robotpark)
[ -n "$pid" ] || { echo "Robot Park is not running"; exit 1; }
S rm -f /data/local/tmp/st.txt
S kill -STOP "$pid"; mark stopped "\"pid\":$pid"
sleep 3
S "nohup strace -f -qq -ttt -y -s 0 -e trace=read,write,readv,writev -e signal=none -o /data/local/tmp/st.txt -p $pid > /dev/null 2>&1 &"
sleep 3
mark cont; S kill -CONT "$pid"
sleep "$SECS"
S kill -STOP "$pid"; mark stopped_again
sleep 3
S pkill -INT strace; sleep 3
S kill -CONT "$pid"; mark resumed
# the guest clock against ours: our time before and after one guest date
for i in 1 2 3 4 5; do
  a=$(ms); g=$(S date +%s%N); b=$(ms)
  printf '{"ev":"clock%s","t_ms":%s,"before":%s,"after":%s,"guest_ns":%s}\n' "$i" "$b" "$a" "$b" "$g" >> "$M"
done
S cat /proc/"$pid"/status | grep -E '^(Name|Threads)'
"$ADB" pull /data/local/tmp/st.txt "$OUT/validate.strace.txt" > /dev/null
wc -l "$OUT/validate.strace.txt"
"$ADB" unroot > /dev/null 2>&1 || true
