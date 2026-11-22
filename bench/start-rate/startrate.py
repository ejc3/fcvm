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
import json
import os
import resource
import selectors
import signal
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

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
        next(l for l in Path("/proc/meminfo").read_text().splitlines() if l.startswith("MemTotal")).split()[1]
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
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
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
    log = open(out / "baseline.log", "w")
    base_proc = subprocess.Popen(run, stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
    deadline = time.time() + 600
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
        sys.exit("baseline never became healthy in 600 s")
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
        serve_proc = subprocess.Popen(
            sfc + ["snapshot", "serve", snap], stdout=slog, stderr=subprocess.STDOUT, env=senv, start_new_session=True
        )
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
                    l for l in Path("/proc/meminfo").read_text().splitlines() if l.startswith("MemAvailable")
                ).split()[1]
                fh.write(
                    f"{now_ns()}\t{wall():.3f}\t{la[0]}\t{la[1]}\t{la[3]}\t{st.get('procs_running')}\t"
                    f"{st.get('procs_blocked')}\t{int(ma)//1024}\n"
                )
                fh.flush()
                self.stop.wait(1.0)


class Clone:
    __slots__ = ("i", "name", "proc", "log", "t_spawn", "spawn_ns", "marks", "t_healthy", "exit", "buf", "error")

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


def cmd_run(a):
    prep = json.loads(Path(a.prep).read_text())
    out = Path(a.out)
    (out / "clones").mkdir(parents=True, exist_ok=True)
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

    sampler = HostSampler(out / "host.tsv")
    sampler.start()
    pidstat = subprocess.Popen(
        ["pidstat", "-u", "-h", str(a.pidstat_interval)],
        stdout=open(out / "pidstat.log", "w"), stderr=subprocess.STDOUT, start_new_session=True,
    )

    sel = selectors.DefaultSelector()
    sel_lock = threading.Lock()
    clones = [Clone(i, f"{run_id}-c{i:05d}") for i in range(a.count)]
    pending = 0  # spawned, not yet healthy and not failed
    cv = threading.Condition()
    healthy_n = 0
    failed_n = 0
    t0 = now_ns()
    t0_wall = wall()

    def finish(c, err=None):
        nonlocal pending, failed_n, healthy_n
        with cv:
            if err is not None and c.error is None and c.t_healthy is None:
                c.error = err
                failed_n += 1
                pending -= 1
            elif err is None:
                healthy_n += 1
                pending -= 1
            cv.notify_all()

    def on_line(c, line, t):
        text = line.decode(errors="replace")
        c.log.write(f"{(t - c.spawn_ns)/1e6:10.2f} {text}\n")
        for m in EXTRA_MARKERS:
            if m in text and m not in c.marks:
                c.marks[m] = (t - c.spawn_ns) / 1e6
        if c.t_healthy is None and HEALTHY_MARKER in text:
            c.t_healthy = t
            finish(c)

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
                    if c.t_healthy is None:
                        rc = c.proc.wait()
                        c.exit = rc
                        finish(c, f"exited {rc} before healthy")
                    continue
                c.buf += chunk
                while b"\n" in c.buf:
                    line, _, c.buf = c.buf.partition(b"\n")
                    on_line(c, line, t)

    rt = threading.Thread(target=reader, name="reader", daemon=True)
    rt.start()

    next_i = 0
    next_lock = threading.Lock()

    def launcher():
        nonlocal next_i, pending
        while True:
            with cv:
                while pending >= a.concurrency:
                    cv.wait(0.5)
                with next_lock:
                    if next_i >= a.count:
                        return
                    c = clones[next_i]
                    next_i += 1
                pending += 1
            c.spawn_ns = now_ns()
            r = w = None
            try:
                c.log = open(out / "clones" / f"{c.name}.log", "w")
                r, w = os.pipe()
                os.set_blocking(r, False)
                c.proc = subprocess.Popen(
                    base_cmd + ["--name", c.name], stdout=w, stderr=w, stdin=subprocess.DEVNULL,
                    env=env, close_fds=True, start_new_session=True,
                )
            except OSError as e:
                # A launcher that dies here leaves the clone pending for ever, so the run
                # would wait on it instead of reporting it.
                for fd in (w, r):
                    if fd is not None:
                        os.close(fd)
                finish(c, f"spawn failed: {e}")
                continue
            c.t_spawn = (now_ns() - c.spawn_ns) / 1e6
            os.close(w)
            with sel_lock:
                sel.register(r, selectors.EVENT_READ, c)

    launchers = [threading.Thread(target=launcher, name=f"launcher-{k}", daemon=True) for k in range(a.launchers)]
    for th in launchers:
        th.start()

    deadline_extra = a.timeout * 1e9
    while True:
        with cv:
            done = healthy_n + failed_n
            if done >= a.count:
                break
            cv.wait(0.5)
        t = now_ns()
        for c in clones:
            if c.spawn_ns and c.t_healthy is None and c.error is None and t - c.spawn_ns > deadline_extra:
                finish(c, f"not healthy within {a.timeout}s")
        if a.fail_fast and failed_n:
            break
    for th in launchers:
        th.join(timeout=1)
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

    # Teardown: SIGTERM every clone, then wait.
    td0 = now_ns()
    for c in clones:
        if c.proc is not None and c.proc.poll() is None:
            kill(c.proc.pid, signal.SIGTERM, prep["sudo"])
    for c in clones:
        if c.proc is not None:
            try:
                c.proc.wait(timeout=max(1, 120 - (now_ns() - td0) / 1e9))
            except subprocess.TimeoutExpired:
                kill(c.proc.pid, signal.SIGKILL, prep["sudo"])
    teardown_s = (now_ns() - td0) / 1e9
    time.sleep(0.5)
    reader_stop.set()
    rt.join(timeout=5)
    sampler.stop.set()
    sampler.join(timeout=3)
    pidstat.send_signal(signal.SIGINT)
    try:
        pidstat.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pidstat.kill()

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
        "t0_mono_ns": t0, "all_done_mono_ns": t_all, "hold_end_mono_ns": t_hold_end, "teardown_end_mono_ns": now_ns(),
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
    rows = [json.loads(l) for l in (out / "samples.jsonl").read_text().splitlines() if l.strip()]
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
        host_rows = [dict(zip(hdr, l.split("\t"))) for l in lines[1:]]
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
    l = s["spawn_to_healthy_ms"]
    print(f"   spawn->healthy p50 {l['p50']} p90 {l['p90']} p99 {l['p99']} max {l['max']} ms; popen p50 {s['popen_ms']['p50']} ms")
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


def main():
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
    r.add_argument("--concurrency", type=int, required=True)
    r.add_argument("--launchers", type=int, default=4)
    r.add_argument("--timeout", type=float, default=180)
    r.add_argument("--rust-log", default="fcvm=info,health-monitor=info")
    r.add_argument("--pidstat-interval", type=int, default=2)
    r.add_argument("--fail-fast", action="store_true")
    r.add_argument("--hold", type=float, default=0, help="seconds to keep the clones up after the last is healthy")
    r.set_defaults(fn=cmd_run)
    t = sub.add_parser("teardown")
    t.add_argument("--prep", required=True)
    t.set_defaults(fn=cmd_teardown)
    q = sub.add_parser("report")
    q.add_argument("dirs", nargs="+")
    q.set_defaults(fn=cmd_report)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
