#!/usr/bin/env python3
"""Matched-basis per-instance memory: fcvm clones vs host containers, same corpus.

Both sides are measured through the SAME two bases, over the SAME process set
definition, which is the error report.py's header records as having refuted the
first density claim (fcvm side summed PSS over firecracker processes only while
the container side measured a whole cgroup):

  cgroup   memory.current of the one cgroup that contains EVERY process of one
           instance -- on the fcvm side a leaf cgroup the launcher joins before
           exec'ing fcvm (so fcvm, firecracker, the namespace holder and pasta
           are all inside it); on the container side podman's own container
           cgroup.
  pss      PSS summed over exactly that cgroup's process set.

A third, attribution-free basis is recorded beside them: MemAvailable measured
on a quiesced box before the instances exist and again while they are held, so a
reader can check the attributed numbers against a machine-level delta.

Every instance renders one corpus page before it is sampled, so what is measured
is an instance that has done the work, not one that has merely booted.  The
sampling itself is report.py's `sample` subcommand, unmodified.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPORT = os.path.join(HERE, "report.py")
CDPDRIVE = os.path.join(HERE, "cdpdrive.py")


def log(msg):
    print(f"{time.strftime('%H:%M:%S')} {msg}", file=sys.stderr, flush=True)


def die(msg, code=2):
    log("BLOCKED: " + msg)
    sys.exit(code)


def sh(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def loadavg1():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def wait_quiet(limit, timeout_s):
    """Refuse a contaminated box; wait out a decaying average up to timeout_s."""
    deadline = time.monotonic() + timeout_s
    while True:
        la = loadavg1()
        if la < limit:
            return la
        if time.monotonic() >= deadline:
            die(f"1-min load {la} did not fall below {limit} within {timeout_s}s")
        log(f"settling: load={la}, need < {limit}")
        time.sleep(10)


def port_open(host, port, timeout=0.5):
    try:
        with socket.create_connection((host, port), timeout):
            return True
    except OSError:
        return False


# --------------------------------------------------------------------------
# sampling (report.py, unmodified, is the only thing that reads memory)
# --------------------------------------------------------------------------
def sample(extra: dict, cgroup_root=None, cgroup_prefix=None, podman_prefix=None,
           state_dir=None, name_prefix=None):
    cmd = [sys.executable, REPORT, "sample"]
    if cgroup_root:
        cmd += ["--cgroup-root", cgroup_root, "--cgroup-prefix", cgroup_prefix]
    if podman_prefix:
        cmd += ["--podman-prefix", podman_prefix]
    if state_dir:
        # The REFUTED basis, recorded on purpose: PSS over the firecracker
        # processes alone. It is never the headline; it sits in the record so a
        # reader can see how far off the old accounting was from the same run.
        cmd += ["--state-dir", state_dir, "--name-prefix", name_prefix]
    cmd += ["--extra", ", ".join(f'"{k}": {json.dumps(v)}' for k, v in extra.items())]
    r = sh(cmd)
    if r.returncode != 0:
        die(f"report.py sample failed: {r.stderr.strip()}")
    return json.loads(r.stdout)


def cgroup_cpu_usec(cg_path):
    """usage_usec of one cgroup.

    In cgroup v2 this accumulates the CPU time of every descendant, including
    processes that have already exited, so reading it after an instance is gone
    but before its leaf is removed gives the COMPLETE CPU cost of that instance:
    clone spawn, snapshot restore, the render, and teardown.
    """
    try:
        with open(os.path.join(cg_path, "cpu.stat")) as f:
            for line in f:
                if line.startswith("usage_usec"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        return None
    return None


def quiesce(seconds=4):
    subprocess.run(["sync"], check=False)
    time.sleep(seconds)


# --------------------------------------------------------------------------
# fcvm side
# --------------------------------------------------------------------------
class CgroupSet:
    """One slice with the memory controller delegated, one leaf per instance."""

    def __init__(self, base):
        self.base = base

    def setup(self):
        if sh(["sudo", "-n", "mkdir", "-p", self.base]).returncode != 0:
            die(f"cannot create {self.base}; per-instance cgroup accounting is the measurement")
        r = sh(["sudo", "-n", "sh", "-c", f"echo '+memory' > {self.base}/cgroup.subtree_control"])
        if r.returncode != 0:
            die(f"cannot delegate +memory to {self.base}: {r.stderr.strip()}")

    def leaf(self, name):
        path = f"{self.base}/{name}"
        if sh(["sudo", "-n", "mkdir", "-p", path]).returncode != 0:
            die(f"cannot create leaf cgroup {path}")
        return path

    def rm(self, name):
        sh(["sudo", "-n", "rmdir", f"{self.base}/{name}"])

    def rm_all(self):
        for name in sorted(os.listdir(self.base)) if os.path.isdir(self.base) else []:
            if os.path.isdir(os.path.join(self.base, name)):
                self.rm(name)
        sh(["sudo", "-n", "rmdir", self.base])


def spawn_in_cgroup(cg_path, argv, log_path, env=None):
    """Launch argv with the launching shell moved into cg_path first.

    cgroup membership is inherited across fork and survives the user-namespace
    and uid transitions fcvm makes, so every descendant lands in the same leaf.
    """
    script = f'sudo -n sh -c "echo $BASHPID > {cg_path}/cgroup.procs" || exit 90; exec "$0" "$@"'
    handle = open(log_path, "wb")
    return subprocess.Popen(["bash", "-c", script] + list(argv),
                            stdout=handle, stderr=subprocess.STDOUT,
                            env=env or os.environ.copy())


def state_files(state_dir):
    try:
        return [os.path.join(state_dir, f) for f in os.listdir(state_dir) if f.endswith(".json")]
    except OSError:
        return []


def find_clone_state(state_dir, name, deadline, proc):
    """The clone's own state record, once it carries a pid and a host-side IP."""
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return None
        for path in state_files(state_dir):
            try:
                with open(path) as f:
                    st = json.load(f)
            except (OSError, ValueError):
                continue
            if st.get("name") != name or not st.get("pid"):
                continue
            net = (st.get("config") or {}).get("network") or {}
            for key in ("loopback_ip", "host_ip", "guest_ip"):
                if net.get(key):
                    return st, net[key]
        time.sleep(0.05)
    return None


def clone_gone(state_dir, name):
    for path in state_files(state_dir):
        try:
            with open(path) as f:
                if json.load(f).get("name") == name:
                    return False
        except (OSError, ValueError):
            continue
    return True


def render(endpoint, url, timeout=120):
    r = sh([sys.executable, CDPDRIVE, endpoint, url, "--format", "jpeg"], timeout=timeout)
    return r.returncode == 0, (r.stdout or r.stderr)[-400:]


class FcvmSide:
    name = "fcvm-clone"

    def __init__(self, args, cg: CgroupSet, run_id):
        self.args = args
        self.cg = cg
        self.run_id = run_id
        self.serve_proc = None
        self.serve_pid = None

    def start_serve(self):
        cg = self.cg.leaf("serve-0")
        log_path = os.path.join(self.args.results, "logs", "serve.log")
        argv = [self.args.fcvm, "snapshot", "serve", self.args.tag,
                "--uffd-mode", self.args.uffd_mode,
                "--uffd-prefetch", self.args.uffd_prefetch]
        self.serve_proc = spawn_in_cgroup(cg, argv, log_path)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if self.serve_proc.poll() is not None:
                die(f"snapshot serve exited {self.serve_proc.returncode}; see {log_path}")
            try:
                text = open(log_path, errors="replace").read()
            except OSError:
                text = ""
            m = re.search(r"Serve PID: (\d+)", text)
            if m and "Waiting for VMs" in text:
                self.serve_pid = int(m.group(1))
                log(f"serve up: pid={self.serve_pid} ({self.args.uffd_mode}, prefetch={self.args.uffd_prefetch})")
                return
            time.sleep(0.25)
        die(f"snapshot serve never announced a pid; see {log_path}")

    def stop_serve(self):
        if not self.serve_proc:
            return
        self.serve_proc.terminate()
        try:
            self.serve_proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.serve_proc.kill()
            self.serve_proc.wait(timeout=30)
        self.serve_proc = None
        self.cg.rm("serve-0")

    def bring_up(self, n, cell_tag):
        """n clones, each restored, each having rendered one corpus page."""
        live = []
        for i in range(n):
            leaf = f"req-{cell_tag}-{i}"
            cgp = self.cg.leaf(leaf)
            name = f"mem-{self.run_id}-{cell_tag}-{i}"
            log_path = os.path.join(self.args.results, "logs", f"{name}.log")
            argv = [self.args.fcvm, "snapshot", "run", "--pid", str(self.serve_pid),
                    "--name", name, "--no-dirty-tracking", "--no-swap"]
            env = dict(os.environ, RUST_LOG="fcvm=info")
            proc = spawn_in_cgroup(cgp, argv, log_path, env)
            live.append({"i": i, "leaf": leaf, "name": name, "proc": proc, "log": log_path})
        # wait for every clone's published CDP port, then render one page on each
        for c in live:
            found = find_clone_state(self.args.state_dir, c["name"], time.monotonic() + 180, c["proc"])
            if not found:
                die(f"clone {c['name']} never published a usable state file; see {c['log']}")
            st, ip = found
            c["vm_id"] = st.get("vm_id")
            c["endpoint"] = f"{ip}:{self.args.cdp_port}"
            deadline = time.monotonic() + 180
            while not port_open(ip, self.args.cdp_port):
                if time.monotonic() >= deadline:
                    die(f"clone {c['name']} never answered on {c['endpoint']}; see {c['log']}")
                time.sleep(0.05)
        for c in live:
            url = self.args.urls[c["i"] % len(self.args.urls)]
            ok, out = render(c["endpoint"], url)
            if not ok:
                die(f"clone {c['name']} failed to render {url}: {out}")
            c["url"] = url
        return live

    def tear_down(self, live):
        for c in live:
            try:
                c["proc"].terminate()
            except ProcessLookupError:
                pass
        for c in live:
            try:
                c["proc"].wait(timeout=120)
            except subprocess.TimeoutExpired:
                c["proc"].kill()
                c["proc"].wait(timeout=60)
        deadline = time.monotonic() + 120
        for c in live:
            while not clone_gone(self.args.state_dir, c["name"]):
                if time.monotonic() >= deadline:
                    die(f"clone {c['name']} state file outlived its process; a later cell would be contaminated")
                time.sleep(0.2)
        for c in live:
            self.cg.rm(c["leaf"])

    def sample(self, extra, cell_tag):
        """The clones, plus the UFFD serve process measured on the same bases.

        The serve is SHARED by every clone, so it is never summed into the
        per-instance total; it is recorded beside it as the fixed cost of the
        arrangement, on the same two bases, and the fit's intercept is where a
        reader can see it again from the other direction."""
        rec = sample(extra, cgroup_root=self.cg.base, cgroup_prefix=f"req-{cell_tag}-",
                     state_dir=self.args.state_dir,
                     name_prefix=f"mem-{self.run_id}-{cell_tag}-")
        serve = sample({"_": 0}, cgroup_root=self.cg.base, cgroup_prefix="serve-")
        rec["serve_cgroup_kb"] = serve.get("clone_cgroup_kb", 0)
        rec["serve_pss_kb"] = serve.get("clone_pss_kb", 0)
        rec["serve_procs"] = serve.get("clone_procs", 0)
        return rec


# --------------------------------------------------------------------------
# container side
# --------------------------------------------------------------------------
# Every container keeps the image's DEFAULT ports (CDP 9222, pageserver 8000) in
# a network namespace of its own, and the host reaches its CDP through a
# published port. The obvious alternative, one shared host network namespace
# with BENCH_CDP_PORT per container, cannot work with this image: entry.sh warms
# Chromium with `render.py ... ` and render.py's --cdp-host defaults to
# 127.0.0.1:9222, so a container told to listen anywhere else fails its own
# warmup and never writes /run/bench-ready ("CDP discovery failed: Connection
# refused", 2026-08-30 18:04).
#
# slirp4netns:allow_host_loopback=true puts the host's loopback at 10.0.2.2
# inside the namespace, which is the same address the guest VM reaches the
# replay server on through pasta, so BENCH_RESOLVE_ALL_TO is the same value on
# both sides of the comparison.
#
# The render is then driven INSIDE the container with the image's own render.py,
# because Chromium binds the namespace's loopback and ignores
# --remote-debugging-address, so a published port reaches an address nothing is
# listening on (cdpdrive: 130 resolve attempts, no page target, 2026-08-30
# 18:10). What the memory measurement needs is that the instance rendered the
# page; where the driver runs does not change the instance's footprint, and the
# exec'd python has exited before anything is sampled. The single cputime
# container keeps the host-driven path, where one container can own the host's
# 9222 and the driver sits outside its cgroup, exactly as cdpdrive sits outside
# a clone's.
CONTAINER_NET = "slirp4netns:allow_host_loopback=true"
CONTAINER_RESOLVE_TO = "10.0.2.2"


class ContainerSide:
    name = "host-container"

    def __init__(self, args, run_id):
        self.args = args
        self.run_id = run_id

    def prefix(self, cell_tag):
        return f"cbmem-{cell_tag}-"

    def bring_up(self, n, cell_tag):
        live = []
        for i in range(n):
            name = f"{self.prefix(cell_tag)}{i}"
            cdp = self.args.container_cdp_base + i
            sh(["podman", "rm", "-f", name])
            r = sh(["podman", "run", "-d", "--name", name,
                    "--network", CONTAINER_NET,
                    "-e", f"BENCH_RESOLVE_ALL_TO={CONTAINER_RESOLVE_TO}",
                    self.args.image])
            if r.returncode != 0:
                die(f"podman run {name} failed: {r.stderr.strip()}")
            live.append({"i": i, "name": name, "cdp": cdp})
        for c in live:
            deadline = time.monotonic() + 180
            while sh(["podman", "exec", c["name"], "test", "-f", "/run/bench-ready"]).returncode != 0:
                if time.monotonic() >= deadline:
                    logs = sh(["podman", "logs", "--tail", "20", c["name"]]).stdout
                    die(f"container {c['name']} never became ready: {logs}")
                time.sleep(0.25)
        for c in live:
            url = self.args.urls[c["i"] % len(self.args.urls)]
            r = sh(["podman", "exec", c["name"], "python3", "/opt/bench/render.py", url,
                    "--out-prefix", "/tmp/mem", "--format", "jpeg"], timeout=180)
            if r.returncode != 0:
                die(f"container {c['name']} failed to render {url}: "
                    f"{(r.stdout + r.stderr)[-400:]}")
            c["url"] = url
        return live

    def tear_down(self, live):
        for c in live:
            sh(["podman", "rm", "-f", c["name"]])
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            names = sh(["podman", "ps", "-a", "--format", "{{.Names}}"]).stdout.split()
            if not [n for n in names if n.startswith("cbmem-")]:
                return
            time.sleep(0.5)
        die("containers outlived their removal; a later cell would be contaminated")

    def sample(self, extra, cell_tag):
        return sample(extra, podman_prefix=self.prefix(cell_tag))


# --------------------------------------------------------------------------
def slope_intercept(xs, ys):
    """Least squares over the concrete N grid: marginal cost and fixed cost."""
    n = len(xs)
    if n < 2:
        return None, None
    mx, my = statistics.mean(xs), statistics.mean(ys)
    denom = sum((x - mx) ** 2 for x in xs)
    if denom == 0:
        return None, None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / denom
    return slope, my - slope * mx


def run_cell(side, args, n, rep, out):
    cell_tag = f"{side.name.split('-')[0]}{n}r{rep}"
    common = {"side": side.name, "n": n, "rep": rep, "run_id": args.run_id,
              "snapshot": args.tag if side.name == "fcvm-clone" else None,
              "image": args.image, "uffd_mode": args.uffd_mode if side.name == "fcvm-clone" else None,
              "uffd_prefetch": args.uffd_prefetch if side.name == "fcvm-clone" else None}
    quiesce()
    pre = side.sample(dict(common, phase="pre"), cell_tag)
    out.write(json.dumps(pre) + "\n"); out.flush()
    log(f"{side.name} n={n} rep={rep}: bringing up")
    live = side.bring_up(n, cell_tag)
    time.sleep(args.settle)
    steady = []
    for k in range(3):
        rec = side.sample(dict(common, phase="steady", sample=k,
                               instances_expected=n,
                               mem_available_pre_kb=pre["mem_available_kb"]), cell_tag)
        out.write(json.dumps(rec) + "\n"); out.flush()
        steady.append(rec)
        time.sleep(1)
    log(f"{side.name} n={n} rep={rep}: "
        + " ".join(f"{k}={steady[1].get(k)}" for k in
                   ("clones", "clone_cgroup_kb", "clone_pss_kb", "pool_containers",
                    "pool_cgroup_kb", "pool_pss_kb") if k in steady[1]))
    # Fail closed on a cell that lost an instance. Per-instance memory divides
    # by the n that was ASKED for, so a cell measured with fewer live instances
    # than that would silently report a smaller number per instance.
    counted = steady[1].get("clones" if side.name == "fcvm-clone" else "pool_containers")
    if counted != n:
        die(f"{side.name} n={n} rep={rep}: {counted} instance(s) were accounted, not {n}; "
            "the per-instance figure from this cell would be wrong")
    side.tear_down(live)
    quiesce()
    post = side.sample(dict(common, phase="post"), cell_tag)
    out.write(json.dumps(post) + "\n"); out.flush()
    return {"side": side.name, "n": n, "rep": rep, "pre": pre, "steady": steady, "post": post,
            "urls": [c["url"] for c in live]}


def cell_values(cell):
    """The middle steady sample, on each basis, as MiB totals."""
    s = cell["steady"][1]
    if cell["side"] == "fcvm-clone":
        counted = s.get("clones", 0)
        cg = s.get("clone_cgroup_kb", 0) / 1024
        pss = s.get("clone_pss_kb", 0) / 1024
    else:
        counted = s.get("pool_containers", 0)
        cg = s.get("pool_cgroup_kb", 0) / 1024
        pss = s.get("pool_pss_kb", 0) / 1024
    avail_delta = (cell["pre"]["mem_available_kb"] - s["mem_available_kb"]) / 1024
    out = {"instances_counted": counted, "cgroup_mib": cg, "pss_mib": pss,
           "mem_available_delta_mib": avail_delta}
    if "fc_only_pss_kb" in s:
        out["refuted_fc_only_pss_mib"] = s["fc_only_pss_kb"] / 1024
    if "serve_pss_kb" in s:
        out["serve_pss_mib"] = s["serve_pss_kb"] / 1024
        out["serve_cgroup_mib"] = s.get("serve_cgroup_kb", 0) / 1024
    return out


def run_cputime(args, cg, fcvm_side, out_path):
    """CPU-seconds to produce one screenshot, on both sides, same corpus.

    This is a different metric from the wall-clock arms and is kept separate
    from them on purpose: it is the only quantity in this report that is the
    same KIND of number as a published CPU-time figure. It is still measured on
    a different machine from any such figure, so it licenses no conversion.

      fcvm    one clone per request, sequentially: spawn, restore, render one
              corpus page, tear down. Its leaf cgroup's usage_usec is read once
              the instance is gone, so it covers the whole lifecycle.
      host    one warm container, the same schedule of renders, its cgroup's
              usage_usec differenced across the run and divided by the renders.
              The container's idle cost between renders is inside that
              difference, which is what a warm pool actually pays.
    """
    res = {"reps": args.cputime_reps, "urls": args.urls, "fcvm": None, "host": None}
    if fcvm_side is not None:
        per = []
        # The UFFD serve is shared by every clone and sits OUTSIDE the clone's
        # cgroup, so it would be missing from a per-clone CPU figure. Its own
        # leaf is differenced across the whole loop and reported per request.
        serve_cpu_before = cgroup_cpu_usec(f"{cg.base}/serve-0")
        for i in range(args.cputime_reps):
            leaf = f"req-cpu-{i}"
            cgp = cg.leaf(leaf)
            name = f"mem-{args.run_id}-cpu-{i}"
            log_path = os.path.join(args.results, "logs", f"{name}.log")
            argv = [args.fcvm, "snapshot", "run", "--pid", str(fcvm_side.serve_pid),
                    "--name", name, "--no-dirty-tracking", "--no-swap"]
            proc = spawn_in_cgroup(cgp, argv, log_path, dict(os.environ, RUST_LOG="fcvm=info"))
            found = find_clone_state(args.state_dir, name, time.monotonic() + 180, proc)
            if not found:
                die(f"cputime clone {name} never published a state file; see {log_path}")
            _, ip = found
            deadline = time.monotonic() + 180
            while not port_open(ip, args.cdp_port):
                if time.monotonic() >= deadline:
                    die(f"cputime clone {name} never answered CDP; see {log_path}")
                time.sleep(0.05)
            t0 = time.monotonic()
            ok, out = render(f"{ip}:{args.cdp_port}", args.urls[i % len(args.urls)])
            wall = (time.monotonic() - t0) * 1000
            if not ok:
                die(f"cputime clone {name} failed to render: {out}")
            proc.terminate()
            try:
                proc.wait(timeout=120)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=60)
            gone_by = time.monotonic() + 120
            while not clone_gone(args.state_dir, name):
                if time.monotonic() >= gone_by:
                    die(f"cputime clone {name} state file outlived its process")
                time.sleep(0.2)
            usec = cgroup_cpu_usec(cgp)
            cg.rm(leaf)
            if usec is None:
                die(f"cputime clone {name} left no cpu.stat to read")
            per.append({"i": i, "url": args.urls[i % len(args.urls)],
                        "cpu_ms": usec / 1000.0, "render_wall_ms": round(wall, 1)})
            log(f"cputime fcvm {i + 1}/{args.cputime_reps}: {usec / 1000.0:.0f} ms CPU")
        serve_cpu_after = cgroup_cpu_usec(f"{cg.base}/serve-0")
        serve_per_req = None
        if serve_cpu_before is not None and serve_cpu_after is not None:
            serve_per_req = round((serve_cpu_after - serve_cpu_before) / 1000.0
                                  / args.cputime_reps, 1)
        vals = sorted(r["cpu_ms"] for r in per)
        res["fcvm"] = {"n": len(vals), "per_request_cpu_ms_p50": round(vals[len(vals) // 2], 1),
                       "serve_cpu_ms_per_request": serve_per_req,
                       "per_request_cpu_ms_mean": round(statistics.mean(vals), 1),
                       "min": round(vals[0], 1), "max": round(vals[-1], 1),
                       "basis": "leaf cgroup usage_usec over one whole clone lifecycle "
                                "(spawn, restore, render, teardown)",
                       "records": per}
    # host: one warm container, the same renders.
    #
    # A failure here is recorded, not fatal: the fcvm half above costs 42 clone
    # lifecycles and is already measured, and an unwritten cputime.json threw
    # all of it away once (2026-08-30 17:53, "cputime container never became
    # ready" after 42 clones).
    name = f"cbmem-cpu-{args.run_id[:8]}"
    sh(["podman", "rm", "-f", name])
    r = sh(["podman", "run", "-d", "--name", name, "--network", "host",
            "-e", f"BENCH_RESOLVE_ALL_TO={args.container_resolve_to}", args.image])
    if r.returncode != 0:
        res["host_error"] = f"podman run failed: {r.stderr.strip()}"
        with open(out_path, "w") as f:
            json.dump(res, f, indent=1)
        return {k: v for k, v in res.items() if k != "urls"}
    try:
        deadline = time.monotonic() + 180
        while True:
            if sh(["podman", "exec", name, "test", "-f", "/run/bench-ready"]).returncode == 0 \
                    and port_open("127.0.0.1", 9222):
                break
            if time.monotonic() >= deadline:
                logs = sh(["podman", "logs", "--tail", "40", name]).stdout
                state = sh(["podman", "inspect", "--format",
                            "{{.State.Status}} {{.State.ExitCode}}", name]).stdout.strip()
                res["host_error"] = ("container never became ready; state=%s logs=%s"
                                     % (state, logs[-1500:]))
                log("cputime host arm FAILED: " + res["host_error"])
                raise TimeoutError(res["host_error"])
            time.sleep(0.25)
        # An empty CgroupPath would make this "/sys/fs/cgroup", whose cpu.stat
        # exists and reports the WHOLE MACHINE: a fail-open that would publish a
        # per-render CPU figure with every other process on the box inside it.
        rel = sh(["podman", "inspect", "--format", "{{.State.CgroupPath}}", name]).stdout.strip()
        if not rel.startswith("/") or rel == "/":
            die(f"podman reports no container cgroup for {name} (got {rel!r}); "
                "a CPU figure read from the root cgroup would be the whole machine")
        cgp = "/sys/fs/cgroup" + rel
        if not os.path.isdir(cgp):
            die(f"container cgroup {cgp} does not exist; nothing can be attributed to it")
        # Two warmup renders first, outside the window, so first-touch costs
        # (fonts, code paths, page cache) are not charged to the measured reps.
        for i in range(2):
            render("127.0.0.1:9222", args.urls[i % len(args.urls)])
        before = cgroup_cpu_usec(cgp)
        if before is None:
            die(f"cputime container cgroup {cgp} has no cpu.stat")
        t0 = time.monotonic()
        for i in range(args.cputime_reps):
            ok, out = render("127.0.0.1:9222", args.urls[i % len(args.urls)])
            if not ok:
                die(f"cputime container failed to render: {out}")
        wall = (time.monotonic() - t0) * 1000
        after = cgroup_cpu_usec(cgp)
        res["host"] = {"n": args.cputime_reps,
                       "per_request_cpu_ms": round((after - before) / 1000.0 / args.cputime_reps, 1),
                       "total_cpu_ms": round((after - before) / 1000.0, 1),
                       "total_wall_ms": round(wall, 1),
                       "cgroup": cgp,
                       "basis": "container cgroup usage_usec differenced across the renders, "
                                "divided by the renders; includes the container's idle cost "
                                "between them"}
        log(f"cputime host: {res['host']['per_request_cpu_ms']} ms CPU per render")
    except TimeoutError:
        pass
    finally:
        sh(["podman", "rm", "-f", name])
    with open(out_path, "w") as f:
        json.dump(res, f, indent=1)
    return {k: v for k, v in res.items() if k != "urls"}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--results", required=True)
    p.add_argument("--tag", required=True, help="golden snapshot the clones restore from")
    p.add_argument("--image", default="localhost/chromium-bench-req")
    p.add_argument("--urls", required=True, help="comma-separated corpus")
    p.add_argument("--ns", default="1,2,4,8")
    p.add_argument("--reps", type=int, default=2)
    p.add_argument("--sides", default="fcvm,container")
    p.add_argument("--fcvm", default=os.path.join(os.path.dirname(os.path.dirname(HERE)), "target/release/fcvm"))
    p.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    p.add_argument("--cdp-port", type=int, default=9222)
    p.add_argument("--container-cdp-base", type=int, default=9300)
    p.add_argument("--container-http-base", type=int, default=8100)
    p.add_argument("--container-resolve-to", default="127.0.0.1")
    p.add_argument("--uffd-mode", default="minor")
    p.add_argument("--uffd-prefetch", default="on")
    p.add_argument("--settle", type=float, default=5.0)
    p.add_argument("--quiet-limit", type=float, default=1.0)
    p.add_argument("--quiet-wait", type=float, default=300.0)
    p.add_argument("--cputime-reps", type=int, default=0,
                   help="CPU-seconds per screenshot on both sides, over this many renders")
    args = p.parse_args()

    args.urls = [u.strip() for u in args.urls.split(",") if u.strip()]
    args.ns = [int(x) for x in args.ns.split(",") if x.strip()]
    args.state_dir = os.path.join(args.data_root, "state")
    args.run_id = uuid.uuid4().hex
    os.makedirs(os.path.join(args.results, "logs"), exist_ok=True)

    for tool in ("podman", "sudo", "bash"):
        if not shutil.which(tool):
            die(f"'{tool}' is missing; this harness cannot render a verdict without it")
    if not os.access(args.fcvm, os.X_OK):
        die(f"no fcvm binary at {args.fcvm}")
    snap = os.path.join(args.data_root, "snapshots", args.tag, "config.json")
    if "fcvm" in args.sides and not os.path.exists(snap):
        die(f"no golden snapshot at {snap}")
    if sh(["sudo", "-n", "true"]).returncode != 0:
        die("passwordless sudo is required to create the per-instance cgroups")
    stray = sh(["bash", "-c", "pgrep -a 'fcvm|firecracker' || true"]).stdout.strip()
    if stray:
        die(f"stray fcvm/firecracker processes would be charged to this measurement:\n{stray}")
    la = wait_quiet(args.quiet_limit, args.quiet_wait)

    meta = {"run_id": args.run_id, "started": time.time(), "loadavg1_at_start": la,
            "host_kernel": os.uname().release, "machine": os.uname().machine,
            "snapshot": args.tag, "image": args.image,
            "image_id": sh(["podman", "inspect", "--format", "{{.Id}}", args.image]).stdout.strip(),
            "fcvm_sha256": sh(["sha256sum", args.fcvm]).stdout.split()[0] if os.path.exists(args.fcvm) else None,
            "report_py_sha256": sh(["sha256sum", REPORT]).stdout.split()[0],
            "cdpdrive_sha256": sh(["sha256sum", CDPDRIVE]).stdout.split()[0],
            "urls": args.urls, "ns": args.ns, "reps": args.reps,
            "uffd_mode": args.uffd_mode, "uffd_prefetch": args.uffd_prefetch,
            "basis": "cgroup memory.current and PSS summed over EXACTLY that cgroup's "
                     "process set, on both sides: an fcvm clone's leaf cgroup holds fcvm, "
                     "firecracker, the namespace holder and pasta; a container's cgroup is "
                     "podman's own. MemAvailable delta from a quiesced pre-sample is recorded "
                     "beside them as an attribution-free check."}
    with open(os.path.join(args.results, "run.json"), "w") as f:
        json.dump(meta, f, indent=1)

    cg = CgroupSet(f"/sys/fs/cgroup/cbmem-{args.run_id}.slice")
    cells = []
    out = open(os.path.join(args.results, "samples.jsonl"), "a")
    fcvm_side = None
    try:
        sides = []
        if "fcvm" in args.sides:
            cg.setup()
            fcvm_side = FcvmSide(args, cg, args.run_id)
            fcvm_side.start_serve()
            sides.append(fcvm_side)
        if "container" in args.sides:
            sides.append(ContainerSide(args, args.run_id))
        for side in sides:
            for n in args.ns:
                for rep in range(1, args.reps + 1):
                    cells.append(run_cell(side, args, n, rep, out))
        if args.cputime_reps:
            cpu = run_cputime(args, cg, fcvm_side,
                              os.path.join(args.results, "cputime.json"))
            print(json.dumps(cpu, indent=1))
    finally:
        if fcvm_side:
            fcvm_side.stop_serve()
        cg.rm_all()
        out.close()

    summary = {"run_id": args.run_id, "meta": meta, "cells": []}
    for c in cells:
        v = cell_values(c)
        summary["cells"].append({"side": c["side"], "n": c["n"], "rep": c["rep"], **v,
                                 "cgroup_mib_per_instance": round(v["cgroup_mib"] / c["n"], 1),
                                 "pss_mib_per_instance": round(v["pss_mib"] / c["n"], 1),
                                 "mem_available_delta_mib_per_instance": round(
                                     v["mem_available_delta_mib"] / c["n"], 1)})
    summary["fits"] = {}
    for side in sorted({c["side"] for c in cells}):
        rows = [(c["n"], cell_values(c)) for c in cells if c["side"] == side]
        fit = {}
        for basis in ("cgroup_mib", "pss_mib", "mem_available_delta_mib"):
            xs = [n for n, _ in rows]
            ys = [v[basis] for _, v in rows]
            slope, intercept = slope_intercept(xs, ys)
            fit[basis] = {"marginal_mib_per_instance": None if slope is None else round(slope, 1),
                          "fixed_mib": None if intercept is None else round(intercept, 1),
                          "points": sorted(zip(xs, [round(y, 1) for y in ys]))}
        # Per instance AT EACH N, with the observed spread across repetitions.
        # A single average across the whole N grid would mix N=1, where the
        # fixed cost is charged entirely to one instance, with N=8, where it is
        # spread over eight, and read as neither.
        fit["per_n"] = {}
        for n in sorted({n for n, _ in rows}):
            at = [v for m, v in rows if m == n]
            cell = {"reps": len(at)}
            for basis in ("cgroup_mib", "pss_mib", "mem_available_delta_mib"):
                vals = [v[basis] / n for v in at]
                cell[basis] = {"mean": round(statistics.mean(vals), 1),
                               "min": round(min(vals), 1), "max": round(max(vals), 1)}
            for extra in ("serve_pss_mib", "serve_cgroup_mib", "refuted_fc_only_pss_mib"):
                vals = [v[extra] for v in at if extra in v]
                if vals:
                    cell[extra + "_total"] = {"mean": round(statistics.mean(vals), 1),
                                              "min": round(min(vals), 1),
                                              "max": round(max(vals), 1)}
            fit["per_n"][n] = cell
        summary["fits"][side] = fit
    with open(os.path.join(args.results, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    print(json.dumps(summary["fits"], indent=1))
    log(f"records in {args.results}")


if __name__ == "__main__":
    main()
