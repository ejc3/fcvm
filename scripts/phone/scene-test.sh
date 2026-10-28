#!/bin/bash
# Robot Park (fleet/phone-app) on a booted phone: scripts/phone/run.sh calls this once Android
# is up. Checks, each failing the run:
#   1. the app's GLES context is on the VM's GPU, and it draws at least 30 frames a second;
#   2. seams: the left and right halves of a two-phone image, and the whole image, drawn at
#      the same t (the control protocol's `capture`, the same render path as the screen), agree
#      pixel for pixel wherever they overlap (up to 9 pixels a tile 1 level apart, see seam.py),
#      so the halves stitch with no seam; and one rect drawn twice gives the same bytes;
#   3. surface loss: the app keeps drawing after its surface is destroyed and created again
#      (home, then back), and after its EGL context is torn down and built again (the
#      EGL_CONTEXT_LOST path), and the rebuilt context draws the same pixels as before;
#   4. the fixed actions, by socket and by intent, and nothing else: an unknown action is refused.
# Screenshots and the stitched seam images go to OUT/shots and OUT/seam.
#
#   scene-test.sh FCVM PID APK_IN_CONTAINER OUT GPU_NAME
# OUT is a host directory the phone container sees at /out (OUT_IN names another path).
set -uo pipefail
F=$1 PID=$2 APK=$3 OUT=$4 GPU_NAME=$5
IN=${OUT_IN:-/out}
HERE=$(cd "$(dirname "$0")" && pwd)
X() { timeout "${XTIMEOUT:-120}" "$F" exec --pid "$PID" -- "$@"; }
A() { X /android/platform-tools/adb "$@"; }
C() { X /usr/local/bin/phone.sh ctl "$1"; }
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
status() { C '{"type":"status"}'; }
field() { jq -r "$1" <<< "$2" 2>/dev/null; }
mkdir -p "$OUT/seam" "$OUT/shots"

out=$(X /usr/local/bin/phone.sh app "$APK" 2>&1) || { bad "Robot Park did not start: $out"; exit 1; }
[[ "$out" == *"$GPU_NAME"* ]] && ok "Robot Park's GLES context: $(grep -o 'renderer=.*version=[^ ]*' <<< "$out" | head -1)" \
  || bad "Robot Park's renderer does not name '$GPU_NAME': $out"

# A two-phone image: this phone's scene is the right half; the captures draw any rect.
now=$(date +%s%3N)
epoch=$(( now - 30000 ))
SCENE=$(jq -nc --argjson e "$epoch" '{type:"scene", epoch_unix_ms:$e, rect:{x:0.5,y:0,w:0.5,h:1}, image_aspect:1.125,
  words:[{text:"HELLO PHONES",t:20}], lights:[{pos:[-2,1.6,6],color:[1,0.45,0.15],t:0},{pos:[3,1.4,2],color:[0.3,0.6,1],t:0}],
  moves:[{t:60,move:"dance"}]}')
r=$(C "$SCENE"); [ "$(field .ok "$r")" = true ] && ok "scene accepted" || bad "scene refused: $r"
r=$(C '{"type":"place","vm":"phone-test","slot":"0","host":"example-host","gpu":"Tesla T4","restored_in_ms":0}')
[ "$(field .ok "$r")" = true ] || bad "place refused: $r"
sleep 8
s=$(status)
fps=$(field .fps "$s")
python3 -c 'import sys; sys.exit(0 if float(sys.argv[1]) >= 30 else 1)' "${fps:-0}" \
  && ok "Robot Park draws $fps frames a second ($(field .surface "$s" | tr -d '\n ') surface, $(field .samples "$s")x MSAA, $(field .robots "$s") robots)" \
  || bad "Robot Park draws below 30 frames a second: $s"

# Seams: the whole image and its two halves at three times (walking; the sign crossing the
# seam, the lights; the dance with confetti), and the left half again at the end.
capture() { # name t x w W -> the reply, with the file pulled to OUT/seam
  local r
  r=$(C "$(jq -nc --arg n "$1" --argjson t "$2" --argjson x "$3" --argjson w "$4" --argjson W "$5" \
    '{type:"capture", name:$n, t:$t, rect:{x:$x,y:0,w:$w,h:1}, image_aspect:1.125, w:$W, h:1280}')")
  [ "$(field .ok "$r")" = true ] || { echo "capture $1 failed: $r" >&2; echo "{}"; return; }
  A pull "$(field .path "$r")" "$IN/seam/$1.rgba" > /dev/null 2>&1
  jq -c --arg p "$OUT/seam/$1.rgba" '. + {path:$p}' <<< "$r"
}
frames="[]"
for t in 12 41.5 63.25; do
  n=${t//./-}
  wh=$(capture "whole-$n" "$t" 0 1 1440)
  le=$(capture "left-$n" "$t" 0 0.5 720)
  ri=$(capture "right-$n" "$t" 0.5 0.5 720)
  frames=$(jq -c --argjson t "$t" --argjson a "$wh" --argjson b "$le" --argjson c "$ri" '. + [{t:$t, whole:$a, tiles:[$b,$c]}]' <<< "$frames")
done
again=$(capture "left-12-again" 12 0 0.5 720)
first=$(jq -c '.[0].tiles[0]' <<< "$frames")
jq -n --argjson f "$frames" --argjson a "$first" --argjson b "$again" '{frames:$f, repeats:[[$a,$b]]}' > "$OUT/seam/manifest.json"
if seam=$(python3 "$HERE/seam.py" "$OUT/seam/manifest.json" "$OUT/seam"); then
  ok "seams: both halves equal the whole image at t=12, 41.5 and 63.25 ($(jq -c '[.frames[].tiles[].pixels] | add' <<< "$seam") pixels compared, $(jq -c '[.frames[].tiles[].differ] | add' <<< "$seam") 1 level apart), and a redraw is identical"
else
  bad "seams: $seam"
fi
echo "$seam" > "$OUT/seam/report.json"

# Surface loss: home destroys the surface, coming back creates a new one. Drawing is judged by
# frames counted between two status samples taken after the change (fps alone keeps its last
# value when drawing stops).
drawn() { # frames drawn over 2 s, now
  local a b
  a=$(field .frames "$(status)"); sleep 2; b=$(field .frames "$(status)")
  echo $(( ${b:-0} - ${a:-0} ))
}
A shell input keyevent KEYCODE_HOME
sleep 2
A shell am start -n dev.fcvm.robotpark/.MainActivity > /dev/null 2>&1
sleep 3
n=$(drawn)
[ "$n" -ge 60 ] && ok "after the surface was destroyed and created again it drew $n frames in 2 s" \
  || bad "after the surface came back it drew $n frames in 2 s: $(status)"
g0=$(field .context_generation "$(status)")
r=$(C '{"type":"drop_context"}')
sleep 1
n=$(drawn)
s=$(status)
again2=$(capture "left-12-recreated" 12 0 0.5 720)
if [ "$(field .context_generation "$s")" = $(( g0 + 1 )) ] && [ "$n" -ge 60 ] \
   && [ "$(field .fnv64 "$again2")" = "$(field .fnv64 "$first")" ]; then
  ok "a torn-down and rebuilt EGL context (generation $(field .context_generation "$s")) drew $n frames in 2 s and the same pixels"
else
  bad "context rebuild: $r; $n frames in 2 s; status $s; capture $(field .fnv64 "$again2") against $(field .fnv64 "$first")"
fi

# A scene the GPU cannot draw (a whole image 204,800 pixels wide, past its framebuffers) is
# reported, and the next scene draws again.
r=$(C "$(jq -c '.rect = {x:0.5, y:0, w:0.00390625, h:0.0625} | .image_aspect = 10' <<< "$SCENE")")
sleep 1
e=$(field '.last_error // empty' "$(status)")
C "$SCENE" > /dev/null
sleep 1
n=$(drawn)
[ -n "$e" ] && [ "$n" -ge 60 ] && ok "a scene too large to draw is reported ($e) and the next scene drew $n frames in 2 s" \
  || bad "after a scene too large to draw: last_error '$e', $n frames in 2 s (scene reply $r)"

# The fixed actions.
act() { C "{\"type\":\"action\",\"name\":\"$1\"}"; }
r=$(act shell); [ -n "$(field .error "$r")" ] && [ "$(field .ok "$r")" != true ] && ok "an unknown action is refused: $(field .error "$r")" || bad "an unknown action was taken: $r"
X /usr/local/bin/phone.sh shot $IN/shots/scene.png
act info > /dev/null; sleep 1.5
[ "$(field .screen "$(status)")" = info ] && ok "action info: the info screen" || bad "action info did not open the info screen"
X /usr/local/bin/phone.sh shot $IN/shots/info.png
act scene > /dev/null; sleep 1
[ "$(field .screen "$(status)")" = scene ] && ok "action scene: back to the scene" || bad "action scene did not close the info screen"
act dark > /dev/null; sleep 1.5
[ "$(field .night "$(status)")" = true ] && ok "action dark: night" || bad "action dark did not turn night on"
X /usr/local/bin/phone.sh shot $IN/shots/night.png
act light > /dev/null; sleep 1
[ "$(field .night "$(status)")" = false ] && ok "action light: day" || bad "action light did not turn night off"
r=$(act dance); [ "$(field .ok "$r")" = true ] && ok "action dance at t=$(field .t "$r")" || bad "action dance: $r"
sleep 3
X /usr/local/bin/phone.sh shot $IN/shots/dance.png
A shell am start -a dev.fcvm.robotpark.ACTION --es name wave > /dev/null 2>&1
sleep 1.5
A shell am start -a dev.fcvm.robotpark.ACTION --es name info > /dev/null 2>&1
sleep 1.5
[ "$(field .screen "$(status)")" = info ] && ok "actions by intent (wave, info)" || bad "the info intent did not open the info screen"
act scene > /dev/null
act rotate > /dev/null; sleep 4
sz=$(field '.surface|join("x")' "$(status)")
[ "$sz" = 1280x720 ] && ok "action rotate: landscape ($sz)" || bad "action rotate: surface $sz"
X /usr/local/bin/phone.sh shot $IN/shots/landscape.png
act rotate > /dev/null; sleep 4
act benchmark > /dev/null
b=""
for _ in $(seq 1 60); do
  sleep 1
  b=$(field '.benchmark.stages // empty' "$(status)")
  [ -n "$b" ] && break
done
[ -n "$b" ] && ok "action benchmark: $(jq -c '[.[] | {robots, ms_per_frame}]' <<< "$b")" || bad "action benchmark gave no result in 60 s"
act info > /dev/null; sleep 1.5
X /usr/local/bin/phone.sh shot $IN/shots/info-benchmark.png
act scene > /dev/null
u0=$(field .uptime_ms "$(status)")
act restart > /dev/null
s=""
for _ in $(seq 1 30); do
  sleep 1
  s=$(status 2>/dev/null) || continue
  [ -n "$(field .uptime_ms "$s")" ] && [ "$(field .uptime_ms "$s")" -lt "$u0" ] && [ "$(field '.fps > 0' "$s")" = true ] && break
done
n=$(drawn)
if [ -n "$s" ] && [ "$(field .uptime_ms "$s")" -lt "$u0" ] && [ "$n" -ge 60 ]; then
  ok "action restart: a new process drew $n frames in 2 s"
else
  bad "action restart: $n frames in 2 s after it; status $s"
fi
exit $fail
