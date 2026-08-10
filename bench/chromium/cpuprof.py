#!/usr/bin/env python3
"""cpuprof.py — per-request CPU attribution for one fcvm shared-nothing request.

Wall-clock decomposition already exists (bench.sh). This measures where the CPU
goes, because THAT is what sets throughput at saturation.

Accounting bases, in order of authority:

 1. **Per-request leaf cgroup `cpu.stat`** — the ground truth. cgroup v2 base
    cpu stats accumulate `usage_usec/user_usec/system_usec` for every task that
    was ever in the subtree, INCLUDING tasks that have already exited. So the
    address-space reclaim a SIGKILLed Firecracker burns in `exit_mm()` lands
    here even though the process is gone before we can read its /proc entry.
    cgroup membership is inherited across fork/exec/setns, so every descendant
    of the request is covered by construction.

 2. **Per-thread /proc sampling** — the attribution. utime/stime/guest_time are
    monotonic per thread, so `max()` over samples = that thread's final value
    (as of its last observation). A thread that is born and dies entirely
    between two samples is missed; the difference against basis 1 is reported
    as `unattributed_ms` rather than hidden.

 3. **Whole-machine /proc/stat** — the reconciliation. Anything the cgroup does
    not see (kernel threads doing work on the request's behalf, softirq, IRQ)
    shows up as the gap between this and basis 1.

Guest vs host is exact, not estimated: /proc/<tid>/stat field 43 is `guest_time`
(ticks the thread spent executing guest code via KVM_RUN), and utime already
includes it. So per thread:
    guest      = guest_time
    user(vmm)  = utime - guest_time
    system     = stime
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time

HZ = os.sysconf("SC_CLK_TCK")
CGROOT = "/sys/fs/cgroup"


# --------------------------------------------------------------------------
# /proc parsing
# --------------------------------------------------------------------------
def read_task_stat(path):
    """Parse /proc/<pid>/stat or /proc/<pid>/task/<tid>/stat.

    comm (field 2) may contain spaces and parens, so split after the LAST ')'.
    Returns (comm, utime_ticks, stime_ticks, guest_ticks, state) or None.
    """
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None
    try:
        lp = data.index(b"(")
        rp = data.rindex(b")")
    except ValueError:
        return None
    comm = data[lp + 1:rp].decode("utf-8", "replace")
    rest = data[rp + 2:].split()
    if len(rest) < 41:
        return None
    # rest[0] is field 3 (state); field N -> rest[N-3]
    return (
        comm,
        int(rest[11]),   # 14 utime  (INCLUDES guest_time)
        int(rest[12]),   # 15 stime
        int(rest[40]),   # 43 guest_time — literal KVM_RUN time, the guest/host split
        rest[0].decode(),
        int(rest[7]),    # 10 minflt — minor faults: the file-backed arm's touched pages
        int(rest[9]),    # 12 majflt
    )


def read_proc_stat():
    """Whole-machine CPU from /proc/stat, in seconds."""
    with open("/proc/stat") as f:
        for line in f:
            if line.startswith("cpu "):
                v = [int(x) for x in line.split()[1:]]
                keys = ["user", "nice", "system", "idle", "iowait",
                        "irq", "softirq", "steal", "guest", "guest_nice"]
                d = {k: v[i] / HZ for i, k in enumerate(keys) if i < len(v)}
                # user already includes guest; nice already includes guest_nice
                d["busy"] = sum(d.get(k, 0.0) for k in
                                ["user", "nice", "system", "irq", "softirq", "steal"])
                d["total"] = d["busy"] + d.get("idle", 0.0) + d.get("iowait", 0.0)
                return d
    raise RuntimeError("no cpu line in /proc/stat")


def read_cpu_stat(cg):
    """cgroup v2 base cpu stats, in seconds. Present regardless of whether the
    cpu controller is enabled in subtree_control."""
    out = {}
    try:
        with open(os.path.join(cg, "cpu.stat")) as f:
            for line in f:
                k, _, v = line.partition(" ")
                if k in ("usage_usec", "user_usec", "system_usec", "nice_usec"):
                    out[k[:-5]] = int(v) / 1e6
    except OSError:
        return None
    return out


def cg_procs(cg):
    try:
        with open(os.path.join(cg, "cgroup.procs")) as f:
            return [int(x) for x in f.read().split()]
    except OSError:
        return []


# --------------------------------------------------------------------------
# sampler
# --------------------------------------------------------------------------
class Sampler(threading.Thread):
    """Walks the request cgroup + a fixed extra pid set at SAMPLE_S intervals.

    Keeps max(utime), max(stime), max(guest) per (pid, tid) — monotonic counters,
    so max == the last value we managed to observe before the thread vanished.
    """

    def __init__(self, cg, extra_pids, period):
        super().__init__(daemon=True)
        self.cg = cg
        self.extra_pids = list(extra_pids)
        self.period = period
        self.stop_flag = threading.Event()
        self.tasks = {}          # (pid, tid) -> [comm, ut, st, gt]
        self.cpu_timeline = []   # (t, usage, user, system)
        self.procs_seen = {}     # pid -> comm (first seen)
        self.proc_cgroup = {}    # pid -> /proc/<pid>/cgroup (first seen)
        self.nsamples = 0
        self.first_empty_t = None
        self.self_tid = None

    def _snap_pid(self, pid):
        tdir = "/proc/%d/task" % pid
        try:
            tids = os.listdir(tdir)
        except OSError:
            return
        for tid in tids:
            r = read_task_stat("%s/%s/stat" % (tdir, tid))
            if r is None:
                continue
            comm, ut, st, gt, _, mn, mj = r
            key = (pid, int(tid))
            cur = self.tasks.get(key)
            if cur is None:
                self.tasks[key] = [comm, ut, st, gt, mn, mj]
            else:
                # utime/stime/guest/minflt/majflt are monotonic per task, so max()
                # over samples == the last value observed before the task vanished.
                for i, v in ((1, ut), (2, st), (3, gt), (4, mn), (5, mj)):
                    if v > cur[i]:
                        cur[i] = v
                cur[0] = comm

    def run(self):
        self.self_tid = threading.get_native_id()
        while not self.stop_flag.is_set():
            t = time.time()
            cs = read_cpu_stat(self.cg)
            if cs:
                self.cpu_timeline.append(
                    (t, cs.get("usage", 0.0), cs.get("user", 0.0), cs.get("system", 0.0)))
            pids = cg_procs(self.cg)
            if not pids and self.first_empty_t is None and self.nsamples > 3:
                self.first_empty_t = t
            elif pids:
                self.first_empty_t = None
            for pid in pids:
                if pid not in self.procs_seen:
                    r = read_task_stat("/proc/%d/stat" % pid)
                    self.procs_seen[pid] = r[0] if r else "?"
                    # fcvm's --no-swap path tries to migrate Firecracker into
                    # /sys/fs/cgroup/fcvm.slice (common.rs::disable_cgroup_swap).
                    # If that ever succeeds, Firecracker LEAVES the request cgroup
                    # and the cgroup basis silently stops covering it. Record where
                    # each process actually sits so the claim is checked, not assumed.
                    try:
                        with open("/proc/%d/cgroup" % pid) as f:
                            self.proc_cgroup[pid] = f.read().strip()
                    except OSError:
                        pass
                self._snap_pid(pid)
            for pid in self.extra_pids:
                self._snap_pid(pid)
            self.nsamples += 1
            dt = self.period - (time.time() - t)
            if dt > 0:
                time.sleep(dt)


# --------------------------------------------------------------------------
# machine-wide per-process snapshot (start/end only — for the residual)
# --------------------------------------------------------------------------
def machine_snapshot():
    out = {}
    for e in os.listdir("/proc"):
        if not e.isdigit():
            continue
        pid = int(e)
        r = read_task_stat("/proc/%d/stat" % pid)
        if r is None:
            continue
        out[pid] = (r[0], r[1], r[2], r[3])   # comm, utime, stime, guest
    return out


# --------------------------------------------------------------------------
# cgroup helpers (sudo, because /sys/fs/cgroup is root-owned)
# --------------------------------------------------------------------------
def sh(cmd, check=True):
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError("cmd failed: %s\n%s" % (cmd, r.stderr))
    return r


def cg_create(path):
    sh("sudo -n mkdir -p %s" % path)


def cg_destroy(path):
    sh("sudo -n rmdir %s" % path, check=False)


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cg-base", required=True)
    ap.add_argument("--name", required=True)
    ap.add_argument("--fcvm", required=True)
    ap.add_argument("--memarm", required=True, choices=["file", "uffd"])
    ap.add_argument("--tag", required=True)
    ap.add_argument("--serve-pid", default="")
    ap.add_argument("--exec-cmd", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--period-ms", type=float, default=5.0)
    ap.add_argument("--timeout", type=int, default=120)
    ap.add_argument("--rust-log", default="fcvm=debug,uffd=debug")
    ap.add_argument("--vmlog", default="")
    ap.add_argument("--serve-log", default="")
    args = ap.parse_args()

    cg = os.path.join(args.cg_base, "req-" + args.name)
    cg_create(cg)

    extra = []
    if args.serve_pid:
        extra.append(int(args.serve_pid))

    rec = {"name": args.name, "memarm": args.memarm, "cgroup": cg}

    # ---- pre-request state -------------------------------------------------
    rec["loadavg_before"] = open("/proc/loadavg").read().split()[:3]
    serve_log_off = 0
    if args.serve_log and os.path.exists(args.serve_log):
        serve_log_off = os.path.getsize(args.serve_log)
    mach0 = machine_snapshot()
    ps0 = read_proc_stat()
    cs0 = read_cpu_stat(cg)
    self0 = read_task_stat("/proc/self/stat")

    sampler = Sampler(cg, extra, args.period_ms / 1000.0)
    sampler.start()
    time.sleep(0.02)  # let one baseline sample land

    src = ["--pid", args.serve_pid] if args.memarm == "uffd" else ["--snapshot", args.tag]

    # cgroup v2 delegation: migrating a task needs write access to the common
    # ancestor's cgroup.procs, which is root. bench.sh pays a `sudo` INSIDE the
    # request for that. Here the shell blocks on `read` until the parent has
    # migrated it as root, so the sudo cost sits entirely OUTSIDE the measured
    # window and the shell is already in the leaf before `exec`. Every descendant
    # (fcvm, firecracker, pasta, the unshare holder) then inherits the cgroup —
    # inheritance survives fork, exec, setuid and setns, so nothing can escape
    # the accounting by reparenting.
    inner = (
        "read _; "
        "exec timeout -k 10 %d env RUST_LOG=%s %s snapshot run %s "
        "--name %s --no-dirty-tracking --no-swap --exec %s"
    ) % (args.timeout, args.rust_log, args.fcvm, " ".join(src),
         args.name, shquote(args.exec_cmd))

    vmlog = open(args.vmlog, "wb") if args.vmlog else subprocess.DEVNULL

    proc = subprocess.Popen(["/bin/sh", "-c", inner], stdin=subprocess.PIPE,
                            stdout=vmlog, stderr=subprocess.STDOUT)
    sh("sudo -n sh -c 'echo %d > %s/cgroup.procs'" % (proc.pid, cg))
    if proc.pid not in cg_procs(cg):
        raise RuntimeError("request shell %d did not land in %s" % (proc.pid, cg))
    rec["cgroup_join_verified"] = True

    t0 = time.time()
    proc.stdin.write(b"\n")
    proc.stdin.flush()
    proc.stdin.close()
    rc = proc.wait()
    t_caller = time.time()      # caller unblocked: fcvm has exited
    # Read cpu.stat HERE rather than interpolating the sampler timeline: reading
    # it forces an rstat flush, so this is the exact cgroup CPU at the instant the
    # caller was released. Everything after it is teardown the caller did not wait for.
    cs_caller = read_cpu_stat(cg)

    # ---- teardown watch: wait until the cgroup is genuinely empty -----------
    t_gone = None
    deadline = t_caller + 30.0
    while time.time() < deadline:
        if not cg_procs(cg):
            t_gone = time.time()
            break
        time.sleep(0.001)
    if t_gone is None:
        t_gone = time.time()
        rec["teardown_timeout"] = True
    cs_gone = read_cpu_stat(cg)

    # a couple more samples so cpu.stat settles after the last task exits
    time.sleep(0.05)
    sampler.stop_flag.set()
    sampler.join(timeout=5)

    # Close the measurement window HERE, before the (possibly seconds-long)
    # fault-count wait below — otherwise idle background CPU during that wait
    # inflates the whole-machine basis and makes the residual look like a finding.
    cs1 = read_cpu_stat(cg)
    ps1 = read_proc_stat()
    mach1 = machine_snapshot()
    self1 = read_task_stat("/proc/self/stat")

    # The serve process logs `VM exited ... fault_count=N` only once the clone's
    # uffd fd is closed, i.e. strictly AFTER the clone's Firecracker is reaped.
    # Wait for that line rather than sleeping a fixed amount and hoping.
    if args.serve_log:
        rec["uffd_fault_count"] = None
        fc_deadline = time.time() + 5.0
        while time.time() < fc_deadline:
            tail = ""
            try:
                with open(args.serve_log, "rb") as f:
                    f.seek(serve_log_off)
                    tail = f.read().decode("utf-8", "replace")
            except OSError:
                pass
            m = re.findall(r"fault_count=(\d+)\s+elapsed_secs=\"?([\d.]+)", tail)
            if m:
                rec["uffd_fault_count"] = int(m[-1][0])
                rec["uffd_handler_elapsed_s"] = float(m[-1][1])
                rec["uffd_serve_log_tail"] = tail[-3000:]
                break
            time.sleep(0.005)

    # ---- cgroup basis ------------------------------------------------------
    def d(a, b, k):
        return (b.get(k, 0.0) - a.get(k, 0.0)) if (a and b) else None

    rec["rc"] = rc
    rec["wall_total_s"] = t_caller - t0
    rec["wall_to_gone_s"] = t_gone - t0
    rec["wall_teardown_after_caller_s"] = t_gone - t_caller
    rec["cgroup_cpu_s"] = {
        "usage": d(cs0, cs1, "usage"),
        "user": d(cs0, cs1, "user"),
        "system": d(cs0, cs1, "system"),
    }

    # CPU inside the cgroup, split at the exact moment the caller was released.
    rec["cgroup_cpu_blocking_s"] = {k: d(cs0, cs_caller, k)
                                    for k in ("usage", "user", "system")}
    rec["cgroup_cpu_after_release_s"] = {k: d(cs_caller, cs1, k)
                                         for k in ("usage", "user", "system")}
    # Sanity: everything after the cgroup emptied should be ~0. If it is not, the
    # 50 ms settle was too short and the teardown figure is truncated.
    rec["cgroup_cpu_after_gone_s"] = {k: d(cs_gone, cs1, k)
                                      for k in ("usage", "user", "system")}
    rec["cpu_timeline"] = [[round(t - t0, 4), round(u, 6), round(us, 6), round(sy, 6)]
                           for (t, u, us, sy) in sampler.cpu_timeline]

    # ---- per-thread attribution -------------------------------------------
    # Only tasks inside the cgroup; the serve pid is handled separately below.
    serve_pid = int(args.serve_pid) if args.serve_pid else None
    per_proc = {}
    for (pid, tid), (comm, ut, st, gt, mn, mj) in sampler.tasks.items():
        if serve_pid and pid == serve_pid:
            continue
        p = per_proc.setdefault(pid, {"comm": sampler.procs_seen.get(pid, "?"),
                                      "cgroup": sampler.proc_cgroup.get(pid, ""),
                                      "threads": {}})
        p["threads"][tid] = {"comm": comm, "user_s": ut / HZ,
                             "system_s": st / HZ, "guest_s": gt / HZ,
                             "minflt": mn, "majflt": mj}
    rec["processes"] = per_proc

    # ---- serve process (long-lived, OUTSIDE the request cgroup) ------------
    if serve_pid:
        sv = {"pid": serve_pid, "threads": {}}
        for (pid, tid), (comm, ut, st, gt, mn, mj) in sampler.tasks.items():
            if pid != serve_pid:
                continue
            sv["threads"][tid] = {"comm": comm, "user_s": ut / HZ,
                                  "system_s": st / HZ, "guest_s": gt / HZ,
                                  "minflt": mn, "majflt": mj}
        # delta over the window from the machine-wide snapshots (exact, since the
        # serve process spans the whole window)
        if serve_pid in mach0 and serve_pid in mach1:
            c0, u0, s0, g0 = mach0[serve_pid]
            c1, u1, s1, g1 = mach1[serve_pid]
            sv["delta_user_s"] = (u1 - u0) / HZ
            sv["delta_system_s"] = (s1 - s0) / HZ
        rec["serve"] = sv

    # ---- whole-machine reconciliation --------------------------------------
    rec["machine_cpu_s"] = {k: (ps1[k] - ps0[k]) for k in ps1 if k in ps0}
    deltas = []
    for pid, (comm, ut, st, gt) in mach1.items():
        if pid in mach0:
            c0, u0, s0, g0 = mach0[pid]
            du, ds = (ut - u0) / HZ, (st - s0) / HZ
        else:
            du, ds = ut / HZ, st / HZ
        if du + ds > 0.0005:
            deltas.append({"pid": pid, "comm": comm, "user_s": du, "system_s": ds})
    deltas.sort(key=lambda x: -(x["user_s"] + x["system_s"]))
    rec["machine_top"] = deltas[:40]

    # profiler's own overhead — it is NOT in the cgroup, so it pollutes the
    # whole-machine basis only, and must be declared rather than absorbed.
    if self0 and self1:
        rec["profiler_cpu_s"] = ((self1[1] - self0[1]) + (self1[2] - self0[2])) / HZ
    rec["sampler_samples"] = sampler.nsamples
    rec["sampler_period_ms"] = args.period_ms
    rec["loadavg_after"] = open("/proc/loadavg").read().split()[:3]

    with open(args.out, "w") as f:
        json.dump(rec, f)

    cg_destroy(cg)
    return 0


def shquote(s):
    return "'" + s.replace("'", "'\"'\"'") + "'"


if __name__ == "__main__":
    sys.exit(main())
