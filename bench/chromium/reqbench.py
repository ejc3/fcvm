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
AND over an equal-length control window taken immediately before the kill, so
background load can be subtracted instead of silently inflating the attribution.

At saturation that CPU competes with new requests. A latency win is not a
capacity win, and this harness deliberately makes it impossible to report one as
the other.
"""

import argparse
import ctypes
import json
import os
import random
import select
import shutil
import signal
import subprocess
import sys
import time
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
    vals = [int(x) for x in parts[1:11]]
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
        return None
    return fd


def wait_pidfds(fds: list[int], timeout_s: float) -> bool:
    """Block until every pidfd is readable (process exited) or the deadline passes."""
    remaining = [fd for fd in fds if fd is not None]
    deadline = time.monotonic() + timeout_s
    poller = select.poll()
    for fd in remaining:
        poller.register(fd, select.POLLIN)
    while remaining:
        left = deadline - time.monotonic()
        if left <= 0:
            return False
        for fd, _ev in poller.poll(left * 1000):
            poller.unregister(fd)
            remaining.remove(fd)
    return True


def sample_all_until_gone(pids: dict, deadline: float) -> dict:
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

    Still a tight loop with no sleep, deliberately: the zombie window between
    `exit_notify()` and the reaper is short, and catching state `Z` is what
    upgrades a CPU figure from a lower bound to a complete one (see the module
    docstring). The cost of that spin is now measured (`self_cpu_ms`) and
    subtracted from the machine accounting rather than left in it.
    """
    live = dict(pids)
    last: dict = {}
    zombie: dict = {name: False for name in pids}
    while live and time.monotonic() < deadline:
        for name, pid in list(live.items()):
            s = proc_stat_fields(pid)
            if s is None:
                del live[name]
                continue
            last[name] = s
            if s[0] in ("Z", "X", "x"):
                zombie[name] = True
                del live[name]
    out = {}
    for name in pids:
        s = last.get(name)
        out[name] = {
            "cpu_ms": (s[1] + s[2]) * 1000.0 / CLK_TCK if s else None,
            "zombie_seen": zombie[name],
        }
    return out


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


def scan_state(state_dir: str, fcvm_pid: int = 0, name: str = ""):
    """One pass over the state dir. Matches on fcvm pid OR on VM name.

    The name-keyed match is not a convenience: `allocate_loopback_ip` saves the
    state file while `vm_state.pid` is still null (the pid is only set POST-RESUME,
    `src/commands/common.rs`), so there is a whole window — network setup, mount
    namespace, volume servers, the restore itself — in which the file exists,
    Firecracker may already be running, and a pid-keyed scan returns nothing. The
    name IS set before the first save, so it is the only key that covers that
    window. A state file left behind with `pid: null` is never removed by fcvm's
    own sweeper either (`cleanup_stale_state` bails on a null pid), so recovering
    it here is the difference between a reaped clone and a permanent leak.
    """
    try:
        names = os.listdir(state_dir)
    except OSError:
        return None, None
    for fname in names:
        if not fname.endswith(".json"):
            continue
        path = os.path.join(state_dir, fname)
        try:
            with open(path) as f:
                st = json.load(f)
        except (OSError, ValueError):
            continue
        if (fcvm_pid and st.get("pid") == fcvm_pid) or (name and st.get("name") == name):
            return path, st
    return None, None


def find_state(state_dir: str, fcvm_pid: int, deadline: float, watch=None, name: str = ""):
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
    try:
        while True:
            watch.drain()
            path, st = scan_state(state_dir, fcvm_pid, name)
            if st is not None:
                return path, st
            if not watch.wait(deadline - time.monotonic()):
                return None, None
    finally:
        if own:
            watch.close()


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


def wait_port(endpoint: str, deadline: float) -> float:
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
        try:
            s = socket.create_connection((host, int(port)), 0.25)
            s.close()
            return (time.monotonic() - t0) * 1000
        except OSError:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"CDP port {endpoint} never answered")
            time.sleep(delay)
            delay = min(delay * 1.5, 0.02)


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


def reap_disk(out: dict, state_path: str, data_dir: str) -> list:
    """Remove the clone's state file (+ its lock) and data dir. Errors are RECORDED.

    Never `ignore_errors=True`: a silently-failed rmtree reinstates the leak this
    function exists to prevent, and clone dirs can be root-owned when the bench
    runs under SUDO, so EPERM has to surface.
    """
    reaped = []
    if state_path and os.path.exists(state_path):
        try:
            os.remove(state_path)
            reaped.append(state_path)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{state_path}: {e}")
        # cleanup_stale_state removes `<vm_id>.json.lock` alongside the state file
        # (src/state/manager.rs); it never runs under SIGKILL, so we remove it too.
        lock = f"{state_path}.lock" if state_path else ""
        if lock and os.path.exists(lock):
            try:
                os.remove(lock)
                reaped.append(lock)
            except OSError as e:
                out.setdefault("disk_errors", []).append(f"{lock}: {e}")
    if data_dir and os.path.isdir(data_dir):
        try:
            shutil.rmtree(data_dir)
            reaped.append(data_dir)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{data_dir}: {e}")
    return reaped


def teardown_fast(fcvm_pid: int, state_path: str, data_dir: str, timeout_s: float) -> dict:
    """Concurrent SIGKILL via the pdeathsig chain, then synchronous on-disk reap.

    Raises `SurvivedTeardown` if any tracked child outlived the kill. That is not
    politeness: reaping the state file and rmtree'ing the data dir of a VM whose
    Firecracker is still RUNNING deletes the only record of a live microVM and
    pulls its rootfs out from under it, and every later measurement then runs on a
    box carrying an invisible ~1 GB tenant. Continuing the schedule after that is
    measuring contention, not the request path.
    """
    out: dict = {"mode": "fast"}

    kids = children_of(fcvm_pid)
    # Keyed by comm, but a COLLISION MUST NOT DROP A CHILD. `{proc_comm(p): p for p
    # in kids}` silently keeps only the last of any two children sharing a comm,
    # and a child that is not in `tracked` is neither waited on nor CPU-accounted —
    # it just does not exist as far as the leak check is concerned. fcvm's clone
    # happens to have three distinct comms today (firecracker / sleep / pasta), so
    # this never bit; that is luck, not a guarantee.
    tracked: dict = {}
    for p in kids:
        base = proc_comm(p) or f"pid{p}"
        key = base if base not in tracked else f"{base}#{p}"
        tracked[key] = p
    out["children"] = tracked
    fds = {name: pidfd_open(pid) for name, pid in tracked.items()}

    # Control window: same machine, same instant, no reclaim running. Gives the
    # background busy rate to subtract, so ambient load is not attributed to us.
    #
    # It SLEEPS. It used to be `while time.monotonic() - ctl_t0 < 0.05: pass`,
    # i.e. the "ambient" rate was measured while this thread held 100% of one
    # core. Measured back to back on this box, same ambient load: the spinning
    # version reported control_busy_cores 1.40 / 1.40 / 1.20 with 50.0 ms of the
    # harness's OWN cpu inside a 50 ms window; the sleeping version reports
    # 0.20 / 0.40 / 0.00 with 0.0 ms. That ~1.2-core inflation was then multiplied
    # by the ENTIRE reclaim window — in which the harness spins only while a child
    # is still alive — so the subtraction removed work that was never done there.
    # Belt and braces: the harness's own CPU is measured over both windows and
    # subtracted from each, so any residual self-load cancels by construction.
    ctl_self0 = self_cpu_ms()
    ctl_busy0, _ = machine_busy_jiffies()
    ctl_t0 = time.monotonic()
    time.sleep(0.05)
    ctl_busy1, _ = machine_busy_jiffies()
    ctl_dt = time.monotonic() - ctl_t0
    ctl_self_ms = self_cpu_ms() - ctl_self0
    ctl_busy_ms = (ctl_busy1 - ctl_busy0) * 1000.0 / CLK_TCK
    ctl_rate = (ctl_busy_ms - ctl_self_ms) / 1000.0 / ctl_dt if ctl_dt > 0 else 0.0

    # Sampled HERE, immediately before the kill — not before the control window.
    # Spanning the control window made the delta absorb ~50 ms of the still-running
    # VM's ordinary CPU and report it as reclaim.
    pre = {name: proc_stat_fields(pid) for name, pid in tracked.items()}

    self0 = self_cpu_ms()
    busy0, _ = machine_busy_jiffies()
    t_kill = time.monotonic()
    # ONE signal. The kernel fans SIGKILL out to firecracker, the namespace holder
    # AND pasta in a single forget_original_parent() pass — concurrent by
    # construction (all three carry PR_SET_PDEATHSIG; see the module docstring).
    try:
        os.kill(fcvm_pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    out["signal_ms"] = (time.monotonic() - t_kill) * 1000

    deadline = t_kill + timeout_s
    cpu = sample_all_until_gone(tracked, deadline)
    all_gone = wait_pidfds(list(fds.values()), max(0.0, deadline - time.monotonic()))
    t_gone = time.monotonic()
    busy1, _ = machine_busy_jiffies()
    self_ms = self_cpu_ms() - self0

    for fd in fds.values():
        if fd is not None:
            os.close(fd)

    window_s = t_gone - t_kill
    machine_cpu_ms = (busy1 - busy0) * 1000.0 / CLK_TCK
    out["reap_wall_ms"] = window_s * 1000
    out["all_gone"] = all_gone
    out["machine_cpu_ms"] = machine_cpu_ms
    out["harness_cpu_ms"] = self_ms
    out["machine_cpu_ms_excess"] = (machine_cpu_ms - self_ms) - ctl_rate * window_s * 1000
    out["control_busy_cores"] = ctl_rate
    out["control_harness_cpu_ms"] = ctl_self_ms

    # /proc/<pid>/stat counts in jiffies, so every CPU figure here is quantized to
    # one tick. At CLK_TCK=100 that is 10 ms, and a sub-tick reclaim reports a hard
    # 0.0 — a claim of zero CPU with zero uncertainty, which is exactly AGENTS.md
    # defect 6. Emit the bound instead of the point.
    tick_ms = 1000.0 / CLK_TCK
    out["tick_ms"] = tick_ms
    out["per_child_cpu"] = {}
    for name, pid in tracked.items():
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
        survivors = {
            name: pid for name, pid in tracked.items() if proc_stat_fields(pid) is not None
        }
        out["survivors"] = survivors
        out["disk_reap_skipped"] = True
        # Do NOT reap: the state file is the only record that these are ours.
        # SIGKILL each survivor directly so the box is not left carrying them, then
        # abort the schedule — a later rep measured next to a leaked microVM is not
        # a measurement of this request path.
        for pid in survivors.values():
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        out["teardown_total_ms"] = (time.monotonic() - t_kill) * 1000
        raise SurvivedTeardown(
            f"fast teardown of fcvm {fcvm_pid} left {survivors} alive after "
            f"{timeout_s:.1f}s; state {state_path} and data {data_dir} NOT reaped",
            out,
        )

    t_disk = time.monotonic()
    reaped = reap_disk(out, state_path, data_dir)
    out["disk_reap_ms"] = (time.monotonic() - t_disk) * 1000
    out["disk_reaped"] = reaped
    out["teardown_total_ms"] = (time.monotonic() - t_kill) * 1000
    return out


def teardown_normal(proc: subprocess.Popen, fcvm_pid: int, timeout_s: float) -> dict:
    """fcvm's own cleanup: SIGTERM, then await the full sequential unwind.

    kill -> holder kill -> network cleanup -> state delete -> FC log save ->
    data-dir removal, each awaited. This is the control the fast arm is measured
    against.
    """
    out: dict = {"mode": "normal"}
    kids = children_of(fcvm_pid)
    fds = [pidfd_open(p) for p in kids]
    t0 = time.monotonic()
    try:
        os.kill(fcvm_pid, signal.SIGTERM)
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
        proc.wait(timeout=10)
    out["fcvm_exit_ms"] = (time.monotonic() - t0) * 1000
    out["all_gone"] = wait_pidfds(fds, max(0.0, t0 + timeout_s - time.monotonic()))
    for fd in fds:
        if fd is not None:
            os.close(fd)
    out["reap_wall_ms"] = (time.monotonic() - t0) * 1000
    out["teardown_total_ms"] = out["reap_wall_ms"]
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

    name = f"rb-{os.getpid()}-{rep}-{'fast' if fast else 'norm'}"
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
    try:
        with open(log, "wb") as lf:
            # stdout/stderr to a FILE, never a pipe we do not drain: an undrained
            # 64 KB pipe blocks fcvm's writer and stalls everything behind it
            # (AGENTS.md "Pipe Buffer Deadlock in Tests").
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
        fcvm_pid = proc.pid

        state_path = data_dir = None
        try:
            deadline = t_spawn + args.timeout
            t = time.monotonic()
            state_path, state = find_state(args.state_dir, fcvm_pid, deadline, watch, name)
            if state is None:
                raise TimeoutError("clone state file never appeared")
            rec["discover_ms"] = (time.monotonic() - t) * 1000
            vm_id = state.get("vm_id", "")
            data_dir = os.path.join(args.data_root, "vm-disks", vm_id)
            rec["vm_id"] = vm_id

            endpoint = clone_cdp_endpoint(state, args.cdp_port)
            rec["endpoint"] = endpoint
            rec["port_wait_ms"] = wait_port(endpoint, deadline)

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
                render_module=os.path.join(HERE, "render.py"),
            )
            result = cdpdrive.drive(drive_args)
            rec["render"] = result
            rec["ok"] = bool(result.get("ok"))
            # THE CALLER'S ANSWER IS IN HAND HERE. Everything after this line is
            # teardown, and none of it is latency the caller pays.
            rec["blocking_ms"] = (time.monotonic() - t_spawn) * 1000
        except Exception as e:
            rec["ok"] = False
            rec["error"] = f"{type(e).__name__}: {e}"
            rec["blocking_ms"] = (time.monotonic() - t_spawn) * 1000
            if state_path is None:
                # find_state may have timed out while fcvm had ALREADY written the
                # file (it is saved with `pid: null` until post-resume). Leaving
                # state_path/data_dir as None here means teardown reaps nothing and
                # the clone's disk artifacts leak permanently — nothing else ever
                # removes a state file whose pid is null. Rescan by NAME once.
                state_path, state = scan_state(args.state_dir, fcvm_pid, name)
                if state is not None:
                    rec["recovered_state_by_name"] = True
                    rec["vm_id"] = state.get("vm_id", "")
                    data_dir = os.path.join(args.data_root, "vm-disks", rec["vm_id"])
    finally:
        watch.close()

    if fast:
        try:
            rec["teardown"] = teardown_fast(fcvm_pid, state_path, data_dir,
                                            args.teardown_timeout)
        except SurvivedTeardown as e:
            # The rep still gets a record, with the survivor list in it.
            rec["teardown"] = e.teardown
            rec["ok"] = False
            rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
            rec["log"] = log
            e.record = rec
            raise
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
    else:
        rec["teardown"] = teardown_normal(proc, fcvm_pid, args.teardown_timeout)
    rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
    rec["log"] = log
    return rec



def clone_backend_args(args) -> list:
    """UFFD serve-backed vs FILE-backed restore, as CLI args.

    These are different memory backends with different per-request costs -- the
    published 573 ms stage baseline is FILE-backed, and every guest page touched
    during startup costs a UFFD round trip on the other path. Mixing them and
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
    name = f"rb-{os.getpid()}-{rep}-noop"
    log = os.path.join(args.out_dir, f"{name}.log")
    rec: dict = {"arm": "noop", "rep": rep, "name": name}
    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap",
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    watch = DirWatch(args.state_dir)
    t_spawn = time.monotonic()
    try:
        with open(log, "wb") as lf:
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
        fcvm_pid = proc.pid
        try:
            deadline = t_spawn + args.timeout
            state_path, state = find_state(args.state_dir, fcvm_pid, deadline, watch, name)
            if state is None:
                raise TimeoutError("clone state file never appeared")
            rec["vm_id"] = state.get("vm_id", "")
            endpoint = clone_cdp_endpoint(state, args.cdp_port)
            rec["port_wait_ms"] = wait_port(endpoint, deadline)
            rec["ok"] = True
        except Exception as e:
            rec["ok"] = False
            rec["error"] = f"{type(e).__name__}: {e}"
    finally:
        watch.close()
    rec["blocking_ms"] = (time.monotonic() - t_spawn) * 1000
    rec["teardown"] = teardown_normal(proc, fcvm_pid, args.teardown_timeout)
    rec["wall_ms"] = (time.monotonic() - t_spawn) * 1000
    rec["log"] = log
    return rec


def run_exec_request(args, rep: int) -> dict:
    """Baseline arm: the existing per-request exec path, teardown fully awaited."""
    name = f"rb-{os.getpid()}-{rep}-exec"
    log = os.path.join(args.out_dir, f"{name}.log")
    driver = (
        f"python3 /opt/bench/render.py {args.url} --out-prefix /tmp/rb "
        f"--format {args.format} --quality {args.quality}"
    )
    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap", "--exec", driver,
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    rec: dict = {"arm": "exec", "rep": rep, "name": name, "timed_out": False}
    t0 = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
    try:
        rc = proc.wait(timeout=args.timeout + args.teardown_timeout)
    except subprocess.TimeoutExpired:
        # This was a bare `proc.wait(timeout=...)` with no handler, so a single
        # slow rep raised `TimeoutExpired` straight out of main() and killed the
        # harness — leaving the spawned fcvm ORPHANED (Python installs no
        # pdeathsig and reqbench is not a subreaper) with its Firecracker, holder,
        # pasta, state file and vm-disks dir still live, into the next run's
        # measurements. The exec arm was the only arm with this hole; the other two
        # always reach a teardown call.
        rec["timed_out"] = True
        try:
            os.kill(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pass
        rc = -9
        # The exec arm never resolves the clone's state file in the happy path, so
        # find it now — by name, since the pid may never have been written.
        sp, state = scan_state(args.state_dir, proc.pid, name)
        dd = os.path.join(args.data_root, "vm-disks", state.get("vm_id", "")) if state else ""
        rec["reaped"] = reap_disk(rec, sp, dd)
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
        ok=(rc == 0),
        # The exec arm has no separable "response in hand" instant that the host
        # can observe: fcvm returns only after its own teardown. blocking == wall
        # is not an approximation, it is the arm's defining property.
        blocking_ms=wall, wall_ms=wall,
        render_total_ms=render_ms, rc=rc, log=log,
    )
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
    p.add_argument("--ws-url", default="")
    p.add_argument("--fcvm", default=os.path.join(HERE, "..", "..", "target", "release", "fcvm"))
    p.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    p.add_argument("--state-dir", default="")
    p.add_argument("--out-dir", required=True)
    p.add_argument("--timeout", type=float, default=120.0)
    p.add_argument("--teardown-timeout", type=float, default=60.0)
    p.add_argument("--rust-log", default="fcvm=debug")
    args = p.parse_args()

    args.fcvm = os.path.abspath(args.fcvm)
    args.state_dir = args.state_dir or os.path.join(args.data_root, "state")
    os.makedirs(args.out_dir, exist_ok=True)
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]

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
    with open(out_path, "a") as out:
        meta = {
            "kind": "meta", "seed": args.seed,
            "backend": "file" if args.snapshot_tag else "uffd", "arms": arms, "reps": args.reps,
            "warmup": args.warmup, "url": args.url, "format": args.format,
            "loadavg": open("/proc/loadavg").read().split()[:3],
            "started": time.time(),
        }
        out.write(json.dumps(meta) + "\n")
        out.flush()
        for rep, arm, is_warmup in schedule:
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
                rec.update(ok=False, error=f"{type(e).__name__}: {e}")
                fatal = e
            except Exception as e:  # noqa: BLE001 - record, then re-raise
                rec = {"arm": arm, "rep": rep, "ok": False, "error": f"{type(e).__name__}: {e}"}
                fatal = e
            rec["warmup"] = is_warmup  # discarded explicitly at analysis, never silently
            rec["loadavg1"] = float(open("/proc/loadavg").read().split()[0])
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
