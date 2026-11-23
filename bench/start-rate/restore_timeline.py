#!/usr/bin/env python3
"""Where a restored clone's time goes between the VMM's resume and fc-agent's readiness.

Reads one start-rate run directory (bench/start-rate/startrate.py run). Per clone it takes,
from the clone's own log, the time of each restore marker and fc-agent's phase record in
the restore acknowledgement. A marker's time is the harness's receive time of its line,
except for a line fc-agent logged earlier than fcvm received it: a restore's progress lines
travel on the output connection after the acknowledgement and say when the guest logged
them (`logged=`), and a held line that went to the serial console says how long it was held
(`held_ms=`). Those are placed at the time they were logged. With `--trace DIR` it joins
two host-side records taken during the run:

  s2fault.txt     (s2fault.bt)   the vCPU thread's stage-2 faults and the time each took to
                                 return to the guest, device aborts, time in handle_userfault,
                                 in buckets from the vCPU's first stage-2 fault; that fault's
                                 time and the thread's on-CPU, waiting and guest time then
  vcpustat.jsonl  (vcpustat.py)  later samples of the vCPU thread's on-CPU, runqueue-wait and
                                 guest time, and the clock anchors that place s2fault.bt's
                                 times on the wall clock of the clone's log lines

The window is resume -> ready, two wall-clock stamps of the clone's fcvm process. The
buckets count from the vCPU's first stage-2 fault, which is not the resume stamp: the vCPU
thread can wait for a CPU first, and fcvm writes the line after the resume call returns. So
each clone's buckets are shifted by its own offset (`first_fault_after_resume_ms`, reported
as a row) and a count or time is the part of the buckets inside the window: whole buckets,
and a bucket the window starts or ends in by the share of it the window covers (the buckets
are 10 ms wide for the first second after the first fault, 100 ms after).

The window is split into: vCPU on a CPU, vCPU runnable and waiting for one, vCPU blocked in a
userfault, and the rest (before the first fault, the guest idle, or the vCPU thread asleep in
the host for another reason). On-CPU and waiting time are counted from the later of the
resume stamp and the first fault, where s2fault.bt read the thread's counters exactly, to
the ready stamp, read off the line between the two records around it (the first-fault record
and the sampler's samples). Guest time is the kernel's tick accounting, which /proc gives
in whole 10 ms: against an exact start it reads about 5 ms low per clone, so the table
gives its mean and it means little for one clone or a window of a few ticks.

The rows from s2fault.txt need no sample: a clone the sampler missed is in them and is
left out of the on-CPU rows only, counted by reason.
"""
import argparse
import datetime
import json
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
# When the guest logged a line fcvm received later (see the module docstring).
LOGGED = re.compile(r" logged=(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z")
HELD_MS = re.compile(r" held_ms=(\d+(?:\.\d+)?)\s*$")
FC_PID = re.compile(r"Firecracker Child\.id\(\) returned: Some\((\d+)\)")
FINE_NS, FINE_BUCKETS, COARSE_NS = 10_000_000, 100, 100_000_000
TICK_MS = 10
# The four records s2fault.bt takes at a vCPU's first stage-2 fault.
FIRST_FAULT = ("t0", "run0", "wait0", "guest0")


def bucket_span(index):
    """The [start, end) of an s2fault.bt bucket, in ns after the VMM's first stage-2 fault:
    100 buckets of 10 ms, then 100 ms each."""
    if index < FINE_BUCKETS:
        return index * FINE_NS, (index + 1) * FINE_NS
    start = FINE_BUCKETS * FINE_NS + (index - FINE_BUCKETS) * COARSE_NS
    return start, start + COARSE_NS


def within(buckets, start_ns, end_ns):
    """The part of `buckets` ({index: value}) that lies in [start_ns, end_ns), both counted
    from the VMM's first stage-2 fault: whole buckets, and a bucket the window starts or
    ends in by the share of it the window covers."""
    total = 0.0
    for index, value in buckets.items():
        start, end = bucket_span(index)
        overlap = min(end, end_ns) - max(start, start_ns)
        if overlap > 0:
            total += value * overlap / (end - start)
    return total


def stamp_s(text):
    return datetime.datetime.fromisoformat(text).replace(tzinfo=datetime.timezone.utc).timestamp()


def logged_before_receipt_ms(line):
    """How long before fcvm logged `line` the guest logged it: 0 for a line fcvm logs as it
    arrives, and for a line that says when it was logged or how long it was held, that long."""
    held = HELD_MS.search(line)
    if held:
        return float(held.group(1))
    logged, received = LOGGED.search(line), STAMP.search(line)
    if logged and received:
        return (stamp_s(received.group(1)) - stamp_s(logged.group(1))) * 1000
    return 0.0


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
            if key == "tcp_begin" and " begin" not in line:
                continue
            if key == "tcp_done" and " complete " not in line:
                continue
            earlier = logged_before_receipt_ms(line)
            t[key] = ms - earlier
            m = STAMP.search(line)
            if m:
                wall[key] = stamp_s(m.group(1)) - earlier / 1000
        if phases is None and "guest_phases=" in line:
            m = re.search(r"guest_phases=(\{.*\})", line)
            if m:
                try:
                    phases = json.loads(m.group(1))
                except ValueError:
                    pass
    return {"t": t, "wall": wall, "phases": phases, "fc_pid": fc_pid}


def load_vcpustat(path):
    """({pid: [series]}, [anchor]). A series is the samples of one process, told apart from
    another that had the same pid by its start time; an anchor is one reading of the wall
    clock against CLOCK_MONOTONIC."""
    by_process, anchors = {}, []
    with open(path) as f:
        lines = f.readlines()
    for line in lines:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("anchor"):
            anchors.append(row)
        elif "pid" in row:
            by_process.setdefault((row["pid"], row.get("start")), []).append(row)
    by_pid = {}
    for (pid, _), series in by_process.items():
        by_pid.setdefault(pid, []).append(series)
    return by_pid, sorted(anchors, key=lambda a: a["mono_ns"])


def wall_of(anchors, mono_ns):
    """The wall clock (s) at `mono_ns` of CLOCK_MONOTONIC, by the anchor nearest to it."""
    nearest = min(anchors, key=lambda a: abs(a["mono_ns"] - mono_ns))
    return (nearest["wall_ns"] + (mono_ns - nearest["mono_ns"])) / 1e9


def series_for(by_pid, pid, resume_wall):
    """The series of the process that had `pid` when the clone resumed, if the sampler saw
    one: the first whose samples do not all end before the resume."""
    live = [series for series in by_pid.get(pid, []) if series[-1]["w"] >= resume_wall]
    return min(live, key=lambda series: series[0]["w"]) if live else None


def load_buckets(path):
    """bpftrace map dump -> {map: {pid: {bucket: value}}}, and for the maps keyed by pid
    alone (the first-fault records) {map: {pid: value}}."""
    maps = {}
    bucketed = re.compile(r"^@(\w+)\[(\d+), (\d+)\]: (\d+)$")
    single = re.compile(r"^@(\w+)\[(\d+)\]: (\d+)$")
    with open(path, errors="replace") as f:
        lines = f.readlines()
    for line in lines:
        m = bucketed.match(line.strip())
        if m:
            maps.setdefault(m.group(1), {}).setdefault(int(m.group(2)), {})[int(m.group(3))] = int(m.group(4))
            continue
        m = single.match(line.strip())
        if m and m.group(1) in FIRST_FAULT:
            maps.setdefault(m.group(1), {})[int(m.group(2))] = int(m.group(3))
    return maps


def value_at(points, wall, key):
    """`key` at `wall`, on the line between the two points around it. Before the first
    point it is the first point's value; after the last it is unknown (None)."""
    if wall <= points[0]["w"]:
        return points[0][key]
    for before, after in zip(points, points[1:]):
        if wall <= after["w"]:
            span = after["w"] - before["w"]
            share = (wall - before["w"]) / span if span > 0 else 1.0
            return before[key] + (after[key] - before[key]) * share
    return None


def join_clone(c, by_pid, anchors, maps, shared_pids=()):
    """One clone's resume -> ready window split by the host-side records: (row, reasons).
    The row is None when s2fault.bt holds nothing for the clone; `reasons` names each part
    of the row the records cannot give."""
    pid, w = c["fc_pid"], c["wall"]
    if "resume" not in w or "ready" not in w:
        return None, ["no wall-clock stamp on its resume or ready line"]
    if pid in shared_pids:
        return None, ["its pid was another clone's VMM in this run, and the tracer's buckets are per pid"]
    if pid not in maps.get("t0", {}):
        return None, ["no stage-2 fault of its vCPU in the trace"]
    t0 = wall_of(anchors, maps["t0"][pid])
    if t0 >= w["ready"]:
        return None, ["its vCPU's first stage-2 fault is stamped after its ready line"]
    window = (w["ready"] - w["resume"]) * 1000
    start_ns, end_ns = (w["resume"] - t0) * 1e9, (w["ready"] - t0) * 1e9
    inside = lambda name: within(maps.get(name, {}).get(pid, {}), start_ns, end_ns)
    faults, fault_ms = inside("s2"), inside("s2ns") / 1e6
    userfault = inside("ufns") / 1e6
    row = {"pid": pid, "window_ms": window, "first_fault_after_resume_ms": (t0 - w["resume"]) * 1000,
           "userfault_ms": userfault, "userfaults": inside("ufn"), "stage2_faults": faults,
           "stage2_fault_ms": fault_ms, "device_aborts": inside("mmio")}
    if faults >= 1:
        row["us_per_stage2_fault"] = fault_ms * 1000 / faults

    series = series_for(by_pid, pid, w["resume"])
    if series is None:
        return row, ["the sampler never saw its VMM (no on-CPU rows)"]
    # The vCPU thread's counters at its first fault, then the sampler's later samples.
    first = {"w": t0, "run": maps.get("run0", {}).get(pid), "wait": maps.get("wait0", {}).get(pid),
             "guest_ms": maps.get("guest0", {}).get(pid, 0) / 1e6}
    if first["run"] is None or first["wait"] is None:
        return row, ["no on-CPU record at its vCPU's first fault (no on-CPU rows)"]
    later = [dict(s, guest_ms=s["gt"] * TICK_MS) for s in series if s["w"] > t0]
    points = [first] + later
    begin = max(w["resume"], t0)
    end = {key: value_at(points, w["ready"], key) for key in ("run", "wait", "guest_ms")}
    if end["run"] is None:
        return row, ["ready after the sampler's last sample of its VMM (no on-CPU rows)"]
    run = (end["run"] - value_at(points, begin, "run")) / 1e6
    wait = (end["wait"] - value_at(points, begin, "wait")) / 1e6
    guest = end["guest_ms"] - value_at(points, begin, "guest_ms")
    row.update({"vcpu_run_ms": run, "vcpu_wait_ms": wait, "guest_ms": guest,
                "outside_guest_ms": run - guest, "rest_ms": window - run - wait - userfault,
                # No sample inside the window: the line runs from the first fault to a
                # sample taken after ready.
                "sampled_in_window": any(s["w"] <= w["ready"] for s in later)})
    if "healthy" in w:
        h = value_at(points, w["healthy"], "run")
        later_run = value_at(points, w["healthy"] + 2.0, "run")
        if h is not None:
            row["ready_to_healthy_vcpu_run_ms"] = (h - end["run"]) / 1e6
        if h is not None and later_run is not None:
            row["two_s_after_healthy_vcpu_run_ms"] = (later_run - h) / 1e6
            main = [s for s in series if s.get("mrun") is not None]
            if main and main[0]["w"] <= w["healthy"]:
                m0, m1 = value_at(main, w["healthy"], "mrun"), value_at(main, w["healthy"] + 2.0, "mrun")
                if m1 is not None:
                    row["two_s_after_healthy_vmm_run_ms"] = (m1 - m0) / 1e6
    return row, []


def pct(vals, q):
    vals = sorted(vals)
    return vals[min(len(vals) - 1, int(q * len(vals)))] if vals else float("nan")


def table(title, rows, unit="ms", of=None):
    """n is the clones the row has a value for; `of` the clones in the cell."""
    print(f"\n{title}")
    count = "n" if of is None else f"n of {of}"
    print(f"  {'':34} {count:>10} {'mean':>10} {'p50':>10} {'p90':>10} {'p99':>10}  {unit}")
    for name, vals in rows:
        if vals:
            print(f"  {name:34} {len(vals):10} {sum(vals) / len(vals):10.1f} {pct(vals, .5):10.1f} "
                  f"{pct(vals, .9):10.1f} {pct(vals, .99):10.1f}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run")
    ap.add_argument("--trace", help="directory with vcpustat.jsonl and s2fault.txt")
    ap.add_argument("--first", type=int, default=0, help="first clone (by name order) to include")
    ap.add_argument("--last", type=int, default=None, help="one past the last clone to include")
    ap.add_argument("--json", help="write the per-clone rows here")
    a = ap.parse_args()

    names = sorted(os.listdir(os.path.join(a.run, "clones")))[a.first:a.last]
    every = [parse_clone(os.path.join(a.run, "clones", n)) for n in names]
    clones = [c for c in every if "ready" in c["t"] and "resume" in c["t"]]
    cell = len(every)
    print(f"{a.run}: {cell} clones, {len(clones)} with resume and ready, "
          f"{cell - len(clones)} without (left out of every row)")

    table("steps, between the clone's log lines (fc-agent's at the time the guest logged them)",
          [(f"{x} -> {y}", [c["t"][y] - c["t"][x] for c in clones if x in c["t"] and y in c["t"]])
           for x, y in STEPS], of=cell)
    late = sum(1 for c in clones if "reset_wake" in c["t"] and c["t"]["reset_wake"] > c["t"].get("phases_done", 1e18))
    print(f"\n  transport reset read by the watcher after the restore finished: {late} of {len(clones)}")

    keys = sorted({k for c in clones if c["phases"] for k, v in c["phases"].items()
                   if isinstance(v, (int, float)) and not isinstance(v, bool)})
    table("fc-agent's phase record (guest clock)",
          [(k, [c["phases"][k] for c in clones if c["phases"] and k in c["phases"]])
           for k in keys if pct([c["phases"][k] for c in clones if c["phases"] and k in c["phases"]], .99) >= 0.05],
          of=cell)
    netlink = [c["phases"].get("tcp_gate_netlink") for c in clones if c["phases"]]
    print(f"\n  gate over nfnetlink: {netlink.count(True)} of {len(netlink)}")

    if not a.trace:
        return
    by_pid, anchors = load_vcpustat(os.path.join(a.trace, "vcpustat.jsonl"))
    if not anchors:
        sys.exit(f"{a.trace}/vcpustat.jsonl holds no clock anchor (it is from a vcpustat.py older "
                 "than the anchors): s2fault.txt's times cannot be placed in the clones' windows")
    maps = load_buckets(os.path.join(a.trace, "s2fault.txt"))
    if not maps.get("t0"):
        sys.exit(f"{a.trace}/s2fault.txt holds no first-fault record (@t0): bpftrace prints its "
                 "maps when it is stopped with SIGINT")
    seen, shared = set(), set()
    for c in every:
        (shared if c["fc_pid"] in seen else seen).add(c["fc_pid"])
    out, left_out = [], {}
    for c in clones:
        row, reasons = join_clone(c, by_pid, anchors, maps, shared)
        if row:
            out.append(row)
        for reason in reasons:
            left_out[reason] = left_out.get(reason, 0) + 1
    timed = [r for r in out if "vcpu_run_ms" in r]
    print(f"\n{len(out)} of {cell} clones joined with s2fault.txt, {len(timed)} of them with the "
          f"sampler's record too")
    for reason, count in sorted(left_out.items()):
        print(f"  left out, {reason}: {count}")
    unsampled = sum(1 for r in timed if not r["sampled_in_window"])
    print(f"  on-CPU rows read off a line from the first fault to a sample after ready "
          f"(no sample inside the window): {unsampled} of {len(timed)}")
    widest = max(anchor["spread_ns"] for anchor in anchors)
    print(f"  clock anchors: {len(anchors)}, the widest reading {widest / 1000:.0f} us")
    table("resume -> ready, per clone (host clocks; guest_ms is whole 10 ms ticks, read its mean)",
          [(k, [r[k] for r in out if k in r]) for k in
           ["window_ms", "first_fault_after_resume_ms", "stage2_fault_ms", "userfault_ms", "vcpu_run_ms",
            "vcpu_wait_ms", "rest_ms", "guest_ms", "outside_guest_ms", "ready_to_healthy_vcpu_run_ms",
            "two_s_after_healthy_vcpu_run_ms", "two_s_after_healthy_vmm_run_ms"]], of=cell)
    table("resume -> ready, per clone",
          [(k, [r[k] for r in out if k in r]) for k in ["stage2_faults", "device_aborts", "userfaults"]],
          unit="count", of=cell)
    table("stage-2 fault on guest memory to the next guest entry, per clone's mean",
          [("us_per_stage2_fault", [r["us_per_stage2_fault"] for r in out if "us_per_stage2_fault" in r])],
          unit="us", of=cell)
    with open(os.path.join(a.trace, "s2fault.txt"), errors="replace") as f:
        for line in f:
            if line.startswith(("@execs:", "@forks:")):
                print(f"  host {line[1:].strip()} during the trace")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f)


if __name__ == "__main__":
    sys.exit(main())
