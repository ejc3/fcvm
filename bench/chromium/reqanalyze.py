#!/usr/bin/env python3
"""Analysis for the request-optimized A/B. Medians with bootstrap CIs, drift, teardown.

Every rule here exists because of a defect listed in bench/chromium/AGENTS.md:

  * Warmups are dropped EXPLICITLY and the count is printed (defect: cold-cache
    runs inflate p50 silently).
  * Uncertainty is a bootstrap CI on the MEDIAN, and every figure is rounded to
    its own CI (defect 6: "129.0 MiB" on +/-20 MiB data is a false claim).
  * The `noop` drift-control series is split first-half/second-half and the
    difference is reported. If the control moved, the arm deltas are suspect and
    this script says so rather than letting the reader assume a quiet box
    (defect 2: a control probe drifted 631 -> 706 ms in the retracted run).
  * Teardown is never one number. blocking / reap_wall / reclaim_cpu are
    reported separately, and CPU figures flagged `complete=False` are counted as
    LOWER BOUNDS and excluded from the headline rather than averaged in.
"""

import argparse
import json
import math
import os
import random
import statistics
import sys


def median_ci(xs, iters=20000, conf=0.95, seed=12345):
    """Median with a percentile-bootstrap CI. Returns (median, lo, hi, n)."""
    xs = [float(x) for x in xs if x is not None and not math.isnan(float(x))]
    n = len(xs)
    if n == 0:
        return None, None, None, 0
    med = statistics.median(xs)
    if n < 3:
        return med, min(xs), max(xs), n
    rng = random.Random(seed)
    boots = []
    for _ in range(iters):
        boots.append(statistics.median([xs[rng.randrange(n)] for _ in range(n)]))
    boots.sort()
    lo = boots[int((1 - conf) / 2 * iters)]
    hi = boots[int((1 + conf) / 2 * iters) - 1]
    return med, lo, hi, n


def fmt(med, lo, hi, n=None, unit="ms"):
    """Round the estimate to the precision its own CI can support (defect 6)."""
    if med is None:
        return "n/a"
    half = max(abs(hi - med), abs(med - lo)) if lo is not None else 0.0
    digits = 0 if half >= 5 else (1 if half >= 0.5 else 2)
    s = f"{med:.{digits}f} [{lo:.{digits}f}, {hi:.{digits}f}] {unit}"
    return s + (f" n={n}" if n else "")


def load(paths):
    recs, metas = [], []
    for p in paths:
        with open(p) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                r = json.loads(line)
                (metas if r.get("kind") == "meta" else recs).append(r)
    return recs, metas


def hodges_lehmann_shift(a, b, iters=20000, seed=999):
    """Bootstrap CI on the difference of medians (b - a). Sign matters, so report it."""
    a = [float(x) for x in a]
    b = [float(x) for x in b]
    if not a or not b:
        return None, None, None
    rng = random.Random(seed)
    d = statistics.median(b) - statistics.median(a)
    boots = []
    for _ in range(iters):
        sa = [a[rng.randrange(len(a))] for _ in range(len(a))]
        sb = [b[rng.randrange(len(b))] for _ in range(len(b))]
        boots.append(statistics.median(sb) - statistics.median(sa))
    boots.sort()
    return d, boots[int(0.025 * iters)], boots[int(0.975 * iters) - 1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl", nargs="+")
    ap.add_argument("--json-out", default="")
    args = ap.parse_args()

    recs, metas = load(args.jsonl)
    live = [r for r in recs if not r.get("warmup")]
    warm = [r for r in recs if r.get("warmup")]
    ok = [r for r in live if r.get("ok")]
    bad = [r for r in live if not r.get("ok")]

    print("=" * 78)
    print("REQUEST-OPTIMIZED A/B  --  medians with 95% bootstrap CIs")
    print("=" * 78)
    for m in metas:
        print(f"  seed={m.get('seed')} arms={m.get('arms')} reps={m.get('reps')} "
              f"warmup={m.get('warmup')} url={m.get('url')}")
        print(f"  loadavg at start: {m.get('loadavg')}")
    print(f"\n  records: {len(recs)}  warmup DISCARDED: {len(warm)}  "
          f"measured: {len(live)}  ok: {len(ok)}  failed: {len(bad)}")
    if bad:
        seen = {}
        for r in bad:
            seen.setdefault(r.get("error", f"rc={r.get('rc')}"), 0)
            seen[r.get("error", f"rc={r.get('rc')}")] += 1
        for k, v in sorted(seen.items(), key=lambda kv: -kv[1]):
            print(f"    FAILURE x{v}: {k}")

    la = [r.get("loadavg1") for r in live if r.get("loadavg1") is not None]
    if la:
        print(f"  loadavg1 during run: min={min(la):.1f} median={statistics.median(la):.1f} "
              f"max={max(la):.1f}   <-- contention check (AGENTS.md)")

    arms = []
    for r in ok:
        if r["arm"] not in arms:
            arms.append(r["arm"])
    arms.sort(key=lambda a: {"exec": 0, "cdp": 1, "cdp-fast": 2, "noop": 9}.get(a, 5))
    by = {a: [r for r in ok if r["arm"] == a] for a in arms}

    out = {"arms": {}, "n_failed": len(bad), "n_warmup_discarded": len(warm)}

    # ---- 1. drift control FIRST. If this moved, nothing below is trustworthy.
    print("\n" + "-" * 78)
    print("DRIFT CONTROL (arm=noop: clone spawn + restore + teardown, NO page, NO CDP)")
    print("-" * 78)
    noop = by.get("noop", [])
    if len(noop) >= 6:
        noop_sorted = sorted(noop, key=lambda r: r["rep"])
        half = len(noop_sorted) // 2
        f = [r["blocking_ms"] for r in noop_sorted[:half]]
        s = [r["blocking_ms"] for r in noop_sorted[half:]]
        d, dlo, dhi = hodges_lehmann_shift(f, s)
        print(f"  first half : {fmt(*median_ci(f))}")
        print(f"  second half: {fmt(*median_ci(s))}")
        print(f"  drift (2nd - 1st): {d:.1f} ms, 95% CI [{dlo:.1f}, {dhi:.1f}]")
        drifted = not (dlo <= 0 <= dhi)
        print("  VERDICT: " + ("DRIFT DETECTED -- arm deltas are confounded, do not publish"
                               if drifted else
                               "no significant drift; interleaved arm deltas are usable"))
        out["drift"] = {"delta_ms": d, "ci": [dlo, dhi], "significant": drifted}
    else:
        print("  (insufficient noop samples)")

    # ---- 2. end-to-end, measured end to end, never composed from stages
    print("\n" + "-" * 78)
    print("END-TO-END, CALLER-BLOCKING (spawn -> answer in hand). Measured, not composed.")
    print("-" * 78)
    for a in arms:
        v = [r["blocking_ms"] for r in by[a]]
        print(f"  {a:9s} blocking  {fmt(*median_ci(v))}")
        out["arms"].setdefault(a, {})["blocking_ms"] = dict(
            zip(("median", "lo", "hi", "n"), median_ci(v)))
    print("\n  WALL (spawn -> VM fully gone; what the MACHINE pays, not the caller)")
    for a in arms:
        v = [r["wall_ms"] for r in by[a]]
        print(f"  {a:9s} wall      {fmt(*median_ci(v))}")
        out["arms"][a]["wall_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(v)))

    # ---- 3. paired deltas with CIs
    print("\n" + "-" * 78)
    print("DELTAS (negative = faster). CI crossing zero = NOT a result.")
    print("-" * 78)
    pairs = [("exec", "cdp", "PART 1: request transport (exec'd python -> host CDP)"),
             ("cdp", "cdp-fast", "PART 2: teardown discipline (awaited -> early response)"),
             ("exec", "cdp-fast", "BOTH parts combined")]
    out["deltas"] = {}
    for a, b, label in pairs:
        if a in by and b in by:
            print(f"  {label}")
            for metric in ("blocking_ms", "wall_ms"):
                va = [r[metric] for r in by[a]]
                vb = [r[metric] for r in by[b]]
                d, lo, hi = hodges_lehmann_shift(va, vb)
                sig = "" if (lo <= 0 <= hi) else "  *"
                print(f"    {metric:12s} {a} -> {b}: {d:+.1f} ms  CI [{lo:+.1f}, {hi:+.1f}]{sig}")
                out["deltas"][f"{a}->{b}:{metric}"] = {"delta": d, "ci": [lo, hi],
                                                       "significant": not (lo <= 0 <= hi)}

    # ---- 4. CDP stage decomposition (the per-request connect cost this design ADDS)
    print("\n" + "-" * 78)
    print("CDP ARMS: per-request stage decomposition (host -> clone via socat relay + published port)")
    print("-" * 78)
    stage_keys = ["resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms", "connect_total_ms",
                  "navigate_ms", "screenshot_ms", "total_ms"]
    for a in [x for x in arms if x.startswith("cdp")]:
        print(f"  [{a}]")
        pw = [r.get("port_wait_ms") for r in by[a] if r.get("port_wait_ms") is not None]
        if pw:
            print(f"    {'port_wait_ms':16s} {fmt(*median_ci(pw))}   "
                  f"(restore -> first TCP accept; the ONLY readiness wait)")
            out["arms"][a]["port_wait_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(pw)))
        for k in stage_keys:
            # cdpdrive nests its timings under render.stages; reading render[k]
            # directly silently yields nothing, which looks like "no data" rather
            # than a bug. Read both, prefer stages.
            v = [(r.get("render", {}).get("stages") or {}).get(k)
                 if (r.get("render", {}).get("stages") or {}).get(k) is not None
                 else r.get("render", {}).get(k)
                 for r in by[a]]
            v = [x for x in v if x is not None]
            if v:
                print(f"    {k:16s} {fmt(*median_ci(v))}")
                out["arms"][a][k] = dict(zip(("median", "lo", "hi", "n"), median_ci(v)))
    if "exec" in by:
        v = [r.get("render_total_ms") for r in by["exec"] if r.get("render_total_ms")]
        if v:
            print(f"  [exec] in-guest render.py total_ms {fmt(*median_ci(v))}")
            out["arms"]["exec"]["render_total_ms"] = dict(
                zip(("median", "lo", "hi", "n"), median_ci(v)))

    # ---- 5. teardown, three separate numbers, never one
    print("\n" + "-" * 78)
    print("TEARDOWN -- three numbers, deliberately not summed into one")
    print("-" * 78)
    for a in arms:
        td = [r.get("teardown") for r in by[a] if r.get("teardown")]
        if not td:
            print(f"  {a:9s} (teardown is inside fcvm and not separable in this arm)")
            continue
        rw = [t.get("reap_wall_ms") for t in td if t.get("reap_wall_ms") is not None]
        tt = [t.get("teardown_total_ms") for t in td if t.get("teardown_total_ms") is not None]
        print(f"  [{a}] mode={td[0].get('mode')}")
        print(f"    reap_wall_ms      {fmt(*median_ci(rw))}   (kill -> processes truly gone)")
        print(f"    teardown_total_ms {fmt(*median_ci(tt))}   (incl. synchronous on-disk reap)")
        out["arms"][a]["reap_wall_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(rw)))
        dr = [t.get("disk_reap_ms") for t in td if t.get("disk_reap_ms") is not None]
        if dr:
            print(f"    disk_reap_ms      {fmt(*median_ci(dr))}   (state file + data dir)")
            out["arms"][a]["disk_reap_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(dr)))
        mc = [t.get("machine_cpu_ms_excess") for t in td
              if t.get("machine_cpu_ms_excess") is not None]
        if mc:
            print(f"    machine_cpu_excess{fmt(*median_ci(mc))}   "
                  f"(whole-machine busy jiffies over reclaim, ambient subtracted)")
            out["arms"][a]["machine_cpu_ms_excess"] = dict(
                zip(("median", "lo", "hi", "n"), median_ci(mc)))
        comp, incomp = [], []
        for t in td:
            for _cname, c in (t.get("per_child_cpu") or {}).items():
                if c.get("reclaim_cpu_ms") is None:
                    continue
                (comp if c.get("complete") else incomp).append(c["reclaim_cpu_ms"])
        if comp:
            print(f"    reclaim_cpu_ms    {fmt(*median_ci(comp))}   "
                  f"COMPLETE (zombie state observed, exit_mm already ran)")
            out["arms"][a]["reclaim_cpu_ms_complete"] = dict(
                zip(("median", "lo", "hi", "n"), median_ci(comp)))
        if incomp:
            print(f"    reclaim_cpu_ms    {fmt(*median_ci(incomp))}   "
                  f"LOWER BOUND ONLY (reaper won the race) -- not merged with the above")
            out["arms"][a]["reclaim_cpu_ms_lower_bound"] = dict(
                zip(("median", "lo", "hi", "n"), median_ci(incomp)))
        ag = [t.get("all_gone") for t in td]
        leaked = ag.count(False)
        print(f"    all_gone: {ag.count(True)}/{len(ag)} confirmed"
              + (f"  ** {leaked} NOT CONFIRMED GONE **" if leaked else ""))
        out["arms"][a]["all_gone_confirmed"] = [ag.count(True), len(ag)]

    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump(out, f, indent=2, default=str)
        print(f"\nwrote {args.json_out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
