#!/usr/bin/env python3
"""Recompute a hostcdp summary.json from one complete host run.

The run metadata must account for every record before any summary is written.
The p50 uses the convention reqanalyze publishes (statistics.median), so the
host p50 and the VM p50 it is divided by are the same kind of number. The
records are untouched.

hostcdp.sh can write "failures": 0 because it exits 4 on the first failed rep,
so a summary it reaches is a run that had none. This script has no such
invariant: it is pointed at a directory. Filtering on `warmup` alone put a
failed rep's wall_ms -- a timeout, the largest number in the file -- into the
distribution while the field beside it still said no failures. Measured on a
five-rep fixture with one failure: p95 30000.0, mean 6082.0, failures 0.

Refusal removes any earlier summary before returning, so an interrupted or
failed recomputation cannot leave an old success beside the rejected records.
It also rewrites the file hostcdp.sh wrote, so it has to carry
loadavg1_measured forward. That field is the answer to "was the box busy while
this was measured", and a summary that dropped it cannot be checked against
that question again.
"""
import fcntl
import os
import statistics
import sys

from compare import Refusal, load_host_dataset, write_json_atomic


def pct(values, p):
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


d = sys.argv[1]
summary_path = os.path.join(d, "summary.json")
lock_path = os.path.join(d, ".resummarize.lock")
try:
    lock = open(lock_path, "a+")
except OSError as error:
    sys.exit(f"REFUSING: cannot open {lock_path}: {error}")
with lock:
    fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
    try:
        os.unlink(summary_path)
    except FileNotFoundError:
        pass
    except OSError as error:
        sys.exit(f"REFUSING: cannot remove stale {summary_path}: {error}")
    try:
        _meta, rows, _transformed, _counts, _identities = load_host_dataset(d)
    except Refusal as error:
        sys.exit(f"REFUSING: {error}")
    measured_rows = [r for r in rows if r["warmup"] is False]

    vals = [r["wall_ms"] for r in measured_rows]
    by_url = {}
    for r in measured_rows:
        by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
    per_url = {
        u: {"n": len(v), "p50_ms": round(pct(v, 50), 1),
            "p95_ms": round(pct(v, 95), 1),
            "mean_ms": round(statistics.mean(v), 1)}
        for u, v in by_url.items()
    }
    la = [r["loadavg1"] for r in measured_rows]
    load = {"n": len(la), "min": round(min(la), 2),
            "median": round(statistics.median(la), 2), "max": round(max(la), 2)}
    out = {
        "n": len(vals), "p50_ms": round(pct(vals, 50), 1),
        "p95_ms": round(pct(vals, 95), 1),
        "mean_ms": round(statistics.mean(vals), 1), "failures": 0,
        "p50_convention": "statistics.median", "loadavg1_measured": load,
        "per_url": per_url,
    }
    write_json_atomic(summary_path, out)
    print(d, out["n"], out["p50_ms"], out["p95_ms"], out["mean_ms"])
