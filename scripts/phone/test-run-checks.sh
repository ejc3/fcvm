#!/bin/bash
# scripts/phone/checks.sh, which run.sh sources, against a stand-in fcvm: a run that cannot
# read the phone's frames reports a FAIL line with what the phone answered and keeps the
# phone's logs, and carries on to its other checks; any other command that would end the run
# under `set -e` ends it with a FAIL line naming the line and the command, never silently.
#   scripts/phone/test-run-checks.sh
set -uo pipefail
checks=$(cd "$(dirname "$0")" && pwd)/checks.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
# The stand-in answers `exec --pid P -- /usr/local/bin/phone.sh draws 4` from FAKE_DRAWS:
# frames (60 fps samples), slow (12 fps), offline (adb's error, exit 1, as phone.sh exits
# when `adb logcat -c` fails); `exec --pid P -- sh -c SCRIPT` (the log collection) by keeping
# SCRIPT in $LOGS/collect-script; and `exec --pid P --vm -- dmesg`.
cat > "$dir/fcvm" <<'STUB'
#!/bin/bash
shift 3   # exec --pid P
vm=""
[ "$1" = --vm ] && { vm=1; shift; }
[ "$1" = -- ] && shift
[ -n "$vm" ] && [ "$1" = sh ] && { echo "guest load 0.50"; exit 0; }
case "$*" in
  "/usr/local/bin/phone.sh draws 4")
    case "$FAKE_DRAWS" in
      frames) printf 'I PHONEPROBE: fps=60.1 instances=25600\nI PHONEPROBE: fps=59.7 instances=25600\n' ;;
      slow) printf 'I PHONEPROBE: fps=12.0 instances=25600\n' ;;
      offline) echo "error: device offline" >&2; exit 1 ;;
      partial) printf 'I PHONEPROBE: fps=60.1 instances=25600\n'; echo "error: device offline" >&2; exit 1 ;;
    esac ;;
  "sh -c "*) printf '%s\n' "$3" > "$LOGS/collect-script" ;;
  dmesg) echo "[    1.0] kernel log" ;;
  *) echo "stub: unexpected $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$dir/fcvm"
fail=0
pass() { echo "ok   $*"; }
miss() { echo "FAIL $*"; fail=1; }
# One run of a script that sources checks.sh as run.sh does, with $1 as its body.
script() {
  local mode=$1 body=$2
  mkdir -p "$dir/logs-$mode"
  F="$dir/fcvm" LOGS="$dir/logs-$mode" FAKE_DRAWS=$mode \
    bash -c ". '$checks'
$body
echo \"continued fail=\$fail\"" > "$dir/out" 2>&1
}

script frames 'check_draws 7 "after the retake"'
grep -q '^ok   the app draws after the retake: lowest fps sample 59.7' "$dir/out" && grep -q 'continued fail=0' "$dir/out" \
  && pass "a phone drawing 60 fps passes" || miss "a phone drawing 60 fps: $(cat "$dir/out")"

script slow 'check_draws 7 "after the retake"'
grep -q "^FAIL the app does not draw 30 fps after the retake: lowest '12.0'" "$dir/out" && grep -q 'continued fail=1' "$dir/out" \
  && pass "a phone drawing 12 fps fails the check and the run goes on" || miss "a phone drawing 12 fps: $(cat "$dir/out")"

script offline 'check_draws 7 "on the source phone after the snapshot"'
grep -q "^FAIL the app does not draw 30 fps on the source phone after the snapshot: lowest 'none'" "$dir/out" \
  && pass "a phone whose frames cannot be read fails the check" || miss "a phone whose frames cannot be read: $(cat "$dir/out")"
grep -q 'continued fail=1' "$dir/out" && pass "the run goes on after a phone logged no frame" \
  || miss "the run ended at a phone that logged no frame: $(cat "$dir/out")"
grep -q 'device offline' "$dir/out" && pass "the FAIL line carries what the phone answered" \
  || miss "the FAIL line does not say what the phone answered: $(cat "$dir/out")"
c=$dir/logs-offline/collect-script
[ -f "$c" ] && grep -q emulator.log "$c" && grep -q gpu-hook.log "$c" && grep -q 'logcat -d' "$c" && grep -q 'devices' "$c" \
  && grep -q 'kernel log' "$dir/logs-offline/out/draws-1/dmesg.txt" && grep -q "the phone's logs: $dir/logs-offline/out/draws-1" "$dir/out" \
  && grep -q 'guest load' "$dir/logs-offline/out/draws-1/guest.txt" && grep -qE '^[0-9.]+ [0-9.]+ [0-9.]+ ' "$dir/logs-offline/out/draws-1/host.txt" \
  && pass "a failed frame check keeps the emulator, GPU-hook, logcat and kernel logs, and the guest's and host's load" || miss "the logs were not kept: $(cat "$dir/out" "$c")"
[ ! -e "$dir/logs-frames/collect-script" ] && pass "a passing check keeps no logs" || miss "a passing check collected logs"

# frames read before adb failed are not a pass: the read itself has to succeed
script partial 'check_draws 7 "after the retake"'
grep -q "^FAIL the app does not draw 30 fps after the retake: lowest '60.1'; phone.sh draws exit 1" "$dir/out" \
  && pass "a frame read that fails after printing samples fails the check" || miss "a frame read that failed after its samples: $(cat "$dir/out")"

# The run's EXIT handler stops the VMs even when keeping the phone's logs fails (a full disk).
script frames 'pid=7 finished=0
LOGS=$(mktemp -u)/no/such/dir; mkdir() { return 1; }
stop_runner() { echo CLEANUP-RAN; }
trap "finish_run stop_runner" EXIT
exit 2'
grep -q CLEANUP-RAN "$dir/out" && pass "the VMs are stopped when the logs cannot be kept" || miss "a failed log keep skipped the cleanup: $(cat "$dir/out")"

# Any other failing command ends the run with a FAIL line naming it, also inside a function
# and a command substitution, as `snapshot_dir=$(grep ... | head -1)` would.
script frames 'f() { local x; x=$(false | head -1); echo "after $x"; }
f'
grep -q '^FAIL run.sh stopped at line [0-9]*' "$dir/out" && ! grep -q continued "$dir/out" \
  && pass "a command failing under set -e ends the run with a FAIL line" || miss "a command failing under set -e: $(cat "$dir/out")"
exit $fail
