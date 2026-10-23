#!/usr/bin/env python3
"""Summarize ComputeSDK burst_tti result files for the fcvm participant.

For each burst and for all bursts pooled, prints the harness's own statistics (median,
p95, p99 after it trims the fastest and slowest 5 percent, which is what the published
leaderboard shows) and the untrimmed distribution (nearest-rank percentiles).

usage: summarize.py burst-1.json [burst-2.json ...]
"""
import json
import math
import sys


def nearest_rank(sorted_values, p):
    # The harness's percentile(): index ceil(p/100 * n) - 1.
    idx = max(0, math.ceil(p / 100 * len(sorted_values)) - 1)
    return sorted_values[min(idx, len(sorted_values) - 1)]


def harness_stats(values):
    # computeStats() in benchmarks/src/util/stats.ts: trim floor(5%) from each end.
    s = sorted(values)
    trim = math.floor(len(s) * 0.05)
    t = s[trim : len(s) - trim] if trim > 0 and len(s) - 2 * trim > 0 else s
    mid = len(t) // 2
    median = (t[mid - 1] + t[mid]) / 2 if len(t) % 2 == 0 else t[mid]
    return median, nearest_rank(t, 95), nearest_rank(t, 99)


def row(label, values, failures, extra=""):
    s = sorted(values)
    hm, h95, h99 = harness_stats(s)
    raw = [nearest_rank(s, p) for p in (50, 90, 95, 99)]
    print(
        f"| {label} | {len(s)}/{len(s) + failures} | {hm:.0f} | {h95:.0f} | {h99:.0f} | "
        f"{s[0]:.0f} | {raw[0]:.0f} | {raw[1]:.0f} | {raw[2]:.0f} | {raw[3]:.0f} | {s[-1]:.0f} |{extra}"
    )


def main(paths):
    pooled, pooled_fail = [], 0
    print(
        "| burst | ok | harness median | harness p95 | harness p99 | min | p50 | p90 | p95 | p99 | max | wall clock | first ready |"
    )
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for i, path in enumerate(paths, 1):
        doc = json.load(open(path))
        (r,) = [r for r in doc["results"] if r["provider"] == "fcvm"]
        ok = [it["ttiMs"] for it in r["iterations"] if not it.get("error")]
        fail = sum(1 for it in r["iterations"] if it.get("error"))
        pooled += ok
        pooled_fail += fail
        extra = f" {r.get('wallClockMs', float('nan')):.0f} | {r.get('timeToFirstReadyMs', float('nan')):.0f} |"
        row(str(i), ok, fail, extra)
    if len(paths) > 1:
        row("all", pooled, pooled_fail, " | |")
    s = sorted(pooled)
    print()
    print("deciles of all bursts (ms): " + ", ".join(f"{nearest_rank(s, p):.0f}" for p in range(10, 101, 10)))


if __name__ == "__main__":
    main(sys.argv[1:])
