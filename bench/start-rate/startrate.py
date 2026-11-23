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


def host_facts(repo):
    store = sh(["stat", "-f", "-c", "%T", "/mnt/fcvm-btrfs"]).strip()
    src = ""
    for line in Path("/proc/mounts").read_text().splitlines():
        parts = line.split()
        if len(parts) > 2 and parts[1] == "/mnt/fcvm-btrfs":
            src = parts[0]
    backing = None
    if src.startswith("/dev/loop"):
        try:
            backing = Path(f"/sys/block/{Path(src).name}/loop/backing_file").read_text().strip()
        except OSError:
            backing = "unknown"
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
POOL_STATES = ("building", "ready", "leased", "resetting", "destroying")


def pool_stats(fcvm, sudo):
    """`fcvm netpool stats` as a dict, or None when no pool answers."""
    try:
        r = subprocess.run(fcvm_cmd(fcvm, sudo) + ["netpool", "stats"], text=True, capture_output=True)
        return json.loads(r.stdout) if r.returncode == 0 else None
    except (OSError, json.JSONDecodeError):
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


def churn_verdict(cfg, clones, alive, pool):
    """Whether a churn window is a sustained rate, and its figures.

    cfg: alive, lifetime_s, concurrency, window_start_s, window_end_s.
    clones: one dict per clone with spawn_s, healthy_s, stop_s, exit_s (seconds since the
      run's start, None when it did not happen), generation (None when the clone built its
      own namespace) and error.
    alive: (t_s, in_flight, running) samples. pool: (t_s, counters) samples.

    Each condition of bench/start-rate/README.md "Churn" is one entry of `conditions`; the
    verdict is ok only when every one holds. A condition whose records are missing fails.
    """
    w0, w1 = cfg["window_start_s"], cfg["window_end_s"]
    window = w1 - w0
    inside = lambda t: t is not None and w0 <= t < w1  # noqa: E731
    starts = [c for c in clones if inside(c["healthy_s"])]
    stops = [c for c in clones if c["healthy_s"] is not None and inside(c["exit_s"])]
    failed = [c for c in clones if c["error"] and (inside(c["spawn_s"]) or inside(c["exit_s"]))]
    alive_w = [x for x in alive if w0 <= x[0] < w1]
    pool_w = [x for x in pool if w0 <= x[0] < w1]
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
    cond("no failed clone", not failed, f"{len(failed)} failed" + (f", first: {failed[0]['error']}" if failed else ""))

    if alive_w:
        running = [x[2] for x in alive_w]
        head, tail = ends(alive_w, lambda x: x[2])
        mean = sum(running) / len(running)
        # The clones being replaced at any moment are a random few (rate x time to
        # healthy), so the count is never still; a swing of a fifth is a sawtooth.
        swing = max(5, 0.2 * cfg["alive"])
        flat = abs(tail - head) <= max(1, CHURN_FLAT * cfg["alive"]) and max(running) - min(running) <= swing
        cond("alive count flat", flat, f"mean {mean:.1f}, {min(running)} to {max(running)}, "
             f"first tenth {head:.1f}, last tenth {tail:.1f}, target {cfg['alive']}")
    else:
        mean = None
        cond("alive count flat", False, "no alive samples in the window")

    recycled = [c for c in starts if (c["generation"] or 0) >= 2]
    cond("every start in a recycled namespace", starts and len(recycled) == len(starts),
         f"{len(recycled)} of {len(starts)} starts had generation 2 or more")

    slots = builds = destroys = reset_p99 = None
    if len(pool_w) >= 2:
        first, last = pool_w[0][1], pool_w[-1][1]
        slots = sum(first[k] for k in POOL_STATES)
        head, tail = ends(pool_w, lambda x: x[1]["ready"])
        cond("free slots flat", abs(tail - head) <= CHURN_FLAT * slots,
             f"ready: first tenth {head:.1f}, last tenth {tail:.1f}, of {slots} slots")
        builds, destroys = last["builds"] - first["builds"], last["destroys"] - first["destroys"]
        cond("no namespace built or destroyed", builds == 0 and destroys == 0,
             f"{builds} built, {destroys} destroyed in the window")
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
            cond(name, False, "fewer than two samples of the pool's counters in the window")

    lat = [(c["healthy_s"] - c["spawn_s"]) * 1000 for c in starts if c["spawn_s"] is not None]
    return {
        "ok": all(c["ok"] for c in conditions),
        "conditions": conditions,
        "window_s": round(window, 2),
        "starts": len(starts),
        "stops": len(stops),
        "starts_per_s": round(rate, 2),
        "stops_per_s": round(len(stops) / window, 2) if window > 0 else 0.0,
        "recycled_fraction": round(len(recycled) / len(starts), 4) if starts else None,
        "alive_mean": round(mean, 1) if mean is not None else None,
        "spawn_to_healthy_ms": {"p50": pct(lat, 0.5), "p90": pct(lat, 0.9), "p99": pct(lat, 0.99), "max": pct(lat, 1.0)},
        "pool": {"slots": slots, "builds": builds, "destroys": destroys, "reset_p99_s": reset_p99},
    }


def lifetime_of(index, lifetime_s, jitter):
    """Clone `index`'s lifetime: uniform in lifetime_s x (1 - jitter .. 1 + jitter), the same
    for a given index in every run. With one lifetime for all, the clones a burst started
    together stop together for the whole run, and the alive count swings with them."""
    return lifetime_s * (1 + jitter * (2 * random.Random(index).random() - 1))


def recycled_only_since(clones, lifetime_s, now_s, quiet_s=2.0, min_starts=20):
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
    __slots__ = ("generation", "stop_ns", "exit_ns")

    def __init__(self, i, name):
        super().__init__(i, name)
        self.generation = None
        self.stop_ns = None
        self.exit_ns = None


def cmd_churn(a):
    """Starts and stops at one rate: each clone lives LIFETIME seconds after it is healthy
    and is then stopped, and a new one starts while fewer than ALIVE are up or starting."""
    need = max(CHURN_MIN_WINDOW_S, CHURN_MIN_LIFETIMES * a.lifetime)
    if a.duration < need:
        sys.exit(f"BLOCKED: --duration {a.duration} is under {need:.0f} s "
                 f"({CHURN_MIN_WINDOW_S} s and {CHURN_MIN_LIFETIMES} lifetimes)")
    prep = json.loads(Path(a.prep).read_text())
    if pool_stats(a.fcvm, prep["sudo"]) is None:
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
        "launchers": a.launchers,
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
                c.error = f"exited {c.exit} before healthy"
                flying.pop(c.i, None)
                st["in_flight"] -= 1
                st["failed"] += 1
            elif c.t_healthy is not None:
                st["running"] -= 1
                if c.stop_ns is None:
                    c.error = f"exited {c.exit} while running, before its stop"
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
            except OSError as e:
                for fd in (w, r):
                    if fd is not None:
                        os.close(fd)
                with cv:
                    c.done, c.error = True, f"spawn failed: {e}"
                    flying.pop(c.i, None)
                    st["in_flight"] -= 1
                    st["failed"] += 1
                    cv.notify_all()
                continue
            os.close(w)
            with sel_lock:
                sel.register(r, selectors.EVENT_READ, c)

    alive_samples, pool_samples = [], []
    samplers_stop = threading.Event()

    def alive_sampler():
        while not samplers_stop.wait(0.5):
            with cv:
                alive_samples.append((secs(now_ns()), st["in_flight"], st["running"]))

    def pool_sampler():
        with open(out / "pool.jsonl", "w") as fh:
            while not samplers_stop.wait(1.0):
                t = secs(now_ns())
                counters = pool_stats(a.fcvm, prep["sudo"])
                if counters is not None:
                    pool_samples.append((t, counters))
                    fh.write(json.dumps({"t_s": round(t, 3), **counters}) + "\n")
                    fh.flush()

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
                    c.done, c.error = True, f"not healthy within {a.timeout}s"
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
                warm = recycled_only_since(healthy, a.lifetime, secs(now_ns()))
            if warm:
                break
        w0 = now_ns()
        measured = warm and service(w0 + int(a.duration * 1e9))
        w1 = now_ns()

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
        while True:
            counters = pool_stats(a.fcvm, prep["sudo"])
            idle = counters is None or (counters["leased"] == 0 and counters["resetting"] == 0)
            if idle or now_ns() - drain0 > 300e9:
                break
            time.sleep(0.2)
        drain_s = (now_ns() - drain0) / 1e9
        time.sleep(0.5)
        stop_threads()

        rows = []
        with open(out / "samples.jsonl", "w") as fh:
            for c in clones:
                if c.log and not c.log.closed:
                    c.log.close()
                row = {
                    "i": c.i, "name": c.name, "spawn_s": secs(c.spawn_ns), "healthy_s": secs(c.t_healthy),
                    "stop_s": secs(c.stop_ns), "exit_s": secs(c.exit_ns), "generation": c.generation,
                    "error": c.error, "exit": c.exit,
                }
                rows.append(row)
                fh.write(json.dumps(row) + "\n")
        with open(out / "alive.tsv", "w") as fh:
            fh.write("t_s\tin_flight\trunning\n")
            for t, flying, running in alive_samples:
                fh.write(f"{t:.3f}\t{flying}\t{running}\n")
        window = {"alive": a.alive, "lifetime_s": a.lifetime, "concurrency": a.concurrency,
                  "window_start_s": secs(w0), "window_end_s": secs(w1)}
        verdict = churn_verdict(window, rows, alive_samples, pool_samples)
        verdict["conditions"].append({
            "name": "warm-up ended", "ok": bool(warm),
            "detail": f"{secs(w0):.1f} s until every start of 2 s took a recycled namespace"
                      if warm else f"starts were not all recycled within {a.warmup_limit} s",
        })
        verdict["conditions"].append({
            "name": "state files agree", "ok": bool(steady) and len(state_healthy) == len(steady),
            "detail": f"{len(state_healthy)} of {len(steady)} running clones healthy in fcvm ls",
        })
        verdict["ok"] = bool(measured) and all(c["ok"] for c in verdict["conditions"])
        warm_starts = [r for r in rows if r["healthy_s"] is not None and r["healthy_s"] < secs(w0)]
        summary = {
            "config": cfg, "t0_wall": t0_wall, **verdict,
            "warmup": {"s": round(secs(w0), 2), "starts": len(warm_starts),
                       "starts_per_s": round(len(warm_starts) / secs(w0), 2) if secs(w0) else None},
            "drain": {"clones": left, "teardown_s": round(teardown_s, 2), "until_pool_idle_s": round(drain_s, 2)},
        }
        (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print_churn(summary)
        if not summary["ok"]:
            print("FAILED: not a sustained rate; see the conditions in summary.json", file=sys.stderr)
            sys.exit(1)


def print_churn(s):
    print(f"window {s['window_s']} s: {s['starts_per_s']} starts/s, {s['stops_per_s']} stops/s, "
          f"alive {s['alive_mean']}, recycled {s['recycled_fraction']}, "
          f"spawn to healthy p50 {s['spawn_to_healthy_ms']['p50']} ms p99 {s['spawn_to_healthy_ms']['p99']} ms")
    print(f"warm-up {s['warmup']['s']} s at {s['warmup']['starts_per_s']} starts/s; "
          f"drain of {s['drain']['clones']} clones {s['drain']['teardown_s']} s, pool idle after {s['drain']['until_pool_idle_s']} s")
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
    c.add_argument("--timeout", type=float, default=180)
    c.add_argument("--rust-log", default="fcvm=info,health-monitor=info")
    c.add_argument("--fail-fast", action="store_true")
    c.set_defaults(fn=cmd_churn)

    t = sub.add_parser("teardown")
    t.add_argument("--prep", required=True)
    t.set_defaults(fn=cmd_teardown)
    q = sub.add_parser("report")
    q.add_argument("dirs", nargs="+")
    q.set_defaults(fn=cmd_report)
    return ap


def main():
    a = build_parser().parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
