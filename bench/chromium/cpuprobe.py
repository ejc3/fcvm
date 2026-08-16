#!/usr/bin/env python3
"""Instantaneous CPU demand and run-queue starvation of fcvm's VMM processes.

Average CPU per request cannot answer "is 2 vCPU enough". A guest with 2 vCPUs
can never exceed 2.0 instantaneous cores, so an average of 1.69 is measured
UNDER A CEILING: it is equally consistent with "1.69 cores of steady demand and
0.31 spare" and with "bursts that saturate both cores, separated by idle". The
two call for opposite decisions, and the average cannot tell them apart.

Two things separate them, and neither is an average:

  peak instantaneous cores, measured where there IS headroom. Give the guest
  more vCPUs than it can use and the peak stops being clipped, so it reports
  demand rather than the cap.

  run-queue WAIT time. A thread that is runnable but not running is starving,
  by definition. `/proc/<tid>/schedstat` field 1 counts exactly those
  nanoseconds -- but only while `kernel.sched_schedstats=1`, which is off by
  default. Without it the field reads a constant 0, which looks like "no
  starvation" and is really "not measured". This refuses to report wait unless
  the sysctl is on, because a silent zero here would be the fail-open shape
  this repo keeps finding.

Resolution matters. `/proc/<pid>/stat` counts in 10 ms jiffies, so over a ~1 s
render a 20 ms sampling window spans 0-2 ticks and quantizes instantaneous
cores to 0.5-core steps -- coarser than the effect being measured. schedstat
field 0 is `se.sum_exec_runtime` in NANOSECONDS and has no such floor.

Threads are attributed by comm: firecracker names its vCPU threads `fc_vcpu N`,
so vCPU work separates from the device/API/UFFD threads that serve it. That
split is what distinguishes "the guest wants more cores" from "the VMM's own
I/O path is the bottleneck".

Usage:
    cpuprobe.py --watch firecracker --out probe.jsonl [--interval-ms 20]

Samples every matching process for its whole lifetime and writes one JSON
object per process on exit, plus a summary to stderr.
"""

import argparse
import json
import os
import sys
import threading
import time


def schedstats_enabled() -> bool:
    try:
        with open("/proc/sys/kernel/sched_schedstats") as handle:
            return handle.read().strip() == "1"
    except OSError:
        return False


def read_comm(tid_dir: str) -> str:
    try:
        with open(os.path.join(tid_dir, "comm")) as handle:
            return handle.read().strip()
    except OSError:
        return ""


def process_ended(pid: int) -> bool:
    """True once the process is gone OR is a zombie awaiting its parent's wait().

    An exited process keeps /proc/<pid>/task/<tid>/schedstat readable until it
    is reaped, so "the directory disappeared" is NOT the end of a process -- it
    is the end of a process whose parent already reaped it. Between exit and
    reap, every counter is frozen and every delta is zero, which reads exactly
    like a perfectly idle process. track() looped there forever.

    Found by test_runtime_before_the_probe_attached_is_not_counted, which parks
    its child in exactly that state: the probe hung with one thread in
    nanosleep beside a <defunct> child. In the field this needs only a VMM
    whose parent is slow to reap it, or an fcvm that died first -- and before
    followers ran in threads, one zombie froze the ENTIRE scan loop, so every
    later process went unmeasured with no error.

    State comes from /proc/<pid>/status, not /proc/<pid>/stat: comm sits inside
    parentheses in field 2 of stat and may contain spaces, so positional
    parsing of the state field misreads any process whose name has one. That
    same trap already bit the bench-stop ancestor walk.
    """
    try:
        with open(f"/proc/{pid}/status") as handle:
            for line in handle:
                if line.startswith("State:"):
                    return line.split()[1] == "Z"
    except OSError:
        return True
    return True


def read_schedstat(tid_dir: str):
    """(run_ns, wait_ns) for one thread, or None if it is gone."""
    try:
        with open(os.path.join(tid_dir, "schedstat")) as handle:
            fields = handle.read().split()
        return int(fields[0]), int(fields[1])
    except (OSError, IndexError, ValueError):
        return None


def sample_process(pid: int):
    """Sum per-thread runtime, split into vCPU threads and everything else.

    Returns None once the process is gone, so the caller can stop cleanly
    rather than treating a dead process as an idle one.
    """
    task_root = f"/proc/{pid}/task"
    try:
        tids = os.listdir(task_root)
    except OSError:
        return None
    total_run = total_wait = vcpu_run = vcpu_wait = 0
    threads = 0
    vcpus = 0
    for tid in tids:
        tid_dir = os.path.join(task_root, tid)
        stat = read_schedstat(tid_dir)
        if stat is None:
            continue
        run_ns, wait_ns = stat
        comm = read_comm(tid_dir)
        threads += 1
        total_run += run_ns
        total_wait += wait_ns
        if comm.startswith("fc_vcpu"):
            vcpus += 1
            vcpu_run += run_ns
            vcpu_wait += wait_ns
    if threads == 0:
        return None
    return {
        "run_ns": total_run,
        "wait_ns": total_wait,
        "vcpu_run_ns": vcpu_run,
        "vcpu_wait_ns": vcpu_wait,
        "threads": threads,
        "vcpu_threads": vcpus,
    }


def percentile(values, q):
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    pos = (len(ordered) - 1) * q
    low = int(pos)
    high = min(low + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (pos - low)


def track(pid: int, interval_s: float, want_wait: bool, stop=None) -> dict:
    """Follow one process to exit, returning its instantaneous-cores series.

    Per-thread counters are accumulated PER TID and carried forward, not summed
    over whatever threads happen to exist at each sample. Summing the live set
    is not monotonic: when a thread exits its runtime disappears from the total,
    so the cumulative figure DROPS and inter-sample deltas can go negative.
    Caught by validating against a synthetic load, where the reported mean came
    back at 0.01 cores beside a 1.05-core peak -- arithmetically impossible, and
    the tell that the burner threads had taken their runtime with them.
    """
    task_root = f"/proc/{pid}/task"
    last: dict[str, tuple[int, int]] = {}
    run_total = wait_total = 0
    vcpu_run_total = vcpu_wait_total = 0
    vcpu_tids: set[str] = set()
    series = []
    started = time.monotonic()
    previous_t = started
    threads_seen: set[str] = set()

    def collect(accumulate=True):
        """Advance the accumulators; returns (d_run, d_wait, d_vcpu_run) or None.

        `accumulate=False` is the PRIMING call. It must populate `last` without
        touching the totals: on the first pass `last` is empty, so every delta
        is measured from zero and therefore equals that thread's whole runtime
        SINCE PROCESS START. Folding those in charges the probe for work done
        before it attached -- inflating total_run_ms and mean_cores by however
        long the VMM had been alive, which for a snapshot restore is the entire
        boot. The per-sample series was always correct; only the totals were
        wrong, so nothing about the shape of a run revealed it.
        """
        nonlocal run_total, wait_total, vcpu_run_total, vcpu_wait_total
        try:
            tids = os.listdir(task_root)
        except OSError:
            return None
        d_run = d_wait = d_vcpu_run = d_vcpu_wait = 0
        for tid in tids:
            tid_dir = os.path.join(task_root, tid)
            stat = read_schedstat(tid_dir)
            if stat is None:
                continue
            run_ns, wait_ns = stat
            threads_seen.add(tid)
            if tid not in vcpu_tids and read_comm(tid_dir).startswith("fc_vcpu"):
                vcpu_tids.add(tid)
            prev_run, prev_wait = last.get(tid, (0, 0))
            # A TID can be reused within one process; treat a counter that went
            # backwards as a fresh thread rather than emitting a negative delta.
            if run_ns < prev_run:
                prev_run, prev_wait = 0, 0
            d_run += run_ns - prev_run
            d_wait += wait_ns - prev_wait
            if tid in vcpu_tids:
                d_vcpu_run += run_ns - prev_run
                d_vcpu_wait += wait_ns - prev_wait
            last[tid] = (run_ns, wait_ns)
        if accumulate:
            run_total += d_run
            wait_total += d_wait
            vcpu_run_total += d_vcpu_run
            vcpu_wait_total += d_vcpu_wait
        return d_run, d_wait, d_vcpu_run

    if collect(accumulate=False) is None:
        return {}
    while True:
        time.sleep(interval_s)
        now = time.monotonic()
        deltas = collect()
        if deltas is None or process_ended(pid):
            break
        if stop is not None and stop.is_set():
            # Asked to wind up while this process is still alive. Keep what was
            # measured -- a partial series is data; discarding it because the
            # run ended is not.
            previous_t = now
            break
        span_ns = (now - previous_t) * 1e9
        if span_ns > 0:
            d_run, d_wait, d_vcpu = deltas
            series.append({
                "t": round(now - started, 4),
                "cores": d_run / span_ns,
                "vcpu_cores": d_vcpu / span_ns,
                "wait_cores": (d_wait / span_ns) if want_wait else None,
            })
        previous_t = now

    lifetime_s = previous_t - started
    cores = [row["cores"] for row in series]
    vcpu_cores = [row["vcpu_cores"] for row in series]
    wait_cores = [row["wait_cores"] for row in series if row["wait_cores"] is not None]
    out = {
        "pid": pid,
        "lifetime_s": lifetime_s,
        "samples": len(series),
        "threads": len(threads_seen),
        "vcpu_threads": len(vcpu_tids),
        "total_run_ms": run_total / 1e6,
        "vcpu_run_ms": vcpu_run_total / 1e6,
        "mean_cores": (run_total / 1e9 / lifetime_s) if lifetime_s > 0 else None,
        "cores_p50": percentile(cores, 0.50),
        "cores_p95": percentile(cores, 0.95),
        "cores_max": max(cores) if cores else None,
        "vcpu_cores_max": max(vcpu_cores) if vcpu_cores else None,
        # The clipping question as a fraction of the render rather than one
        # number: how much of the time did demand sit at or above each vCPU
        # budget a caller might pick?
        "frac_above_1_5_cores": (sum(1 for c in cores if c >= 1.5) / len(cores)) if cores else None,
        "frac_above_2_cores": (sum(1 for c in cores if c >= 2.0) / len(cores)) if cores else None,
        "frac_above_3_cores": (sum(1 for c in cores if c >= 3.0) / len(cores)) if cores else None,
        "series": series,
    }
    if want_wait:
        out["total_wait_ms"] = wait_total / 1e6
        out["vcpu_wait_ms"] = vcpu_wait_total / 1e6
        out["wait_cores_max"] = max(wait_cores) if wait_cores else None
        out["wait_cores_p95"] = percentile(wait_cores, 0.95)
    else:
        # Absent, not zero. A zero here would read as "nothing starved".
        out["wait_unmeasured"] = "kernel.sched_schedstats=0"
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--watch", default="firecracker",
                        help="process comm to follow (exact match)")
    parser.add_argument("--out", required=True)
    parser.add_argument("--interval-ms", type=float, default=20.0)
    parser.add_argument("--scan-ms", type=float, default=50.0,
                        help="how often to look for new processes; the probe is "
                             "idle between scans, so this bounds its own cost")
    parser.add_argument("--duration-s", type=float, default=0.0,
                        help="stop watching after this long; 0 means until SIGINT")
    args = parser.parse_args()

    want_wait = schedstats_enabled()
    if not want_wait:
        print("cpuprobe: kernel.sched_schedstats=0, so run-queue WAIT is not "
              "accounted and will be reported as unmeasured rather than zero. "
              "Enable with: sudo sysctl -w kernel.sched_schedstats=1",
              file=sys.stderr)

    interval_s = args.interval_ms / 1000.0
    deadline = time.monotonic() + args.duration_s if args.duration_s > 0 else None
    # EVERY pid examined is remembered, not just the matching ones. Caching only
    # matches means re-reading /proc/<pid>/comm for every non-matching pid on
    # every scan: ~1300 opens per pass at 100 Hz, measured at 52.6% of a core,
    # and it slowed the renders being measured from ~950 ms to 4-5 s. A probe
    # that perturbs its subject by 4x is not measuring the subject.
    checked: set[int] = set()
    records = []
    # One follower THREAD per process. track() runs until its process exits, so
    # calling it inline froze the scan loop for that whole lifetime: any VMM
    # that started AND finished during another's track was never even added to
    # `checked`, and vanished from the record without a word. Concurrent clones
    # -- the case the corpus campaign is built out of -- were reported as one
    # process. Threads are the right tool here despite the GIL: every follower
    # is blocked in time.sleep or a /proc read, so they interleave rather than
    # compete, and the probe's own cost is unchanged.
    stop = threading.Event()
    sink_lock = threading.Lock()
    followers: list[threading.Thread] = []
    with open(args.out, "w") as sink:
        def follow(pid):
            record = track(pid, interval_s, want_wait, stop=stop)
            if not record:
                return
            with sink_lock:
                records.append(record)
                sink.write(json.dumps(record) + "\n")
                sink.flush()

        try:
            while deadline is None or time.monotonic() < deadline:
                for entry in os.listdir("/proc"):
                    if not entry.isdigit():
                        continue
                    pid = int(entry)
                    if pid in checked:
                        continue
                    checked.add(pid)
                    comm = read_comm(f"/proc/{pid}")
                    # comm is TRUNCATED TO 15 BYTES by the kernel, so fcvm's
                    # `firecracker-default-<hash>.bin` appears as
                    # `firecracker-def`. An exact match on "firecracker" finds
                    # nothing, silently: the first run of this probe produced an
                    # empty file and no error at all.
                    if not comm.startswith(args.watch[:15]):
                        continue
                    follower = threading.Thread(target=follow, args=(pid,),
                                                daemon=True)
                    follower.start()
                    followers.append(follower)
                time.sleep(args.scan_ms / 1000.0)
        except KeyboardInterrupt:
            pass
        finally:
            # Wind the followers up and collect what they have. Without this the
            # sink closes while they are still writing to it, and a run that was
            # measured correctly loses its records on the way out.
            stop.set()
            for follower in followers:
                follower.join(timeout=5.0)

    if not records:
        print("cpuprobe: no matching process was ever observed", file=sys.stderr)
        return 1
    maxes = [r["cores_max"] for r in records if r.get("cores_max") is not None]
    means = [r["mean_cores"] for r in records if r.get("mean_cores") is not None]
    print(f"cpuprobe: {len(records)} process(es) of comm={args.watch!r}", file=sys.stderr)
    # A process that exits before its second sample produces an empty series, so
    # cores_max is None. With every record in that state percentile() returns
    # None -- which `:.2f` raises TypeError on, and max([]) raises ValueError on.
    # The records are already written and flushed by then, so the crash would
    # discard only the summary, but it would also mask the real finding: the
    # probe attached too late to measure anything.
    if not maxes or not means:
        print("  no process lived long enough for a second sample; the records "
              "hold no instantaneous figures (raise --duration-s, or lower "
              "--scan-ms so the probe attaches sooner)", file=sys.stderr)
        return 1
    print(f"  mean cores   p50={percentile(means, 0.5):.2f}", file=sys.stderr)
    print(f"  PEAK cores   p50={percentile(maxes, 0.5):.2f} "
          f"p95={percentile(maxes, 0.95):.2f} max={max(maxes):.2f}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
