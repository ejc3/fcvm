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
  * AVAILABILITY IS PER ARM, and it gates publication. Every median here is
    computed over successes only, so two arms with different drop rates are
    differently censored and their difference is not a like-for-like comparison.
    Each arm therefore reports attempted/failed with an exact (Clopper-Pearson)
    binomial interval, and any arm that dropped a request is marked
    `publishable: false`. "0 failures" is not a 0% rate: 0/426 is [0, 0.70%].
  * THE LEAK CHECK RUNS OVER EVERY ATTEMPT, NOT OVER THE SUCCESSES. A request
    that failed is exactly the one whose teardown is most likely to have leaked,
    and filtering it out reported `all_gone: 27/27 confirmed` on a set containing
    three teardowns that had explicitly recorded `all_gone: false`.
  * Per-child reclaim CPU is reported PER CHILD. Pooling the children into one
    median and taking the median of {firecracker 110 ms, holder 0, pasta 0}
    publishes 0 — it medians the straggler away, which is the opposite of the
    thing being measured.
"""

import argparse
import json
import math
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


def _log_binom_pmf(i: int, n: int, p: float) -> float:
    if p <= 0.0:
        return 0.0 if i == 0 else -math.inf
    if p >= 1.0:
        return 0.0 if i == n else -math.inf
    return (
        math.lgamma(n + 1) - math.lgamma(i + 1) - math.lgamma(n - i + 1)
        + i * math.log(p) + (n - i) * math.log1p(-p)
    )


def _binom_cdf(k: int, n: int, p: float) -> float:
    """P(X <= k) for X ~ Binomial(n, p). Log-space terms, so no overflow at large n."""
    if k < 0:
        return 0.0
    if k >= n:
        return 1.0
    return sum(math.exp(_log_binom_pmf(i, n, p)) for i in range(k + 1))


def _bisect(f, lo: float, hi: float, iters: int = 80) -> float:
    for _ in range(iters):
        mid = (lo + hi) / 2
        if f(mid) > 0:
            hi = mid
        else:
            lo = mid
    return (lo + hi) / 2


def clopper_pearson(k: int, n: int, conf: float = 0.95):
    """EXACT binomial CI for k events in n trials, as fractions.

    Used for failure rates. "0 failures in 426" is not a 0% failure rate — its
    two-sided 95% upper bound is 0.86%, and quoting the bare 0 reads as a
    guarantee the data does not carry (AGENTS.md defect 6, applied to counts).
    No scipy in this repo, so the tails are summed in log space and inverted by
    bisection rather than pulled from a beta quantile.
    """
    if n <= 0:
        return 0.0, 1.0
    a = (1 - conf) / 2
    lo = 0.0 if k == 0 else _bisect(lambda p: 1 - _binom_cdf(k - 1, n, p) - a, 0.0, k / n)
    hi = 1.0 if k == n else _bisect(lambda p: a - _binom_cdf(k, n, p), k / n, 1.0)
    return lo, hi


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


def main_with(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl", nargs="+")
    ap.add_argument("--json-out", default="")
    args = ap.parse_args(argv)

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
    for r in live:
        if r.get("arm") and r["arm"] not in arms:
            arms.append(r["arm"])
    arms.sort(key=lambda a: {"exec": 0, "cdp": 1, "cdp-fast": 2, "noop": 9}.get(a, 5))
    by = {a: [r for r in ok if r.get("arm") == a] for a in arms}
    # EVERY attempt, not just the successes. The teardown/leak section reads this
    # one: a request that failed is precisely the one whose teardown is most
    # likely to have leaked, and the ok-filter made `all_gone: false` invisible.
    attempted = {a: [r for r in live if r.get("arm") == a] for a in arms}

    out = {"arms": {}, "n_failed": len(bad), "n_warmup_discarded": len(warm)}

    # ---- 0. availability per arm. This gates everything printed below it.
    print("\n" + "-" * 78)
    print("AVAILABILITY PER ARM (medians below are computed over SUCCESSES ONLY)")
    print("-" * 78)
    any_unpublishable = False
    for a in arms:
        n_att = len(attempted[a])
        n_bad = sum(1 for r in attempted[a] if not r.get("ok"))
        lo, hi = clopper_pearson(n_bad, n_att)
        pub = n_bad == 0
        any_unpublishable |= not pub
        note = "" if pub else "   ** DO NOT PUBLISH THIS ARM'S LATENCY **"
        print(f"  {a:9s} {n_att - n_bad}/{n_att} completed   "
              f"failure {100 * n_bad / n_att if n_att else 0:.1f}% "
              f"[{100 * lo:.2f}%, {100 * hi:.2f}%] 95% CP{note}")
        out["arms"].setdefault(a, {}).update(
            attempted=n_att, failed=n_bad,
            failure_rate=(n_bad / n_att) if n_att else None,
            failure_rate_ci=[lo, hi], publishable=pub,
        )
    if any_unpublishable:
        print("  Arms are DIFFERENTLY CENSORED, so a cross-arm delta below is not a")
        print("  like-for-like comparison. Quote the per-arm failure rate next to any")
        print("  number taken from an arm marked DO NOT PUBLISH, or do not quote it.")

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
        v = [r["blocking_ms"] for r in by[a] if r.get("blocking_ms") is not None]
        print(f"  {a:9s} blocking  {fmt(*median_ci(v))}")
        out["arms"].setdefault(a, {})["blocking_ms"] = dict(
            zip(("median", "lo", "hi", "n"), median_ci(v)))
    print("\n  WALL (spawn -> VM fully gone; what the MACHINE pays, not the caller)")
    for a in arms:
        v = [r["wall_ms"] for r in by[a] if r.get("wall_ms") is not None]
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
            # A delta between differently-censored arms is not a like-for-like
            # comparison: each median is conditioned on that arm's successes.
            censor = ""
            if not (out["arms"][a]["publishable"] and out["arms"][b]["publishable"]):
                censor = (f"  [CENSORED: {a} dropped {out['arms'][a]['failed']}/"
                          f"{out['arms'][a]['attempted']}, {b} dropped "
                          f"{out['arms'][b]['failed']}/{out['arms'][b]['attempted']} "
                          f"-- DO NOT PUBLISH without both rates]")
            for metric in ("blocking_ms", "wall_ms"):
                va = [r[metric] for r in by[a] if r.get(metric) is not None]
                vb = [r[metric] for r in by[b] if r.get(metric) is not None]
                d, lo, hi = hodges_lehmann_shift(va, vb)
                if d is None:
                    continue
                sig = "" if (lo <= 0 <= hi) else "  *"
                print(f"  {label}{censor}")
                print(f"    {metric:12s} {a} -> {b}: {d:+.1f} ms  CI [{lo:+.1f}, {hi:+.1f}]{sig}")
                out["deltas"][f"{a}->{b}:{metric}"] = {
                    "delta": d, "ci": [lo, hi],
                    "significant": not (lo <= 0 <= hi),
                    "publishable": censor == "",
                }

    # ---- 4. CDP stage decomposition (the per-request connect cost this design ADDS)
    print("\n" + "-" * 78)
    print("CDP ARMS: per-request stage decomposition (host -> clone over forward-localhost)")
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
        # `--ws-url` prewiring SKIPS /json/list and writes a synthetic
        # `resolve_ms = 0.0`. Pooling that with measured values publishes a stage
        # that was never timed, so the two populations are never mixed.
        prewired = {bool((r.get("render") or {}).get("target_prewired")) for r in by[a]}
        out["arms"][a]["target_prewired"] = sorted(prewired)
        if prewired == {True}:
            print("    resolve_ms       SKIPPED (--ws-url prewired; NOT measured)")
        elif len(prewired) > 1:
            print("    resolve_ms       MIXED prewired/measured records -- refusing to pool")
        for k in stage_keys:
            if k == "resolve_ms" and (prewired == {True} or len(prewired) > 1):
                continue
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
        # `attempted`, NOT `by`: see the module docstring. A failed request's
        # teardown is the one most likely to have leaked.
        td = [r.get("teardown") for r in attempted[a] if r.get("teardown")]
        if not td:
            print(f"  {a:9s} (teardown is inside fcvm and not separable in this arm)")
            continue
        ok_td = [r.get("teardown") for r in by[a] if r.get("teardown")]
        rw = [t.get("reap_wall_ms") for t in ok_td if t.get("reap_wall_ms") is not None]
        tt = [t.get("teardown_total_ms") for t in ok_td if t.get("teardown_total_ms") is not None]
        print(f"  [{a}] mode={td[0].get('mode')}")
        if rw:
            print(f"    reap_wall_ms      {fmt(*median_ci(rw))}   (kill -> processes truly gone)")
            print(f"    teardown_total_ms {fmt(*median_ci(tt))}   (incl. synchronous on-disk reap)")
            out["arms"][a]["reap_wall_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(rw)))
        dr = [t.get("disk_reap_ms") for t in ok_td if t.get("disk_reap_ms") is not None]
        if dr:
            print(f"    disk_reap_ms      {fmt(*median_ci(dr))}   (state file + data dir)")
            out["arms"][a]["disk_reap_ms"] = dict(zip(("median", "lo", "hi", "n"), median_ci(dr)))

        # machine_cpu_ms_excess is a whole-machine subtraction whose control window
        # and reclaim window carry different amounts of the HARNESS's own load. It
        # is only reported when the record proves the harness subtracted itself out
        # of both (`harness_cpu_ms` present) — older records cannot support it.
        mc = [t.get("machine_cpu_ms_excess") for t in ok_td
              if t.get("machine_cpu_ms_excess") is not None
              and t.get("harness_cpu_ms") is not None]
        stale = [t for t in ok_td if t.get("machine_cpu_ms_excess") is not None
                 and t.get("harness_cpu_ms") is None]
        if stale:
            print(f"    machine_cpu_excess WITHHELD on {len(stale)} record(s): they predate "
                  f"harness self-CPU subtraction, so the ambient baseline includes the "
                  f"sampler's own spin and the value is not an attribution")
        if mc:
            print(f"    machine_cpu_excess {fmt(*median_ci(mc))}   "
                  f"(whole-machine busy jiffies over reclaim, ambient AND harness subtracted)")
            out["arms"][a]["machine_cpu_ms_excess"] = dict(
                zip(("median", "lo", "hi", "n"), median_ci(mc)))

        # PER CHILD, keyed by comm. Pooling loses the straggler: the median of
        # {firecracker 110 ms, holder 0, pasta 0} is 0.
        tick = next((t.get("tick_ms") for t in td if t.get("tick_ms")), None)
        by_child: dict = {}
        for t in ok_td:
            for cname, c in (t.get("per_child_cpu") or {}).items():
                if c.get("reclaim_cpu_ms") is None:
                    continue
                b = by_child.setdefault(cname, {"complete": [], "lower": [], "sub_tick": 0})
                (b["complete"] if c.get("complete") else b["lower"]).append(c["reclaim_cpu_ms"])
                if c.get("below_resolution") or c["reclaim_cpu_ms"] == 0.0:
                    b["sub_tick"] += 1
        if by_child:
            res = f" (/proc tick = {tick:.0f} ms)" if tick else ""
            print(f"    reclaim_cpu_ms per child{res}")
            out["arms"][a]["reclaim_cpu_ms_by_child"] = {}
            for cname in sorted(by_child):
                b = by_child[cname]
                n_tot = len(b["complete"]) + len(b["lower"])
                if b["sub_tick"] == n_tot and tick:
                    print(f"      {cname:20s} < {2 * tick:.0f} ms "
                          f"(below /proc tick resolution, n={n_tot})")
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname] = {
                        "median": 0.0, "below_resolution": True,
                        "upper_bound_ms": 2 * tick, "n": n_tot,
                    }
                    continue
                if b["complete"]:
                    m = median_ci(b["complete"])
                    print(f"      {cname:20s} {fmt(*m)}   COMPLETE (zombie observed)")
                    out["arms"][a]["reclaim_cpu_ms_by_child"][cname] = dict(
                        zip(("median", "lo", "hi", "n"), m))
                if b["lower"]:
                    m = median_ci(b["lower"])
                    print(f"      {cname:20s} {fmt(*m)}   LOWER BOUND (reaper won the race)")
                    out["arms"][a]["reclaim_cpu_ms_by_child"].setdefault(cname, {})[
                        "lower_bound"] = dict(zip(("median", "lo", "hi", "n"), m))

        ag = [t.get("all_gone") for t in td]
        leaked = ag.count(False)
        print(f"    all_gone: {ag.count(True)}/{len(ag)} confirmed"
              + (f"  ** {leaked} NOT CONFIRMED GONE **" if leaked else ""))
        out["arms"][a]["all_gone_confirmed"] = [ag.count(True), len(ag)]
        if leaked:
            for r in attempted[a]:
                t = r.get("teardown") or {}
                if t.get("all_gone") is False:
                    print(f"      rep {r.get('rep')} ok={r.get('ok')} "
                          f"survivors={t.get('survivors') or t.get('children')}")

    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump(out, f, indent=2, default=str)
        print(f"\nwrote {args.json_out}")
    return 0


def main():
    return main_with()


if __name__ == "__main__":
    sys.exit(main())
