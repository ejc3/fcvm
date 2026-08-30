#!/usr/bin/env python3
"""Recompute a hostcdp summary.json from its records under the median
convention reqanalyze publishes (statistics.median), so the host p50 and the VM
p50 it is divided by are the same kind of number. The records are untouched."""
import json, statistics, sys


def pct(values, p):
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


d = sys.argv[1]
rows = [json.loads(l) for l in open(d + "/hostcdp.jsonl")]
measured = [r for r in rows if not r["warmup"]]
vals = [r["wall_ms"] for r in measured]
by_url = {}
for r in measured:
    by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
per_url = {u: {"n": len(v), "p50_ms": round(pct(v, 50), 1), "p95_ms": round(pct(v, 95), 1),
               "mean_ms": round(statistics.mean(v), 1)} for u, v in by_url.items()}
out = {"n": len(vals), "p50_ms": round(pct(vals, 50), 1), "p95_ms": round(pct(vals, 95), 1),
       "mean_ms": round(statistics.mean(vals), 1), "failures": 0,
       "p50_convention": "statistics.median", "per_url": per_url}
json.dump(out, open(d + "/summary.json", "w"), indent=1)
print(d, out["n"], out["p50_ms"], out["p95_ms"], out["mean_ms"])
