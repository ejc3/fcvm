#!/usr/bin/env python3
"""Start-rate benchmark: how many snapshot clones reach healthy per second on one host.

A start counts when the clone's own fcvm process logs that its health monitor saw the VM
healthy ("VM healthy, switching to ... polling"), which it writes after persisting
health_status=healthy to the clone's state file. The snapshot records an HTTP health check
(prep boots the workload with --health-check), so healthy means the guest's network is up,
fc-agent is connected and the workload answered HTTP through the clone's own network path.
At the end the run reads `fcvm ls --json` and fails unless every clone the logs call healthy
is also healthy in its state file.

Subcommands:
  prep      boot the baseline VM, snapshot it, start the memory server; writes prep.json
  run       launch COUNT clones keeping at most CONCURRENCY spawned-but-not-healthy at once,
            wait for all of them, verify, tear them down, write the records
  teardown  stop the serve and the baseline recorded in prep.json
  report    print the summary of one or more run directories

Only the standard library is used. Every number in summary.json comes from the run's own
records (samples.jsonl, host.tsv), so a summary can be regenerated with `report`.
"""

import argparse
import heapq
import json
import os
import random
import re
import resource
import selectors
import signal
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

BASELINE_TIMEOUT_S = 600

HEALTHY_MARKER = "VM healthy, switching to"

# (stage name, marker that opens it, marker that closes it); times are the harness's
# receive times of the clone's log lines, relative to its spawn.
CLONE_START = ("Cloning VM from serve PID", "Cloning VM directly from snapshot")
STAGES = [
    ("pre-log", None, CLONE_START),
    ("state+listeners", CLONE_START, "spawning namespace holder"),
    ("netns+pasta", "spawning namespace holder", "pasta + bridge setup complete"),
    ("net->load", "pasta + bridge setup complete", "loading snapshot with"),
    ("snapshot-load", "loading snapshot with", "VM resume completed"),
    ("resume->agent", "VM resume completed", "exec server ready"),
    ("agent->healthy", "exec server ready", HEALTHY_MARKER),
]
# Markers recorded whenever they appear, for stages that differ by network mode.
EXTRA_MARKERS = [
    "Cloning VM from serve PID",
    "Cloning VM directly from snapshot",
    "connecting to memory server",
    "spawning namespace holder",
    "pasta + bridge setup complete",
    "loading snapshot with",
    "VM resume completed",
    "exec server ready",
    "starting health monitor",
    HEALTHY_MARKER,
]


def now_ns():
    return time.monotonic_ns()


def wall():
    return time.time()


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw).stdout


def fcvm_cmd(fcvm, sudo):
    # A harness already running as root (make runs it under sudo for bridged and routed)
    # spawns fcvm directly, so no clone pays for a sudo process.
    return ["sudo", "-E", fcvm] if sudo and os.geteuid() != 0 else [fcvm]


def imds(path):
    try:
        tok_req = urllib.request.Request(
            "http://169.254.169.254/latest/api/token",
            method="PUT",
            headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"},
        )
        tok = urllib.request.urlopen(tok_req, timeout=2).read().decode()
        req = urllib.request.Request(
            f"http://169.254.169.254/latest/meta-data/{path}",
            headers={"X-aws-ec2-metadata-token": tok},
        )
        return urllib.request.urlopen(req, timeout=2).read().decode()
    except Exception:
        return None


def store_queue_depths(dev, sys_block=Path("/sys/class/block")):
    """`nr_requests` of the store's devices: of each member when the device has members (an
    md RAID0 over NVMe disks, whose own queue says nothing about theirs), else of the device
    itself. A partition has no queue of its own, so its disk's is read. A device or value that
    cannot be read is left out; none at all gives None."""
    if not dev:
        return None
    base = sys_block / dev
    members = sorted((base / "slaves").glob("*")) if (base / "slaves").is_dir() else []
    depths = {}
    for d in members or [base]:
        d = d.resolve()
        if not (d / "queue").is_dir():
            d = d.parent
        try:
            depths[d.name] = int((d / "queue" / "nr_requests").read_text())
        except (OSError, ValueError):
            pass
    return depths or None


def host_facts(repo):
    store = sh(["stat", "-f", "-c", "%T", "/mnt/fcvm-btrfs"]).strip()
    src = ""
    for line in Path("/proc/mounts").read_text().splitlines():
        parts = line.split()
        if len(parts) > 2 and parts[1] == "/mnt/fcvm-btrfs":
            src = parts[0]
    backing = None
    backing_fs = None
    if src.startswith("/dev/loop"):
        try:
            backing = Path(f"/sys/block/{Path(src).name}/loop/backing_file").read_text().strip()
            backing_fs = sh(["stat", "-f", "-c", "%T", backing]).strip()
        except (OSError, subprocess.CalledProcessError):
            backing = backing or "unknown"
    queue_depth = store_queue_depths(Path(src).name)
    mem_kb = int(
        next(ln for ln in Path("/proc/meminfo").read_text().splitlines() if ln.startswith("MemTotal")).split()[1]
    )
    rev = subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True, capture_output=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(repo), "diff", "--quiet"]).returncode != 0
    return {
        "instance_type": imds("instance-type"),
        "kernel": os.uname().release,
        "cpus": os.cpu_count(),
        "mem_gib": round(mem_kb / 1048576, 1),
        "store_fs": store,
        "store_device": src,
        "store_backing_file": backing,
        "store_backing_fs": backing_fs,
        "store_queue_depths": queue_depth,
        "source_revision": rev,
        "source_dirty": dirty,
    }


# --------------------------------------------------------------------------- prep


def cmd_prep(a):
    out = claim_dir(a.out)
    stamp = int(wall())
    base = f"sr-base-{stamp}"
    snap = f"sr-snap-{a.network}-{stamp}"
    fc = fcvm_cmd(a.fcvm, a.sudo)
    env = dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "fcvm=info"))
    run = fc + [
        "podman", "run", "--name", base, "--network", a.network,
        "--cpu", str(a.cpu), "--mem", str(a.mem),
        "--health-check", "http://localhost/",
        "--no-snapshot",
    ] + (["--ipv6-prefix", a.ipv6_prefix] if a.ipv6_prefix else []) + [a.image]
    with ChildOwner(a.sudo) as own:
        log = open(out / "baseline.log", "w")
        base_proc = own.add(
            subprocess.Popen(run, stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
        )
        deadline = time.time() + BASELINE_TIMEOUT_S
        base_pid = None
        while time.time() < deadline:
            if base_proc.poll() is not None:
                sys.exit(f"baseline exited early ({base_proc.returncode}); see {out/'baseline.log'}")
            vms = list_vms(a.fcvm, a.sudo)
            hit = [v for v in vms if v.get("name") == base and v.get("health_status") == "healthy"]
            if hit:
                base_pid = hit[0]["pid"]
                break
            time.sleep(0.5)
        if base_pid is None:
            sys.exit(f"baseline never became healthy in {BASELINE_TIMEOUT_S} s")
        print(f"baseline healthy pid={base_pid}")
        sh(fc + ["snapshot", "create", "--pid", str(base_pid), "--tag", snap], env=env)
        print(f"snapshot {snap} created")
        serve_pid = None
        serve_proc = None
        if a.uffd:
            slog = open(out / "serve.log", "w")
            # The serve admits 256 clones by default (uffd::server DEFAULT_MAX_CLONES_PER_SERVER);
            # a start-rate run keeps every clone alive, so it raises the cap.
            senv = dict(env, FCVM_UFFD_MAX_CLONES=str(a.max_clones))
            sfc = (["sudo", "--preserve-env=RUST_LOG,FCVM_UFFD_MAX_CLONES", a.fcvm] if a.sudo and os.geteuid() != 0 else [a.fcvm])
            serve_proc = own.add(subprocess.Popen(
                sfc + ["snapshot", "serve", snap], stdout=slog, stderr=subprocess.STDOUT, env=senv, start_new_session=True
            ))
            deadline = time.time() + 120
            while time.time() < deadline and serve_pid is None:
                for line in (out / "serve.log").read_text(errors="replace").splitlines():
                    if "Serve PID:" in line:
                        serve_pid = int(line.split("Serve PID:")[1].split()[0])
                if serve_proc.poll() is not None:
                    sys.exit(f"serve exited; see {out/'serve.log'}")
                time.sleep(0.1)
            if serve_pid is None:
                sys.exit("serve never printed its PID")
            print(f"serve pid={serve_pid}")
        rec = {
            "baseline": base, "baseline_pid": base_pid, "baseline_launcher_pid": base_proc.pid,
            "snapshot": snap, "serve_pid": serve_pid,
            "serve_launcher_pid": serve_proc.pid if serve_proc else None,
            "max_clones": a.max_clones if a.uffd else None,
            "network": a.network, "cpu": a.cpu, "mem_mib": a.mem, "image": a.image, "sudo": a.sudo,
            "ipv6_prefix": a.ipv6_prefix,
        }
        (out / "prep.json").write_text(json.dumps(rec, indent=2) + "\n")
        print(json.dumps(rec))

        own.release_all()  # prep.json is published: baseline and serve are meant to outlive prep

def list_vms(fcvm, sudo):
    """`fcvm ls --json`, or exit: a run that cannot read health renders no verdict."""
    try:
        vms = json.loads(sh(fcvm_cmd(fcvm, sudo) + ["ls", "--json"]))
    except (subprocess.CalledProcessError, json.JSONDecodeError, OSError) as e:
        sys.exit(f"BLOCKED: fcvm ls --json failed: {e}")
    if not isinstance(vms, list):
        sys.exit("BLOCKED: fcvm ls --json did not return a list")
    return vms


def cmd_teardown(a):
    prep = json.loads(Path(a.prep).read_text())
    for key in ("serve_pid", "baseline_pid"):
        pid = prep.get(key)
        if pid:
            kill(pid, signal.SIGTERM, prep["sudo"])
    print("teardown signalled")


def kill(pid, sig, sudo):
    if sudo and os.geteuid() != 0:
        subprocess.run(["sudo", "kill", f"-{sig.name[3:]}", str(pid)], capture_output=True)
    else:
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass


# --------------------------------------------------------------------------- run


class HostSampler(threading.Thread):
    """Once a second: load average, MemAvailable, running/blocked tasks, fcvm process count."""

    def __init__(self, path):
        super().__init__(name="host-sampler", daemon=True)
        self.path = path
        self.stop = threading.Event()

    def run(self):
        with open(self.path, "w") as fh:
            fh.write("t_mono_ns\tt_wall\tload1\tload5\trunnable\tprocs_running\tprocs_blocked\tmem_avail_mib\n")
            while not self.stop.is_set():
                la = Path("/proc/loadavg").read_text().split()
                st = {}
                for line in Path("/proc/stat").read_text().splitlines():
                    k, _, v = line.partition(" ")
                    if k in ("procs_running", "procs_blocked"):
                        st[k] = v.strip()
                ma = next(
                    ln for ln in Path("/proc/meminfo").read_text().splitlines() if ln.startswith("MemAvailable")
                ).split()[1]
                fh.write(
                    f"{now_ns()}\t{wall():.3f}\t{la[0]}\t{la[1]}\t{la[3]}\t{st.get('procs_running')}\t"
                    f"{st.get('procs_blocked')}\t{int(ma)//1024}\n"
                )
                fh.flush()
                self.stop.wait(1.0)


class Clone:
    __slots__ = ("i", "name", "proc", "log", "t_spawn", "spawn_ns", "marks", "t_healthy", "exit", "buf", "error", "done")

    def __init__(self, i, name):
        self.i, self.name = i, name
        self.proc = None
        self.log = None
        self.t_spawn = None
        self.spawn_ns = None
        self.marks = {}
        self.t_healthy = None
        self.exit = None
        self.buf = b""
        self.error = None
        self.done = False


def ensure_fd_limit(count, rlimit=resource):
    """Raise the soft descriptor limit to what `count` clones hold, or exit BLOCKED.

    Each clone holds its log file and the read end of its output pipe for the whole run,
    and sudo resets the soft limit to 1024: past about 500 clones os.pipe() failed with
    EMFILE inside a launcher thread and the run hung (bridged, 2026-11-22).
    """
    need = 2 * count + 256
    soft, hard = rlimit.getrlimit(rlimit.RLIMIT_NOFILE)
    if soft < need:
        rlimit.setrlimit(rlimit.RLIMIT_NOFILE, (min(need, hard), hard))
        soft = rlimit.getrlimit(rlimit.RLIMIT_NOFILE)[0]
    if soft < need:
        sys.exit(f"BLOCKED: {count} clones need {need} file descriptors, the limit is {soft}")
    return soft


class ChildOwner:
    """Owns every child a command starts and stops them on any exit path.

    Leaving the `with` block, by return, exception, SystemExit, Ctrl-C or SIGTERM (turned into
    KeyboardInterrupt), sends each child its stop signal, SIGKILLs what is left and reaps it.
    A command releases only the children it means to leave running, after it has published
    whatever lets a later command find them.
    """

    def __init__(self, sudo):
        self.sudo = sudo
        self.lock = threading.Lock()
        self.children = []  # (proc, group, first signal, via sudo)
        self.prev_term = None
        self.installed = False
        self.before_stop = None  # runs first on exit: end whatever still starts children
        self.after_stop = None  # runs last on exit: end threads that read the children

    def add(self, proc, group="", sig=signal.SIGTERM, sudo=None):
        with self.lock:
            self.children.append((proc, group, sig, self.sudo if sudo is None else sudo))
        return proc

    def release_all(self):
        with self.lock:
            self.children = []

    def forget(self, proc):
        """Drop a child that has been reaped, so a long run's list holds only live ones."""
        with self.lock:
            self.children = [entry for entry in self.children if entry[0] is not proc]

    def stop(self, group=None, limit_s=120):
        """Stop the children of `group` (all when None); seconds taken. Safe to call twice."""
        td0 = now_ns()
        with self.lock:
            mine = [(p, sg, su) for p, g, sg, su in self.children if group is None or g == group]
        for p, sg, su in mine:
            if p.poll() is None:
                kill(p.pid, sg, su)
        for p, _, su in mine:
            try:
                p.wait(timeout=max(1, limit_s - (now_ns() - td0) / 1e9))
            except subprocess.TimeoutExpired:
                kill(p.pid, signal.SIGKILL, su)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pass
        return (now_ns() - td0) / 1e9

    def __enter__(self):
        if threading.current_thread() is threading.main_thread():
            self.prev_term = signal.signal(signal.SIGTERM, _interrupt)
            self.installed = True
        return self

    def __exit__(self, *exc):
        if self.installed:
            signal.signal(signal.SIGTERM, self.prev_term)
        try:
            if self.before_stop:
                self.before_stop()
            self.stop()
        finally:
            if self.after_stop:
                self.after_stop()
        return False


def _interrupt(signum, frame):
    raise KeyboardInterrupt


def positive_int(text):
    try:
        v = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is not an integer") from None
    if v < 1:
        raise argparse.ArgumentTypeError(f"must be at least 1, got {v}")
    return v


def claim_dir(path):
    """Create the run's output directory, or exit when it exists: a run never reuses one.

    os.mkdir is the atomic claim, so two runs given one OUT cannot both win, and a stale
    summary.json from an earlier run is never presented as the current one.
    """
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.mkdir(p)
    except FileExistsError:
        sys.exit(f"BLOCKED: {p} already exists; a run needs a fresh output directory")
    return p


class Progress:
    """Run-wide completion state. Every read-modify-write of it happens under one lock.

    A clone finishes exactly once: the first of healthy, failed or timed out wins and later
    events for it are ignored, so pending never goes negative and no clone counts twice.
    """

    def __init__(self, count, concurrency):
        self.cv = threading.Condition()
        self.count = count
        self.concurrency = concurrency
        self.pending = 0  # spawned (or being spawned), not yet healthy and not failed
        self.healthy_n = 0
        self.failed_n = 0
        self.next_i = 0
        self.stopping = False

    def complete(self, c, t=None, err=None):
        """Publish c's terminal outcome; False when c already has one."""
        with self.cv:
            if c.done:
                return False
            c.done = True
            if err is None:
                c.t_healthy = t
                self.healthy_n += 1
            else:
                c.error = err
                self.failed_n += 1
            self.pending -= 1
            self.cv.notify_all()
            return True

    def claim(self):
        """A launcher's next clone index once a slot is free, or None when the run is over or stopped."""
        with self.cv:
            while self.pending >= self.concurrency and not self.stopping:
                self.cv.wait(0.5)
            if self.stopping or self.next_i >= self.count:
                return None
            i = self.next_i
            self.next_i += 1
            self.pending += 1
            return i

    def stop(self):
        with self.cv:
            self.stopping = True
            self.cv.notify_all()

    def counts(self):
        with self.cv:
            return self.healthy_n, self.failed_n


def cmd_run(a):
    prep = json.loads(Path(a.prep).read_text())
    out = claim_dir(a.out)
    (out / "clones").mkdir()
    repo = Path(__file__).resolve().parents[2]
    facts = host_facts(repo)
    cfg = {
        "count": a.count, "concurrency": a.concurrency, "backend": "uffd" if prep["serve_pid"] else "file",
        "network": prep["network"], "cpu": prep["cpu"], "mem_mib": prep["mem_mib"], "image": prep["image"],
        "snapshot": prep["snapshot"], "clone_timeout_s": a.timeout, "rust_log": a.rust_log,
        "launchers": a.launchers, "pidstat_interval": a.pidstat_interval, "hold_s": a.hold,
    }
    (out / "host.json").write_text(json.dumps({**facts, "config": cfg}, indent=2) + "\n")
    print(json.dumps(cfg))

    list_vms(a.fcvm, prep["sudo"])  # exits when health cannot be read
    ensure_fd_limit(a.count)
    for tool in ("pidstat",):
        if subprocess.run(["sh", "-c", f"command -v {tool}"], capture_output=True).returncode:
            sys.exit(f"BLOCKED: {tool} missing (sysstat)")
    base_cmd = fcvm_cmd(a.fcvm, prep["sudo"]) + ["snapshot", "run"]
    if prep["serve_pid"]:
        base_cmd += ["--pid", str(prep["serve_pid"])]
    else:
        base_cmd += ["--snapshot", prep["snapshot"]]
    if prep.get("ipv6_prefix"):
        base_cmd += ["--ipv6-prefix", prep["ipv6_prefix"]]
    env = dict(os.environ, RUST_LOG=a.rust_log, NO_COLOR="1")
    run_id = f"sr{int(wall()) % 100000}"

    sel = selectors.DefaultSelector()
    sel_lock = threading.Lock()
    clones = [Clone(i, f"{run_id}-c{i:05d}") for i in range(a.count)]
    progress = Progress(a.count, a.concurrency)
    t0 = now_ns()
    t0_wall = wall()

    def on_line(c, line, t):
        text = line.decode(errors="replace")
        c.log.write(f"{(t - c.spawn_ns)/1e6:10.2f} {text}\n")
        for m in EXTRA_MARKERS:
            if m in text and m not in c.marks:
                c.marks[m] = (t - c.spawn_ns) / 1e6
        if HEALTHY_MARKER in text:
            progress.complete(c, t=t)

    reader_stop = threading.Event()

    def reader():
        while not reader_stop.is_set():
            with sel_lock:
                has = bool(sel.get_map())
            if not has:
                time.sleep(0.005)
                continue
            for key, _ in sel.select(timeout=0.05):
                c = key.data
                t = now_ns()
                try:
                    chunk = os.read(key.fd, 65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    with sel_lock:
                        sel.unregister(key.fd)
                    os.close(key.fd)
                    if c.buf:
                        on_line(c, c.buf, t)
                        c.buf = b""
                    c.log.flush()
                    if not c.done:
                        rc = c.proc.wait()
                        c.exit = rc
                        progress.complete(c, err=f"exited {rc} before healthy")
                    continue
                c.buf += chunk
                while b"\n" in c.buf:
                    line, _, c.buf = c.buf.partition(b"\n")
                    on_line(c, line, t)

    def launcher():
        while True:
            i = progress.claim()
            if i is None:
                return
            c = clones[i]
            c.spawn_ns = now_ns()
            r = w = None
            try:
                c.log = open(out / "clones" / f"{c.name}.log", "w")
                r, w = os.pipe()
                os.set_blocking(r, False)
                c.proc = own.add(
                    subprocess.Popen(
                        base_cmd + ["--name", c.name], stdout=w, stderr=w, stdin=subprocess.DEVNULL,
                        env=env, close_fds=True, start_new_session=True,
                    ),
                    group="clones",
                )
            except OSError as e:
                # A launcher that dies here leaves the clone pending for ever, so the run
                # would wait on it instead of reporting it.
                for fd in (w, r):
                    if fd is not None:
                        os.close(fd)
                progress.complete(c, err=f"spawn failed: {e}")
                continue
            c.t_spawn = (now_ns() - c.spawn_ns) / 1e6
            os.close(w)
            with sel_lock:
                sel.register(r, selectors.EVENT_READ, c)

    sampler = HostSampler(out / "host.tsv")
    rt = threading.Thread(target=reader, name="reader", daemon=True)
    launchers = [threading.Thread(target=launcher, name=f"launcher-{k}", daemon=True) for k in range(a.launchers)]
    own = ChildOwner(prep["sudo"])

    def stop_launching():
        progress.stop()
        for th in launchers:
            if th.is_alive():
                th.join()

    def stop_threads():
        reader_stop.set()
        if rt.is_alive():
            rt.join(timeout=5)
        sampler.stop.set()
        if sampler.is_alive():
            sampler.join(timeout=3)

    own.before_stop = stop_launching
    own.after_stop = stop_threads

    with own:
        sampler.start()
        with open(out / "pidstat.log", "w") as pidstat_log:
            own.add(
                subprocess.Popen(
                    ["pidstat", "-u", "-h", str(a.pidstat_interval)],
                    stdout=pidstat_log, stderr=subprocess.STDOUT, start_new_session=True,
                ),
                group="pidstat", sig=signal.SIGINT, sudo=False,
            )
        rt.start()
        for th in launchers:
            th.start()

        deadline_extra = a.timeout * 1e9
        while True:
            with progress.cv:
                healthy_n, failed_n = progress.healthy_n, progress.failed_n
                if healthy_n + failed_n >= a.count:
                    break
                progress.cv.wait(0.5)
            t = now_ns()
            for c in clones:
                if c.spawn_ns and not c.done and t - c.spawn_ns > deadline_extra:
                    progress.complete(c, err=f"not healthy within {a.timeout}s")
            if a.fail_fast and progress.counts()[1]:
                break
        # No launcher may start a clone after this point: the sweeps below must see every child.
        stop_launching()
        healthy_n, failed_n = progress.counts()
        t_all = now_ns()
        print(f"healthy={healthy_n} failed={failed_n} in {(t_all - t0)/1e9:.2f}s")

        # Verify against the state files: every clone the log calls healthy must be healthy there.
        vms = list_vms(a.fcvm, prep["sudo"])
        by_name = {v.get("name"): v for v in vms}
        ok_names = [c.name for c in clones if c.t_healthy is not None]
        state_healthy = [n for n in ok_names if by_name.get(n, {}).get("health_status") == "healthy"]
        alive = sum(1 for c in clones if c.proc is not None and c.proc.poll() is None)
        verify = {
            "log_healthy": len(ok_names),
            "state_healthy": len(state_healthy),
            "ls_entries_for_run": sum(1 for n in by_name if n and n.startswith(run_id)),
            "alive_at_end": alive,
        }
        print(json.dumps(verify))
        if a.hold:
            # Keep every clone up so host.tsv and pidstat record what the idle clones cost.
            print(f"holding {alive} clones for {a.hold}s")
            time.sleep(a.hold)
        t_hold_end = now_ns()

        teardown_s = own.stop("clones")
        time.sleep(0.5)
        reader_stop.set()
        rt.join(timeout=5)
        sampler.stop.set()
        sampler.join(timeout=3)
        own.stop("pidstat")

        with open(out / "samples.jsonl", "w") as fh:
            for c in clones:
                if c.log:
                    c.log.close()
                fh.write(json.dumps({
                    "i": c.i, "name": c.name,
                    "spawn_at_ms": (c.spawn_ns - t0) / 1e6 if c.spawn_ns else None,
                    "popen_ms": c.t_spawn,
                    "healthy_at_ms": (c.t_healthy - t0) / 1e6 if c.t_healthy else None,
                    "spawn_to_healthy_ms": (c.t_healthy - c.spawn_ns) / 1e6 if c.t_healthy else None,
                    "marks_ms": c.marks, "error": c.error, "exit": c.exit,
                }) + "\n")
        (out / "phases.json").write_text(json.dumps({
            "t0_mono_ns": t0, "all_done_mono_ns": t_all, "hold_end_mono_ns": t_hold_end,
            "teardown_end_mono_ns": now_ns(),
            "t0_wall": t0_wall, "all_done_wall": t0_wall + (t_all - t0) / 1e9,
            "hold_end_wall": t0_wall + (t_hold_end - t0) / 1e9,
        }) + "\n")
        summary = summarize(out, verify=verify, teardown_s=teardown_s, t0_wall=t0_wall)
        print_summary(summary)
        bad = (
            verify["state_healthy"] != verify["log_healthy"]
            or verify["log_healthy"] == 0
            or failed_n > 0
        )
        if bad:
            print("FAILED: see summary.json (failures, or log/state health disagree)", file=sys.stderr)
            sys.exit(1)


# --------------------------------------------------------------------------- churn

POOLED_MARKER = "running in a pooled namespace"
GENERATION_RE = re.compile(r"\bgeneration=(\d+)")
# The measured window of a churn run: at least this long, and this many clone lifetimes.
CHURN_MIN_WINDOW_S = 60
CHURN_MIN_LIFETIMES = 10
# How far two figures that should be equal may differ, as a fraction of their scale.
CHURN_FLAT = 0.02
# The samples of the pool's counters around a window: how far from each edge the nearest
# may be, and how far apart two may be.
CHURN_POOL_EDGE_S = 2.0
CHURN_POOL_GAP_S = 3.0
CHURN_POOL_TIMEOUT_S = 2.0
# A clone still starting at the window's end is hung past this many window p99s.
CHURN_HUNG_P99S = 10
# How far the clones up and starting may be from ALIVE, and the alive count from
# rate x time up.
CHURN_OCCUPANCY = 0.05
CHURN_LITTLE = 0.10
CHURN_WARM_STARTS = 20
POOL_STATES = ("building", "ready", "leased", "resetting", "destroying")


def pool_stats(fcvm, sudo, timeout=None):
    """`fcvm netpool stats` as a dict, or None when no pool answers (within `timeout` s)."""
    try:
        r = subprocess.run(fcvm_cmd(fcvm, sudo) + ["netpool", "stats"], text=True, capture_output=True,
                           timeout=timeout)
        return json.loads(r.stdout) if r.returncode == 0 else None
    except (OSError, json.JSONDecodeError, subprocess.TimeoutExpired):
        return None


def reset_quantile_s(first, last, q):
    """Upper edge, in seconds, of the bucket holding quantile q of the resets between two
    samples of the pool's counters (`reset_us_log2[i]` counts resets of 2^i to 2^(i+1) us)."""
    hist = [b - a for a, b in zip(first["reset_us_log2"], last["reset_us_log2"])]
    total = sum(hist)
    if total <= 0:
        return None
    wanted, seen = max(1, int(-(-total * q // 1))), 0
    for i, n in enumerate(hist):
        seen += n
        if n and seen >= wanted:
            return (1 << (i + 1)) / 1e6
    return None


def ends(samples, value):
    """Means of `value` over the first and the last tenth of `samples` (at least one each)."""
    k = max(1, len(samples) // 10)
    head = [value(x) for x in samples[:k]]
    tail = [value(x) for x in samples[-k:]]
    return sum(head) / len(head), sum(tail) / len(tail)


def pool_bracket(pool, w0, w1):
    """The pool's counters around a window: the last sample answered by w0, the first asked
    for at or after w1, and whether the samples between cover the window.

    `pool` is (asked_s, answered_s, counters) per sample, or (asked_s, counters) from a
    sampler that asked one question after another: such a sample was answered by the time
    the next was asked. A count taken from samples inside the window alone misses what
    happened between the last of them and the window's end, so the two edges are samples
    known to lie outside it. Returns (first, last, between, coverage); first or last is
    None when there is none, and coverage["ok"] says whether each edge is within
    CHURN_POOL_EDGE_S of the window and no two samples are over CHURN_POOL_GAP_S apart.
    """
    rows = sorted(((x[0], x[1], x[2]) if len(x) == 3 else (x[0], None, x[1]) for x in pool), key=lambda x: x[0])
    answered = [done if done is not None else (rows[i + 1][0] if i + 1 < len(rows) else None)
                for i, (_, done, _) in enumerate(rows)]
    first = last = None
    for i, done in enumerate(answered):
        if done is not None and done <= w0:
            first = i
    for i, (asked, _, _) in enumerate(rows):
        if asked >= w1:
            last = i
            break
    coverage = {"samples": 0, "before_s": None, "after_s": None, "max_gap_s": None, "ok": False}
    if first is None or last is None:
        inside = [x for x in rows if w0 <= x[0] < w1]
        coverage["samples"] = len(inside)
        if first is not None:
            coverage["before_s"] = round(w0 - answered[first], 3)
        if last is not None:
            coverage["after_s"] = round(rows[last][0] - w1, 3)
        return (rows[first] if first is not None else None, rows[last] if last is not None else None,
                inside, coverage)
    span = rows[first:last + 1]
    gaps = [b[0] - a[0] for a, b in zip(span, span[1:])]
    coverage.update(
        samples=len(span), before_s=round(w0 - answered[first], 3), after_s=round(rows[last][0] - w1, 3),
        max_gap_s=round(max(gaps), 3) if gaps else None,
    )
    coverage["ok"] = (coverage["before_s"] <= CHURN_POOL_EDGE_S and coverage["after_s"] <= CHURN_POOL_EDGE_S
                      and bool(gaps) and max(gaps) <= CHURN_POOL_GAP_S)
    return rows[first], rows[last], span, coverage


def alive_coverage(alive, w0, w1):
    """Whether the (t_s, in_flight, running) samples watch the whole window: one within
    CHURN_POOL_EDGE_S of each edge, and no two over CHURN_POOL_GAP_S apart between them."""
    times = sorted(x[0] for x in alive if w0 - CHURN_POOL_EDGE_S <= x[0] <= w1 + CHURN_POOL_EDGE_S)
    cover = {"samples": len(times), "first_s": None, "last_s": None, "max_gap_s": None, "ok": False}
    if times:
        gaps = [b - a for a, b in zip(times, times[1:])]
        cover.update(first_s=round(max(0.0, times[0] - w0), 3), last_s=round(max(0.0, w1 - times[-1]), 3),
                     max_gap_s=round(max(gaps), 3) if gaps else None)
        cover["ok"] = (times[0] <= w0 + CHURN_POOL_EDGE_S and times[-1] >= w1 - CHURN_POOL_EDGE_S
                       and bool(gaps) and max(gaps) <= CHURN_POOL_GAP_S)
    return cover


def churn_verdict(cfg, clones, alive, pool, run=None):
    """Whether a churn window is a sustained rate, and its figures.

    cfg: alive, lifetime_s, concurrency, window_start_s, window_end_s, clone_timeout_s.
    clones: one dict per clone with spawn_s, healthy_s, stop_s, exit_s (seconds since the
      run's start, None when it did not happen), generation (None when the clone built its
      own namespace), error, and fail_s, when the harness called it failed (records
      without it are judged by exit_s).
    alive: (t_s, in_flight, running) samples. pool: samples as `pool_bracket` takes them.
    run: what only the run itself knows: warm (warm-up ended), warm_detail, measured (the
      window ran to its end), state_steady and state_healthy (clones the logs call running,
      and how many of them `fcvm ls` calls healthy), left ({leased, resetting, listed} after
      the drain), polls_failed. None judges the window's records alone.

    Each condition of bench/start-rate/README.md "Churn" is one entry of `conditions`; the
    verdict is ok only when every one holds. A condition whose records are missing fails.
    `sustained` holds the window's figures when the verdict is ok and is None otherwise;
    `observed` holds them either way.
    """
    w0, w1 = cfg["window_start_s"], cfg["window_end_s"]
    window = w1 - w0
    inside = lambda t: t is not None and w0 <= t < w1  # noqa: E731
    starts = [c for c in clones if inside(c["healthy_s"])]
    stops = [c for c in clones if c["healthy_s"] is not None and inside(c["exit_s"])]
    lat = [(c["healthy_s"] - c["spawn_s"]) * 1000 for c in starts if c["spawn_s"] is not None]

    def failed_at(c):
        for key in ("fail_s", "exit_s", "spawn_s"):
            if c.get(key) is not None:
                return c[key]
        return None

    errors = [c for c in clones if c["error"]]
    failed = [c for c in errors if inside(failed_at(c))]
    failed_warmup = [c for c in errors if failed_at(c) is not None and failed_at(c) < w0]
    # A start that hangs has no error yet when the window ends. A clone spawned before the
    # end and not healthy by it is one, once it is older than ten times the window's p99.
    timeout = cfg.get("clone_timeout_s")
    p99 = pct(lat, 0.99)
    bounds = [x for x in (timeout, CHURN_HUNG_P99S * p99 / 1000 if p99 is not None else None) if x is not None]
    hung_after = min(bounds) if bounds else 0.0
    # Decided from when things happened, not from whether an error was recorded: a clone
    # that was not healthy by the window's end and had not failed by then was still
    # starting at the end, whatever became of it afterwards (its fcvm exiting half a
    # second later gives it an error with a time outside the window).
    failed_by_end = lambda c: bool(c["error"]) and failed_at(c) is not None and failed_at(c) < w1  # noqa: E731
    hung = [c for c in clones if not failed_by_end(c) and c["spawn_s"] is not None and c["spawn_s"] < w1
            and (c["healthy_s"] is None or c["healthy_s"] >= w1) and w1 - c["spawn_s"] > hung_after]
    alive_w = [x for x in alive if w0 <= x[0] < w1]
    alive_cover = alive_coverage(alive, w0, w1)
    rate = len(starts) / window if window > 0 else 0.0
    conditions = []

    def cond(name, ok, detail):
        conditions.append({"name": name, "ok": bool(ok), "detail": detail})

    need = max(CHURN_MIN_WINDOW_S, CHURN_MIN_LIFETIMES * cfg["lifetime_s"])
    cond("window", window >= need, f"{window:.1f} s measured, {need:.1f} s needed "
         f"({CHURN_MIN_WINDOW_S} s and {CHURN_MIN_LIFETIMES} lifetimes of {cfg['lifetime_s']} s)")
    slack = max(2, CHURN_FLAT * len(starts))
    cond("starts equal stops", starts and abs(len(starts) - len(stops)) <= slack,
         f"{len(starts)} starts, {len(stops)} stops, {slack:.0f} apart allowed")
    cond("no failed clone", not failed and not hung,
         f"{len(failed)} failed in the window" + (f" (first: {failed[0]['error']})" if failed else "")
         + f", {len(hung)} still starting at its end after more than {hung_after:.2f} s"
         + f", {len(failed_warmup)} failed in warm-up")

    mean = in_flight = None
    if not alive_cover["ok"]:
        # A sampler that stalled leaves one sample, or a stretch without any: a count
        # that was not watched over the whole window cannot be called flat.
        detail = (f"the alive samples do not cover the window: {alive_cover['samples']} samples, the first "
                  f"{alive_cover['first_s']} s from its start, the last {alive_cover['last_s']} s from its end, "
                  f"{alive_cover['max_gap_s']} s the longest gap (allowed: {CHURN_POOL_EDGE_S} s, "
                  f"{CHURN_POOL_EDGE_S} s, {CHURN_POOL_GAP_S} s)")
        cond("alive count flat", False, detail)
        cond("alive count accounted for", False, detail)
    elif alive_w:
        running = [x[2] for x in alive_w]
        head, tail = ends(alive_w, lambda x: x[2])
        mean = sum(running) / len(running)
        in_flight = sum(x[1] for x in alive_w) / len(alive_w)
        # The clones being replaced at any moment are a random few (rate x time to
        # healthy), so the count is never still; a swing of a fifth is a sawtooth.
        swing = max(5, 0.2 * cfg["alive"])
        flat = abs(tail - head) <= max(1, CHURN_FLAT * cfg["alive"]) and max(running) - min(running) <= swing
        cond("alive count flat", flat, f"mean {mean:.1f}, {min(running)} to {max(running)}, "
             f"first tenth {head:.1f}, last tenth {tail:.1f}, target {cfg['alive']}")
        # The loop is closed: ALIVE is asked for and the rate is what comes out. The run
        # held ALIVE only if the clones up and starting add up to it, and its rate is the
        # rate of that many clones only if alive = rate x the time a clone stays up.
        up_s = [c["exit_s"] - c["healthy_s"] for c in stops]
        occupied = mean + in_flight
        held = abs(occupied - cfg["alive"]) <= max(1, CHURN_OCCUPANCY * cfg["alive"])
        if up_s:
            expected = rate * sum(up_s) / len(up_s)
            consistent = abs(mean - expected) <= max(1, CHURN_LITTLE * mean)
            cond("alive count accounted for", held and consistent,
                 f"{mean:.1f} up + {in_flight:.1f} starting = {occupied:.1f} of the {cfg['alive']} asked for; "
                 f"{rate:.1f} starts/s x {sum(up_s) / len(up_s):.2f} s up each = {expected:.1f}")
        else:
            cond("alive count accounted for", False, "no clone stopped in the window")
    else:
        cond("alive count flat", False, "no alive samples in the window")
        cond("alive count accounted for", False, "no alive samples in the window")

    recycled = [c for c in starts if (c["generation"] or 0) >= 2]
    cond("every start in a recycled namespace", starts and len(recycled) == len(starts),
         f"{len(recycled)} of {len(starts)} starts had generation 2 or more")

    slots = builds = destroys = reset_p99 = None
    polls_failed = (run or {}).get("polls_failed")
    first, last, between, coverage = pool_bracket(pool, w0, w1)
    cond("pool counters cover the window", coverage["ok"],
         f"{coverage['samples']} samples; the first answered {coverage['before_s']} s before the window, "
         f"the last asked {coverage['after_s']} s after it, {coverage['max_gap_s']} s the longest gap "
         f"(allowed: {CHURN_POOL_EDGE_S} s, {CHURN_POOL_EDGE_S} s, {CHURN_POOL_GAP_S} s)"
         + (f"; {polls_failed} polls failed" if polls_failed else ""))
    if coverage["ok"]:
        first, last = first[2], last[2]
        slots = sum(first[k] for k in POOL_STATES)
        head, tail = ends(between, lambda x: x[2]["ready"])
        cond("free slots flat", abs(tail - head) <= CHURN_FLAT * slots,
             f"ready: first tenth {head:.1f}, last tenth {tail:.1f}, of {slots} slots")
        builds, destroys = last["builds"] - first["builds"], last["destroys"] - first["destroys"]
        cond("no namespace built or destroyed", builds == 0 and destroys == 0,
             f"{builds} built, {destroys} destroyed between the samples around the window")
        reset_p99 = reset_quantile_s(first, last, 0.99)
        if reset_p99 is None or mean is None:
            cond("slots cover the resets", False, "no reset finished in the window")
        else:
            wanted = mean + rate * reset_p99 + cfg["concurrency"]
            cond("slots cover the resets", slots >= wanted,
                 f"{slots} slots; alive {mean:.0f} + {rate:.1f}/s x reset p99 {reset_p99:.3f} s "
                 f"+ {cfg['concurrency']} in flight = {wanted:.0f}")
    else:
        for name in ("free slots flat", "no namespace built or destroyed", "slots cover the resets"):
            cond(name, False, "the pool's counters do not cover the window")

    if run is not None:
        cond("warm-up ended", run.get("warm"), run.get("warm_detail") or "not recorded")
        cond("window completed", run.get("measured"),
             "the window ran to its end" if run.get("measured") else "the run stopped before the window's end")
        steady, state_healthy = run.get("state_steady"), run.get("state_healthy")
        cond("state files agree", steady and state_healthy == steady,
             f"{state_healthy} of {steady} running clones healthy in fcvm ls")
        left = run.get("left") or {}
        counts = [left.get(k) for k in ("leased", "resetting", "listed")]
        cond("nothing left after the drain", all(n == 0 for n in counts),
             "after the drain: " + ", ".join(
                 f"{k} {'not recorded' if n is None else n}" for k, n in zip(("leased", "resetting", "listed in fcvm ls"), counts)))

    figures = {
        "window_s": round(window, 2),
        "starts": len(starts),
        "stops": len(stops),
        "starts_per_s": round(rate, 2),
        "stops_per_s": round(len(stops) / window, 2) if window > 0 else 0.0,
        "recycled_fraction": round(len(recycled) / len(starts), 4) if starts else None,
        "alive_target": cfg["alive"],
        "alive_mean": round(mean, 1) if mean is not None else None,
        "in_flight_mean": round(in_flight, 1) if in_flight is not None else None,
        "spawn_to_healthy_ms": {"p50": pct(lat, 0.5), "p90": pct(lat, 0.9), "p99": p99, "max": pct(lat, 1.0)},
    }
    ok = all(c["ok"] for c in conditions)
    return {
        "ok": ok,
        "conditions": conditions,
        "sustained": figures if ok else None,
        "observed": figures,
        "failures": {"window": len(failed), "still_starting_at_end": len(hung), "warmup": len(failed_warmup),
                     "total": len({id(c) for c in errors} | {id(c) for c in hung})},
        "alive_coverage": alive_cover,
        "pool": {"slots": slots, "builds": builds, "destroys": destroys, "reset_p99_s": reset_p99,
                 "coverage": coverage, "polls_failed": polls_failed},
    }


def lifetime_of(index, lifetime_s, jitter):
    """Clone `index`'s lifetime: uniform in lifetime_s x (1 - jitter .. 1 + jitter), the same
    for a given index in every run. With one lifetime for all, the clones a burst started
    together stop together for the whole run, and the alive count swings with them."""
    return lifetime_s * (1 + jitter * (2 * random.Random(index).random() - 1))


def warmup_quiet_s(alive, lifetime_s, min_starts=CHURN_WARM_STARTS):
    """The stretch of all-recycled starts that ends warm-up: 2 s, or for a small run twice
    the time ALIVE clones of this lifetime take to make `min_starts` starts. With 2 s for
    every run, one that starts fewer than ten clones a second never ends its warm-up."""
    return max(2.0, 2 * min_starts * lifetime_s / alive)


def recycled_only_since(clones, lifetime_s, now_s, quiet_s=2.0, min_starts=CHURN_WARM_STARTS):
    """Whether warm-up is over at now_s: a lifetime has passed since the first healthy
    clone, and every start of the last quiet_s (at least min_starts of them) took a
    recycled namespace. `clones` is (healthy_s, generation) per healthy clone, in order."""
    if not clones or now_s - clones[0][0] < lifetime_s:
        return False
    recent = 0
    for t, generation in reversed(clones):
        if t < now_s - quiet_s:
            break
        if (generation or 0) < 2:
            return False
        recent += 1
    return recent >= min_starts


class ChurnClone(Clone):
    __slots__ = ("generation", "stop_ns", "exit_ns", "fail_ns", "popen_ms")

    def __init__(self, i, name):
        super().__init__(i, name)
        self.generation = None
        self.stop_ns = None
        self.exit_ns = None
        self.fail_ns = None
        self.popen_ms = None


def cmd_churn(a):
    """Starts and stops at one rate: each clone lives LIFETIME seconds after it is healthy
    and is then stopped, and a new one starts while fewer than ALIVE are up or starting."""
    need = max(CHURN_MIN_WINDOW_S, CHURN_MIN_LIFETIMES * a.lifetime)
    if a.duration < need:
        sys.exit(f"BLOCKED: --duration {a.duration} is under {need:.0f} s "
                 f"({CHURN_MIN_WINDOW_S} s and {CHURN_MIN_LIFETIMES} lifetimes)")
    if a.timeout >= a.duration:
        sys.exit(f"BLOCKED: --timeout {a.timeout} is not under --duration {a.duration}: a start that hangs "
                 "would outlast the window without ever timing out")
    quiet_s = warmup_quiet_s(a.alive, a.lifetime)
    if quiet_s > a.warmup_limit / 2:
        sys.exit(f"BLOCKED: {a.alive} clones of {a.lifetime} s start too seldom for a warm-up within "
                 f"--warmup-limit {a.warmup_limit}: it ends on {quiet_s:.0f} s of recycled starts")
    prep = json.loads(Path(a.prep).read_text())
    if prep["network"] != "rootless":
        sys.exit(f"BLOCKED: the prep's network is {prep['network']}; only a rootless clone is pooled")
    if pool_stats(a.fcvm, prep["sudo"], CHURN_POOL_TIMEOUT_S) is None:
        sys.exit("BLOCKED: `fcvm netpool stats` gave no counters; churn measures clones in a running pool")
    out = claim_dir(a.out)
    (out / "clones").mkdir()
    repo = Path(__file__).resolve().parents[2]
    cfg = {
        "mode": "churn", "alive": a.alive, "lifetime_s": a.lifetime, "concurrency": a.concurrency,
        "lifetime_jitter": a.lifetime_jitter, "duration_s": a.duration, "warmup_limit_s": a.warmup_limit,
        "backend": "uffd" if prep["serve_pid"] else "file", "network": prep["network"],
        "cpu": prep["cpu"], "mem_mib": prep["mem_mib"], "image": prep["image"],
        "snapshot": prep["snapshot"], "clone_timeout_s": a.timeout, "rust_log": a.rust_log,
        "launchers": a.launchers, "warmup_quiet_s": quiet_s,
    }
    (out / "host.json").write_text(json.dumps({**host_facts(repo), "config": cfg}, indent=2) + "\n")
    print(json.dumps(cfg))
    list_vms(a.fcvm, prep["sudo"])
    ensure_fd_limit(a.alive + a.concurrency + 64)

    base_cmd = fcvm_cmd(a.fcvm, prep["sudo"]) + ["snapshot", "run"]
    base_cmd += ["--pid", str(prep["serve_pid"])] if prep["serve_pid"] else ["--snapshot", prep["snapshot"]]
    # A clone inherits its snapshot's published ports, and a clone with one is not pooled.
    base_cmd += ["--publish", "none"]
    env = dict(os.environ, RUST_LOG=a.rust_log, NO_COLOR="1")
    run_id = f"ch{int(wall()) % 100000}"

    cv = threading.Condition()
    st = {"in_flight": 0, "running": 0, "next": 0, "failed": 0, "stopping": False}
    flying = {}      # index -> clone, spawned and not yet healthy or failed; under cv
    clones = []      # every ChurnClone, by index; appended under cv
    healthy = []     # (healthy_s, generation), in order; appended under cv
    due = []         # heap of (stop at, monotonic ns, index)
    sel = selectors.DefaultSelector()
    sel_lock = threading.Lock()
    t0 = now_ns()
    t0_wall = wall()
    secs = lambda t: None if t is None else (t - t0) / 1e9  # noqa: E731

    def on_line(c, line, t):
        text = line.decode(errors="replace")
        c.log.write(f"{(t - c.spawn_ns)/1e6:10.2f} {text}\n")
        if POOLED_MARKER in text:
            found = GENERATION_RE.search(text)
            c.generation = int(found.group(1)) if found else None
        if HEALTHY_MARKER in text:
            with cv:
                if not c.done:
                    c.done, c.t_healthy = True, t
                    flying.pop(c.i, None)
                    st["in_flight"] -= 1
                    st["running"] += 1
                    healthy.append((secs(t), c.generation))
                    life = lifetime_of(c.i, a.lifetime, a.lifetime_jitter)
                    heapq.heappush(due, (t + int(life * 1e9), c.i))
                    cv.notify_all()

    def on_exit(c, t):
        c.exit = c.proc.wait()
        c.exit_ns = t
        own.forget(c.proc)
        with cv:
            if not c.done:
                c.done = True
                c.error, c.fail_ns = f"exited {c.exit} before healthy", t
                flying.pop(c.i, None)
                st["in_flight"] -= 1
                st["failed"] += 1
            elif c.t_healthy is not None:
                st["running"] -= 1
                if c.stop_ns is None:
                    c.error, c.fail_ns = f"exited {c.exit} while running, before its stop", t
                    st["failed"] += 1
            cv.notify_all()

    reader_stop = threading.Event()

    def reader():
        while not reader_stop.is_set():
            with sel_lock:
                has = bool(sel.get_map())
            if not has:
                time.sleep(0.005)
                continue
            for key, _ in sel.select(timeout=0.05):
                c = key.data
                t = now_ns()
                try:
                    chunk = os.read(key.fd, 65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    with sel_lock:
                        sel.unregister(key.fd)
                    os.close(key.fd)
                    if c.buf:
                        on_line(c, c.buf, t)
                        c.buf = b""
                    c.log.close()
                    on_exit(c, t)
                    continue
                c.buf += chunk
                while b"\n" in c.buf:
                    line, _, c.buf = c.buf.partition(b"\n")
                    on_line(c, line, t)

    def launcher():
        while True:
            with cv:
                while not st["stopping"] and (
                    st["in_flight"] >= a.concurrency or st["in_flight"] + st["running"] >= a.alive
                ):
                    cv.wait(0.2)
                if st["stopping"]:
                    return
                c = ChurnClone(st["next"], f"{run_id}-c{st['next']:07d}")
                st["next"] += 1
                st["in_flight"] += 1
                flying[c.i] = c
                clones.append(c)
            c.spawn_ns = now_ns()
            r = w = None
            try:
                c.log = open(out / "clones" / f"{c.name}.log", "w")
                r, w = os.pipe()
                os.set_blocking(r, False)
                c.proc = own.add(
                    subprocess.Popen(
                        base_cmd + ["--name", c.name], stdout=w, stderr=w, stdin=subprocess.DEVNULL,
                        env=env, close_fds=True, start_new_session=True,
                    ),
                    group="clones",
                )
                c.popen_ms = (now_ns() - c.spawn_ns) / 1e6
            except OSError as e:
                for fd in (w, r):
                    if fd is not None:
                        os.close(fd)
                with cv:
                    c.done, c.error, c.fail_ns = True, f"spawn failed: {e}", now_ns()
                    flying.pop(c.i, None)
                    st["in_flight"] -= 1
                    st["failed"] += 1
                    cv.notify_all()
                continue
            os.close(w)
            with sel_lock:
                sel.register(r, selectors.EVENT_READ, c)

    alive_samples, pool_samples = [], []
    polls = {"failed": 0}
    pool_lock = threading.Lock()
    pool_log = open(out / "pool.jsonl", "w")
    samplers_stop = threading.Event()

    def alive_sampler():
        while not samplers_stop.wait(0.5):
            with cv:
                alive_samples.append((secs(now_ns()), st["in_flight"], st["running"]))

    def sample_pool():
        """Ask the pool for its counters once and record the answer, or that there was
        none: a pool too loaded to answer must show as a gap, not as fewer samples."""
        asked = secs(now_ns())
        counters = pool_stats(a.fcvm, prep["sudo"], CHURN_POOL_TIMEOUT_S)
        answered = secs(now_ns())
        with pool_lock:
            if counters is None:
                polls["failed"] += 1
                row = {"t_s": asked, "answered_s": answered, "failed": True}
            else:
                pool_samples.append((asked, answered, counters))
                # Unrounded: the window's edges lie microseconds from the two samples taken
                # around it, and a rounded time can land on the other side of an edge.
                row = {"t_s": asked, "answered_s": answered, **counters}
            pool_log.write(json.dumps(row) + "\n")
            pool_log.flush()
        return counters

    def pool_sampler():
        while not samplers_stop.wait(1.0):
            sample_pool()

    def service(until_ns):
        """Stop the clones whose lifetime is over and fail the ones that never got healthy,
        until until_ns. Returns False when a clone failed and --fail-fast is set."""
        while now_ns() < until_ns:
            t = now_ns()
            with cv:
                expired = []
                while due and due[0][0] <= t:
                    expired.append(clones[heapq.heappop(due)[1]])
                late = [c for c in flying.values() if c.spawn_ns and t - c.spawn_ns > a.timeout * 1e9]
                for c in late:
                    c.done, c.error, c.fail_ns = True, f"not healthy within {a.timeout}s", t
                    del flying[c.i]
                    st["in_flight"] -= 1
                    st["failed"] += 1
                for c in expired:
                    c.stop_ns = t
                failed = a.fail_fast and st["failed"] > 0
                cv.notify_all()
            for c in expired + late:
                if c.proc is not None and c.proc.poll() is None:
                    kill(c.proc.pid, signal.SIGTERM, prep["sudo"])
            if failed:
                return False
            time.sleep(0.02)
        return True

    sampler = HostSampler(out / "host.tsv")
    threads = [threading.Thread(target=reader, name="reader", daemon=True),
               threading.Thread(target=alive_sampler, name="alive", daemon=True),
               threading.Thread(target=pool_sampler, name="pool", daemon=True)]
    launchers = [threading.Thread(target=launcher, name=f"launcher-{k}", daemon=True) for k in range(a.launchers)]
    own = ChildOwner(prep["sudo"])

    def stop_launching():
        with cv:
            st["stopping"] = True
            cv.notify_all()
        for th in launchers:
            if th.is_alive():
                th.join()

    def stop_threads():
        reader_stop.set()
        samplers_stop.set()
        sampler.stop.set()
        for th in threads + [sampler]:
            if th.is_alive():
                th.join(timeout=5)

    own.before_stop = stop_launching
    own.after_stop = stop_threads

    with own:
        sampler.start()
        for th in threads + launchers:
            th.start()

        # Warm-up: until every recent start took a namespace some clone had before.
        warm_limit = t0 + int(a.warmup_limit * 1e9)
        warm = False
        while now_ns() < warm_limit:
            if not service(now_ns() + int(0.25e9)):
                break
            with cv:
                warm = recycled_only_since(healthy, a.lifetime, secs(now_ns()), quiet_s)
            if warm:
                break
        # The window lies between two samples of the pool's counters: one answered before
        # it starts, one asked for after it ends.
        sample_pool()
        w0 = now_ns()
        cpu0 = resource.getrusage(resource.RUSAGE_SELF)
        measured = warm and service(w0 + int(a.duration * 1e9))
        w1 = now_ns()
        cpu1 = resource.getrusage(resource.RUSAGE_SELF)
        sample_pool()

        # Verify against the state files: a clone the logs call healthy, that was not being
        # stopped before or after the listing, must be healthy in its state file.
        with cv:
            before = {c.name for c in clones if c.t_healthy is not None and c.stop_ns is None}
        by_name = {v.get("name"): v for v in list_vms(a.fcvm, prep["sudo"])}
        with cv:
            steady = [c.name for c in clones if c.name in before and c.stop_ns is None]
        state_healthy = [n for n in steady if by_name.get(n, {}).get("health_status") == "healthy"]

        stop_launching()
        drain0 = now_ns()
        with cv:
            left = st["in_flight"] + st["running"]
            # The drain is this run's stop for every clone still up, and the end of every
            # clone still starting: neither is a failure.
            for c in clones:
                if c.t_healthy is not None and c.stop_ns is None and c.exit_ns is None:
                    c.stop_ns = drain0
            for c in flying.values():
                c.done = True
            st["in_flight"] -= len(flying)
            flying.clear()
        teardown_s = own.stop("clones")
        # Until the pool has every namespace back, or has not answered three times in a
        # row: a pool that is gone has nothing to wait for, and the leak check then fails
        # for want of its counters.
        counters, unanswered = None, 0
        while True:
            counters = pool_stats(a.fcvm, prep["sudo"], CHURN_POOL_TIMEOUT_S)
            unanswered = 0 if counters is not None else unanswered + 1
            idle = counters is not None and counters["leased"] == 0 and counters["resetting"] == 0
            if idle or unanswered >= 3 or now_ns() - drain0 > 300e9:
                break
            time.sleep(0.2)
        drain_s = (now_ns() - drain0) / 1e9
        time.sleep(0.5)
        stop_threads()
        pool_log.close()
        # What the run left behind: namespaces still lent or being reset, and clones of
        # this run still in `fcvm ls`.
        try:
            listed = sum(1 for v in list_vms(a.fcvm, prep["sudo"]) if str(v.get("name", "")).startswith(run_id + "-"))
        except SystemExit:
            listed = None
        left_after = {"leased": counters["leased"] if counters else None,
                      "resetting": counters["resetting"] if counters else None, "listed": listed}

        rows = []
        with open(out / "samples.jsonl", "w") as fh:
            for c in clones:
                if c.log and not c.log.closed:
                    c.log.close()
                row = {
                    "i": c.i, "name": c.name, "spawn_s": secs(c.spawn_ns), "healthy_s": secs(c.t_healthy),
                    "stop_s": secs(c.stop_ns), "exit_s": secs(c.exit_ns), "fail_s": secs(c.fail_ns),
                    "generation": c.generation, "error": c.error, "exit": c.exit, "popen_ms": c.popen_ms,
                }
                rows.append(row)
                fh.write(json.dumps(row) + "\n")
        with open(out / "alive.tsv", "w") as fh:
            fh.write("t_s\tin_flight\trunning\n")
            for t, flying, running in alive_samples:
                fh.write(f"{t!r}\t{flying}\t{running}\n")
        window_s = (w1 - w0) / 1e9
        cpu_s = (cpu1.ru_utime + cpu1.ru_stime) - (cpu0.ru_utime + cpu0.ru_stime)
        run = {
            "window_start_s": secs(w0), "window_end_s": secs(w1),
            "warm": bool(warm), "measured": bool(measured),
            "warm_detail": f"{secs(w0):.1f} s until every start of {quiet_s:.1f} s took a recycled namespace"
                           if warm else f"starts were not all recycled within {a.warmup_limit} s",
            "state_steady": len(steady), "state_healthy": len(state_healthy),
            "left": left_after, "polls_failed": polls["failed"],
            "drain": {"clones": left, "teardown_s": round(teardown_s, 2), "until_pool_idle_s": round(drain_s, 2)},
            # The harness's own cost over the window: one process, its reader and launcher
            # threads under one interpreter lock. Near one core, the harness is the limit.
            "driver": {"cpu_s": round(cpu_s, 2), "cpu_cores": round(cpu_s / window_s, 3) if window_s > 0 else None},
        }
        (out / "run.json").write_text(json.dumps(run, indent=2) + "\n")
        summary = churn_summary(cfg, t0_wall, rows, alive_samples, pool_samples, run)
        (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print_churn(summary)
        if not summary["ok"]:
            print("FAILED: not a sustained rate; see the conditions in summary.json", file=sys.stderr)
            sys.exit(1)


def churn_summary(cfg, t0_wall, rows, alive_samples, pool_samples, run):
    """A churn run's summary from its records: the verdict, the warm-up and the drain apart,
    and the harness's own cost."""
    w0, w1 = run["window_start_s"], run["window_end_s"]
    window = {"alive": cfg["alive"], "lifetime_s": cfg["lifetime_s"], "concurrency": cfg["concurrency"],
              "clone_timeout_s": cfg.get("clone_timeout_s"), "window_start_s": w0, "window_end_s": w1}
    verdict = churn_verdict(window, rows, alive_samples, pool_samples, run)
    warm_starts = [r for r in rows if r["healthy_s"] is not None and r["healthy_s"] < w0]
    popen = [r["popen_ms"] for r in rows
             if r.get("popen_ms") is not None and r["spawn_s"] is not None and w0 <= r["spawn_s"] < w1]
    driver = dict(run.get("driver") or {})
    driver["popen_ms"] = {"p50": pct(popen, 0.5), "p99": pct(popen, 0.99), "max": pct(popen, 1.0)}
    return {
        "config": cfg, "t0_wall": t0_wall, "window_start_s": w0, "window_end_s": w1, **verdict,
        "warmup": {"s": round(w0, 2), "starts": len(warm_starts),
                   "starts_per_s": round(len(warm_starts) / w0, 2) if w0 else None,
                   "failures": verdict["failures"]["warmup"]},
        "drain": run.get("drain"),
        "left": run.get("left"),
        "driver": driver,
    }


def churn_from_records(out):
    """A churn directory's summary, judged again from its records (samples.jsonl,
    alive.tsv, pool.jsonl, and run.json or, for a run of the first harness, what its
    summary.json says of the run). A record a condition needs and the directory lacks
    fails that condition."""
    out = Path(out)
    old = json.loads((out / "summary.json").read_text()) if (out / "summary.json").exists() else {}
    cfg = old.get("config") or json.loads((out / "host.json").read_text())["config"]
    rows = [json.loads(line) for line in (out / "samples.jsonl").read_text().splitlines() if line.strip()]
    alive = []
    for line in (out / "alive.tsv").read_text().splitlines()[1:]:
        t, flying, running = line.split("\t")
        alive.append((float(t), int(flying), int(running)))
    pool, polls_failed = [], 0
    for line in (out / "pool.jsonl").read_text().splitlines():
        row = json.loads(line)
        if row.get("failed"):
            polls_failed += 1
            continue
        asked, answered = row.pop("t_s"), row.pop("answered_s", None)
        pool.append((asked, answered, row))
    if (out / "run.json").exists():
        run = json.loads((out / "run.json").read_text())
    else:
        # The first harness kept these in its summary alone: the window's edges rounded, its
        # two run-time conditions as text, and no listing after the drain.
        by_name = {c["name"]: c for c in old.get("conditions", [])}
        w0 = old["warmup"]["s"]
        state = re.match(r"(\d+) of (\d+) ", by_name.get("state files agree", {}).get("detail", ""))
        warm = by_name.get("warm-up ended", {}).get("ok")
        drain = old.get("drain") or {}
        idle = drain.get("until_pool_idle_s") is not None and drain["until_pool_idle_s"] < 300
        run = {
            "window_start_s": w0, "window_end_s": w0 + old["window_s"],
            "warm": warm, "measured": warm, "warm_detail": by_name.get("warm-up ended", {}).get("detail"),
            "state_healthy": int(state.group(1)) if state else None,
            "state_steady": int(state.group(2)) if state else None,
            "left": {"leased": 0 if idle else None, "resetting": 0 if idle else None, "listed": None},
            "polls_failed": None, "drain": drain, "driver": None,
        }
    run.setdefault("polls_failed", polls_failed)
    return churn_summary(cfg, old.get("t0_wall"), rows, alive, pool, run)


def print_churn(s):
    o = s["observed"]
    print(("" if s["ok"] else "NOT SUSTAINED: ")
          + f"window {o['window_s']} s: {o['starts_per_s']} starts/s, {o['stops_per_s']} stops/s, "
          f"alive {o['alive_mean']} up + {o['in_flight_mean']} starting of {o['alive_target']} asked for, "
          f"recycled {o['recycled_fraction']}, "
          f"spawn to healthy p50 {o['spawn_to_healthy_ms']['p50']} ms p99 {o['spawn_to_healthy_ms']['p99']} ms")
    f = s["failures"]
    print(f"failures: {f['window']} in the window, {f['still_starting_at_end']} still starting at its end, "
          f"{f['warmup']} in warm-up")
    if s.get("warmup"):
        print(f"warm-up {s['warmup']['s']} s at {s['warmup']['starts_per_s']} starts/s")
    if s.get("drain"):
        print(f"drain of {s['drain']['clones']} clones {s['drain']['teardown_s']} s, "
              f"pool idle after {s['drain']['until_pool_idle_s']} s")
    d = s.get("driver") or {}
    if d.get("cpu_cores") is not None:
        print(f"harness: {d['cpu_cores']} cores of its own over the window, "
              f"spawn call p50 {d['popen_ms']['p50']} ms p99 {d['popen_ms']['p99']} ms")
    for c in s["conditions"]:
        print(f"  {'ok  ' if c['ok'] else 'FAIL'} {c['name']}: {c['detail']}")


# --------------------------------------------------------------------------- report


def pct(vals, q):
    vals = sorted(vals)
    if not vals:
        return None
    k = max(0, min(len(vals) - 1, int(round((len(vals) - 1) * q))))
    return round(vals[k], 1)


def mark(marks, key):
    """A marker's time; a tuple names alternatives (the UFFD and File paths log differently)."""
    if isinstance(key, tuple):
        return next((marks[k] for k in key if k in marks), None)
    return marks.get(key)


def summarize(out, verify=None, teardown_s=None, t0_wall=None):
    out = Path(out)
    rows = [json.loads(ln) for ln in (out / "samples.jsonl").read_text().splitlines() if ln.strip()]
    host = json.loads((out / "host.json").read_text())
    ok = [r for r in rows if r["healthy_at_ms"] is not None]
    hs = sorted(r["healthy_at_ms"] for r in ok)
    lat = [r["spawn_to_healthy_ms"] for r in ok]
    first_spawn = min((r["spawn_at_ms"] for r in rows if r["spawn_at_ms"] is not None), default=0)
    span_s = (hs[-1] - first_spawn) / 1000 if hs else None
    # Best sustained window: most clones reaching healthy inside any 1 s (and 5 s) window.
    def best_window(w_ms):
        best, j = 0, 0
        for i in range(len(hs)):
            while hs[i] - hs[j] > w_ms:
                j += 1
            best = max(best, i - j + 1)
        return best
    per_s = {}
    for h in hs:
        per_s[int(h // 1000)] = per_s.get(int(h // 1000), 0) + 1
    stages = {}
    for name, start, end in STAGES:
        vals = []
        for r in ok:
            m = r["marks_ms"]
            a = 0.0 if start is None else mark(m, start)
            b = mark(m, end)
            if a is not None and b is not None:
                vals.append(b - a)
        stages[name] = {"p50": pct(vals, 0.5), "p99": pct(vals, 0.99), "n": len(vals)}
    host_rows = []
    hp = out / "host.tsv"
    if hp.exists():
        lines = hp.read_text().splitlines()
        hdr = lines[0].split("\t")
        host_rows = [dict(zip(hdr, ln.split("\t"))) for ln in lines[1:]]
    s = {
        "dir": str(out),
        "instance_type": host.get("instance_type"),
        "config": host.get("config"),
        "launched": len([r for r in rows if r["spawn_at_ms"] is not None]),
        "healthy": len(ok),
        "failed": len([r for r in rows if r["error"]]),
        "errors": sorted({r["error"] for r in rows if r["error"]})[:10],
        "span_s": round(span_s, 3) if span_s else None,
        "overall_rate_per_s": round(len(ok) / span_s, 1) if span_s else None,
        "best_1s_window": best_window(1000),
        "best_5s_window_rate": round(best_window(5000) / 5, 1),
        "spawn_to_healthy_ms": {"p50": pct(lat, 0.5), "p90": pct(lat, 0.9), "p99": pct(lat, 0.99), "max": pct(lat, 1)},
        "popen_ms": {"p50": pct([r["popen_ms"] for r in rows if r["popen_ms"]], 0.5),
                     "p99": pct([r["popen_ms"] for r in rows if r["popen_ms"]], 0.99)},
        "healthy_per_second": [per_s.get(k, 0) for k in range(0, (max(per_s) + 1) if per_s else 0)],
        "stages_ms": stages,
        "peak_load1": max((float(r["load1"]) for r in host_rows), default=None),
        "peak_procs_running": max((int(r["procs_running"]) for r in host_rows), default=None),
        "min_mem_avail_mib": min((int(r["mem_avail_mib"]) for r in host_rows), default=None),
        "verify": verify,
        "teardown_s": round(teardown_s, 2) if teardown_s is not None else None,
        "t0_wall": t0_wall,
    }
    if verify is not None:
        (out / "summary.json").write_text(json.dumps(s, indent=2) + "\n")
    return s


def print_summary(s):
    c = s["config"] or {}
    print(f"== {c.get('backend')}/{c.get('network')} N={c.get('count')} C={c.get('concurrency')} on {s['instance_type']}")
    print(f"   healthy {s['healthy']}/{s['launched']} failed {s['failed']} in {s['span_s']} s: "
          f"overall {s['overall_rate_per_s']}/s, best 1 s window {s['best_1s_window']}, best 5 s {s['best_5s_window_rate']}/s")
    lat = s["spawn_to_healthy_ms"]
    print(f"   spawn->healthy p50 {lat['p50']} p90 {lat['p90']} p99 {lat['p99']} max {lat['max']} ms; popen p50 {s['popen_ms']['p50']} ms")
    print(f"   peak load1 {s['peak_load1']} procs_running {s['peak_procs_running']} min MemAvailable {s['min_mem_avail_mib']} MiB")
    for k, v in s["stages_ms"].items():
        print(f"   {k:<16} p50 {v['p50']} p99 {v['p99']} (n={v['n']})")
    if s["errors"]:
        print(f"   errors: {s['errors']}")


def cmd_report(a):
    for d in a.dirs:
        host = Path(d) / "host.json"
        mode = json.loads(host.read_text()).get("config", {}).get("mode") if host.exists() else None
        if mode == "churn":
            # A churn run is judged again from its records, by the verdict as it is now.
            s = churn_from_records(d)
            if a.json:
                print(json.dumps(s))
            else:
                print_churn(s)
            continue
        p = Path(d) / "summary.json"
        s = json.loads(p.read_text()) if p.exists() else summarize(d)
        print_summary(s)


def build_parser():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("prep")
    p.add_argument("--fcvm", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--network", default="rootless", choices=["rootless", "bridged", "routed"])
    p.add_argument("--uffd", action="store_true", help="also start a memory server for UFFD clones")
    p.add_argument("--cpu", type=int, default=1)
    p.add_argument("--mem", type=int, default=256)
    p.add_argument("--image", default="docker.io/library/nginx:alpine")
    p.add_argument("--ipv6-prefix")
    p.add_argument("--sudo", action="store_true")
    p.add_argument("--max-clones", type=int, default=8192, help="FCVM_UFFD_MAX_CLONES for the serve")
    p.set_defaults(fn=cmd_prep)
    r = sub.add_parser("run")
    r.add_argument("--fcvm", required=True)
    r.add_argument("--prep", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--count", type=int, required=True)
    r.add_argument("--concurrency", type=positive_int, required=True)
    r.add_argument("--launchers", type=positive_int, default=4)
    r.add_argument("--timeout", type=float, default=180)
    r.add_argument("--rust-log", default="fcvm=info,health-monitor=info")
    r.add_argument("--pidstat-interval", type=int, default=2)
    r.add_argument("--fail-fast", action="store_true")
    r.add_argument("--hold", type=float, default=0, help="seconds to keep the clones up after the last is healthy")
    r.set_defaults(fn=cmd_run)
    c = sub.add_parser("churn")
    c.add_argument("--fcvm", required=True)
    c.add_argument("--prep", required=True)
    c.add_argument("--out", required=True)
    c.add_argument("--alive", type=positive_int, required=True, help="clones up or starting at once")
    c.add_argument("--lifetime", type=float, required=True, help="mean seconds a clone runs after it is healthy")
    c.add_argument("--lifetime-jitter", type=float, default=0.2,
                   help="each clone's lifetime is uniform in LIFETIME x (1 - J .. 1 + J)")
    c.add_argument("--concurrency", type=positive_int, required=True, help="clones starting at once, at most")
    c.add_argument("--duration", type=float, default=120, help="seconds of the measured window")
    c.add_argument("--warmup-limit", type=float, default=600, help="give up when starts are not all recycled by then")
    c.add_argument("--launchers", type=positive_int, default=4)
    c.add_argument("--timeout", type=float, default=30,
                   help="seconds a clone has to get healthy; must be under --duration")
    c.add_argument("--rust-log", default="fcvm=info,health-monitor=info")
    c.add_argument("--fail-fast", action="store_true")
    c.set_defaults(fn=cmd_churn)

    t = sub.add_parser("teardown")
    t.add_argument("--prep", required=True)
    t.set_defaults(fn=cmd_teardown)
    q = sub.add_parser("report")
    q.add_argument("dirs", nargs="+")
    q.add_argument("--json", action="store_true", help="print a churn directory's summary as JSON")
    q.set_defaults(fn=cmd_report)
    return ap


def main():
    a = build_parser().parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
