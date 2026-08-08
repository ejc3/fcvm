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
`kill(fcvm, SIGKILL)` is not "kill the parent and hope". fcvm spawns Firecracker
and the namespace holder with `PR_SET_PDEATHSIG=SIGKILL`
(`src/utils.rs::install_namespace_pre_exec`, `src/commands/common.rs::spawn_namespace_holder`).
When fcvm dies the kernel's `exit_notify`/`forget_original_parent` walks its child
list and delivers SIGKILL to EVERY child with a pdeathsig in one pass, before
anything else can run. So the two kills are issued concurrently by construction —
there is no ordering to get wrong, no `.await` between them, and no code of ours
that has to survive to do it.

That is also why this is the fast path that CANNOT leak. AGENTS.md is explicit:
"Prefer kernel-enforced reaping over cleanup code. A Drop impl, a signal handler,
or an always() cleanup step does not run when the process is SIGKILLed — which is
exactly the case that leaked." PR #730 restored precisely this chain after a
privilege boundary broke it and ~490 microVMs leaked. This arm depends on the
same guarantee the leak fix established, and
`test_sigkill_kills_firecracker_rootless` plus `test_bench_fast_teardown_leaks_nothing_clone`
(tests/test_signal_cleanup.rs) are its regression proof.

NO JANITOR. On-disk reaping (state file, data dir) is done synchronously, right
here, after the clock stops. It is off the caller's critical path but it is not
deferred to some sweeper that might not run — and it is MEASURED, not hidden.
SIGKILL cannot be caught, so fcvm's `cleanup_vm` never runs and the state file
survives the kill; reaping it here is REQUIRED, not an optimization.

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


def sample_until_gone(pid: int, deadline: float) -> dict:
    """Busy-read /proc/<pid>/stat until the task vanishes; keep the LAST sample.

    Deliberately a tight loop with no sleep: the zombie window between
    `exit_notify()` and the reaper is short, and catching state `Z` is what
    upgrades the CPU figure from a lower bound to a complete one (see module
    docstring). The loop is bounded by `deadline`.
    """
    last = None
    zombie_seen = False
    while time.monotonic() < deadline:
        s = proc_stat_fields(pid)
        if s is None:
            break
        last = s
        if s[0] in ("Z", "X", "x"):
            zombie_seen = True
            break
    if last is None:
        return {"cpu_ms": None, "zombie_seen": False}
    return {
        "cpu_ms": (last[1] + last[2]) * 1000.0 / CLK_TCK,
        "zombie_seen": zombie_seen,
    }


# ------------------------------------------------------------------ fcvm glue


def find_state(state_dir: str, fcvm_pid: int, deadline: float):
    """Locate the clone's state file by fcvm PID.

    A filesystem scan, not an inotify watch, and deliberately OFF the measured
    critical path where possible: it runs while the clone is still restoring.
    Recorded as `discover_ms` so its cost is visible rather than smuggled into
    the request time.
    """
    while time.monotonic() < deadline:
        try:
            names = os.listdir(state_dir)
        except OSError:
            names = []
        for name in names:
            if not name.endswith(".json"):
                continue
            path = os.path.join(state_dir, name)
            try:
                with open(path) as f:
                    st = json.load(f)
            except (OSError, ValueError):
                continue
            if st.get("pid") == fcvm_pid:
                return path, st
    return None, None


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


def teardown_fast(fcvm_pid: int, state_path: str, data_dir: str, timeout_s: float) -> dict:
    """Concurrent SIGKILL via the pdeathsig chain, then synchronous on-disk reap."""
    out: dict = {"mode": "fast"}

    kids = children_of(fcvm_pid)
    tracked = {proc_comm(p) or f"pid{p}": p for p in kids}
    out["children"] = tracked
    fds = {name: pidfd_open(pid) for name, pid in tracked.items()}
    pre = {name: proc_stat_fields(pid) for name, pid in tracked.items()}

    # Control window: same machine, same instant, no reclaim running. Gives the
    # background busy rate to subtract, so ambient load is not attributed to us.
    ctl_busy0, ctl_tot0 = machine_busy_jiffies()
    ctl_t0 = time.monotonic()
    while time.monotonic() - ctl_t0 < 0.05:
        pass
    ctl_busy1, ctl_tot1 = machine_busy_jiffies()
    ctl_dt = time.monotonic() - ctl_t0
    ctl_rate = (ctl_busy1 - ctl_busy0) / CLK_TCK / ctl_dt if ctl_dt > 0 else 0.0

    busy0, _ = machine_busy_jiffies()
    t_kill = time.monotonic()
    # ONE signal. The kernel fans SIGKILL out to firecracker AND the holder in a
    # single forget_original_parent() pass — concurrent by construction.
    try:
        os.kill(fcvm_pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    out["signal_ms"] = (time.monotonic() - t_kill) * 1000

    deadline = t_kill + timeout_s
    cpu = {name: sample_until_gone(pid, deadline) for name, pid in tracked.items()}
    all_gone = wait_pidfds(list(fds.values()), max(0.0, deadline - time.monotonic()))
    t_gone = time.monotonic()
    busy1, _ = machine_busy_jiffies()

    for fd in fds.values():
        if fd is not None:
            os.close(fd)

    window_s = t_gone - t_kill
    machine_cpu_ms = (busy1 - busy0) * 1000.0 / CLK_TCK
    out["reap_wall_ms"] = window_s * 1000
    out["all_gone"] = all_gone
    out["machine_cpu_ms"] = machine_cpu_ms
    out["machine_cpu_ms_excess"] = machine_cpu_ms - ctl_rate * window_s * 1000
    out["control_busy_cores"] = ctl_rate
    out["per_child_cpu"] = {}
    for name, pid in tracked.items():
        before = pre[name]
        base = (before[1] + before[2]) * 1000.0 / CLK_TCK if before else 0.0
        s = cpu[name]
        out["per_child_cpu"][name] = {
            "cpu_before_ms": base,
            "cpu_final_ms": s["cpu_ms"],
            "reclaim_cpu_ms": (s["cpu_ms"] - base) if s["cpu_ms"] is not None else None,
            # True  -> exit_mm() had already run, figure is COMPLETE.
            # False -> reaper won the race, figure is a LOWER BOUND.
            "complete": s["zombie_seen"],
        }

    t_disk = time.monotonic()
    reaped = []
    for path in (state_path,):
        if path and os.path.exists(path):
            try:
                os.remove(path)
                reaped.append(path)
            except OSError as e:
                out.setdefault("disk_errors", []).append(f"{path}: {e}")
    if data_dir and os.path.isdir(data_dir):
        try:
            shutil.rmtree(data_dir)
            reaped.append(data_dir)
        except OSError as e:
            out.setdefault("disk_errors", []).append(f"{data_dir}: {e}")
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


def run_cdp_request(args, rep: int, fast: bool) -> dict:
    import cdpdrive

    name = f"rb-{os.getpid()}-{rep}-{'fast' if fast else 'norm'}"
    log = os.path.join(args.out_dir, f"{name}.log")
    rec: dict = {"arm": "cdp-fast" if fast else "cdp", "rep": rep, "name": name}

    cmd = [args.fcvm, "snapshot", "run"] + clone_backend_args(args) + [
        "--name", name, "--no-dirty-tracking", "--no-swap",
    ]
    env = dict(os.environ, RUST_LOG=args.rust_log)
    t_spawn = time.monotonic()
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
        state_path, state = find_state(args.state_dir, fcvm_pid, deadline)
        if state is None:
            raise TimeoutError("clone state file never appeared")
        rec["discover_ms"] = (time.monotonic() - t) * 1000
        vm_id = state.get("vm_id", "")
        data_dir = os.path.join(args.data_root, "vm-disks", vm_id)
        rec["vm_id"] = vm_id

        endpoint = clone_cdp_endpoint(state, args.cdp_port)
        rec["endpoint"] = endpoint
        rec["port_wait_ms"] = wait_port(endpoint, deadline)

        drive_args = argparse.Namespace(
            cdp_host=endpoint,
            url=args.url,
            format=args.format,
            quality=args.quality,
            timeout=max(1.0, deadline - time.monotonic()),
            idle_wait_ms=0.0,
            out_prefix="",
            ws_url=args.ws_url,
            connect_retries=200,
            nav_timing=True,
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

    if fast:
        rec["teardown"] = teardown_fast(fcvm_pid, state_path, data_dir, args.teardown_timeout)
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
    t_spawn = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
    fcvm_pid = proc.pid
    try:
        deadline = t_spawn + args.timeout
        state_path, state = find_state(args.state_dir, fcvm_pid, deadline)
        if state is None:
            raise TimeoutError("clone state file never appeared")
        rec["vm_id"] = state.get("vm_id", "")
        endpoint = clone_cdp_endpoint(state, args.cdp_port)
        rec["port_wait_ms"] = wait_port(endpoint, deadline)
        rec["ok"] = True
    except Exception as e:
        rec["ok"] = False
        rec["error"] = f"{type(e).__name__}: {e}"
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
    t0 = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL, env=env)
    rc = proc.wait(timeout=args.timeout + args.teardown_timeout)
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
    return {
        "arm": "exec", "rep": rep, "name": name, "ok": rc == 0,
        # The exec arm has no separable "response in hand" instant that the host
        # can observe: fcvm returns only after its own teardown. blocking == wall
        # is not an approximation, it is the arm's defining property.
        "blocking_ms": wall, "wall_ms": wall,
        "render_total_ms": render_ms, "rc": rc, "log": log,
    }


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
            if arm == "exec":
                rec = run_exec_request(args, rep)
            elif arm == "noop":
                rec = run_noop_request(args, rep)
            else:
                rec = run_cdp_request(args, rep, fast=(arm == "cdp-fast"))
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
    print(f"\nwrote {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
