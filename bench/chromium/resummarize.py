#!/usr/bin/env python3
"""Recompute a hostcdp summary.json from its records under the median
convention reqanalyze publishes (statistics.median), so the host p50 and the VM
p50 it is divided by are the same kind of number. The records are untouched.

hostcdp.sh can write "failures": 0 because it exits 4 on the first failed rep,
so a summary it reaches is a run that had none. This script has no such
invariant: it is pointed at a directory. Filtering on `warmup` alone put a
failed rep's wall_ms -- a timeout, the largest number in the file -- into the
distribution while the field beside it still said no failures. Measured on a
five-rep fixture with one failure: p95 30000.0, mean 6082.0, failures 0.

It also rewrites the file hostcdp.sh wrote, so it has to carry
loadavg1_measured forward. That field is the answer to "was the box busy while
this was measured", and a summary that dropped it cannot be checked against
that question again.
"""
import json, statistics, sys


def pct(values, p):
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


d = sys.argv[1]
with open(d + "/hostcdp.jsonl") as handle:
    rows = [json.loads(l) for l in handle if l.strip()]
measured_rows = [r for r in rows if not r["warmup"]]
failed = [r for r in measured_rows if not r.get("ok")]
if failed:
    sys.exit(f"REFUSING: {len(failed)} of {len(measured_rows)} measured reps in {d} "
             "failed; a failed rep's wall_ms is a timeout and this run is partial. "
             f"First: rep {failed[0].get('rep')} {failed[0].get('url')}")
if not measured_rows:
    sys.exit(f"REFUSING: no measured reps in {len(rows)} rows in {d}; nothing to summarise")

vals = [r["wall_ms"] for r in measured_rows]
by_url = {}
for r in measured_rows:
    by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
per_url = {u: {"n": len(v), "p50_ms": round(pct(v, 50), 1), "p95_ms": round(pct(v, 95), 1),
               "mean_ms": round(statistics.mean(v), 1)} for u, v in by_url.items()}
la = [r["loadavg1"] for r in measured_rows if isinstance(r.get("loadavg1"), (int, float))]
load = None
if la:
    load = {"n": len(la), "min": round(min(la), 2),
            "median": round(statistics.median(la), 2), "max": round(max(la), 2)}
out = {"n": len(vals), "p50_ms": round(pct(vals, 50), 1), "p95_ms": round(pct(vals, 95), 1),
       # 0 by construction: the run is refused above if any measured rep
       # failed, which is the same invariant hostcdp.sh's own summary has.
       "mean_ms": round(statistics.mean(vals), 1), "failures": len(failed),
       "p50_convention": "statistics.median", "loadavg1_measured": load,
       "per_url": per_url}
with open(d + "/summary.json", "w") as handle:
    json.dump(out, handle, indent=1)
print(d, out["n"], out["p50_ms"], out["p95_ms"], out["mean_ms"])
