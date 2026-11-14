#!/bin/bash
# fleet/phone/gpu-hook against a stub adb: a retake takes up the GPU only when the
# emulator's console says it is released, returns success without one only when the console
# says "held", and fails when the console does not answer, which would otherwise leave a
# released phone paused while the caller is told it was taken up. A prepared retake waits
# for 'go' on its input before it takes anything up. A streamer that ignores
# SIGTERM is killed, not waited for, and an emulator that exited and was never reaped (a
# zombie) is not running.
#   scripts/phone/test-gpu-hook.sh
set -euo pipefail
hook=$(cd "$(dirname "$0")/../../fleet/phone" && pwd)/gpu-hook
dir=$(mktemp -d)
zparent=""
trap 'kill "$sleeper" $zparent 2> /dev/null; rm -rf "$dir"' EXIT
sleep 300 &
sleeper=$!
echo "$sleeper" > "$dir/emulator.pid"
# The stub answers 'emu gpu <command>' from $dir/<command>, failing when the file is absent,
# and records every command it was asked.
cat > "$dir/adb" <<STUB
#!/bin/bash
echo "\$*" >> "$dir/asked"
[ -f "$dir/\$3" ] || exit 1
cat "$dir/\$3"
STUB
chmod +x "$dir/adb"
run() { GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" FRAME_TIMEOUT_MS=500 "$hook" retake > "$dir/out" 2>&1; }
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

rm -f "$dir/asked"
if run; then bad "a retake succeeded with a console that does not answer: $(cat "$dir/out")"; else ok "a retake fails when the console does not answer"; fi
! grep -q 'gpu retake' "$dir/asked" 2> /dev/null || bad "the retake was attempted without a status"

printf 'held\r\nOK\r\n' > "$dir/status"
rm -f "$dir/asked"
if run && ! grep -q 'gpu retake' "$dir/asked"; then ok "a held GPU needs no retake"; else bad "a held GPU: $(cat "$dir/out")"; fi

printf 'released\r\nOK\r\n' > "$dir/status"
printf 'retaken_ms=7\r\nOK\r\n' > "$dir/retake"
printf 'posted\r\nOK\r\n' > "$dir/frame"
rm -f "$dir/asked"
if run && grep -q 'gpu retake' "$dir/asked" && grep -q '^first_frame_at_ms=\|first_frame_at_ms=' "$dir/out"; then
  ok "a released GPU is retaken and its first frame waited for"
else
  bad "a released GPU: $(cat "$dir/out")"
fi
# first_frame_at_ms is the wall clock (ms since the epoch), which the restore record compares
# with the host's; first_frame_ms is an interval on the monotonic clock.
at=$(sed -n 's/.*first_frame_at_ms=\([0-9]*\).*/\1/p' "$dir/out" | tail -1)
# Within a minute either way, so a wall-clock step meanwhile does not fail it; an uptime is
# decades away.
d=$(( $(date +%s%3N) - ${at:-0} )); d=${d#-}
if [ -n "$at" ] && [ "$d" -lt 60000 ]; then
  ok "first_frame_at_ms is the wall clock"
else
  bad "first_frame_at_ms is not the wall clock: '$at', now $(date +%s%3N)"
fi

printf 'what\r\n' > "$dir/status"
if run; then bad "a retake succeeded on an unexpected status: $(cat "$dir/out")"; else ok "an unexpected status fails"; fi

# prepared-retake (fc-agent starts it while the GPU is coming up): the console's status is
# asked before 'go' and the retake only after it; input that ends without 'go' takes
# nothing up and fails; the streamer stays held until 'go'.
prep() { GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" FRAME_TIMEOUT_MS=500 "$hook" prepared-retake > "$dir/out" 2>&1; }
printf 'released\r\nOK\r\n' > "$dir/status"
rm -f "$dir/asked"
if prep < /dev/null; then bad "a prepared retake whose input ended succeeded: $(cat "$dir/out")"; else ok "a prepared retake whose input ends without go fails"; fi
if grep -q 'gpu status' "$dir/asked" && ! grep -q 'gpu retake' "$dir/asked"; then
  ok "a prepared retake without go asked the status and took nothing up"
else
  bad "a prepared retake without go asked: '$(cat "$dir/asked" 2> /dev/null)'"
fi
# held for a 'go' from a fifo: status asked, nothing taken up, the streamer held, until go
prep_go() {
  rm -f "$dir/asked" "$dir/go" "$dir/out"
  touch "$dir/streamer.hold"
  mkfifo "$dir/go"
  prep < "$dir/go" &
  p=$!
  exec 9> "$dir/go"
  until grep -q '^prepared' "$dir/out" 2> /dev/null; do sleep 0.01; done
  sleep 0.2
  if grep -q 'gpu status' "$dir/asked" && ! grep -q 'gpu retake' "$dir/asked" && [ -e "$dir/streamer.hold" ]; then
    ok "a prepared retake ($1) asks the status and holds the GPU and the streamer until go"
  else
    bad "a prepared retake ($1) before go: asked '$(cat "$dir/asked" 2> /dev/null)', hold $([ -e "$dir/streamer.hold" ] && echo kept || echo gone)"
  fi
  echo go >&9
  exec 9>&-
  rc=0
  wait "$p" || rc=$?
}
prep_go released
if [ "$rc" = 0 ] && grep -q 'gpu retake' "$dir/asked" && grep -q 'first_frame_at_ms=' "$dir/out" && [ ! -e "$dir/streamer.hold" ]; then
  ok "a prepared retake takes the GPU up after go and lets the streamer start"
else
  bad "a prepared retake after go: exit $rc, asked '$(cat "$dir/asked")': $(cat "$dir/out")"
fi
printf 'held\r\nOK\r\n' > "$dir/status"
prep_go held
if [ "$rc" = 0 ] && ! grep -q 'gpu retake' "$dir/asked" && [ ! -e "$dir/streamer.hold" ]; then
  ok "a prepared retake of a held GPU lets the streamer start after go"
else
  bad "a prepared retake of a held GPU: exit $rc: $(cat "$dir/out")"
fi

# The phone streamer holds a CUDA context: a release stops it and holds it stopped (the
# phone.sh serve loop waits on the hold file), a retake lets it start again.
mkdir -p "$dir/bin"
printf '#!/bin/bash\nwhile :; do sleep 0.05; done\n' > "$dir/bin/phone-streamer"
chmod +x "$dir/bin/phone-streamer"
"$dir/bin/phone-streamer" &
streamer=$!
printf 'released_ms=5\r\nOK\r\n' > "$dir/release"
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" "$hook" release > "$dir/out" 2>&1 || bad "the release failed: $(cat "$dir/out")"
if ! kill -0 "$streamer" 2> /dev/null && [ -e "$dir/streamer.hold" ]; then
  ok "a release stops the streamer and holds it"
else
  bad "the streamer still runs, or is not held, after the release"
  kill "$streamer" 2> /dev/null || true
fi
wait "$streamer" 2> /dev/null || true
printf 'released\r\nOK\r\n' > "$dir/status"
if run && [ ! -e "$dir/streamer.hold" ]; then ok "a retake lets the streamer start again"; else bad "the streamer is still held after the retake: $(cat "$dir/out")"; fi

# phone.sh's loop checks the hold and starts the streamer under streamer.lock. A release that
# comes while the loop holds it (it has seen no hold and is starting a streamer) waits for
# it, and stops the streamer the loop names in streamer.pid.
rm -f "$dir/streamer.hold" "$dir/streamer.pid"
(
  exec 8> "$dir/streamer.lock"
  flock 8
  echo locked > "$dir/loop"
  sleep 0.5   # the release runs now
  "$dir/bin/phone-streamer" 8>&- &
  echo $! > "$dir/streamer.pid"
  flock -u 8
  wait
) &
loop=$!
until [ -e "$dir/loop" ]; do sleep 0.01; done
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" "$hook" release > "$dir/out" 2>&1 || bad "the release failed: $(cat "$dir/out")"
until [ -s "$dir/streamer.pid" ]; do sleep 0.01; done
s=$(cat "$dir/streamer.pid")
if ! kill -0 "$s" 2> /dev/null && [ -e "$dir/streamer.hold" ]; then
  ok "a release during the loop's start stops the streamer it started"
else
  bad "a streamer the loop started during the release still runs"
  kill "$s" 2> /dev/null
fi
wait "$loop" 2> /dev/null || true

# A streamer stuck in its teardown (here: one that ignores SIGTERM) gets SIGKILL after
# STOP_TERM_MS, so the release does not wait for it for ever with streamer.lock held.
rm -f "$dir/streamer.hold" "$dir/streamer.pid"
# it says when SIGTERM is ignored, so the release cannot signal it before that
rm -f "$dir/stuck.ready"
printf '#!/bin/bash\ntrap "" TERM\ntouch "%s/stuck.ready"\nwhile :; do sleep 0.05; done\n' "$dir" > "$dir/bin/phone-streamer"
"$dir/bin/phone-streamer" &
stuck=$!
until [ -e "$dir/stuck.ready" ]; do sleep 0.01; done
echo "$stuck" > "$dir/streamer.pid"
t0=$(date +%s%3N)
rc=0
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" STOP_TERM_MS=300 timeout 10 "$hook" release > "$dir/out" 2>&1 || rc=$?
took=$(( $(date +%s%3N) - t0 ))
if [ "$rc" = 0 ] && ! kill -0 "$stuck" 2> /dev/null && grep -q 'SIGKILL' "$dir/out"; then
  ok "a streamer that ignores SIGTERM is killed and the release goes on (${took} ms)"
else
  bad "a streamer that ignores SIGTERM: release exit $rc after ${took} ms, streamer $(kill -0 "$stuck" 2> /dev/null && echo still runs || echo gone): $(cat "$dir/out")"
  kill -9 "$stuck" 2> /dev/null || true
fi
wait "$stuck" 2> /dev/null || true

# An emulator that exited but was never reaped (its parent reaps nothing) is a zombie: the
# hook does not ask its console. A release has nothing to let go of; a retake fails and
# says the emulator exited, since the phone it would take up is gone.
# the parent forks a child that exits at once and never waits for it
python3 -c 'import os, sys, time
pid = os.fork()
if pid == 0: os._exit(0)
open(sys.argv[1], "w").write(str(pid))
time.sleep(300)' "$dir/zombie.pid" &
zparent=$!
zstate=""
for _ in $(seq 1 250); do
  [ -s "$dir/zombie.pid" ] && zstate=$(sed 's/.*) //' "/proc/$(cat "$dir/zombie.pid")/stat" 2> /dev/null | cut -d' ' -f1)
  [ "$zstate" = Z ] && break
  sleep 0.02
done
[ "$zstate" = Z ] || { bad "the zombie fixture made no zombie (state '$zstate')"; exit 1; }
cp "$dir/zombie.pid" "$dir/emulator.pid"
printf 'released\r\nOK\r\n' > "$dir/status"
rm -f "$dir/asked" "$dir/streamer.hold"
rc=0
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" "$hook" release > "$dir/out" 2>&1 || rc=$?
if [ "$rc" = 0 ] && ! grep -q 'gpu' "$dir/asked" 2> /dev/null && grep -q 'exited' "$dir/out"; then
  ok "a release with a zombie emulator asks no console and says the emulator exited"
else
  bad "a release with a zombie emulator: exit $rc, asked '$(cat "$dir/asked" 2> /dev/null)': $(cat "$dir/out")"
fi
rm -f "$dir/asked"
rc=0
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" FRAME_TIMEOUT_MS=500 "$hook" retake > "$dir/out" 2>&1 || rc=$?
if [ "$rc" != 0 ] && ! grep -q 'gpu' "$dir/asked" 2> /dev/null && grep -q 'exited' "$dir/out"; then
  ok "a retake with a zombie emulator fails, asks no console and says the emulator exited"
else
  bad "a retake with a zombie emulator: exit $rc, asked '$(cat "$dir/asked" 2> /dev/null)': $(cat "$dir/out")"
fi

# an emulator that exited and was reaped (its pid names no process) fails a retake the same way
echo 2147483646 > "$dir/emulator.pid"
rm -f "$dir/asked"
rc=0
GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" FRAME_TIMEOUT_MS=500 "$hook" retake > "$dir/out" 2>&1 || rc=$?
[ "$rc" != 0 ] && ! grep -q 'gpu' "$dir/asked" 2> /dev/null && grep -q 'exited' "$dir/out" \
  && ok "a retake with a reaped emulator fails and says it exited" || bad "a retake with a reaped emulator: exit $rc: $(cat "$dir/out")"

# `podman exec` (how fc-agent runs the hook) ends the exec session's other processes once
# the hook exits, so a line still in the pipe to the log's tee was lost: the clone's first
# frame went missing from /tmp/gpu-hook.log in 1 of 5 phone-tests, and 3 of 301 runs of a
# copy of the hook's logging under `podman exec` (#1416). A tee held from reading until the
# test lets it go makes that every time: the hook must still be running (waiting for its
# tee) a second after tee is held, and the log must have the line once tee goes on.
mkdir -p "$dir/slowtee"
rm -f "$dir/tee.held" "$dir/tee.go"
printf '#!/bin/bash\ntouch "%s/tee.held"\nuntil [ -e "%s/tee.go" ]; do sleep 0.01; done\nexec /usr/bin/tee "$@"\n' "$dir" "$dir" > "$dir/slowtee/tee"
chmod +x "$dir/slowtee/tee"
kill "$zparent" 2> /dev/null || true; wait "$zparent" 2> /dev/null || true; zparent=""
echo "$sleeper" > "$dir/emulator.pid"
printf 'released\r\nOK\r\n' > "$dir/status"
rm -f "$dir/gpu-hook.log" "$dir/asked"
(exec env PATH="$dir/slowtee:$PATH" GPU_HOOK_ADB="$dir/adb" GPU_HOOK_STATE="$dir" FRAME_TIMEOUT_MS=500 setsid "$hook" retake > "$dir/out" 2>&1) &
h=$!
# past the hook's last console call (the posted frame), so all that is left is its exit
until [ -e "$dir/tee.held" ] && grep -q 'gpu frame' "$dir/asked" 2> /dev/null; do sleep 0.01; done
sleep 1
if kill -0 "$h" 2> /dev/null; then ok "the hook waits for its log's tee before it exits"; else bad "the hook exited while its tee still held its output"; fi
touch "$dir/tee.go"
wait "$h"
pkill -9 -s "$h" 2> /dev/null || true
if grep -q 'first_frame_at_ms=' "$dir/gpu-hook.log" 2> /dev/null; then
  ok "the hook's last line reaches its log before the hook exits"
else
  bad "the hook exited before its log had its last line: '$(cat "$dir/gpu-hook.log" 2> /dev/null)'"
fi
exit $fail
