#!/bin/bash
# fleet/phone/gpu-hook against a stub adb: a retake takes up the GPU only when the
# emulator's console says it is released, returns success without one only when the console
# says "held", and fails when the console does not answer, which would otherwise leave a
# released phone paused while the caller is told it was taken up.
#   scripts/phone/test-gpu-hook.sh
set -euo pipefail
hook=$(cd "$(dirname "$0")/../../fleet/phone" && pwd)/gpu-hook
dir=$(mktemp -d)
trap 'kill "$sleeper" 2> /dev/null; rm -rf "$dir"' EXIT
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

printf 'what\r\n' > "$dir/status"
if run; then bad "a retake succeeded on an unexpected status: $(cat "$dir/out")"; else ok "an unexpected status fails"; fi

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
exit $fail
