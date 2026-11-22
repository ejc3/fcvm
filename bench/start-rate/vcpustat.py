#!/usr/bin/env python3
"""Sample each new firecracker process's vCPU thread while it is young.

Every PERIOD it reads, for each firecracker pid first seen less than WINDOW ago:
  /proc/<pid>/task/<vcpu tid>/schedstat  on-CPU ns, runqueue wait ns
  /proc/<pid>/task/<vcpu tid>/stat       utime, stime, guest_time (ticks), minflt, majflt
and appends one JSON line per sample with the wall clock. Stop with SIGTERM.
"""
import json, os, signal, sys, time

OUT = sys.argv[1]
PERIOD = float(sys.argv[2]) if len(sys.argv) > 2 else 0.025
WINDOW = float(sys.argv[3]) if len(sys.argv) > 3 else 12.0
stop = False
def on_term(*_):
    global stop
    stop = True
signal.signal(signal.SIGTERM, on_term)
signal.signal(signal.SIGINT, on_term)

seen = set()      # pids already classified
pending = {}      # pid -> first seen with another comm (it may exec firecracker yet)
young = {}        # pid -> (first_seen, vcpu tid)

def comm(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None

def discover(now):
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        if pid in seen:
            continue
        c = comm(f"/proc/{pid}/comm")
        if c is None:
            continue
        if not c.startswith("firecracker"):
            # a child that has not exec'd firecracker yet keeps its parent's comm
            if now - pending.setdefault(pid, now) > 2.0:
                seen.add(pid)
                del pending[pid]
            continue
        pending.pop(pid, None)
        tid = None
        try:
            for t in os.listdir(f"/proc/{pid}/task"):
                if (comm(f"/proc/{pid}/task/{t}/comm") or "").startswith("fc_vcpu"):
                    tid = int(t)
                    break
        except OSError:
            continue
        if tid is None:
            continue  # vCPU thread not created yet; look again
        seen.add(pid)
        young[pid] = (now, tid)

def sample(out, now):
    wall = time.time()
    for pid, (first, tid) in list(young.items()):
        if now - first > WINDOW:
            del young[pid]
            continue
        try:
            with open(f"/proc/{pid}/task/{tid}/schedstat") as f:
                run, wait, _ = f.read().split()
            with open(f"/proc/{pid}/task/{tid}/stat") as f:
                s = f.read()
            with open(f"/proc/{pid}/schedstat") as f:
                mrun = f.read().split()[0]
        except OSError:
            del young[pid]
            continue
        f_ = s[s.rindex(")") + 2:].split()
        # after comm: state=0 ... minflt=7 majflt=9 utime=11 stime=12 ... guest_time=40
        out.write(json.dumps({"w": wall, "pid": pid, "run": int(run), "wait": int(wait),
                              "minflt": int(f_[7]), "majflt": int(f_[9]), "ut": int(f_[11]),
                              "st": int(f_[12]), "gt": int(f_[40]), "mrun": int(mrun)}) + "\n")

with open(OUT, "w") as out:
    last_discover = 0.0
    while not stop:
        now = time.monotonic()
        # A VMM is resumed tens of ms after it starts; find it before that.
        if now - last_discover >= min(0.1, 2 * PERIOD):
            discover(now)
            last_discover = now
        sample(out, now)
        spent = time.monotonic() - now
        time.sleep(max(0.0, PERIOD - spent))
