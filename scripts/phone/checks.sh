# The shell setup and the checks scripts/phone/run.sh shares with its test
# (scripts/phone/test-run-checks.sh). Sourced; expects F (the fcvm binary) and LOGS (the run's
# log directory, whose out/ the phone container sees at /out).
#
# A command that fails under `set -e` ends the run. -E carries the ERR trap into functions,
# and the trap prints a FAIL line naming the line and the command, so no run ends silently.
# It prints in the run's own shell only: inside a command substitution the substitution's
# status reaches the assignment, which fails in the run's shell and is reported there.
set -eEuo pipefail
trap 'rc=$?; [ "$BASHPID" != "$$" ] || echo "FAIL run.sh stopped at line $LINENO (exit $rc): $BASH_COMMAND; logs in ${LOGS:-?}"' ERR
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

# The lowest fps sample in the text $1, of its last $2 samples if given ("" when it has none).
lowest_fps_in() {
  { grep -o 'fps=[0-9.]*' || [ $? = 1 ]; } <<< "$1" | tail -n "${2:-+1}" | cut -d= -f2 | sort -n | head -1
}
at_least_30() { python3 -c 'import sys; sys.exit(0 if sys.argv[1] and float(sys.argv[1]) >= 30 else 1)' "${1:-}"; }
# The phone container's emulator log, GPU-hook log, logcat, adb's device list and the
# emulator console's GPU state, the VM's kernel log and load, and the host's load and busiest
# processes at that moment, kept in $LOGS/out/$2 for a check that failed on VM $1.
keep_phone_logs() {
  local rc=0
  mkdir -p "$LOGS/out/$2"
  { date -u +%FT%T.%3NZ; cat /proc/loadavg; top -bn1 -w 200 | head -40; } > "$LOGS/out/$2/host.txt" 2>&1
  "$F" exec --pid "$1" -- sh -c "d=/out/$2; A=/android/platform-tools/adb
    cp /tmp/emulator.log /tmp/gpu-hook.log \$d/
    ps -eo pid,stat,etime,pcpu,args > \$d/ps.txt
    timeout 20 \$A devices -l > \$d/adb-devices.txt 2>&1
    timeout 20 \$A emu gpu status > \$d/gpu-status.txt 2>&1
    timeout 30 \$A logcat -d -v threadtime > \$d/logcat.txt 2>&1
    timeout 30 \$A logcat -d -b events -v threadtime > \$d/events.txt 2>&1" > "$LOGS/out/$2/collect.log" 2>&1 || rc=$?
  "$F" exec --pid "$1" --vm -- dmesg > "$LOGS/out/$2/dmesg.txt" 2>&1 || rc=$?
  "$F" exec --pid "$1" --vm -- sh -c 'cat /proc/loadavg; top -bn1 -w 200 | head -30' > "$LOGS/out/$2/guest.txt" 2>&1 || rc=$?
  echo "     the phone's logs: $LOGS/out/$2 (collection exit $rc)"
}
# VM $1's probe app draws at least 30 frames a second; $2 says when ("after the retake").
# A phone whose frames cannot be read (phone.sh draws fails when adb does) fails the check
# with what it answered, and the run goes on to its other checks.
kept=0
check_draws() {
  local out rc=0 fps
  out=$("$F" exec --pid "$1" -- /usr/local/bin/phone.sh draws 4 2>&1) || rc=$?
  fps=$(lowest_fps_in "$out")
  if at_least_30 "$fps"; then
    ok "the app draws $2: lowest fps sample $fps"
    return
  fi
  bad "the app does not draw 30 fps $2: lowest '${fps:-none}'; phone.sh draws exit $rc, its last lines: $(tail -3 <<< "$out" | tr '\n' ' ')"
  kept=$((kept + 1))
  keep_phone_logs "$1" "draws-$kept"
}
