#!/usr/bin/env bash
# Run a command (a VM test lane) with every VMM confined and its seccomp policy in log mode,
# then fail if the kernel logged any syscall the policy does not list for a Firecracker
# thread. A logged record is a syscall the enforcing policy would trap.
#
# It fails closed: when auditd is not running (the kernel then printk's records and
# rate-limits them, so a clean result would mean nothing), when the audit log cannot be read
# or its lines cannot be counted,
# when a positive control's record does not appear (one runs before the lane and one after
# it, which is also the wait for auditd to write the lane's records), when auditd stopped or
# rotated its log during the
# lane or the kernel dropped a record (auditd keeps num_logs files of max_log_file, 5 of
# 8 MB by default, so a syscall logged in a loop pushes earlier records out of every file),
# and when the kernel is below the confinement floor.
#
# Usage: scripts/confine-seccomp-audit.sh COMMAND...
set -uo pipefail

fail() { echo "confine-seccomp-audit: BLOCKED: $*" >&2; exit 2; }
for tool in python3 sudo auditctl; do command -v "$tool" >/dev/null || fail "'$tool' missing"; done
log=/var/log/audit/audit.log
systemctl is-active --quiet auditd || fail "auditd is not running; without it the kernel rate-limits seccomp records"
sudo test -r "$log" || fail "cannot read $log"

# Positive control: a process that logs one syscall through a SECCOMP_RET_LOG filter must
# leave a record, or the lane cannot see anything. It runs before the lane and again after
# it: auditd writes records in order, so once the closing control's record is in the log,
# every record of the lane is too, and an auditd that stopped during the lane is caught.
control_program=$(cat <<'PY'
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
)
# control COMM FROM_LINE: log one syscall as COMM, then wait up to 10 s for its record.
control() {
  python3 -c "$control_program" "$1" || fail "the positive control could not install its filter"
  local tries
  for tries in $(seq 50); do
    sudo tail -n +"$(($2 + 1))" "$log" | grep -q "type=SECCOMP.*comm=\"$1\"" && return 0
    sleep 0.2
  done
  return 1
}
# The log's line count. The log is read with privilege everywhere: /var/log/audit is root's,
# and a redirect (`sudo wc -l < "$log"`) is opened by this shell, not by sudo. Where this user
# cannot read the log that gave an empty count, every later read started at line 1, and an
# earlier run's control record stood in for this run's.
log_lines() {
  local lines
  lines=$(sudo cat "$log" | wc -l) || fail "cannot read $log to count its lines"
  case "$lines" in
    '' | *[!0-9]*) fail "the line count of $log is not a number: '$lines'" ;;
  esac
  echo "$lines"
}
start_line=$(log_lines) || exit 2
control fcvm-audit-ctl "$start_line" || fail "the positive control's seccomp record did not reach $log"

# The log file's inode changes when auditd rotates it; `lost` counts the records the kernel
# could not queue.
log_id() { sudo stat -c %i "$log"; }
lost() { sudo auditctl -s | awk '$1 == "lost" { print $2 }'; }
# Per thread and syscall number, the Firecracker records on stdin. The kernel writes a
# thread name that holds a space (the vCPU threads are "fc_vcpu N") as hex, unquoted.
summary() {
  python3 -c '
import re, sys, collections
c = collections.Counter()
for l in sys.stdin:
    m = re.search(r"comm=(?:\"([^\"]*)\"|([0-9A-Fa-f]+)) .*syscall=(\d+)", l)
    if not m:
        c[("(unparsed record)", -1)] += 1
        continue
    comm = m.group(1) if m.group(1) is not None else bytes.fromhex(m.group(2)).decode(errors="replace")
    c[(comm, int(m.group(3)))] += 1
for (comm, nr), n in sorted(c.items()): print(f"  {comm} syscall {nr} x{n}")
'
}
# The seccomp records on stdin whose executable is a Firecracker binary. The kernel writes
# exe quoted, or as hex when the path holds a space or a quote.
firecracker_records() {
  python3 -c '
import os, re, sys
for l in sys.stdin:
    if "type=SECCOMP" not in l:
        continue
    m = re.search(r" exe=(?:\"([^\"]*)\"|([0-9A-Fa-f]+))(?: |$)", l)
    if not m:
        continue
    exe = m.group(1) if m.group(1) is not None else bytes.fromhex(m.group(2)).decode(errors="replace")
    if os.path.basename(exe).startswith("firecracker"):
        sys.stdout.write(l)
'
}

start_line=$(log_lines) || exit 2
start_id=$(log_id)
start_lost=$(lost)
[ -n "$start_id" ] && [ -n "$start_lost" ] || fail "cannot read $log's inode or the kernel's lost-record count"
FCVM_CONFINE=required FCVM_CONFINE_SECCOMP=log "$@"
status=$?
end_lost=$(lost)
if [ "$(log_id)" != "$start_id" ] || [ "$end_lost" != "$start_lost" ]; then
  echo "confine-seccomp-audit: Firecracker records in the newest log file (the list is incomplete):" >&2
  sudo cat "$log" | firecracker_records | summary >&2
  fail "audit records are missing: auditd rotated $log during the lane, or the kernel dropped records (lost $start_lost -> $end_lost). A syscall logged in a loop does this; add the ones above to the policy, or raise max_log_file in /etc/audit/auditd.conf, and run again"
fi
systemctl is-active --quiet auditd || fail "auditd stopped during the lane, so its records are incomplete"
control fcvm-audit-end "$start_line" \
  || fail "the closing control's seccomp record did not reach $log within 10 s: records the lane logged may not be there either"
if [ "$(log_id)" != "$start_id" ] || [ "$(lost)" != "$start_lost" ]; then
  fail "audit records are missing: auditd rotated $log or the kernel dropped records while the closing control ran"
fi
records=$(sudo tail -n +"$((start_line + 1))" "$log" | firecracker_records)
if [ -n "$records" ]; then
  echo "confine-seccomp-audit: syscalls the confined policy does not list:" >&2
  summary <<< "$records" >&2
  exit 1
fi
[ "$status" -eq 0 ] || { echo "confine-seccomp-audit: the lane failed ($status)" >&2; exit "$status"; }
echo "confine-seccomp-audit: clean, no unlisted syscall logged"
