#!/usr/bin/env python3
"""Sample each new firecracker process's vCPU thread while it is young.

Every PERIOD it reads, for each firecracker process first seen less than WINDOW ago:
  /proc/<pid>/task/<vcpu tid>/schedstat  on-CPU ns, runqueue wait ns
  /proc/<pid>/task/<vcpu tid>/stat       utime, stime, guest_time (ticks), minflt, majflt
  /proc/<pid>/schedstat                  the main (VMM) thread's on-CPU ns
and appends one JSON line per sample with the wall clock at that process's read and the
process's start time, which with the pid names one process across pid reuse. Once a second
it also appends a clock anchor, `{"anchor": true, "mono_ns": .., "wall_ns": ..,
"spread_ns": ..}`: one reading of the wall clock between two of CLOCK_MONOTONIC, which is
the clock bpftrace's `nsecs` reads, so restore_timeline.py can place s2fault.bt's times on
the wall clock of the clones' log lines. Stop with SIGTERM.

usage: vcpustat.py OUT [PERIOD_S [WINDOW_S]]
"""
import json
import os
import signal
import sys
import time

# Names a child of fcvm carries between fork and its exec of firecracker.
PRE_EXEC = ("fcvm", "tokio-runtime-w")


def parse_stat(text):
    """Fields of a /proc/<pid>/[task/<tid>/]stat line. The comm can hold spaces and
    parentheses, so fields are counted from the last ')': state is field 3 of proc(5)."""
    f = text[text.rindex(")") + 2:].split()
    return {"ppid": int(f[1]), "minflt": int(f[7]), "majflt": int(f[9]), "ut": int(f[11]),
            "st": int(f[12]), "start": int(f[19]), "gt": int(f[40])}


def read(path):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return None


def clock_anchor(tries=20, tight_ns=20_000, mono=None, wall=None):
    """The wall clock and CLOCK_MONOTONIC at one instant: the wall clock read between two
    readings of the monotonic one, and the monotonic time taken as their middle. A reading
    interrupted between the three calls is wide; the tightest of up to `tries` is kept and
    its width recorded."""
    mono = mono or (lambda: time.clock_gettime_ns(time.CLOCK_MONOTONIC))
    wall = wall or time.time_ns
    best = None
    for _ in range(tries):
        before = mono()
        w = wall()
        after = mono()
        if best is None or after - before < best["spread_ns"]:
            best = {"anchor": True, "mono_ns": (before + after) // 2, "wall_ns": w,
                    "spread_ns": after - before}
        if best["spread_ns"] <= tight_ns:
            break
    return best


class Sampler:
    def __init__(self, window, proc="/proc"):
        self.window, self.proc = window, proc
        self.seen = set()   # pids classified: not a VMM, or a VMM already being sampled
        self.young = {}     # pid -> (first seen, vcpu tid, start time)

    def discover(self, now):
        listed = {int(n) for n in os.listdir(self.proc) if n.isdigit()}
        # A pid that left /proc can come back as another process.
        self.seen &= listed
        for pid in listed - self.seen:
            comm = (read(f"{self.proc}/{pid}/comm") or "").strip()
            if not comm.startswith("firecracker"):
                # A single-threaded child of fcvm may still exec firecracker; look again.
                try:
                    threads = len(os.listdir(f"{self.proc}/{pid}/task"))
                except OSError:
                    threads = 0
                if not (comm in PRE_EXEC and threads == 1):
                    self.seen.add(pid)
                continue
            tid = None
            try:
                for t in os.listdir(f"{self.proc}/{pid}/task"):
                    if (read(f"{self.proc}/{pid}/task/{t}/comm") or "").startswith("fc_vcpu"):
                        tid = int(t)
                        break
            except OSError:
                continue
            stat = read(f"{self.proc}/{pid}/stat")
            if tid is None or stat is None:
                continue  # vCPU thread not created yet; look again
            self.seen.add(pid)
            self.young[pid] = (now, tid, parse_stat(stat)["start"])

    def sample(self, out, now, wall_clock=time.time):
        for pid, (first, tid, start) in list(self.young.items()):
            if now - first > self.window:
                del self.young[pid]
                continue
            # Stamped per process, at its own read: a pass over hundreds of VMMs takes tens
            # of ms, and one stamp for the pass would date the last reads that much early.
            wall = wall_clock()
            sched = read(f"{self.proc}/{pid}/task/{tid}/schedstat")
            stat = read(f"{self.proc}/{pid}/task/{tid}/stat")
            main = read(f"{self.proc}/{pid}/schedstat")
            if sched is None or stat is None or main is None:
                del self.young[pid]
                continue
            run, wait, _ = sched.split()
            s = parse_stat(stat)
            out.write(json.dumps({"w": wall, "pid": pid, "start": start, "run": int(run),
                                  "wait": int(wait), "minflt": s["minflt"], "majflt": s["majflt"],
                                  "ut": s["ut"], "st": s["st"], "gt": s["gt"],
                                  "mrun": int(main.split()[0])}) + "\n")


def main(argv):
    out_path = argv[1]
    period = float(argv[2]) if len(argv) > 2 else 0.025
    window = float(argv[3]) if len(argv) > 3 else 12.0
    stop = []
    signal.signal(signal.SIGTERM, lambda *_: stop.append(1))
    signal.signal(signal.SIGINT, lambda *_: stop.append(1))
    sampler = Sampler(window)
    with open(out_path, "w") as out:
        last_discover = last_anchor = 0.0
        while not stop:
            now = time.monotonic()
            if now - last_anchor >= 1.0:
                out.write(json.dumps(clock_anchor()) + "\n")
                last_anchor = now
            # A VMM is resumed tens of ms after it starts; find it before that.
            if now - last_discover >= min(0.1, 2 * period):
                sampler.discover(now)
                last_discover = now
            sampler.sample(out, now)
            time.sleep(max(0.0, period - (time.monotonic() - now)))


if __name__ == "__main__":
    main(sys.argv)
