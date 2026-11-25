#!/usr/bin/env python3
"""Figures of one pages-mode run (`fcvm fleet renders-bench`) over a scored window.

    renders_analyze.py OUT [--skip-s 30] [--last-s N] [--json]

Reads OUT/renders.jsonl (one row per render) and OUT/samples.jsonl (one row a second: each
side's level, cgroup CPU time and CPU pressure). The window starts --skip-s after the
first sample, or is the last --last-s of the run. A render belongs to the window by the
time its answer arrived. CPU per render is the side's cgroup CPU time over the window
divided by the renders answered in it (the mean); CPU pressure is the share of the window
in which some task of the side waited for a CPU.
"""

import argparse
import json
import os
import statistics
import sys


def rows(path):
    with open(path) as handle:
        return [json.loads(line) for line in handle if line.strip()]


def pct(values, q):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def analyze(out, skip_s, last_s):
    samples = rows(os.path.join(out, "samples.jsonl"))
    renders = rows(os.path.join(out, "renders.jsonl"))
    if len(samples) < 2:
        raise SystemExit(f"{out}: fewer than two samples")
    t0, t1 = samples[0]["at"], samples[-1]["at"]
    start = t1 - last_s * 1000 if last_s else t0 + skip_s * 1000
    window = [s for s in samples if s["at"] >= start]
    if len(window) < 2:
        raise SystemExit(f"{out}: the window holds fewer than two samples")
    first, last = window[0], window[-1]
    seconds = (last["at"] - first["at"]) / 1000
    result = {"out": out, "window_s": round(seconds, 1), "sides": {},
              "recycles": last.get("recycles", 0) - first.get("recycles", 0)}
    for side in ("fcvm", "kitesurf"):
        mine = [r for r in renders if r["side"] == side and first["at"] <= r["at"] <= last["at"]]
        ok = [r["ms"] for r in mine if r["ok"]]
        levels = [s[side]["level"] for s in window]
        cpu = (last[side]["usage_usec"] or 0) - (first[side]["usage_usec"] or 0)
        psi = (last[side]["pressure_usec"] or 0) - (first[side]["pressure_usec"] or 0)
        by_page = {}
        for r in mine:
            if r["ok"]:
                by_page.setdefault(r["page"], []).append(r["ms"])
        result["sides"][side] = {
            "in_flight_median": statistics.median(levels),
            "in_flight_range": [min(levels), max(levels)],
            "renders": len(mine), "failed": len(mine) - len(ok),
            "renders_per_s": round(len(ok) / seconds, 2) if seconds else None,
            "median_ms": round(statistics.median(ok), 1) if ok else None,
            "p90_ms": round(pct(ok, 0.9), 1) if ok else None,
            "mean_ms": round(statistics.fmean(ok), 1) if ok else None,
            "cores_busy": round(cpu / 1e6 / seconds, 1) if seconds else None,
            "cpu_pressure": round(psi / 1e6 / seconds, 3) if seconds else None,
            "cpu_ms_per_render": round(cpu / 1000 / len(ok), 1) if ok else None,
            "pool_waits": last[side].get("pool_waits", 0) - first[side].get("pool_waits", 0),
            "image_sizes": sorted({f"{(r.get('detail') or {}).get('width')}x{(r.get('detail') or {}).get('height')}"
                                   for r in mine if r["ok"]}),
            "errors": sorted({(r.get("error") or "")[:100] for r in mine if not r["ok"]})[:5],
            "page_median_ms": {p: round(statistics.median(v), 1) for p, v in sorted(by_page.items())},
        }
    harness = [s["harness"] for s in window]
    hp = (harness[-1]["pressure_usec"] or 0) - (harness[0]["pressure_usec"] or 0)
    hu = (harness[-1]["usage_usec"] or 0) - (harness[0]["usage_usec"] or 0)
    parts = {}
    for name in (harness[0].get("parts") or {}):
        a, b = harness[0]["parts"][name], harness[-1]["parts"][name]
        parts[name] = {
            "cpu_pressure": round(((b["pressure_usec"] or 0) - (a["pressure_usec"] or 0)) / 1e6 / seconds, 3),
            "cores_busy": round(((b["usage_usec"] or 0) - (a["usage_usec"] or 0)) / 1e6 / seconds, 2),
        }
    probes = {side: sorted((h["replay_ms"].get(side) if h["replay_ms"].get(side) is not None else float("inf"))
                           for h in harness) for side in ("fcvm", "kitesurf")}
    result["harness"] = {
        "parts": parts,
        "replay_ms_median": {side: round(statistics.median(v), 2) for side, v in probes.items()},
        "replay_ms_p90": {side: round(v[int(0.9 * len(v))], 2) for side, v in probes.items()},
        "cpu_pressure": round(hp / 1e6 / seconds, 3), "cores_busy": round(hu / 1e6 / seconds, 1),
        "replay_ms_max": {side: max((h["replay_ms"].get(side) or 0) for h in harness)
                          for side in ("fcvm", "kitesurf")},
    }
    return result


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("out", nargs="+")
    p.add_argument("--skip-s", type=float, default=30.0)
    p.add_argument("--last-s", type=float, default=0.0)
    p.add_argument("--json", action="store_true")
    opts = p.parse_args(argv)
    for out in opts.out:
        result = analyze(out, opts.skip_s, opts.last_s)
        if opts.json:
            print(json.dumps(result))
            continue
        print(f"{out}: {result['window_s']} s scored, {result['recycles']} engine recycles, "
              f"harness {result['harness']}")
        for side, s in result["sides"].items():
            print(f"  {side}: in flight {s['in_flight_median']} {s['in_flight_range']}, "
                  f"{s['renders_per_s']} renders/s, failed {s['failed']} of {s['renders']}, "
                  f"median {s['median_ms']} ms, p90 {s['p90_ms']}, mean {s['mean_ms']}, "
                  f"cores busy {s['cores_busy']}, CPU pressure {s['cpu_pressure']}, "
                  f"CPU per render {s['cpu_ms_per_render']} ms, pool waits {s['pool_waits']}")
            print(f"    image sizes: {s['image_sizes']}")
            if s["errors"]:
                print(f"    errors: {s['errors']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
