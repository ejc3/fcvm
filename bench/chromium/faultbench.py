#!/usr/bin/env python3
"""Ground truth: how many guest page faults does one Chromium render request take,
and what does each one cost, for every memory backend.

This measures, per request, on a matched basis across all four backends:

  1. FAULT COUNT
       UFFD arms  : exact, from an in-handler trace (one record per UFFD event).
       file arm   : `min_flt` delta on the firecracker process (kernel fault entries).
       ALL arms   : resident-page count of the guest-RAM VMAs, read from
                    /proc/<fc>/pagemap. This is the ONLY number that means the same
                    thing for every backend ("granules materialised"), so it is the
                    basis for the cross-backend comparison. For UFFD it must equal the
                    handler's fault count; for file-backed it does NOT equal min_flt,
                    and the ratio is the kernel's fault-around factor.
  2. BYTES faulted in vs guest RAM size (the real working set).
  3. PER-FAULT COST: serve-process CPU / fault count, plus the per-fault ioctl service
     time straight out of the trace (t_after - t_before around the UFFDIO_*).
  4. LOCALITY: every faulted offset is recorded, so runs/clustering are measurable.
  5. STABILITY ACROSS CLONES: Jaccard over the faulted-offset sets of repeated clones
     of the SAME snapshot -- the input that decides whether record-once-then-prefetch
     is viable.

Nothing here optimises anything. It establishes the numbers everything else rests on.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
import shutil
import signal
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
# Overridable so this can run against an instrumented build in a separate target dir
# without swapping the binary that a concurrently running bench.sh is invoking.
FCVM = Path(os.environ.get("FCVM", REPO_ROOT / "target" / "release" / "fcvm"))
STATE_DIR = Path("/mnt/fcvm-btrfs/state")
SNAP_DIR = Path("/mnt/fcvm-btrfs/snapshots")

PAGE = 4096
HUGE = 2 * 1024 * 1024

# A busy host moves every latency number in the run, and the effect is invisible in
# the results afterwards. One idle core's worth of background work is the ceiling.
MAX_START_LOAD = 1.0

# ---------------------------------------------------------------------------
# driver: reuse bench.sh's DRIVER_TEMPLATE verbatim so the workload measured here
# is byte-identical to the workload whose 241/295/324/904 ms file-vs-UFFD gaps
# motivated this measurement. Re-deriving it here would silently measure something
# else.
# ---------------------------------------------------------------------------


def driver_template() -> str:
    src = (SCRIPT_DIR / "bench.sh").read_text()
    m = re.search(r"^DRIVER_TEMPLATE='(.*?)'\s*$", src, re.S | re.M)
    if not m:
        sys.exit("could not extract DRIVER_TEMPLATE from bench.sh")
    return m.group(1)


def build_driver(url: str, fmt: str, qual: int, hold: float) -> str:
    host, port = re.match(r"https?://([^:/]+):(\d+)", url).groups()
    code = driver_template()
    for k, v in (
        ("@PHOST@", host),
        ("@PPORT@", port),
        ("@URL@", url),
        ("@FMT@", fmt),
        ("@QUAL@", str(qual)),
        ("@HOLD@", str(hold)),
    ):
        code = code.replace(k, v)
    return "python3 -c '" + code + "'"


# ---------------------------------------------------------------------------
# /proc helpers
# ---------------------------------------------------------------------------


def proc_stat(pid: int):
    """(min_flt, maj_flt, utime_ticks, stime_ticks) or None if the process is gone."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            data = f.read()
    except OSError:
        return None
    # comm can contain spaces and ')' -- split after the LAST ')'
    rest = data[data.rindex(b")") + 2 :].split()
    # rest[0] is field 3 (state); min_flt is field 10 -> rest[7]
    return (int(rest[7]), int(rest[9]), int(rest[11]), int(rest[12]))


def read_maps(pid: int):
    out = []
    try:
        with open(f"/proc/{pid}/maps") as f:
            for line in f:
                parts = line.split(maxsplit=5)
                a, b = parts[0].split("-")
                out.append(
                    {
                        "start": int(a, 16),
                        "end": int(b, 16),
                        "perms": parts[1],
                        "path": parts[5].strip() if len(parts) > 5 else "",
                    }
                )
    except OSError:
        return []
    return out


def guest_ram_vmas(pid: int, want_bytes: int):
    """The VMAs that hold guest RAM, for every backend.

    file arm  : MAP_PRIVATE of <snap>/memory.bin
    copy arm  : anonymous
    minor arm : MAP_PRIVATE of a memfd (shmem or hugetlb)

    Selected by size, not by name, so one rule covers all three. Firecracker may split
    guest RAM into several regions, so every large VMA is returned; the caller records
    the total and whether it matches the configured guest RAM
    (`vma_total_matches_guest`), because a mismatch means this snapshot is not
    comparable with the other arms.
    """
    vmas = [v for v in read_maps(pid) if (v["end"] - v["start"]) >= 64 * 1024 * 1024]
    vmas = [v for v in vmas if "memory.bin" in v["path"] or "memfd" in v["path"] or v["path"] == ""]
    vmas.sort(key=lambda v: -(v["end"] - v["start"]))
    keep, total = [], 0
    for v in vmas:
        if total >= want_bytes:
            break
        keep.append(v)
        total += v["end"] - v["start"]
    return keep


def pagemap_resident(pid: int, vmas):
    """Resident 4KiB page offsets per VMA, straight from /proc/<pid>/pagemap.

    Bit 63 = present. PFNs are zeroed for unprivileged readers but the present bit
    is not, which is all this needs. Works identically for file-backed, anonymous
    and memfd mappings -- that is the point: it is the one comparable basis.
    """
    res = []
    try:
        fd = os.open(f"/proc/{pid}/pagemap", os.O_RDONLY)
    except OSError:
        return None
    try:
        for v in vmas:
            npages = (v["end"] - v["start"]) // PAGE
            os.lseek(fd, (v["start"] // PAGE) * 8, os.SEEK_SET)
            buf = b""
            remaining = npages * 8
            while remaining > 0:
                chunk = os.read(fd, min(remaining, 1 << 22))
                if not chunk:
                    break
                buf += chunk
                remaining -= len(chunk)
            present = []
            n = len(buf) // 8
            for i, word in enumerate(struct.unpack_from(f"<{n}Q", buf)):
                if word & (1 << 63):
                    present.append(i)
            res.append({"start": v["start"], "path": v["path"], "npages": npages, "present": present})
    finally:
        os.close(fd)
    return res


def smaps_rss(pid: int, vmas):
    """Rss/Pss per selected VMA (bytes). Cross-check on pagemap."""
    want = {v["start"] for v in vmas}
    out = {}
    try:
        with open(f"/proc/{pid}/smaps") as f:
            cur = None
            for line in f:
                if "-" in line.split()[0] and ":" not in line.split()[0]:
                    start = int(line.split("-")[0], 16)
                    cur = start if start in want else None
                    if cur is not None:
                        out[cur] = {}
                elif cur is not None:
                    k, _, v = line.partition(":")
                    if k in ("Rss", "Pss", "Private_Dirty", "Shared_Clean", "Private_Clean"):
                        out[cur][k] = int(v.split()[0]) * 1024
    except OSError:
        return {}
    return out


# ---------------------------------------------------------------------------
# ftrace: kvm:kvm_guest_fault
#
# This is the ONE fault instrument that means the same thing for every backend.
# `min_flt` does NOT work here: a guest memory access traps to KVM's
# user_mem_abort(), which resolves the host page via get_user_pages(), and
# mm_account_fault() skips accounting when regs == NULL (the GUP case). So guest
# RAM faults never reach the firecracker process's min_flt counter. The arm64
# tracepoint fires once per stage-2 abort and carries the faulting guest physical
# address, which is exactly one "guest page fault" -- and exactly the event that,
# on the UFFD arms, turns into a userspace round trip.
# ---------------------------------------------------------------------------

FTRACE = Path("/sys/kernel/tracing/instances/faultbench")


def _sudo_write(path, value):
    subprocess.run(["sudo", "sh", "-c", f"echo {value} > {path}"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def ftrace_setup(bufsize_kb=8192):
    subprocess.run(["sudo", "mkdir", "-p", str(FTRACE)], check=True)
    _sudo_write(FTRACE / "tracing_on", 0)
    _sudo_write(FTRACE / "buffer_size_kb", bufsize_kb)
    _sudo_write(FTRACE / "events/kvm/kvm_guest_fault/enable", 1)
    _sudo_write(FTRACE / "trace", "")


def ftrace_teardown():
    subprocess.run(["sudo", "sh", "-c",
                    f"echo 0 > {FTRACE}/tracing_on; echo 0 > {FTRACE}/events/kvm/kvm_guest_fault/enable; "
                    f"rmdir {FTRACE} 2>/dev/null || true"], check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def ftrace_start():
    _sudo_write(FTRACE / "trace", "")
    _sudo_write(FTRACE / "tracing_on", 1)


def ftrace_stop_dump(dest: Path):
    _sudo_write(FTRACE / "tracing_on", 0)
    with open(dest, "wb") as f:
        subprocess.run(["sudo", "cat", str(FTRACE / "trace")], stdout=f, check=False)
    # overflow check: a dropped event silently truncates the fault set, which would
    # understate every downstream number. Surface it rather than average it away.
    # Every CPU has its own ring buffer and guest faults arrive on the vCPU threads,
    # which are not pinned to cpu0. Reading cpu0 alone reports "no drops" while another
    # CPU's buffer overran, which silently truncates the fault set.
    st = subprocess.run(
        ["sudo", "sh", "-c",
         f'for s in {FTRACE}/per_cpu/cpu*/stats; do echo "== $s"; cat "$s"; done'],
        capture_output=True, text=True, check=False)
    return {"raw": st.stdout, "lost": ftrace_lost_events(st.stdout)}


def ftrace_lost_events(stats_text):
    """Total `overrun` + `dropped events` across every CPU's ring buffer.

    Non-zero means the fault set is incomplete, so every count derived from it is a
    lower bound rather than a measurement.
    """
    lost = 0
    for line in stats_text.splitlines():
        key, _, value = line.partition(":")
        if key.strip() in ("overrun", "dropped events"):
            try:
                lost += int(value.strip())
            except ValueError:
                continue
    return lost


# ---------------------------------------------------------------------------
# fcvm process discovery
# ---------------------------------------------------------------------------


def firecracker_pids():
    out = set()
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/comm") as f:
                if f.read().strip() == "firecracker":
                    out.add(int(d))
        except OSError:
            pass
    return out


def serve_pid_for(tag: str, umode: str):
    """The pid of the serve for this tag AND this UFFD mode.

    The tag alone is not an identity: the same snapshot is served in both `copy` and
    `minor` mode, and matching on the tag would return whichever serve happened to be
    running. The harness would then sample serve CPU on a process it did not start and
    later signal it, so both the measurement and the teardown would land on a stranger.
    `uffd_mode` is written into the serve's own state entry by `snapshot serve`.
    """
    for p in STATE_DIR.glob("*.json"):
        try:
            st = json.loads(p.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        cfg = st.get("config") or {}
        if (cfg.get("process_type") == "serve"
                and cfg.get("snapshot_name") == tag
                and (cfg.get("uffd_mode") or "copy") == umode):
            return st.get("pid")
    return None


# ---------------------------------------------------------------------------
# one request, fully instrumented
# ---------------------------------------------------------------------------


class Sampler(threading.Thread):
    """Follows the clone's firecracker process for the life of the request.

    High frequency on /proc/<fc>/stat (one small read -> min_flt/maj_flt/CPU), and
    pagemap+smaps only at the hold point, where the render is finished and the
    process is quiescent enough for a 4 MiB pagemap walk not to perturb anything.
    """

    def __init__(self, pre_pids, guest_bytes, hold_evt, stop_evt):
        super().__init__(daemon=True)
        self.pre_pids = pre_pids
        self.guest_bytes = guest_bytes
        self.hold_evt = hold_evt
        self.stop_evt = stop_evt
        self.fc_pid = None
        self.series = []          # (t_wall, min_flt, maj_flt, utime, stime)
        self.snapshot = None      # pagemap/smaps at hold
        # Every thread this firecracker ran during the request. kvm:kvm_guest_fault is
        # emitted in vCPU thread context, so its ftrace pid is a TID, not the process
        # id: without this set the analyzer cannot tell this VM's faults from another
        # VM's in a host-wide trace.
        self.fc_tids = set()
        self.first_seen = None
        self.last = None
        self.error = None

    def run(self):
        try:
            self._run()
        except Exception as e:  # a sampler crash must not look like a clean measurement
            self.error = repr(e)

    def _run(self):
        t_dead = time.time() + 120
        while time.time() < t_dead and not self.stop_evt.is_set():
            new = firecracker_pids() - self.pre_pids
            if new:
                self.fc_pid = sorted(new)[0]
                self.first_seen = time.time()
                break
            time.sleep(0.001)
        if self.fc_pid is None:
            return

        pid = self.fc_pid
        took_snapshot = False
        while not self.stop_evt.is_set():
            st = proc_stat(pid)
            if st is None:
                break
            try:
                self.fc_tids.update(int(t) for t in os.listdir(f"/proc/{pid}/task"))
            except OSError:
                pass  # the process exited between the stat and the task listing
            self.series.append((time.time(), *st))
            self.last = st
            if self.hold_evt.is_set() and not took_snapshot:
                # give the render a moment to settle, then take the one expensive read
                time.sleep(0.25)
                vmas = guest_ram_vmas(pid, self.guest_bytes)
                pm = pagemap_resident(pid, vmas)
                sm = smaps_rss(pid, vmas)
                st2 = proc_stat(pid)
                # The selector takes any anonymous VMA of 64 MiB or more, so a large
                # non-guest mapping can displace a real guest-RAM region. When the
                # total does not match, pagemap and smaps are measuring a different
                # accounting base than the other arms and must not be compared.
                vma_total = sum(v["end"] - v["start"] for v in vmas)
                self.snapshot = {
                    "t": time.time(),
                    "vma_total_bytes": vma_total,
                    "vma_total_matches_guest": vma_total == self.guest_bytes,
                    "vmas": [{"start": v["start"], "size": v["end"] - v["start"], "path": v["path"]} for v in vmas],
                    "pagemap": pm,
                    "smaps": {str(k): v for k, v in sm.items()},
                    "stat_at_snapshot": st2,
                }
                took_snapshot = True
            time.sleep(0.002)


def run_request(*, cell, tag, memarm, umode, serve_pid, url, name, out_dir, hold,
                fmt="jpeg", qual=80, guest_bytes=2 << 30, req_timeout=120, ftrace=False):
    log_path = out_dir / "requests" / f"{name}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)

    execcmd = build_driver(url, fmt, qual, hold)
    src = ["--pid", str(serve_pid)] if memarm == "uffd" else ["--snapshot", tag]
    cmd = [
        "timeout", "-k", "10", str(req_timeout),
        str(FCVM), "snapshot", "run", *src, "--name", name,
        "--no-dirty-tracking", "--no-swap", "--exec", execcmd,
    ]
    env = dict(os.environ, RUST_LOG="fcvm=debug")

    pre = firecracker_pids()
    hold_evt, stop_evt = threading.Event(), threading.Event()
    serve_before = proc_stat(serve_pid) if memarm == "uffd" else None
    sampler = Sampler(pre, guest_bytes, hold_evt, stop_evt)
    sampler.start()

    if ftrace:
        ftrace_start()
    t0 = time.time()
    lines = []
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, bufsize=1, env=env)
    marks = {}
    with open(log_path, "w") as lf:
        lf.write(f"{t0:.6f} BENCH_T0\n")
        for line in proc.stdout:
            ts = time.time()
            lf.write(f"{ts:.6f} {line}")
            lines.append((ts, line.rstrip("\n")))
            for mk in ("BENCH_EXEC_UP", "BENCH_NET_UP", "RENDER_OK", "BENCH_HOLD_START"):
                if mk in line and mk not in marks:
                    marks[mk] = ts
            if "BENCH_HOLD_START" in line:
                hold_evt.set()
        rc = proc.wait()
    t1 = time.time()
    stop_evt.set()
    ftrace_stats = None
    kvm_trace_path = None
    if ftrace:
        kvm_trace_path = out_dir / "traces" / "kvm" / f"{name}.trace"
        kvm_trace_path.parent.mkdir(parents=True, exist_ok=True)
        ftrace_stats = ftrace_stop_dump(kvm_trace_path)
    sampler.join(timeout=10)
    serve_after = proc_stat(serve_pid) if memarm == "uffd" else None

    rec = {
        "cell": cell, "name": name, "url": url, "memarm": memarm, "umode": umode,
        "tag": tag, "hold_s": hold, "rc": rc,
        "t0": t0, "t1": t1, "wall_ms": (t1 - t0) * 1000.0,
        "marks": marks,
        "fc_pid": sampler.fc_pid,
        "fc_tids": sorted(sampler.fc_tids),
        "fc_first_seen": sampler.first_seen,
        "sampler_error": sampler.error,
        "stat_series": sampler.series,
        "hold_snapshot": sampler.snapshot,
        "serve_pid": serve_pid if memarm == "uffd" else None,
        "serve_stat_before": serve_before,
        "serve_stat_after": serve_after,
        "ftrace": bool(ftrace),
        "kvm_trace": str(kvm_trace_path) if kvm_trace_path else None,
        "ftrace_stats": ftrace_stats,
        "ftrace_lost_events": (ftrace_stats or {}).get("lost"),
        "render_line": next((ln for _, ln in lines if ln.startswith("RENDER_OK")), None),
    }
    return rec


# ---------------------------------------------------------------------------
# serve lifecycle
# ---------------------------------------------------------------------------


def start_serve(tag, umode, trace_dir, log_dir):
    args = [str(FCVM), "snapshot", "serve", tag]
    if umode == "minor":
        args += ["--uffd-mode", "minor"]
    env = dict(os.environ, RUST_LOG="fcvm=debug", FCVM_UFFD_FAULT_TRACE=str(trace_dir))
    log_dir.mkdir(parents=True, exist_ok=True)
    lf = open(log_dir / f"serve-{tag}-{umode}.log", "w")
    proc = subprocess.Popen(args, stdout=lf, stderr=subprocess.STDOUT, env=env)
    t_dead = time.time() + 60
    while time.time() < t_dead:
        pid = serve_pid_for(tag, umode)
        # Only the process this call spawned may be measured or signalled later.
        if pid == proc.pid and Path(f"/proc/{pid}").exists():
            return proc, pid
        if pid is not None and pid != proc.pid:
            proc.kill()
            proc.wait()
            sys.exit(
                f"serve for {tag} ({umode}) is already running as pid {pid}; this run "
                f"spawned {proc.pid} and will not measure or signal a process it does "
                f"not own"
            )
        if proc.poll() is not None:
            sys.exit(f"serve for {tag} ({umode}) exited early rc={proc.returncode}")
        time.sleep(0.2)
    sys.exit(f"serve for {tag} ({umode}) did not register in state")


def stop_serve(proc, pid, tag=None, umode=None):
    """Stop a serve and prove it is gone: process reaped, state entry removed.

    A serve that outlives its cell keeps its UFFD socket and state entry, so the next
    cell's `start_serve` can bind to it and measure the wrong process. Returning
    silently after a `kill()` that was never reaped leaves a zombie holding the state
    entry, which looks exactly like a live serve to `serve_pid_for`.
    """
    if proc is None:
        return
    try:
        proc.send_signal(signal.SIGTERM)
    except ProcessLookupError:
        pass
    t_dead = time.time() + 90
    while time.time() < t_dead:
        if proc.poll() is not None and serve_pid_for_pid_gone(pid):
            return
        time.sleep(0.2)
    try:
        proc.kill()
    except ProcessLookupError:
        pass
    # SIGKILL is not instant and an unreaped corpse still owns its pid.
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        raise SystemExit(
            f"[faultbench] serve pid {pid} did not die within 10s of SIGKILL; refusing "
            f"to continue with a serve still holding its UFFD socket"
        )
    t_dead = time.time() + 10
    while time.time() < t_dead and not serve_pid_for_pid_gone(pid):
        time.sleep(0.2)
    if not serve_pid_for_pid_gone(pid):
        raise SystemExit(
            f"[faultbench] serve pid {pid} was killed but /proc/{pid} still exists 10s "
            f"later; something is holding the process"
        )
    if tag is not None and serve_pid_for(tag, umode) is not None:
        raise SystemExit(
            f"[faultbench] serve state entry for {tag} ({umode}) outlived pid {pid}; a "
            f"later cell would bind to a serve this run already stopped"
        )


def serve_pid_for_pid_gone(pid):
    return not Path(f"/proc/{pid}").exists()


def require_clones_gone(prefix, timeout_s, context):
    """Stop the run if a clone outlived its request.

    Serial isolation is what makes every later number attributable. A clone still up
    keeps faulting and burning CPU while the NEXT request is measured, so its cost
    lands on the wrong VM in both the per-request and the serve-CPU figures. The
    records already written stay valid, which is why this stops rather than skips.
    """
    if not wait_clones_gone(prefix, timeout_s):
        raise SystemExit(
            f"[faultbench] clone cleanup for {prefix} did not finish within "
            f"{timeout_s}s after {context}; refusing to measure the next request "
            f"alongside a surviving clone"
        )


def require_fresh_out_dir(out):
    """Refuse an output directory that already holds a run.

    requests.jsonl is APPENDED to and traces are matched by mtime, so reusing a
    directory blends two runs, possibly taken with different arguments, into one
    analysis. Absent or empty is fine; anything already in it is not.
    """
    if out.exists() and any(out.iterdir()):
        raise SystemExit(
            f"[faultbench] --out {out} is not empty; a run appends to requests.jsonl "
            f"and matches traces by mtime, so reusing it would blend two runs into one "
            f"analysis. Name a fresh directory."
        )


def wait_clones_gone(prefix, timeout=120):
    t_dead = time.time() + timeout
    while time.time() < t_dead:
        alive = 0
        for p in STATE_DIR.glob("*.json"):
            try:
                st = json.loads(p.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            cfg = st.get("config") or {}
            if cfg.get("process_type") == "clone" and (cfg.get("name") or "").startswith(prefix):
                alive += 1
        if alive == 0:
            return True
        time.sleep(0.5)
    return False


def build_schedule(cells, pages, reps, warmup, seed):
    """The (cell, page, rep) request list, shuffled from a recorded seed.

    Walking cells in argument order attributes any host drift during the run entirely
    to whichever cell ran last, which is indistinguishable from a real difference
    between the arms. Shuffling spreads drift across all of them.

    Warmups stay in front, and in cell order: they exist to reach steady state, and
    interleaving them would leave a cell measured before it is warm.
    """
    warm = [(c, p, i, True)
            for c in cells for p in pages for i in range(1, warmup + 1)]
    measured = [(c, p, warmup + i, False)
                for c in cells for p in pages for i in range(1, reps + 1)]
    random.Random(seed).shuffle(measured)
    return warm + measured


SETTLE_POLL_S = 5.0


def settle_wait_secs():
    """The bounded quiet-gate settle window, shared knob with reqbench/hostcdp."""
    raw = os.environ.get("SETTLE_WAIT_SECS", "0")
    try:
        value = float(raw)
    except ValueError:
        raise SystemExit(
            f"[faultbench] SETTLE_WAIT_SECS must be a number of seconds, got {raw!r}"
        )
    # float() accepts 'nan' and 'inf', and nan also slips the negative check
    # (nan compares false to everything); either would make the bounded
    # window unbounded, so require a finite value.
    if not math.isfinite(value):
        raise SystemExit(
            f"[faultbench] SETTLE_WAIT_SECS must be a finite number of seconds, got {raw!r}"
        )
    if value < 0:
        raise SystemExit(
            f"[faultbench] SETTLE_WAIT_SECS must not be negative, got {raw!r}"
        )
    return value


def host_precheck(expected_firecrackers=0):
    """Refuse to start on a host that is already busy or already running VMs.

    A foreign Firecracker competes for CPU and shows up in a host-wide ftrace dump;
    background load moves every latency number. Both are invisible after the fact,
    which is why this runs before the first request rather than being reported with
    the results.

    SETTLE_WAIT_SECS > 0 bounds a wait for the load to fall below MAX_START_LOAD
    before refusing: the make one-shot chain reaches this gate seconds after its
    own prerequisite builds, whose work a 1-minute average still carries. A
    foreign firecracker still refuses immediately; it does not go away by waiting.
    """
    deadline = time.monotonic() + settle_wait_secs()
    while True:
        load1 = os.getloadavg()[0]
        foreign = firecracker_pids()
        info = {"loadavg_1m": load1, "firecracker_pids": sorted(foreign),
                "uptime_s": float(Path("/proc/uptime").read_text().split()[0])}
        if len(foreign) > expected_firecrackers:
            raise SystemExit(
                f"[faultbench] {len(foreign)} firecracker processes already running "
                f"({sorted(foreign)}); they would compete for CPU and pollute the host-wide "
                f"ftrace dump. Stop them first."
            )
        if load1 <= MAX_START_LOAD:
            return info
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SystemExit(
                f"[faultbench] 1-minute load average is {load1:.2f}, above {MAX_START_LOAD}; "
                f"every latency number would be measuring the other workload too"
            )
        print(f"[faultbench] settling: load1={load1:.2f} above {MAX_START_LOAD}; "
              f"re-sampling ({remaining:.0f}s left in the settle window)", flush=True)
        time.sleep(min(SETTLE_POLL_S, remaining))


class LoadSampler(threading.Thread):
    """Append /proc/loadavg to samples/loadavg.jsonl for the life of the run.

    Drift that a precheck cannot see is only detectable against a continuous record.
    """

    def __init__(self, dest: Path, stop_evt, period_s=1.0):
        super().__init__(daemon=True)
        self.dest = dest
        self.stop_evt = stop_evt
        self.period_s = period_s

    def run(self):
        self.dest.parent.mkdir(parents=True, exist_ok=True)
        with open(self.dest, "a") as f:
            while not self.stop_evt.is_set():
                one, five, fifteen = os.getloadavg()
                f.write(json.dumps({"t": time.time(), "load1": one, "load5": five,
                                    "load15": fifteen}) + "\n")
                f.flush()
                self.stop_evt.wait(self.period_s)


def prewarm(tag):
    """Pull memory.bin into the page cache so the file arm is measured warm.

    Without this the file arm takes major faults off NVMe and the comparison
    becomes 'page cache vs disk', not 'file-backed vs UFFD'.
    """
    p = SNAP_DIR / tag / "memory.bin"
    try:
        with open(p, "rb") as f:
            while f.read(1 << 24):
                pass
    except OSError:
        subprocess.run(["sudo", "sh", "-c", f"cat {p} > /dev/null"], check=False)


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


CELLS = {
    # cell               (memarm, uffd mode, snapshot key, granule bytes)
    "file-4k":          ("file", None,    "rootless", PAGE),
    "uffd-4k-copy":     ("uffd", "copy",  "rootless", PAGE),
    "uffd-4k-minor":    ("uffd", "minor", "rootless", PAGE),
    "uffd-huge-minor":  ("uffd", "minor", "huge",     HUGE),
    "uffd-huge-copy":   ("uffd", "copy",  "huge",     HUGE),
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--reps", type=int, default=6)
    ap.add_argument("--warmup", type=int, default=2, help="discarded leading reps per cell")
    ap.add_argument("--hold", type=float, default=3.0)
    ap.add_argument("--page", default="medium.html")
    ap.add_argument("--pages", default="", help="comma list of pages to sweep (overrides --page)")
    ap.add_argument("--cells", default="file-4k,uffd-4k-copy,uffd-4k-minor,uffd-huge-minor")
    ap.add_argument("--http-port", type=int, default=19578)
    ap.add_argument("--guest-mib", type=int, default=2048)
    ap.add_argument("--ftrace", action="store_true",
                    help="capture kvm:kvm_guest_fault (exact per-backend fault events + IPAs). "
                         "Perturbs latency, so run it as a SEPARATE pass from the timing pass.")
    ap.add_argument("--label", default="", help="tag written into every record")
    ap.add_argument("--seed", type=int, default=None,
                    help="request-order shuffle seed; recorded in run.json and in every "
                         "record so a run can be replayed in the same order")
    args = ap.parse_args()

    # Every refusal must run BEFORE the output directory is dirtied and
    # before hostserver.py is spawned: the server starts outside the teardown
    # try/finally, so a refusal after it leaks the server, and directories
    # made before a refusal make require_fresh_out_dir refuse the retry.
    precheck = host_precheck()
    print(f"[faultbench] host at start: load1={precheck['loadavg_1m']:.2f} "
          f"firecrackers={len(precheck['firecracker_pids'])}", flush=True)

    # golden snapshot tags, resolved the same way bench.sh does (content-addressed)
    tags = {}
    for d in SNAP_DIR.glob("cb-golden-*"):
        if d.is_dir():
            key = d.name.split("-")[2]
            tags[key] = d.name

    pages = [p.strip() for p in args.pages.split(",") if p.strip()] or [args.page]
    # The seed is recorded, so a suspicious run can be replayed in the same order.
    seed = args.seed if args.seed is not None else int(time.time())
    cells = [c.strip() for c in args.cells.split(",") if c.strip()]
    unknown = [c for c in cells if c not in CELLS]
    if unknown:
        raise SystemExit(f"[faultbench] unknown cells {unknown}; known: {sorted(CELLS)}")
    for c in [c for c in cells if not tags.get(CELLS[c][2])]:
        print(f"[faultbench] SKIP {c}: no golden snapshot for {CELLS[c][2]}", flush=True)
    cells = [c for c in cells if tags.get(CELLS[c][2])]
    if not cells:
        raise SystemExit("[faultbench] no cell has a golden snapshot; nothing to measure")

    out = Path(args.out)
    require_fresh_out_dir(out)
    (out / "requests").mkdir(parents=True, exist_ok=True)
    (out / "traces").mkdir(parents=True, exist_ok=True)
    (out / "logs").mkdir(parents=True, exist_ok=True)

    host4 = subprocess.run(
        "ip -4 route get 1.1.1.1 | grep -oP 'src \\K\\S+' | head -1",
        shell=True, capture_output=True, text=True).stdout.strip()
    srv = subprocess.Popen(
        [sys.executable, str(SCRIPT_DIR / "hostserver.py"), "--root", str(SCRIPT_DIR / "pages"),
         "--port", str(args.http_port)],
        stdout=open(out / "logs" / "hostserver.log", "w"), stderr=subprocess.STDOUT)
    time.sleep(1.0)

    runid = time.strftime("%H%M%S")
    results = []
    (out / "run.json").write_text(json.dumps({
        "runid": runid, "seed": seed, "cells": cells, "pages": pages,
        "reps": args.reps, "warmup": args.warmup, "label": args.label,
        "precheck": precheck,
    }, indent=2))

    load_stop = threading.Event()
    load_sampler = LoadSampler(out / "samples" / "loadavg.jsonl", load_stop)
    load_sampler.start()
    if args.ftrace:
        ftrace_setup()

    # Every serve is started up front and torn down on the way out, whatever happens in
    # between: the schedule interleaves cells, and an exception between start and stop
    # used to leave the serve, its trace directory and its state entry behind.
    serves = {}
    try:
        for cell in cells:
            memarm, umode, snapkey, granule = CELLS[cell]
            tag = tags[snapkey]
            trace_dir = out / "traces" / cell
            trace_dir.mkdir(parents=True, exist_ok=True)
            if memarm == "uffd":
                proc, pid = start_serve(tag, umode, trace_dir, out / "logs")
                serves[cell] = (proc, pid, tag, umode)
                print(f"[faultbench] {cell}: serve pid={pid} tag={tag} mode={umode}", flush=True)
            else:
                prewarm(tag)
                print(f"[faultbench] {cell}: file-backed, memory.bin prewarmed", flush=True)

        schedule = build_schedule(cells, pages, args.reps, args.warmup, seed)
        for cell, page, i, is_warmup in schedule:
            memarm, umode, snapkey, granule = CELLS[cell]
            tag = tags[snapkey]
            serve_pid = serves.get(cell, (None, None, None, None))[1]
            url = f"http://{host4}:{args.http_port}/{page}"
            name = f"fb-{runid}-{cell}-{page.split('.')[0]}-{i}"
            rec = run_request(
                cell=cell, tag=tag, memarm=memarm, umode=umode,
                serve_pid=serve_pid, url=url, name=name, out_dir=out,
                hold=args.hold, guest_bytes=args.guest_mib << 20,
                ftrace=args.ftrace)
            rec["rep"] = i
            rec["page"] = page
            rec["label"] = args.label
            rec["warmup"] = is_warmup
            rec["granule"] = granule
            rec["guest_bytes"] = args.guest_mib << 20
            rec["seed"] = seed
            results.append(rec)
            with open(out / "requests.jsonl", "a") as f:
                f.write(json.dumps(rec) + "\n")
            print(f"[faultbench] {cell} {page} rep{i}{' (warmup)' if is_warmup else ''}: "
                  f"rc={rec['rc']} wall={rec['wall_ms']:.0f}ms fc_pid={rec['fc_pid']}", flush=True)
            require_clones_gone(f"fb-{runid}", 120, f"{cell} {page} rep{i}")
            time.sleep(1.0)
    finally:
        # Every step runs even if an earlier one fails, then the failures are
        # reported together. Letting stop_serve's SystemExit propagate from here
        # skipped the remaining serves, the load sampler, the host server and the
        # ftrace instance, so one bad teardown leaked all of them.
        teardown_errors = []
        for cell, (proc, pid, tag, umode) in serves.items():
            try:
                stop_serve(proc, pid, tag, umode)
            except SystemExit as error:
                teardown_errors.append(f"{cell}: {error}")
        load_stop.set()
        load_sampler.join(timeout=5)
        srv.terminate()
        if args.ftrace:
            ftrace_teardown()
        if teardown_errors:
            raise SystemExit("[faultbench] teardown failed:\n  " + "\n  ".join(teardown_errors))

    print(f"[faultbench] done: {len(results)} requests -> {out}", flush=True)


if __name__ == "__main__":
    main()
