#!/usr/bin/env python3
"""Corrected-run statistics for the chromium shared-nothing bench.

Everything the first run's adversarial review refuted is computed here with the
defect fixed, and every number carries an uncertainty:

  defect 1  memory density on a MATCHED basis (per-clone cgroup covering the
            whole process set, plus a whole-machine MemAvailable delta), with
            the host-native container pool measured the same way
  defect 2  egress modes drawn from ONE seeded interleaved schedule; the
            control probes quantify the wall-clock drift that a block design
            would have aliased onto the mode factor
  defect 3  the BURST is the experimental unit; clones inside a burst are
            pseudoreplicates. Medians with bootstrap CIs across bursts
  defect 4  exec-stage waiting attributed from the exec client's own retry
            counters (RUST_LOG=fcvm=debug), not inferred
  defect 5  slope AND intercept with standard errors, over a stated N range,
            with req/GiB quoted at CONCRETE concurrency levels
  defect 6  every figure quoted with an uncertainty and rounded to it

stdlib only, like the rest of the harness.
"""

import argparse
import bisect
import glob
import json
import math
import os
import random
import statistics
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from reqanalyze import clopper_pearson  # noqa: E402 - sibling module, one exact-binomial impl

BOOT_N = 10000
BOOT_SEED = 20260808
# The sustained phase runs ONE window per cell, so no rate interval is derivable
# from it as collected. Splitting that window is the best available and is
# labelled as not-a-replicate; the durable fix is to replicate the window.
SUSTAINED_SUBWINDOWS = 5


# ---------------------------------------------------------------------------
# uncertainty helpers (defect 6)
# ---------------------------------------------------------------------------
def boot_ci(vals, stat=statistics.median, n=BOOT_N, seed=BOOT_SEED, alpha=0.05):
    """Percentile bootstrap CI for `stat`. Returns (point, lo, hi, n_obs)."""
    vals = [v for v in vals if v is not None]
    if not vals:
        return None, None, None, 0
    if len(vals) == 1:
        return vals[0], None, None, 1
    rng = random.Random(seed)
    k = len(vals)
    reps = []
    for _ in range(n):
        reps.append(stat([vals[rng.randrange(k)] for _ in range(k)]))
    reps.sort()
    lo = reps[int(alpha / 2 * n)]
    hi = reps[min(n - 1, int((1 - alpha / 2) * n))]
    return stat(vals), lo, hi, k


def boot_diff_ci(a, b, stat=statistics.median, n=BOOT_N, seed=BOOT_SEED, alpha=0.05):
    """Bootstrap CI for stat(a) - stat(b) (independent resampling)."""
    a = [v for v in a if v is not None]
    b = [v for v in b if v is not None]
    if len(a) < 2 or len(b) < 2:
        return None, None, None
    rng = random.Random(seed)
    reps = []
    for _ in range(n):
        sa = [a[rng.randrange(len(a))] for _ in range(len(a))]
        sb = [b[rng.randrange(len(b))] for _ in range(len(b))]
        reps.append(stat(sa) - stat(sb))
    reps.sort()
    return stat(a) - stat(b), reps[int(alpha / 2 * n)], reps[min(n - 1, int((1 - alpha / 2) * n))]


def fmt(v, unc=None, unit="", sig=None):
    """Round a value TO its uncertainty (defect 6: no false precision)."""
    if v is None:
        return "n/a"
    if unc is None or unc == 0 or not math.isfinite(unc):
        return f"{v:.0f}{unit}" if abs(v) >= 100 else f"{v:.3g}{unit}"
    # first significant digit of the uncertainty sets the decimal place
    d = max(0, -int(math.floor(math.log10(abs(unc)))))
    return f"{v:.{d}f}{unit}"


def pm(v, lo, hi, unit=""):
    """'x (95% CI a-b)' with x rounded to the CI half-width."""
    if v is None:
        return "n/a"
    if lo is None or hi is None:
        return f"{v:.3g}{unit} (n=1, no CI)"
    half = (hi - lo) / 2 or 1e-9
    return f"{fmt(v, half)}{unit} (95% CI {fmt(lo, half)}-{fmt(hi, half)})"


# ---------------------------------------------------------------------------
# OLS with standard errors (defect 5) — normal equations, Gauss-Jordan inverse
# ---------------------------------------------------------------------------
def _inv(m):
    n = len(m)
    a = [row[:] + [1.0 if i == j else 0.0 for j in range(n)] for i, row in enumerate(m)]
    for c in range(n):
        p = max(range(c, n), key=lambda r: abs(a[r][c]))
        if abs(a[p][c]) < 1e-12:
            return None
        a[c], a[p] = a[p], a[c]
        piv = a[c][c]
        a[c] = [x / piv for x in a[c]]
        for r in range(n):
            if r == c:
                continue
            f = a[r][c]
            if f:
                a[r] = [x - f * y for x, y in zip(a[r], a[c])]
    return [row[n:] for row in a]


def ols(X, y):
    """Returns (beta, se, r2, n). X includes its own intercept column."""
    n, k = len(X), len(X[0])
    if n <= k:
        return None
    xtx = [[sum(X[i][a] * X[i][b] for i in range(n)) for b in range(k)] for a in range(k)]
    xty = [sum(X[i][a] * y[i] for i in range(n)) for a in range(k)]
    inv = _inv(xtx)
    if inv is None:
        return None
    beta = [sum(inv[a][b] * xty[b] for b in range(k)) for a in range(k)]
    resid = [y[i] - sum(X[i][a] * beta[a] for a in range(k)) for i in range(n)]
    rss = sum(r * r for r in resid)
    ybar = sum(y) / n
    tss = sum((v - ybar) ** 2 for v in y)
    sigma2 = rss / (n - k)
    se = [math.sqrt(max(0.0, sigma2 * inv[a][a])) for a in range(k)]
    return beta, se, (1 - rss / tss if tss > 0 else float("nan")), n


# ---------------------------------------------------------------------------
# loading
# ---------------------------------------------------------------------------
def load_requests(results):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import report as R
    recs = []
    for p in sorted(glob.glob(os.path.join(results, "requests", "*.log"))):
        try:
            recs.append(R.parse_request_log(p))
        except Exception as e:  # a corrupt log must not silently vanish
            print(f"WARN: unparsable {p}: {e}", file=sys.stderr)
    return recs


def load_jsonl(path):
    out = []
    if not os.path.exists(path):
        return out
    with open(path) as source:
        for line in source:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return out


# ---------------------------------------------------------------------------
# analyses
# ---------------------------------------------------------------------------
def analyse_latency(recs, out):
    """A: end-to-end per-request wall time, decomposed by stage."""
    m = [r for r in recs if r.get("phase") == "p3" and r.get("ok") and not r.get("held")]
    out["n_matrix_requests"] = len(m)
    out["n_matrix_failed"] = len([r for r in recs if r.get("phase") == "p3" and not r.get("ok")])

    def cell(rs, key):
        return [r[key] for r in rs if r.get(key) is not None]

    # headline end-to-end: the primary arm (rootless-proxy, uffd, medium page)
    prim = [r for r in m if r.get("mode") == "rootless-proxy" and r.get("arm") == "uffd-4k"
            and r.get("url") == "medium"]
    stages = {}
    # exec_up splits into exec_handshake (fcvm's restore->GO) + exec_spawn
    # (GO -> the guest command's first output). Both are needed for the stage
    # decomposition to add up without an unattributable remainder.
    # r_connect_ms is the CDP handshake (/json/list + TCP + RFC6455 upgrade). Under
    # this design it is paid on EVERY request, so it is a reported stage, not a
    # detail folded into "render".
    for key in ("restore_ms", "exec_handshake_ms", "exec_spawn_ms", "exec_up_ms",
                "net_ready_host_ms", "r_connect_ms", "r_navigate_ms", "r_idle_ms",
                "r_screenshot_ms", "r_dom_ms", "artifact_ms", "destroy_ms", "total_ms",
                "exec_attempts", "exec_retry_wait_ms"):
        v, lo, hi, n = boot_ci(cell(prim, key))
        stages[key] = {"median": v, "ci_lo": lo, "ci_hi": hi, "n": n}
    out["primary_cell"] = {"mode": "rootless-proxy", "arm": "uffd-4k", "page": "medium",
                           "stages": stages}

    # per-arm end-to-end totals
    arms = {}
    for r in m:
        arms.setdefault((r.get("arm"), r.get("url")), []).append(r)
    out["by_arm"] = {}
    for (arm, url), rs in sorted(arms.items(), key=lambda kv: (str(kv[0][0]), str(kv[0][1]))):
        v, lo, hi, n = boot_ci(cell(rs, "total_ms"))
        a, alo, ahi, _ = boot_ci(cell(rs, "artifact_ms"))
        out["by_arm"][f"{arm}/{url}"] = {
            "total_median_ms": v, "total_ci": [lo, hi],
            "artifact_median_ms": a, "artifact_ci": [alo, ahi], "n": n}
    return m


def analyse_drift_and_egress(m, out):
    """C: egress modes from the interleaved schedule + measured drift (defect 2)."""
    # what a block design would have aliased: drift of the pure-orchestration probe
    noop = sorted([r for r in m if r.get("arm") == "control-noop" and r.get("total_ms")],
                  key=lambda r: r["sched_ts"])
    if len(noop) >= 4:
        t0 = noop[0]["sched_ts"]
        X = [[1.0, (r["sched_ts"] - t0) / 3600.0] for r in noop]
        y = [r["total_ms"] for r in noop]
        fit = ols(X, y)
        if fit:
            beta, se, r2, n = fit
            out["orchestration_drift"] = {
                "intercept_ms": beta[0], "intercept_se": se[0],
                "slope_ms_per_hour": beta[1], "slope_se": se[1],
                "r2": r2, "n": n,
                "span_hours": (noop[-1]["sched_ts"] - t0) / 3600.0,
                "first_ms": noop[0]["total_ms"], "last_ms": noop[-1]["total_ms"],
                "median_ms": statistics.median(y),
            }
    # mode comparison on the http fixture pages, uffd arm only
    eg = [r for r in m if r.get("arm") == "uffd-4k" and r.get("url") in
          ("minimal", "medium", "heavy", "medium-https")]
    modes = sorted({r["mode"] for r in eg})
    pages = sorted({r["url"] for r in eg})
    out["egress_modes"] = {}
    for mode in modes:
        rs = [r for r in eg if r["mode"] == mode]
        v, lo, hi, n = boot_ci([r["artifact_ms"] for r in rs if r.get("artifact_ms")])
        nv, nlo, nhi, _ = boot_ci([r["net_ready_host_ms"] for r in rs
                                   if r.get("net_ready_host_ms") is not None])
        out["egress_modes"][mode] = {
            "artifact_median_ms": v, "artifact_ci": [lo, hi], "n": n,
            "first_egress_median_ms": nv, "first_egress_ci": [nlo, nhi],
        }
    # joint model: mode + page dummies + linear time. Randomisation already
    # removes the mode/time confound; the time term only shrinks residuals.
    if len(eg) > len(modes) + len(pages) + 2:
        t0 = min(r["sched_ts"] for r in eg)
        cols = modes[1:] + pages[1:]
        X, y = [], []
        for r in eg:
            if r.get("artifact_ms") is None:
                continue
            row = [1.0]
            row += [1.0 if r["mode"] == mm else 0.0 for mm in modes[1:]]
            row += [1.0 if r["url"] == pp else 0.0 for pp in pages[1:]]
            row.append((r["sched_ts"] - t0) / 3600.0)
            X.append(row)
            y.append(r["artifact_ms"])
        fit = ols(X, y)
        if fit:
            beta, se, r2, n = fit
            names = ["intercept(" + modes[0] + "," + pages[0] + ")"] + \
                    [f"mode:{mm}" for mm in modes[1:]] + \
                    [f"page:{pp}" for pp in pages[1:]] + ["time_hours"]
            out["egress_model"] = {
                "terms": [{"name": nm, "coef_ms": b, "se_ms": s}
                          for nm, b, s in zip(names, beta, se)],
                "r2": r2, "n": n,
                "note": "coefficients are contrasts vs the reference cell; "
                        "time_hours is the residual drift after randomisation",
            }


def analyse_format_and_isolation(m, out):
    """A: JPEG-vs-PNG and site-isolation on/off, measured on THIS run."""
    def grp(arm):
        return [r for r in m if r.get("arm") == arm]
    res = {}
    png, jpeg = grp("shot-png"), grp("shot-jpeg")
    for label, key in (("artifact_ms", "artifact_ms"), ("screenshot_ms", "r_screenshot_ms"),
                       ("total_ms", "total_ms"), ("bytes", "r_png_bytes")):
        d, lo, hi = boot_diff_ci([r[key] for r in jpeg if r.get(key) is not None],
                                 [r[key] for r in png if r.get(key) is not None])
        base, blo, bhi, bn = boot_ci([r[key] for r in png if r.get(key) is not None])
        res[label] = {"png_median": base, "png_ci": [blo, bhi], "png_n": bn,
                      "jpeg_minus_png": d, "diff_ci": [lo, hi],
                      "pct": (100.0 * d / base) if (d is not None and base) else None}
    out["screenshot_format"] = res

    on, off = grp("siteiso-on"), grp("siteiso-off")
    res2 = {}
    # PSS first: RSS counts every shared page once per process, so a change that
    # merely REDUCES the process count (which is exactly what site-isolation-off
    # does) shows a large RSS "saving" that is partly an accounting artifact.
    for label, key in (("artifact_ms", "artifact_ms"), ("total_ms", "total_ms"),
                       ("chrome_pss_kb", "chrome_pss_kb"),
                       ("chrome_rss_kb", "chrome_rss_kb"), ("chrome_procs", "chrome_procs")):
        d, lo, hi = boot_diff_ci([r[key] for r in off if r.get(key) is not None],
                                 [r[key] for r in on if r.get(key) is not None])
        base, blo, bhi, bn = boot_ci([r[key] for r in on if r.get(key) is not None])
        res2[label] = {"on_median": base, "on_ci": [blo, bhi], "on_n": bn,
                       "off_minus_on": d, "diff_ci": [lo, hi],
                       "pct": (100.0 * d / base) if (d is not None and base) else None}
    out["site_isolation"] = res2


def analyse_density(results, out):
    """B: memory density on the matched basis, slope AND intercept (defects 1, 5)."""
    dens = [s for s in load_jsonl(os.path.join(results, "samples", "density.jsonl"))]
    out["density"] = {}
    dropped = {"contaminated": 0, "cells_lost": 0}
    cells = sorted({s.get("cell") for s in dens if s.get("cell")})
    for cell in cells:
        rows = []          # (N, cgroup_MiB, pss_MiB, memavail_delta_MiB, fc_only_MiB)
        reps = sorted({s.get("rep") for s in dens if s.get("cell") == cell})
        for rep in reps:
            for N in sorted({s.get("n") for s in dens if s.get("cell") == cell and s.get("rep") == rep}):
                sel = [s for s in dens if s.get("cell") == cell and s.get("rep") == rep and s.get("n") == N]
                pre = [s for s in sel if s.get("phase") == "pre"]
                steady = [s for s in sel if s.get("phase") == "steady"]
                if not pre or not steady:
                    continue
                # A steady sample is usable only if EXACTLY N clones were up.
                # `>= N` would silently admit a sample taken while a previous
                # cell's clone had not finished tearing down (the harness logs
                # "clones still up after 90s" when that happens), and that extra
                # clone lands in the same cgroup set — inflating this cell's
                # memory by a whole clone. Require equality and count the drops.
                usable = [s for s in steady if s.get("clones", 0) == N]
                dropped["contaminated"] += len(steady) - len(usable)
                steady = usable
                if not steady:
                    dropped["cells_lost"] += 1
                    continue
                cg = statistics.median([s.get("clone_cgroup_kb", 0) for s in steady]) / 1024.0
                pss = statistics.median([s.get("clone_pss_kb", 0) for s in steady]) / 1024.0
                fco = None
                if all("fc_only_pss_kb" in s for s in steady):
                    fco = statistics.median(
                        [s["fc_only_pss_kb"] for s in steady]) / 1024.0
                avail_pre = statistics.median([s["mem_available_kb"] for s in pre])
                avail_st = statistics.median([s["mem_available_kb"] for s in steady])
                rows.append((N, cg, pss, (avail_pre - avail_st) / 1024.0, fco))
        if len(rows) < 3:
            out["density"][cell] = {"error": "insufficient usable samples", "rows": rows}
            continue
        entry = {"n_range": [min(r[0] for r in rows), max(r[0] for r in rows)],
                 "n_points": len(rows), "rows": rows, "fits": {}}
        bases = [(1, "cgroup"), (2, "pss"), (3, "memavailable_delta")]
        if all(row[4] is not None for row in rows):
            bases.append((4, "fc_only_pss_REFUTED_BASIS"))
        for idx, basis in bases:
            X = [[1.0, float(r[0])] for r in rows]
            y = [r[idx] for r in rows]
            fit = ols(X, y)
            if not fit:
                continue
            beta, se, r2, n = fit
            entry["fits"][basis] = {
                "intercept_mib": beta[0], "intercept_se": se[0],
                "slope_mib_per_clone": beta[1], "slope_se": se[1],
                "r2": r2, "n": n,
            }
            # req/GiB at CONCRETE N, not asymptotically (defect 5)
            per_n = {}
            for N in (1, 2, 4, 8, 16, 32):
                tot = beta[0] + beta[1] * N
                if tot > 0:
                    per_n[N] = N * 1024.0 / tot
            entry["fits"][basis]["reqs_per_gib_at_n"] = per_n
        out["density"][cell] = entry
    out["density_sample_drops"] = dropped


def analyse_pool(results, out):
    """E/B comparator: host-native warm container pool, SAME two bases."""
    pool = load_jsonl(os.path.join(results, "samples", "podman-pool.jsonl"))
    rows = []
    for rep in sorted({s.get("rep") for s in pool}):
        pre = [s for s in pool if s.get("rep") == rep and s.get("phase") == "pre"]
        if not pre:
            continue
        avail_pre = statistics.median([s["mem_available_kb"] for s in pre])
        for s in pool:
            if s.get("rep") != rep or s.get("phase") != "steady":
                continue
            n = s.get("pool_containers", 0)
            if not n:
                continue
            rows.append((n, s.get("pool_cgroup_kb", 0) / 1024.0,
                         s.get("pool_pss_kb", 0) / 1024.0,
                         (avail_pre - s["mem_available_kb"]) / 1024.0))
    out["host_pool"] = {"rows": rows, "fits": {}}
    if len(rows) >= 3:
        for idx, basis in ((1, "cgroup"), (2, "pss"), (3, "memavailable_delta")):
            fit = ols([[1.0, float(r[0])] for r in rows], [r[idx] for r in rows])
            if not fit:
                continue
            beta, se, r2, n = fit
            per_n = {}
            for N in (1, 2, 4, 8, 16, 32):
                tot = beta[0] + beta[1] * N
                if tot > 0:
                    per_n[N] = N * 1024.0 / tot
            out["host_pool"]["fits"][basis] = {
                "intercept_mib": beta[0], "intercept_se": se[0],
                "slope_mib_per_container": beta[1], "slope_se": se[1],
                "r2": r2, "n": n, "reqs_per_gib_at_n": per_n}
        out["host_pool"]["n_range"] = [min(r[0] for r in rows), max(r[0] for r in rows)]


def analyse_throughput(results, recs, out):
    """D: bursts (burst = experimental unit) and the sustained phase (defect 3)."""
    bursts = [b for b in load_jsonl(os.path.join(results, "samples", "bursts.jsonl"))
              if b.get("phase") == "burst"]
    by = {}
    for b in bursts:
        by.setdefault((b["cell"], b["n"]), []).append(b)
    out["bursts"] = {}
    for (cell, n), bs in sorted(by.items()):
        rps = [b["n"] / (b["t1"] - b["t0"]) for b in bs if b["t1"] > b["t0"]]
        wall = [(b["t1"] - b["t0"]) * 1000 for b in bs]
        v, lo, hi, k = boot_ci(rps)
        w, wlo, whi, _ = boot_ci(wall)
        out["bursts"][f"{cell}/N={n}"] = {
            "n_bursts": k, "clones_per_burst": n,
            "rps_median": v, "rps_ci": [lo, hi],
            "burst_wall_median_ms": w, "burst_wall_ci": [wlo, whi],
            "unit": "burst (clones within a burst are pseudoreplicates)",
        }
    meta = [b for b in load_jsonl(os.path.join(results, "samples", "bursts.jsonl"))
            if b.get("phase") == "sustained-meta"]
    out["sustained"] = {}
    for mrec in meta:
        cell, rate = mrec["cell"], mrec["rate"]
        done = [r for r in recs if r.get("phase") == f"sust-r{rate}" and
                r.get("arm") == cell and r.get("ok")]
        t_lo, t_hi = mrec.get("t0", 0), mrec.get("t1", 0)
        dur = t_hi - t_lo
        v, lo, hi, k = boot_ci([r["total_ms"] for r in done if r.get("total_ms")])
        # THE RATE GETS AN INTERVAL TOO. `achieved_rps` was a bare quotient over
        # ONE 63.6 s window, printed to two decimals into a table whose only CI
        # column is latency — while the burst path immediately above it bootstraps
        # its rate because bursts are replicated 5x. Splitting the single window
        # into equal sub-windows and bootstrapping the per-sub-window rate is not
        # a substitute for replicating the window (the sub-windows are not
        # independent draws of a fresh machine state), so it is labelled as what
        # it is. When the per-request timestamps needed for the split are absent,
        # emit NO interval rather than a fabricated one.
        rate_ci = None
        rate_basis = "single unreplicated window; no interval derivable"
        comp_ts = [r["t0_ts"] + r["total_ms"] / 1000.0
                   for r in done if r.get("t0_ts") and r.get("total_ms")]
        if dur > 0 and len(comp_ts) == len(done) and len(done) >= SUSTAINED_SUBWINDOWS:
            w = dur / SUSTAINED_SUBWINDOWS
            counts = [0] * SUSTAINED_SUBWINDOWS
            for t in comp_ts:
                i = int((t - t_lo) / w)
                counts[min(max(i, 0), SUSTAINED_SUBWINDOWS - 1)] += 1
            rv, rlo, rhi, _ = boot_ci([c / w for c in counts])
            rate_ci = [rlo, rhi]
            rate_basis = (f"{SUSTAINED_SUBWINDOWS} equal sub-windows of one "
                          f"{dur:.1f}s window (sub-windows are NOT independent "
                          f"replicates; median {rv:.2f} rps)")
        # "462/462 completed" is not a 0% failure rate, same rule as everywhere
        # else in this bench. analyze.py had no binomial function at all.
        launched = mrec.get("launched", 0)
        f_lo, f_hi = clopper_pearson(max(0, launched - len(done)), launched)
        out["sustained"][f"{cell}/target={rate}rps"] = {
            "launched": launched, "skipped": mrec["skipped"],
            "completed_ok": len(done),
            "achieved_rps": (len(done) / dur) if dur > 0 else None,
            "rate_ci": rate_ci, "rate_ci_basis": rate_basis,
            "incomplete_rate_ci": [f_lo, f_hi],
            "duration_s": dur,
            "latency_median_ms": v, "latency_ci": [lo, hi], "n": k,
        }


def analyse_load(results, recs, out):
    """Contention audit. Every phase gets the load it was actually measured under,
    and the matrix requests get a load-vs-latency correlation, so "the box was
    quiet" is a checkable statement rather than an assurance."""
    samples = load_jsonl(os.path.join(results, "samples", "loadavg.jsonl"))
    out["load"] = {"n_samples": len(samples)}
    if not samples:
        out["load"]["note"] = "no load samples recorded for this run"
        return
    samples.sort(key=lambda s: s["ts"])

    def stats(sel):
        v = sorted(s["load1"] for s in sel)
        if not v:
            return None
        return {"n": len(v), "median": v[len(v) // 2], "p90": v[int(0.9 * (len(v) - 1))],
                "max": v[-1], "min": v[0]}
    out["load"]["whole_run"] = stats(samples)
    # per phase, from the request timestamps that phase actually produced
    phases = {}
    for r in recs:
        ph, t = r.get("phase"), r.get("t0_ts")
        if ph and t:
            lo, hi = phases.get(ph, (t, t))
            phases[ph] = (min(lo, t), max(hi, t))
    out["load"]["by_phase"] = {}
    for ph, (lo, hi) in sorted(phases.items()):
        sel = [s for s in samples if lo <= s["ts"] <= hi]
        st = stats(sel)
        if st:
            st["window_utc"] = [time.strftime("%H:%M:%S", time.gmtime(lo)),
                                time.strftime("%H:%M:%S", time.gmtime(hi))]
            st["duration_s"] = round(hi - lo, 1)
            out["load"]["by_phase"][ph] = st
    # does load explain latency inside the matrix? (it should not, if quiet)
    m = [r for r in recs if r.get("phase") == "p3" and r.get("ok")
         and r.get("total_ms") and r.get("t0_ts")]
    if len(m) > 10 and len(samples) > 2:
        ts = [s["ts"] for s in samples]
        lv = [s["load1"] for s in samples]

        def load_at(t):
            i = bisect.bisect_left(ts, t)
            return lv[min(i, len(lv) - 1)]
        X = [[1.0, load_at(r["t0_ts"])] for r in m]
        y = [r["total_ms"] for r in m]
        fit = ols(X, y)
        if fit:
            beta, se, r2, n = fit
            out["load"]["latency_vs_load"] = {
                "ms_per_load_unit": beta[1], "se": se[1], "r2": r2, "n": n,
                "note": "slope indistinguishable from 0 (|beta| < 2*se) means load "
                        "did not measurably drive request latency in this run",
                "significant": abs(beta[1]) > 2 * se[1],
            }


def analyse_host_baseline(recs, out):
    """E: host-native warm floor, measured the same way."""
    warm = [r for r in recs if str(r.get("phase", "")).startswith("base-podman-warm") and r.get("ok")]
    cold = [r for r in recs if str(r.get("phase", "")).startswith("base-podman-cold") and r.get("ok")]
    fcold = [r for r in recs if str(r.get("phase", "")).startswith("base-fcvm-cold") and r.get("ok")]
    out["host_baseline"] = {}
    for label, rs in (("podman-warm", warm), ("podman-cold", cold), ("fcvm-cold-boot", fcold)):
        by_fmt = {}
        for r in rs:
            by_fmt.setdefault((r.get("mode"), r.get("url")), []).append(r)
        for (mode, url), sub in sorted(by_fmt.items(), key=lambda kv: (str(kv[0][0]), str(kv[0][1]))):
            v, lo, hi, n = boot_ci([r["total_ms"] for r in sub if r.get("total_ms")])
            a, alo, ahi, _ = boot_ci([r["artifact_ms"] for r in sub if r.get("artifact_ms")])
            out["host_baseline"][f"{label}/{mode}/{url}"] = {
                "total_median_ms": v, "total_ci": [lo, hi],
                "artifact_median_ms": a, "artifact_ci": [alo, ahi], "n": n}


def ci_cell(v, ci, unit=""):
    if v is None:
        return "n/a"
    lo, hi = (ci or [None, None])
    if lo is None or hi is None:
        return f"{v:.3g}{unit}"
    half = (hi - lo) / 2 or 1e-9
    return f"{fmt(v, half)} ({fmt(lo, half)}-{fmt(hi, half)}){unit}"


def write_md(out, path):
    """Deterministic tables straight from the computed stats — the prose in
    summary.md quotes these, so no figure is hand-transcribed."""
    L = []
    a = L.append
    a("<!-- generated by analyze.py; do not hand-edit numbers -->\n")

    a("### A. Per-request end-to-end wall time (primary cell)\n")
    p = out.get("primary_cell", {})
    a(f"Cell: mode=`{p.get('mode')}` arm=`{p.get('arm')}` page=`{p.get('page')}`. "
      "Medians with percentile-bootstrap 95% CIs; the burst-free matrix is one "
      "request at a time.\n")
    a("| stage | median (95% CI) ms | n |")
    a("|---|---|---|")
    labels = [("restore_ms", "restore (t0 -> cloned)"),
              ("exec_up_ms", "exec up (cloned -> driver first output)"),
              ("exec_retry_wait_ms", "&nbsp;&nbsp;of which fcvm exec retry sleep"),
              ("exec_attempts", "&nbsp;&nbsp;exec connect attempts (count)"),
              ("net_ready_host_ms", "first egress (driver -> host TCP up)"),
              ("r_navigate_ms", "page load (navigate -> loadEventFired)"),
              ("r_screenshot_ms", "screenshot"),
              ("r_dom_ms", "DOM dump"),
              ("artifact_ms", "**artifact (t0 -> reply could be sent)**"),
              ("destroy_ms", "teardown"),
              ("total_ms", "**total (t0 -> clone gone)**")]
    for key, lab in labels:
        s = p.get("stages", {}).get(key, {})
        a(f"| {lab} | {ci_cell(s.get('median'), [s.get('ci_lo'), s.get('ci_hi')])} | {s.get('n', 0)} |")
    a("")

    a("### C. Egress modes (one interleaved schedule)\n")
    a("| mode | artifact ms, median (95% CI) | first-egress ms, median (95% CI) | n |")
    a("|---|---|---|---|")
    for mode, v in sorted(out.get("egress_modes", {}).items()):
        a(f"| {mode} | {ci_cell(v.get('artifact_median_ms'), v.get('artifact_ci'))} "
          f"| {ci_cell(v.get('first_egress_median_ms'), v.get('first_egress_ci'))} | {v.get('n')} |")
    d = out.get("orchestration_drift")
    if d:
        a("")
        a(f"Control probe (restore -> exec -> teardown, no egress, no render): drift "
          f"**{d['slope_ms_per_hour']:.0f} +/- {d['slope_se']:.0f} ms/hour** over "
          f"{d['span_hours']:.2f} h (n={d['n']}, R^2={d['r2']:.2f}); median "
          f"{d['median_ms']:.0f} ms. This is the magnitude a block design would have "
          f"aliased onto the mode factor.")
    a("")

    a("### A. Screenshot format and site isolation (measured on this run)\n")
    a("| metric | PNG baseline (95% CI) | JPEG q80 - PNG (95% CI) | change |")
    a("|---|---|---|---|")
    for k, v in out.get("screenshot_format", {}).items():
        pctv = f"{v['pct']:+.0f}%" if v.get("pct") is not None else "n/a"
        a(f"| {k} | {ci_cell(v.get('png_median'), v.get('png_ci'))} "
          f"| {ci_cell(v.get('jpeg_minus_png'), v.get('diff_ci'))} | {pctv} |")
    a("")
    a("| metric | site-isolation ON (95% CI) | OFF - ON (95% CI) | change |")
    a("|---|---|---|---|")
    for k, v in out.get("site_isolation", {}).items():
        pctv = f"{v['pct']:+.0f}%" if v.get("pct") is not None else "n/a"
        a(f"| {k} | {ci_cell(v.get('on_median'), v.get('on_ci'))} "
          f"| {ci_cell(v.get('off_minus_on'), v.get('diff_ci'))} | {pctv} |")
    a("")

    a("### B. Memory density — slope AND intercept, matched basis\n")
    a("Basis `cgroup` = memory.current of the per-clone cgroup containing every "
      "process of that clone. Basis `pss` = PSS summed over that same process set. "
      "Basis `memavailable_delta` = whole-machine MemAvailable drop from a "
      "quiescent baseline re-anchored per measurement. `fc_only_pss` is the "
      "REFUTED basis from the first run, shown only to size the error.\n")
    a("| cell | basis | intercept MiB | slope MiB/clone | R^2 | N range | pts |")
    a("|---|---|---|---|---|---|---|")
    for cell, v in sorted(out.get("density", {}).items()):
        if "fits" not in v:
            a(f"| {cell} | - | {v.get('error')} | | | | |")
            continue
        for basis, f_ in v["fits"].items():
            a(f"| {cell} | {basis} | {fmt(f_['intercept_mib'], f_['intercept_se'])} +/- "
              f"{f_['intercept_se']:.0f} | {fmt(f_['slope_mib_per_clone'], f_['slope_se'])} +/- "
              f"{f_['slope_se']:.0f} | {f_['r2']:.3f} | {v['n_range'][0]}-{v['n_range'][1]} | {f_['n']} |")
    hp = out.get("host_pool", {})
    for basis, f_ in hp.get("fits", {}).items():
        nr = hp.get("n_range", [0, 0])
        a(f"| host-native container pool | {basis} | {fmt(f_['intercept_mib'], f_['intercept_se'])} +/- "
          f"{f_['intercept_se']:.0f} | {fmt(f_['slope_mib_per_container'], f_['slope_se'])} +/- "
          f"{f_['slope_se']:.0f} | {f_['r2']:.3f} | {nr[0]}-{nr[1]} | {f_['n']} |")
    a("")
    a("Concurrent requests per GiB at CONCRETE concurrency (not asymptotic):\n")
    a("| cell (basis=cgroup) | N=1 | N=2 | N=4 | N=8 | N=16 |")
    a("|---|---|---|---|---|---|")
    for cell, v in sorted(out.get("density", {}).items()):
        f_ = v.get("fits", {}).get("cgroup")
        if not f_:
            continue
        r = f_["reqs_per_gib_at_n"]
        a(f"| {cell} | " + " | ".join(
            (f"{r[str(n)] if str(n) in r else r.get(n, float('nan')):.2f}"
             if (str(n) in r or n in r) else "n/a") for n in (1, 2, 4, 8, 16)) + " |")
    f_ = hp.get("fits", {}).get("cgroup")
    if f_:
        r = f_["reqs_per_gib_at_n"]
        a("| host-native container pool | " + " | ".join(
            (f"{r[str(n)] if str(n) in r else r.get(n, float('nan')):.2f}"
             if (str(n) in r or n in r) else "n/a") for n in (1, 2, 4, 8, 16)) + " |")
    a("")

    a("### D. Throughput — bursts (burst = experimental unit) and sustained\n")
    a("| cell / burst size | bursts | burst wall ms, median (95% CI) | req/s, median (95% CI) |")
    a("|---|---|---|---|")
    for k, v in sorted(out.get("bursts", {}).items()):
        a(f"| {k} | {v['n_bursts']} | {ci_cell(v.get('burst_wall_median_ms'), v.get('burst_wall_ci'))} "
          f"| {ci_cell(v.get('rps_median'), v.get('rps_ci'))} |")
    a("")
    # `achieved req/s` is one decimal, not two: a single unreplicated window
    # cannot support 0.01 rps of resolution, and quoting 7.26 claimed it did.
    a("| sustained cell | launched | completed (CP 95% incomplete) "
      "| achieved req/s (sub-window CI) | latency ms, median (95% CI) |")
    a("|---|---|---|---|---|")
    for k, v in sorted(out.get("sustained", {}).items()):
        ar = f"{v['achieved_rps']:.1f}" if v.get("achieved_rps") else "n/a"
        if v.get("rate_ci"):
            ar += f" ({v['rate_ci'][0]:.1f}-{v['rate_ci'][1]:.1f})"
        else:
            ar += " (single window, no interval)"
        fci = v.get("incomplete_rate_ci")
        comp = f"{v['completed_ok']}"
        if fci:
            comp += f" [{100 * fci[0]:.2f}%, {100 * fci[1]:.2f}%]"
        a(f"| {k} | {v['launched']} | {comp} | {ar} "
          f"| {ci_cell(v.get('latency_median_ms'), v.get('latency_ci'))} |")
    a("")
    a("*Sub-window CIs split ONE window; the sub-windows are not independent "
      "replicates of a fresh machine state, unlike the burst cells above (5 "
      "bursts each). Do not read them as run-to-run uncertainty.*\n")

    a("### E. Host-native baseline (same pages, same screenshot format)\n")
    a("| baseline | total ms, median (95% CI) | artifact ms, median (95% CI) | n |")
    a("|---|---|---|---|")
    for k, v in sorted(out.get("host_baseline", {}).items()):
        a(f"| {k} | {ci_cell(v.get('total_median_ms'), v.get('total_ci'))} "
          f"| {ci_cell(v.get('artifact_median_ms'), v.get('artifact_ci'))} | {v.get('n')} |")
    a("")
    open(path, "w").write("\n".join(L) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results_dir")
    ap.add_argument("-o", "--out", default=None)
    ap.add_argument("--md", default=None, help="also write generated tables here")
    args = ap.parse_args()
    res = args.results_dir
    recs = load_requests(res)
    out = {"results_dir": os.path.abspath(res), "n_request_logs": len(recs)}
    hi = os.path.join(res, "hostinfo.json")
    if os.path.exists(hi):
        out["hostinfo"] = json.load(open(hi))
    av = os.path.join(res, "availability.json")
    if os.path.exists(av):
        out["availability"] = json.load(open(av))
    out["baselines"] = load_jsonl(os.path.join(res, "samples", "baselines.jsonl"))

    m = analyse_latency(recs, out)
    analyse_drift_and_egress(m, out)
    analyse_format_and_isolation(m, out)
    analyse_density(res, out)
    analyse_pool(res, out)
    analyse_throughput(res, recs, out)
    analyse_host_baseline(recs, out)
    analyse_load(res, recs, out)

    dst = args.out or os.path.join(res, "corrected.json")
    json.dump(out, open(dst, "w"), indent=1, default=str)
    print(f"wrote {dst}")
    mdp = args.md or os.path.join(res, "tables.md")
    write_md(out, mdp)
    print(f"wrote {mdp}")


if __name__ == "__main__":
    sys.exit(main())
