#!/bin/bash
# fleet/phone/phone.sh against a stub adb: `bugreports` prints how many bug reports dumpstate
# took since boot, 0 before the first, and fails when adb cannot ask, so run.sh's check
# cannot pass on a phone it could not ask. Then `serve` with stand-ins for adb, the
# emulator, the streamer and nvidia-smi: a phone is streamed only once SurfaceFlinger and
# the app render on the VM's GPU, and every way a start fails ends in the streamer's
# failure mode, after the serving streamer has let go of its ports.
#   scripts/phone/test-phone-sh.sh
set -euo pipefail
phone=$(cd "$(dirname "$0")/../../fleet/phone" && pwd)/phone.sh
dir=$(mktemp -d)
# stand-ins a failing check left running end with the test (their paths are under $dir)
trap 'pkill -9 -f -- "$dir/" 2> /dev/null || true; rm -rf "$dir"' EXIT
# running, not a zombie, as phone.sh's alive() counts it
running_pid() { local st; st=$(sed 's/.*) //' "/proc/$1/stat" 2> /dev/null | cut -d' ' -f1) && [ -n "$st" ] && [ "$st" != Z ]; }
# The stub prints $dir/last_id for `shell getprop dumpstate.last_id`, and fails when it is absent.
cat > "$dir/adb" <<STUB
#!/bin/bash
[ -f "$dir/last_id" ] || exit 1
cat "$dir/last_id"
STUB
chmod +x "$dir/adb"
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
count() { PHONE_ADB="$dir/adb" "$phone" bugreports 2> /dev/null; }

printf '1\r\n' > "$dir/last_id"
out=$(count) && [ "$out" = 1 ] && ok "a bug report taken counts 1" || bad "a bug report taken counted '$out'"
printf '\r\n' > "$dir/last_id"
out=$(count) && [ "$out" = 0 ] && ok "a phone that took none counts 0" || bad "a phone with no bug report counted '$out'"
rm -f "$dir/last_id"
if out=$(count); then bad "bugreports succeeded when adb could not ask: '$out'"; else ok "bugreports fails when adb cannot ask"; fi
# One run of `phone.sh serve` in a fresh directory. Stand-ins log to $D/log; FAKE_* steer
# them. Prints the directory.
serve() {
  local D
  D=$(mktemp -d "$dir/serve.XXXXXX")
  mkdir -p "$D/bin" "$D/run"
  cat > "$D/bin/adb" <<'EOF'
#!/bin/bash
D=$FAKE_DIR
echo "adb $*" >> "$D/log"
case "$*" in
  # a phone reports booted only once its emulator runs
  "shell getprop sys.boot_completed") [ -e "$D/emulator-up" ] && echo 1 ;;
  "shell cmd uwb disable-uwb") [ -z "${FAKE_UWB_FAILS:-}" ] || exit 1 ;;
  "shell dumpsys SurfaceFlinger")
    [ -e "$D/go" ] && echo "SurfaceFlinger checked with the go file present" >> "$D/log"
    echo "GLES: Google (NVIDIA Corporation), Android Emulator OpenGL ES Translator (${FAKE_RENDERER:-Tesla T4/PCIe/SSE2}), OpenGL ES 3.1" ;;
  # the started activity comes to the top (am's -n component, else the package last named)
  "shell am start"*) if [ "${FAKE_AM:-ok}" = ok ]; then c=$(sed -n 's/.* -n \([^ ]*\).*/\1/p' <<< "$*"); c=${c:-${*: -1}/.Main}; printf 'Starting: Intent { cmp=%s }\nStatus: ok\n' "$c"; echo "$c" > "$D/top"; else echo "Error: Activity class does not exist."; fi ;;
  "shell dumpsys activity activities") [ -s "$D/top" ] && echo "  topResumedActivity=ActivityRecord{1 u0 $(cat "$D/top") t9}" ;;
  "logcat -d -s ROBOTPARK:I") echo "I ROBOTPARK: renderer=Android Emulator OpenGL ES Translator (${FAKE_APP_RENDERER:-Tesla T4/PCIe/SSE2})" ;;
  "emu kill") kill "$(cat "$D/run/emulator.pid")" ;;
esac
exit 0
EOF
  cat > "$D/bin/emulator" <<'EOF'
#!/bin/bash
trap 'exit 0' TERM
${FAKE_EMULATOR_DELAY:+sleep "$FAKE_EMULATOR_DELAY"}
echo "emulator $*" >> "$FAKE_DIR/log"
echo "emulator preloads ${LD_PRELOAD:-nothing}" >> "$FAKE_DIR/log"
touch "$FAKE_DIR/emulator-up"
# a stand-in nobody stopped ends with the test's directory
while [ -d "$FAKE_DIR" ]; do sleep 0.05; done
EOF
  cat > "$D/bin/phone-streamer" <<'EOF'
#!/bin/bash
D=$FAKE_DIR
if [ "$1" = fail ]; then
  # failure mode names a serving streamer still running when it starts
  p=$(cat "$D/serve.pid" 2> /dev/null || true)
  st=$( [ -n "$p" ] && sed 's/.*) //' "/proc/$p/stat" 2> /dev/null | cut -d' ' -f1 || true)
  if [ -n "$st" ] && [ "$st" != Z ]; then echo "a serving streamer still runs at failure mode" >> "$D/log"; fi
  echo "streamer fail: $2" >> "$D/log"; exit 0
fi
echo $$ > "$D/serve.pid"
if [ -n "${FAKE_STREAMER_STUCK:-}" ]; then trap '' TERM; else trap 'sleep 0.3; echo "streamer serve exited" >> "$D/log"; exit 0' TERM; fi
echo "streamer serve" >> "$D/log"
while :; do
  [ -e "$PHONE_GO_FILE" ] && [ ! -e "$D/streaming" ] && { touch "$D/streaming"; echo "streamer streaming" >> "$D/log"; }
  sleep 0.05
done
EOF
  cat > "$D/bin/nvidia-smi" <<'EOF'
#!/bin/bash
# a failed GPU query comes once the streamer serves (its TERM trap set), so the check that
# failure mode waits for it to exit has a serving streamer to wait for
if [ -n "${FAKE_NO_GPU:-}" ]; then
  for _ in $(seq 1 500); do grep -q "streamer serve" "$FAKE_DIR/log" 2> /dev/null && break; sleep 0.01; done
  exit 3
fi
echo "Tesla T4"
EOF
  printf '#!/bin/sh\nexit 0\n' > "$D/bin/mountpoint"
  printf '#!/bin/sh\nexit 0\n' > "$D/bin/mount"
  chmod +x "$D/bin/"*
  (
    export PATH="$D/bin:$PATH" FAKE_DIR="$D" HOME="$D"
    export PHONE_ADB="$D/bin/adb" EMU_DIR="$D/bin" PHONE_STREAMER="$D/bin/phone-streamer" PHONE_KVM=/dev/null
    export PHONE_RUN="$D/run" PHONE_GO_FILE="$D/go" PHONE_AVD_HOME="$D/avd" PHONE_APK=/work/robotpark.apk BOOT_TIMEOUT=5
    mkdir -p "$D/nvidia"; [ -n "${FAKE_NO_TLS:-}" ] || : > "$D/nvidia/libnvidia-tls.so.580.1"
    export PHONE_NVIDIA_LIB_DIR="$D/nvidia"
    # FAKE_LOCK_SECS: the GPU hook holds streamer.lock for that long as serve starts
    if [ -n "${FAKE_LOCK_SECS:-}" ]; then
      ( exec 9> "$D/run/streamer.lock"; flock 9; echo "lock taken" >> "$D/log"; sleep "$FAKE_LOCK_SECS"; echo "lock released" >> "$D/log" ) &
      until grep -q "lock taken" "$D/log" 2> /dev/null; do sleep 0.01; done
    fi
    # a phone that started runs until its emulator exits: end it once it streams
    ( for _ in $(seq 1 100); do [ -e "$D/streaming" ] && { kill "$(cat "$D/run/emulator.pid")"; exit; }; sleep 0.05; done ) &
    timeout 20 "$phone" serve > "$D/out" 2>&1
    echo "exit $?" >> "$D/log"
  )
  echo "$D"
}
has() { grep -qF -- "$2" "$1/log"; }
# the emulator the serve started is no longer running
stopped() { ! kill -0 "$(cat "$1/run/emulator.pid")" 2>/dev/null; }
before() { # the first line with $2 comes before the first with $3
  local a b
  a=$(grep -nF -- "$2" "$1/log" | head -1 | cut -d: -f1)
  b=$(grep -nF -- "$3" "$1/log" | head -1 | cut -d: -f1)
  [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]
}

D=$(serve)
has "$D" "streamer streaming" && ok "a phone on the GPU streams" || bad "a phone on the GPU never streamed: $(cat "$D/log" "$D/out")"
has "$D" "adb install /work/robotpark.apk" && has "$D" "-no-snapshot-load" && ok "every serve boots cold and installs the app it was given" \
  || bad "serve reused a phone or skipped the install: $(cat "$D/log")"
D=$(FAKE_EMULATOR_DELAY=0.5 serve)
before "$D" "-no-snapshot-load" "adb install /work/robotpark.apk" && ok "serve installs the app only once a slow emulator runs" \
  || bad "serve went on before its emulator ran: $(cat "$D/log")"
has "$D" "SurfaceFlinger checked with the go file present" && bad "SurfaceFlinger was checked after streaming was allowed" \
  || ok "streaming is allowed only after SurfaceFlinger's check"
before "$D" "-grpc 8554" "streamer streaming" && before "$D" "logcat -d -s ROBOTPARK:I" "streamer streaming" \
  && ok "the emulator serves its screen and the app drew before streaming" || bad "streaming began before the app: $(cat "$D/log")"
D=$(FAKE_LOCK_SECS=0.5 serve)
before "$D" "lock released" "streamer serve" && ok "the streamer starts only under streamer.lock, which the GPU hook takes" \
  || bad "the streamer started while the GPU hook held streamer.lock: $(cat "$D/log")"
D=$(FAKE_NO_GPU=1 serve)
has "$D" "streamer fail: nvidia-smi names no GPU" && ok "a failed GPU query reports through the failure mode" \
  || bad "a failed GPU query: $(cat "$D/log" "$D/out")"
before "$D" "streamer serve exited" "streamer fail:" && ok "failure mode starts after the serving streamer exited" \
  || bad "failure mode did not wait for the serving streamer: $(cat "$D/log")"
# a streamer stuck in its teardown (one that ignores SIGTERM) is killed after STOP_TERM_MS,
# so failure mode still starts
D=$(FAKE_NO_GPU=1 FAKE_STREAMER_STUCK=1 STOP_TERM_MS=500 serve)
has "$D" "streamer fail: nvidia-smi names no GPU" && grep -q "SIGKILL" "$D/out" \
  && ! has "$D" "a serving streamer still runs at failure mode" && ! running_pid "$(cat "$D/serve.pid")" && ok "a streamer that ignores SIGTERM is killed and failure mode starts" \
  || bad "a streamer that ignores SIGTERM: $(cat "$D/log" "$D/out")"
D=$(FAKE_RENDERER=SwiftShader serve)
has "$D" "streamer fail: SurfaceFlinger does not render on Tesla T4" && ok "a software SurfaceFlinger is reported failed" \
  || bad "a software SurfaceFlinger: $(cat "$D/log" "$D/out")"
has "$D" "streamer streaming" && bad "a software-rendered phone was streamed" || ok "a software-rendered phone is never streamed"
stopped "$D" && ok "a failed phone's emulator is stopped" || bad "the emulator of a phone reported failed still runs"
D=$(FAKE_APP_RENDERER=SwiftShader serve)
has "$D" "streamer fail: the app did not start: Robot Park does not render on Tesla T4" && ok "an app rendering in software is reported failed" \
  || bad "an app in software: $(cat "$D/log" "$D/out")"
D=$(FAKE_AM=error serve)
has "$D" "streamer fail: the app did not start: am start" && ok "an app that does not start is reported failed" \
  || bad "an app that does not start: $(cat "$D/log" "$D/out")"
has "$D" "streamer streaming" && bad "a phone without its app was streamed" || ok "a phone without its app is never streamed"
stopped "$D" && ok "the emulator of a phone whose app did not start is stopped" || bad "the emulator of a phone whose app did not start still runs"
# UWB: the image's UWB service, given no HAL by -feature -Uwb, asks for a bug report a minute or
# two after boot unless UWB is turned off; serve and boot turn it off once booted, before the
# app, and a phone that cannot is reported failed.
D=$(serve)
before "$D" "adb shell getprop sys.boot_completed" "adb shell cmd uwb disable-uwb" && before "$D" "adb shell cmd uwb disable-uwb" "adb shell am start" \
  && ok "serve turns UWB off once booted, before the app starts" || bad "serve did not turn UWB off between boot and the app: $(cat "$D/log")"
D=$(FAKE_UWB_FAILS=1 serve)
has "$D" "streamer fail: could not turn UWB off" && ok "a phone whose UWB cannot be turned off is reported failed" \
  || bad "a phone whose UWB stayed on: $(cat "$D/log" "$D/out")"
has "$D" "streamer streaming" && bad "a phone with UWB on was streamed" || ok "a phone with UWB on is never streamed"
# `phone.sh boot` (run.sh's phone) with the same stand-ins: prints the exit status last.
boot() {
  local D
  D=$(mktemp -d "$dir/boot.XXXXXX")
  mkdir -p "$D/bin" "$D/run"
  cp "$dir"/serve.*/bin/adb "$dir"/serve.*/bin/emulator "$dir"/serve.*/bin/mountpoint "$dir"/serve.*/bin/mount "$D/bin/" 2> /dev/null || true
  (
    export PATH="$D/bin:$PATH" FAKE_DIR="$D" HOME="$D"
    export PHONE_ADB="$D/bin/adb" EMU_DIR="$D/bin" PHONE_RUN="$D/run" PHONE_AVD_HOME="$D/avd" BOOT_TIMEOUT=5
    mkdir -p "$D/nvidia"; [ -n "${FAKE_NO_TLS:-}" ] || : > "$D/nvidia/libnvidia-tls.so.580.1"
    export PHONE_NVIDIA_LIB_DIR="$D/nvidia"
    timeout 20 "$phone" boot > "$D/out" 2>&1
    echo "exit $?" >> "$D/log"
    kill "$(cat "$D/run/emulator.pid")" 2> /dev/null
  )
  echo "$D"
}
D=$(boot)
has "$D" "exit 0" && before "$D" "adb shell getprop sys.boot_completed" "adb shell cmd uwb disable-uwb" && grep -q '^boot_completed_ms=' "$D/out" \
  && ok "boot turns UWB off once booted" || bad "boot did not turn UWB off: $(cat "$D/log" "$D/out")"
# The emulator preloads the driver's TLS library, so 28 releases and retakes do not use up
# glibc's static TLS; without it the phone is reported failed.
D=$(serve)
has "$D" "emulator preloads $D/nvidia/libnvidia-tls.so.580.1" && ok "serve's emulator preloads the driver's TLS library" \
  || bad "serve's emulator does not preload libnvidia-tls: $(grep preloads "$D/log")"
D=$(FAKE_NO_TLS=1 serve)
has "$D" "streamer fail: no libnvidia-tls" && ok "a phone with no libnvidia-tls is reported failed" || bad "no libnvidia-tls: $(cat "$D/log" "$D/out")"
D=$(boot)
has "$D" "emulator preloads $D/nvidia/libnvidia-tls.so.580.1" && ok "boot's emulator preloads the driver's TLS library" \
  || bad "boot's emulator does not preload libnvidia-tls: $(grep preloads "$D/log")"
D=$(FAKE_NO_TLS=1 boot)
has "$D" "exit 1" && grep -q "no libnvidia-tls" "$D/out" && ok "boot fails with no libnvidia-tls" || bad "boot with no libnvidia-tls: $(cat "$D/log" "$D/out")"
D=$(FAKE_UWB_FAILS=1 boot)
has "$D" "exit 1" && grep -q "could not turn UWB off" "$D/out" && ok "boot fails when UWB cannot be turned off" \
  || bad "boot with UWB on: $(cat "$D/log" "$D/out")"
# `action`: each name sends its one request to the app's control socket (a stand-in that
# records what it is sent), and an action whose effect is drawn in the scene (rotate,
# dark_mode, dance, wave) shows the scene tab first, so the viewer sees it; switch_screen and
# dark_mode toggle; any other name is refused.
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 -c '
import os, socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(16)
with open(sys.argv[2], "a") as log:
    while True:
        c, _ = s.accept(); f = c.makefile("rw")
        for line in f:
            # status requests apart: logged to status.log, answered from status-reply (its
            # first line, used once) or with an app of no screen whose uptime is 1 ms and which
            # holds a scene; any other request is logged to ctl.log and answered from reply
            # (used once), else ok. Every request is also logged to all.log, in order.
            st = "\"type\":\"status\"" in line
            (open(sys.argv[4], "a") if st else log).write(line)
            log.flush()
            open(sys.argv[6], "a").write(line)
            path = sys.argv[5] if st else sys.argv[3]
            try:
                with open(path) as r: lines = r.read().splitlines(True)
                reply = lines[0]
                if lines[1:]: open(path, "w").write("".join(lines[1:]))
                else: os.remove(path)
            except (FileNotFoundError, IndexError):
                reply = "{\"ok\": true, \"uptime_ms\": 1, \"scene_epoch_unix_ms\": 1761100000000}\n" if st else "{\"ok\": true}\n"
            f.write(reply); f.flush()
        c.close()
' "$port" "$dir/ctl.log" "$dir/reply" "$dir/status.log" "$dir/status-reply" "$dir/all.log" &
ctl=$!
for _ in $(seq 1 50); do (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null && break; sleep 0.1; done
act() { PHONE_CTL_PORT=$port PHONE_RUN="$dir" "$phone" action "$1" > /dev/null 2> "$dir/act.err"; }
for a in switch_screen switch_screen rotate dark_mode dark_mode dance wave gpu_benchmark restart_app dark_mode restart_app dark_mode; do
  act "$a" || bad "action $a failed: $(cat "$dir/act.err")"
done
S='{"type":"action","name":"scene"}'
want="{\"type\":\"action\",\"name\":\"info\"}
$S
$S
{\"type\":\"action\",\"name\":\"rotate\"}
$S
{\"type\":\"action\",\"name\":\"dark\"}
$S
{\"type\":\"action\",\"name\":\"light\"}
$S
{\"type\":\"action\",\"name\":\"dance\"}
$S
{\"type\":\"action\",\"name\":\"wave\"}
{\"type\":\"action\",\"name\":\"benchmark\"}
{\"type\":\"action\",\"name\":\"restart\"}
$S
{\"type\":\"action\",\"name\":\"dark\"}
{\"type\":\"action\",\"name\":\"restart\"}
$S
{\"type\":\"action\",\"name\":\"dark\"}"
[ "$(cat "$dir/ctl.log")" = "$want" ] && ok "each action sends its one request to the app, the scene tab first for those drawn in the scene" \
  || bad "the app was sent: $(cat "$dir/ctl.log")"
for a in reboot_phone shell "rotate; id" ""; do
  if act "$a"; then bad "action '$a' was accepted"; else rc=$?; [ "$rc" = 2 ] && ok "action '$a' is refused" || bad "action '$a' exited $rc"; fi
done
[ "$(wc -l < "$dir/ctl.log")" = 19 ] && ok "a refused action sends the app nothing" || bad "a refused action reached the app"
# A reply that is not a JSON object without "error" fails the action, and a refused toggle
# leaves the cached state as it was, so the next one asks for the same change again.
printf 'not json\n' > "$dir/reply"
if act rotate; then bad "an action the app answered with 'not json' succeeded"; else ok "an action whose reply is not JSON fails"; fi
printf '{"error":"the app is not on screen"}\n' > "$dir/reply"
if act switch_screen; then bad "a refused switch_screen succeeded"; else ok "a refused toggle fails"; fi
act switch_screen || bad "switch_screen failed: $(cat "$dir/act.err")"
[ "$(tail -2 "$dir/ctl.log")" = '{"type":"action","name":"info"}
{"type":"action","name":"info"}' ] && ok "a refused toggle is asked for again, not undone" || bad "after a refused switch_screen the app was sent: $(tail -2 "$dir/ctl.log")"
# switch_screen goes by the screen the app says it shows (a tap on its tabs changes it too),
# and the cache only when the app names none.
: > "$dir/ctl.log"
printf '{"ok":true,"screen":"info"}\n' > "$dir/status-reply"
act switch_screen || bad "switch_screen failed: $(cat "$dir/act.err")"
printf '{"ok":true,"screen":"info"}\n' > "$dir/status-reply"
act switch_screen || bad "switch_screen failed: $(cat "$dir/act.err")"
[ "$(cat "$dir/ctl.log")" = '{"type":"action","name":"scene"}
{"type":"action","name":"scene"}' ] && ok "switch_screen leaves the screen the app says it shows" || bad "switch_screen with the app on info sent: $(cat "$dir/ctl.log")"
# restart_app: the place the agent sent is kept and sent to the restarted app once it answers
: > "$dir/ctl.log"
PHONE_CTL_PORT=$port PHONE_RUN="$dir" "$phone" place '{"type":"place","vm":"fleet-phone-1","slot":"phone-1"}' > /dev/null 2> "$dir/act.err" \
  || bad "place failed: $(cat "$dir/act.err")"
printf '{"ok":true,"uptime_ms":60000}\n' > "$dir/status-reply"
act restart_app || bad "restart_app failed: $(cat "$dir/act.err")"
[ "$(cat "$dir/ctl.log")" = '{"type":"place","vm":"fleet-phone-1","slot":"phone-1"}
{"type":"action","name":"restart"}
{"type":"place","vm":"fleet-phone-1","slot":"phone-1"}' ] && ok "a restarted app is told its place again" || bad "place and restart_app sent: $(cat "$dir/ctl.log")"
# restart_app waits until the restarted app holds the streamer's scene (a scene epoch in its
# status) before anything else reaches it: a dance or wave filed under the default scene's clock
# was cleared when the scene came, after the app had answered ok (#1518).
: > "$dir/all.log"
printf '%s\n' '{"ok":true,"uptime_ms":60000,"scene_epoch_unix_ms":1761100000000}' \
  '{"ok":true,"uptime_ms":5,"scene_epoch_unix_ms":0}' '{"ok":true,"uptime_ms":9,"scene_epoch_unix_ms":0}' > "$dir/status-reply"
act restart_app || bad "restart_app failed: $(cat "$dir/act.err")"
want=$(printf '%s\n' status restart status status status place)
got=$(jq -r 'if .type == "action" then .name else .type end' "$dir/all.log")
[ "$got" = "$want" ] && ok "restart_app waits for the restarted app's scene before telling it its place" \
  || bad "restart_app with the scene late sent: $(tr '\n' ' ' <<< "$got")"
# The new process is known once its uptime goes back, even if its scene comes after its uptime
# passes the old one's: the wait ends with the scene, not 10 s later.
: > "$dir/all.log"
printf '%s\n' '{"ok":true,"uptime_ms":1000,"scene_epoch_unix_ms":1761100000000}' \
  '{"ok":true,"uptime_ms":900,"scene_epoch_unix_ms":0}' '{"ok":true,"uptime_ms":1100,"scene_epoch_unix_ms":1761100000000}' \
  '{"ok":true,"uptime_ms":1300,"scene_epoch_unix_ms":1761100000000}' '{"ok":true,"uptime_ms":1500,"scene_epoch_unix_ms":1761100000000}' > "$dir/status-reply"
act restart_app || bad "restart_app failed: $(cat "$dir/act.err")"
want=$(printf '%s\n' status restart status status place)
got=$(jq -r 'if .type == "action" then .name else .type end' "$dir/all.log")
rm -f "$dir/status-reply"
[ "$got" = "$want" ] && ok "restart_app remembers the new process while it waits for the scene" \
  || bad "restart_app with the uptime past the old one's by the scene sent: $(tr '\n' ' ' <<< "$got")"
rm -f "$dir/place"
# The browser's actions, with a stub adb and a stand-in for the streamer's /health and the
# browser's DevTools list: open_browser brings the loaded page to the front and reloads it, or
# starts the browser again while the viewer's GPU mirror is open; an action for Robot Park
# brings it back to the front first; restart_app restarts whichever is in front; a phone
# without the browser refuses open_browser.
B=$dir/browser
mkdir -p "$B"
cat > "$B/adb" <<STUB
#!/bin/bash
echo "adb \$*" >> "$B/adb.log"
case "\$*" in
  "shell pm path org.cromite.cromite") [ -e "$B/installed" ] && echo "package:/data/app/base.apk" ;;
  # am start -W: the started activity comes to the top. With $B/cut, am's output ends after its
  # Starting line (#1691): the activity still comes to the top when the file says "front", and
  # not at all when it says "never".
  "shell am start"*)
    c=\$(sed -n 's/.* -n \([^ ]*\).*/\1/p' <<< "\$*"); [ -n "\$c" ] || c="\${@: -1}/.Main"
    cut=\$(cat "$B/cut" 2> /dev/null)
    # adb did not reach am: nothing starts, and am prints nothing
    if [ "\$cut" = noconn ]; then echo "adb: error: closed" >&2; exit 1; fi
    [ "\$cut" = never ] || echo "\$c" > "$B/top"
    # am takes the intent and then never answers
    if [ -e "$B/amhang" ]; then printf 'Starting: Intent { cmp=%s }\r\n' "\$c"; sleep 30; exit 0; fi
    if [ -n "\$cut" ]; then printf 'Starting: Intent { cmp=%s }\r\n' "\$c"; exit 255; fi
    printf 'Starting: Intent { cmp=%s }\r\nStatus: ok\r\nTotalTime: 123\r\n' "\$c" ;;
  "shell dumpsys activity activities") [ -e "$B/slow" ] && sleep 1; [ -e "$B/hang" ] && sleep 30; [ -s "$B/top" ] && printf '  topResumedActivity=ActivityRecord{44375846 u0 %s t9}\r\n' "\$(cat "$B/top")" ;;
esac
exit 0
STUB
chmod +x "$B/adb"
hport=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 -c '
import http.server, json, os, sys
d = sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        if self.path == "/health":
            body = {"mirror": {"open": os.path.exists(d + "/mirror")}}
        elif self.path == "/json/list":
            body = [{"id": "7", "type": "page", "url": "https://www.nvidia.com/en-us/", "title": "NVIDIA"}]
        elif self.path.startswith("/json/close/"):
            open(d + "/closed", "a").write(self.path + "\n"); body = "Target is closing"
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers(); return
        b = json.dumps(body).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
' "$hport" "$B" &
http=$!
for _ in $(seq 1 50); do (exec 3<> "/dev/tcp/127.0.0.1/$hport") 2> /dev/null && break; sleep 0.1; done
bact() { : > "$B/adb.log"; : > "$dir/ctl.log"; PHONE_ADB="$B/adb" PHONE_CTL_PORT=$port PHONE_RUN="$B" PHONE_STREAMER_PORT=$hport PHONE_DEVTOOLS_PORT=$hport \
  "$phone" action "$@" > "$B/out" 2> "$dir/act.err"; }
adb_has() { grep -qF -- "$1" "$B/adb.log"; }
if bact open_browser; then bad "open_browser succeeded on a phone without the browser"; else ok "a phone without the browser refuses open_browser"; fi
touch "$B/installed"
bact open_browser || bad "open_browser failed: $(cat "$dir/act.err")"
adb_has "shell am start -W -n org.cromite.cromite/org.chromium.chrome.browser.ChromeTabbedActivity" && adb_has "shell input keycombination 113 46" \
  && ! adb_has "force-stop" && grep -q "shown_ms=123" "$B/out" && [ "$(cat "$B/front")" = browser ] \
  && ok "open_browser brings the loaded page to the front and reloads it" || bad "open_browser ran: $(cat "$B/adb.log" "$B/out")"
# #1691: in a restored clone am's output ended after its Starting line, with no Status line,
# and open_browser failed although the browser had come to the front. What is in front is the
# activity manager's top resumed activity, not am's output.
echo dev.fcvm.robotpark/.MainActivity > "$B/top"; echo front > "$B/cut"
bact open_browser && [ "$(cat "$B/front")" = browser ] && grep -q "shown_ms=[0-9]" "$B/out" && adb_has "shell input keycombination 113 46" \
  && ok "open_browser whose am output ends after Starting goes by the browser in front" \
  || bad "open_browser with am's output cut: $(cat "$dir/act.err" "$B/out")"
echo dev.fcvm.robotpark/.MainActivity > "$B/top"; echo never > "$B/cut"
if FRONT_WAIT_MS=1000 bact open_browser; then bad "open_browser succeeded with Robot Park in front"
elif [ "$(grep -c "shell am start -W -n org.cromite.cromite" "$B/adb.log")" = 2 ] && grep -q "did not come to the front (dev.fcvm.robotpark/.MainActivity is)" "$dir/act.err"; then
  ok "open_browser fails, naming what is in front, when the browser never comes to the front after a second start"
else bad "open_browser with the browser never in front: $(cat "$dir/act.err" "$B/adb.log")"; fi
# another of the browser's activities in front is not the tab open_browser asked for
echo org.cromite.cromite/org.chromium.chrome.browser.settings.SettingsActivity > "$B/top"; echo never > "$B/cut"
if FRONT_WAIT_MS=1000 bact open_browser; then bad "open_browser succeeded with the browser's settings in front"
else ok "open_browser fails when another of the browser's activities is in front"; fi
# with a slow activity manager (each dumpsys 1 s) two tries still end within the wait asked
# for, so they fit in a take-over action's 20 s, rather than after a count of polls
echo dev.fcvm.robotpark/.MainActivity > "$B/top"; touch "$B/slow"; t0=$(date +%s%3N)
FRONT_WAIT_MS=1000 bact open_browser || true; t=$(( $(date +%s%3N) - t0 ))
rm -f "$B/slow"
[ "$t" -lt 8000 ] && ok "two starts with a slow activity manager end within their deadline ($t ms)" \
  || bad "open_browser with a slow activity manager took $t ms"
# an activity manager that never answers, and an am that takes the intent and never returns:
# every adb call is cut off within the try's budget
echo dev.fcvm.robotpark/.MainActivity > "$B/top"; rm -f "$B/cut"; touch "$B/hang" "$B/amhang"; t0=$(date +%s%3N)
FRONT_WAIT_MS=1000 bact open_browser || true; t=$(( $(date +%s%3N) - t0 ))
rm -f "$B/hang" "$B/amhang"
[ "$t" -lt 10000 ] && ok "two starts with am and the activity manager hung end within their budget ($t ms)" \
  || bad "open_browser with am and the activity manager hung took $t ms"
# a start that adb never delivered (no Starting line) does not count, though the tab it asked
# for happens to be in front already
echo org.cromite.cromite/org.chromium.chrome.browser.ChromeTabbedActivity > "$B/top"; echo noconn > "$B/cut"
if FRONT_WAIT_MS=1000 bact open_browser; then bad "open_browser succeeded with no start delivered"
elif [ "$(grep -c "shell am start -W -n org.cromite.cromite" "$B/adb.log")" = 2 ]; then ok "a start adb never delivered is sent again and then fails"
else bad "open_browser with no start delivered ran: $(cat "$B/adb.log")"; fi
rm -f "$B/cut"; echo browser > "$B/front"; echo org.cromite.cromite/org.chromium.chrome.browser.ChromeTabbedActivity > "$B/top"
bact dance || bad "dance after open_browser failed: $(cat "$dir/act.err")"
adb_has "shell am start -W -n dev.fcvm.robotpark/.MainActivity" && [ "$(cat "$dir/ctl.log")" = '{"type":"action","name":"scene"}
{"type":"action","name":"dance"}' ] && [ "$(cat "$B/front")" = app ] \
  && ok "an action for Robot Park brings it to the front first" || bad "dance with the browser in front ran: $(cat "$B/adb.log" "$dir/ctl.log")"
bact dance || bad "dance failed: $(cat "$dir/act.err")"
[ -s "$B/adb.log" ] && bad "dance with Robot Park in front ran adb: $(cat "$B/adb.log")" || ok "with Robot Park in front its actions run no adb"
bact open_browser || bad "open_browser failed: $(cat "$dir/act.err")"; bact restart_app || bad "restart_app with the browser in front failed: $(cat "$dir/act.err")"
adb_has "shell am force-stop org.cromite.cromite" && adb_has "shell am start -W -a android.intent.action.VIEW -d https://www.nvidia.com/ org.cromite.cromite" \
  && [ ! -s "$dir/ctl.log" ] && grep -q "/json/close/7" "$B/closed" \
  && ok "restart_app restarts the browser in front, in one tab, and not Robot Park" || bad "restart_app with the browser in front ran: $(cat "$B/adb.log" "$dir/ctl.log")"
bact open_app || bad "open_app failed: $(cat "$dir/act.err")"
adb_has "shell am start -W -n dev.fcvm.robotpark/.MainActivity" && [ "$(cat "$dir/ctl.log")" = '{"type":"action","name":"scene"}' ] && [ "$(cat "$B/front")" = app ] \
  && ok "open_app brings Robot Park to the front on its scene tab" || bad "open_app ran: $(cat "$B/adb.log" "$dir/ctl.log")"
touch "$B/mirror"
bact open_browser || bad "open_browser with the mirror open failed: $(cat "$dir/act.err")"
adb_has "shell am force-stop org.cromite.cromite" && adb_has "android.intent.action.VIEW" && ! adb_has "keycombination" \
  && ok "with the GPU mirror open, open_browser starts the browser again" || bad "open_browser with the mirror open ran: $(cat "$B/adb.log")"
# back from the browser with the mirror open, Robot Park starts again: its process may predate
# the mirror's copy (the mirror joined while the browser was in front)
bact open_app || bad "open_app with the mirror open failed: $(cat "$dir/act.err")"
adb_has "shell am start -W -n dev.fcvm.robotpark/.MainActivity" && [ "$(cat "$dir/ctl.log")" = '{"type":"action","name":"restart"}
{"type":"action","name":"scene"}' ] \
  && ok "with the GPU mirror open, Robot Park back from the browser starts again" || bad "open_app with the mirror open ran: $(cat "$B/adb.log" "$dir/ctl.log")"
bact open_browser > /dev/null || true; bact dance || bad "dance with the mirror open failed: $(cat "$dir/act.err")"
[ "$(cat "$dir/ctl.log")" = '{"type":"action","name":"restart"}
{"type":"action","name":"scene"}
{"type":"action","name":"dance"}' ] && ok "an action for Robot Park with the mirror open restarts it, then sends the action" \
  || bad "dance with the browser in front and the mirror open sent: $(cat "$dir/ctl.log")"
# `tap x y`: the point, fractions of the screen as it is turned now, goes to `input tap` at the
# pixel of Robot Park's surface (from its status), portrait or turned; nothing reaches the
# phone while the browser is in front, or for a point that is not two fractions.
tap() { # reply to the status request, then the point's words
  printf '%s\n' "$1" > "$dir/status-reply"
  : > "$dir/status.log"
  echo app > "$B/front"
  bact tap "${@:2}"
}
tap '{"ok":true,"surface":[720,1280]}' 0.5000 0.2500 || bad "a tap failed: $(cat "$dir/act.err")"
adb_has "shell input tap 360 320" && [ "$(cat "$dir/status.log")" = '{"type":"status"}' ] && [ ! -s "$dir/ctl.log" ] \
  && ok "a tap on an upright phone lands at its pixel" || bad "a tap on an upright phone ran: $(cat "$B/adb.log" "$dir/ctl.log")"
tap '{"ok":true,"surface":[1280,720]}' 0.5000 0.2500 || bad "a tap on a turned phone failed: $(cat "$dir/act.err")"
adb_has "shell input tap 640 180" && ok "a tap on a turned phone lands at the turned screen's pixel" || bad "a tap on a turned phone ran: $(cat "$B/adb.log")"
tap '{"ok":true,"surface":[720,1280]}' 1.0000 1.0000 || bad "a tap in the corner failed: $(cat "$dir/act.err")"
adb_has "shell input tap 719 1279" && ok "a tap at 1, 1 lands on the last pixel" || bad "a tap at 1, 1 ran: $(cat "$B/adb.log")"
tap '{"ok":true,"surface":[720,1280]}' 0.0000 0.0000 || bad "a tap at the origin failed: $(cat "$dir/act.err")"
adb_has "shell input tap 0 0" && ok "a tap at 0, 0 lands on the first pixel" || bad "a tap at 0, 0 ran: $(cat "$B/adb.log")"
printf '{"ok":true,"surface":[720,1280]}\n' > "$dir/status-reply"
: > "$dir/status.log"
echo browser > "$B/front"
if bact tap 0.5000 0.5000; then bad "a tap with the browser in front succeeded"; else
  grep -q "taps reach the phone only in Robot Park" "$dir/act.err" && ! adb_has "input tap" && [ ! -s "$dir/ctl.log" ] && [ ! -s "$dir/status.log" ] \
    && ok "with the browser in front a tap fails, saying why, and reaches nothing" || bad "a tap with the browser in front: $(cat "$dir/act.err" "$B/adb.log" "$dir/ctl.log")"
fi
rm -f "$dir/status-reply"
for pt in "0.5 0.5" "1.0001 0.5000" "0.5000 -0.1000" "0.5000" "0.5000 0.5000;id" "1e-4 0.5000"; do
  # shellcheck disable=SC2086 # the words are the test
  if tap '{"ok":true,"surface":[720,1280]}' $pt; then bad "a tap at '$pt' was accepted"; else
    rc=$?; [ "$rc" = 2 ] && ! adb_has "input tap" && ok "a tap at '$pt' is refused" || bad "a tap at '$pt' exited $rc: $(cat "$B/adb.log")"
  fi
done
rm -f "$dir/status-reply"
if tap '{"ok":true}' 0.5000 0.5000; then bad "a tap with no screen size succeeded"; else
  ! adb_has "input tap" && grep -q "no screen size" "$dir/act.err" && ok "a tap with no screen size from the app fails" || bad "a tap with no size: $(cat "$dir/act.err" "$B/adb.log")"
fi
# serve with a browser: it opens www.nvidia.com before Robot Park starts in front of it, and a
# browser that shows no NVIDIA page fails the phone.
D=$(PHONE_BROWSER_APK=/work/cromite.apk PHONE_DEVTOOLS_PORT=$hport serve)
has "$D" "streamer streaming" && before "$D" "adb install -r -g /work/cromite.apk" "adb install /work/robotpark.apk" \
  && before "$D" "android.intent.action.VIEW -d https://www.nvidia.com/ org.cromite.cromite" "dev.fcvm.robotpark/.MainActivity" \
  && [ "$(cat "$D/run/front")" = app ] && ok "serve opens the browser on www.nvidia.com, then Robot Park in front" \
  || bad "serve with a browser: $(cat "$D/log" "$D/out")"
closed=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
D=$(PHONE_BROWSER_APK=/work/cromite.apk PHONE_DEVTOOLS_PORT=$closed PHONE_BROWSER_WAIT=1 serve)
has "$D" "streamer fail: the browser did not start: the browser shows no NVIDIA page" && ! has "$D" "streamer streaming" \
  && ok "a browser that shows no NVIDIA page fails the phone" || bad "a browser with no page: $(cat "$D/log" "$D/out")"
kill "$http" 2> /dev/null || true
kill "$ctl" 2> /dev/null || true
# The streamer's Makefile runs protoc once for its generated sources, however parallel the
# build (a dry run names each command it would run).
n=$(make -C "$(dirname "$phone")/streamer" -n -j4 OUT="$dir/pb" "$dir/pb/screen.pb.cc" "$dir/pb/screen.grpc.pb.cc" 2> /dev/null | grep -c '^protoc' || true)
[ "$n" = 1 ] && ok "a parallel streamer build runs protoc once" || bad "a parallel streamer build runs protoc $n times"
exit $fail
