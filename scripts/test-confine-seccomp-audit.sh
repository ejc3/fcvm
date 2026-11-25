#!/usr/bin/env bash
# Tests of scripts/confine-seccomp-audit.sh's verdicts. Needs auditd, sudo and python3, so CI
# does not run it (its containers have no auditd); run it by hand after changing the script.
# A lane here is a process named like a Firecracker binary that logs one syscall through a
# SECCOMP_RET_LOG filter, which is the record a confined VMM in log mode leaves.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
for tool in python3 sudo auditctl; do
  command -v "$tool" >/dev/null || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
systemctl is-active --quiet auditd || { echo "BLOCKED: auditd is not running" >&2; exit 2; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$(command -v python3)" "$work/firecracker-audit-test"
cat > "$work/log-one-syscall.py" <<'PY'
import ctypes, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
if len(sys.argv) > 1:
    libc.prctl(15, sys.argv[1].encode(), 0, 0, 0)  # PR_SET_NAME
assert libc.prctl(38, 1, 0, 0, 0) == 0  # PR_SET_NO_NEW_PRIVS
buf = ctypes.create_string_buffer(struct.pack("HBBI", 0x06, 0, 0, 0x7ffc0000))  # RET_LOG
class Fprog(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]
assert libc.prctl(22, 2, ctypes.byref(Fprog(1, ctypes.cast(buf, ctypes.c_void_p))), 0, 0) == 0
libc.getppid()
PY
cat > "$work/logs.sh" <<EOF2
#!/bin/bash
"$work/firecracker-audit-test" "$work/log-one-syscall.py"; sleep 1
EOF2
cat > "$work/logs-as-a-vcpu.sh" <<EOF2
#!/bin/bash
"$work/firecracker-audit-test" "$work/log-one-syscall.py" "fc_vcpu 0"; sleep 1
EOF2
cat > "$work/logs-then-rotates.sh" <<EOF2
#!/bin/bash
"$work/firecracker-audit-test" "$work/log-one-syscall.py"; sleep 1
sudo pkill -USR1 -x auditd; sleep 2
EOF2
chmod +x "$work"/*.sh
failures=0
expect() { # NAME WANTED-STATUS WANTED-TEXT COMMAND...
  local name=$1 want=$2 text=$3; shift 3
  local out; out=$("$here/confine-seccomp-audit.sh" "$@" 2>&1); local got=$?
  if [ "$got" = "$want" ] && grep -q "$text" <<< "$out"; then
    echo "ok: $name"
  else
    echo "FAIL: $name: exit $got (wanted $want with '$text'):"; sed 's/^/    /' <<< "$out"
    failures=$((failures + 1))
  fi
}
expect "a lane that logs nothing is clean" 0 "clean, no unlisted syscall logged" true
expect "a logged syscall fails the lane and is named" 1 "firecracker-aud syscall" "$work/logs.sh"
# The kernel writes a thread name with a space in it as hex; the summary named no thread for
# such a record.
expect "a vCPU thread's record is named" 1 "fc_vcpu 0 syscall" "$work/logs-as-a-vcpu.sh"
# The records of a lane during which auditd rotated its log are not all in the file the lane
# started with: reading on from the old line number in the new file found nothing and said
# clean.
expect "a rotated log blocks" 2 "audit records are missing" "$work/logs-then-rotates.sh"
[ "$failures" -eq 0 ] || { echo "$failures failed" >&2; exit 1; }
echo "all passed"
