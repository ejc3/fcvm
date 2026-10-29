#!/bin/bash
# The phone remoting workloads, run inside the phone container (adb to the emulator).
#
#   wl.sh run <workload> <outdir>   one workload: launch (startup), then a 60 s scripted window
#                                    with the screen captured and 10 s of the stream dumped
#   wl.sh list                       the workloads
#
# Markers (one JSON line each, wall clock ms) go to <outdir>/<workload>.markers.jsonl; the
# emulator's stats file is the caller's. DUMP names the emulator's GFXSTREAM_STATS_DUMP.
set -uo pipefail
ADB=${ADB:-/android/platform-tools/adb}
STREAMER=${STREAMER:-/usr/local/bin/phone-streamer}
STEADY=${STEADY:-60}
S() { "$ADB" shell "$@" | tr -d '\r'; }
ms() { date +%s%3N; }
mark() { printf '{"ev":"%s","t_ms":%s%s}\n' "$1" "$(ms)" "${2:+,$2}" >> "$M"; }
swipe() { S input swipe "$@" > /dev/null; }
tap() { S input tap "$@" > /dev/null; }
key() { S input keyevent "$@" > /dev/null; }
# runs "$@" repeatedly until the steady window ends
until_end() { while [ "$(ms)" -lt "$END" ]; do "$@"; done; }

# Each workload: pkg (for gfxinfo), launch (prints am start output), and a step for the window.
WORKLOADS="robotpark launcher settings shade firefox_article firefox_images wikipedia maps vlc game"
URL_ARTICLE=https://en.wikipedia.org/wiki/Android_%28operating_system%29
URL_IMAGES=https://commons.wikimedia.org/wiki/Commons:Featured_pictures/Animals/Mammals
WIKI_ARTICLE=https://en.wikipedia.org/wiki/Moon

am_view() { # url, type, component or package
  if [[ "$3" == */* ]]; then S am start -W -a android.intent.action.VIEW -d "$1" ${2:+-t "$2"} -n "$3"
  else S am start -W -a android.intent.action.VIEW -d "$1" ${2:+-t "$2"} "$3"; fi
}
scroll_page() { # ten flings down, ten up
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do swipe 360 1000 360 350 300; sleep 0.6; [ "$(ms)" -lt "$END" ] || return; done
  for i in 1 2 3 4 5 6 7 8 9 10; do swipe 360 350 360 1000 300; sleep 0.6; [ "$(ms)" -lt "$END" ] || return; done
}

pkg_of() {
  case "$1" in
  robotpark) echo dev.fcvm.robotpark ;;
  launcher) echo com.android.launcher3 ;;
  settings) echo com.android.settings ;;
  shade) echo com.android.systemui ;;
  firefox_*) echo org.mozilla.fennec_fdroid ;;
  wikipedia) echo org.wikipedia ;;
  maps) echo app.organicmaps ;;
  vlc) echo org.videolan.vlc ;;
  game) echo org.supertuxkart.stk ;;
  esac
}

launch() {
  case "$1" in
  robotpark) S am start -W -n dev.fcvm.robotpark/.MainActivity ;;
  launcher)
    S am force-stop com.android.launcher3
    S am start -W -a android.intent.action.MAIN -c android.intent.category.HOME ;;
  settings) S am start -W -n com.android.settings/.Settings ;;
  shade) key HOME; echo "Status: ok (no launch)" ;;
  firefox_article) am_view "$URL_ARTICLE" "" org.mozilla.fennec_fdroid/org.mozilla.fenix.IntentReceiverActivity ;;
  firefox_images) am_view "$URL_IMAGES" "" org.mozilla.fennec_fdroid/org.mozilla.fenix.IntentReceiverActivity ;;
  wikipedia) am_view "$WIKI_ARTICLE" "" org.wikipedia ;;
  maps) am_view "geo:39.7285,-121.8375?z=15" "" app.organicmaps ;;
  vlc) S am start -W -a android.intent.action.VIEW -d file:///sdcard/Movies/bbb720.mp4 -t video/mp4 --ez from_start true -n org.videolan.vlc/.gui.video.VideoPlayerActivity ;;
  game) S am start -W -n org.supertuxkart.stk/.SuperTuxKartActivity ;;
  esac
}

# after the launch's settle time: what each app needs before its window
post_launch() {
  case "$1" in
  maps) tap 663 895; sleep 1 ;;   # close the place card the geo intent opens
  game)                           # SuperTuxKart (landscape 1280x720): a Novice race on Nessie's Pond
    tap 408 470; sleep 3          # Singleplayer
    tap 1203 590; sleep 3         # Tux, continue
    tap 185 215; sleep 1          # Novice
    tap 215 505; sleep 3          # Normal Race
    tap 729 427; sleep 2          # Nessie's Pond
    tap 1068 620; sleep 20 ;;     # Start Race: load and countdown
  esac
}

step() {
  case "$1" in
  robotpark|vlc) sleep 1 ;;
  launcher)
    swipe 360 1150 360 300 250; sleep 1                            # open the app drawer
    swipe 360 1000 360 400 300; sleep 0.7; swipe 360 400 360 1000 300; sleep 0.7
    key HOME; sleep 1
    swipe 650 700 70 700 250; sleep 0.8; swipe 70 700 650 700 250; sleep 0.8 ;;  # home pages
  settings)
    local i
    for i in 1 2 3; do swipe 360 1000 360 350 300; sleep 0.6; done
    for i in 1 2 3; do swipe 360 350 360 1000 300; sleep 0.6; done ;;
  shade)
    swipe 360 2 360 700 300; sleep 1       # notifications
    swipe 360 2 360 900 300; sleep 1       # quick settings
    swipe 360 1250 360 100 250; sleep 1 ;; # collapse
  firefox_*|wikipedia) scroll_page ;;
  maps)                                    # pan four ways, zoom in twice and out twice (buttons)
    swipe 200 640 520 640 400; sleep 0.8; swipe 360 400 360 900 400; sleep 0.8
    swipe 520 640 200 640 400; sleep 0.8; swipe 360 900 360 400 400; sleep 0.8
    tap 656 384; sleep 1; tap 656 384; sleep 1; tap 656 496; sleep 1; tap 656 496; sleep 1 ;;
  game) swipe 205 512 100 512 1500; sleep 0.3; swipe 205 512 310 512 1500; sleep 0.3 ;;  # steer left, right
  esac
}

case "${1:-}" in
list) echo "$WORKLOADS" ;;
run)
  W=${2:?workload}; OUT=${3:?outdir}
  mkdir -p "$OUT"
  M=$OUT/$W.markers.jsonl
  : > "$M"
  PKG=$(pkg_of "$W")
  case "$W" in launcher|shade|robotpark) ;; *) S am force-stop "$PKG" > /dev/null ;; esac
  [ "$W" = robotpark ] && S am force-stop dev.fcvm.robotpark > /dev/null
  key HOME; sleep 3
  mark launch_start "\"workload\":\"$W\",\"pkg\":\"$PKG\""
  out=$(launch "$W")
  total=$(grep -o 'TotalTime: [0-9]*' <<< "$out" | grep -o '[0-9]*' || true)
  mark launch_done "\"total_time_ms\":${total:-null},\"status\":\"$(grep -m1 -o 'Status: [a-z]*' <<< "$out" | cut -d' ' -f2)\""
  sleep "${SETTLE:-15}"
  post_launch "$W"
  S dumpsys gfxinfo "$PKG" reset > /dev/null
  # the steady window: capture the screen, dump 10 s of the stream from 20 s in
  "$STREAMER" capture "$STEADY" "$OUT/$W.nv12" > "$OUT/$W.capture.json" 2> "$OUT/$W.capture.err" &
  cap=$!
  mark steady_start
  END=$(( $(ms) + STEADY * 1000 ))
  (
    sleep 20; [ -n "${DUMP:-}" ] && touch "$DUMP.on" && mark dump_on
    sleep 10; [ -n "${DUMP:-}" ] && rm -f "$DUMP.on" && mark dump_off
  ) &
  dumper=$!
  until_end step "$W"
  mark steady_end
  S dumpsys gfxinfo "$PKG" > "$OUT/$W.gfxinfo.txt"
  wait "$dumper"
  wait "$cap"
  sleep 2.5
  [ -n "${DUMP:-}" ] && [ -f "$DUMP" ] && mv "$DUMP" "$OUT/$W.dump"
  S getprop debug.hwui.renderer > "$OUT/$W.hwui.txt"
  grep -m1 -E 'Pipeline=' "$OUT/$W.gfxinfo.txt" >> "$OUT/$W.hwui.txt" || true
  mark done
  echo "done $W"
  ;;
*) echo "usage: wl.sh run <workload> <outdir> | list" >&2; exit 2 ;;
esac
