#!/usr/bin/env python3
"""Request-optimized A/B: resident CDP + fast teardown, with honest kernel accounting.

THREE ARMS, so the two independent changes can be attributed separately instead
of being reported as one lump:

  exec      today's path. `fcvm snapshot run --exec <python driver>`: exec
            handshake, guest python interpreter boot, in-guest CDP connect,
            render, then fcvm's own SEQUENTIAL teardown, all awaited.
  cdp       resident render. The clone is restored with no --exec; the host
            drives Chromium's DevTools endpoint over fcvm's port forwarding.
            Teardown is still fcvm's: SIGTERM, then await the full sequence.
            (exec -> cdp isolates PART 1.)
  cdp-fast  same request path, fast teardown: the response is delivered the
            instant the image is in hand, THEN one SIGKILL to fcvm.
            (cdp -> cdp-fast isolates PART 2.)

WHY ONE SIGNAL IS THE CONCURRENT KILL
-------------------------------------
`kill(fcvm, SIGKILL)` is not "kill the parent and hope". fcvm arms
`PR_SET_PDEATHSIG=SIGKILL` on each of its three long-lived children:
Firecracker (`src/utils.rs::install_namespace_pre_exec`), the namespace holder
(`src/commands/common.rs::spawn_namespace_holder`) and pasta
(`src/network/pasta.rs`). When fcvm dies the kernel's
`exit_notify`/`forget_original_parent` walks its child list and delivers SIGKILL
to EVERY child with a pdeathsig in one pass, before anything else can run. So the
kills are issued concurrently by construction — there is no ordering to get
wrong, no `.await` between them, and no code of ours that has to survive to do it.

WHAT THAT GUARANTEE DOES AND DOES NOT COVER. It is kernel-enforced for all three
hops, but pasta's arming carries a precondition the other two do not:
`commit_creds()` zeroes `pdeath_signal` whenever uid/gid change or
`cred_cap_issubset(old, new)` fails, and pasta `setns()`es into the holder's user
namespace after the `pre_exec`. Under sudo that is a capability LOSS, so the
subset test passes and the signal survives; run fcvm genuinely unprivileged and it
becomes a capability GAIN, the kernel clears the signal, and pasta falls back to
passt's own 1-second PID watch of the holder. So: kernel-enforced for the VMM and
the holder unconditionally, and for pasta while fcvm runs as root. This harness
does not assume any of it — `teardown_fast` waits on a pidfd per child and
REFUSES to reap on-disk state if any of them is still alive (see there).

AGENTS.md is explicit: "Prefer kernel-enforced reaping over cleanup code. A Drop
impl, a signal handler, or an always() cleanup step does not run when the process
is SIGKILLed — which is exactly the case that leaked." PR #730 restored precisely
this chain after a privilege boundary broke it and ~490 microVMs leaked. This arm
depends on the same guarantee the leak fix established;
`test_sigkill_reaps_rootless_vm_tree` plus
`test_bench_fast_teardown_leaks_nothing_clone` (tests/test_signal_cleanup.rs) are
its regression proof, and both require firecracker, holder AND pasta to be found
before asserting, so a discovery drift fails loudly instead of silently checking
nothing.

NO JANITOR. On-disk reaping (state file, its lock, data dir) is done
synchronously, right here, after the clock stops. It is off the caller's critical
path but it is not deferred to some sweeper that might not run — and it is
MEASURED, not hidden. SIGKILL cannot be caught, so fcvm's `cleanup_vm` never runs
and both artifacts survive the kill; reaping them here is REQUIRED, not an
optimization.

WHAT IS ACTUALLY BEING CLAIMED (part 3)
---------------------------------------
The claim under test is: "early response converts teardown from LATENCY into
THROUGHPUT cost." Converts. Not removes. So this harness refuses to report one
number and reports three:

  blocking_ms       what the caller waits. Spawn -> image in hand.
  reap_wall_ms      SIGKILL -> both processes actually gone (pidfd readable).
                    The caller does not wait for this. The MACHINE does.
  reclaim_cpu_ms    CPU burned tearing the address space down, from
                    /proc/<pid>/stat utime+stime.

`reclaim_cpu_ms` is a real number, not an estimate, and here is why it is
trustworthy: `do_exit()` runs `exit_mm()` — which unmaps and frees the whole
address space, the expensive part for a ~1 GB VM — BEFORE `exit_notify()` turns
the task into a zombie. A `/proc/<pid>/stat` read taken while the task is in
state `Z` therefore already includes all of the reclaim. The sampler records
whether it caught the `Z` state (`zombie_seen`); when it did, the CPU figure is
COMPLETE, and when it did not (the parent's reaper won the race) the figure is a
LOWER BOUND and is labelled as one. Never averaged together.

Whole-machine `/proc/stat` busy-jiffy deltas are recorded over the reclaim window
and against a post-terminal ambient control. A pre-kill control includes the
still-running VM's ordinary CPU and subtracts work absent from reclaim, so that
older accounting was withdrawn.

At saturation that CPU competes with new requests. A latency win is not a
capacity win, and this harness deliberately makes it impossible to report one as
the other.
"""

import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import os
import platform
import random
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time
import uuid
from urllib.parse import urlparse, urlunparse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

CLK_TCK = os.sysconf("SC_CLK_TCK")


# ---------------------------------------------------------------- procfs utils


def proc_stat_fields(pid: int):
    """(state, utime_ticks, stime_ticks, starttime) or None if the pid is gone.

    `comm` (field 2) can contain spaces and parentheses, so split after the LAST
    `") "` — the standard trap in /proc/<pid>/stat parsing.
    """
    try:
        with open(f"/proc/{pid}/stat") as f:
            raw = f.read()
    except OSError:
        return None
    try:
        after = raw.rsplit(") ", 1)[1]
        f = after.split()
        return f[0], int(f[11]), int(f[12]), int(f[19])
    except (IndexError, ValueError):
        return None


def machine_busy_jiffies():
    """Non-idle jiffies from /proc/stat's aggregate line."""
    with open("/proc/stat") as f:
        parts = f.readline().split()
    # guest/guest_nice (fields 9/10) are already included in user/nice. Summing
    # them again double-counts exactly the VM CPU this benchmark is measuring.
    vals = [int(x) for x in parts[1:9]]
    idle = vals[3] + vals[4]  # idle + iowait
    return sum(vals) - idle, sum(vals)


def self_cpu_ms() -> float:
    """This harness's OWN cpu (utime+stime), in ms.

    Subtracted from BOTH accounting windows so the sampler's own load cancels by
    construction instead of being attributed to kernel reclaim. Without this the
    control window and the reclaim window carry different amounts of harness CPU
    and the subtraction removes work that was never done in the window it is
    subtracted from.
    """
    s = proc_stat_fields(os.getpid())
    return (s[1] + s[2]) * 1000.0 / CLK_TCK if s else 0.0


def children_of(pid: int) -> list[int]:
    """Direct children via /proc/<pid>/task/<tid>/children."""
    out = []
    try:
        for tid in os.listdir(f"/proc/{pid}/task"):
            try:
                with open(f"/proc/{pid}/task/{tid}/children") as f:
                    out += [int(x) for x in f.read().split()]
            except OSError:
                pass
    except OSError:
        pass
    return out


def children_of_frozen(pid: int) -> list[int]:
    """Enumerate every direct child after ``pid`` has entered group stop.

    This is a safety boundary, not a diagnostic best effort. Once the parent is
    frozen it cannot fork another child, so any procfs read failure means we have
    not proved the child set and must retain its state and disk.
    """
    children: set[int] = set()
    for tid in os.listdir(f"/proc/{pid}/task"):
        with open(f"/proc/{pid}/task/{tid}/children") as child_file:
            children.update(int(value) for value in child_file.read().split())
    return sorted(children)


def proc_comm(pid: int) -> str:
    try:
        with open(f"/proc/{pid}/comm") as f:
            return f.read().strip()
    except OSError:
        return ""


_libc = ctypes.CDLL("libc.so.6", use_errno=True)


def pidfd_open(pid: int):
    """A handle that refers to THIS exact process for its whole lifetime.

    Used instead of polling /proc/<pid>: a pidfd can never alias a reused PID, and
    poll() on it becomes readable exactly when the process exits — so the
    "is it gone yet" wait is an event, not a timer. AGENTS.md: no sleeps,
    event-driven only.
    """
    fd = _libc.syscall(434, ctypes.c_int(pid), ctypes.c_uint(0))  # SYS_pidfd_open
    if fd < 0:
        error = ctypes.get_errno()
        if error == errno.ESRCH:
            return None
        raise OSError(error, os.strerror(error), f"pidfd_open({pid})")
    return fd


def pidfd_send_signal(fd: int, sig: int) -> None:
    """Signal the exact process pinned by ``fd``; a recycled PID is irrelevant."""
    result = _libc.syscall(
        424, ctypes.c_int(fd), ctypes.c_int(sig), ctypes.c_void_p(), ctypes.c_uint(0)
    )
    if result < 0:
        error = ctypes.get_errno()
        if error != errno.ESRCH:
            raise OSError(error, os.strerror(error), "pidfd_send_signal")


def wait_pidfds(fds: list[int], timeout_s: float, interruptible: bool = False) -> bool:
    """Block until every pidfd is readable (process exited) or the deadline passes."""
    remaining = [fd for fd in fds if fd is not None]
    deadline = time.monotonic() + timeout_s
    poller = select.poll()
    for fd in remaining:
        poller.register(fd, select.POLLIN)
    while remaining:
        left = deadline - time.monotonic()
        # The signal handler records an interrupt instead of raising from an
        # arbitrary ownership handoff.  Poll in bounded slices so a requested
        # shutdown reaches the request scope promptly; the scope then performs
        # exact teardown before re-raising HarnessInterrupted.
        ready = poller.poll(max(0.0, min(left, 0.1)) * 1000)
        for fd, _ev in ready:
            poller.unregister(fd)
            remaining.remove(fd)
        if not ready and deadline - time.monotonic() <= 0:
            return False
        if interruptible:
            raise_if_harness_interrupted()
    return True


def freeze_and_capture_children(pid: int) -> tuple[list[int], list[int | None]]:
    """Freeze a live direct child, then capture its complete child set.

    Reading ``/proc/<pid>/children`` after an exit loses the attribution, while
    reading it from a running parent races a final fork.  SIGSTOP plus waitid is
    the kernel synchronization point: once CLD_STOPPED is observed, this parent
    can neither fork nor exit until the caller signals it again.  WNOWAIT leaves
    ownership of the eventual exit status with ``subprocess.Popen``.
    """
    stop_sent = False
    captured_fds: list[int | None] = []
    try:
        os.kill(pid, signal.SIGSTOP)
        stop_sent = True
    except ProcessLookupError as error:
        raise RuntimeError(f"fcvm {pid} exited before child attribution") from error
    try:
        status = os.waitid(
            os.P_PID,
            pid,
            os.WSTOPPED | os.WEXITED | os.WNOWAIT,
        )
        if status is None or status.si_code != os.CLD_STOPPED:
            raise RuntimeError(
                f"fcvm {pid} exited before it could be frozen for child attribution"
            )
        kids = children_of_frozen(pid)
        for child in kids:
            child_fd = pidfd_open(child)
            captured_fds.append(child_fd)
        return kids, captured_fds
    except (ChildProcessError, OSError) as error:
        raise RuntimeError(f"cannot establish child attribution for fcvm {pid}: {error}")
    finally:
        # Successful callers deliberately receive a stopped parent and choose
        # TERM+CONT or KILL.  A failed capture must never strand the VM stopped.
        if stop_sent and sys.exc_info()[0] is not None:
            for fd in captured_fds:
                if fd is not None:
                    os.close(fd)
            try:
                os.kill(pid, signal.SIGCONT)
            except ProcessLookupError:
                pass


def close_pidfds(fds) -> None:
    """Close every live pidfd exactly once."""
    for fd in fds:
        if fd is not None:
            os.close(fd)


def abort_frozen_owner(
    pid: int,
    parent_fd: int | None,
    child_fds: list[int | None],
    timeout_s: float = 10.0,
) -> bool:
    """Kill and await a frozen exact owner set without touching its disk."""
    # SIGKILL wakes and terminates a stopped task. Never SIGCONT first: that
    # would open a scheduler window in which the owner can fork an unpinned child.
    # A missing parent pidfd is not permission to signal the numeric PID: the
    # failed pin means the original process may already have exited and that
    # number may now identify an unrelated process. Retain the disk and report
    # the lifecycle proof as failed instead.
    if parent_fd is not None:
        try:
            pidfd_send_signal(parent_fd, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass
    for child_fd in child_fds:
        if child_fd is None:
            continue
        try:
            pidfd_send_signal(child_fd, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass
    exact_set_gone = wait_pidfds(
        [fd for fd in [parent_fd, *child_fds] if fd is not None],
        timeout_s,
    )
    return parent_fd is not None and exact_set_gone


SAMPLE_PERIOD_S = 0.0002
SPAWN_SIGNALS = {signal.SIGINT, signal.SIGTERM}


def sample_all_until_gone(
    pids: dict,
    initial_stats: dict,
    deadline: float,
) -> tuple[dict, float]:
    """Sample EVERY child's /proc/<pid>/stat concurrently until each one vanishes.

    Concurrently, not one at a time. The previous version was a per-child call
    inside a dict comprehension, so child 2 was not sampled at all until child 1
    had already gone. Children exit on wildly different timescales after a single
    SIGKILL (the namespace holder in a fraction of a millisecond, Firecracker in
    tens), so a fast child that was not first in `children_of()` order was ALWAYS
    already reaped by the time its turn came: `proc_stat_fields` returned None on
    the first read, `last` stayed None, and its `reclaim_cpu_ms` was recorded as
    `null`. That is not a lower bound, it is a missing measurement whose presence
    depended on procfs ordering.

    IT SLEEPS BETWEEN PASSES, and the period is returned so the residual bias is
    quantifiable. It used to be a tight loop with no sleep, defended on the
    grounds that catching state `Z` upgrades a CPU figure from a lower bound to a
    complete one and that the cost of the spin was "measured and subtracted".
    Accounting is not throttling, and the subtraction could not survive its own
    arithmetic. Measured on this box against a child that outlives its parent by
    ~0.5 s:

        reap_wall_ms=541.3  harness_cpu_ms=540.0   (100% of one core)
        machine_cpu_ms=630.0  machine_cpu_ms_excess=-17.9

    Three consequences, all INSIDE the window being measured. (a) `machine_cpu_ms
    - self_ms` subtracts a ~540 ms quantity that is itself quantized to one jiffy
    in order to expose a signal REVIEW.md states is below /proc tick resolution
    (< 20 ms); the noise on the subtrahend is the size of the signal, and the run
    above produced a NEGATIVE excess — impossible for a reclaim, and direct
    evidence the attribution was broken. (b) The control window SLEEPS while this
    one SPUN, so the two windows carried opposite harness load profiles — exactly
    the defect the `+610.4 ms` machine-cost figure was withdrawn for, inverted
    onto the other side of the same subtraction. (c) `reap_wall_ms` itself is
    corrupted, because the spin competes for CPU with the exiting Firecracker it
    is timing.

    At 200 us the zombie argument still holds: `exit_mm()` completes before
    `exit_notify()`, so the reclaim is already in the counters by the time state
    `Z` is reachable, and MISSING the `Z` only downgrades that child's figure to a
    labelled LOWER BOUND — a state the record already models (`complete`, and
    reqanalyze prints the two populations separately, never averaged).
    """
    live = dict(pids)
    last: dict = dict(initial_stats)
    starttimes = {
        name: fields[3] if fields is not None else None
        for name, fields in initial_stats.items()
    }
    zombie: dict = {name: False for name in pids}
    while live and time.monotonic() < deadline:
        for name, pid in list(live.items()):
            s = proc_stat_fields(pid)
            # A pidfd pins lifetime, but procfs is addressed by the numeric PID.
            # Once that number is reused, its counters belong to another process.
            if s is None or (
                starttimes.get(name) is not None
                and s[3] != starttimes[name]
            ):
                del live[name]
                continue
            last[name] = s
            if s[0] in ("Z", "X", "x"):
                zombie[name] = True
                del live[name]
        if live:
            time.sleep(SAMPLE_PERIOD_S)
    out = {}
    for name in pids:
        s = last.get(name)
        out[name] = {
            "cpu_ms": (s[1] + s[2]) * 1000.0 / CLK_TCK if s else None,
            "zombie_seen": zombie[name],
        }
    return out, SAMPLE_PERIOD_S


# ------------------------------------------------------------------ fcvm glue


IN_CLOSE_WRITE = 0x00000008
IN_MOVED_TO = 0x00000080
IN_CREATE = 0x00000100


class DirWatch:
    """inotify watch on a directory. REGISTER BEFORE THE SPAWN THAT CAN TRIGGER IT.

    fcvm itself uses exactly this discipline (`crate::utils::DirWatch`, e.g.
    `src/network/pasta.rs`: "Register the inotify watch BEFORE ... the spawn"), and
    for the same reason: a watch registered after the spawn can miss the event and
    then wait for one that has already happened.
    """

    def __init__(self, path: str):
        self.fd = _libc.inotify_init1(os.O_NONBLOCK)
        if self.fd < 0:
            raise OSError(ctypes.get_errno(), f"inotify_init1 on {path}")
        wd = _libc.inotify_add_watch(
            self.fd, path.encode(), IN_CREATE | IN_MOVED_TO | IN_CLOSE_WRITE
        )
        if wd < 0:
            err = ctypes.get_errno()
            os.close(self.fd)
            self.fd = -1
            raise OSError(err, f"inotify_add_watch on {path}")

    def drain(self) -> None:
        """Consume queued events. Call BEFORE scanning, never after: an event that
        lands between a scan and a drain would be thrown away, and the next wait
        would block for a change that already happened."""
        while True:
            try:
                if not os.read(self.fd, 65536):
                    return
            except BlockingIOError:
                return
            except OSError:
                return

    def wait(self, timeout_s: float) -> bool:
        """Block until the directory changes. Returns False on timeout."""
        if timeout_s <= 0:
            return False
        poller = select.poll()
        poller.register(self.fd, select.POLLIN)
        return bool(poller.poll(timeout_s * 1000))

    def close(self) -> None:
        if self.fd >= 0:
            os.close(self.fd)
            self.fd = -1


def state_path_baseline(state_dir: str) -> frozenset[str]:
    """Snapshot state paths that existed before a clone was spawned.

    A clone first publishes a name-bearing state with a null PID.  Name alone
    cannot distinguish that new record from debris left by an earlier refused
    clone using the same run ID.  Callers take this snapshot after installing
    the directory watch but before ``Popen`` and permanently exclude every path
    in it from this clone's discovery and recovery scope.
    """
    try:
        return frozenset(
            os.path.join(state_dir, entry)
            for entry in os.listdir(state_dir)
            if entry.endswith(".json")
        )
    except OSError as error:
        raise RuntimeError(f"cannot snapshot state directory {state_dir}: {error}")


def scan_state(
    state_dir: str,
    fcvm_pid: int = 0,
    name: str = "",
    fcvm_start_time: int | None = None,
    excluded_paths: frozenset[str] = frozenset(),
    allow_unowned: bool = True,
):
    """One pass over the state dir, preferring the run-unique VM name.

    The name-keyed match is not a convenience: `allocate_loopback_ip` saves the
    state file while `vm_state.pid` is still null (the pid is only set POST-RESUME,
    `src/commands/common.rs`), so there is a whole window — network setup, mount
    namespace, volume servers, the restore itself — in which the file exists,
    Firecracker may already be running, and a pid-keyed scan returns nothing. The
    name IS set before the first save, so it is the only key that covers that
    window. Pre-spawn paths are excluded because name alone is discovery, not
    ownership; destructive cleanup separately requires the exact PID start time.
    """
    try:
        names = os.listdir(state_dir)
    except OSError:
        return None, None
    for fname in names:
        if not fname.endswith(".json"):
            continue
        path = os.path.join(state_dir, fname)
        if path in excluded_paths:
            continue
        try:
            with open(path) as f:
                st = json.load(f)
        except (OSError, ValueError):
            continue
        if name:
            # A numeric PID can be reused while a stale state file still names
            # its old owner.  When the caller has the run-unique name, require
            # that identity and use the PID only to reject a state now owned by
            # somebody else.  A null PID is the expected pre-resume shape.
            if st.get("name") != name:
                continue
            owner = st.get("pid")
            if owner is None and allow_unowned:
                return path, st
            if (
                fcvm_pid
                and fcvm_start_time is not None
                and owner == fcvm_pid
                and st.get("pid_start_time") == fcvm_start_time
            ):
                return path, st
            continue
        if (
            fcvm_pid
            and fcvm_start_time is not None
            and st.get("pid") == fcvm_pid
            and st.get("pid_start_time") == fcvm_start_time
        ):
            return path, st
    return None, None


def log_tail(path: str, limit: int = 4096) -> str:
    if not path:
        return ""
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            f.seek(max(0, f.tell() - limit))
            return f.read().decode("utf8", "replace").strip()
    except OSError:
        return ""


def exited_before(proc: subprocess.Popen, milestone: str, log_path: str = "") -> RuntimeError:
    rc = proc.poll()
    tail = log_tail(log_path)
    detail = f"; log tail: {tail}" if tail else ""
    return RuntimeError(
        f"clone process {proc.pid} exited with status {rc} before {milestone}{detail}"
    )


def spawned_process_start_time(proc: subprocess.Popen) -> int:
    """Capture the immutable procfs identity of the process returned by Popen."""
    fields = proc_stat_fields(proc.pid)
    if fields is None:
        raise exited_before(proc, "its process identity could be pinned")
    return fields[3]


def find_state(
    state_dir: str,
    fcvm_pid: int,
    deadline: float,
    watch=None,
    name: str = "",
    proc: subprocess.Popen | None = None,
    log_path: str = "",
    fcvm_start_time: int | None = None,
    excluded_paths: frozenset[str] = frozenset(),
):
    """Locate the clone's state file by fcvm PID (or name), EVENT-DRIVEN.

    Was a `while time.monotonic() < deadline:` loop with an `os.listdir` plus a
    full `json.load` of every file in it and NO sleep, no yield and no backoff. It
    therefore burned 100% of one core for its entire duration — measured at 400 ms
    of harness CPU for a 400 ms wall wait — while the 2-vCPU clone it is waiting on
    was restoring on the same box. `wait_port` a few lines below already had a
    backoff ladder, so the omission was local, not a policy.

    Now it blocks in `poll()` on an inotify fd and rescans only when the directory
    actually changes: zero CPU while waiting, and it notices the file sooner than a
    poll loop would. `discover_ms` still records the wall cost.
    """
    own = watch is None
    if own:
        watch = DirWatch(state_dir)
    pid_fd = pidfd_open(fcvm_pid) if proc is not None else None
    poller = select.poll()
    poller.register(watch.fd, select.POLLIN)
    if pid_fd is not None:
        poller.register(pid_fd, select.POLLIN)
    try:
        while True:
            raise_if_harness_interrupted()
            watch.drain()
            path, st = scan_state(
                state_dir,
                fcvm_pid,
                name,
                fcvm_start_time,
                excluded_paths,
            )
            if st is not None:
                return path, st
            if proc is not None and proc.poll() is not None:
                raise exited_before(proc, "publishing its state file", log_path)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                watch.drain()
                path, st = scan_state(
                    state_dir,
                    fcvm_pid,
                    name,
                    fcvm_start_time,
                    excluded_paths,
                )
                if st is not None:
                    return path, st
                if proc is not None and proc.poll() is not None:
                    raise exited_before(proc, "publishing its state file", log_path)
                return None, None
            timeout = min(remaining, 0.1)
            if not poller.poll(timeout * 1000) and timeout == remaining:
                watch.drain()
                path, st = scan_state(
                    state_dir,
                    fcvm_pid,
                    name,
                    fcvm_start_time,
                    excluded_paths,
                )
                if st is not None:
                    return path, st
                if proc is not None and proc.poll() is not None:
                    raise exited_before(proc, "publishing its state file", log_path)
                return None, None
    finally:
        if pid_fd is not None:
            os.close(pid_fd)
        if own:
            watch.close()


def wait_state_owned(
    state_path: str,
    fcvm_pid: int,
    deadline: float,
    watch: DirWatch,
    proc: subprocess.Popen,
    fcvm_start_time: int,
    expected_name: str = "",
):
    """Wait until the state records the spawned fcvm's exact process identity.

    fcvm publishes the state by name before restore, while ``pid`` is still
    null, then atomically replaces it after resume with the owner PID.  Killing
    fcvm in that interval bypasses its signal handler and leaves an unsweepable
    null-PID state file plus the clone disk.  The noop arm records port readiness
    at the first successful connect, then waits here before beginning teardown.

    Watch both the directory and the process pidfd.  A clone that exits before
    claiming its state is a failure immediately, not a reason to consume the
    rest of a 120-second benchmark deadline.
    """
    pid_fd = pidfd_open(fcvm_pid)
    poller = select.poll()
    poller.register(watch.fd, select.POLLIN)
    if pid_fd is not None:
        poller.register(pid_fd, select.POLLIN)
    try:
        while True:
            raise_if_harness_interrupted()
            watch.drain()
            try:
                with open(state_path) as f:
                    state = json.load(f)
            except (OSError, ValueError):
                state = None
            if (
                state is not None
                and state.get("pid") == fcvm_pid
                and state.get("pid_start_time") == fcvm_start_time
                and (not expected_name or state.get("name") == expected_name)
                and state.get("lifecycle_ready") is True
            ):
                return state

            rc = proc.poll()
            if rc is not None:
                raise exited_before(proc, f"claiming state {state_path}")

            remaining = deadline - time.monotonic()
            if remaining <= 0:
                try:
                    with open(state_path) as f:
                        final_state = json.load(f)
                except (OSError, ValueError):
                    final_state = None
                if (
                    final_state is not None
                    and final_state.get("pid") == fcvm_pid
                    and final_state.get("pid_start_time") == fcvm_start_time
                    and (
                        not expected_name
                        or final_state.get("name") == expected_name
                    )
                    and final_state.get("lifecycle_ready") is True
                ):
                    return final_state
                if proc.poll() is not None:
                    raise exited_before(proc, f"claiming state {state_path}")
                raise TimeoutError(
                    f"state {state_path} never recorded ready owner "
                    f"({fcvm_pid}, {fcvm_start_time})"
                )
            # pidfds are available on every supported fcvm host. Keep a bounded
            # fallback only for platforms where Python cannot open one, so a
            # process exit can never turn into a full-deadline wait.
            timeout = min(remaining, 0.1)
            if not poller.poll(timeout * 1000):
                if timeout == remaining:
                    try:
                        with open(state_path) as f:
                            final_state = json.load(f)
                    except (OSError, ValueError):
                        final_state = None
                    if (
                        final_state is not None
                        and final_state.get("pid") == fcvm_pid
                        and final_state.get("pid_start_time") == fcvm_start_time
                        and (
                            not expected_name
                            or final_state.get("name") == expected_name
                        )
                        and final_state.get("lifecycle_ready") is True
                    ):
                        return final_state
                    if proc.poll() is not None:
                        raise exited_before(proc, f"claiming state {state_path}")
                    raise TimeoutError(
                        f"state {state_path} never recorded ready owner "
                        f"({fcvm_pid}, {fcvm_start_time})"
                    )
    finally:
        if pid_fd is not None:
            os.close(pid_fd)


def clone_cdp_endpoint(state: dict, port: int) -> str:
    """Host-side address of the clone's published CDP port.

    fcvm gives every VM a unique address so the SAME guest port can be published
    by many clones at once: rootless/pasta -> a unique 127.x.y.z loopback IP,
    bridged -> the veth's host IP with DNAT scoped to it, routed -> a unique
    loopback IP fronted by the built-in TCP proxy. Port mappings are inherited by
    a restored clone from the snapshot metadata
    (`src/commands/snapshot.rs`: `snapshot_config.metadata.port_mappings`), so
    `--publish 9222:9222` on the golden VM applies to every clone with no
    per-clone plumbing of ours.
    """
    net = state.get("config", {}).get("network", {}) or {}
    for key in ("loopback_ip", "host_ip", "guest_ip"):
        ip = net.get(key)
        if ip:
            return f"{ip}:{port}"
    raise RuntimeError(f"no usable host-side IP in network config: {sorted(net)}")


def clone_data_dir(data_root: str, state: dict) -> str:
    """Return the exact per-clone disk directory from a complete state record."""
    vm_id = state.get("vm_id")
    if not valid_vm_id(vm_id):
        raise RuntimeError(f"clone state has an invalid vm_id: {vm_id!r}")
    return os.path.join(data_root, "vm-disks", vm_id)


def wait_port(
    endpoint: str,
    deadline: float,
    proc: subprocess.Popen | None = None,
    log_path: str = "",
) -> float:
    """Retry-connect until the forwarded CDP port answers. Returns wait in ms.

    This is the readiness signal, and it is the minimum possible one: the first
    successful TCP connect to Chromium's own listener. There is no health poll,
    no exec handshake and no marker file in the request path.
    """
    import socket

    host, port = endpoint.rsplit(":", 1)
    t0 = time.monotonic()
    delay = 0.001
    while True:
        raise_if_harness_interrupted()
        if proc is not None:
            rc = proc.poll()
            if rc is not None:
                raise exited_before(proc, f"CDP port {endpoint} answered", log_path)
        try:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                if proc is not None and proc.poll() is not None:
                    raise exited_before(
                        proc, f"CDP port {endpoint} answered", log_path
                    )
                raise TimeoutError(f"CDP port {endpoint} never answered")
            s = socket.create_connection(
                (host, int(port)), min(0.25, remaining)
            )
            s.close()
            if time.monotonic() > deadline:
                if proc is not None and proc.poll() is not None:
                    raise exited_before(
                        proc, f"CDP port {endpoint} answered", log_path
                    )
                raise TimeoutError(
                    f"CDP port {endpoint} answered only after the deadline"
                )
            return (time.monotonic() - t0) * 1000
        except OSError:
            if proc is not None and proc.poll() is not None:
                raise exited_before(proc, f"CDP port {endpoint} answered", log_path)
            if time.monotonic() >= deadline:
                if proc is not None and proc.poll() is not None:
                    raise exited_before(
                        proc, f"CDP port {endpoint} answered", log_path
                    )
                raise TimeoutError(f"CDP port {endpoint} never answered")
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(delay, remaining))
            delay = min(delay * 1.5, 0.02)


def sha256_file(path: str) -> str:
    """Content identity for the exact fcvm binary used by the run."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def harness_sha256() -> str:
    """Content identity for every script that defines one request sample."""
    h = hashlib.sha256()
    h.update(b"fcvm-chromium-request-harness-v1\0")
    for name in ("reqbench.py", "cdpdrive.py", "render.py", "reqbench.sh"):
        encoded = name.encode()
        h.update(len(encoded).to_bytes(4, "big"))
        h.update(encoded)
        with open(os.path.join(HERE, name), "rb") as f:
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                h.update(chunk)
    return h.hexdigest()


def command_text(argv: list[str]) -> str:
    """Best-effort provenance command; the binary hash remains authoritative."""
    try:
        return subprocess.check_output(argv, text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def snapshot_generation(data_root: str, snapshot_name: str) -> dict:
    """Authoritative generation and runtime shape for a snapshot tag.

    A tag can be deleted and recreated. Pooling on the tag alone would merge
    records from different memory/disk generations, so the metadata also carries
    the snapshot's creation timestamp and source VM UUID from config.json.
    """
    path = os.path.join(data_root, "snapshots", snapshot_name, "config.json")
    try:
        with open(path) as f:
            config = json.load(f)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"cannot identify snapshot generation from {path}: {error}")
    created_at = config.get("created_at")
    vm_id = config.get("vm_id")
    if not isinstance(created_at, str) or not created_at:
        raise RuntimeError(f"snapshot config {path} has no created_at")
    if not isinstance(vm_id, str) or not vm_id:
        raise RuntimeError(f"snapshot config {path} has no vm_id")
    metadata = config.get("metadata")
    if not isinstance(metadata, dict):
        raise RuntimeError(f"snapshot config {path} has no metadata object")
    image = metadata.get("image")
    vcpu = metadata.get("vcpu")
    memory_mib = metadata.get("memory_mib")
    network_mode = metadata.get("network_mode")
    port_mappings = metadata.get("port_mappings")
    if not isinstance(image, str) or not image:
        raise RuntimeError(f"snapshot config {path} has no image")
    if not isinstance(vcpu, int) or isinstance(vcpu, bool) or vcpu <= 0:
        raise RuntimeError(f"snapshot config {path} has no positive vcpu")
    if not isinstance(memory_mib, int) or isinstance(memory_mib, bool) or memory_mib <= 0:
        raise RuntimeError(f"snapshot config {path} has no positive memory_mib")
    if network_mode not in ("rootless", "bridged", "routed"):
        raise RuntimeError(f"snapshot config {path} has invalid network_mode {network_mode!r}")
    if not isinstance(port_mappings, list):
        raise RuntimeError(f"snapshot config {path} has no port_mappings list")

    provenance_path = os.path.join(
        data_root, "snapshots", snapshot_name, "reqbench-provenance.json"
    )
    try:
        with open(provenance_path) as f:
            provenance = json.load(f)
    except (OSError, ValueError) as error:
        raise RuntimeError(
            f"cannot identify benchmark image content from {provenance_path}: {error}; "
            "recreate the golden snapshot with reqbench.sh golden"
        )
    expected_provenance = {
        "snapshot_created_at": created_at,
        "snapshot_vm_id": vm_id,
        "image": image,
    }
    for field, expected in expected_provenance.items():
        if provenance.get(field) != expected:
            raise RuntimeError(
                f"snapshot provenance {provenance_path} has {field}="
                f"{provenance.get(field)!r}, expected {expected!r}"
            )
    image_id = provenance.get("image_id")
    if (
        not isinstance(image_id, str)
        or len(image_id) != 71
        or not image_id.startswith("sha256:")
        or any(character not in "0123456789abcdef" for character in image_id[7:])
        or image_id != image
    ):
        raise RuntimeError(
            f"snapshot provenance {provenance_path} has invalid/mismatched image_id"
        )
    creator_fcvm_sha256 = provenance.get("creator_fcvm_sha256")
    creator_runtime_bundle_sha256 = provenance.get(
        "creator_runtime_bundle_sha256"
    )
    source_revision = provenance.get("source_revision")
    for field, value, length in (
        ("creator_fcvm_sha256", creator_fcvm_sha256, 64),
        ("creator_runtime_bundle_sha256", creator_runtime_bundle_sha256, 64),
        ("source_revision", source_revision, 40),
    ):
        if (
            not isinstance(value, str)
            or len(value) != length
            or any(character not in "0123456789abcdef" for character in value)
        ):
            raise RuntimeError(
                f"snapshot provenance {provenance_path} has invalid {field}"
            )

    return {
        "created_at": created_at,
        "vm_id": vm_id,
        "image": image,
        "image_id": image_id,
        "creator_fcvm_sha256": creator_fcvm_sha256,
        "creator_runtime_bundle_sha256": creator_runtime_bundle_sha256,
        "source_revision": source_revision,
        "vcpu": vcpu,
        "memory_mib": memory_mib,
        "network_mode": network_mode,
        "port_mappings": port_mappings,
    }


def serve_uffd_mode(state_dir: str, serve_pid: int, snapshot_name: str) -> str:
    """Validate the exact live serve process and return its memory mode."""
    actual = proc_stat_fields(serve_pid)
    if actual is None:
        raise RuntimeError(f"serve PID {serve_pid} is not running")
    candidates = []
    try:
        names = os.listdir(state_dir)
    except OSError as error:
        raise RuntimeError(f"cannot inspect serve state in {state_dir}: {error}")
    for name in names:
        if not name.endswith(".json"):
            continue
        path = os.path.join(state_dir, name)
        try:
            with open(path) as f:
                state = json.load(f)
        except (OSError, ValueError):
            continue
        if state.get("pid") == serve_pid:
            candidates.append((path, state))
    exact = [
        (path, state)
        for path, state in candidates
        if state.get("pid_start_time") == actual[3]
    ]
    if len(exact) != 1:
        raise RuntimeError(
            f"serve PID {serve_pid} has {len(exact)} state records with its exact "
            f"start time {actual[3]}: {[path for path, _state in exact]!r}"
        )
    path, state = exact[0]
    config = state.get("config") or {}
    if config.get("process_type") != "serve":
        raise RuntimeError(f"state {path} is not a snapshot serve process")
    served_snapshot = config.get("snapshot_name")
    if served_snapshot != snapshot_name:
        raise RuntimeError(
            f"serve PID {serve_pid} serves {served_snapshot!r}, not declared "
            f"snapshot {snapshot_name!r}"
        )
    mode = config.get("uffd_mode")
    if mode not in ("copy", "minor"):
        raise RuntimeError(f"serve state {path} has invalid UFFD mode {mode!r}")
    return mode


def read_trimmed(path: str) -> str:
    with open(path) as f:
        return f.read().strip()


# ------------------------------------------------------------------- teardown


class SurvivedTeardown(RuntimeError):
    """A tracked child outlived the fast teardown. The box is now contaminated.

    Carries the partial teardown dict so the failure still lands in the artifact
    with its survivor list. A leak that aborts the run but leaves no record is
    only marginally better than one that does not abort it.
    """

    def __init__(self, message, teardown=None):
        super().__init__(message)
        self.teardown = teardown or {}
        self.record: dict = {}


class HarnessInterrupted(BaseException):
    """A host signal that must unwind only after the active clone is reaped."""


_pending_harness_signal = 0


def record_harness_interrupt(signum, _frame):
    """Record, but never asynchronously raise, INT/TERM.

    Raising from a Python signal handler can land between Popen returning and
    ownership publication, between request completion and teardown, or inside
    the stopped-owner critical section.  Every one of those boundaries can
    orphan a VM.  Recording makes signal delivery race-free: request waits poll
    the flag, teardown runs with no asynchronous exception, and only an exact
    terminal proof turns the pending signal back into HarnessInterrupted.
    """
    global _pending_harness_signal
    if not _pending_harness_signal:
        _pending_harness_signal = signum


def raise_if_harness_interrupted() -> None:
    if _pending_harness_signal:
        raise HarnessInterrupted(f"received signal {_pending_harness_signal}")


def valid_vm_id(vm_id) -> bool:
    """Match the only VM identifier shape fcvm mints (`vm-` + UUID simple)."""
    return (
        isinstance(vm_id, str)
        and len(vm_id) == 35
        and vm_id.startswith("vm-")
        and all(character in "0123456789abcdef" for character in vm_id[3:])
    )


def valid_snapshot_name(name) -> bool:
    return (
        isinstance(name, str)
        and 1 <= len(name) <= 128
        and name not in (".", "..")
        and all(character.isascii() and (character.isalnum() or character in "-_.")
                for character in name)
    )


def safe_vm_data_dir(data_root: str, state_path: str, data_dir: str) -> str:
    """Return an exact child of this run's trusted ``vm-disks`` root, else "".

    Every call site computes `os.path.join(data_root, "vm-disks", vm_id)` from a
    state file, and `state.get("vm_id", "")` is empty on a partially-written
    file. `os.path.join("/mnt/fcvm-btrfs", "vm-disks", "")` is
    `/mnt/fcvm-btrfs/vm-disks/`, `os.path.isdir` says True, and the rmtree below
    then deletes EVERY VM's disks on the box — under sudo. Commit e1286e3f called
    exactly this arithmetic catastrophic and guarded `tests/test_signal_cleanup.rs`
    (`an_empty_vm_id_resolves_to_the_shared_disk_root_and_must_be_rejected`); the
    Python harness that does the identical computation was left unguarded, so the
    guarantee held only for the Rust fixture.

    Symlinks are resolved on both sides before comparing, so a `vm-disks/<id>`
    that points somewhere else cannot smuggle the deletion out of the tree.
    """
    if not data_root or not state_path or not data_dir:
        return ""
    trusted = os.path.realpath(os.path.join(data_root, "vm-disks"))
    raw = os.path.abspath(os.path.normpath(data_dir))
    head, vm_id = os.path.split(raw)
    expected = os.path.join(trusted, vm_id)
    real = os.path.realpath(raw)
    try:
        contained = os.path.commonpath((trusted, real)) == trusted
    except ValueError:
        contained = False
    if (
        not valid_vm_id(vm_id)
        or os.path.basename(state_path) != f"{vm_id}.json"
        or os.path.realpath(head) != trusted
        or os.path.islink(raw)
        or real != expected
        or not contained
        or (os.path.lexists(raw) and not os.path.isdir(raw))
    ):
        return ""
    return real


def reap_disk(
    out: dict,
    data_root: str,
    state_path: str,
    data_dir: str,
    expected_owner: tuple[int, int] | None = None,
) -> list:
    """Remove the clone's state file (+ its lock) and data dir. Errors are RECORDED.

    Never `ignore_errors=True`: a silently-failed rmtree reinstates the leak this
    function exists to prevent, and clone dirs can be root-owned when the bench
    runs under SUDO, so EPERM has to surface.
    """
    reaped = []
    state_lock = None
    if expected_owner is not None:
        # Manual cleanup is destructive and happens after the process has gone,
        # when a numeric PID by itself can already name somebody else.  Serialize
        # with fcvm's state writer, read without following symlinks, and require
        # the exact (PID, procfs starttime) captured immediately after Popen.
        # A fresh null-PID record may identify this spawn for diagnostics, but it
        # is not enough authority to remove its disk.
        lock_path = f"{state_path}.lock" if state_path else ""
        try:
            state_lock = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW)
            fcntl.flock(state_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            state_fd = os.open(state_path, os.O_RDONLY | os.O_NOFOLLOW)
            try:
                if not stat.S_ISREG(os.fstat(state_fd).st_mode):
                    raise RuntimeError("state path is not a regular file")
                with os.fdopen(state_fd) as state_file:
                    state_fd = -1
                    persisted = json.load(state_file)
            finally:
                if state_fd >= 0:
                    os.close(state_fd)
            actual_owner = (persisted.get("pid"), persisted.get("pid_start_time"))
            if actual_owner != expected_owner:
                raise RuntimeError(
                    f"state owner is {actual_owner}, expected exact owner "
                    f"{expected_owner}"
                )
        except (OSError, ValueError, RuntimeError) as error:
            if state_lock is not None:
                try:
                    fcntl.flock(state_lock, fcntl.LOCK_UN)
                finally:
                    os.close(state_lock)
            out.setdefault("disk_errors", []).append(
                f"{state_path}: refusing to remove without exact state identity: {error}"
            )
            return reaped
    if data_dir:
        safe = safe_vm_data_dir(data_root, state_path, data_dir)
        if not safe:
            out.setdefault("disk_errors", []).append(
                f"{data_dir}: refusing to remove — not an exact child of "
                f"{os.path.join(data_root, 'vm-disks')} "
                f"(an empty vm_id collapses to the SHARED disk root)"
            )
            # The state file is the only durable pointer to the disk we just
            # refused to touch.  Deleting it here would orphan that disk, so an
            # unsafe target makes the entire reap a no-op.
            if state_lock is not None:
                fcntl.flock(state_lock, fcntl.LOCK_UN)
                os.close(state_lock)
            return reaped
        else:
            data_dir = safe
    # Remove the disk first.  If this fails, keep the state and lock as the only
    # durable pointer to the still-allocated clone rather than orphaning it.
    if data_dir and os.path.isdir(data_dir):
        try:
            shutil.rmtree(data_dir)
            reaped.append(data_dir)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{data_dir}: {e}")
            if state_lock is not None:
                fcntl.flock(state_lock, fcntl.LOCK_UN)
                os.close(state_lock)
            return reaped
    if state_path and os.path.lexists(state_path):
        try:
            os.remove(state_path)
            reaped.append(state_path)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{state_path}: {e}")
    # cleanup_stale_state removes `<vm_id>.json.lock` alongside the state file;
    # it never runs under SIGKILL, so remove the lock independently. The state
    # may already be gone while its lock remains, and that is still a leak.
    lock = f"{state_path}.lock" if state_path else ""
    if lock and os.path.lexists(lock):
        if state_lock is not None:
            fcntl.flock(state_lock, fcntl.LOCK_UN)
            os.close(state_lock)
            state_lock = None
        try:
            os.remove(lock)
            reaped.append(lock)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{lock}: {e}")
    elif state_lock is not None:
        fcntl.flock(state_lock, fcntl.LOCK_UN)
        os.close(state_lock)
    return reaped


def measure_fast_reap(
    fcvm_pid: int,
    parent_fd: int,
    tracked: dict[str, int],
    fds: dict[str, int | None],
    timeout_s: float,
) -> dict:
    """Measure one fast reap while guaranteeing a stopped owner cannot escape."""
    all_fds = [parent_fd, *fds.values()]
    try:
        pre = {name: proc_stat_fields(pid) for name, pid in tracked.items()}
        missing_pre = [name for name, fields in pre.items() if fields is None]
        if missing_pre:
            raise RuntimeError(
                f"cannot capture pre-kill CPU/starttime for pinned processes {missing_pre}"
            )
        self0 = self_cpu_ms()
        busy0, _ = machine_busy_jiffies()
        t_kill = time.monotonic()
        try:
            os.kill(fcvm_pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        signal_ms = (time.monotonic() - t_kill) * 1000

        deadline = t_kill + timeout_s
        cpu, sample_period_s = sample_all_until_gone(
            tracked, pre, deadline
        )
        all_gone = wait_pidfds(all_fds, max(0.0, deadline - time.monotonic()))
        live_exact: dict[str, int] = {}
        parent_live = False
        if not all_gone:
            poller = select.poll()
            for fd in all_fds:
                if fd is not None:
                    poller.register(fd, select.POLLIN)
            ready = {fd for fd, _event in poller.poll(0)}
            parent_live = parent_fd not in ready
            live_exact = {
                name: tracked[name]
                for name, fd in fds.items()
                if fd is not None and fd not in ready
            }
        t_gone = time.monotonic()
        busy1, _ = machine_busy_jiffies()
        self_ms = self_cpu_ms() - self0

        # Measure ambient load only after the exact VM process set is terminal.
        # A pre-kill control contains the still-running VM's ordinary CPU and
        # subtracts work that is absent from the reclaim window.
        ctl_self0 = self_cpu_ms()
        ctl_busy0, _ = machine_busy_jiffies()
        ctl_t0 = time.monotonic()
        time.sleep(0.05)
        ctl_busy1, _ = machine_busy_jiffies()
        ctl_dt = time.monotonic() - ctl_t0
        ctl_self_ms = self_cpu_ms() - ctl_self0
        ctl_busy_ms = (ctl_busy1 - ctl_busy0) * 1000.0 / CLK_TCK
        ctl_rate = (
            (ctl_busy_ms - ctl_self_ms) / 1000.0 / ctl_dt
            if ctl_dt > 0
            else 0.0
        )
        return {
            "all_gone": all_gone,
            "busy0": busy0,
            "busy1": busy1,
            "cpu": cpu,
            "ctl_rate": ctl_rate,
            "ctl_self_ms": ctl_self_ms,
            "live_exact": live_exact,
            "parent_live": parent_live,
            "pre": pre,
            "sample_period_s": sample_period_s,
            "self_ms": self_ms,
            "signal_ms": signal_ms,
            "t_gone": t_gone,
            "t_kill": t_kill,
        }
    except BaseException:
        # Any instrumentation error after SIGSTOP is a lifecycle failure. Make
        # the exact owner set terminal, retain its durable disk pointer, and let
        # the caller emit a structured abort instead of stranding a stopped VM.
        abort_frozen_owner(fcvm_pid, parent_fd, list(fds.values()))
        close_pidfds(all_fds)
        raise


def teardown_fast(
    fcvm_pid: int,
    data_root: str,
    state_path: str,
    data_dir: str,
    timeout_s: float,
    verify_disk_cleanup: bool = False,
    expected_pid_start_time: int | None = None,
) -> dict:
    """Concurrent SIGKILL via the pdeathsig chain, then synchronous on-disk reap.

    Raises `SurvivedTeardown` if any tracked child outlived the kill. That is not
    politeness: reaping the state file and rmtree'ing the data dir of a VM whose
    Firecracker is still RUNNING deletes the only record of a live microVM and
    pulls its rootfs out from under it, and every later measurement then runs on a
    box carrying an invisible ~1 GB tenant. Continuing the schedule after that is
    measuring contention, not the request path.
    """
    out: dict = {
        "mode": "fast",
        "accounting_version": "post-terminal-ambient-v1",
    }

    parent_fd = None
    captured_fds: list[int | None] = []
    try:
        parent_fd = pidfd_open(fcvm_pid)
        if parent_fd is None:
            raise RuntimeError(f"fcvm {fcvm_pid} exited before owner pinning")
        kids, captured_fds = freeze_and_capture_children(fcvm_pid)
        if not kids or any(fd is None for fd in captured_fds):
            raise RuntimeError(
                f"fcvm {fcvm_pid} has no completely pinned child set"
            )
    except (RuntimeError, OSError) as error:
        terminal = abort_frozen_owner(fcvm_pid, parent_fd, captured_fds)
        close_pidfds([parent_fd, *captured_fds])
        out["child_attribution_established"] = False
        out["all_gone"] = terminal
        out["disk_reap_skipped"] = True
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} cannot prove its child set: {error}; "
            f"state {state_path} and data {data_dir} NOT reaped",
            out,
        ) from error
    out["child_attribution_established"] = True
    try:
        # Keyed by comm, but a collision must not drop a child.
        tracked: dict = {}
        for p in kids:
            base = proc_comm(p) or f"pid{p}"
            key = base if base not in tracked else f"{base}#{p}"
            tracked[key] = p
        out["children"] = tracked
        captured_by_pid = dict(zip(kids, captured_fds))
        fds = {name: captured_by_pid[pid] for name, pid in tracked.items()}
        sampled = dict(tracked)
        sampled["fcvm"] = fcvm_pid
    except BaseException as error:
        abort_frozen_owner(fcvm_pid, parent_fd, captured_fds)
        close_pidfds([parent_fd, *captured_fds])
        out["child_attribution_established"] = False
        out["disk_reap_skipped"] = True
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} cannot pin its complete owner set: "
            f"{error}; state {state_path} and data {data_dir} NOT reaped",
            out,
        ) from error

    try:
        measured = measure_fast_reap(fcvm_pid, parent_fd, sampled, fds, timeout_s)
    except BaseException as error:
        out["disk_reap_skipped"] = True
        out["measurement_error"] = f"{type(error).__name__}: {error}"
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} failed while its exact process set "
            f"was pinned; state {state_path} and data {data_dir} NOT reaped: {error}",
            out,
        ) from error

    all_gone = measured["all_gone"]
    busy0 = measured["busy0"]
    busy1 = measured["busy1"]
    cpu = measured["cpu"]
    ctl_rate = measured["ctl_rate"]
    ctl_self_ms = measured["ctl_self_ms"]
    pre = measured["pre"]
    sample_period_s = measured["sample_period_s"]
    self_ms = measured["self_ms"]
    t_gone = measured["t_gone"]
    t_kill = measured["t_kill"]
    out["signal_ms"] = measured["signal_ms"]

    window_s = t_gone - t_kill
    machine_cpu_ms = (busy1 - busy0) * 1000.0 / CLK_TCK
    out["reap_wall_ms"] = window_s * 1000
    out["all_gone"] = all_gone
    out["machine_cpu_ms"] = machine_cpu_ms
    out["harness_cpu_ms"] = self_ms
    out["machine_cpu_ms_excess"] = (machine_cpu_ms - self_ms) - ctl_rate * window_s * 1000
    out["control_busy_cores"] = ctl_rate
    out["control_harness_cpu_ms"] = ctl_self_ms
    # Both accounting windows now sleep, so the subtraction is between like and
    # like. The sampler's residual duty cycle is bounded by this period and is
    # recorded rather than argued about.
    out["sample_period_s"] = sample_period_s

    # /proc/<pid>/stat counts in jiffies, so every CPU figure here is quantized to
    # one tick. At CLK_TCK=100 that is 10 ms, and a sub-tick reclaim reports a hard
    # 0.0 — a claim of zero CPU with zero uncertainty, which is exactly AGENTS.md
    # defect 6. Emit the bound instead of the point.
    tick_ms = 1000.0 / CLK_TCK
    out["tick_ms"] = tick_ms
    out["per_child_cpu"] = {}
    for name, pid in sampled.items():
        before = pre[name]
        base = (before[1] + before[2]) * 1000.0 / CLK_TCK if before else 0.0
        s = cpu[name]
        delta = (s["cpu_ms"] - base) if s["cpu_ms"] is not None else None
        out["per_child_cpu"][name] = {
            "pid": pid,
            "cpu_before_ms": base,
            "cpu_final_ms": s["cpu_ms"],
            "reclaim_cpu_ms": delta,
            # The interval the two quantized reads can actually support: each
            # endpoint is truncated to a tick, so the true delta lies in
            # [delta, delta + 2*tick].
            "reclaim_cpu_ms_lo": delta,
            "reclaim_cpu_ms_hi": (delta + 2 * tick_ms) if delta is not None else None,
            "below_resolution": (delta == 0.0) if delta is not None else None,
            # True  -> exit_mm() had already run, figure is COMPLETE.
            # False -> reaper won the race, figure is a LOWER BOUND.
            "complete": s["zombie_seen"],
        }

    if not all_gone:
        survivors = dict(measured["live_exact"])
        if measured["parent_live"]:
            survivors["fcvm"] = fcvm_pid
        out["survivors"] = survivors
        out["disk_reap_skipped"] = True
        for name in measured["live_exact"]:
            fd = fds.get(name)
            if fd is not None:
                try:
                    pidfd_send_signal(fd, signal.SIGKILL)
                except (ProcessLookupError, PermissionError, OSError):
                    pass
        if measured["parent_live"]:
            try:
                pidfd_send_signal(parent_fd, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
        wait_pidfds([parent_fd, *fds.values()], 10.0)
        close_pidfds([parent_fd, *fds.values()])
        out["teardown_total_ms"] = (time.monotonic() - t_kill) * 1000
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} left {survivors} alive after "
            f"{timeout_s:.1f}s; state {state_path} and data {data_dir} NOT reaped",
            out,
        )
    close_pidfds([parent_fd, *fds.values()])

    t_disk = time.monotonic()
    if verify_disk_cleanup and (not state_path or not data_dir):
        out["disk_cleanup_verified"] = False
        out["teardown_total_ms"] = (time.monotonic() - t_kill) * 1000
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} cannot verify on-disk cleanup "
            f"without both exact paths: state={state_path!r} data={data_dir!r}",
            out,
        )
    expected_owner = (
        (fcvm_pid, expected_pid_start_time)
        if expected_pid_start_time is not None
        else None
    )
    reaped = reap_disk(out, data_root, state_path, data_dir, expected_owner)
    out["disk_reap_ms"] = (time.monotonic() - t_disk) * 1000
    out["disk_reaped"] = reaped
    # VERIFY ABSENCE, then gate. `reap_disk` recorded EPERM in `out["disk_errors"]`
    # and nothing on the branch ever read that key: a rep whose rmtree failed was
    # written with `ok: true` and the schedule continued. The asymmetry was the
    # tell — the PROCESS half of this function already aborts, the DISK half did
    # not. `vm-disks/<vm_id>` holds a reflink of the golden rootfs, so a failed
    # rmtree pins the golden snapshot's extents on btrfs, and the state file it
    # leaves behind can never be swept (`cleanup_stale_state` bails on a null
    # pid). `disk_reap_ms` is also a published per-arm median, computed over
    # records that included failed reaps.
    left = [p for p in (state_path, f"{state_path}.lock" if state_path else "", data_dir)
            if p and os.path.lexists(p)]
    out["disk_cleanup_verified"] = not left and not out.get("disk_errors")
    out["teardown_total_ms"] = (time.monotonic() - t_kill) * 1000
    if left or out.get("disk_errors"):
        out["disk_reap_failed"] = left
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} could not reap on-disk state: "
            f"left={left} errors={out.get('disk_errors', [])}",
            out,
        )
    return out


def teardown_normal(
    proc: subprocess.Popen,
    fcvm_pid: int,
    timeout_s: float,
    data_root: str = "",
    state_path: str = "",
    data_dir: str = "",
    verify_disk_cleanup: bool = False,
    expected_pid_start_time: int | None = None,
) -> dict:
    """fcvm's own cleanup: SIGTERM, then await the full sequential unwind.

    kill -> holder kill -> network cleanup -> state delete -> FC log save ->
    data-dir removal, each awaited. This is the control the fast arm is measured
    against.

    Raises `SurvivedTeardown` on a real survivor, for the same reason
    `teardown_fast` does: the `cdp` and `noop` arms used to compute `all_gone`,
    record it, and continue the schedule with a live Firecracker/holder/pasta on
    the box. `reqanalyze` printed `** N NOT CONFIRMED GONE **` and still exited 0,
    so nothing failed.

    TWO defects, in opposite directions, and both had to be fixed together. On the
    timed_out path the verdict was decided with a ZERO observation budget:
    `proc.wait(timeout=timeout_s)` spends the whole budget, `proc.wait(timeout=10)`
    spends more, so `max(0.0, t0 + timeout_s - now)` is always 0.0 by the time it
    is used — and `wait_pidfds` with a 0 budget returns False WITHOUT EVER
    POLLING. Measured: a pdeathsig child that died sub-millisecond with its parent
    was reported `all_gone: False`. Gating on that boolean as it stood would have
    aborted every healthy rep that merely hit the SIGTERM timeout. So the wait
    gets a real budget, and the verdict is then re-decided by a fresh liveness
    check on the PIDFDS — not on the raw pids, which can alias a reused pid, which
    is why this file opened pidfds in the first place.
    """
    out: dict = {"mode": "normal"}
    parent_fd = None
    fds: list[int | None] = []
    try:
        parent_fd = pidfd_open(fcvm_pid)
        if parent_fd is None:
            raise RuntimeError(f"fcvm {fcvm_pid} exited before owner pinning")
        kids, fds = freeze_and_capture_children(fcvm_pid)
        if not kids or any(fd is None for fd in fds):
            raise RuntimeError(
                f"fcvm {fcvm_pid} has no completely pinned child set"
            )
    except (RuntimeError, OSError) as error:
        terminal = abort_frozen_owner(fcvm_pid, parent_fd, fds)
        close_pidfds([parent_fd, *fds])
        out["child_attribution_established"] = False
        out["all_gone"] = terminal
        out["disk_reap_skipped"] = True
        raise SurvivedTeardown(
            f"normal teardown of fcvm {fcvm_pid} cannot prove its child set: {error}; "
            f"state {state_path} and data {data_dir} NOT reaped",
            out,
        ) from error
    out["child_attribution_established"] = True
    t0 = time.monotonic()

    live = []
    all_fds = [parent_fd, *fds]
    try:
        # Queue TERM while the parent is stopped, then resume it. It cannot fork
        # a new untracked child between attribution and signal delivery.
        try:
            os.kill(fcvm_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.kill(fcvm_pid, signal.SIGCONT)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            out["timed_out"] = True
            try:
                os.kill(fcvm_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(
                    f"fcvm {fcvm_pid} survived SIGKILL for 10 seconds"
                ) from error
        out["fcvm_exit_ms"] = (time.monotonic() - t0) * 1000
        left = max(0.0, t0 + timeout_s - time.monotonic())
        out["all_gone"] = wait_pidfds(all_fds, left if left > 0 else 0.5)
        if not out["all_gone"]:
            poller = select.poll()
            for fd in fds:
                if fd is not None:
                    poller.register(fd, select.POLLIN)
            ready = {fd for fd, _ev in poller.poll(0)}
            live = [
                p for p, fd in zip(kids, fds)
                if fd is not None and fd not in ready
            ]
            out["all_gone"] = not live
    except BaseException as error:
        terminal = abort_frozen_owner(fcvm_pid, parent_fd, fds)
        out["all_gone"] = terminal
        out["disk_reap_skipped"] = True
        close_pidfds(all_fds)
        raise SurvivedTeardown(
            f"normal teardown of fcvm {fcvm_pid} failed before terminal ownership "
            f"was proved; state {state_path} and data {data_dir} NOT reaped: {error}",
            out,
        ) from error
    out["reap_wall_ms"] = (time.monotonic() - t0) * 1000
    out["teardown_total_ms"] = out["reap_wall_ms"]
    if live:
        out["survivors"] = {p: proc_comm(p) for p in live}
        for p, fd in zip(kids, fds):
            if p not in live or fd is None:
                continue
            try:
                pidfd_send_signal(fd, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
        wait_pidfds(fds, 10.0)
        close_pidfds(all_fds)
        raise SurvivedTeardown(
            f"normal teardown of fcvm {fcvm_pid} left {out['survivors']} alive after "
            f"{timeout_s:.1f}s",
            out,
        )
    close_pidfds(all_fds)

    # Process exit is not sufficient. In particular, killing a clone after its
    # first state save but before the post-resume PID save used to leave a
    # permanent null-PID state file and its reflinked disk while the record said
    # `ok: true`. fcvm has exited, so no later cleanup can still be in flight.
    if verify_disk_cleanup and (not state_path or not data_dir):
        out["disk_cleanup_verified"] = False
        out["teardown_total_ms"] = (time.monotonic() - t0) * 1000
        raise SurvivedTeardown(
            f"normal teardown of fcvm {fcvm_pid} cannot verify on-disk cleanup "
            f"without both exact paths: state={state_path!r} data={data_dir!r}",
            out,
        )

    expected = [
        p
        for p in (
            state_path,
            f"{state_path}.lock" if state_path else "",
            data_dir,
        )
        if p
    ]
    left = [p for p in expected if os.path.lexists(p)]
    out["disk_cleanup_verified"] = not left
    if left:
        out["disk_cleanup_left"] = left
        t_disk = time.monotonic()
        expected_owner = (
            (fcvm_pid, expected_pid_start_time)
            if expected_pid_start_time is not None
            else None
        )
        out["disk_reaped"] = reap_disk(
            out, data_root, state_path, data_dir, expected_owner
        )
        out["disk_reap_ms"] = (time.monotonic() - t_disk) * 1000
        still_left = [p for p in expected if os.path.lexists(p)]
        out["disk_reap_failed"] = still_left
        out["teardown_total_ms"] = (time.monotonic() - t0) * 1000
        raise SurvivedTeardown(
            f"normal teardown of fcvm {fcvm_pid} left on-disk state {left}; "
            f"exact-path reap left {still_left} and errors={out.get('disk_errors', [])}",
            out,
        )
    return out


# ----------------------------------------------------------------- one request


def clone_ws_url(supplied: str, endpoint: str) -> str:
    """Re-host a prewired `--ws-url` onto THIS clone's endpoint.

    `--ws-url` exists to skip the per-request `/json/list`, and the reusable part
    of the URL is the PATH — the page target id, which the golden snapshot bakes
    in so every clone presents the same one. The NETLOC is not reusable: every
    clone gets its own host-side address (`clone_cdp_endpoint`), so a single
    prewired URL passed verbatim names one fixed 127.x.y.z:port for the whole run.
    If that address happens to answer, the rep measures a DIFFERENT clone than the
    one it just spawned and tore down.
    """
    u = urlparse(supplied)
    return urlunparse(u._replace(netloc=endpoint))


def run_cdp_request(args, rep: int, fast: bool) -> dict:
    import cdpdrive

    name = f"rb-{args.run_id}-{rep}-{'fast' if fast else 'norm'}"
    log = os.path.join(args.out_dir, f"{name}.log")
    rec: dict = {"arm": "cdp-fast" if fast else "cdp", "rep": rep, "name": name}

    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap",
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    # Watch registered BEFORE the spawn: a watch created afterwards can miss the
    # state file's creation and then block waiting for an event already past.
    watch = DirWatch(args.state_dir)
    t_spawn = time.monotonic()
    interrupted = None
    fcvm_start_time = None
    try:
        pre_spawn_state_paths = state_path_baseline(args.state_dir)
        raise_if_harness_interrupted()
        previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, SPAWN_SIGNALS)
        spawn_complete = False
        with open(log, "wb") as lf:
            # stdout/stderr to a FILE, never a pipe we do not drain: an undrained
            # 64 KB pipe blocks fcvm's writer and stalls everything behind it
            # (AGENTS.md "Pipe Buffer Deadlock in Tests").
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
        fcvm_pid = proc.pid
        spawn_complete = True

        state_path = data_dir = None
        try:
            fcvm_start_time = spawned_process_start_time(proc)
            # Pending INT/TERM is delivered only after both process object and
            # exact PID are published to this teardown scope.
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
            deadline = t_spawn + args.timeout
            t = time.monotonic()
            state_path, state = find_state(
                args.state_dir,
                fcvm_pid,
                deadline,
                watch,
                name,
                proc,
                log,
                fcvm_start_time,
                pre_spawn_state_paths,
            )
            if state is None:
                raise TimeoutError("clone state file never appeared")
            rec["discover_ms"] = (time.monotonic() - t) * 1000
            vm_id = state.get("vm_id", "")
            data_dir = clone_data_dir(args.data_root, state)
            rec["vm_id"] = vm_id

            endpoint = clone_cdp_endpoint(state, args.cdp_port)
            rec["endpoint"] = endpoint
            rec["state_to_port_ms"] = wait_port(endpoint, deadline, proc, log)
            rec["spawn_to_port_ms"] = (time.monotonic() - t_spawn) * 1000

            ws_url = clone_ws_url(args.ws_url, endpoint) if args.ws_url else ""
            rec["ws_url_prewired"] = bool(ws_url)
            drive_args = argparse.Namespace(
                cdp_host=endpoint,
                url=args.url,
                format=args.format,
                quality=args.quality,
                timeout=max(1.0, deadline - time.monotonic()),
                idle_wait_ms=0.0,
                out_prefix="",
                ws_url=ws_url,
                connect_retries=200,
                nav_timing=True,
                print_target=False,
                # This Namespace is an explicit, CLOSED field list, so every flag
                # cdpdrive grows has to be added here too or `drive()` raises
                # AttributeError — which is not in its except tuple, escapes it,
                # and is swallowed by the `except Exception` below, failing every
                # cdp rep. cdpdrive also reads it with getattr; both halves.
                host_header="",
                render_module=os.path.join(HERE, "render.py"),
            )
            result = cdpdrive.drive(drive_args)
            raise_if_harness_interrupted()
            rec["render"] = result
            rec["ok"] = bool(result.get("ok"))
            if not rec["ok"]:
                # LIFT THE DIAGNOSTIC TO THE TOP LEVEL. cdpdrive can return
                # ok=false WITHOUT raising, in which case the `except` below never
                # runs and `rec["error"]` was never set — the label stayed buried
                # under `rec["render"]`. reqanalyze's only failure breakdown reads
                # the top level (`r.get("error", f"rc={r.get('rc')}")`), so a
                # WsClosed transport drop printed as `FAILURE x1: rc=None`.
                # Separating transport drops from render failures is the entire
                # point of REVIEW.md's 200-request availability gate.
                rec["error"] = result.get("error", "cdpdrive reported ok=false")
                rec["failure_class"] = result.get("failure_class", "render")
                rec["failure_stage"] = result.get("stage", "")
            # THE CALLER'S ANSWER IS IN HAND HERE. Everything after this line is
            # teardown, and none of it is latency the caller pays.
            rec["blocking_ms"] = (time.monotonic() - t_spawn) * 1000
            t_owned = time.monotonic()
            state = wait_state_owned(
                state_path,
                fcvm_pid,
                deadline,
                watch,
                proc,
                fcvm_start_time,
                name,
            )
            data_dir = clone_data_dir(args.data_root, state)
            rec["state_owner_wait_ms"] = (time.monotonic() - t_owned) * 1000
            rec["state_owner_pid"] = state.get("pid")
        except BaseException as e:
            rec["ok"] = False
            rec["request_error"] = f"{type(e).__name__}: {e}"
            rec["error"] = rec["request_error"]
            if not isinstance(e, Exception):
                interrupted = e
            rec.setdefault("blocking_ms", (time.monotonic() - t_spawn) * 1000)
            if state_path is None and fcvm_start_time is not None:
                # find_state may have timed out while fcvm had ALREADY written the
                # file (it is saved with `pid: null` until post-resume). Leaving
                # A newly published null-PID file is useful to verify graceful
                # cleanup, but is never enough authority for manual deletion.
                # Rescan by name while permanently excluding pre-spawn paths;
                # reap_disk will require the exact PID start time if it remains.
                state_path, state = scan_state(
                    args.state_dir,
                    fcvm_pid,
                    name,
                    fcvm_start_time,
                    pre_spawn_state_paths,
                )
                if state is not None:
                    rec["recovered_state_by_name"] = True
                    rec["vm_id"] = state.get("vm_id", "")
                    try:
                        data_dir = clone_data_dir(args.data_root, state)
                    except RuntimeError as data_error:
                        rec["state_error"] = str(data_error)
    finally:
        if "previous_mask" in locals() and not spawn_complete:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        watch.close()

    if fast:
        try:
            rec["teardown"] = teardown_fast(
                fcvm_pid,
                args.data_root,
                state_path,
                data_dir,
                args.teardown_timeout,
                verify_disk_cleanup=True,
                expected_pid_start_time=fcvm_start_time,
            )
        except SurvivedTeardown as e:
            # The rep still gets a record, with the survivor list in it.
            rec["teardown"] = e.teardown
            rec["ok"] = False
            rec["teardown_error"] = str(e)
            if rec.get("request_error"):
                rec["error"] = f"{rec['request_error']}; teardown: {e}"
            else:
                rec["error"] = f"{type(e).__name__}: {e}"
            rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
            rec["log"] = log
            e.record = rec
            raise e from interrupted
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
    else:
        try:
            rec["teardown"] = teardown_normal(
                proc,
                fcvm_pid,
                args.teardown_timeout,
                args.data_root,
                state_path,
                data_dir,
                verify_disk_cleanup=True,
                expected_pid_start_time=fcvm_start_time,
            )
        except SurvivedTeardown as e:
            rec["teardown"] = e.teardown
            rec["ok"] = False
            rec["teardown_error"] = str(e)
            if rec.get("request_error"):
                rec["error"] = f"{rec['request_error']}; teardown: {e}"
            else:
                rec["error"] = f"{type(e).__name__}: {e}"
            rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
            rec["log"] = log
            e.record = rec
            raise e from interrupted
    rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
    rec["log"] = log
    if interrupted is not None:
        raise interrupted
    raise_if_harness_interrupted()
    return rec



def clone_backend_args(args) -> list:
    """UFFD serve-backed vs FILE-backed restore, as CLI args.

    These are different memory backends with different per-request costs -- the
    published stage baseline (`corrected.json` primary cell: total 890.6 ms
    [869.6, 928.9], n=12) is UFFD-4K, and every guest page touched during startup
    costs a UFFD round trip on that path but not on the file path. This comment
    used to cite "the published 573 ms stage baseline", a figure that appears in
    no committed artifact and whose only other occurrence was the AGENTS.md line
    citing this file. Mixing the backends and
    comparing against that baseline would be exactly the matched-accounting
    failure AGENTS.md defect 1 describes, so the backend is explicit and recorded
    in the run metadata rather than implied by which flag happened to be passed.
    """
    if args.snapshot_tag:
        return ["--snapshot", args.snapshot_tag]
    return ["--pid", str(args.serve_pid)]


def run_noop_request(args, rep: int) -> dict:
    """DRIFT CONTROL. Clone spawn -> CDP port answers -> normal teardown. No page.

    AGENTS.md defect 2 requires a probe that exercises NONE of the varied
    dimension, so wall-clock drift is measurable and removable rather than being
    silently absorbed into an arm's effect. The varied dimension here is the
    REQUEST transport (in-guest exec'd python vs host-driven CDP) and the
    teardown discipline. This probe does neither: it never loads a page, never
    speaks CDP, never touches the page server.

    What it DOES include is exactly the substrate every arm shares — fcvm clone
    spawn, snapshot restore, and fcvm's own teardown — so a machine that slows
    down over the run shows up here, in a series with no arm effect in it.
    """
    name = f"rb-{args.run_id}-{rep}-noop"
    log = os.path.join(args.out_dir, f"{name}.log")
    rec: dict = {"arm": "noop", "rep": rep, "name": name}
    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap",
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    watch = DirWatch(args.state_dir)
    t_spawn = time.monotonic()
    state_path = data_dir = None
    interrupted = None
    fcvm_start_time = None
    try:
        pre_spawn_state_paths = state_path_baseline(args.state_dir)
        raise_if_harness_interrupted()
        previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, SPAWN_SIGNALS)
        spawn_complete = False
        with open(log, "wb") as lf:
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
        fcvm_pid = proc.pid
        spawn_complete = True
        try:
            fcvm_start_time = spawned_process_start_time(proc)
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
            deadline = t_spawn + args.timeout
            t = time.monotonic()
            state_path, state = find_state(
                args.state_dir,
                fcvm_pid,
                deadline,
                watch,
                name,
                proc,
                log,
                fcvm_start_time,
                pre_spawn_state_paths,
            )
            if state is None:
                raise TimeoutError("clone state file never appeared")
            rec["discover_ms"] = (time.monotonic() - t) * 1000
            rec["vm_id"] = state.get("vm_id", "")
            data_dir = clone_data_dir(args.data_root, state)
            endpoint = clone_cdp_endpoint(state, args.cdp_port)
            rec["endpoint"] = endpoint
            rec["state_to_port_ms"] = wait_port(endpoint, deadline, proc, log)
            rec["spawn_to_port_ms"] = (time.monotonic() - t_spawn) * 1000
            # This is the noop caller's response boundary. Waiting for fcvm to
            # claim the state is teardown preparation and must not inflate it.
            rec["blocking_ms"] = rec["spawn_to_port_ms"]
            t_owned = time.monotonic()
            state = wait_state_owned(
                state_path,
                fcvm_pid,
                deadline,
                watch,
                proc,
                fcvm_start_time,
                name,
            )
            data_dir = clone_data_dir(args.data_root, state)
            rec["state_owner_wait_ms"] = (time.monotonic() - t_owned) * 1000
            rec["state_owner_pid"] = state.get("pid")
            rec["ok"] = True
        except BaseException as e:
            rec["ok"] = False
            rec["request_error"] = f"{type(e).__name__}: {e}"
            rec["error"] = rec["request_error"]
            if not isinstance(e, Exception):
                interrupted = e
            if state_path is None and fcvm_start_time is not None:
                state_path, state = scan_state(
                    args.state_dir,
                    fcvm_pid,
                    name,
                    fcvm_start_time,
                    pre_spawn_state_paths,
                )
                if state is not None:
                    rec["recovered_state_by_name"] = True
                    rec["vm_id"] = state.get("vm_id", "")
                    try:
                        data_dir = clone_data_dir(args.data_root, state)
                    except RuntimeError as data_error:
                        rec["state_error"] = str(data_error)
    finally:
        if "previous_mask" in locals() and not spawn_complete:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        watch.close()
    rec.setdefault("blocking_ms", (time.monotonic() - t_spawn) * 1000)
    try:
        rec["teardown"] = teardown_normal(
            proc,
            fcvm_pid,
            args.teardown_timeout,
            args.data_root,
            state_path,
            data_dir,
            verify_disk_cleanup=True,
            expected_pid_start_time=fcvm_start_time,
        )
    except SurvivedTeardown as e:
        rec["teardown"] = e.teardown
        rec["ok"] = False
        rec["teardown_error"] = str(e)
        if rec.get("request_error"):
            rec["error"] = f"{rec['request_error']}; teardown: {e}"
        else:
            rec["error"] = f"{type(e).__name__}: {e}"
        rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
        rec["log"] = log
        e.record = rec
        raise e from interrupted
    rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
    rec["log"] = log
    if interrupted is not None:
        raise interrupted
    raise_if_harness_interrupted()
    return rec


def run_exec_request(args, rep: int) -> dict:
    """Baseline arm, including verified process and on-disk teardown."""
    name = f"rb-{args.run_id}-{rep}-exec"
    log = os.path.join(args.out_dir, f"{name}.log")
    driver = shlex.join([
        "python3", "/opt/bench/render.py", args.url,
        "--out-prefix", "/tmp/rb",
        "--format", args.format,
        "--quality", str(args.quality),
    ])
    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap", "--exec", driver,
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    rec: dict = {"arm": "exec", "rep": rep, "name": name, "timed_out": False}
    t0 = time.monotonic()
    state_path = data_dir = None
    proc = None
    parent_fd = None
    kids: list[int] = []
    fds: list[int | None] = []
    watch = DirWatch(args.state_dir)
    fcvm_start_time = None
    try:
        pre_spawn_state_paths = state_path_baseline(args.state_dir)
        raise_if_harness_interrupted()
        previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, SPAWN_SIGNALS)
        spawn_mask_restored = False
        with open(log, "wb") as lf:
            proc = subprocess.Popen(
                cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env
            )
        fcvm_start_time = spawned_process_start_time(proc)
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
        spawn_mask_restored = True
        deadline = t0 + args.timeout
        state_path, state = find_state(
            args.state_dir,
            proc.pid,
            deadline,
            watch,
            name,
            proc,
            log,
            fcvm_start_time,
            pre_spawn_state_paths,
        )
        if state is None:
            raise TimeoutError("exec clone state file never appeared")
        rec["vm_id"] = state.get("vm_id", "")
        data_dir = clone_data_dir(args.data_root, state)
        state = wait_state_owned(
            state_path,
            proc.pid,
            deadline,
            watch,
            proc,
            fcvm_start_time,
            name,
        )
        data_dir = clone_data_dir(args.data_root, state)
        rec["state_owner_pid"] = state.get("pid")
        parent_fd = pidfd_open(proc.pid)
        if parent_fd is None:
            raise RuntimeError(f"cannot open pidfd for exec clone owner {proc.pid}")
        # Pin the complete direct-child set while the owner is stopped. On a
        # normal fcvm exit these are precisely the processes whose pdeathsig
        # contract must make terminal; waiting on an empty list would be a
        # vacuous lifecycle proof.
        kids, fds = freeze_and_capture_children(proc.pid)
        if not kids or any(fd is None for fd in fds):
            abort_frozen_owner(proc.pid, parent_fd, fds)
            raise RuntimeError(
                f"exec clone owner {proc.pid} has no completely pinned child set"
            )
        os.kill(proc.pid, signal.SIGCONT)
    except BaseException as request_error:
        if "previous_mask" in locals() and not spawn_mask_restored:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
            spawn_mask_restored = True
        if (
            proc is not None
            and state_path is None
            and fcvm_start_time is not None
        ):
            state_path, state = scan_state(
                args.state_dir,
                proc.pid,
                name,
                fcvm_start_time,
                pre_spawn_state_paths,
            )
            if state is not None:
                rec["recovered_state_by_name"] = True
                rec["vm_id"] = state.get("vm_id", "")
                try:
                    data_dir = clone_data_dir(args.data_root, state)
                except RuntimeError as data_error:
                    rec["state_error"] = str(data_error)
        rec["ok"] = False
        rec["request_error"] = f"{type(request_error).__name__}: {request_error}"
        rec["error"] = rec["request_error"]
        if parent_fd is not None or fds:
            terminal = abort_frozen_owner(proc.pid, parent_fd, fds)
            close_pidfds([parent_fd, *fds])
            teardown_error = SurvivedTeardown(
                f"exec clone failed while establishing its exact owner set "
                f"(terminal={terminal}); state {state_path} and data {data_dir} "
                "NOT reaped",
                {
                    "mode": "exec",
                    "all_gone": terminal,
                    "child_attribution_established": False,
                    "disk_reap_skipped": True,
                },
            )
            rec["teardown"] = teardown_error.teardown
            rec["teardown_error"] = str(teardown_error)
            rec["error"] += f"; teardown: {teardown_error}"
            rec["blocking_ms"] = rec["wall_ms"] = (
                time.monotonic() - t0
            ) * 1000
            rec["log"] = log
            teardown_error.record = rec
            raise teardown_error from request_error
        if proc is not None and proc.poll() is None:
            try:
                rec["teardown"] = teardown_normal(
                    proc,
                    proc.pid,
                    args.teardown_timeout,
                    args.data_root,
                    state_path or "",
                    data_dir or "",
                    verify_disk_cleanup=True,
                    expected_pid_start_time=fcvm_start_time,
                )
            except SurvivedTeardown as teardown_error:
                rec["teardown"] = teardown_error.teardown
                rec["teardown_error"] = str(teardown_error)
                rec["error"] += f"; teardown: {teardown_error}"
                rec["blocking_ms"] = rec["wall_ms"] = (time.monotonic() - t0) * 1000
                rec["log"] = log
                teardown_error.record = rec
                raise teardown_error from request_error
        else:
            teardown_error = SurvivedTeardown(
                f"exec clone exited before child attribution; state {state_path} and "
                f"data {data_dir} NOT reaped",
                {"mode": "exec", "all_gone": False, "disk_reap_skipped": True},
            )
            rec["teardown"] = teardown_error.teardown
            rec["teardown_error"] = str(teardown_error)
            rec["error"] += f"; teardown: {teardown_error}"
            rec["blocking_ms"] = rec["wall_ms"] = (time.monotonic() - t0) * 1000
            rec["log"] = log
            teardown_error.record = rec
            raise teardown_error from request_error
        if not isinstance(request_error, Exception):
            raise
        rec["blocking_ms"] = rec["wall_ms"] = (time.monotonic() - t0) * 1000
        rec["log"] = log
        return rec
    finally:
        watch.close()

    wait_budget = max(0.0, deadline - time.monotonic())
    interrupted = None
    try:
        exited = wait_pidfds([parent_fd], wait_budget, interruptible=True)
    except HarnessInterrupted as interrupt:
        interrupted = interrupt
        rec["interrupted"] = True
        exited = False
    if not exited:
        rec["timed_out"] = interrupted is None
        try:
            timeout_kids, timeout_fds = freeze_and_capture_children(proc.pid)
        except RuntimeError as capture_error:
            try:
                pidfd_send_signal(parent_fd, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
            wait_pidfds([parent_fd], args.teardown_timeout)
            close_pidfds([parent_fd])
            teardown = {
                "mode": "exec",
                "all_gone": False,
                "child_attribution_established": False,
                "disk_reap_skipped": True,
            }
            error = SurvivedTeardown(
                f"exec clone timeout could not prove its child set: {capture_error}; "
                f"state {state_path} and data {data_dir} NOT reaped",
                teardown,
            )
            rec.update(
                ok=False,
                error=str(error),
                teardown=teardown,
                blocking_ms=(time.monotonic() - t0) * 1000,
                wall_ms=(time.monotonic() - t0) * 1000,
                log=log,
            )
            error.record = rec
            raise error from capture_error
        # No production child is allowed to appear after the state ownership
        # barrier. A mismatch means the original attribution was incomplete;
        # kill every exact handle, retain disk evidence, and fail closed.
        if timeout_kids != kids or any(fd is None for fd in timeout_fds):
            abort_frozen_owner(proc.pid, parent_fd, [*fds, *timeout_fds])
            close_pidfds([parent_fd, *fds, *timeout_fds])
            teardown = {
                "mode": "exec",
                "all_gone": False,
                "child_attribution_established": False,
                "disk_reap_skipped": True,
                "initial_children": kids,
                "timeout_children": timeout_kids,
            }
            error = SurvivedTeardown(
                f"exec clone child set changed after ownership publication: "
                f"{kids} -> {timeout_kids}; state {state_path} and data "
                f"{data_dir} NOT reaped",
                teardown,
            )
            rec.update(
                ok=False,
                error=str(error),
                teardown=teardown,
                blocking_ms=(time.monotonic() - t0) * 1000,
                wall_ms=(time.monotonic() - t0) * 1000,
                log=log,
            )
            error.record = rec
            raise error
        close_pidfds(timeout_fds)
        try:
            pidfd_send_signal(parent_fd, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass
        if not wait_pidfds([parent_fd], args.teardown_timeout):
            rec["parent_survived_kill"] = True
            for fd in [parent_fd, *fds]:
                if fd is not None:
                    os.close(fd)
            teardown = {
                "mode": "exec",
                "all_gone": False,
                "disk_reap_skipped": True,
                "parent_survived_kill": True,
            }
            error = SurvivedTeardown(
                f"exec clone owner {proc.pid} survived SIGKILL; state {state_path} "
                f"and data {data_dir} NOT reaped",
                teardown,
            )
            rec.update(
                ok=False,
                error=str(error),
                teardown=teardown,
                blocking_ms=(time.monotonic() - t0) * 1000,
                wall_ms=(time.monotonic() - t0) * 1000,
                log=log,
            )
            error.record = rec
            raise error
    rc = proc.wait()
    all_gone = wait_pidfds(fds, args.teardown_timeout)

    teardown = {
        "mode": "exec",
        "all_gone": all_gone,
        "child_attribution_established": True,
    }
    rec["teardown"] = teardown
    if not all_gone:
        poller = select.poll()
        for fd in fds:
            if fd is not None:
                poller.register(fd, select.POLLIN)
        ready = {fd for fd, _event in poller.poll(0)}
        survivors = {
            p: proc_comm(p)
            for p, fd in zip(kids, fds)
            if fd is not None and fd not in ready
        }
        teardown["survivors"] = survivors
        teardown["disk_reap_skipped"] = True
        rec["survivors"] = survivors
        rec["disk_reap_skipped"] = True
        for pid, fd in zip(kids, fds):
            if pid not in survivors or fd is None:
                continue
            try:
                pidfd_send_signal(fd, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
        wait_pidfds(fds, args.teardown_timeout)
        close_pidfds([parent_fd, *fds])
        error = SurvivedTeardown(
            f"exec arm left {survivors} alive; state {state_path} and data {data_dir} "
            "NOT reaped",
            teardown,
        )
        rec.update(ok=False, error=str(error), log=log)
        rec["blocking_ms"] = rec["wall_ms"] = (time.monotonic() - t0) * 1000
        error.record = rec
        raise error
    close_pidfds([parent_fd, *fds])

    expected = [state_path, f"{state_path}.lock", data_dir]
    left = [path for path in expected if path and os.path.lexists(path)]
    teardown["disk_cleanup_verified"] = not left
    if left:
        teardown["disk_cleanup_left"] = left
        teardown["disk_reaped"] = reap_disk(
            rec,
            args.data_root,
            state_path,
            data_dir,
            (proc.pid, fcvm_start_time),
        )
        still_left = [path for path in expected if path and os.path.lexists(path)]
        teardown["disk_reap_failed"] = still_left
        teardown["disk_cleanup_verified"] = not still_left and not rec.get("disk_errors")
        if (
            still_left
            or rec.get("disk_errors")
            or (not rec["timed_out"] and interrupted is None)
        ):
            error = SurvivedTeardown(
                f"exec arm left on-disk state {left}; exact-path reap left {still_left}",
                teardown,
            )
            rec.update(ok=False, error=str(error), log=log)
            rec["blocking_ms"] = rec["wall_ms"] = (time.monotonic() - t0) * 1000
            error.record = rec
            raise error

    wall = (time.monotonic() - t0) * 1000
    render_ms = None
    try:
        with open(log, "rb") as f:
            for line in f:
                if b"RENDER_OK" in line:
                    for tok in line.decode("utf8", "replace").split():
                        if tok.startswith("total_ms="):
                            render_ms = float(tok.split("=", 1)[1])
    except OSError:
        pass
    rec.update(
        ok=(rc == 0 and not rec["timed_out"]),
        # The exec arm has no separable "response in hand" instant that the host
        # can observe: fcvm returns only after its own teardown. blocking == wall
        # is not an approximation, it is the arm's defining property.
        blocking_ms=wall, wall_ms=wall,
        render_total_ms=render_ms, rc=rc, log=log,
    )
    if not rec["ok"]:
        reason = "timed out" if rec["timed_out"] else f"exited with status {rc}"
        rec["error"] = f"exec clone {reason}: {log_tail(log)}"
    if interrupted is not None:
        raise interrupted
    raise_if_harness_interrupted()
    return rec


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--serve-pid", type=int, default=0,
                   help="UFFD serve pid (omit when using --snapshot-tag)")
    p.add_argument("--snapshot-tag", default="",
                   help="FILE-backed restore from this tag instead of a UFFD serve")
    p.add_argument("--url", required=True)
    p.add_argument("--arms", default="exec,cdp,cdp-fast,noop")
    p.add_argument("--reps", type=int, default=10)
    p.add_argument("--warmup", type=int, default=2, help="discarded EXPLICITLY, and reported")
    p.add_argument("--seed", type=int, default=20260808)
    p.add_argument("--format", choices=("png", "jpeg"), default="jpeg")
    p.add_argument("--quality", type=int, default=80)
    p.add_argument("--cdp-port", type=int, default=9222)
    p.add_argument("--image", default="")
    p.add_argument("--image-id", default="")
    p.add_argument("--snapshot-name", default="")
    p.add_argument("--network-mode", default="")
    p.add_argument("--cpu", type=int, default=0)
    p.add_argument("--memory-mib", type=int, default=0)
    p.add_argument("--ws-url", default="")
    p.add_argument("--fcvm", default=os.path.join(HERE, "..", "..", "target", "release", "fcvm"))
    p.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    p.add_argument("--state-dir", default="")
    p.add_argument("--out-dir", required=True)
    p.add_argument("--timeout", type=float, default=120.0)
    p.add_argument("--teardown-timeout", type=float, default=60.0)
    p.add_argument("--rust-log", default="fcvm=debug")
    p.add_argument("--run-id", default="",
                   help="32-hex invocation identity (generated when omitted)")
    args = p.parse_args()
    signal.signal(signal.SIGINT, record_harness_interrupt)
    signal.signal(signal.SIGTERM, record_harness_interrupt)
    args.state_dir = args.state_dir or os.path.join(args.data_root, "state")

    # EXACTLY ONE backend. With neither flag `clone_backend_args` returned
    # `["--pid", "0"]` — a silently wrong invocation that would be recorded as
    # `"backend": "uffd"` in the run metadata. With both, the tag silently wins
    # and the serve is measured as if it were file-backed. These are different
    # memory backends with different per-request costs; mixing them is AGENTS.md
    # defect 1.
    if bool(args.serve_pid) == bool(args.snapshot_tag):
        p.error("give exactly one of --serve-pid (UFFD) or --snapshot-tag (FILE)")

    snapshot_name = args.snapshot_name or args.snapshot_tag
    if not snapshot_name:
        p.error("--snapshot-name is required for an auditable UFFD run")
    if not valid_snapshot_name(snapshot_name):
        p.error(
            "snapshot name must be 1..128 ASCII letters, digits, '-', '_', or '.', "
            "excluding . and .."
        )
    if not args.image_id:
        p.error("--image-id is required for an auditable run")
    if not args.network_mode or args.cpu <= 0 or args.memory_mib <= 0:
        p.error("--network-mode, positive --cpu, and positive --memory-mib are required")
    snapshot_lock_path = os.path.join(
        args.data_root, "snapshots", f"{snapshot_name}.lock"
    )
    try:
        snapshot_lock = open(snapshot_lock_path, "a+")
        fcntl.flock(snapshot_lock, fcntl.LOCK_SH)
        snapshot = snapshot_generation(args.data_root, snapshot_name)
    except RuntimeError as error:
        p.error(str(error))
    except OSError as error:
        p.error(f"cannot hold snapshot generation lock {snapshot_lock_path}: {error}")

    declared_shape = {
        "image": args.image,
        "image_id": args.image_id,
        "network_mode": args.network_mode,
        "vcpu": args.cpu,
        "memory_mib": args.memory_mib,
    }
    for field, declared in declared_shape.items():
        if declared != snapshot[field]:
            p.error(
                f"declared {field}={declared!r} does not match snapshot "
                f"{snapshot_name} {field}={snapshot[field]!r}; recreate the golden "
                "or use its actual shape"
            )
    cdp_mapping = any(
        isinstance(mapping, dict)
        and mapping.get("proto") == "tcp"
        and mapping.get("host_port") == args.cdp_port
        and mapping.get("guest_port") == args.cdp_port
        for mapping in snapshot["port_mappings"]
    )
    if not cdp_mapping:
        p.error(
            f"snapshot {snapshot_name} does not publish TCP "
            f"{args.cdp_port}:{args.cdp_port}: {snapshot['port_mappings']!r}"
        )
    try:
        uffd_mode = (
            serve_uffd_mode(args.state_dir, args.serve_pid, snapshot_name)
            if args.serve_pid
            else "file"
        )
    except RuntimeError as error:
        p.error(str(error))

    args.fcvm = os.path.abspath(args.fcvm)
    runtime_bundle = os.environ.get("REQBENCH_RUNTIME_BUNDLE", "")
    manifest_path = os.path.join(runtime_bundle, "MANIFEST.sha256")
    if os.path.realpath(runtime_bundle) != os.path.realpath(HERE):
        p.error("reqbench.py must execute from reqbench.sh's staged runtime bundle")
    if not os.path.isfile(manifest_path):
        p.error(f"staged runtime manifest is missing: {manifest_path}")
    current_fcvm_sha256 = sha256_file(args.fcvm)
    current_runtime_bundle_sha256 = sha256_file(manifest_path)
    current_source_revision = os.environ.get("REQBENCH_SOURCE_REVISION", "")
    creator_identity = {
        "creator_fcvm_sha256": current_fcvm_sha256,
        "creator_runtime_bundle_sha256": current_runtime_bundle_sha256,
        "source_revision": current_source_revision,
    }
    for field, current in creator_identity.items():
        if snapshot[field] != current:
            p.error(
                f"snapshot {snapshot_name} was created with {field}="
                f"{snapshot[field]!r}, current runtime is {current!r}; recreate "
                "the golden with this staged runtime"
            )
    os.makedirs(args.out_dir, exist_ok=True)
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    allowed_arms = {"exec", "cdp", "cdp-fast", "noop"}
    if not arms or len(set(arms)) != len(arms) or any(a not in allowed_arms for a in arms):
        p.error(
            "--arms must be a non-empty, duplicate-free subset of "
            "exec,cdp,cdp-fast,noop"
        )
    if "noop" not in arms or "exec" not in arms or not ({"cdp", "cdp-fast"} & set(arms)):
        p.error("publication runs require exec, noop, and at least one CDP arm")

    args.run_id = args.run_id or uuid.uuid4().hex
    if (
        len(args.run_id) != 32
        or any(character not in "0123456789abcdef" for character in args.run_id)
    ):
        p.error("--run-id must be exactly 32 lowercase hexadecimal characters")

    # Defect 2 of bench/chromium/AGENTS.md: interleave, never block. Arms are
    # shuffled request-by-request from a RECORDED seed so an arm's effect cannot
    # be confounded with wall-clock drift, exactly the failure that killed the
    # retracted egress ordering.
    rng = random.Random(args.seed)
    schedule = []
    for rep in range(args.warmup + args.reps):
        order = list(arms)
        rng.shuffle(order)
        for arm in order:
            schedule.append((rep, arm, rep < args.warmup))

    out_path = os.path.join(args.out_dir, "reqbench.jsonl")
    run_id = args.run_id
    try:
        quiet_guard_loadavg1 = float(os.environ["REQBENCH_GUARD_LOADAVG1"])
        quiet_guard_vm_processes = int(
            os.environ["REQBENCH_GUARD_VM_PROCESSES"]
        )
        quiet_loadavg1_limit = float(
            os.environ["REQBENCH_QUIET_LOADAVG1_LIMIT"]
        )
    except (KeyError, ValueError) as error:
        p.error(f"quiet-host guard provenance is incomplete: {error}")
    with open(out_path, "a") as out:
        meta = {
            "kind": "meta", "run_id": run_id, "seed": args.seed,
            "backend": "file" if args.snapshot_tag else "uffd", "arms": arms, "reps": args.reps,
            "uffd_mode": uffd_mode,
            "warmup": args.warmup, "url": args.url, "format": args.format,
            "quality": args.quality,
            "source_revision": current_source_revision,
            "fcvm_path": args.fcvm,
            "fcvm_sha256": current_fcvm_sha256,
            "fcvm_version": command_text([args.fcvm, "--version"]),
            "harness_sha256": harness_sha256(),
            "runtime_bundle_sha256": current_runtime_bundle_sha256,
            "snapshot": snapshot_name,
            "snapshot_created_at": snapshot["created_at"],
            "snapshot_vm_id": snapshot["vm_id"],
            "image": snapshot["image"],
            "image_id": snapshot["image_id"],
            "cdp_port": args.cdp_port,
            "port_mappings": snapshot["port_mappings"],
            "network_mode": snapshot["network_mode"],
            "cpu": snapshot["vcpu"],
            "memory_mib": snapshot["memory_mib"],
            "rust_log": args.rust_log,
            "ws_url_prewired": bool(args.ws_url),
            "allow_busy": os.environ.get("ALLOW_BUSY", "0") == "1",
            "quiet_guard_passed": os.environ.get("REQBENCH_QUIET_GUARD") == "1",
            "quiet_guard_loadavg1": quiet_guard_loadavg1,
            "quiet_vm_processes": quiet_guard_vm_processes,
            "quiet_loadavg1_limit": quiet_loadavg1_limit,
            "host_boot_id": read_trimmed("/proc/sys/kernel/random/boot_id"),
            "host_kernel_release": platform.release(),
            "host_machine": platform.machine(),
            "loadavg": read_trimmed("/proc/loadavg").split()[:3],
            "started": time.time(),
        }
        out.write(json.dumps(meta) + "\n")
        out.flush()
        for rep, arm, is_warmup in schedule:
            # A signal delivered between attempts cannot own a process, so exit
            # before spawning the next clone. Signals delivered during an
            # attempt are re-raised only after that attempt's exact teardown.
            raise_if_harness_interrupted()
            # Every rep records something, including the ones that blow up. A rep
            # that raises out of the loop used to take the whole run with it and
            # leave no trace in the artifact, so `n=` was the only evidence that
            # anything had been dropped.
            try:
                if arm == "exec":
                    rec = run_exec_request(args, rep)
                elif arm == "noop":
                    rec = run_noop_request(args, rep)
                else:
                    rec = run_cdp_request(args, rep, fast=(arm == "cdp-fast"))
                fatal = None
            except SurvivedTeardown as e:
                rec = dict(e.record) or {"arm": arm, "rep": rep}
                request_error = rec.get("request_error") or rec.get("error")
                rec["ok"] = False
                rec["teardown_error"] = str(e)
                rec["error"] = (
                    f"{request_error}; teardown: {e}"
                    if request_error
                    else f"{type(e).__name__}: {e}"
                )
                fatal = e
            except Exception as e:  # noqa: BLE001 - record, then re-raise
                rec = {"arm": arm, "rep": rep, "ok": False, "error": f"{type(e).__name__}: {e}"}
                fatal = e
            rec["warmup"] = is_warmup  # discarded explicitly at analysis, never silently
            rec["run_id"] = run_id
            rec["record_id"] = f"{run_id}:{arm}:{rep}:{int(is_warmup)}"
            rec["loadavg1"] = float(read_trimmed("/proc/loadavg").split()[0])
            out.write(json.dumps(rec) + "\n")
            out.flush()
            print(
                f"{arm:9s} rep={rep}{' [warmup]' if is_warmup else ''} "
                f"ok={rec.get('ok')} blocking={rec.get('blocking_ms', 0):.1f}ms "
                f"wall={rec.get('wall_ms', 0):.1f}ms "
                f"teardown={rec.get('teardown', {}).get('teardown_total_ms', 0):.1f}ms",
                flush=True,
            )
            if fatal is not None:
                # STOP. A survivor means the box now carries a microVM this harness
                # cannot see; every later rep would be measuring contention.
                print(f"\nABORTING SCHEDULE: {fatal}", file=sys.stderr, flush=True)
                print(f"wrote {out_path}")
                return 4
    print(f"\nwrote {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
