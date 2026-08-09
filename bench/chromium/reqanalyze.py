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
    `publishable: false`. "0 failures" is not a 0% rate: 0/426 is [0, 0.86%].
    (This line said 0.70% while `clopper_pearson`'s own docstring 80 lines below
    said 0.86% for the same k/n — two point estimates of one quantity disagreeing
    inside one file, which is defect 6's cousin and exactly what the AGENTS.md
    `snapshot-load` entry was written about. The function computes 0.862%.)
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


MIN_CDP_ATTEMPTS_PER_BACKEND = 200


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
    """Load each artifact with the backend declared by that artifact.

    A record does not repeat the backend on every line; its enclosing JSONL file
    supplies that identity.  Keeping files separate here prevents a `file` run
    and a `uffd` run passed in one invocation from becoming one synthetic sample.
    """
    datasets = []
    for p in paths:
        recs, metas = [], []
        with open(p) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                r = json.loads(line)
                (metas if r.get("kind") == "meta" else recs).append(r)

        errors = []
        declared = []
        if not metas:
            errors.append("no metadata record declares this file's backend")
        for i, meta in enumerate(metas):
            backend = meta.get("backend")
            if not isinstance(backend, str) or not backend.strip():
                errors.append(f"metadata record {i} has no backend")
            else:
                declared.append(backend.strip())
        backends = sorted(set(declared))
        if len(backends) > 1:
            errors.append("file declares multiple backends: " + ", ".join(backends))
        backend = backends[0] if len(backends) == 1 and not errors else None
        datasets.append({
            "backend": backend,
            "records": recs,
            "metas": metas,
            "sources": [p],
            "metadata_errors": errors,
        })
    return datasets


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


def failure_label(r: dict) -> str:
    """The failure string an operator needs, wherever the producer put it.

    `r.get("error", f"rc={r.get('rc')}")` alone reported every CDP drop as
    `rc=None`: `run_cdp_request` nests the whole cdpdrive result under `render`,
    and on a clean `ok: false` (cdpdrive returning rather than raising) the top
    level carried no `error` at all. The producer now lifts the label, but the
    analyzer is the PUBLICATION GATE reading a JSONL artifact that outlives the
    producer, so it reads both and prefers the top level.
    """
    return (
        r.get("error")
        or (r.get("render") or {}).get("error")
        or f"rc={r.get('rc')}"
    )


def failure_class(r: dict) -> str:
    """Transport / readiness / render, wherever the producer put it."""
    return (
        r.get("failure_class")
        or (r.get("render") or {}).get("failure_class")
        or "unknown"
    )


def analyze_backend(recs, metas, backend, sources, metadata_errors):
    live = [r for r in recs if not r.get("warmup")]
    warm = [r for r in recs if r.get("warmup")]
    ok = [r for r in live if r.get("ok")]
    bad = [r for r in live if not r.get("ok")]

    print("=" * 78)
    print("REQUEST-OPTIMIZED A/B  --  medians with 95% bootstrap CIs")
    print("=" * 78)
    print(f"  backend={backend or 'UNASSIGNED'}")
    for source in sources:
        print(f"  input={source}")
    for error in metadata_errors:
        print(f"  BACKEND METADATA ERROR: {error}")
    for m in metas:
        print(f"  seed={m.get('seed')} arms={m.get('arms')} reps={m.get('reps')} "
              f"warmup={m.get('warmup')} url={m.get('url')}")
        print(f"  loadavg at start: {m.get('loadavg')}")
    print(f"\n  records: {len(recs)}  warmup DISCARDED: {len(warm)}  "
          f"measured: {len(live)}  ok: {len(ok)}  failed: {len(bad)}")
    if bad:
        seen: dict = {}
        for r in bad:
            seen[failure_label(r)] = seen.get(failure_label(r), 0) + 1
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

    out = {
        "backend": backend,
        "sources": sources,
        "metadata_errors": metadata_errors,
        "arms": {},
        "n_failed": len(bad),
        "n_warmup_discarded": len(warm),
    }

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
        # A transport drop, a readiness exhaustion and a render error are three
        # different defects and must not share one bucket in the artifact. This is
        # what makes REVIEW.md's "200 CDP requests per backend at 0 failures" gate
        # checkable from the analyzer's own output instead of by hand-reading
        # jsonl — and `failure_class` was, until now, written by cdpdrive and read
        # by nothing at all.
        classes: dict = {}
        for r in attempted[a]:
            if not r.get("ok"):
                c = failure_class(r)
                classes[c] = classes.get(c, 0) + 1
        if classes:
            print("            failure classes: "
                  + ", ".join(f"{k}={v}" for k, v in sorted(classes.items())))
        out["arms"].setdefault(a, {}).update(
            attempted=n_att, failed=n_bad,
            failure_rate=(n_bad / n_att) if n_att else None,
            failure_rate_ci=[lo, hi], publishable=pub,
            failure_classes=classes,
        )
    if any_unpublishable:
        print("  Arms are DIFFERENTLY CENSORED, so a cross-arm delta below is not a")
        print("  like-for-like comparison. Quote the per-arm failure rate next to any")
        print("  number taken from an arm marked DO NOT PUBLISH, or do not quote it.")

    # ---- 0b. sample size per backend and per CDP arm. Metadata names the arms
    # the producer intended to run, so an aborted arm with zero records cannot
    # disappear from the gate merely by disappearing from the observed records.
    expected_cdp_arms = set()
    for meta in metas:
        for arm in meta.get("arms") or []:
            if isinstance(arm, str) and arm.startswith("cdp"):
                expected_cdp_arms.add(arm)
    if not expected_cdp_arms:
        expected_cdp_arms.update(a for a in arms if a.startswith("cdp"))
    expected_cdp_arms = sorted(expected_cdp_arms)
    cdp_counts = {a: len(attempted.get(a, [])) for a in expected_cdp_arms}
    cdp_short = {
        a: count for a, count in cdp_counts.items()
        if count < MIN_CDP_ATTEMPTS_PER_BACKEND
    }
    cdp_sample_passed = bool(expected_cdp_arms) and not cdp_short
    print("\n" + "-" * 78)
    print("CDP SAMPLE-SIZE GATE (measured, non-warmup attempts; evaluated per backend)")
    print("-" * 78)
    if not expected_cdp_arms:
        print("  FAIL: no CDP arm is declared or observed")
    for arm in expected_cdp_arms:
        count = cdp_counts[arm]
        verdict = "PASS" if count >= MIN_CDP_ATTEMPTS_PER_BACKEND else "FAIL"
        print(f"  {arm:9s} {count}/{MIN_CDP_ATTEMPTS_PER_BACKEND} attempts  {verdict}")
    out["cdp_sample_size"] = {
        "required_per_arm": MIN_CDP_ATTEMPTS_PER_BACKEND,
        "expected_arms": expected_cdp_arms,
        "measured_non_warmup_attempts_per_arm": cdp_counts,
        "passed": cdp_sample_passed,
    }

    # ---- 1. drift control FIRST. If this moved, nothing below is trustworthy.
    print("\n" + "-" * 78)
    print("DRIFT CONTROL (arm=noop: clone spawn + restore + teardown, NO page, NO CDP)")
    print("-" * 78)
    noop = by.get("noop", [])
    drifted = False
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
        out["drift"] = {
            "evaluated": True, "n": len(noop), "delta_ms": d,
            "ci": [dlo, dhi], "significant": drifted, "passed": not drifted,
        }
    else:
        print("  (insufficient noop samples)")
        out["drift"] = {
            "evaluated": False, "n": len(noop), "delta_ms": None,
            "ci": None, "significant": False, "passed": True,
        }

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

    # ---- 4. CDP stage decomposition
    print("\n" + "-" * 78)
    # NOT "forward-localhost". That flag is GUEST -> HOST (bench/chromium/AGENTS.md,
    # src/cli/args.rs: "Enables containers to reach host-only services via
    # localhost"), so it cannot carry this path in this direction — and pointing it
    # at the CDP port HIJACKS the guest's own loopback listener. What is actually
    # measured is `--publish 9222:9222`; fc-agent DNATs that eligible TCP port to
    # guest loopback. reqbench.sh never passes --forward-localhost and there is no
    # benchmark-owned relay in the path.
    print("CDP ARMS: per-request stage decomposition (host -> --publish -> guest loopback)")
    print("-" * 78)
    stage_keys = ["resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms", "connect_total_ms",
                  "navigate_ms", "screenshot_ms", "total_ms"]
    for a in [x for x in arms if x.startswith("cdp")]:
        print(f"  [{a}]")
        for metric, explanation in (
            ("spawn_to_port_ms", "process spawn -> first TCP accept; stable readiness boundary"),
            ("state_to_port_ms", "first state discovery -> first TCP accept; diagnostic only"),
        ):
            values = [r.get(metric) for r in by[a] if r.get(metric) is not None]
            if values:
                print(f"    {metric:16s} {fmt(*median_ci(values))}   ({explanation})")
                out["arms"][a][metric] = dict(
                    zip(("median", "lo", "hi", "n"), median_ci(values)))
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
    any_unconfirmed_teardown = False
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

        # CLASSIFY ON True, NOT ON False. `ag.count(False)` drove the warning
        # gate while `len(ag)` drove the denominator, so a null or an absent
        # `all_gone` landed in the denominator and OUT of the gate: 27 confirmed
        # + 2 null + 1 missing printed `all_gone: 27/30 confirmed` with no
        # warning and no per-rep dump — the numerator and denominator disagreeing
        # by three while the report reads clean. Exactly the failure mode this
        # section exists to prevent (see the module docstring). Today's producer
        # cannot emit a non-bool here, but the analyzer is the publication gate
        # for an artifact that outlives the producer.
        ag = [t.get("all_gone") for t in td]
        confirmed = ag.count(True)
        unconfirmed = len(ag) - confirmed
        print(f"    all_gone: {confirmed}/{len(ag)} confirmed"
              + (f"  ** {unconfirmed} NOT CONFIRMED GONE **" if unconfirmed else ""))
        out["arms"][a]["all_gone_confirmed"] = [confirmed, len(ag)]
        # Kept separate so "we watched it survive" is distinguishable from "we
        # have no evidence either way".
        out["arms"][a]["all_gone_no_evidence"] = sum(
            1 for x in ag if x is not True and x is not False)
        if unconfirmed:
            any_unconfirmed_teardown = True
            for r in attempted[a]:
                t = r.get("teardown") or {}
                if t.get("all_gone") is not True:
                    print(f"      rep {r.get('rep')} ok={r.get('ok')} "
                          f"all_gone={t.get('all_gone')!r} "
                          f"survivors={t.get('survivors') or t.get('children')}")

    failed_by_arm = {
        arm: data["failed"] for arm, data in out["arms"].items()
        if data.get("failed")
    }
    unconfirmed_teardowns = sum(
        confirmed[1] - confirmed[0]
        for data in out["arms"].values()
        if (confirmed := data.get("all_gone_confirmed"))
    )
    reasons = []
    if metadata_errors or backend is None:
        reasons.append("backend metadata does not assign every input file to one backend")
    if any_unpublishable:
        reasons.append("one or more arms dropped requests")
    if not expected_cdp_arms:
        reasons.append("no measured CDP arm was declared or observed")
    for arm, count in cdp_short.items():
        reasons.append(
            f"CDP arm {arm} has {count}/{MIN_CDP_ATTEMPTS_PER_BACKEND} "
            "measured non-warmup attempts"
        )
    if drifted:
        reasons.append("baseline drift was detected in the noop control")
    if any_unconfirmed_teardown:
        reasons.append("one or more teardowns were not confirmed gone")

    out["gate"] = {
        "passed": not reasons,
        "reasons": reasons,
        "backend_metadata": {
            "passed": backend is not None and not metadata_errors,
            "backend": backend,
            "sources": sources,
            "errors": metadata_errors,
        },
        "availability": {
            "passed": not any_unpublishable,
            "failed_attempts_per_arm": failed_by_arm,
        },
        "cdp_sample_size": out["cdp_sample_size"],
        "baseline_drift": out["drift"],
        "teardown": {
            "passed": not any_unconfirmed_teardown,
            "unconfirmed": unconfirmed_teardowns,
        },
    }
    out["publishable"] = out["gate"]["passed"]
    return out


def main_with(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl", nargs="+")
    ap.add_argument("--json-out", default="")
    ap.add_argument("--no-gate", action="store_true",
                    help="exit 0 even when this run is unpublishable (exploratory only)")
    args = ap.parse_args(argv)

    # Valid files with the same backend can contribute to one backend sample.
    # Files from different backends, and files whose metadata is invalid, never
    # share an analysis population.
    groups = []
    valid_group = {}
    invalid_number = 0
    for dataset in load(args.jsonl):
        backend = dataset["backend"]
        if backend is None:
            invalid_number += 1
            group = dict(dataset)
            group["key"] = f"unassigned-{invalid_number}"
            groups.append(group)
            continue
        if backend not in valid_group:
            group = {
                "key": backend,
                "backend": backend,
                "records": [],
                "metas": [],
                "sources": [],
                "metadata_errors": [],
            }
            valid_group[backend] = group
            groups.append(group)
        group = valid_group[backend]
        for field in ("records", "metas", "sources", "metadata_errors"):
            group[field].extend(dataset[field])

    backend_results = {}
    for group in groups:
        backend_results[group["key"]] = analyze_backend(
            group["records"], group["metas"], group["backend"],
            group["sources"], group["metadata_errors"],
        )

    overall_reasons = [
        f"{key}: {reason}"
        for key, result in backend_results.items()
        for reason in result["gate"]["reasons"]
    ]
    publishable = not overall_reasons
    exit_code_overridden = bool(args.no_gate and not publishable)
    if len(backend_results) == 1:
        # Retain the original convenient top-level `arms` report for callers
        # analyzing one backend, while making the backend boundary explicit.
        result = dict(next(iter(backend_results.values())))
        result["backends"] = backend_results
        result["gate"] = dict(result["gate"])
        result["gate"]["exit_code_overridden"] = exit_code_overridden
    else:
        result = {
            "backends": backend_results,
            "publishable": publishable,
            "gate": {
                "passed": publishable,
                "reasons": overall_reasons,
                "exit_code_overridden": exit_code_overridden,
            },
        }

    # The override changes process control only. It cannot rewrite the evidence
    # or turn an exploratory run into a publishable one in the JSON artifact.
    result["publishable"] = publishable
    result["gate"]["passed"] = publishable
    result["gate"]["reasons"] = overall_reasons

    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump(result, f, indent=2, default=str)
        print(f"\nwrote {args.json_out}")

    if not publishable:
        print("\nUNPUBLISHABLE RUN:")
        for reason in overall_reasons:
            print(f"  - {reason}")
        if args.no_gate:
            print("Exit status overridden by --no-gate; publishable remains false.")
            return 0
        print("Exiting 5. Pass --no-gate to override only the exit status.")
        return 5
    return 0


def main():
    return main_with()


if __name__ == "__main__":
    sys.exit(main())
