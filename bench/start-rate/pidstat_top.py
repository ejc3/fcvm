#!/usr/bin/env python3
"""Sum `pidstat -u -h` CPU by command over a run: which processes the host's cores went to.

Usage: pidstat_top.py RUN_DIR [N] [start|hold]

With `start`, only samples from the run's start to its last healthy clone; with `hold`, only the
hold phase after it (phases.json; pidstat prints local clock time to the second).
Prints, per command, total CPU-seconds, its share of all CPU used, and the peak cores in one
sample. pidstat -h rows: Time UID PID %usr %system %guest %wait %CPU CPU Command.
"""

import sys
from collections import defaultdict
from pathlib import Path


def main():
    d = Path(sys.argv[1])
    top_n = int(sys.argv[2]) if len(sys.argv) > 2 else 15
    interval = 2.0
    hj = d / "host.json"
    if hj.exists():
        import json

        interval = float(json.loads(hj.read_text()).get("config", {}).get("pidstat_interval", 2))
    window = sys.argv[3] if len(sys.argv) > 3 else None
    lo = hi = None
    if window:
        import json
        import time as _t

        ph = json.loads((d / "phases.json").read_text())
        a, b = {"start": ("t0_wall", "all_done_wall"), "hold": ("all_done_wall", "hold_end_wall")}[window]
        lo = _t.strftime("%H:%M:%S", _t.localtime(ph[a]))
        hi = _t.strftime("%H:%M:%S", _t.localtime(ph[b]))
    tot = defaultdict(float)
    sysv = defaultdict(float)
    per_sample = defaultdict(lambda: defaultdict(float))
    for line in (d / "pidstat.log").read_text(errors="replace").splitlines():
        f = line.split()
        if len(f) < 10 or f[0].startswith("#") or not (f[1].isdigit() and f[2].isdigit()):
            continue
        try:
            t, usr, syst, cpu = f[0], float(f[3]), float(f[4]), float(f[7])
        except ValueError:
            continue
        if lo is not None and not (lo <= t <= hi):
            continue
        cmd = f[9]
        tot[cmd] += cpu / 100 * interval
        sysv[cmd] += syst / 100 * interval
        per_sample[t][cmd] += cpu / 100
    allcpu = sum(tot.values()) or 1
    peak = defaultdict(float)
    for t, m in per_sample.items():
        for c, v in m.items():
            peak[c] = max(peak[c], v)
    print(f"{'command':<20} {'cpu_s':>9} {'share':>6} {'sys_s':>9} {'peak_cores':>10}")
    for c, v in sorted(tot.items(), key=lambda kv: -kv[1])[:top_n]:
        print(f"{c:<20} {v:9.1f} {v/allcpu*100:5.1f}% {sysv[c]:9.1f} {peak[c]:10.1f}")
    print(f"{'ALL':<20} {allcpu:9.1f}")


if __name__ == "__main__":
    main()
