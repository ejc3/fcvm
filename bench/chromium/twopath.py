#!/usr/bin/env python3
"""PROFILE (not a benchmark) of exactly two shared-nothing Chromium render paths.

  PATH A  file    `fcvm snapshot run --snapshot <tag>`   guest RAM = MAP_PRIVATE of
                  <snap>/memory.bin, resolved by in-kernel minor faults off warm page cache.
  PATH B  uffd    `fcvm snapshot serve` + `snapshot run --pid <serve>`  guest RAM =
                  MAP_ANONYMOUS registered UFFD MISSING, resolved by a cross-process
                  UFFDIO_COPY per granule.

...crossed with TWO REQUEST DRIVERS, which is the second axis (`--drivers`):

  exec   `snapshot run --exec <python driver>`. The driver runs INSIDE the guest, so
         every request starts a Python interpreter in the VM. That interpreter start is
         page-fault-heavy, which means it does not merely add a constant to both paths --
         it inflates the very quantity the A/B is trying to measure, because on path B
         each of those faults is a UFFD round trip and on path A it is an in-kernel minor
         fault. Measuring the file-vs-UFFD gap here OVERSTATES it.
  cdp    `snapshot run` with NO --exec; the host drives Chromium's DevTools Protocol
         through fcvm's port forwarding. Nothing of ours runs in the guest, no interpreter
         starts, and the guest touches strictly fewer pages.

Both drivers are kept, and they are interleaved in ONE session against ONE golden, so the
exec->cdp delta is a within-run difference rather than a comparison across two runs whose
goldens, page caches and machine states differ.

One page, one egress mode (rootless, the default), one screenshot format. All four arms
share every stage boundary that exists on both drivers, so the decomposition is directly
comparable.

TWO PASSES, deliberately separate:

  pass "timing"  hold=0, no ftrace, no UFFD fault trace.  -> wall clock + CPU.
  pass "faults"  hold>0, kvm:kvm_guest_fault ftrace ON, UFFD fault trace ON. -> fault
                 counts/costs. ftrace perturbs latency, so its wall times are NOT the
                 timing result and are reported separately.

INTERLEAVED, always (AGENTS.md defect 2): every rep runs all four arms back to back, in an
order SHUFFLED from a recorded seed, so neither the memory backend nor the request driver
is confounded with wall-clock drift or with the machine state its predecessor left behind.
Blocked execution is what made the retracted egress comparison meaningless.

FAULT INSTRUMENTS and what each one counts -- these are NOT interchangeable:

  kvm:kvm_guest_fault   one arm64 stage-2 abort, carrying the faulting guest physical
                        address (IPA). Fires for BOTH paths and means the same thing in
                        both: "the guest touched a page the stage-2 tables did not map".
                        This is the ONLY comparable fault basis, and it is the event that
                        on path B turns into a userspace round trip.
  UFFD handler trace    one record per UFFD event the handler resolved, with the ioctl
                        service time around UFFDIO_COPY. Path B only, by construction.
  /proc/<fc>/min_flt    ASYMMETRIC, and therefore NOT a comparable count -- but the
                        asymmetry is itself evidence. Measured on a quiet box: path A's
                        vCPU threads take ~31k minor faults while path B's take ~12, and
                        path B's SERVE process takes ~8k instead. The counter follows
                        whichever address space the page is installed into, so it moves
                        with the work rather than measuring it, and it undercounts the
                        stage-2 abort count on both paths. Use kvm_guest_fault for the
                        comparison; use min_flt only to show WHERE the faults landed.
  pagemap resident      granules materialised in the guest-RAM VMAs at end of render.
                        Comparable across paths; cross-checks the fault counts.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))
# The cdp driver is REUSED, not reimplemented: cdpdrive.py already times every hop of the
# connect separately and borrows render.py's frame codec verbatim, so the host-driven arm
# and the in-guest arm speak CDP through identical code. A second implementation here
# would be free to disagree with it and silently invalidate the exec-vs-cdp A/B.
import cdpdrive  # noqa: E402
REPO_ROOT = SCRIPT_DIR.parent.parent
FCVM = Path(os.environ.get("FCVM", REPO_ROOT / "target" / "release" / "fcvm"))
STATE_DIR = Path("/mnt/fcvm-btrfs/state")
SNAP_DIR = Path("/mnt/fcvm-btrfs/snapshots")
PAGE = 4096
HZ = os.sysconf("SC_CLK_TCK")

# ---------------------------------------------------------------------------
# driver: reuse bench.sh's DRIVER_TEMPLATE verbatim, so the workload profiled here is
# byte-identical to the one that produced the 573 ms baseline being confirmed.
# ---------------------------------------------------------------------------


def build_driver(url: str, fmt: str, qual: int, hold: float) -> str:
    src = (SCRIPT_DIR / "bench.sh").read_text()
    m = re.search(r"^DRIVER_TEMPLATE='(.*?)'\s*$", src, re.S | re.M)
    if not m:
        sys.exit("could not extract DRIVER_TEMPLATE from bench.sh")
    code = m.group(1)
    host, port = re.match(r"https?://([^:/]+):(\d+)", url).groups()
    for k, v in (("@PHOST@", host), ("@PPORT@", port), ("@URL@", url),
                 ("@FMT@", fmt), ("@QUAL@", str(qual)), ("@HOLD@", str(hold))):
        code = code.replace(k, v)
    return "python3 -c '" + code + "'"


# ---------------------------------------------------------------------------
# /proc
# ---------------------------------------------------------------------------


def read_stat(path: str):
    """(comm, utime, stime, min_flt, maj_flt, state) from a /proc[/task]/stat file.

    comm may contain spaces AND ')', so the split is after the LAST ')'.
    """
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError:
        return None
    try:
        lp, rp = data.index(b"("), data.rindex(b")")
    except ValueError:
        return None
    comm = data[lp + 1:rp].decode("utf-8", "replace")
    rest = data[rp + 2:].split()
    # rest[0]=state(3) rest[7]=minflt(10) rest[9]=majflt(12) rest[11]=utime(14) rest[12]=stime(15)
    try:
        return (comm, int(rest[11]), int(rest[12]), int(rest[7]), int(rest[9]),
                rest[0].decode())
    except (IndexError, ValueError):
        return None


def children_of(pid: int):
    out = []
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return out
    for tid in tids:
        try:
            with open(f"/proc/{pid}/task/{tid}/children") as f:
                out.extend(int(x) for x in f.read().split())
        except OSError:
            pass
    return out


def read_maps(pid: int):
    out = []
    try:
        with open(f"/proc/{pid}/maps") as f:
            for line in f:
                parts = line.split(maxsplit=5)
                a, b = parts[0].split("-")
                out.append({"start": int(a, 16), "end": int(b, 16),
                            "path": parts[5].strip() if len(parts) > 5 else ""})
    except OSError:
        return []
    return out


def guest_ram_vmas(pid: int, want_bytes: int):
    """Guest RAM VMAs, selected by size so one rule covers file-backed and anonymous."""
    vmas = [v for v in read_maps(pid) if (v["end"] - v["start"]) >= 64 * 1024 * 1024]
    vmas = [v for v in vmas
            if "memory.bin" in v["path"] or "memfd" in v["path"] or v["path"] == ""]
    vmas.sort(key=lambda v: -(v["end"] - v["start"]))
    keep, total = [], 0
    for v in vmas:
        if total >= want_bytes:
            break
        keep.append(v)
        total += v["end"] - v["start"]
    return keep


def pagemap_resident(pid: int, vmas):
    """Count of present 4KiB pages per guest-RAM VMA. Bit 63 = present."""
    res = []
    try:
        fd = os.open(f"/proc/{pid}/pagemap", os.O_RDONLY)
    except OSError:
        return None
    try:
        for v in vmas:
            npages = (v["end"] - v["start"]) // PAGE
            os.lseek(fd, (v["start"] // PAGE) * 8, os.SEEK_SET)
            buf, remaining = b"", npages * 8
            while remaining > 0:
                chunk = os.read(fd, min(remaining, 1 << 22))
                if not chunk:
                    break
                buf += chunk
                remaining -= len(chunk)
            n = len(buf) // 8
            present = sum(1 for w in struct.unpack_from(f"<{n}Q", buf) if w & (1 << 63))
            res.append({"start": v["start"], "path": v["path"],
                        "npages": npages, "present": present})
    finally:
        os.close(fd)
    return res


# ---------------------------------------------------------------------------
# cgroup v2: the accounting basis that SURVIVES process exit
#
# /proc polling always undercounts by up to one sample period, and misses whatever a
# process burns between the last successful read and exit -- which for firecracker is
# exactly the kernel address-space reclaim we care about. cpu.stat/memory.stat of a leaf
# cgroup are cumulative over everything that ever ran in it, dead children included, so
# they are the ground truth the per-process split is reconciled against.
# ---------------------------------------------------------------------------

CG_ROOT = Path("/sys/fs/cgroup/twopath.slice")


def sh(cmd, check=False):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, check=check)


def cg_setup():
    r = sh(f"sudo -n mkdir -p {CG_ROOT} && "
           f"sudo -n sh -c 'echo +cpu > {CG_ROOT}/cgroup.subtree_control' && "
           f"sudo -n sh -c 'echo +memory > {CG_ROOT}/cgroup.subtree_control'")
    return r.returncode == 0


def cg_new(leaf: str):
    p = CG_ROOT / leaf
    if sh(f"sudo -n mkdir -p {p}").returncode != 0:
        return None
    return p


def cg_rm(p):
    if p:
        sh(f"sudo -n rmdir {p}")


def cg_read(p):
    """cpu.stat + memory.stat of a leaf, cumulative and exit-proof."""
    out = {}
    for fn, keys in (("cpu.stat", ("usage_usec", "user_usec", "system_usec")),
                     ("memory.stat", ("pgfault", "pgmajfault")),
                     ("memory.peak", None)):
        try:
            txt = (Path(p) / fn).read_text()
        except OSError:
            continue
        if keys is None:
            out[fn] = int(txt.strip())
            continue
        for line in txt.splitlines():
            k, _, v = line.partition(" ")
            if k in keys:
                out[k] = int(v)
    return out


# ---------------------------------------------------------------------------
# ftrace: kvm:kvm_guest_fault, the one comparable fault basis
# ---------------------------------------------------------------------------

FT = Path("/sys/kernel/tracing/instances/twopath")


def ft_write(path, value):
    return sh(f"sudo -n sh -c 'echo {value} > {path}'").returncode == 0


def ft_setup(bufsize_kb=32768):
    if sh(f"sudo -n mkdir -p {FT}").returncode != 0:
        return False
    ft_write(FT / "tracing_on", 0)
    ft_write(FT / "buffer_size_kb", bufsize_kb)
    # CLOCK_MONOTONIC, not the default per-cpu 'local' clock: the fault stream is only useful
    # if it can be WINDOWED to a stage boundary, and the stage marks are taken with
    # time.monotonic() in this process. Without this the two clocks have no common origin and
    # "faults during render" cannot be separated from "faults during the probes/hold".
    ft_write(FT / "trace_clock", "mono")
    ok = ft_write(FT / "events/kvm/kvm_guest_fault/enable", 1)
    ft_write(FT / "trace", "")
    return ok


def ft_teardown():
    sh(f"sudo -n sh -c 'echo 0 > {FT}/tracing_on; "
       f"echo 0 > {FT}/events/kvm/kvm_guest_fault/enable; rmdir {FT} 2>/dev/null || true'")


def ft_start():
    ft_write(FT / "trace", "")
    ft_write(FT / "tracing_on", 1)


def ft_stop_dump(dest: Path):
    ft_write(FT / "tracing_on", 0)
    dest.parent.mkdir(parents=True, exist_ok=True)
    with open(dest, "wb") as f:
        subprocess.run(["sudo", "-n", "cat", str(FT / "trace")], stdout=f, check=False)
    # Overruns silently truncate the fault set and would understate everything downstream.
    over = 0
    st = sh(f"sudo -n sh -c 'cat {FT}/per_cpu/cpu*/stats'")
    for line in st.stdout.splitlines():
        if line.startswith("overrun:"):
            over += int(line.split(":")[1])
    return over


# The TASK field can contain spaces -- firecracker's vCPU threads are named "fc_vcpu 0".
# An \S+ comm silently matched nothing and reported 0 faults for every request.
TRACE_LINE = re.compile(r"^\s*(.+?)-(\d+)\s+\[\d+\]\s+\S+\s+([\d.]+):\s+kvm_guest_fault:\s+ipa\s+(0x[0-9a-f]+)")


def parse_kvm_trace(path: Path, seq_out: Path = None, tids: set = None):
    """-> summary dict; also writes an (ts_us, ipa) LE-u64 sidecar for offline analysis.

    The sidecar is what makes locality / run-length / cross-clone-overlap / temporal-shape
    answerable for BOTH paths. Path A has no userspace handler and therefore no UFFD trace,
    so the stage-2 IPA sequence is its ONLY per-fault record -- and it is the one record that
    means the same thing on both paths.

    `tids` restricts counting to THIS request's firecracker threads. The ftrace instance is
    machine-wide, so any other agent's VM lands in the same buffer; filtering by our own TIDs
    makes the fault numbers exact even on a box we do not exclusively own (the WALL CLOCK
    still is not -- contention inflates that no matter what we filter).
    """
    n = 0
    ipas = set()
    by_comm = {}
    by_tid = {}
    foreign = 0
    first = last = None
    seq = []
    try:
        with open(path, errors="replace") as f:
            for line in f:
                m = TRACE_LINE.match(line)
                if not m:
                    continue
                comm, lpid, ts, ipa = m.groups()
                if tids is not None and int(lpid) not in tids:
                    foreign += 1
                    continue
                n += 1
                page = int(ipa, 16) & ~(PAGE - 1)
                ipas.add(page)
                by_comm[comm] = by_comm.get(comm, 0) + 1
                # by TID, not just by comm: two concurrent VMs both name their threads
                # "fc_vcpu 0", so a comm histogram cannot distinguish "our 2 vCPUs" from
                # "our vCPU 0 plus somebody else's". The TID set can, and it is the
                # evidence that this trace saw exactly one VM: ours.
                by_tid[f"{comm}-{lpid}"] = by_tid.get(f"{comm}-{lpid}", 0) + 1
                ts = float(ts)
                if first is None:
                    first = ts
                last = ts
                # ABSOLUTE CLOCK_MONOTONIC microseconds (trace_clock=mono), so the sequence
                # can be windowed against the stage marks recorded with time.monotonic().
                seq.append((int(ts * 1e6), page))
    except OSError:
        return None
    if seq_out is not None and seq:
        seq_out.parent.mkdir(parents=True, exist_ok=True)
        buf = bytearray()
        for tus, page in seq:
            buf += struct.pack("<QQ", tus, page)
        seq_out.write_bytes(bytes(buf))
    return {"events": n, "unique_pages": len(ipas), "by_comm": by_comm,
            "by_tid": by_tid,
            "vcpu_tids": sorted(k for k in by_tid if k.startswith("fc_vcpu")),
            "foreign_events_filtered": foreign,
            "filtered_by_tids": sorted(tids) if tids else None,
            "first_ts": first, "last_ts": last,
            "span_ms": (last - first) * 1000.0 if first is not None else None,
            "seq_file": str(seq_out) if seq_out is not None and seq else None}


def parse_fault_trace(path: Path):
    """UFFD handler trace: LE u64 triples (file_offset, before_ns, after_ns)."""
    try:
        buf = path.read_bytes()
    except OSError:
        return None
    n = len(buf) // 24
    if n == 0:
        return {"faults": 0}
    recs = struct.unpack_from(f"<{n * 3}Q", buf)
    offs = recs[0::3]
    before = recs[1::3]
    after = recs[2::3]
    svc = [after[i] - before[i] for i in range(n)]
    # inter-fault gap = wake + epoll + uffd read + guest re-entry + next fault delivery
    gaps = [before[i + 1] - after[i] for i in range(n - 1) if before[i + 1] >= after[i]]
    svc_s = sorted(svc)
    gaps_s = sorted(gaps) if gaps else [0]

    def q(a, p):
        return a[min(len(a) - 1, int(p * (len(a) - 1)))]

    return {
        "faults": n,
        "unique_offsets": len(set(offs)),
        "span_ns": after[-1] - before[0],
        "svc_ns_sum": sum(svc),
        "svc_ns_p50": q(svc_s, 0.5), "svc_ns_p90": q(svc_s, 0.9), "svc_ns_max": svc_s[-1],
        "gap_ns_sum": sum(gaps),
        "gap_ns_p50": q(gaps_s, 0.5), "gap_ns_p90": q(gaps_s, 0.9),
    }


# ---------------------------------------------------------------------------
# process discovery / sampling
# ---------------------------------------------------------------------------


class Sampler(threading.Thread):
    """Follows the whole clone process tree from the fcvm pid.

    Keeps the LAST observed utime/stime/min_flt/maj_flt per (pid,tid). These counters are
    monotonic and every process in the tree is CREATED during the request, so the last
    read is the total, modulo whatever it burned after the final read -- which is why the
    cgroup total is read afterwards and the two are reconciled.
    """

    def __init__(self, root_pid, period=0.004):
        super().__init__(daemon=True)
        self.root = root_pid
        self.period = period
        self.stop = threading.Event()
        self.hold = threading.Event()
        self.guest_bytes = 0
        self.tasks = {}       # (pid,tid) -> [comm, ut, st, mn, mj]
        self.procs = {}       # pid -> comm
        self.fc_pid = None
        self.fc_seen_at = None
        self.hold_snapshot = None
        self.samples = 0
        self.error = None
        self.foreign_fc_max = 0
        self.load_max = 0.0
        self.rustc_seen = False

    def _snap(self, pid):
        r = read_stat(f"/proc/{pid}/stat")
        if r is None:
            return False
        self.procs.setdefault(pid, r[0])
        # comm is truncated to 15 chars, so the real one is "firecracker-def"
        # (from firecracker-default-<sha>.bin). Match the prefix, never the whole word.
        if r[0].startswith("firecracker") and self.fc_pid is None:
            self.fc_pid = pid
            self.fc_seen_at = time.time()
        try:
            tids = os.listdir(f"/proc/{pid}/task")
        except OSError:
            tids = []
        for tid in tids:
            t = read_stat(f"/proc/{pid}/task/{tid}/stat")
            if t is None:
                continue
            k = (pid, int(tid))
            cur = self.tasks.get(k)
            if cur is None:
                self.tasks[k] = [t[0], t[1], t[2], t[3], t[4]]
            else:
                cur[0] = t[0]
                for i, v in enumerate(t[1:5], start=1):
                    if v > cur[i]:
                        cur[i] = v
        return True

    def run(self):
        try:
            known = {self.root}
            took = False
            tick = 0
            while not self.stop.is_set():
                frontier = list(known)
                for pid in frontier:
                    if not self._snap(pid):
                        continue
                    for c in children_of(pid):
                        known.add(c)
                self.samples += 1
                # Contention witness. This box is shared and other agents' VMs and cargo
                # builds appeared mid-profile more than once; a number taken while a
                # foreign firecracker or a 64-way rustc was running is not a quiet-box
                # number, and the only honest way to say so is to record it per request.
                tick += 1
                if tick % 25 == 0:
                    foreign = len(firecracker_pids() - known)
                    if foreign > self.foreign_fc_max:
                        self.foreign_fc_max = foreign
                    la = float(open("/proc/loadavg").read().split()[0])
                    if la > self.load_max:
                        self.load_max = la
                    self.rustc_seen = self.rustc_seen or bool(
                        subprocess.run(["pgrep", "-x", "rustc"], capture_output=True).stdout)
                if self.hold.is_set() and not took and self.fc_pid:
                    took = True
                    time.sleep(0.20)
                    v = guest_ram_vmas(self.fc_pid, self.guest_bytes)
                    self.hold_snapshot = {"t": time.time(), "pagemap": pagemap_resident(self.fc_pid, v)}
                time.sleep(self.period)
            # one last pass: processes may be zombies but /proc/<pid>/stat of a zombie
            # already includes exit_mm() reclaim (AGENTS.md teardown note)
            for pid in list(known):
                self._snap(pid)
        except Exception as e:
            self.error = repr(e)


def firecracker_pids():
    out = set()
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            # comm is TASK_COMM_LEN-1 = 15 chars, and the binary is
            # firecracker-default-<sha>.bin, so comm is "firecracker-def". An exact
            # == "firecracker" test matches NOTHING -- it silently made both the
            # stray-VM guard and the per-request leak check always report "clean".
            with open(f"/proc/{d}/comm") as f:
                if f.read().strip().startswith("firecracker"):
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


def clone_states(prefix: str):
    out = []
    for p in STATE_DIR.glob("*.json"):
        try:
            st = json.loads(p.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if (st.get("name") or "").startswith(prefix):
            out.append(st)
    return out


# ---------------------------------------------------------------------------
# stage decomposition -- IDENTICAL boundaries for both paths
# ---------------------------------------------------------------------------

# The first three stages and the last are IDENTICAL on both drivers -- same marks, same
# fcvm log lines -- because they are fcvm's own clone lifecycle and neither driver touches
# it. That shared spine is what makes an exec-vs-cdp total comparable at all.
#
# Between `restore_done` and `render_ok` the two drivers do genuinely different things and
# the stage names say so rather than pretending to a false correspondence:
#
#   exec   restore -> exec handshake -> guest python starts -> guest polls the fixture
#          server -> render -> in-guest probes -> fcvm tears the exec session down
#   cdp    restore -> guest is ready when its CDP port answers -> host connects CDP ->
#          render -> one host-side probe
#
# `guest_exec_spawn` has NO cdp counterpart by construction: it is a Python interpreter
# starting inside the VM, and removing it is the point of the cdp driver. `port_wait` is
# the cdp readiness signal and is the nearest thing to a counterpart for
# exec_handshake+guest_exec_spawn+net_check combined -- so those are what it must be
# compared against, never against `guest_exec_spawn` alone.
STAGES_EXEC = [
    ("harness",      "t0",          "fcvm_first"),
    ("clone_setup",  "fcvm_first",  "restore_begin"),
    ("restore",      "restore_begin", "restore_done"),
    ("exec_handshake", "restore_done", "go_sent"),
    ("guest_exec_spawn", "go_sent",  "exec_up"),
    ("net_check",    "exec_up",     "net_up"),
    ("render",       "net_up",      "render_ok"),
    ("probes",       "render_ok",   "hold_start"),
    ("exec_teardown", "hold_start", "exec_finished"),
    ("vm_teardown",  "exec_finished", "exit"),
]

STAGES_CDP = [
    ("harness",      "t0",           "fcvm_first"),
    ("clone_setup",  "fcvm_first",   "restore_begin"),
    ("restore",      "restore_begin", "restore_done"),
    # port_wait is SMALL and can even go slightly negative. That is not noise, it is the
    # measurement: MEASURED with probe_readiness.py, pasta completes the TCP handshake on
    # the host side ~60 ms before the guest can serve anything, so a successful connect is
    # NOT a readiness signal. The guest-readiness wait therefore lands in cdp_resolve
    # below, not here. (The residual negative part is log-read skew: restore_done is
    # stamped when the harness drains the line, cdp_begin when the host acts.)
    ("port_wait",    "restore_done",  "cdp_begin"),
    # SPLIT from cdp_connect deliberately: this stage is the first HTTP round trip to the
    # guest, so it carries the real "is the guest serving yet" wait on top of the cost of
    # /json/list itself. Lumping it with the WebSocket setup would hide which of the two
    # a change actually moved.
    ("cdp_resolve",  "cdp_begin",     "cdp_resolved"),
    ("cdp_ws",       "cdp_resolved",  "cdp_connected"),
    ("render",       "cdp_connected", "render_ok"),
    ("probes",       "render_ok",     "hold_start"),
    # teardown_begin, NOT hold_start: in the fault pass the harness holds the clone alive
    # so the sampler can read its pagemap, and charging that deliberate sleep to teardown
    # would report a ~1.5 s teardown. In the timing pass hold=0 and the two marks coincide.
    ("vm_teardown",  "teardown_begin", "exit"),
]

STAGES = {"exec": STAGES_EXEC, "cdp": STAGES_CDP}

ANCHORS = [
    # (mark, matcher) -- first match wins. Both paths emit all of these.
    ("fcvm_first",    lambda s: " INFO fcvm::" in s or " DEBUG fcvm::" in s),
    ("restore_begin", lambda s: "loading snapshot with File backend" in s
                             or "loading snapshot with UFFD backend" in s
                             or "loading snapshot with Uffd backend" in s),
    ("restore_done",  lambda s: "VM resume completed" in s),
    ("go_sent",       lambda s: "exec handshake: GO sent" in s),
    ("exec_up",       lambda s: s.startswith("BENCH_EXEC_UP")),
    ("net_up",        lambda s: s.startswith("BENCH_NET_UP")),
    ("render_ok",     lambda s: s.startswith("RENDER_OK")),
    ("hold_start",    lambda s: s.startswith("BENCH_HOLD_START")),
    ("exec_finished", lambda s: "exec finished, cleaning up" in s),
]


def decompose(marks, driver="exec"):
    out = {}
    for name, a, b in STAGES[driver]:
        if a in marks and b in marks:
            out[name] = (marks[b] - marks[a]) * 1000.0
        else:
            out[name] = None
    return out


# ---------------------------------------------------------------------------
# cdp driver: reaching a restored clone from the host
# ---------------------------------------------------------------------------


def state_by_name(name: str, deadline: float):
    """The clone's state file, found by NAME.

    NOT by pid: the request is spawned as `bash -c '... exec timeout ... fcvm ...'`, and
    GNU timeout FORKS the command rather than exec'ing it, so Popen.pid is timeout's pid
    and fcvm is its child. `fcvm snapshot run` records std::process::id() -- its own pid --
    so a pid match against Popen.pid finds nothing, forever. The per-request name is unique
    by construction, which makes it the reliable key.
    """
    while time.monotonic() < deadline:
        for p in STATE_DIR.glob("*.json"):
            try:
                st = json.loads(p.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            if st.get("name") == name:
                return st
        time.sleep(0.01)
    return None


def clone_cdp_endpoint(state: dict, port: int) -> str:
    """Host-side address of the clone's published CDP relay port.

    fcvm hands every VM a unique address, so every clone can publish the SAME guest port:
    rootless/pasta gives a unique 127.x.y.z loopback IP. Clones inherit `port_mappings`
    from the snapshot metadata, so `--publish 9223:9223` on the golden covers every clone
    with no per-clone plumbing here.
    """
    net = (state.get("config") or {}).get("network") or {}
    for key in ("loopback_ip", "host_ip", "guest_ip"):
        if net.get(key):
            return f"{net[key]}:{port}"
    raise RuntimeError(f"no host-side IP in clone network config: {sorted(net)}")


def wait_port(endpoint: str, deadline: float) -> float:
    """Retry-connect until the clone's CDP relay answers; returns the wait in ms.

    This is the cdp driver's ENTIRE readiness protocol, and it is the minimum possible
    one -- the first successful TCP connect to a listener that was already running when
    the snapshot was taken. No exec handshake, no interpreter start, no marker file, no
    health poll. It is the stage that replaces exec_handshake + guest_exec_spawn +
    net_check.
    """
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


# ---------------------------------------------------------------------------
# one request
# ---------------------------------------------------------------------------


def run_request(*, driver, path, tag, serve_pid, url, name, out_dir, hold, fmt, qual,
                guest_bytes, ftrace, cdp_port, req_timeout=180):
    log_path = out_dir / "requests" / f"{name}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    src = f"--pid {serve_pid}" if path == "B-uffd" else f"--snapshot {tag}"
    # The ONLY difference in the fcvm invocation between the two drivers. Everything
    # else -- backend flags, cgroup join, timeout wrapper, log level -- is identical, so
    # any measured difference is the request path and not the spawn.
    exec_arg = f" --exec {shquote(build_driver(url, fmt, qual, hold))}" if driver == "exec" else ""

    leaf = cg_new(f"req-{name}")
    cg0 = cg_read(leaf) if leaf else {}

    # The subshell joins the cgroup FIRST (so nothing can escape the accounting by
    # reparenting -- inheritance survives fork/setuid/unshare), prints BENCH_T0 AFTER the
    # join (so the join's sudo cost is never charged to a measured stage), then execs.
    join = f'sudo -n sh -c "echo $$ > {leaf}/cgroup.procs" 2>/dev/null; ' if leaf else ""
    inner = (f'{join}echo BENCH_T0; '
             f'exec timeout -k 10 {req_timeout} '
             f'env RUST_LOG=fcvm=debug {FCVM} snapshot run {src} --name {name} '
             f'--no-dirty-tracking --no-swap{exec_arg}')
    cmd = ["bash", "-c", inner]

    pre_fc = firecracker_pids()
    if ftrace:
        ft_start()

    t_spawn = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, bufsize=1)
    sampler = Sampler(proc.pid)
    sampler.guest_bytes = guest_bytes
    sampler.start()

    marks = {}
    marks_mono = {}   # same marks on CLOCK_MONOTONIC, to window the ftrace fault stream
    obs = {"render_line": None, "swap_move_ok": False, "vm_id": None}
    mlock = threading.Lock()

    def mark(key, ts=None, tm=None):
        with mlock:
            if key not in marks:
                marks[key] = ts if ts is not None else time.time()
                marks_mono[key] = tm if tm is not None else time.monotonic()

    lf = open(log_path, "w")
    lf.write(f"{t_spawn:.6f} BENCH_SPAWN path={path} driver={driver} name={name}\n")

    def on_line(s, ts, tm):
        lf.write(f"{ts:.6f} {s}\n")
        if s.startswith("BENCH_T0"):
            # BENCH_T0 carries the shell's own clock; use OUR arrival stamp so every
            # mark in this record comes from one clock.
            mark("t0", ts, tm)
        for mk, pred in ANCHORS:
            if mk not in marks and pred(s):
                mark(mk, ts, tm)
        if s.startswith("RENDER_OK"):
            obs["render_line"] = s
        if s.startswith("BENCH_HOLD_START"):
            sampler.hold.set()
        if obs["vm_id"] is None:
            m = re.search(r"vm_id=(vm-[0-9a-f]{32})", s)
            if m:
                obs["vm_id"] = m.group(1)
        # --no-swap must FAIL to move firecracker out of our leaf cgroup, or the
        # cgroup basis silently stops covering the VM. Detect, never assume.
        if "moved to dedicated cgroup with swap disabled" in s:
            obs["swap_move_ok"] = True

    def pump():
        for line in proc.stdout:
            on_line(line.rstrip("\n"), time.time(), time.monotonic())

    cdp = {"result": None, "error": None, "endpoint": None, "port_wait_ms": None,
           "state_wait_ms": None}
    # Pre-bound: if BOTH waits below time out, an unbound rc would raise NameError while
    # building the record -- losing this request AND aborting the whole run for what is
    # only a stuck teardown.
    rc = None
    try:
        if driver == "exec":
            # fcvm owns the whole request: it execs the guest driver and exits by itself.
            pump()
            rc = proc.wait()
        else:
            # The host owns the request. fcvm's stdout must still be DRAINED continuously
            # -- an undrained 64 KB pipe blocks fcvm's writer and stalls the very restore
            # we are timing (AGENTS.md "Pipe Buffer Deadlock") -- so the log reader moves
            # to a thread and the main thread drives CDP.
            reader = threading.Thread(target=pump, name=f"log-{name}", daemon=True)
            reader.start()
            fcvm_pid = None
            try:
                deadline = time.monotonic() + req_timeout
                t = time.monotonic()
                st = state_by_name(name, deadline)
                if st is None:
                    raise TimeoutError("clone state file never appeared")
                cdp["state_wait_ms"] = (time.monotonic() - t) * 1000
                fcvm_pid = st.get("pid")
                obs["vm_id"] = obs["vm_id"] or st.get("vm_id")
                cdp["endpoint"] = clone_cdp_endpoint(st, cdp_port)
                cdp["port_wait_ms"] = wait_port(cdp["endpoint"], deadline)

                tw, tm_ = time.time(), time.monotonic()
                mark("cdp_begin", tw, tm_)
                res = cdpdrive.drive(argparse.Namespace(
                    cdp_host=cdp["endpoint"], url=url, format=fmt, quality=qual,
                    timeout=max(2.0, deadline - time.monotonic()), idle_wait_ms=0.0,
                    out_prefix="", ws_url="", connect_retries=200, nav_timing=True,
                    render_module=str(SCRIPT_DIR / "render.py")))
                cdp["result"] = res
                sg = res.get("stages", {})
                # The two inner marks are DERIVED from the driver's own split rather than
                # taken separately, because taking them separately would mean re-timing
                # work the driver already timed and the two clocks would disagree by the
                # cost of the extra timestamp. connect_total_ms is measured from drive()'s
                # entry, which is cdp_begin, so both derived marks sit on this record's
                # clock.
                conn = sg.get("connect_total_ms") or 0.0
                resolve = sg.get("resolve_ms") or 0.0
                rend = sum(sg.get(k) or 0.0
                           for k in ("navigate_ms", "idle_ms", "screenshot_ms", "decode_ms"))
                mark("cdp_resolved", tw + resolve / 1000.0, tm_ + resolve / 1000.0)
                mark("cdp_connected", tw + conn / 1000.0, tm_ + conn / 1000.0)
                mark("render_ok", tw + (conn + rend) / 1000.0, tm_ + (conn + rend) / 1000.0)
                if res.get("ok"):
                    # Synthesised in render.py's own format so ONE parser downstream reads
                    # both drivers. total_ms matches render.py's definition (connect
                    # included), or the two drivers' totals would not be comparable.
                    obs["render_line"] = (
                        "RENDER_OK connect_ms=%.1f navigate_ms=%.1f idle_ms=%.1f "
                        "screenshot_ms=%.1f dom_ms=0.0 total_ms=%.1f png_bytes=%d"
                        % (conn, sg.get("navigate_ms") or 0.0, sg.get("idle_ms") or 0.0,
                           sg.get("screenshot_ms") or 0.0, conn + rend,
                           res.get("image_bytes") or 0))
                else:
                    cdp["error"] = res.get("error")
            except Exception as e:  # noqa: BLE001 - recorded, then torn down cleanly
                cdp["error"] = f"{type(e).__name__}: {e}"
            finally:
                mark("hold_start")
                if hold > 0:
                    sampler.hold.set()
                    time.sleep(hold)
                mark("teardown_begin")
                # SIGTERM to fcvm's OWN pid (state.pid == std::process::id()), not to the
                # timeout wrapper, so this is exactly fcvm's normal teardown -- the same
                # cleanup the exec driver reaches by itself. Not SIGKILL: that would be
                # the fast-teardown experiment, which is a different question.
                if fcvm_pid:
                    try:
                        os.kill(fcvm_pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                else:
                    proc.terminate()
                try:
                    rc = proc.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    rc = proc.wait(timeout=30)
                reader.join(timeout=10)
    finally:
        lf.close()
    render_line = obs["render_line"]
    swap_move_ok = obs["swap_move_ok"]
    vm_id = obs["vm_id"]
    t_exit = time.time()
    marks["exit"] = t_exit
    marks_mono["exit"] = time.monotonic()
    sampler.stop.set()

    over = None
    kvm = None
    kp = None
    if ftrace:
        # stop + dump FIRST (bounded capture), parse after the sampler is joined so the
        # firecracker TID set used for filtering is complete
        kp = out_dir / "traces" / "kvm" / f"{name}.trace"
        over = ft_stop_dump(kp)
    sampler.join(timeout=15)
    if ftrace and kp is not None:
        fc_tids = {t for (p, t) in sampler.tasks if p == sampler.fc_pid} or None
        kvm = parse_kvm_trace(kp, seq_out=out_dir / "traces" / "ipaseq" / f"{name}.ipaseq",
                              tids=fc_tids)
        # the text trace is ~14 MB/request and fully redundant with the sidecar
        subprocess.run(["gzip", "-f", str(kp)], check=False)
    cg1 = cg_read(leaf) if leaf else {}
    cg_rm(leaf)

    # reap check: nothing from this request may outlive it
    t_reap0 = time.time()
    leaked_fc, leaked_state = None, None
    while time.time() - t_reap0 < 20:
        leaked_fc = sorted(firecracker_pids() - pre_fc)
        leaked_state = [s["name"] for s in clone_states(name)]
        if not leaked_fc and not leaked_state:
            break
        time.sleep(0.1)
    reap_ms = (time.time() - t_reap0) * 1000.0

    stages = decompose(marks, driver)
    rec = {
        "path": path, "driver": driver, "name": name, "url": url, "rc": rc, "hold_s": hold,
        "ftrace": ftrace, "vm_id": vm_id,
        "cdp_endpoint": cdp["endpoint"], "cdp_port_wait_ms": cdp["port_wait_ms"],
        "cdp_state_wait_ms": cdp["state_wait_ms"], "cdp_error": cdp["error"],
        "cdp_stages": (cdp["result"] or {}).get("stages"),
        "cdp_image_bytes": (cdp["result"] or {}).get("image_bytes"),
        "cdp_image_sha256": (cdp["result"] or {}).get("image_sha256"),
        "cdp_wh": [(cdp["result"] or {}).get("width"), (cdp["result"] or {}).get("height")],
        # If every clone restored from one golden presents the SAME page target id, the
        # /json/list lookup can be hoisted out of the request entirely (cdpdrive --ws-url).
        # Recorded per request so that is a checkable claim rather than an assumption.
        "cdp_target_id": (cdp["result"] or {}).get("target_id"),
        "cdp_nav": (cdp["result"] or {}).get("nav"),
        "t_spawn": t_spawn, "t_exit": t_exit,
        "wall_total_ms": (t_exit - marks.get("t0", t_spawn)) * 1000.0,
        "marks": marks, "marks_mono": marks_mono, "stages": stages,
        "render_line": render_line,
        "fc_pid": sampler.fc_pid, "sampler_samples": sampler.samples,
        "foreign_fc_max": sampler.foreign_fc_max, "load_max": sampler.load_max,
        "rustc_during": sampler.rustc_seen,
        "contended": bool(sampler.foreign_fc_max or sampler.rustc_seen),
        "sampler_error": sampler.error,
        "hold_snapshot": sampler.hold_snapshot,
        "cgroup": {"before": cg0, "after": cg1, "leaf": str(leaf) if leaf else None},
        "swap_move_escaped_cgroup": swap_move_ok,
        "kvm_ftrace": kvm, "ftrace_overruns": over,
        "leaked_firecracker": leaked_fc, "leaked_state": leaked_state,
        "reap_wait_ms": reap_ms,
        "tasks": [{"pid": p, "tid": t, "comm": v[0], "utime_s": v[1] / HZ,
                   "stime_s": v[2] / HZ, "min_flt": v[3], "maj_flt": v[4]}
                  for (p, t), v in sorted(sampler.tasks.items())],
        "procs": {str(k): v for k, v in sampler.procs.items()},
    }
    if render_line:
        for k, v in re.findall(r"(\w+)=([\w./+-]+)", render_line):
            if k in ("connect_ms", "navigate_ms", "idle_ms", "screenshot_ms", "dom_ms",
                     "total_ms", "png_bytes"):
                try:
                    rec["r_" + k] = float(v)
                except ValueError:
                    pass
    # Success means DIFFERENT things on the two drivers, and conflating them would mark
    # every cdp request failed. exec: fcvm runs the whole request and its exit code is the
    # driver's. cdp: the harness ends the clone with SIGTERM, so a non-zero rc is EXPECTED
    # and the real evidence is that a decoded image of the right size came back.
    if driver == "exec":
        rec["ok"] = rc == 0 and render_line is not None
    else:
        res = cdp["result"] or {}
        rec["ok"] = bool(res.get("ok")) and (res.get("image_bytes") or 0) > 1000 \
            and res.get("width") == 1280
    return rec


def shquote(s):
    return "'" + s.replace("'", "'\"'\"'") + "'"


# ---------------------------------------------------------------------------
# serve lifecycle (path B)
# ---------------------------------------------------------------------------


def start_serve(tag, trace_dir, log_path):
    env = dict(os.environ, RUST_LOG="fcvm=debug")
    if trace_dir:
        Path(trace_dir).mkdir(parents=True, exist_ok=True)
        env["FCVM_UFFD_FAULT_TRACE"] = str(trace_dir)
    lf = open(log_path, "w")
    proc = subprocess.Popen([str(FCVM), "snapshot", "serve", tag],
                            stdout=lf, stderr=subprocess.STDOUT, env=env)
    # OUR serve, identified by OUR child pid -- never by "any serve for this tag" in the
    # shared state dir. `fcvm snapshot serve` records std::process::id(), and nothing execs
    # in between, so proc.pid IS the serve pid. Globbing the state dir instead handed back
    # another agent's serve on this shared box, and every path-B clone then died with
    # "serve process (PID ...) is not running" the moment that serve exited.
    dead = time.time() + 60
    while time.time() < dead:
        if proc.poll() is not None:
            sys.exit(f"serve exited early rc={proc.returncode}; see {log_path}")
        registered = serve_pid_for(tag)
        if registered == proc.pid and Path(f"/proc/{proc.pid}").exists():
            return proc, proc.pid
        if registered is not None and registered != proc.pid:
            sys.exit(f"another serve for {tag} is running (pid {registered}, ours is "
                     f"{proc.pid}) -- the box is not exclusively ours; aborting rather "
                     f"than measuring someone else's memory server")
        time.sleep(0.2)
    sys.exit("serve did not register in state")


def stop_serve(proc, pid):
    if proc is None:
        return
    try:
        proc.send_signal(signal.SIGTERM)
    except ProcessLookupError:
        pass
    dead = time.time() + 90
    while time.time() < dead:
        if proc.poll() is not None and not Path(f"/proc/{pid}").exists():
            return
        time.sleep(0.2)
    try:
        proc.kill()
    except ProcessLookupError:
        pass


def prewarm(tag):
    """memory.bin into page cache. Without it path A measures 'page cache vs disk'."""
    p = SNAP_DIR / tag / "memory.bin"
    n = 0
    with open(p, "rb") as f:
        while True:
            b = f.read(1 << 24)
            if not b:
                break
            n += len(b)
    return n


def load_now():
    la = open("/proc/loadavg").read().split()
    return {"load1": float(la[0]), "load5": float(la[1]), "load15": float(la[2]),
            "firecrackers": len(firecracker_pids())}


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--tag", default="")
    ap.add_argument("--reps", type=int, default=14)
    ap.add_argument("--warmup", type=int, default=3)
    ap.add_argument("--fault-reps", type=int, default=8)
    ap.add_argument("--fault-warmup", type=int, default=2)
    ap.add_argument("--hold", type=float, default=1.5, help="fault pass only")
    ap.add_argument("--page", default="medium.html")
    ap.add_argument("--fmt", default="jpeg")
    ap.add_argument("--qual", type=int, default=80)
    ap.add_argument("--http-port", type=int, default=19771)
    ap.add_argument("--guest-mib", type=int, default=2048)
    ap.add_argument("--passes", default="timing,faults")
    # cdp is the default because it is the request path that survives: the exec driver
    # starts a Python interpreter inside the guest per request, which is page-fault-heavy
    # and therefore inflates the very file-vs-UFFD gap this profile measures. exec is kept
    # so the two request paths can be A/B'd in one session rather than across runs.
    ap.add_argument("--drivers", default="exec,cdp",
                    help="comma-separated subset of exec,cdp")
    # 9223 is the RELAY port. Chromium ignores --remote-debugging-address and binds guest
    # loopback 9222 only, so the golden publishes 9223 and socat bridges it inside the
    # container. Driving 9222 from the host connects to nothing.
    ap.add_argument("--cdp-port", type=int, default=9223)
    # Recorded in meta.json so the exact arm ordering of this run is reproducible.
    ap.add_argument("--seed", default="cdp-rebaseline")
    args = ap.parse_args()

    drivers = [d.strip() for d in args.drivers.split(",") if d.strip()]
    bad = [d for d in drivers if d not in ("exec", "cdp")]
    if bad:
        sys.exit(f"unknown driver(s): {bad}")

    out = Path(args.out)
    (out / "requests").mkdir(parents=True, exist_ok=True)
    (out / "logs").mkdir(parents=True, exist_ok=True)
    (out / "traces").mkdir(parents=True, exist_ok=True)

    tag = args.tag
    if not tag:
        cands = sorted(d.name for d in SNAP_DIR.glob("cb-golden-rootless-*") if d.is_dir())
        if not cands:
            sys.exit("no cb-golden-rootless-* snapshot; run bench.sh phase1 first")
        tag = cands[0]

    # A cdp arm against a snapshot with no port_mappings produces a clone with no host-side
    # ingress, and every request then fails on a connect timeout that reads like a guest
    # fault. The golden either carries the mapping or the run must not start.
    if "cdp" in drivers:
        cfgp = SNAP_DIR / tag / "config.json"
        try:
            pm = (json.loads(cfgp.read_text()).get("metadata") or {}).get("port_mappings") or []
        except (OSError, json.JSONDecodeError) as e:
            sys.exit(f"cannot read {cfgp}: {e}")
        if not any(m.get("guest_port") == args.cdp_port for m in pm):
            sys.exit(f"snapshot {tag} publishes {pm}, not guest port {args.cdp_port} -- "
                     f"clones would be unreachable from the host. Build the golden with "
                     f"mkgolden-cdp.sh (--publish {args.cdp_port}:{args.cdp_port}).")

    host4 = sh("ip -4 route get 1.1.1.1 | grep -oP 'src \\K\\S+' | head -1").stdout.strip()
    url = f"http://{host4}:{args.http_port}/{args.page}"

    # EXCLUSIVITY GUARD. This box is shared (AGENTS.md), and two concurrent runs of this
    # harness silently corrupt each other in three ways at once: one shared ftrace instance
    # merges both VMs' faults into both traces, the serve state glob crosses runs, and the
    # other run's teardown rmdir's the instance mid-request. Every one of those was observed.
    # Refuse rather than publish a contaminated profile.
    stray = firecracker_pids()
    if stray:
        sys.exit(f"NOT EXCLUSIVE: {len(stray)} firecracker process(es) already running "
                 f"{sorted(stray)[:8]} -- wait for the box to go quiet")
    if sh(f"sudo -n test -d {FT}").returncode == 0:
        sys.exit(f"NOT EXCLUSIVE: ftrace instance {FT} already exists -- another twopath.py "
                 f"is running (or crashed; 'sudo rmdir {FT}' to clear)")
    if serve_pid_for(tag) is not None:
        sys.exit(f"NOT EXCLUSIVE: a snapshot serve for {tag} is already running")

    have_cg = cg_setup()
    quiet0 = load_now()
    meta = {
        "tag": tag, "url": url, "fmt": args.fmt, "qual": args.qual,
        "drivers": drivers, "cdp_port": args.cdp_port, "seed": args.seed,
        "guest_mib": args.guest_mib, "fcvm": str(FCVM),
        "fcvm_mtime": os.path.getmtime(FCVM),
        "uname": sh("uname -a").stdout.strip(),
        "nproc": os.cpu_count(),
        "cgroup_basis": have_cg,
        "quiet_before": quiet0,
        "hostname": sh("hostname").stdout.strip(),
        "started": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "page_cache_prewarm_bytes": None,
        "note": "PROFILE, not a benchmark. Interleaved A/B, medians + min/max only.",
    }
    print(f"[twopath] tag={tag} url={url} cgroup={have_cg} drivers={drivers} "
          f"cdp_port={args.cdp_port} load={quiet0}", flush=True)

    srv = subprocess.Popen(
        [sys.executable, str(SCRIPT_DIR / "hostserver.py"), "--root", str(SCRIPT_DIR / "pages"),
         "--port", str(args.http_port)],
        stdout=open(out / "logs" / "hostserver.log", "w"), stderr=subprocess.STDOUT)
    time.sleep(1.0)
    if sh(f"curl -sf -o /dev/null http://{host4}:{args.http_port}/{args.page}").returncode != 0:
        srv.terminate()
        sys.exit("host fixture server unreachable")

    meta["page_cache_prewarm_bytes"] = prewarm(tag)
    print(f"[twopath] prewarmed memory.bin ({meta['page_cache_prewarm_bytes'] >> 20} MiB)",
          flush=True)

    runid = time.strftime("%H%M%S")
    results = []
    serve_proc = serve_pid = None
    passes = [p.strip() for p in args.passes.split(",") if p.strip()]

    try:
        for pas in passes:
            ftrace = pas == "faults"
            hold = args.hold if ftrace else 0.0
            reps = args.fault_reps if ftrace else args.reps
            warm = args.fault_warmup if ftrace else args.warmup
            trace_dir = (out / "traces" / "uffd" / pas) if ftrace else None
            uffd_order = []   # path-B requests in serve-connection order

            if ftrace and not ft_setup():
                print("[twopath] WARNING: ftrace setup failed; fault pass degraded", flush=True)

            serve_proc, serve_pid = start_serve(tag, trace_dir,
                                                out / "logs" / f"serve-{pas}.log")
            print(f"[twopath] pass={pas} serve pid={serve_pid} ftrace={ftrace} hold={hold}",
                  flush=True)

            # INTERLEAVED over BOTH axes inside one pass (defect 2): every rep runs each
            # (driver, path) arm back to back, so neither the memory backend nor the
            # request driver is confounded with wall-clock drift. Warmups are explicit and
            # are recorded with warmup=True so the analysis can drop them by name.
            order = []
            for i in range(1, warm + reps + 1):
                block = [(i, d, p) for d in drivers for p in ("A-file", "B-uffd")]
                # SHUFFLED within the rep, from a RECORDED seed (defect 2). Balance is
                # preserved -- every rep still runs each arm exactly once -- but the
                # position of an arm inside the block varies, so no arm systematically
                # inherits the machine state left by the same predecessor. With four arms
                # a fixed order would have made "cdp always runs after exec" an
                # uncontrolled variable.
                random.Random(f"{args.seed}:{pas}:{i}").shuffle(block)
                order.extend(block)
            for i, d, p in order:
                nm = (f"tp-{runid}-{pas}-{'x' if d == 'exec' else 'c'}"
                      f"{'A' if p.startswith('A') else 'B'}-{i}")
                lo = load_now()
                serve_before = read_stat(f"/proc/{serve_pid}/stat")
                rec = run_request(driver=d, path=p, tag=tag, serve_pid=serve_pid, url=url,
                                  name=nm, out_dir=out, hold=hold, fmt=args.fmt,
                                  qual=args.qual, guest_bytes=args.guest_mib << 20,
                                  ftrace=ftrace, cdp_port=args.cdp_port)
                serve_after = read_stat(f"/proc/{serve_pid}/stat")
                rec.update({"pass": pas, "rep": i, "warmup": i <= warm,
                            "load_before": lo,
                            "serve_pid": serve_pid,
                            "serve_stat_before": serve_before,
                            "serve_stat_after": serve_after})
                if serve_before and serve_after:
                    rec["serve_cpu_s"] = ((serve_after[1] - serve_before[1])
                                          + (serve_after[2] - serve_before[2])) / HZ
                    rec["serve_min_flt"] = serve_after[3] - serve_before[3]
                    rec["serve_maj_flt"] = serve_after[4] - serve_before[4]
                if ftrace and p == "B-uffd":
                    # Traces are matched up AFTER the pass -- see below.
                    uffd_order.append((nm, rec))
                results.append(rec)
                with open(out / "requests.jsonl", "a") as f:
                    f.write(json.dumps(rec) + "\n")
                st = rec["stages"]
                print(f"[twopath] {pas} {d}/{p} r{i}{' (warm)' if rec['warmup'] else ''} "
                      f"ok={rec['ok']} wall={rec['wall_total_ms']:.0f}ms "
                      f"render={st.get('render') and round(st['render']) }ms "
                      f"faults={(rec.get('kvm_ftrace') or {}).get('events')} "
                      f"uffd={(rec.get('uffd_trace') or {}).get('faults')} "
                      f"leak={rec['leaked_firecracker']} load={lo['load1']}", flush=True)
                time.sleep(0.8)

            # MUST come after stop_serve. FaultTrace flushes in Drop, and the handler task is
            # only dropped when the SERVE process shuts down -- not when its VM disconnects
            # (verified: the .faults files appear only after SIGTERM to the serve). So the
            # traces cannot be claimed per-request; they are matched here by the serve's own
            # connection counter, which is assigned in connection order, and clones are
            # strictly serialized in this harness, so vm-<N> is the (N+1)-th path-B request.
            spid = serve_pid
            stop_serve(serve_proc, serve_pid)
            serve_proc = serve_pid = None
            if ftrace and trace_dir:
                files = {}
                for q in Path(trace_dir).glob(f"{spid}-vm-*.faults"):
                    m = re.match(rf"^{spid}-vm-(\d+)\.faults$", q.name)
                    if m:
                        files[int(m.group(1))] = q
                for n, (nm, rec) in enumerate(uffd_order):
                    q = files.get(n)
                    if q is None:
                        rec["uffd_trace"], rec["uffd_trace_path"] = None, None
                        continue
                    dest = q.with_name(f"{nm}.faults")
                    q.rename(dest)
                    rec["uffd_trace"] = parse_fault_trace(dest)
                    rec["uffd_trace_path"] = str(dest)
                    rec["uffd_serve_conn_index"] = n
                unmatched = sorted(set(files) - set(range(len(uffd_order))))
                if unmatched or len(files) != len(uffd_order):
                    print(f"[twopath] WARNING: uffd trace match {len(files)} files vs "
                          f"{len(uffd_order)} path-B requests, extra={unmatched}", flush=True)
                # requests.jsonl was written before the traces existed -- rewrite it whole
                with open(out / "requests.jsonl", "w") as f:
                    for r in results:
                        f.write(json.dumps(r) + "\n")
                print(f"[twopath] attached {sum(1 for _, r in uffd_order if r.get('uffd_trace'))}"
                      f"/{len(uffd_order)} uffd fault traces", flush=True)
                ft_teardown()
    finally:
        if serve_proc:
            stop_serve(serve_proc, serve_pid)
        srv.terminate()
        ft_teardown()
        sh(f"sudo -n rmdir {CG_ROOT}")

    meta["quiet_after"] = load_now()
    meta["n_requests"] = len(results)
    json.dump(meta, open(out / "meta.json", "w"), indent=1)
    print(f"[twopath] done: {len(results)} requests -> {out}", flush=True)


if __name__ == "__main__":
    main()
