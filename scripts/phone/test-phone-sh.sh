#!/bin/bash
# fleet/phone/phone.sh against a stub adb: `bugreports` counts the bug reports in the events
# log, prints 0 for a log with none, and fails when adb cannot read the log, so run.sh's
# check cannot pass on a phone it could not ask. Then `serve` with stand-ins for adb, the
# emulator, the streamer and nvidia-smi: a phone is streamed only once SurfaceFlinger and
# the app render on the VM's GPU, and every way a start fails ends in the streamer's
# failure mode, after the serving streamer has let go of its ports.
#   scripts/phone/test-phone-sh.sh
set -euo pipefail
phone=$(cd "$(dirname "$0")/../../fleet/phone" && pwd)/phone.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
# The stub prints $dir/events for `logcat -d -b events`, and fails when it is absent.
cat > "$dir/adb" <<STUB
#!/bin/bash
[ -f "$dir/events" ] || exit 1
cat "$dir/events"
STUB
chmod +x "$dir/adb"
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
count() { PHONE_ADB="$dir/adb" "$phone" bugreports 2> /dev/null; }

printf 'I am_proc_start: [0,1,2000,com.android.shell,broadcast,{com.android.shell/com.android.shell.BugreportRequestedReceiver}]\r\n' > "$dir/events"
out=$(count) && [ "$out" = 1 ] && ok "a requested bug report counts 1" || bad "a requested bug report counted '$out'"
printf 'I am_proc_start: [0,1,10111,com.android.inputmethod.latin,service]\r\n' > "$dir/events"
out=$(count) && [ "$out" = 0 ] && ok "a log without one counts 0" || bad "a log without a bug report counted '$out'"
rm -f "$dir/events"
if out=$(count); then bad "bugreports succeeded when adb could not read the log: '$out'"; else ok "bugreports fails when adb cannot read the log"; fi
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
  "shell getprop sys.boot_completed") echo 1 ;;
  "shell dumpsys SurfaceFlinger")
    [ -e "$D/go" ] && echo "SurfaceFlinger checked with the go file present" >> "$D/log"
    echo "GLES: Google (NVIDIA Corporation), Android Emulator OpenGL ES Translator (${FAKE_RENDERER:-Tesla T4/PCIe/SSE2}), OpenGL ES 3.1" ;;
  "shell am start"*) if [ "${FAKE_AM:-ok}" = ok ]; then echo "Status: ok"; else echo "Error: Activity class does not exist."; fi ;;
  "logcat -d -s ROBOTPARK:I") echo "I ROBOTPARK: renderer=Android Emulator OpenGL ES Translator (${FAKE_APP_RENDERER:-Tesla T4/PCIe/SSE2})" ;;
  "emu kill") kill "$(cat "$D/run/emulator.pid")" ;;
esac
exit 0
EOF
  cat > "$D/bin/emulator" <<'EOF'
#!/bin/bash
echo "emulator $*" >> "$FAKE_DIR/log"
trap 'exit 0' TERM
# a stand-in nobody stopped ends with the test's directory
while [ -d "$FAKE_DIR" ]; do sleep 0.05; done
EOF
  cat > "$D/bin/phone-streamer" <<'EOF'
#!/bin/bash
D=$FAKE_DIR
if [ "$1" = fail ]; then echo "streamer fail: $2" >> "$D/log"; exit 0; fi
echo "streamer serve" >> "$D/log"
trap 'sleep 0.3; echo "streamer serve exited" >> "$D/log"; exit 0' TERM
while :; do
  [ -e "$PHONE_GO_FILE" ] && [ ! -e "$D/streaming" ] && { touch "$D/streaming"; echo "streamer streaming" >> "$D/log"; }
  sleep 0.05
done
EOF
  cat > "$D/bin/nvidia-smi" <<'EOF'
#!/bin/bash
[ -n "${FAKE_NO_GPU:-}" ] && exit 3
echo "Tesla T4"
EOF
  printf '#!/bin/sh\nexit 0\n' > "$D/bin/mountpoint"
  printf '#!/bin/sh\nexit 0\n' > "$D/bin/mount"
  chmod +x "$D/bin/"*
  (
    export PATH="$D/bin:$PATH" FAKE_DIR="$D" HOME="$D"
    export PHONE_ADB="$D/bin/adb" EMU_DIR="$D/bin" PHONE_STREAMER="$D/bin/phone-streamer" PHONE_KVM=/dev/null
    export PHONE_RUN="$D/run" PHONE_GO_FILE="$D/go" PHONE_AVD_HOME="$D/avd" PHONE_APK=/work/robotpark.apk BOOT_TIMEOUT=5
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
# `action`: each name sends its one request to the app's control socket (a stand-in that
# records what it is sent); switch_screen and dark_mode toggle; any other name is refused.
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 -c '
import os, socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(16)
with open(sys.argv[2], "a") as log:
    while True:
        c, _ = s.accept(); f = c.makefile("rw")
        for line in f:
            log.write(line); log.flush()
            # a reply the test set ($dir/reply, used once), else ok
            try:
                with open(sys.argv[3]) as r: reply = r.read()
                os.remove(sys.argv[3])
            except FileNotFoundError:
                reply = "{\"ok\": true}\n"
            f.write(reply); f.flush()
        c.close()
' "$port" "$dir/ctl.log" "$dir/reply" &
ctl=$!
for _ in $(seq 1 50); do (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null && break; sleep 0.1; done
act() { PHONE_CTL_PORT=$port PHONE_RUN="$dir" "$phone" action "$1" > /dev/null 2> "$dir/act.err"; }
for a in switch_screen switch_screen rotate dark_mode dark_mode dance wave gpu_benchmark restart_app dark_mode restart_app dark_mode; do
  act "$a" || bad "action $a failed: $(cat "$dir/act.err")"
done
want='{"type":"action","name":"info"}
{"type":"action","name":"scene"}
{"type":"action","name":"rotate"}
{"type":"action","name":"dark"}
{"type":"action","name":"light"}
{"type":"action","name":"dance"}
{"type":"action","name":"wave"}
{"type":"action","name":"benchmark"}
{"type":"action","name":"restart"}
{"type":"action","name":"dark"}
{"type":"action","name":"restart"}
{"type":"action","name":"dark"}'
[ "$(cat "$dir/ctl.log")" = "$want" ] && ok "each action sends its one request to the app" || bad "the app was sent: $(cat "$dir/ctl.log")"
for a in reboot_phone shell "rotate; id" ""; do
  if act "$a"; then bad "action '$a' was accepted"; else rc=$?; [ "$rc" = 2 ] && ok "action '$a' is refused" || bad "action '$a' exited $rc"; fi
done
[ "$(wc -l < "$dir/ctl.log")" = 12 ] && ok "a refused action sends the app nothing" || bad "a refused action reached the app"
# A reply that is not a JSON object without "error" fails the action, and a refused toggle
# leaves the cached state as it was, so the next one asks for the same change again.
printf 'not json\n' > "$dir/reply"
if act rotate; then bad "an action the app answered with 'not json' succeeded"; else ok "an action whose reply is not JSON fails"; fi
printf '{"error":"the app is not on screen"}\n' > "$dir/reply"
if act switch_screen; then bad "a refused switch_screen succeeded"; else ok "a refused toggle fails"; fi
act switch_screen || bad "switch_screen failed: $(cat "$dir/act.err")"
[ "$(tail -2 "$dir/ctl.log")" = '{"type":"action","name":"info"}
{"type":"action","name":"info"}' ] && ok "a refused toggle is asked for again, not undone" || bad "after a refused switch_screen the app was sent: $(tail -2 "$dir/ctl.log")"
kill "$ctl" 2> /dev/null || true
# The streamer's Makefile runs protoc once for its generated sources, however parallel the
# build (a dry run names each command it would run).
n=$(make -C "$(dirname "$phone")/streamer" -n -j4 OUT="$dir/pb" "$dir/pb/screen.pb.cc" "$dir/pb/screen.grpc.pb.cc" 2> /dev/null | grep -c '^protoc' || true)
[ "$n" = 1 ] && ok "a parallel streamer build runs protoc once" || bad "a parallel streamer build runs protoc $n times"
exit $fail
