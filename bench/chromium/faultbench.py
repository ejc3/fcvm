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
import os
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

    Selected by size, not by name, so one rule covers all three. Firecracker may
    split guest RAM into several regions; every large VMA is returned and the caller
    checks the total against the configured guest RAM.
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
    st = subprocess.run(["sudo", "cat", str(FTRACE / "per_cpu/cpu0/stats")],
                        capture_output=True, text=True, check=False)
    return st.stdout


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


def serve_pid_for(tag: str):
    for p in STATE_DIR.glob("*.json"):
        try:
            st = json.loads(p.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        cfg = st.get("config") or {}
        if cfg.get("process_type") == "serve" and cfg.get("snapshot_name") == tag:
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
            self.series.append((time.time(), *st))
            self.last = st
            if self.hold_evt.is_set() and not took_snapshot:
                # give the render a moment to settle, then take the one expensive read
                time.sleep(0.25)
                vmas = guest_ram_vmas(pid, self.guest_bytes)
                pm = pagemap_resident(pid, vmas)
                sm = smaps_rss(pid, vmas)
                st2 = proc_stat(pid)
                self.snapshot = {
                    "t": time.time(),
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
        "fc_first_seen": sampler.first_seen,
        "sampler_error": sampler.error,
        "stat_series": sampler.series,
        "hold_snapshot": sampler.snapshot,
        "serve_pid": serve_pid if memarm == "uffd" else None,
        "serve_stat_before": serve_before,
        "serve_stat_after": serve_after,
        "ftrace": bool(ftrace),
        "kvm_trace": str(kvm_trace_path) if kvm_trace_path else None,
        "ftrace_cpu0_stats": ftrace_stats,
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
        pid = serve_pid_for(tag)
        if pid and Path(f"/proc/{pid}").exists():
            return proc, pid
        if proc.poll() is not None:
            sys.exit(f"serve for {tag} ({umode}) exited early rc={proc.returncode}")
        time.sleep(0.2)
    sys.exit(f"serve for {tag} ({umode}) did not register in state")


def stop_serve(proc, pid):
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
    args = ap.parse_args()

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

    # golden snapshot tags, resolved the same way bench.sh does (content-addressed)
    tags = {}
    for d in SNAP_DIR.glob("cb-golden-*"):
        if d.is_dir():
            key = d.name.split("-")[2]
            tags[key] = d.name

    runid = time.strftime("%H%M%S")
    results = []
    pages = [p.strip() for p in args.pages.split(",") if p.strip()] or [args.page]
    if args.ftrace:
        ftrace_setup()

    try:
        for cell in [c.strip() for c in args.cells.split(",") if c.strip()]:
            memarm, umode, snapkey, granule = CELLS[cell]
            tag = tags.get(snapkey)
            if not tag:
                print(f"[faultbench] SKIP {cell}: no golden snapshot for {snapkey}", flush=True)
                continue

            trace_dir = out / "traces" / cell
            trace_dir.mkdir(parents=True, exist_ok=True)
            serve_proc = serve_pid = None
            if memarm == "uffd":
                serve_proc, serve_pid = start_serve(tag, umode, trace_dir, out / "logs")
                print(f"[faultbench] {cell}: serve pid={serve_pid} tag={tag} mode={umode}", flush=True)
            else:
                prewarm(tag)
                print(f"[faultbench] {cell}: file-backed, memory.bin prewarmed", flush=True)

            for page in pages:
                url = f"http://{host4}:{args.http_port}/{page}"
                for i in range(1, args.reps + args.warmup + 1):
                    name = f"fb-{runid}-{cell}-{page.split('.')[0]}-{i}"
                    rec = run_request(
                        cell=cell, tag=tag, memarm=memarm, umode=umode,
                        serve_pid=serve_pid, url=url, name=name, out_dir=out,
                        hold=args.hold, guest_bytes=args.guest_mib << 20,
                        ftrace=args.ftrace)
                    rec["rep"] = i
                    rec["page"] = page
                    rec["label"] = args.label
                    rec["warmup"] = i <= args.warmup
                    rec["granule"] = granule
                    rec["guest_bytes"] = args.guest_mib << 20
                    results.append(rec)
                    with open(out / "requests.jsonl", "a") as f:
                        f.write(json.dumps(rec) + "\n")
                    print(f"[faultbench] {cell} {page} rep{i}{' (warmup)' if rec['warmup'] else ''}: "
                          f"rc={rec['rc']} wall={rec['wall_ms']:.0f}ms fc_pid={rec['fc_pid']}", flush=True)
                    require_clones_gone(f"fb-{runid}", 120, f"{cell} {page} rep{i}")
                    time.sleep(1.0)

            if memarm == "uffd":
                stop_serve(serve_proc, serve_pid)
    finally:
        srv.terminate()
        if args.ftrace:
            ftrace_teardown()

    print(f"[faultbench] done: {len(results)} requests -> {out}", flush=True)


if __name__ == "__main__":
    main()
