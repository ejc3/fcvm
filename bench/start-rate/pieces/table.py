#!/usr/bin/env python3
"""Print the piece table from matrix.sh cells: python3 table.py ROOT [ROOT...] [--cores N]

A cell directory is <piece>-c<C>-n<N>-a<ALIVE> holding summary.json, and optionally
perf-top.txt (from perftop.sh) whose first lines are "pct comm symbol".
"""

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

CELL = re.compile(r"^(?P<piece>[a-z-]+)-c(?P<c>\d+)-n(?P<n>\d+)-a(?P<a>\d+)$")


def load(roots):
    cells = defaultdict(dict)
    for root in roots:
        root = Path(root)
        for d in sorted(root.iterdir()):
            m = CELL.match(d.name)
            if not m or not (d / "summary.json").exists():
                continue
            s = json.loads((d / "summary.json").read_text())
            label = m["piece"] + ("+uffd" if s.get("uffd") else "") + (" cold" if s.get("cold") else "")
            top = []
            if (d / "perf-top.txt").exists():
                top = [line.strip() for line in (d / "perf-top.txt").read_text().splitlines() if line.strip()][:3]
            cells[label][(int(m["c"]), int(m["a"]))] = (s, top)
    return cells


def fmt(x, nd=0):
    if x is None:
        return "-"
    return f"{x:.{nd}f}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("roots", nargs="+")
    ap.add_argument("--cores", type=float, default=132.0,
                    help="cores left for starts (192 less ~60 for 1000 idle clones)")
    ap.add_argument("--ref-c", type=int, default=128)
    args = ap.parse_args()
    cells = load(args.roots)
    cs = sorted({c for v in cells.values() for (c, _a) in v})
    rows = []
    for label, v in cells.items():
        ref = v.get((args.ref_c, 1000)) or v.get((args.ref_c, 0))
        if not ref:
            continue
        s, top = ref
        t = s["timed"]
        host = t["host_cpu_s_per_op_net"]
        cg = t["cgroup_cpu_s_per_op_net"]
        cpu = max(host, cg)
        best = max(x[0]["timed"]["ops_per_s"] for x in v.values())
        ceiling = min(best, args.cores / cpu) if cpu > 0 else best
        rates = {}
        for a in (0, 1000):
            rates[a] = [v[(c, a)][0]["timed"]["ops_per_s"] if (c, a) in v else None for c in cs]
        lat = {k: v[k][0]["timed"]["latency_ms"]["p50"] for k in v}
        lat99 = {k: v[k][0]["timed"]["latency_ms"]["p99"] for k in v}
        steady = s["steady_per_instance_per_s"]["host_busy_cpu_s"]
        rows.append((ceiling, label, rates, cg, host, lat, lat99, steady, top, t))
    rows.sort(key=lambda r: r[0])
    hdr = ["piece", "ceiling/s"] + [f"ops/s C={c} a0/a1000" for c in cs] + [
        f"CPU-s/op cgroup (C={args.ref_c})", "CPU-s/op host", "p50/p99 ms C=1 a0", f"p50/p99 ms C={args.ref_c} a1000",
        "held instance, cores", "ctxt/op", "IPI/op", "top 3 frames"]
    print("| " + " | ".join(hdr) + " |")
    print("|" + "---|" * len(hdr))
    for ceiling, label, rates, cg, host, lat, lat99, steady, top, t in rows:
        cols = [label, fmt(ceiling)]
        for i, _c in enumerate(cs):
            cols.append(f"{fmt(rates[0][i])} / {fmt(rates[1000][i])}")
        k1 = (1, 0)
        kr = (args.ref_c, 1000)
        cols += [fmt(cg, 4), fmt(host, 4),
                 f"{fmt(lat.get(k1), 1)} / {fmt(lat99.get(k1), 1)}",
                 f"{fmt(lat.get(kr), 1)} / {fmt(lat99.get(kr), 1)}",
                 fmt(steady, 4), fmt(t["per_op"]["ctxt"]), fmt(t["per_op"]["ipi_total"]),
                 "; ".join(top)]
        print("| " + " | ".join(cols) + " |")


if __name__ == "__main__":
    main()
