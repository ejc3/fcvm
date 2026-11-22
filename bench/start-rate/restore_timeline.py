#!/usr/bin/env python3
"""Where a restored clone's time goes between the VMM's resume and fc-agent's readiness.

Reads one start-rate run directory (bench/start-rate/startrate.py run). Per clone it takes,
from the clone's own log, the harness's receive time of each restore marker and fc-agent's
phase record in the restore acknowledgement. With `--trace DIR` it joins two host-side
records taken during the run:

  vcpustat.jsonl  (vcpustat.py)  the vCPU thread's on-CPU and runqueue-wait time
  s2fault.txt     (s2fault.bt)   stage-2 faults, device aborts and time in handle_userfault,
                                 100 ms buckets

and splits resume -> ready into: vCPU on a CPU, vCPU runnable and waiting for one, vCPU
blocked in a userfault, and the rest (the guest idle, or the vCPU thread asleep in the host
for another reason).
"""
import argparse
import datetime
import json
import math
import os
import re
import sys

MARKS = [
    ("resume", "VM resume completed"),
    ("epoch", "[fc-agent] detected restore-epoch"),
    ("handling", "[fc-agent] handling restore"),
    ("tcp_begin", "restore phase=tcp-cleanup"),
    ("tcp_done", "restore phase=tcp-cleanup"),
    ("clock", "system clock synchronized"),
    ("published", "snapshot network published"),
    ("phases_done", "restore phases complete"),
    ("ack", "fc-agent acknowledged exact restore generation"),
    ("ready", "exec server ready"),
    ("healthy", "VM healthy, switching to"),
    ("reset_wake", "vsock transport reset observed"),
    ("chronyd", "chronyd restarted after restore"),
    ("journald", "journald restarted after restore"),
]
STEPS = [
    ("resume", "epoch"), ("epoch", "handling"), ("handling", "tcp_begin"),
    ("tcp_begin", "tcp_done"), ("tcp_done", "clock"), ("clock", "published"),
    ("published", "phases_done"), ("phases_done", "ack"), ("ack", "ready"),
    ("resume", "ready"), ("ready", "healthy"), ("ready", "chronyd"), ("ready", "journald"),
]
STAMP = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z")
FC_PID = re.compile(r"Firecracker Child\.id\(\) returned: Some\((\d+)\)")
BUCKET_NS = 100_000_000


def parse_clone(path):
    t, wall, phases, fc_pid = {}, {}, None, None
    with open(path, errors="replace") as f:
        lines = f.readlines()
    for line in lines:
        try:
            ms = float(line[:10])
        except ValueError:
            continue
        if fc_pid is None:
            m = FC_PID.search(line)
            if m:
                fc_pid = int(m.group(1))
        for key, needle in MARKS:
            if key in t or needle not in line:
                continue
            if key == "tcp_begin" and not line.rstrip().endswith("begin"):
                continue
            if key == "tcp_done" and " complete " not in line:
                continue
            t[key] = ms
            m = STAMP.search(line)
            if m:
                wall[key] = datetime.datetime.fromisoformat(m.group(1)).replace(
                    tzinfo=datetime.timezone.utc).timestamp()
        if phases is None and "guest_phases=" in line:
            m = re.search(r"guest_phases=(\{.*\})", line)
            if m:
                try:
                    phases = json.loads(m.group(1))
                except ValueError:
                    pass
    return {"t": t, "wall": wall, "phases": phases, "fc_pid": fc_pid}


def load_vcpustat(path):
    by_pid = {}
    with open(path) as f:
        lines = f.readlines()
    for line in lines:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        by_pid.setdefault(row["pid"], []).append(row)
    return by_pid


def load_buckets(path):
    """bpftrace map dump -> {map: {pid: {bucket: value}}}"""
    maps = {}
    pat = re.compile(r"^@(\w+)\[(\d+), (\d+)\]: (\d+)$")
    with open(path, errors="replace") as f:
        lines = f.readlines()
    for line in lines:
        m = pat.match(line.strip())
        if m:
            maps.setdefault(m.group(1), {}).setdefault(int(m.group(2)), {})[int(m.group(3))] = int(m.group(4))
    return maps


def at(samples, wall, after):
    """The sample nearest `wall`: the first at or after it, or the last at or before it."""
    pick = None
    for row in samples:
        if after:
            if row["w"] >= wall:
                return row
        else:
            if row["w"] <= wall:
                pick = row
            else:
                break
    return pick


def join_clone(c, vs, maps):
    """One clone's resume -> ready window split by the host-side records, or None when the
    sampler holds nothing for its VMM."""
    pid, w = c["fc_pid"], c["wall"]
    if pid not in vs or "resume" not in w or "ready" not in w:
        return None
    s1 = at(vs[pid], w["ready"], after=True)
    if s1 is None:
        return None
    s0 = at(vs[pid], w["resume"], after=False) or {"run": 0, "wait": 0, "gt": 0, "st": 0, "mrun": None}
    window = (w["ready"] - w["resume"]) * 1000
    run = (s1["run"] - s0["run"]) / 1e6
    wait = (s1["wait"] - s0["wait"]) / 1e6
    buckets = math.ceil(window * 1e6 / BUCKET_NS)
    inside = lambda name: sum(v for b, v in maps.get(name, {}).get(pid, {}).items() if b < buckets)
    userfault = inside("ufns") / 1e6
    row = {"pid": pid, "window_ms": window, "vcpu_run_ms": run, "vcpu_wait_ms": wait,
           "userfault_ms": userfault, "userfaults": inside("ufn"), "stage2_faults": inside("s2"),
           "device_aborts": inside("mmio"),
           "guest_ticks_ms": (s1["gt"] - s0["gt"]) * 10, "sys_ticks_ms": (s1["st"] - s0["st"]) * 10,
           "rest_ms": window - run - wait - userfault}
    if "healthy" in w:
        h = at(vs[pid], w["healthy"], after=True)
        if h:
            row["ready_to_healthy_vcpu_run_ms"] = (h["run"] - s1["run"]) / 1e6
        later = at(vs[pid], w["healthy"] + 2.0, after=True)
        if h and later:
            row["two_s_after_healthy_vcpu_run_ms"] = (later["run"] - h["run"]) / 1e6
            if later.get("mrun") is not None and h.get("mrun") is not None:
                row["two_s_after_healthy_vmm_run_ms"] = (later["mrun"] - h["mrun"]) / 1e6
    return row


def pct(vals, q):
    vals = sorted(vals)
    return vals[min(len(vals) - 1, int(q * len(vals)))] if vals else float("nan")


def table(title, rows, unit="ms"):
    print(f"\n{title}")
    print(f"  {'':34} {'n':>5} {'p50':>10} {'p90':>10} {'p99':>10}  {unit}")
    for name, vals in rows:
        if vals:
            print(f"  {name:34} {len(vals):5} {pct(vals, .5):10.1f} {pct(vals, .9):10.1f} {pct(vals, .99):10.1f}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run")
    ap.add_argument("--trace", help="directory with vcpustat.jsonl and s2fault.txt")
    ap.add_argument("--first", type=int, default=0, help="first clone (by name order) to include")
    ap.add_argument("--last", type=int, default=None, help="one past the last clone to include")
    ap.add_argument("--json", help="write the per-clone rows here")
    a = ap.parse_args()

    names = sorted(os.listdir(os.path.join(a.run, "clones")))[a.first:a.last]
    clones = [parse_clone(os.path.join(a.run, "clones", n)) for n in names]
    clones = [c for c in clones if "ready" in c["t"] and "resume" in c["t"]]
    print(f"{a.run}: {len(clones)} clones with resume and ready")

    table("steps, harness receive times of the clone's log lines",
          [(f"{x} -> {y}", [c["t"][y] - c["t"][x] for c in clones if x in c["t"] and y in c["t"]])
           for x, y in STEPS])
    late = sum(1 for c in clones if "reset_wake" in c["t"] and c["t"]["reset_wake"] > c["t"].get("phases_done", 1e18))
    print(f"\n  transport reset read by the watcher after the restore finished: {late} of {len(clones)}")

    keys = sorted({k for c in clones if c["phases"] for k, v in c["phases"].items()
                   if isinstance(v, (int, float)) and not isinstance(v, bool)})
    table("fc-agent's phase record (guest clock)",
          [(k, [c["phases"][k] for c in clones if c["phases"] and k in c["phases"]])
           for k in keys if pct([c["phases"][k] for c in clones if c["phases"] and k in c["phases"]], .99) >= 0.05])
    netlink = [c["phases"].get("tcp_gate_netlink") for c in clones if c["phases"]]
    print(f"\n  gate over nfnetlink: {netlink.count(True)} of {len(netlink)}")

    if not a.trace:
        return
    vs = load_vcpustat(os.path.join(a.trace, "vcpustat.jsonl"))
    maps = load_buckets(os.path.join(a.trace, "s2fault.txt"))
    out = [row for row in (join_clone(c, vs, maps) for c in clones) if row]
    print(f"\n{len(out)} clones joined with the host-side records")
    table("resume -> ready, per clone (host clocks)",
          [(k, [r[k] for r in out if k in r]) for k in
           ["window_ms", "vcpu_run_ms", "vcpu_wait_ms", "userfault_ms", "rest_ms",
            "guest_ticks_ms", "sys_ticks_ms", "ready_to_healthy_vcpu_run_ms",
            "two_s_after_healthy_vcpu_run_ms", "two_s_after_healthy_vmm_run_ms"]])
    table("resume -> ready, per clone", [(k, [r[k] for r in out]) for k in ["stage2_faults", "device_aborts", "userfaults"]], unit="count")
    with open(os.path.join(a.trace, "s2fault.txt"), errors="replace") as f:
        for line in f:
            if line.startswith(("@execs:", "@forks:")):
                print(f"  host {line[1:].strip()} during the trace")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f)


if __name__ == "__main__":
    sys.exit(main())
