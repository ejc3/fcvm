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

Whole-machine `/proc/stat` busy-jiffy deltas are recorded over a window that
encloses the reclaim, then compared with an adjacent post-terminal ambient
control. The harness's own `/proc/<pid>/stat` delta is subtracted from both
windows so the sampler cannot charge itself to reclaim. Those are coarse,
independently quantized counters: raw values and their conservative interval are
retained with every record, and a point is constrained to zero only when the raw
negative lies inside that interval. A pre-kill control includes the still-running
VM's ordinary CPU and subtracts work absent from reclaim, so that older accounting
was withdrawn.

At saturation that CPU competes with new requests. A latency win is not a
capacity win, and this harness deliberately makes it impossible to report one as
the other.
"""

import argparse
import ctypes
import errno
import fcntl
import hashlib
import ipaddress
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
import traceback
import sys
import time
import uuid
from contextlib import ExitStack
from urllib.parse import urlparse, urlunparse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

CLK_TCK = os.sysconf("SC_CLK_TCK")
MACHINE_CPU_RESOLUTION_MS = 1000.0 / CLK_TCK
HARNESS_CPU_RESOLUTION_MS = 1000.0 / CLK_TCK
# /proc/stat busy is the sum of six separately quantized fields; harness CPU is
# the sum of two. A difference of their deltas therefore carries at most eight
# counter quanta of uncertainty. The interval is intentionally broad instead of
# pretending a 50 ms control supports sub-jiffy precision.
CPU_RESIDUAL_UNCERTAINTY_MS = (
    6 * MACHINE_CPU_RESOLUTION_MS + 2 * HARNESS_CPU_RESOLUTION_MS
)
MACHINE_CPU_SOURCE = "/proc/stat:busy(user,nice,system,irq,softirq,steal)"
HARNESS_CPU_SOURCE = "/proc/self/stat:utime+stime"
CONTROL_WINDOW_S = 0.05


# ---------------------------------------------------------------- procfs utils


def private_dirty_total_kb(per_child: dict) -> int | None:
    """Total privatised memory, or None when the total would be a fiction.

    None unless EVERY pinned process was sampled, and None for an empty set. A
    partial sum is not a smaller measurement, it is a different one: if
    firecracker exits between the pin and the read, the remaining processes
    total a few MiB against its few hundred, and a number is what a reader
    takes at face value.

    A function rather than an inline expression so a test can call the rule the
    record is actually built from. Inlining a copy into the test proved only
    that Python sums the way Python sums.
    """
    if not per_child:
        return None
    values = [m.get("private_dirty_kb") for m in per_child.values()]
    if any(v is None for v in values):
        return None
    return sum(values)


def proc_private_dirty_kb(pid: int) -> dict:
    """What this process has PRIVATISED, from /proc/<pid>/smaps_rollup.

    NOT "the pages it wrote". What Private_Dirty counts depends on the memory
    backend, and the two the harness compares do not agree:

      uffd copy   UFFDIO_COPY populates ANONYMOUS memory, so every page the
                  server fills is private the moment it is filled, written or
                  not. With --uffd-prefetch on (the default) the whole recorded
                  working set, about 56k pages, is replayed BEFORE the guest
                  runs and all of it counts here.
      uffd minor  UFFDIO_CONTINUE installs a read-only PTE to the shared page,
                  so a replayed page costs nothing until it is written. This is
                  the arm where the field really does mean "pages written".
      file        MAP_PRIVATE over the page cache: only real writes count.

    So this number is comparable ACROSS CLONES of one configuration, and NOT
    across backends or across prefetch settings, unless the reader also has
    those settings. The caller records them beside the figure for that reason.

    Read alive, immediately before the kill, so it describes the request that
    just completed. It cannot be recovered afterwards the way CPU can, because
    smaps_rollup dies with the address space.

    Not readable from a zombie. smaps_rollup disappears with the address space,
    which is why this cannot be sampled alongside the CPU figures after exit.

    Returns the reason rather than a zero when it cannot measure. A zero with no
    uncertainty is a claim, and an unreadable file does not support one.
    """
    try:
        with open(f"/proc/{pid}/smaps_rollup", "r", encoding="ascii") as handle:
            rollup = handle.read()
    except FileNotFoundError:
        return {"private_dirty_kb": None, "unavailable": "no smaps_rollup (exited, or kernel lacks CONFIG_PROC_PAGE_MONITOR)"}
    except PermissionError as error:
        return {"private_dirty_kb": None, "unavailable": f"permission: {error}"}
    except OSError as error:
        return {"private_dirty_kb": None, "unavailable": f"{type(error).__name__}: {error}"}
    fields = {}
    for line in rollup.splitlines():
        key, _, rest = line.partition(":")
        if key in ("Private_Dirty", "Private_Clean", "Shared_Clean", "Shared_Dirty", "Pss", "Rss"):
            try:
                fields[key.lower() + "_kb"] = int(rest.strip().split()[0])
            except (IndexError, ValueError):
                pass
    if "private_dirty_kb" not in fields:
        return {"private_dirty_kb": None, "unavailable": "smaps_rollup carried no Private_Dirty"}
    return fields


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


def machine_cpu_ms() -> float:
    """Non-idle CPU milliseconds from /proc/stat's aggregate line."""
    with open("/proc/stat") as f:
        parts = f.readline().split()
    # guest/guest_nice are already included in user/nice. Only the first eight
    # counters are independent; remove idle+iowait to leave six busy fields.
    values = [int(value) for value in parts[1:9]]
    busy_jiffies = sum(values) - values[3] - values[4]
    return busy_jiffies * 1000.0 / CLK_TCK


def self_cpu_ms() -> float:
    """This harness's own utime+stime, in ms.

    Subtracted from BOTH accounting windows so the sampler's own load cancels by
    construction instead of being attributed to kernel reclaim. Without this the
    control window and the reclaim window carry different amounts of harness CPU
    and the subtraction removes work that was never done in the window it is
    subtracted from.
    """
    fields = proc_stat_fields(os.getpid())
    return (
        (fields[1] + fields[2]) * 1000.0 / CLK_TCK
        if fields is not None
        else 0.0
    )


# Answer to machine_counter_tracks_this_process(), which is a property of the
# host and therefore constant for the life of the process. None until asked.
_MACHINE_COUNTER_TRACKS = None
# Paired idle/burn windows per probe. Five is the smallest count that gives a
# median with a majority behind it, so one unlucky window cannot decide the
# answer on a host whose ambient load moves between windows.
CPU_TRACKING_PAIRS = 5


class MachineCpuCounterUnusable(RuntimeError):
    """The machine-wide CPU counter does not track this process.

    Distinct from an enclosure violation. A violation means the measurement
    disagreed with itself and the numbers are wrong; this means the environment
    cannot produce the measurement at all. Conflating them turned an
    unsupported CI runner into what looked like a correctness bug in the
    accounting.
    """


def machine_counter_tracks_this_process(burn_ms: float = None) -> bool:
    """Burn a known amount of CPU and check that /proc/stat noticed.

    The enclosure property the accounting rests on is that the machine-wide
    counter includes this process. That is a property of the HOST, and it is
    directly testable: burn CPU deliberately, read both counters around it, and
    see whether the machine counter advanced by at least what we spent.

    This exists because thresholding the symptom does not work. The first
    version of the guard below asked `machine_ms == 0.0`, which classified the
    GitHub-hosted runner correctly on one run and not the next: the same host
    later reported `machine=10.000000ms harness=160.000000ms` — one 10 ms jiffy
    of movement against 160 ms of our own CPU. A counter that moves by one tick
    is no more tracking us than one that does not move at all, but no fixed
    cutoff on the observed shortfall can say so without also swallowing real
    accounting bugs, whose shortfall is resolution-scale by construction.

    A controlled burn separates them, because it fixes the quantity the counter
    is supposed to reproduce.
    """
    # The burn has to clear the residual tolerance, or the comparison below is
    # inside the noise and the probe answers "tracks" no matter what the host
    # does. Caught by test_the_probe_reports_a_frozen_counter_as_untracked: a
    # 60 ms burn against this tolerance (8 quanta, 80 ms at 100 Hz) took the
    # cannot-distinguish branch and called a frozen counter healthy.
    if burn_ms is None:
        burn_ms = 4 * CPU_RESIDUAL_UNCERTAINTY_MS
    # Whether /proc/stat encloses this process is a property of the HOST, so it
    # cannot change between reps and there is no reason to re-measure it. It is
    # also expensive to ask: the probe burns a full core for ~320 ms, and
    # bounded_cpu_residual calls it on every violation, so on a host that
    # violates every rep a 202-rep campaign would spend a minute of busy-spin
    # INSIDE the teardown path -- landing in the ambient control window of the
    # reps either side and inflating exactly the baseline the accounting
    # subtracts. Answer once per process.
    global _MACHINE_COUNTER_TRACKS
    if _MACHINE_COUNTER_TRACKS is not None:
        return _MACHINE_COUNTER_TRACKS
    # PAIRED windows, not one burn. A single burn compares machine movement
    # against our own spend, and on a busy host AMBIENT load supplies that
    # movement all by itself -- so the probe answered "tracks" for a counter
    # that excludes us entirely, purely because the box was busy. The failure is
    # quiet: bounded_cpu_residual then raises RuntimeError (a real accounting
    # bug) where it should raise MachineCpuCounterUnusable (an unusable host),
    # so the operator is sent to debug the wrong thing.
    #
    # Each pair measures machine movement over an IDLE window and over an
    # equal-length BURN window. Ambient load appears in both and cancels in the
    # difference; only work attributable to this process survives it. Five pairs
    # because one difference is a single noisy sample of a quantity that varies
    # with whatever else the box is doing -- the median is what makes the answer
    # about the host rather than about the moment.
    #
    # Cost: 5 * 2 * burn_ms, about 3.2s, paid ONCE per process (memoized). The
    # expense that mattered was calling this on every violation -- 202 reps of a
    # 320ms spin inside the teardown path, landing in the ambient control window
    # of the reps either side. Memoization fixed that, and it fixes this.
    window_s = burn_ms / 1000.0
    excesses = []
    spends = []
    for _ in range(CPU_TRACKING_PAIRS):
        idle_m0 = machine_cpu_ms()
        time.sleep(window_s)  # sleep, NOT spin: the idle window must stay idle
        idle_delta = machine_cpu_ms() - idle_m0

        burn_m0, h0 = machine_cpu_ms(), self_cpu_ms()
        end = time.monotonic() + window_s
        while time.monotonic() < end:
            pass
        burn_delta, spent = machine_cpu_ms() - burn_m0, self_cpu_ms() - h0
        excesses.append(burn_delta - idle_delta)
        spends.append(spent)

    excesses.sort()
    spends.sort()
    middle = len(excesses) // 2
    excess, spent = excesses[middle], spends[middle]
    if spent <= CPU_RESIDUAL_UNCERTAINTY_MS:
        # We failed to burn measurably more than the tolerance (a preempted or
        # heavily throttled probe), so it cannot distinguish anything. Fail
        # toward "tracks", which keeps the real-violation path live rather than
        # excusing it. NOT memoized: this is a statement about the probe, not
        # about the host, so a later attempt may still answer the question.
        return True
    if excess >= spent - CPU_RESIDUAL_UNCERTAINTY_MS:
        _MACHINE_COUNTER_TRACKS = True
        return True
    # The median says "not tracking" -- but that verdict is only trustworthy if
    # the pairs AGREE. Pairing cancels STEADY ambient; it amplifies FLUCTUATING
    # ambient, because a load that starts or stops between the idle window and
    # the burn window lands in the difference at full weight. Observed: two
    # test suites running concurrently made this probe condemn a host that
    # tracks perfectly (excess spread far beyond tolerance, median dragged
    # low). When the spread of excesses exceeds the tolerance the probe cannot
    # distinguish "counter excludes us" from "ambient was choppy", so it fails
    # toward "tracks" -- keeping the strict violation path (RuntimeError) live
    # rather than excusing the host -- and does NOT memoize, since choppiness
    # is a statement about the moment, not the host. A genuinely untracked
    # counter yields excesses that agree near zero (spread within tolerance)
    # and is still condemned.
    spread = excesses[-1] - excesses[0]
    if spread > CPU_RESIDUAL_UNCERTAINTY_MS:
        return True
    _MACHINE_COUNTER_TRACKS = False
    return False


def bounded_cpu_residual(machine_ms: float, harness_ms: float, tracks=None) -> dict:
    """Constrain M-H only within the counters' declared resolution.

    Both inputs are deltas of cumulative counters, so each contributes two
    endpoint errors. Machine reads enclose harness reads; a raw residual below
    the resulting negative tolerance is therefore internally impossible and
    invalidates the measurement instead of being hidden by a clamp.
    """
    raw_ms = machine_ms - harness_ms
    uncertainty_ms = CPU_RESIDUAL_UNCERTAINTY_MS
    # A machine counter that did not move AT ALL while this process
    # demonstrably burned CPU is not an enclosure violation, it is a counter
    # that does not track this process. Observed on GitHub-hosted runners:
    # machine=0.000000ms against harness=150.000000ms. Reporting that as
    # "host CPU delta is smaller than enclosed harness CPU delta" sends the
    # reader hunting a measurement bug that is not there, and it is the reason
    # the bench suite failed in CI while passing on every real bench host.
    #
    # Named separately so the two cases cannot be confused. A REAL violation
    # (the machine counter moved, but by less than the harness) still raises
    # below and still invalidates the measurement.
    if raw_ms < -uncertainty_ms:
        # Which of the two is it? Ask the host, do not guess from the shortfall.
        probe = machine_counter_tracks_this_process if tracks is None else tracks
        if not probe():
            raise MachineCpuCounterUnusable(
                f"/proc/stat does not track this process: a controlled CPU burn "
                f"did not move the machine-wide counter by what this process "
                f"spent, so it cannot enclose it. The measurement that triggered "
                f"this check read machine={machine_ms:.6f}ms against "
                f"harness={harness_ms:.6f}ms. This needs a host whose /proc/stat "
                f"reflects its own processes; GitHub-hosted runners do not."
            )
        raise RuntimeError(
            "host CPU delta is smaller than enclosed harness CPU delta: "
            f"machine={machine_ms:.6f}ms harness={harness_ms:.6f}ms "
            f"raw={raw_ms:.6f}ms tolerance={uncertainty_ms:.6f}ms"
        )
    point_ms = max(0.0, raw_ms)
    return {
        "raw_ms": raw_ms,
        "point_ms": point_ms,
        "lo_ms": max(0.0, raw_ms - uncertainty_ms),
        "hi_ms": max(0.0, raw_ms + uncertainty_ms),
        "uncertainty_ms": uncertainty_ms,
        "clamped": raw_ms < 0.0,
    }


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
TERMINATION_SIGNALS = {signal.SIGINT, signal.SIGTERM}


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
            # Cap at 2 ms, not 20: with a 20 ms cap the attempts past the
            # ramp land ~17-20 ms apart, so restore-readiness figures snapped
            # to the probe grid — a small real effect near an attempt boundary
            # read as a clean bimodal restore floor until the fine-grid gated
            # run (reqbench-20260814-035757) showed readiness is unimodal.
            # ~20 extra connect attempts per 40 ms wait is noise; measurement
            # resolution is not.
            delay = min(delay * 1.5, 0.002)


def sha256_file(path: str) -> str:
    """Content identity for the exact fcvm binary used by the run."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


# Every script that defines one request sample, for either engine. Must stay
# equal to reqbench.sh's staged runtime sources minus the binaries and the
# analyzer (which reads samples but defines none); asserted by
# harness_hash_covers_every_staged_request_script in test_reqbench.py.
HARNESS_SOURCES = ("reqbench.py", "cdpdrive.py", "render.py", "wddrive.py",
                   "reqbench.sh")


def harness_sha256() -> str:
    """Content identity for every script that defines one request sample."""
    h = hashlib.sha256()
    h.update(b"fcvm-chromium-request-harness-v1\0")
    for name in HARNESS_SOURCES:
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
    records from different memory/disk generations, so the exact generation UUID
    and the digest of the config.json bytes are both load-bearing identities.
    """
    path = os.path.join(data_root, "snapshots", snapshot_name, "config.json")
    try:
        with open(path, "rb") as f:
            config_json = f.read()
        config = json.loads(config_json)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"cannot identify snapshot generation from {path}: {error}")
    generation_id = config.get("generation_id")
    try:
        canonical_generation_id = str(uuid.UUID(generation_id))
    except (AttributeError, TypeError, ValueError):
        raise RuntimeError(f"snapshot config {path} has invalid generation_id")
    if canonical_generation_id != generation_id:
        raise RuntimeError(f"snapshot config {path} has non-canonical generation_id")
    config_sha256 = hashlib.sha256(config_json).hexdigest()
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
    # The resolver fcvm gave the guest, as metadata.network_config.dns_server.
    # With `fcvm podman prepare --dns` (reqbench.sh GUEST_DNS) it is the
    # requested one; without, rootless guests record null and bridged guests
    # record the host's first resolver. Which of those it is comes from the
    # provenance's guest_dns below, not from this value.
    network_config = metadata.get("network_config")
    dns_server = None
    if network_config is not None:
        if not isinstance(network_config, dict):
            raise RuntimeError(f"snapshot config {path} has no network_config object")
        dns_server = network_config.get("dns_server")
        if dns_server is not None:
            try:
                canonical_dns_server = str(ipaddress.ip_address(dns_server))
            except (TypeError, ValueError):
                canonical_dns_server = None
            if canonical_dns_server != dns_server:
                raise RuntimeError(
                    f"snapshot config {path} has invalid network_config.dns_server "
                    f"{dns_server!r}: expected an IP literal"
                )

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
        "snapshot_generation_id": generation_id,
        "snapshot_config_sha256": config_sha256,
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
    ):
        raise RuntimeError(f"snapshot provenance {provenance_path} has invalid image_id")
    image_digest = provenance.get("image_digest")
    if image_digest != "" and (
        not isinstance(image_digest, str)
        or len(image_digest) != 71
        or not image_digest.startswith("sha256:")
        or any(character not in "0123456789abcdef" for character in image_digest[7:])
    ):
        raise RuntimeError(f"snapshot provenance {provenance_path} has invalid image_digest")
    image_cache_key = provenance.get("image_cache_key")
    if (
        not isinstance(image_cache_key, str)
        or len(image_cache_key) != 64
        or any(character not in "0123456789abcdef" for character in image_cache_key)
    ):
        raise RuntimeError(f"snapshot provenance {provenance_path} has invalid image_cache_key")
    # guest_dns is the resolver the golden REQUESTED (null: none), which is
    # what the run meta records and the analyzer gates on. The effective
    # dns_server is not a substitute: a bridged guest inherits the host's
    # resolver without anyone asking for it, and a run that resolved its
    # hostnames on the live internet must not read as one with a controlled
    # resolver. When one was requested the snapshot has to have baked it.
    if "guest_dns" not in provenance:
        raise RuntimeError(
            f"snapshot provenance {provenance_path} records no guest_dns; "
            "recreate the golden snapshot with reqbench.sh golden"
        )
    guest_dns = provenance["guest_dns"]
    if guest_dns is not None:
        try:
            canonical_guest_dns = str(ipaddress.ip_address(guest_dns))
        except (TypeError, ValueError):
            canonical_guest_dns = None
        if not isinstance(guest_dns, str) or canonical_guest_dns != guest_dns:
            raise RuntimeError(
                f"snapshot provenance {provenance_path} has invalid guest_dns "
                f"{guest_dns!r}: expected an IP literal or null"
            )
        if dns_server != guest_dns:
            raise RuntimeError(
                f"snapshot provenance {provenance_path} requested guest_dns "
                f"{guest_dns!r} but the snapshot baked dns_server {dns_server!r}"
            )
    image_disk_path = metadata.get("image_disk_path")
    if (
        not isinstance(image_disk_path, str)
        or not os.path.basename(image_disk_path).startswith(image_cache_key + ".")
    ):
        raise RuntimeError(
            f"snapshot config {path} image disk does not match provenance cache key"
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
        "generation_id": generation_id,
        "config_sha256": config_sha256,
        "created_at": created_at,
        "vm_id": vm_id,
        "image": image,
        "image_id": image_id,
        "image_digest": image_digest,
        "image_cache_key": image_cache_key,
        "creator_fcvm_sha256": creator_fcvm_sha256,
        "creator_runtime_bundle_sha256": creator_runtime_bundle_sha256,
        "source_revision": source_revision,
        "vcpu": vcpu,
        "memory_mib": memory_mib,
        "network_mode": network_mode,
        "port_mappings": port_mappings,
        "dns_server": dns_server,
        "guest_dns": guest_dns,
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


class SessionDiscoveryFailed(RuntimeError):
    """The golden holds no usable WebDriver session id; no rep can ever render.

    Escalates out of the per-rep handler (after that rep's teardown) instead of
    being recorded and retried: every retry re-spawns a clone, re-execs, and
    fails the same way, so a swallowed discovery failure burns the entire
    schedule and surfaces 202 reps later as uniform navigate failures with the
    real cause off-screen."""


def rep_error_escalates(error: BaseException) -> bool:
    """Whether a per-rep exception must abort the schedule after teardown.

    Non-Exception BaseExceptions (KeyboardInterrupt, HarnessInterrupted) always
    do; SessionDiscoveryFailed does because retrying it is structurally
    pointless (the missing session id is snapshot state, identical for every
    clone)."""
    return not isinstance(error, Exception) or isinstance(error, SessionDiscoveryFailed)


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


def harness_interrupt_pending() -> int:
    """The recorded INT/TERM, or 0. For work that must yield without unwinding.

    `raise_if_harness_interrupted` is for the request scope, which owns a clone
    and has to reach its teardown before it unwinds. Anything OPTIONAL that runs
    between the request and that teardown has the opposite obligation: get out
    of the way, because the shutdown clock is already running and whoever sent
    the signal may escalate to SIGKILL.
    """
    return _pending_harness_signal


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
        # Memory FIRST, then the CPU baseline. Both must be read while the
        # processes are alive (smaps_rollup dies with the address space), but
        # the ORDER is load-bearing: only the fcvm parent is frozen here, so
        # firecracker, holder and pasta keep burning CPU while this runs, and
        # any CPU they burn between the baseline and the kill lands in
        # reclaim_cpu_ms as if the reap had caused it. Walking smaps_rollup
        # costs 3.9-6.6 ms on a 722 MB RSS process (about 6-9 ms per GiB),
        # which is the order of the 10 ms CLK_TCK quantum this function is
        # careful to bound. Sampling before the baseline removes the window
        # entirely rather than making it small.
        pre_memory = {name: proc_private_dirty_kb(pid) for name, pid in tracked.items()}
        pre = {name: proc_stat_fields(pid) for name, pid in tracked.items()}
        missing_pre = [name for name, fields in pre.items() if fields is None]
        if missing_pre:
            raise RuntimeError(
                f"cannot capture pre-kill CPU/starttime for pinned processes {missing_pre}"
            )
        machine_t0 = time.monotonic()
        machine0 = machine_cpu_ms()
        self0 = self_cpu_ms()
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
        self_ms = self_cpu_ms() - self0
        machine1 = machine_cpu_ms()
        machine_window_ms = (time.monotonic() - machine_t0) * 1000.0

        # Measure ambient load only after the exact VM process set is terminal.
        # A pre-kill control contains the still-running VM's ordinary CPU and
        # subtracts work that is absent from the reclaim window.
        ctl_t0 = time.monotonic()
        ctl_machine0 = machine_cpu_ms()
        ctl_self0 = self_cpu_ms()
        time.sleep(CONTROL_WINDOW_S)
        ctl_self_ms = self_cpu_ms() - ctl_self0
        ctl_machine_ms = machine_cpu_ms() - ctl_machine0
        ctl_wall_ms = (time.monotonic() - ctl_t0) * 1000.0
        if ctl_wall_ms <= 0.0:
            raise RuntimeError("ambient control window was not positive")
        # The process set is ALREADY terminal here: the kill, the wait and
        # t_gone all happened above. A CPU-accounting self-check that fails at
        # this point says the MEASUREMENT is unusable on this host; it says
        # nothing about whether teardown worked. Letting it propagate conflated
        # the two and made teardown_fast report "state and data NOT reaped" for
        # a process set that was gone, on GitHub-hosted runners whose /proc/stat
        # under-reports (machine=30ms against harness=160ms in one window while
        # a controlled burn tracked fine).
        #
        # The figure is withheld rather than guessed. Publication already fails
        # closed without reading this field: reqanalyze requires ~20 CPU fields
        # and rejects the run with "fast teardown has no valid
        # machine_cpu_ms_net" when they are absent. Those messages name the
        # SYMPTOM, so this field carries the CAUSE for whoever reads the record.
        # Nothing consumes it programmatically; saying it gates publication
        # would credit it with work the required-field checks are doing.
        cpu_residual_error = None
        try:
            reclaim_cpu = bounded_cpu_residual(machine1 - machine0, self_ms)
            control_cpu = bounded_cpu_residual(ctl_machine_ms, ctl_self_ms)
        except (MachineCpuCounterUnusable, RuntimeError) as err:
            reclaim_cpu = None
            control_cpu = None
            cpu_residual_error = f"{type(err).__name__}: {err}"
        return {
            "all_gone": all_gone,
            "cpu_residual_error": cpu_residual_error,
            "cpu": cpu,
            "ctl_machine_ms": ctl_machine_ms,
            "ctl_self_ms": ctl_self_ms,
            "ctl_wall_ms": ctl_wall_ms,
            "control_cpu": control_cpu,
            "live_exact": live_exact,
            "machine_ms": machine1 - machine0,
            "machine_window_ms": machine_window_ms,
            "parent_live": parent_live,
            "pre": pre,
            "pre_memory": pre_memory,
            "reclaim_cpu": reclaim_cpu,
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
        "accounting_version": "post-terminal-ambient-v2",
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
    cpu = measured["cpu"]
    ctl_machine_ms = measured["ctl_machine_ms"]
    ctl_self_ms = measured["ctl_self_ms"]
    ctl_wall_ms = measured["ctl_wall_ms"]
    control_cpu = measured["control_cpu"]
    machine_cpu_ms = measured["machine_ms"]
    machine_window_ms = measured["machine_window_ms"]
    pre = measured["pre"]
    pre_memory = measured.get("pre_memory", {})
    # Absolute CPU each pinned child had burned at the kill instant. The VM
    # lives exactly one request, so firecracker's figure IS the per-request
    # VMM+vCPU cost; subtracting the noop arm's (restore + idle) yields
    # CPU-per-render. Recorded per child, in ms (utime+stime, CLK_TCK-scaled).
    out["lifetime_cpu_ms_by_child"] = {
        name: (fields[1] + fields[2]) * 1000.0 / CLK_TCK
        for name, fields in pre.items()
        if fields is not None
    }
    reclaim_cpu = measured["reclaim_cpu"]
    sample_period_s = measured["sample_period_s"]
    self_ms = measured["self_ms"]
    t_gone = measured["t_gone"]
    t_kill = measured["t_kill"]
    out["signal_ms"] = measured["signal_ms"]

    window_s = t_gone - t_kill
    out["reap_wall_ms"] = window_s * 1000
    out["all_gone"] = all_gone
    out["machine_cpu_ms"] = machine_cpu_ms
    out["harness_cpu_ms"] = self_ms
    out["machine_cpu_window_ms"] = machine_window_ms
    out["cpu_residual_error"] = measured.get("cpu_residual_error")
    # A host that cannot support the enclosure measurement still gets a full
    # teardown and a full per-child CPU record below; only the residual-DERIVED
    # figures are withheld, and they are ABSENT rather than zeroed, so a reader
    # gets a KeyError instead of a plausible wrong number.
    if reclaim_cpu is not None and control_cpu is not None:
        ctl_rate = control_cpu["point_ms"] / ctl_wall_ms
        ctl_rate_lo = control_cpu["lo_ms"] / ctl_wall_ms
        ctl_rate_hi = control_cpu["hi_ms"] / ctl_wall_ms
        excess_ms = reclaim_cpu["point_ms"] - ctl_rate * machine_window_ms
        excess_lo_ms = reclaim_cpu["lo_ms"] - ctl_rate_hi * machine_window_ms
        excess_hi_ms = reclaim_cpu["hi_ms"] - ctl_rate_lo * machine_window_ms
        out["machine_cpu_ms_raw"] = reclaim_cpu["raw_ms"]
        out["machine_cpu_ms_net"] = reclaim_cpu["point_ms"]
        out["machine_cpu_ms_net_lo"] = reclaim_cpu["lo_ms"]
        out["machine_cpu_ms_net_hi"] = reclaim_cpu["hi_ms"]
        out["machine_cpu_ms_subtraction_clamped"] = reclaim_cpu["clamped"]
        out["machine_cpu_ms_excess"] = excess_ms
        out["machine_cpu_ms_excess_lo"] = excess_lo_ms
        out["machine_cpu_ms_excess_hi"] = excess_hi_ms
        out["control_machine_cpu_ms"] = ctl_machine_ms
        out["control_harness_cpu_ms"] = ctl_self_ms
        out["control_wall_ms"] = ctl_wall_ms
        out["control_target_ms"] = CONTROL_WINDOW_S * 1000.0
        out["control_cpu_ms_raw"] = control_cpu["raw_ms"]
        out["control_cpu_ms_net"] = control_cpu["point_ms"]
        out["control_cpu_ms_net_lo"] = control_cpu["lo_ms"]
        out["control_cpu_ms_net_hi"] = control_cpu["hi_ms"]
        out["control_cpu_ms_subtraction_clamped"] = control_cpu["clamped"]
        out["control_busy_cores"] = ctl_rate
        out["control_busy_cores_lo"] = ctl_rate_lo
        out["control_busy_cores_hi"] = ctl_rate_hi
        out["cpu_residual_uncertainty_ms"] = reclaim_cpu["uncertainty_ms"]
    out["machine_cpu_source"] = MACHINE_CPU_SOURCE
    out["machine_cpu_resolution_ms"] = MACHINE_CPU_RESOLUTION_MS
    out["harness_cpu_source"] = HARNESS_CPU_SOURCE
    out["harness_cpu_resolution_ms"] = HARNESS_CPU_RESOLUTION_MS
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

    # Private_Dirty as read at ONE INSTANT, per pinned process -- an absolute
    # reading, NOT a delta: no baseline is subtracted anywhere. Calling it a
    # delta invites the reader to treat it as "what this request cost", and it
    # is not. With --uffd-prefetch on, the whole recorded working set (~56k
    # pages) is already private before the guest executes an instruction, and
    # every page of it is counted here. proc_private_dirty_kb's docstring is the
    # long form of what the number does and does not include.
    #
    # Reported next to the latency on the SAME record, so a memory/latency
    # frontier is one measurement rather than a join across two experiments on
    # two goldens.
    out["per_child_memory"] = dict(pre_memory)
    out["private_dirty_unmeasured"] = [
        name for name, m in pre_memory.items() if m.get("private_dirty_kb") is None
    ]
    # None unless EVERY pinned process was sampled. A partial sum is not a
    # smaller measurement, it is a different one: if firecracker exits between
    # the pin and the read, the remaining processes total a few MiB against its
    # few hundred, and a reader who does not separately consult
    # private_dirty_unmeasured would take that at face value.
    out["private_dirty_total_kb"] = private_dirty_total_kb(pre_memory)

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


# -------------------------------------------------------------- failure probe
#
# EVIDENCE CAPTURE FOR AN UNSOLVED ROOT CAUSE. Nothing here fixes anything and
# nothing here is measured.
#
# The 808-clone run produced 3 CDP failures. Every one came from a clone whose
# ARP-triggering readiness ping got no reply (5 clones, 3 failed; of the 803
# whose ping replied, none failed), and in each the guest stayed ALIVE for the
# full 100+ seconds (its Chromium kept writing to the serial console) while
# the host-driven CDP path either never connected or was reset mid-session. The
# `exec` and `noop` arms reach the guest over VSOCK and never failed.
#
# What is NOT known is why the guest stopped answering on the IP path and why it
# never recovered. A transient startup race resolves in milliseconds; these ran
# out the client's own deadline. The leading and UNPROVEN hypothesis is that the
# guest's post-restore network re-initialisation never happened or never
# finished for that clone: fcvm PUTs a `restore-epoch` per clone, fc-agent's
# `watch_restore_epoch` reacts to it by flushing the ARP cache, sending a
# gratuitous ARP and re-registering exec, and a clone that misses that keeps
# stale IP networking for life while vsock keeps working.
#
# VSOCK EXEC KEEPS WORKING DURING THE FAILURE, so the broken guest is
# interrogable at exactly the moment it is broken. Until now the harness tore the
# clone down and the evidence died with it.
#
# Three properties this code has to hold, in priority order:
#   1. It must not perturb what is measured. Every probe runs AFTER the record's
#      `blocking_ms` is stamped, so the caller-visible latency is already
#      closed, and only on the failure path, plus exactly one healthy control per run.
#   2. It must be bounded. It runs against a guest that may be wedged, so every
#      command has a hard timeout enforced by killing the process GROUP (an
#      `fcvm exec` that hangs has a guest-side child behind it), and the whole
#      capture has a budget that skips the steps it cannot afford.
#   3. It must never take the request or the run with it. Every failure inside
#      the probe is recorded as data. The dump is the artifact even when most of
#      it failed to collect.


PROBE_COMMAND_TIMEOUT_S = 20.0
"""Hard bound on one probe command. `fcvm exec`'s own connect ladder spans ~54 s
(41 attempts from 5 ms, 1.5x, capped at 2 s, in `src/commands/exec.rs`), so an
unbounded probe against a wedged exec server would outlast the request that
failed. A live restored clone re-registers exec within single-digit
milliseconds, so 20 s is not a race with a healthy guest: hitting this bound is
itself the finding, and it is recorded as `timed_out`."""

PROBE_BUDGET_S = 60.0
"""Hard bound on one whole capture, checked before each step. A step that does not
fit records why it was skipped rather than being silently absent."""

PROBE_SECTION_LIMIT = 64 * 1024
"""Per-section output cap. `dmesg` and `cat /proc/net/tcp` are unbounded in
principle."""

PROBE_BATCH_LIMIT = 512 * 1024
"""Cap on a whole batch's stdout, applied BEFORE it is split into sections.
Deliberately larger than the per-section cap: clipping the stream at the section
cap would cut the last section's status line off and report a completed section
as unknown."""

PROBE_STREAM_LIMIT = 16 * 1024
"""Cap on a probe process's own stderr (where `fcvm exec` reports why it could
not reach the guest)."""

PROBE_SECTION_MARK = "===fcvm-probe-section"
PROBE_RC_MARK = "===fcvm-probe-rc"

PROBE_GUEST_PASSIVE_SECTIONS = (
    # Restore syncs the guest clock from the host (`fc-agent/src/restore.rs`
    # calls `sync_clock_from_host` FIRST), so a guest still sitting at the
    # snapshot's wall time is direct evidence the restore handler never ran.
    ("guest_date", "date -u +%Y-%m-%dT%H:%M:%SZ"),
    ("guest_uptime", "cat /proc/uptime"),
    ("ip_addr", "ip addr"),
    ("ip_link_stats", "ip -s link"),
    ("ip_route", "ip route"),
    # Does the guest hold a stale gateway MAC? `handle_clone_restore` flushes
    # this table and re-ARPs; an entry pointing at the pre-snapshot bridge port,
    # or a FAILED/INCOMPLETE one, is the shape the hypothesis predicts.
    ("ip_neigh", "ip neigh"),
    ("proc_net_arp", "cat /proc/net/arp"),
    ("proc_net_dev", "cat /proc/net/dev"),
    # Is Chromium still listening on the CDP port INSIDE the guest? The
    # container runs `--network=host` (`fc-agent/src/container.rs`), so the VM's
    # netns is the container's netns and this sees the container's listener.
    ("listening_sockets", "ss -ltnp"),
    ("proc_net_tcp", "cat /proc/net/tcp"),
    # `awk '/[c]hrom/'`, not `grep chrom`: the bracket keeps the matcher's own
    # argv from matching, and the full `args` column is the point: which
    # Chromium flags are live, and on which port.
    ("chromium_processes",
     "ps -eo pid,ppid,stat,etimes,comm,args | awk 'NR==1 || /[c]hrom/'"),
    ("dmesg_tail", "dmesg | tail -50"),
    # fc-agent writes its restore-epoch narration to stderr, which lands on the
    # serial console rather than in a file, so this is usually empty. The
    # authoritative copy is the host-side log-marker capture below. It is here
    # because "usually" is not "always" and an empty section costs nothing.
    ("fc_agent_journal",
     "journalctl -b --no-pager -o cat -n 400 | grep -F '[fc-agent]' | tail -40"),
)

PROBE_HOLDER_NS_SECTIONS = (
    ("ns_ip_addr", "ip addr"),
    ("ns_ip_link_stats", "ip -s link"),
    # The host half of the same question `ip_neigh` asks in the guest. This is
    # the exact table `verify_port_forwarding` reads before it declares the
    # clone ready (`src/network/pasta.rs`: `ip neigh show to 10.0.2.100 dev br0`).
    ("ns_ip_neigh", "ip neigh"),
    ("ns_ip_route", "ip route"),
    ("ns_bridge_link", "bridge link"),
    ("ns_listening_sockets", "ss -ltn"),
)

PROBE_LOG_MARKERS = (
    # fc-agent's restore narration, straight off the guest serial console:
    # "detected restore-epoch", "handling restore", "ARP cache flushed",
    # "exec re-registered after restore", "restore complete", and the
    # "restore metadata fetch failed" line that fires when MMDS is unreachable.
    "[fc-agent]",
    "restore-epoch",
    # pasta's readiness verdict, including the `ping_replied=` field that
    # separated the 5 no-reply clones from the other 803.
    "ping_replied",
    "ARP",
    "neighbour",
    "port forward",
    "pasta",
)

PROBE_LOG_MAX_LINES = 200


def probe_batch_script(sections) -> str:
    """One shell script that runs every section and frames its output and status.

    Batched deliberately: one `fcvm exec` per batch instead of one per command
    means one handshake, one timeout to enforce, and one instant that all the
    state comes from. Each section still carries its OWN exit status, so a
    missing binary inside the batch is recorded as that section's `rc` rather
    than discarding the batch.

    Each section runs in a SUBSHELL, not a brace group. A brace group shares the
    batch's shell, so one section calling `exit` ends the whole batch and every
    later section vanishes with no status line, and silently, since the frame
    that would have said so was never printed. The subshell also keeps a section's
    `cd`, variables and traps out of the next one.
    """
    parts = []
    for name, command in sections:
        quoted = shlex.quote(name)
        parts.append(
            f"printf '{PROBE_SECTION_MARK} %s\\n' {quoted}; "
            f"( {command} ) 2>&1; "
            f"printf '{PROBE_RC_MARK} %s %s\\n' {quoted} \"$?\""
        )
    return "\n".join(parts)


def parse_probe_batch(text: str) -> dict:
    """Split framed batch output into {section: {"output", "rc"}}.

    A section whose RC line never arrived keeps `rc: None`. That is what a batch
    cut off by the timeout looks like, and it must not read as rc 0.
    """
    sections: dict = {}
    current = None
    buffered: list = []

    def close(name, rc):
        body = "\n".join(buffered)
        if len(body) > PROBE_SECTION_LIMIT:
            body = body[:PROBE_SECTION_LIMIT] + "\n...[truncated]"
            sections[name]["truncated"] = True
        sections[name]["output"] = body
        sections[name]["rc"] = rc

    for line in text.splitlines():
        if line.startswith(PROBE_SECTION_MARK + " "):
            if current is not None:
                close(current, None)
            current = line[len(PROBE_SECTION_MARK) + 1:].strip()
            sections[current] = {"output": "", "rc": None}
            buffered = []
        elif current is not None and line.startswith(PROBE_RC_MARK + " "):
            rest = line[len(PROBE_RC_MARK) + 1:].strip()
            name, _, status = rest.rpartition(" ")
            if name == current:
                try:
                    close(current, int(status))
                except ValueError:
                    close(current, None)
                current = None
                buffered = []
            else:
                buffered.append(line)
        elif current is not None:
            buffered.append(line)
    if current is not None:
        close(current, None)
    return sections


def clip(text: str, limit: int) -> str:
    if len(text) <= limit:
        return text
    return text[:limit] + "\n...[truncated]"


def live_group_members(pgid: int) -> list:
    """Pids in `pgid` that are not zombies.

    A zombie is dead, so it is not a survivor, but it does keep the group
    present, which is why `killpg(pgid, 0)` cannot answer this question on a
    host whose PID 1 does not reap. Reading each candidate's state answers it on
    every host. One /proc walk, only ever on the timeout path.
    """
    live = []
    try:
        entries = os.listdir("/proc")
    except OSError:
        return live
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as handle:
                raw = handle.read()
        except OSError:
            continue  # exited between listdir and open
        try:
            fields = raw.rsplit(") ", 1)[1].split()
            state, group = fields[0], int(fields[2])
        except (IndexError, ValueError):
            continue
        if group == pgid and state != "Z":
            live.append(int(entry))
    return live


def kill_process_group(leader_pid: int, timeout_s: float = 2.0) -> dict:
    """SIGKILL a group by its LEADER's pid, then verify no live member is left.

    `start_new_session=True` makes the spawned child both session and process
    group leader, so the group id IS that pid. That identifier stays valid for
    as long as the group has any member, because a member's `pgid` reference
    pins the number against reuse. Reading it back with
    `os.getpgid(leader_pid)` instead asks the LEADER, which is precisely the
    process that may already be gone: a wrapper that spawns its real work and
    exits leaves `communicate()` blocked on the still-open pipe, so by the time
    the timeout fires the leader can be reaped and `getpgid` raises ESRCH.
    Falling back to `proc.kill()` there re-targets that same dead leader and
    every descendant keeps running.
    """
    outcome: dict = {"pgid": leader_pid}
    try:
        os.killpg(leader_pid, signal.SIGKILL)
        outcome["signalled"] = True
    except ProcessLookupError:
        # No member left to signal: the group is already gone.
        outcome["signalled"] = False
        outcome["survivors"] = []
        return outcome
    except OSError as error:
        outcome["signalled"] = False
        outcome["error"] = f"{type(error).__name__}: {error}"
    deadline = time.monotonic() + timeout_s
    while True:
        survivors = live_group_members(leader_pid)
        if not survivors or time.monotonic() >= deadline:
            outcome["survivors"] = survivors
            return outcome
        time.sleep(0.01)


def run_probe_command(argv, timeout_s: float, budget_limited: bool = False,
                      output_limit: int = PROBE_BATCH_LIMIT) -> dict:
    """Run one probe command in its own session; never raise, always record.

    `start_new_session` plus a process-GROUP kill is the whole reason this is
    not `subprocess.run(timeout=...)`: that kills only the direct child, and the
    direct child here is `fcvm exec`, which is holding a vsock session with a
    command running inside the guest.

    `budget_limited` says this command got less than the full per-command
    timeout because the capture's own budget was nearly spent. Without it a
    `timed_out: True` from a 40 ms residual budget is indistinguishable from a
    genuinely wedged guest, which is the whole thing the dump exists to tell
    apart.
    """
    started = time.monotonic()
    record: dict = {"argv": list(argv), "timeout_s": timeout_s,
                    "budget_limited": budget_limited}
    try:
        proc = subprocess.Popen(
            list(argv),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
            text=True,
            errors="replace",
        )
    except OSError as error:
        record["rc"] = None
        record["timed_out"] = False
        record["error"] = f"{type(error).__name__}: {error}"
        record["elapsed_ms"] = (time.monotonic() - started) * 1000
        return record
    timed_out = False
    try:
        out, err = proc.communicate(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        timed_out = True
        record["group_kill"] = kill_process_group(proc.pid)
        try:
            out, err = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            out, err = "", ""
    record["rc"] = proc.returncode
    record["timed_out"] = timed_out
    record["stdout"] = clip(out or "", output_limit)
    record["stderr"] = clip(err or "", PROBE_STREAM_LIMIT)
    record["elapsed_ms"] = (time.monotonic() - started) * 1000
    return record


def probe_log_markers(log_path: str, markers=PROBE_LOG_MARKERS,
                      max_lines: int = PROBE_LOG_MAX_LINES) -> dict:
    """Pull the restore/readiness narration out of THIS clone's own fcvm log.

    fc-agent's restore lines arrive on the guest serial console and pasta's
    readiness verdict is fcvm's own `info!`, so both are already in the per-clone
    log, but a dump that says "go read a 20 000 line log" is not
    self-describing. Keeps the head and the tail of the matches, since the
    restore evidence is at the start and the failure at the end.
    """
    out: dict = {"path": log_path, "matched": 0, "lines": [], "truncated": False}
    if not log_path:
        out["error"] = "no log path for this request"
        return out
    head: list = []
    tail: list = []
    half = max(1, max_lines // 2)
    try:
        with open(log_path, "r", errors="replace") as handle:
            for line in handle:
                if not any(marker in line for marker in markers):
                    continue
                out["matched"] += 1
                line = line.rstrip("\n")
                if len(head) < half:
                    head.append(line)
                else:
                    tail.append(line)
                    if len(tail) > half:
                        tail.pop(0)
    except OSError as error:
        out["error"] = f"{type(error).__name__}: {error}"
        return out
    omitted = out["matched"] - len(head) - len(tail)
    if omitted > 0:
        out["truncated"] = True
        out["lines"] = head + [f"...[{omitted} lines omitted]"] + tail
    else:
        out["lines"] = head + tail
    return out


def read_probe_file(path: str, limit: int = PROBE_SECTION_LIMIT) -> dict:
    try:
        with open(path, "r", errors="replace") as handle:
            return {"path": path, "content": clip(handle.read(limit + 1), limit)}
    except OSError as error:
        return {"path": path, "error": f"{type(error).__name__}: {error}"}


def probe_process_facts(pid) -> dict:
    """pid, comm, scheduler state and argv for one host process, or why not."""
    facts: dict = {"pid": pid}
    if not isinstance(pid, int) or pid <= 0:
        facts["error"] = f"not a usable pid: {pid!r}"
        return facts
    fields = proc_stat_fields(pid)
    if fields is None:
        facts["alive"] = False
        return facts
    facts["alive"] = True
    facts["state"] = fields[0]
    facts["utime_ms"] = fields[1] * 1000.0 / CLK_TCK
    facts["stime_ms"] = fields[2] * 1000.0 / CLK_TCK
    facts["start_time"] = fields[3]
    facts["comm"] = proc_comm(pid)
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            facts["cmdline"] = handle.read(4096).replace(b"\0", b" ").decode(
                "utf8", "replace").strip()
    except OSError as error:
        facts["cmdline_error"] = f"{type(error).__name__}: {error}"
    return facts


def probe_tcp_connect(endpoint: str, timeout_s: float = 2.0) -> dict:
    """Does the host->guest published port answer RIGHT NOW?

    The failure is defined by this path not working; asking it again at probe
    time is what separates "still broken 100 s later" from "recovered and the
    client had already given up".
    """
    import socket

    out: dict = {"endpoint": endpoint, "timeout_s": timeout_s}
    if not endpoint or ":" not in endpoint:
        out["error"] = f"no usable endpoint: {endpoint!r}"
        return out
    host, _, port = endpoint.rpartition(":")
    started = time.monotonic()
    try:
        socket.create_connection((host, int(port)), timeout_s).close()
        out["connected"] = True
    except (OSError, ValueError) as error:
        out["connected"] = False
        out["error"] = f"{type(error).__name__}: {error}"
    out["elapsed_ms"] = (time.monotonic() - started) * 1000
    return out


class FailureProbe:
    """Capture guest-side and host-side state for a clone that just failed.

    One instance per run. It owns the once-per-run control capture, so the
    control cannot be taken twice or taken from a clone that also failed.
    """

    # A control that failed to WRITE is not a control, so the next healthy clone
    # gets another go. Bounded, because each attempt costs up to the full budget
    # and perturbs the rep it lands on, so an unbounded retry against a
    # systematically broken probe would tax every healthy request in the run.
    CONTROL_ATTEMPTS = 3

    def __init__(self, fcvm: str, data_root: str, out_dir: str, run_id: str,
                 cdp_port: int, command_timeout_s: float = PROBE_COMMAND_TIMEOUT_S,
                 budget_s: float = PROBE_BUDGET_S):
        self.fcvm = fcvm
        self.data_root = data_root
        self.out_dir = out_dir
        self.run_id = run_id
        self.cdp_port = cdp_port
        self.command_timeout_s = command_timeout_s
        self.budget_s = budget_s
        self.control_captured = False
        self.control_attempts = 0
        self.control_path = ""
        self.is_warmup = False

    def begin_request(self, is_warmup: bool) -> None:
        """Tell the probe whether the rep about to run is a discarded warmup.

        The control has to come from a HEALTHY clone, so it is the one capture
        that touches a request the analyzer might read. Placing it on a warmup
        rep makes that free: warmups are discarded explicitly at analysis. When
        no warmup rep is available the control still runs, because a failure dump
        with nothing to compare against proves little, and the record it perturbs is
        stamped `probe_perturbed_timings` so it is excludable by hand.
        """
        self.is_warmup = bool(is_warmup)

    def role_for(self, rec: dict):
        if rec.get("ok") is False:
            return "failure"
        if (rec.get("ok") is True and not self.control_captured
                and self.control_attempts < self.CONTROL_ATTEMPTS):
            return "control"
        return None

    def observe(self, rec: dict, *, name: str, fcvm_pid: int, state_path,
                log_path: str, endpoint: str = "") -> None:
        """Capture if this record earns it, and stamp the record either way.

        Wrapped whole: a probe that raises would abort a request that had
        already produced its answer, which is strictly worse than no probe.

        A pending INT/TERM outranks the evidence. The signal handler only
        RECORDS the signal, so nothing here is interrupted asynchronously and a
        capture that began before the signal would run its full budget with the
        clone still up and the harness's teardown behind it, long enough for a
        job runner to escalate to SIGKILL and leave behind exactly the clone this
        probe exists to diagnose. So a signal that is already pending skips the
        capture outright, and one that arrives mid-capture stops it at the next
        step (`capture`'s `interrupted`).
        """
        role = self.role_for(rec)
        if role is None:
            return
        pending = harness_interrupt_pending()
        if pending:
            rec["probe"] = {
                "role": role,
                "path": "",
                "skipped": f"termination signal {pending} pending",
            }
            return
        if role == "control":
            self.control_attempts += 1
        try:
            dump = self.capture(
                role=role, rec=rec, name=name, fcvm_pid=fcvm_pid,
                state_path=state_path, log_path=log_path, endpoint=endpoint,
            )
            path = self.write(name, dump)
            errors = dump.get("errors", [])
            rec["probe"] = {
                "role": role,
                "path": path,
                # A dump with nothing to compare against proves little, so the
                # failing record names the run's healthy control too. Empty when
                # the failure preceded any healthy CDP rep, which is itself worth
                # knowing rather than looking like an absent file.
                "control_path": dump.get("control_path", ""),
                "elapsed_ms": dump.get("elapsed_ms"),
                "budget_exhausted": dump.get("budget_exhausted", False),
                "interrupted_by_signal": dump.get("interrupted_by_signal", 0),
                "errors": errors[:10],
                "error_count": len(errors),
            }
            if role == "control":
                # A WRITTEN dump is what makes this the control. Setting the
                # flag on the exception path below instead retired the control
                # for the whole run over one transient error, and every later
                # failure dump then had nothing to be read against.
                self.control_path = path
                self.control_captured = True
        except Exception as error:  # noqa: BLE001 - the probe is never fatal
            rec["probe"] = {
                "role": role,
                "path": "",
                "probe_error": f"{type(error).__name__}: {error}",
            }
        if role == "control" and not self.is_warmup:
            rec["probe_perturbed_timings"] = True
            print(
                f"probe: control capture attempt {self.control_attempts} on "
                f"MEASURED rep {rec.get('rep')} ({name}); its wall_ms and "
                "teardown are perturbed and the record is stamped "
                "probe_perturbed_timings",
                file=sys.stderr, flush=True,
            )

    def write(self, name: str, dump: dict) -> str:
        """Write the dump atomically, next to this request's clone log."""
        path = os.path.join(self.out_dir, f"{name}.probe.json")
        tmp = f"{path}.tmp"
        with open(tmp, "w") as handle:
            json.dump(dump, handle, indent=2, sort_keys=True)
        os.replace(tmp, path)
        return path

    def capture(self, *, role, rec, name, fcvm_pid, state_path, log_path,
                endpoint) -> dict:
        started = time.monotonic()
        deadline = started + self.budget_s
        dump: dict = {
            "schema": "fcvm-cdp-failure-probe-v1",
            "role": role,
            "run_id": self.run_id,
            "name": name,
            "arm": rec.get("arm"),
            "rep": rec.get("rep"),
            "vm_id": rec.get("vm_id"),
            "endpoint": endpoint or rec.get("endpoint", ""),
            "request_error": rec.get("error"),
            "failure_class": rec.get("failure_class"),
            "failure_stage": rec.get("failure_stage"),
            "blocking_ms": rec.get("blocking_ms"),
            "fcvm_pid": fcvm_pid,
            "state_path": state_path or "",
            "log_path": log_path,
            "captured_at": time.time(),
            "command_timeout_s": self.command_timeout_s,
            "budget_s": self.budget_s,
            "control_path": "" if role == "control" else self.control_path,
            "host": {},
            "guest": {},
            "errors": [],
        }

        def note(where, detail):
            dump["errors"].append(f"{where}: {detail}")

        def remaining():
            return deadline - time.monotonic()

        def interrupted(step):
            """Has a termination signal arrived since the capture began?

            Every step is optional evidence and the teardown waiting behind them
            is not, so the first step to notice ends the capture. Recorded on the
            dump, so a short dump reads as "we left early" rather than as a guest
            that answered nothing.
            """
            pending = harness_interrupt_pending()
            if pending:
                dump["interrupted_by_signal"] = pending
                note(step, f"skipped, termination signal {pending} pending")
            return bool(pending)

        def budget_for(step):
            """Timeout for one step, or None when the step must not run.

            Returns `(timeout, budget_limited)` so a step that got a shortened
            timeout can say so on its own record instead of reporting an
            indistinguishable `timed_out`.
            """
            if interrupted(step):
                return None
            left = remaining()
            if left <= 0:
                dump["budget_exhausted"] = True
                note(step, "skipped, probe budget exhausted")
                return None
            return (min(self.command_timeout_s, left), left < self.command_timeout_s)

        # ---- host, instantaneous: no subprocess, nothing that can hang.
        state = None
        if state_path:
            try:
                with open(state_path) as handle:
                    state = json.load(handle)
            except (OSError, ValueError) as error:
                note("clone_state", f"{type(error).__name__}: {error}")
        else:
            note("clone_state", "no state file was ever found for this clone")
        dump["host"]["clone_state"] = state
        holder_pid = (state or {}).get("holder_pid")
        vm_id = (state or {}).get("vm_id") or rec.get("vm_id") or ""
        dump["host"]["holder_pid"] = holder_pid
        dump["host"]["fcvm_process"] = probe_process_facts(fcvm_pid)
        dump["host"]["fcvm_children"] = [
            probe_process_facts(child) for child in children_of(fcvm_pid)
        ]
        try:
            dump["host"]["loadavg"] = read_trimmed("/proc/loadavg")
        except OSError as error:
            note("loadavg", f"{type(error).__name__}: {error}")

        # pasta identifies itself by a per-VM pid file (`src/network/pasta.rs`
        # writes `pasta-<vm_id[:8]>.pid` under the data dir), which is the only
        # way to name THIS clone's pasta rather than one of the others'.
        pasta: dict = {}
        if valid_vm_id(vm_id):
            pid_path = os.path.join(self.data_root, f"pasta-{vm_id[:8]}.pid")
            pasta["pid_file"] = pid_path
            try:
                with open(pid_path) as handle:
                    pasta["pid"] = int(handle.read().strip())
            except (OSError, ValueError) as error:
                pasta["error"] = f"{type(error).__name__}: {error}"
        else:
            pasta["error"] = f"no usable vm_id to locate the pasta pid file: {vm_id!r}"
        if isinstance(pasta.get("pid"), int):
            pasta["process"] = probe_process_facts(pasta["pid"])
        dump["host"]["pasta"] = pasta

        # The holder's own procfs exposes its network namespace with no
        # privilege and no nsenter, so this half survives even when entering the
        # namespace does not.
        if isinstance(holder_pid, int):
            dump["host"]["holder_procfs"] = {
                which: read_probe_file(f"/proc/{holder_pid}/net/{which}")
                for which in ("arp", "dev", "route", "tcp")
            }
        else:
            note("holder_procfs", f"no holder pid in state: {holder_pid!r}")

        if not interrupted("cdp_connect_now"):
            dump["host"]["cdp_connect_now"] = probe_tcp_connect(
                dump["endpoint"], min(2.0, max(0.1, remaining()))
            )

        # ---- guest, passive. The reason this probe exists: vsock exec still
        # works while the IP path does not, so the broken guest can be asked.
        allowance = budget_for("guest_passive")
        if allowance is not None:
            dump["guest"]["passive"] = self.exec_batch(
                fcvm_pid, PROBE_GUEST_PASSIVE_SECTIONS, *allowance
            )

        # ---- host, inside the clone's network namespace. `-U -n` is exactly
        # what fcvm itself uses (`PastaNetwork::build_nsenter_prefix`), so it
        # needs no privilege in rootless mode; the retry without `-U` covers the
        # modes that have no user namespace. Both attempts are recorded.
        if isinstance(holder_pid, int) and budget_for("holder_namespace") is not None:
            dump["host"]["holder_namespace"] = self.nsenter_batch(
                holder_pid, PROBE_HOLDER_NS_SECTIONS, deadline
            )
        loopback = ((state or {}).get("config", {}).get("network", {}) or {}).get(
            "loopback_ip"
        )
        if loopback:
            allowance = budget_for("host_listeners")
            if allowance is not None:
                # Does pasta's host-side listener for this clone still exist?
                # That is the socket `wait_for_port_forwarding_until` connected
                # to before the clone was declared ready.
                dump["host"]["listeners"] = run_probe_command(
                    ["ss", "-H", "-ltn", "src", loopback], *allowance
                )

        # ---- guest, ACTIVE and last. These three mutate state: arping refills
        # the neighbour table this dump has already recorded. Passive capture
        # runs first for exactly that reason. Their value is that they answer
        # "is this still broken NOW", which the unsolved question is about.
        allowance = budget_for("guest_active")
        if allowance is not None:
            dump["guest"]["active_mutating"] = self.exec_batch(
                fcvm_pid, self.guest_active_sections(), *allowance
            )
            dump["guest"]["active_mutating"]["mutates_guest_state"] = True

        # Last, so it includes anything the probe itself provoked.
        dump["host"]["log_markers"] = probe_log_markers(log_path)
        dump["elapsed_ms"] = (time.monotonic() - started) * 1000
        dump.setdefault("budget_exhausted", False)
        dump.setdefault("interrupted_by_signal", 0)
        return dump

    def guest_active_sections(self):
        port = self.cdp_port
        return (
            # Chromium answering on guest loopback while the host cannot reach
            # it puts the fault in the network path, not in the browser.
            ("cdp_from_guest_loopback",
             f"printf 'GET /json/version HTTP/1.0\\r\\n\\r\\n' "
             f"| nc -w 2 127.0.0.1 {port}"),
            # What restore-epoch does the guest see RIGHT NOW, and can it see
            # MMDS at all? `fetch_latest_metadata` is MMDS V2, so the token PUT
            # comes first. netcat-openbsd is installed in the VM rootfs
            # (`rootfs-config.toml` [packages] debug); curl is not.
            ("mmds_latest",
             "tok=$(printf 'PUT /latest/api/token HTTP/1.0\\r\\n"
             "X-metadata-token-ttl-seconds: 60\\r\\nConnection: close\\r\\n\\r\\n' "
             "| nc -w 2 169.254.169.254 80 | tr -d '\\r' | tail -1); "
             "printf 'GET /latest HTTP/1.0\\r\\nX-metadata-token: %s\\r\\n"
             "Accept: application/json\\r\\nConnection: close\\r\\n\\r\\n' \"$tok\" "
             "| nc -w 2 169.254.169.254 80 | tr -d '\\r' | tail -5"),
            # The reply-verified form of the probe `handle_clone_restore`
            # broadcasts natively (fc-agent sends the ARP request and does not
            # wait; this probe does). iputils-arping is in the VM rootfs. A
            # gateway that does not answer this, 100 s after the request gave
            # up, is a persistent L2 break rather than a race.
            ("arping_gateway",
             "gw=$(ip route show default | awk '/via/{print $3; exit}'); "
             "echo \"gateway=$gw\"; arping -c 1 -w 2 -I eth0 \"$gw\""),
        )

    def exec_batch(self, fcvm_pid: int, sections, timeout_s: float,
                   budget_limited: bool = False) -> dict:
        """Run a section batch in the guest VM over vsock exec.

        `--vm`, not the container: the container runs `--network=host`, so the
        VM's view already covers the container's sockets and processes, and one
        exec is one thing that can hang instead of two.
        """
        script = probe_batch_script(sections)
        # fcvm logs to stderr and keeps stdout clean for command output
        # (`src/main.rs`: "Logs to stderr, keep stdout clean"), so the framed
        # sections parse out of stdout while the exec client's own retry ladder
        # and connect errors stay readable in `stderr`.
        argv = [self.fcvm, "exec", "--pid", str(fcvm_pid), "--vm",
                "--", "sh", "-c", script]
        result = run_probe_command(argv, timeout_s, budget_limited)
        result["sections"] = parse_probe_batch(result.get("stdout", ""))
        result.pop("stdout", None)
        return result

    def nsenter_batch(self, holder_pid: int, sections, deadline: float) -> dict:
        """Enter the clone's network namespace, preferring the way fcvm does it.

        `-U -n --preserve-credentials` is `PastaNetwork::build_nsenter_prefix`
        verbatim, and it needs no privilege in rootless mode because the holder's
        user namespace makes the caller root inside it. Dropping `-U` is the
        retry for a mode with no user namespace. Each attempt is separately
        bounded against the capture deadline, so a hanging first attempt cannot
        buy the second one a fresh budget.
        """
        script = probe_batch_script(sections)
        variants = (
            ("user+net", ["-t", str(holder_pid), "-U", "-n", "--preserve-credentials"]),
            ("net", ["-t", str(holder_pid), "-n", "--preserve-credentials"]),
        )
        attempts = []
        resolved = None
        for variant, flags in variants:
            pending = harness_interrupt_pending()
            if pending:
                attempts.append({
                    "nsenter_variant": variant,
                    "error": f"skipped, termination signal {pending} pending",
                })
                continue
            timeout = min(self.command_timeout_s, deadline - time.monotonic())
            if timeout <= 0:
                attempts.append({"nsenter_variant": variant,
                                 "error": "skipped, probe budget exhausted"})
                continue
            result = run_probe_command(
                ["nsenter", *flags, "--", "sh", "-c", script], timeout,
                budget_limited=timeout < self.command_timeout_s,
            )
            result["nsenter_variant"] = variant
            result["sections"] = parse_probe_batch(result.get("stdout", ""))
            result.pop("stdout", None)
            attempts.append(result)
            if result.get("rc") == 0 and result["sections"]:
                resolved = variant
                break
        return {"attempts": attempts, "resolved_variant": resolved}


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


def spawn_clone_process(cmd: list[str], log: str, env: dict) -> subprocess.Popen:
    """Spawn fcvm with INT/TERM deliverable in the child.

    A signal mask is inherited across both fork and exec.  The harness signal
    handler only records a pending interruption, so it is safe to unblock these
    signals around Popen: ownership is published before any recorded signal is
    raised at a request boundary.  Restore the calling thread's exact mask for
    direct unit-test callers; main itself runs with both signals unblocked.
    """
    previous_mask = signal.pthread_sigmask(signal.SIG_UNBLOCK, TERMINATION_SIGNALS)
    try:
        with open(log, "wb") as lf:
            # stdout/stderr to a FILE, never a pipe we do not drain: an undrained
            # 64 KB pipe blocks fcvm's writer and stalls everything behind it
            # (AGENTS.md "Pipe Buffer Deadlock in Tests").
            return subprocess.Popen(
                cmd,
                stdout=lf,
                stderr=lf,
                stdin=subprocess.DEVNULL,
                env=env,
            )
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)


def discover_wd_session(args, fcvm_pid: int, deadline: float) -> str:
    """Read the golden's warm WebDriver session id out of a live clone, once.

    entry-webkit.sh writes it to /run/bench-session-id in the CONTAINER before
    the warm marker, so it precedes every golden snapshot and restores
    identically into every clone. One `fcvm exec` (vsock, no network stack in
    the path) on the first clone; the caller pins the result for the run.
    Raises rather than returning empty: a run without a session id cannot
    render anything, and a silent "" would surface 202 reps later as uniform
    navigate failures with the real cause off-screen.
    """
    argv = [args.fcvm, "exec", "--pid", str(fcvm_pid), "-c",
            "--", "cat", "/run/bench-session-id"]
    # Bounded by the CLONE's remaining deadline, and every failure mode is
    # SessionDiscoveryFailed: a timeout or an unlaunchable fcvm left as its
    # own type is an ordinary Exception the per-rep handler records and
    # RETRIES, so each later webkit rep would repeat the whole discovery wait
    # against a value that is snapshot state and will not change.
    timeout = max(1.0, min(60.0, deadline - time.monotonic()))
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except (subprocess.TimeoutExpired, OSError) as error:
        raise SessionDiscoveryFailed(
            f"webkit session discovery failed: {type(error).__name__}: {error}"
        ) from error
    session = (proc.stdout or "").strip()
    if proc.returncode != 0 or not session:
        raise SessionDiscoveryFailed(
            "webkit session discovery failed: "
            f"rc={proc.returncode} stdout={proc.stdout!r} stderr={proc.stderr[-300:]!r}"
        )
    return session


def run_cdp_request(args, rep: int, fast: bool, probe=None, op: str = "screenshot") -> dict:
    import cdpdrive

    arm_name = "html" if op == "html" else ("cdp-fast" if fast else "cdp")
    # The clone name carries the arm: cdp and html arms share rep indices, and a
    # shared name would collide two live clones.
    name = f"rb-{args.run_id}-{rep}-{'html' if op == 'html' else ('fast' if fast else 'norm')}"
    log = os.path.join(args.out_dir, f"{name}.log")
    rec: dict = {"arm": arm_name, "rep": rep, "name": name}

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
        proc = spawn_clone_process(cmd, log, env)
        fcvm_pid = proc.pid

        state_path = data_dir = None
        try:
            fcvm_start_time = spawned_process_start_time(proc)
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

            if getattr(args, "engine", "chromium") == "webkit":
                # Classic WebDriver has NO session discovery: the warm session
                # id is written by entry-webkit.sh to /run/bench-session-id
                # BEFORE the golden snapshot, so it is snapshot state --
                # identical across clones for exactly the reason cdpdrive's
                # target id is. Captured ONCE via fcvm exec on the first clone
                # and pinned for every later rep (discovery-once, same pattern
                # as --prewire; warmups run first, so the exec cost lands on a
                # discarded rep).
                if not getattr(args, "wd_session_id", ""):
                    args.wd_session_id = discover_wd_session(args, fcvm_pid, deadline)
                rec["session_prewired"] = True
                import wddrive

                result = wddrive.drive(argparse.Namespace(
                    cdp_host=endpoint,
                    url=url_for_rep(getattr(args, "urls", None) or [args.url], rep),
                    timeout=max(1.0, deadline - time.monotonic()),
                    session_id=args.wd_session_id,
                    out_prefix="",
                ))
            else:
                ws_url = clone_ws_url(args.ws_url, endpoint) if args.ws_url else ""
                rec["ws_url_prewired"] = bool(ws_url)
                drive_args = argparse.Namespace(
                    cdp_host=endpoint,
                    url=url_for_rep(getattr(args, "urls", None) or [args.url], rep),
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
                    op=op,
                    render_module=os.path.join(HERE, "render.py"),
                )
                result = cdpdrive.drive(drive_args)
            raise_if_harness_interrupted()
            rec["render"] = result
            rec["ok"] = bool(result.get("ok"))
            # getattr, not args.prewire: failure-path fixtures drive this
            # function with bare Namespaces, and the attribute lookup runs
            # before the rec["ok"] guard — an AttributeError here replaced
            # every failure label with its own traceback.
            if (
                getattr(args, "prewire", False)
                and not args.ws_url
                and rec["ok"]
                and result.get("target_id")
            ):
                # Discovery-once: pin the page target's WS URL for every later
                # rep. clone_ws_url() re-hosts it onto each clone's endpoint, so
                # only the path — the guest-side target id, identical across
                # clones because it is snapshot state — is load-bearing. This
                # lands on a warmup rep by schedule construction (warmups run
                # first); the analyzer holds measured reps to meta.ws_url_prewired.
                args.ws_url = f"ws://{endpoint}/devtools/page/{result['target_id']}"
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
            if rep_error_escalates(e):
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
        watch.close()

    # EVIDENCE, NOT MEASUREMENT. `rec["blocking_ms"]` is the caller-visible
    # latency and the only number this arm publishes as a response time, and it
    # is already stamped on both the success and the failure path above, so nothing
    # below can move it. The clone is still up, and on the failure path its vsock
    # exec still works while its IP path does not, which is the whole reason to
    # ask it anything before the teardown deletes it.
    if probe is not None:
        probe.observe(
            rec,
            name=name,
            fcvm_pid=fcvm_pid,
            state_path=state_path,
            log_path=log,
            endpoint=rec.get("endpoint", ""),
        )

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
        proc = spawn_clone_process(cmd, log, env)
        fcvm_pid = proc.pid
        try:
            fcvm_start_time = spawned_process_start_time(proc)
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
            if rep_error_escalates(e):
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
        "python3", "/opt/bench/render.py",
        url_for_rep(getattr(args, "urls", None) or [args.url], rep),
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
        proc = spawn_clone_process(cmd, log, env)
        fcvm_start_time = spawned_process_start_time(proc)
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
        if rep_error_escalates(request_error):
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


def dispatch_request(args, rep: int, arm: str, is_warmup: bool, probe=None) -> dict:
    """Route one scheduled attempt to its arm, carrying the failure probe.

    Extracted from `main`'s loop so the wiring is testable: the probe reaching
    the CDP arms is the difference between a failure that leaves evidence and one
    that does not, and a wiring gap inside a 90-line loop body is invisible until
    the next unexplained failure has already been torn down.
    """
    if probe is not None:
        probe.begin_request(is_warmup)
    if arm == "exec":
        return run_exec_request(args, rep)
    if arm == "noop":
        return run_noop_request(args, rep)
    if arm == "html":
        return run_cdp_request(args, rep, fast=False, probe=probe, op="html")
    return run_cdp_request(args, rep, fast=(arm == "cdp-fast"), probe=probe)



# Arms that drive Chromium over CDP and therefore pay the per-request
# handshake. html belongs here: it navigates and extracts the DOM over the
# same connection, differing from cdp only in the terminal stage. Keeping this
# set in one place is the point — it was spelled inline as {"cdp","cdp-fast"}
# while the analyzer already classified html as CDP-class, so a legitimate
# `--arms noop,html` publication run was refused with "requires at least one
# CDP arm" (observed 2026-08-15, corpus campaign).
CDP_CLASS_ARMS = frozenset({"cdp", "cdp-fast", "html"})


def allowed_arms_for_engine(engine: str) -> frozenset:
    """The arms an engine can actually execute; refused upfront, not per-rep.

    webkit excludes exec (its in-guest driver is render.py, which
    Containerfile.webkit-bench does not ship, so every exec rep burns a full
    clone lifecycle on a guaranteed in-guest failure) and html (wddrive has no
    DOM-extraction op; the request would silently return screenshot records the
    analyzer then rejects for missing extract_ms/html_bytes/html_sha256)."""
    if engine == "webkit":
        return frozenset({"cdp", "cdp-fast", "noop"})
    return frozenset({"exec", "cdp", "cdp-fast", "html", "noop"})


def publication_arms_ok(arms) -> bool:
    """Whether an arm set can back a publication run.

    noop is the drift canary every published cell is judged against, and at
    least one CDP-class arm has to actually render something.
    """
    arms = set(arms)
    return "noop" in arms and bool(CDP_CLASS_ARMS & arms)


def parse_urls(spec):
    """Split a comma-separated --url value; single URLs pass through as [url]."""
    return [u.strip() for u in spec.split(",") if u.strip()]


def url_for_rep(urls, rep):
    """Deterministic uniform cycle: rep r renders urls[r % len(urls)]."""
    return urls[rep % len(urls)]


def main() -> int:
    """Run one schedule and release every whole-run resource on every exit."""
    with ExitStack() as resources:
        return main_with_resources(resources)


def main_with_resources(resources: ExitStack) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--serve-pid", type=int, default=0,
                   help="UFFD serve pid (omit when using --snapshot-tag)")
    p.add_argument("--snapshot-tag", default="",
                   help="FILE-backed restore from this tag instead of a UFFD serve")
    p.add_argument("--url", required=True,
                   help="page URL, or a comma-separated list cycled across "
                        "reps (the corpus-mix arm)")
    p.add_argument("--arms", default="exec,cdp,cdp-fast,noop")
    p.add_argument("--reps", type=int, default=10)
    p.add_argument("--warmup", type=int, default=2, help="discarded EXPLICITLY, and reported")
    p.add_argument("--prewire", action="store_true",
                   help="discover the CDP page target once (on the first successful cdp "
                        "rep, a warmup by schedule construction) and pin its re-hosted "
                        "WebSocket URL for every later rep, skipping per-request "
                        "/json/list discovery; the target id is guest-side snapshot "
                        "state, identical across clones like a WebDriver session id")
    p.add_argument("--seed", type=int, default=20260808)
    p.add_argument("--format", choices=("png", "jpeg"), default="jpeg")
    p.add_argument("--quality", type=int, default=80)
    p.add_argument("--cdp-port", type=int, default=9222)
    p.add_argument("--engine", choices=("chromium", "webkit"), default="chromium",
                   help="render driver: chromium drives CDP via cdpdrive; webkit "
                        "drives W3C WebDriver classic via wddrive (the port in "
                        "--cdp-port is then the WD port, 9515)")
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
    # The harness owns these handlers.  Do not inherit a caller's blocked mask:
    # every clone and the harness itself must be able to observe shutdown.
    signal.pthread_sigmask(signal.SIG_UNBLOCK, TERMINATION_SIGNALS)
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
        snapshot_lock = resources.enter_context(open(snapshot_lock_path, "a+"))
        fcntl.flock(snapshot_lock, fcntl.LOCK_SH)
        snapshot = snapshot_generation(args.data_root, snapshot_name)
    except RuntimeError as error:
        p.error(str(error))
    except OSError as error:
        p.error(f"cannot hold snapshot generation lock {snapshot_lock_path}: {error}")
    # Keep snapshot_lock referenced until main returns. Creators/deleters need
    # this lock exclusively, so every request below consumes the same installed
    # generation whose UUID and exact config digest were just recorded.

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
    allowed_arms = allowed_arms_for_engine(args.engine)
    if not arms or len(set(arms)) != len(arms) or any(a not in allowed_arms for a in arms):
        p.error(
            "--arms must be a non-empty, duplicate-free subset of "
            + ",".join(sorted(allowed_arms))
            + f" for --engine {args.engine}"
        )
    if args.engine == "webkit":
        # WebDriver's screenshot is always PNG (wddrive stamps format=png), so
        # the jpeg default in --format would put a declaration in meta the
        # renders can never satisfy.
        args.format = "png"
        if args.ws_url:
            p.error("--ws-url is CDP WebSocket prewiring; --engine webkit "
                    "drives WebDriver classic and cannot use it")
        # --prewire likewise names CDP prewiring. Leaving it set would stamp
        # ws_url_prewired=true in meta for a WebSocket that never exists; the
        # webkit analogue (the inherited WebDriver session) is recorded per
        # rep as session_prewired.
        args.prewire = False
    # exec is ALLOWED but no longer REQUIRED: it is retired from measurement
    # (no published claim rests on it), and run reqbench-20260814-022254-uffd
    # measured that 95% of noop reps following an exec rep land in a +17 ms
    # slow mode (59/62, vs 15% after cdp-fast) — the in-guest Python driver
    # faults a large run-varying page set that pollutes the shared prefetch
    # working set and destabilizes the noop drift canary. Requiring it forced
    # the arm that corrupts the baseline into every publication run.
    if not publication_arms_ok(arms):
        p.error(
            "publication runs require noop and at least one CDP arm "
            "(cdp, cdp-fast, or html)"
        )

    urls = parse_urls(args.url)
    if not urls:
        p.error("--url must name at least one URL")
    if len(urls) > 1 and args.warmup < 2 * len(urls):
        # A mix trains the prefetch working set during its first cycle; the
        # noop baseline is not stationary until every URL has faulted its
        # pages in (measured 2026-08-14: the drift gate rejects runs whose
        # working set converges inside the measured window). Two full cycles
        # of warmup cover convergence.
        p.error(
            f"multi-URL runs need --warmup >= {2 * len(urls)} "
            f"(2x the URL count, working-set convergence); got {args.warmup}"
        )
    args.urls = urls

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
    # One probe per run: it owns the once-per-run healthy control, so the
    # control cannot be taken twice or taken from a clone that also failed.
    probe = FailureProbe(
        fcvm=args.fcvm,
        data_root=args.data_root,
        out_dir=args.out_dir,
        run_id=args.run_id,
        cdp_port=args.cdp_port,
    )
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
            "warmup": args.warmup, "url": args.url, "urls": args.urls, "format": args.format,
            "engine": args.engine,
            "guest_dns": snapshot["guest_dns"],
            "quality": args.quality,
            "source_revision": current_source_revision,
            "fcvm_path": args.fcvm,
            "fcvm_sha256": current_fcvm_sha256,
            "fcvm_version": command_text([args.fcvm, "--version"]),
            "harness_sha256": harness_sha256(),
            "runtime_bundle_sha256": current_runtime_bundle_sha256,
            "snapshot": snapshot_name,
            "snapshot_generation_id": snapshot["generation_id"],
            "snapshot_config_sha256": snapshot["config_sha256"],
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
            "ws_url_prewired": bool(args.ws_url) or bool(args.prewire),
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
                rec = dispatch_request(args, rep, arm, is_warmup, probe)
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
                # The abort below throws away the rest of the schedule on this
                # exception, so the record must carry enough to debug it: an
                # "OSError: [Errno 9] Bad file descriptor" alone cost a run on
                # 2026-08-13 and left nothing to diagnose.
                rec = {
                    "arm": arm,
                    "rep": rep,
                    "ok": False,
                    "error": f"{type(e).__name__}: {e}",
                    "traceback": traceback.format_exc(),
                }
                fatal = e
            rec["warmup"] = is_warmup  # discarded explicitly at analysis, never silently
            rec["url"] = url_for_rep(getattr(args, "urls", None) or [args.url], rep)
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
