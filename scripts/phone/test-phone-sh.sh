#!/bin/bash
# fleet/phone/phone.sh against a stub adb: `bugreports` counts the bug reports in the events
# log, prints 0 for a log with none, and fails when adb cannot read the log, so run.sh's
# check cannot pass on a phone it could not ask.
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
exit $fail
