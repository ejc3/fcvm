#!/usr/bin/env bash
# Run a command (a VM test lane) with every VMM confined and its seccomp policy in log mode,
# then fail if the kernel logged any syscall the policy does not list for a Firecracker
# thread. A logged record is a syscall the enforcing policy would trap.
#
# It fails closed: when auditd is not running (the kernel then printk's records and
# rate-limits them, so a clean result would mean nothing), when the audit log cannot be read,
# when the positive control's record does not appear, and when the kernel is below the
# confinement floor.
#
# Usage: scripts/confine-seccomp-audit.sh COMMAND...
set -uo pipefail

fail() { echo "confine-seccomp-audit: BLOCKED: $*" >&2; exit 2; }
for tool in python3 sudo; do command -v "$tool" >/dev/null || fail "'$tool' missing"; done
log=/var/log/audit/audit.log
systemctl is-active --quiet auditd || fail "auditd is not running; without it the kernel rate-limits seccomp records"
sudo test -r "$log" || fail "cannot read $log"

# Positive control: a process that logs one syscall through a SECCOMP_RET_LOG filter must
# leave a record, or the lane cannot see anything.
marker_comm="fcvm-audit-ctl"
start_line=$(sudo wc -l < "$log")
python3 - "$marker_comm" <<'PY' || fail "the positive control could not install its filter"
import ctypes, struct, sys
libc = ctypes.CDLL(None, use_errno=True)
PR_SET_NAME, PR_SET_NO_NEW_PRIVS, PR_SET_SECCOMP, SECCOMP_MODE_FILTER = 15, 38, 22, 2
libc.prctl(PR_SET_NAME, sys.argv[1].encode(), 0, 0, 0)
assert libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
# BPF: return SECCOMP_RET_LOG for every syscall (allowed, and audited).
prog = struct.pack("HBBI", 0x06, 0, 0, 0x7ffc0000)
buf = ctypes.create_string_buffer(prog)
class Fprog(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]
fprog = Fprog(1, ctypes.cast(buf, ctypes.c_void_p))
assert libc.prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ctypes.byref(fprog), 0, 0) == 0
libc.getppid()
PY
sleep 1
sudo tail -n +"$((start_line + 1))" "$log" | grep -q "type=SECCOMP.*comm=\"$marker_comm\"" \
  || fail "the positive control's seccomp record did not reach $log"

start_line=$(sudo wc -l < "$log")
FCVM_CONFINE=required FCVM_CONFINE_SECCOMP=log "$@"
status=$?
records=$(sudo tail -n +"$((start_line + 1))" "$log" | grep 'type=SECCOMP' \
  | grep -E 'exe="[^"]*/firecracker[^/"]*"')
if [ -n "$records" ]; then
  echo "confine-seccomp-audit: syscalls the confined policy does not list:" >&2
  python3 - <<PY >&2
import re, collections
c = collections.Counter()
for l in """$records""".splitlines():
    m = re.search(r'comm="([^"]+)".*syscall=(\d+)', l)
    if m: c[(m.group(1), int(m.group(2)))] += 1
for (comm, nr), n in sorted(c.items()): print(f"  {comm} syscall {nr} x{n}")
PY
  exit 1
fi
[ "$status" -eq 0 ] || { echo "confine-seccomp-audit: the lane failed ($status)" >&2; exit "$status"; }
echo "confine-seccomp-audit: clean, no unlisted syscall logged"
