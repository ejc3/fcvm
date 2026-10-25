#!/usr/bin/env python3
"""p50/p95/p99 (nearest rank, untrimmed) of runs.csv from bench/gpu-restore/run.sh.

Exits non-zero if an exit-0 run lacks ack_ms: only cache-hit restores belong in the table."""
import csv
import math
import sys


def pct(s, p):
    return s[min(len(s) - 1, max(0, math.ceil(p / 100 * len(s)) - 1))]


rows = list(csv.DictReader(open(sys.argv[1])))
ok = [r for r in rows if r["exit"] == "0"]
if not ok:
    sys.exit(f"no successful run among {len(rows)}")
# Every run is meant to be a cache hit, which prints a restored record with ack_ms. An exit-0 run
# without one booted cold, and its wall time would sit in the cache-hit wall_ms distribution.
no_record = [r["run"] for r in ok if not r["ack_ms"]]
if no_record:
    sys.exit(f"{len(no_record)} exit 0 run(s) have no ack_ms (cold boot or no restored record): "
             f"run {', '.join(no_record)}; refusing to summarize them as cache hits")
print(f"runs {len(rows)}, exit 0: {len(ok)}, "
      f"backends: {sorted({r['backend'] for r in ok})}")
print("| metric | n | min | p50 | p90 | p95 | p99 | max |")
print("|---|---|---|---|---|---|---|---|")
for key in ("wall_ms", "ack_ms", "gpu_ms", "load1"):
    s = sorted(float(r[key]) for r in ok if r[key])
    if s:
        f = ".2f" if key == "load1" else ".0f"
        cells = " | ".join(f"{pct(s, p):{f}}" for p in (50, 90, 95, 99))
        print(f"| {key} | {len(s)} | {s[0]:{f}} | {cells} | {s[-1]:{f}} |")
