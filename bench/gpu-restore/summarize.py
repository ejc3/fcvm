#!/usr/bin/env python3
"""p50/p95/p99 (nearest rank, untrimmed) of runs.csv from bench/gpu-restore/run.sh."""
import csv
import math
import sys


def pct(s, p):
    return s[min(len(s) - 1, max(0, math.ceil(p / 100 * len(s)) - 1))]


rows = list(csv.DictReader(open(sys.argv[1])))
ok = [r for r in rows if r["exit"] == "0"]
print(f"runs {len(rows)}, exit 0: {len(ok)}, backends: {sorted({r['backend'] for r in ok})}")
print("| metric | n | min | p50 | p90 | p95 | p99 | max |")
print("|---|---|---|---|---|---|---|---|")
for key in ("wall_ms", "ack_ms", "gpu_ms"):
    s = sorted(float(r[key]) for r in ok if r[key])
    if s:
        cells = " | ".join(f"{pct(s, p):.0f}" for p in (50, 90, 95, 99))
        print(f"| {key} | {len(s)} | {s[0]:.0f} | {cells} | {s[-1]:.0f} |")
