#!/usr/bin/env python3
"""Summarize one density campaign's records (mcbench.py ladder, restore, fork) per arm.

  summarize.py OUT        prints a JSON summary of OUT/{ladder,restore,fork}.jsonl

Per arm: the highest count that held 20 ticks a second on the campaign's cores and the lowest
that did not, servers per core at that count, the memory each added server cost (the slope of
the VMs' cgroup memory and of the VMs' Firecracker PSS over the counts that held, by least
squares), restore to first chunk and fork times (p50, p90, max)."""
import json, statistics, sys


def load(path):
    try:
        return [json.loads(l) for l in open(path) if l.strip()]
    except FileNotFoundError:
        return []


def pct(xs, p):
    xs = sorted(x for x in xs if x is not None)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def slope(points):
    """Least-squares slope of y over x; None with under two distinct x."""
    if len({x for x, _ in points}) < 2:
        return None
    mx = statistics.fmean(x for x, _ in points)
    my = statistics.fmean(y for _, y in points)
    num = sum((x - mx) * (y - my) for x, y in points)
    den = sum((x - mx) ** 2 for x, _ in points)
    return num / den


def dist(xs):
    xs = [x for x in xs if x is not None]
    return {"n": len(xs), "p50": pct(xs, 0.5), "p90": pct(xs, 0.9), "max": max(xs) if xs else None}


def main(out):
    ladder, restore, fork = (load(f"{out}/{n}.jsonl") for n in ("ladder", "restore", "fork"))
    arms = sorted({r["arm"] for r in ladder + restore + fork})
    summary = {}
    for arm in arms:
        steps = [r for r in ladder if r["arm"] == arm]
        # A count holds only if every run of it held (the ladder's and any confirmation runs).
        counts = {}
        for r in steps:
            counts.setdefault(r["servers"], []).append(r)
        held = [min(rs, key=lambda r: r.get("tps_min") or 0) for k, rs in counts.items() if all(r.get("holds") for r in rs)]
        failed = [min(rs, key=lambda r: r.get("tps_min") or 0) for k, rs in counts.items() if not all(r.get("holds") for r in rs)]
        # Below the lowest count that failed only.
        if failed:
            lowest_fail = min(r["servers"] for r in failed)
            held = [r for r in held if r["servers"] < lowest_fail]
        best = max(held, key=lambda r: r["servers"], default=None)
        worst = min(failed, key=lambda r: r["servers"], default=None)
        mem = [(r["servers"], r["cgroup_mem_mib"]) for r in held if r.get("cgroup_mem_mib") is not None]
        pss = [(r["servers"], r["firecracker_pss_mib"]) for r in held if r.get("firecracker_pss_mib")]
        anon = [(r["servers"], r["cgroup_anon_mib"]) for r in held if r.get("cgroup_anon_mib") is not None]
        rs = [r for r in restore if r["arm"] == arm]
        fs = [r for r in fork if r["arm"] == arm]
        summary[arm] = {
            "cores": best and best["cores"],
            "bots_per_server": best and best["bots_per_server"],
            "held_servers": best and best["servers"],
            "held_servers_per_core": best and best["servers"] / best["cores"],
            "held_tps_min": best and best["tps_min"],
            "held_vm_cores_used": best and best.get("vm_cores_used"),
            "first_failed_servers": worst and worst["servers"],
            "first_failed_tps_min": worst and worst.get("tps_min"),
            "first_failed_servers_below": worst and worst.get("servers_below"),
            "mem_per_server_mib_cgroup": slope(mem),
            "mem_per_server_mib_pss": slope(pss),
            "mem_per_server_mib_anon": slope(anon),
            "runs_per_count": {k: [bool(r.get("holds")) for r in rs] for k, rs in sorted(counts.items())},
            "steps": [{k: r.get(k) for k in ("servers", "holds", "tps_min", "tps_median_of_mins", "servers_below",
                                               "vm_cores_used", "cgroup_mem_mib", "cgroup_anon_mib", "firecracker_pss_mib", "bisect", "confirm")}
                      for r in sorted(steps, key=lambda r: r["servers"])],
            "restore_first_chunk_ms": dist([r.get("first_chunk_ms") for r in rs]),
            "probe_join_ms": dist([r.get("probe_join_ms") for r in rs]),
            "fork_snapshot_ms": dist([r.get("snapshot_ms") for r in fs]),
            "fork_restore_first_chunk_ms": dist([r.get("restore_first_chunk_ms") for r in fs]),
            "fork_total_ms": dist([r.get("fork_ms") for r in fs]),
            "fork_source_kept_its_bots": all(
                r.get("source_sessions_after") == r.get("source_sessions_before") for r in fs) if fs else None,
            "fork_source_tps_after_min": min((r.get("source_tps_after") or 0 for r in fs), default=None),
        }
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main(sys.argv[1])
