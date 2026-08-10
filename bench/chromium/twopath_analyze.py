#!/usr/bin/env python3
"""Analysis for twopath.py. Medians + min/max only -- this is a PROFILE, not a benchmark,
so there are no CIs, no significance tests, and no claims that need them.

Everything here is windowed. The raw per-request fault totals are NOT the answer: the fault
pass holds the VM alive after the screenshot (so the pagemap snapshot can be taken), and
Chromium keeps touching memory during that hold. Faults are therefore attributed to stages
using CLOCK_MONOTONIC marks and the absolute-mono ftrace timestamps, and the headline
"working set" is the faults up to RENDER_OK.
"""

import argparse
import json
import os
import statistics
import struct
import sys
from pathlib import Path

PAGE = 4096
# arm64 Firecracker maps guest RAM at IPA 0x8000_0000, so snapshot-file offset = IPA - BASE.
# Verified, not assumed: on a clean run the shifted stage-2 IPA set covered 100.0% of the
# UFFD handler's offset set (56,845/56,851), the UFFD set carrying 0.6% extra pages that
# firecracker itself faulted in from userspace (device emulation) without a vCPU abort.
# This identity is what makes path A's working set comparable to path B's at all: path A
# has no handler trace, so its ONLY per-page record is the stage-2 IPA stream.
GUEST_RAM_BASE = 0x80000000
STAGE_ORDER = ["harness", "clone_setup", "restore", "exec_handshake", "guest_exec_spawn",
               "net_check", "render", "probes", "exec_teardown", "vm_teardown"]
STAGE_LABEL = {
    "harness": "harness (spawn->fcvm alive)",
    "clone_setup": "clone setup (netns+pasta+reflink)",
    "restore": "snapshot restore (load+resume)",
    "exec_handshake": "exec handshake (resume->GO)",
    "guest_exec_spawn": "guest python up (GO->first output)",
    "net_check": "net check",
    "render": "Chromium render",
    "probes": "probes (nav timing + PSS)",
    "exec_teardown": "exec teardown",
    "vm_teardown": "VM teardown",
}


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def mmm(xs):
    """(median, min, max, n)"""
    xs = [x for x in xs if x is not None]
    if not xs:
        return (None, None, None, 0)
    return (statistics.median(xs), min(xs), max(xs), len(xs))


def f(v, nd=1):
    return "-" if v is None else f"{v:,.{nd}f}"


def load(out: Path):
    recs = [json.loads(l) for l in open(out / "requests.jsonl") if l.strip()]
    return recs


def sel(recs, pas, path, ok_only=True):
    r = [x for x in recs if x["pass"] == pas and x["path"] == path and not x["warmup"]]
    if ok_only:
        r = [x for x in r if x.get("ok")]
    return r


# ---------------------------------------------------------------------------
# trace readers
# ---------------------------------------------------------------------------
def read_ipaseq(p):
    """-> list[(ts_us_mono, page_addr)]"""
    try:
        buf = Path(p).read_bytes()
    except OSError:
        return []
    n = len(buf) // 16
    if not n:
        return []
    v = struct.unpack_from(f"<{n * 2}Q", buf)
    return list(zip(v[0::2], v[1::2]))


def read_faults(p):
    """UFFD handler trace -> list[(offset, before_ns, after_ns)]"""
    try:
        buf = Path(p).read_bytes()
    except OSError:
        return []
    n = len(buf) // 24
    if not n:
        return []
    v = struct.unpack_from(f"<{n * 3}Q", buf)
    return list(zip(v[0::3], v[1::3], v[2::3]))


def window(seq, t_lo, t_hi):
    return [x for x in seq if t_lo <= x[0] / 1e6 <= t_hi]


# ---------------------------------------------------------------------------
# locality / shape
# ---------------------------------------------------------------------------
def run_lengths(pages):
    """Contiguous runs over the SORTED DISTINCT page set.

    This is the fault-around question: if a run of k adjacent pages is needed, one window of
    k pages serves all of them. Arrival-order adjacency is reported separately -- it answers
    a different question (readahead), and conflating the two is how "it looks sequential"
    turns into a wrong fix.
    """
    s = sorted(set(pages))
    if not s:
        return {}
    runs = []
    start = prev = s[0]
    for p in s[1:]:
        if p == prev + PAGE:
            prev = p
            continue
        runs.append((prev - start) // PAGE + 1)
        start = prev = p
    runs.append((prev - start) // PAGE + 1)
    hist = {}
    for r in runs:
        b = ("1" if r == 1 else "2-3" if r < 4 else "4-7" if r < 8 else "8-15" if r < 16
             else "16-31" if r < 32 else "32-63" if r < 64 else "64-127" if r < 128 else "128+")
        h = hist.setdefault(b, {"runs": 0, "pages": 0})
        h["runs"] += 1
        h["pages"] += r
    total = len(s)
    for b in hist:
        hist[b]["pct_pages"] = 100.0 * hist[b]["pages"] / total
    return {"n_runs": len(runs), "n_pages": total,
            "mean_run": total / len(runs), "median_run": statistics.median(runs),
            "max_run": max(runs), "hist": hist,
            "pct_pages_in_runs_ge_16": 100.0 * sum(r for r in runs if r >= 16) / total,
            "pct_pages_in_singletons": 100.0 * sum(r for r in runs if r == 1) / total}


def arrival_adjacency(pages_in_order):
    """Fraction of consecutive faults that are +1 page from the previous fault."""
    if len(pages_in_order) < 2:
        return None
    fwd = sum(1 for a, b in zip(pages_in_order, pages_in_order[1:]) if b == a + PAGE)
    near = sum(1 for a, b in zip(pages_in_order, pages_in_order[1:])
               if 0 < abs(b - a) <= 16 * PAGE)
    return {"pct_next_is_plus1": 100.0 * fwd / (len(pages_in_order) - 1),
            "pct_next_within_64k": 100.0 * near / (len(pages_in_order) - 1)}


def jaccard(a, b):
    a, b = set(a), set(b)
    u = len(a | b)
    return len(a & b) / u if u else None


def region_hist(pages, nbuckets=32, span=None):
    if not pages:
        return []
    lo = 0
    hi = span or (max(pages) + PAGE)
    w = max(PAGE, (hi - lo) // nbuckets)
    h = [0] * (nbuckets + 1)
    for p in set(pages):
        i = min(nbuckets, (p - lo) // w)
        h[i] += 1
    return [{"bucket_gib": round(i * w / (1 << 30), 3), "pages": c}
            for i, c in enumerate(h) if c]


# ---------------------------------------------------------------------------
def cpu_split(rec):
    """Per-process and per-thread CPU (ms) from the sampler's task table."""
    by_proc = {}
    fc_threads = {}
    for t in rec.get("tasks", []):
        cpu = (t["utime_s"] + t["stime_s"]) * 1000.0
        comm = t["comm"]
        key = comm
        e = by_proc.setdefault(key, {"cpu_ms": 0.0, "utime_ms": 0.0, "stime_ms": 0.0,
                                     "min_flt": 0, "maj_flt": 0, "tasks": 0})
        e["cpu_ms"] += cpu
        e["utime_ms"] += t["utime_s"] * 1000.0
        e["stime_ms"] += t["stime_s"] * 1000.0
        e["min_flt"] += t["min_flt"]
        e["maj_flt"] += t["maj_flt"]
        e["tasks"] += 1
        if t["pid"] == rec.get("fc_pid"):
            fc_threads[comm] = fc_threads.get(comm, 0.0) + cpu
    return by_proc, fc_threads


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--md", default=None)
    args = ap.parse_args()
    out = Path(args.out)
    recs = load(out)
    meta = json.load(open(out / "meta.json"))

    R = {"meta": meta}
    lines = []

    def w(s=""):
        lines.append(s)

    A, B = "A-file", "B-uffd"
    tA, tB = sel(recs, "timing", A), sel(recs, "timing", B)
    fA, fB = sel(recs, "faults", A), sel(recs, "faults", B)
    nwarm = len([x for x in recs if x["warmup"]])
    nfail = len([x for x in recs if not x.get("ok")])

    w("# Two-path profile: file-backed restore vs UFFD memory server")
    w()
    w(f"Machine: `{meta['uname']}`, {meta['nproc']} cores. "
      f"Snapshot `{meta['tag']}`, guest {meta['guest_mib']} MiB, page `{meta['url'].rsplit('/', 1)[-1]}`, "
      f"screenshot {meta['fmt']} q{meta['qual']}, egress rootless (default).")
    w(f"Raw records: `requests.jsonl` ({len(recs)} rows), `meta.json`, "
      f"`requests/*.log`, `traces/`.")
    w(f"Warmups discarded explicitly: {nwarm}. Failed requests: {nfail}.")
    w(f"Load before {meta['quiet_before']}, after {meta['quiet_after']}.")
    w()

    # ---------------- stage table ----------------
    w("## 1. Stage-by-stage wall clock (timing pass, no ftrace)")
    w()
    w("Same stage boundaries for both paths, taken from the same `RUST_LOG=fcvm=debug` log "
      "lines. Median [min-max] ms.")
    w()
    w("| stage | A file-backed | B memory-server | B-A |")
    w("|---|---|---|---|")
    stage_rows = {}
    for s in STAGE_ORDER:
        a = mmm([r["stages"].get(s) for r in tA])
        b = mmm([r["stages"].get(s) for r in tB])
        d = (b[0] - a[0]) if (a[0] is not None and b[0] is not None) else None
        stage_rows[s] = {"A": a, "B": b, "delta": d}
        w(f"| {STAGE_LABEL[s]} | {f(a[0])} [{f(a[1])}-{f(a[2])}] | "
          f"{f(b[0])} [{f(b[1])}-{f(b[2])}] | {f(d)} |")
    ta = mmm([r["wall_total_ms"] for r in tA])
    tb = mmm([r["wall_total_ms"] for r in tB])
    gap = tb[0] - ta[0] if ta[0] and tb[0] else None
    w(f"| **TOTAL** | **{f(ta[0])}** [{f(ta[1])}-{f(ta[2])}] | "
      f"**{f(tb[0])}** [{f(tb[1])}-{f(tb[2])}] | **{f(gap)}** |")
    w()
    w(f"n = {ta[3]} (A) / {tb[3]} (B) non-warmup requests, interleaved A/B.")
    w()
    R["stages"] = {s: {"A_median": v["A"][0], "A_min": v["A"][1], "A_max": v["A"][2],
                       "B_median": v["B"][0], "B_min": v["B"][1], "B_max": v["B"][2],
                       "delta": v["delta"]} for s, v in stage_rows.items()}
    R["total"] = {"A": ta, "B": tb, "gap_ms": gap}

    # in-guest render breakdown
    w("### In-guest render breakdown (render.py's own timers)")
    w()
    w("| phase | A | B |")
    w("|---|---|---|")
    for k in ("r_connect_ms", "r_navigate_ms", "r_screenshot_ms", "r_dom_ms", "r_total_ms"):
        a, b = med([r.get(k) for r in tA]), med([r.get(k) for r in tB])
        w(f"| {k[2:-3]} | {f(a)} | {f(b)} |")
    w()

    # ---------------- CPU ----------------
    w("## 2. CPU")
    w()
    w("Two independent bases, reported separately and reconciled (AGENTS.md defect 1).")
    w()
    w("**(a) Per-request leaf cgroup `cpu.stat`** — covers every process of the clone, "
      "including ones that exit before a sampler can read them.")
    w()
    w("| basis | A | B |")
    w("|---|---|---|")
    for key, lab in (("usage_usec", "cgroup total CPU"), ("user_usec", "— user"),
                     ("system_usec", "— system")):
        a = med([(r["cgroup"]["after"].get(key, 0) - r["cgroup"]["before"].get(key, 0)) / 1000.0
                 for r in tA if r["cgroup"]["after"]])
        b = med([(r["cgroup"]["after"].get(key, 0) - r["cgroup"]["before"].get(key, 0)) / 1000.0
                 for r in tB if r["cgroup"]["after"]])
        w(f"| {lab} (ms) | {f(a)} | {f(b)} |")
    sb = med([r.get("serve_cpu_s", 0) * 1000.0 for r in tB])
    w(f"| memory-server CPU (ms, outside the cgroup) | n/a | {f(sb)} |")
    w()
    R["cpu_cgroup"] = {
        "A_usage_ms": med([(r["cgroup"]["after"].get("usage_usec", 0)
                            - r["cgroup"]["before"].get("usage_usec", 0)) / 1000.0
                           for r in tA if r["cgroup"]["after"]]),
        "B_usage_ms": med([(r["cgroup"]["after"].get("usage_usec", 0)
                            - r["cgroup"]["before"].get("usage_usec", 0)) / 1000.0
                           for r in tB if r["cgroup"]["after"]]),
        "B_serve_ms": sb,
    }

    w("**(b) Per-process / per-thread `/proc/<pid>/stat` (utime+stime), 4 ms sampling.** "
      "Median over requests, ms.")
    w()
    procs = set()
    for r in tA + tB:
        procs |= set(cpu_split(r)[0])
    w("| process (comm) | A | B |")
    w("|---|---|---|")
    proc_rows = {}
    for p in sorted(procs):
        a = med([cpu_split(r)[0].get(p, {}).get("cpu_ms") for r in tA])
        b = med([cpu_split(r)[0].get(p, {}).get("cpu_ms") for r in tB])
        if (a or 0) < 0.5 and (b or 0) < 0.5:
            continue
        proc_rows[p] = (a, b)
        w(f"| {p} | {f(a)} | {f(b)} |")
    w()
    R["cpu_procs"] = proc_rows

    w("**Firecracker threads** — guest CPU (vCPU threads) vs host device emulation.")
    w()
    thr = set()
    for r in tA + tB:
        thr |= set(cpu_split(r)[1])
    w("| firecracker thread | A | B |")
    w("|---|---|---|")
    thr_rows = {}
    for t in sorted(thr):
        a = med([cpu_split(r)[1].get(t) for r in tA])
        b = med([cpu_split(r)[1].get(t) for r in tB])
        thr_rows[t] = (a, b)
        w(f"| {t} | {f(a)} | {f(b)} |")
    w()
    R["cpu_fc_threads"] = thr_rows

    # ---------------- faults ----------------
    w("## 3. Page faults")
    w()
    w("`kvm:kvm_guest_fault` is the only basis that means the same thing on both paths: one "
      "arm64 stage-2 abort, i.e. the guest touched a page the stage-2 tables did not map. "
      "On path B each of those becomes a userspace round trip; on path A the kernel resolves "
      "it in place from the page cache.")
    w()
    w("`min_flt` is reported but is NOT a comparable basis, and the rows below show why: it "
      "follows whichever address space the page is installed into, so it MOVES with the work "
      "instead of measuring it. On path A the faults land on firecracker's own vCPU threads; "
      "on path B those threads take almost none and the memory server takes them instead. "
      "The same asymmetry shows up in the per-request cgroup `pgfault`, which is why a "
      "cgroup-only accounting basis would have made path B look cheap: the server sits "
      "OUTSIDE the clone's cgroup.")
    w()

    def fault_summary(rs):
        tot, uniq, win_ev, win_uq, spanms = [], [], [], [], []
        for r in rs:
            k = r.get("kvm_ftrace") or {}
            tot.append(k.get("events"))
            uniq.append(k.get("unique_pages"))
            seq = read_ipaseq(k.get("seq_file") or "")
            mm = r.get("marks_mono") or {}
            if seq and "t0" in mm and "render_ok" in mm:
                wseq = window(seq, mm["t0"], mm["render_ok"])
                win_ev.append(len(wseq))
                win_uq.append(len({p for _, p in wseq}))
            spanms.append(k.get("span_ms"))
        return tot, uniq, win_ev, win_uq, spanms

    aT, aU, aWE, aWU, _ = fault_summary(fA)
    bT, bU, bWE, bWU, _ = fault_summary(fB)
    bUF = [(r.get("uffd_trace") or {}).get("faults") for r in fB]

    w("| measure | A file-backed | B memory-server |")
    w("|---|---|---|")
    w(f"| stage-2 aborts, whole request | {f(med(aT), 0)} | {f(med(bT), 0)} |")
    w(f"| distinct guest pages, whole request | {f(med(aU), 0)} | {f(med(bU), 0)} |")
    w(f"| stage-2 aborts, **spawn->RENDER_OK** | {f(med(aWE), 0)} | {f(med(bWE), 0)} |")
    w(f"| distinct guest pages, **spawn->RENDER_OK** | {f(med(aWU), 0)} | {f(med(bWU), 0)} |")
    w(f"| UFFD faults served (handler trace) | n/a (no handler) | {f(med(bUF), 0)} |")
    wsA = med(aWU) or 0
    wsB = med(bWU) or 0
    w(f"| working set spawn->RENDER_OK (MiB) | {f(wsA * PAGE / (1 << 20))} | "
      f"{f(wsB * PAGE / (1 << 20))} |")
    w(f"| as % of {meta['guest_mib']} MiB guest RAM | "
      f"{f(100.0 * wsA * PAGE / (meta['guest_mib'] << 20))}% | "
      f"{f(100.0 * wsB * PAGE / (meta['guest_mib'] << 20))}% |")
    def vcpu_minflt(rs):
        return med([sum(t["min_flt"] for t in r["tasks"] if t["comm"].startswith("fc_vcpu"))
                    for r in rs])

    def all_minflt(rs):
        return med([sum(t["min_flt"] for t in r["tasks"]) for r in rs])

    mfa, mfb = vcpu_minflt(fA), vcpu_minflt(fB)
    w(f"| min_flt on firecracker **vCPU threads** | {f(mfa, 0)} | {f(mfb, 0)} |")
    w(f"| min_flt over the whole clone process tree | {f(all_minflt(fA), 0)} | "
      f"{f(all_minflt(fB), 0)} |")
    w(f"| min_flt on the **memory-server** process | n/a | "
      f"{f(med([r.get('serve_min_flt') for r in fB]), 0)} |")
    cgpfA = med([r["cgroup"]["after"].get("pgfault", 0) - r["cgroup"]["before"].get("pgfault", 0)
                 for r in fA if r["cgroup"]["after"]])
    cgpfB = med([r["cgroup"]["after"].get("pgfault", 0) - r["cgroup"]["before"].get("pgfault", 0)
                 for r in fB if r["cgroup"]["after"]])
    w(f"| per-request cgroup `memory.stat pgfault` | {f(cgpfA, 0)} | {f(cgpfB, 0)} |")
    ova = med([r.get("ftrace_overruns") for r in fA])
    ovb = med([r.get("ftrace_overruns") for r in fB])
    w(f"| ftrace buffer overruns (0 = nothing lost) | {f(ova, 0)} | {f(ovb, 0)} |")
    w()
    R["faults"] = {"A_events": med(aT), "B_events": med(bT),
                   "A_unique": med(aU), "B_unique": med(bU),
                   "A_events_to_render": med(aWE), "B_events_to_render": med(bWE),
                   "A_unique_to_render": med(aWU), "B_unique_to_render": med(bWU),
                   "B_uffd_served": med(bUF),
                   "A_vcpu_min_flt": mfa, "B_vcpu_min_flt": mfb,
                   "B_serve_min_flt": med([r.get("serve_min_flt") for r in fB]),
                   "A_cgroup_pgfault": cgpfA, "B_cgroup_pgfault": cgpfB,
                   "overruns_A": ova, "overruns_B": ovb}

    # pagemap cross-check
    def pm(rs):
        v = []
        for r in rs:
            hs = r.get("hold_snapshot") or {}
            pmv = hs.get("pagemap") or []
            if pmv:
                v.append(sum(x["present"] for x in pmv))
        return med(v)
    w(f"Pagemap cross-check (guest-RAM VMAs resident at hold): A {f(pm(fA), 0)} pages, "
      f"B {f(pm(fB), 0)} pages.")
    w()
    R["faults"]["A_pagemap_resident"] = pm(fA)
    R["faults"]["B_pagemap_resident"] = pm(fB)

    # ---------------- per-fault cost ----------------
    w("## 4. Per-fault cost on path B (handler trace)")
    w()
    svc50 = med([(r.get("uffd_trace") or {}).get("svc_ns_p50") for r in fB])
    svc90 = med([(r.get("uffd_trace") or {}).get("svc_ns_p90") for r in fB])
    gap50 = med([(r.get("uffd_trace") or {}).get("gap_ns_p50") for r in fB])
    gap90 = med([(r.get("uffd_trace") or {}).get("gap_ns_p90") for r in fB])
    svcsum = med([(r.get("uffd_trace") or {}).get("svc_ns_sum") for r in fB])
    gapsum = med([(r.get("uffd_trace") or {}).get("gap_ns_sum") for r in fB])
    w("| component | p50 | p90 | total per request |")
    w("|---|---|---|---|")
    w(f"| `UFFDIO_COPY` ioctl (handler-side service) | {f(svc50, 0)} ns | {f(svc90, 0)} ns | "
      f"{f((svcsum or 0) / 1e6)} ms |")
    w(f"| gap between faults (wake + epoll + read + guest re-entry) | {f(gap50, 0)} ns | "
      f"{f(gap90, 0)} ns | {f((gapsum or 0) / 1e6)} ms |")
    w()
    R["per_fault"] = {"svc_ns_p50": svc50, "svc_ns_p90": svc90,
                      "gap_ns_p50": gap50, "gap_ns_p90": gap90,
                      "svc_ms_total": (svcsum or 0) / 1e6, "gap_ms_total": (gapsum or 0) / 1e6}

    # ---------------- divergence ----------------
    w("## 5. Where the two diverge — reconciliation")
    w()
    nB = med(bUF) or 0
    per_fault_us = (gap * 1000.0 / nB) if (gap and nB) else None
    w(f"- measured wall gap (timing pass, medians): **{f(gap)} ms**")
    w(f"- UFFD faults served per request: **{f(nB, 0)}**")
    w(f"- implied per-fault delta: **{f(per_fault_us, 2)} us/fault**")
    w(f"- measured handler-side ioctl time: {f(svc50, 0)} ns p50 "
      f"({f((svcsum or 0) / 1e6)} ms total) — that is only "
      f"{f(100.0 * ((svcsum or 0) / 1e6) / gap if gap else None)}% of the gap")
    w()
    R["divergence"] = {"wall_gap_ms": gap, "uffd_faults": nB,
                       "implied_per_fault_us": per_fault_us,
                       "handler_ioctl_ms": (svcsum or 0) / 1e6}

    # ---------------- trace shape ----------------
    w("## 6. Fault trace shape (path B handler trace + both paths' stage-2 IPA streams)")
    w()

    def shape_from_uffd(rs):
        per = []
        for r in rs:
            fp = r.get("uffd_trace_path")
            if not fp:
                continue
            fl = read_faults(fp)
            if not fl:
                continue
            pages = [o for o, _, _ in fl]
            per.append({"name": r["name"], "pages": pages})
        return per

    def shape_from_ipaseq(rs, upto_render=True):
        """Faulted page set from the stage-2 abort stream, on the SAME offset basis as the
        UFFD trace. This is how path A gets a working set at all."""
        per = []
        for r in rs:
            k = r.get("kvm_ftrace") or {}
            seq = read_ipaseq(k.get("seq_file") or "")
            if not seq:
                continue
            mm = r.get("marks_mono") or {}
            if upto_render and "t0" in mm and "render_ok" in mm:
                seq = window(seq, mm["t0"], mm["render_ok"])
            per.append({"name": r["name"],
                        "pages": [p - GUEST_RAM_BASE for _, p in seq if p >= GUEST_RAM_BASE]})
        return [x for x in per if x["pages"]]

    shp = shape_from_uffd(fB)
    ipA = shape_from_ipaseq(fA)
    ipB = shape_from_ipaseq(fB)
    R["cross_clone"] = {}
    if shp:
        rl = run_lengths(shp[0]["pages"])
        aa = arrival_adjacency(shp[0]["pages"])
        w("### 6a. Locality (sorted-distinct contiguous runs, representative clone)")
        w()
        w(f"{rl['n_pages']:,} distinct pages in {rl['n_runs']:,} contiguous runs "
          f"(mean {rl['mean_run']:.1f} pages, median {rl['median_run']:.0f}, "
          f"max {rl['max_run']:,}).")
        w()
        w("| run length (pages) | runs | pages | % of working set |")
        w("|---|---|---|---|")
        for b in ("1", "2-3", "4-7", "8-15", "16-31", "32-63", "64-127", "128+"):
            if b in rl["hist"]:
                h = rl["hist"][b]
                w(f"| {b} | {h['runs']:,} | {h['pages']:,} | {h['pct_pages']:.1f}% |")
        w()
        w(f"Pages living in runs of >=16: **{rl['pct_pages_in_runs_ge_16']:.1f}%**. "
          f"Isolated singletons: {rl['pct_pages_in_singletons']:.1f}%.")
        if aa:
            w(f"In ARRIVAL order, {aa['pct_next_is_plus1']:.1f}% of faults are exactly the "
              f"next page after the previous one, and {aa['pct_next_within_64k']:.1f}% land "
              f"within +/-64 KiB.")
        w()
        R["locality"] = rl
        R["arrival"] = aa

        # cross-clone stability
        w("### 6b. Cross-clone stability (the prefetch question)")
        w()
        sets = [set(s["pages"]) for s in shp]
        n = len(sets)
        js = []
        for i in range(n):
            for j in range(i + 1, n):
                js.append(jaccard(sets[i], sets[j]))
        allp = set.intersection(*sets) if sets else set()
        anyp = set.union(*sets) if sets else set()
        counts = {}
        for p in anyp:
            c = sum(1 for s in sets if p in s)
            counts[c] = counts.get(c, 0) + 1
        w(f"{n} clones of the same golden snapshot, same page, same everything.")
        w()
        w(f"- pairwise Jaccard: median **{f(med(js), 4)}** "
          f"(min {f(min(js), 4) if js else '-'}, max {f(max(js), 4) if js else '-'}), "
          f"{len(js)} pairs")
        w(f"- pages faulted by ALL {n} clones: **{len(allp):,}** "
          f"({100.0 * len(allp) / len(anyp):.1f}% of the union)")
        w(f"- union over all clones: {len(anyp):,} pages "
          f"({len(anyp) * PAGE / (1 << 20):.1f} MiB)")
        w(f"- median per-clone set: {f(med([len(s) for s in sets]), 0)} pages")
        w()
        w("| faulted by N of the clones | pages | % of union |")
        w("|---|---|---|")
        for c in sorted(counts, reverse=True):
            w(f"| {c} | {counts[c]:,} | {100.0 * counts[c] / len(anyp):.1f}% |")
        w()
        R["cross_clone"] = {
            "n_clones": n, "jaccard_median": med(js), "jaccard_min": min(js),
            "jaccard_max": max(js) if js else None,
            "pages_in_all": len(allp), "union": len(anyp),
            "pct_in_all": 100.0 * len(allp) / len(anyp) if anyp else None,
            "median_set": med([len(s) for s in sets]),
            "count_hist": counts,
        }

        # working set shape
        w("### 6c. Working-set shape over the memory file")
        w()
        rh = region_hist(list(shp[0]["pages"]), 32, meta["guest_mib"] << 20)
        w("| offset (GiB) | distinct pages faulted |")
        w("|---|---|")
        for row in rh:
            w(f"| {row['bucket_gib']:.2f} | {row['pages']:,} |")
        w()
        R["region_hist"] = rh

        # cross-clone stability on BOTH paths, from the stage-2 stream (same basis), and
        # the A-vs-B comparison that says whether the two paths do the same work.
        w("### 6c-bis. Cross-clone stability on BOTH paths, and A vs B")
        w()
        w("Page sets here come from the stage-2 abort stream windowed to spawn->RENDER_OK "
          "and shifted to snapshot-file offsets, so path A and path B are on one basis.")
        w()
        w("| set | clones | median pages | pairwise Jaccard (median) | in ALL clones |")
        w("|---|---|---|---|---|")
        stab = {}
        for lab, per in (("A file-backed", ipA), ("B memory-server", ipB)):
            if len(per) < 2:
                continue
            sets = [set(x["pages"]) for x in per]
            js = [jaccard(sets[i], sets[j]) for i in range(len(sets))
                  for j in range(i + 1, len(sets))]
            allp = set.intersection(*sets)
            anyp = set.union(*sets)
            stab[lab] = {"n": len(sets), "median_pages": med([len(s) for s in sets]),
                         "jaccard_median": med(js), "jaccard_min": min(js) if js else None,
                         "in_all": len(allp), "union": len(anyp),
                         "pct_in_all": 100.0 * len(allp) / len(anyp) if anyp else None}
            w(f"| {lab} | {len(sets)} | {f(med([len(s) for s in sets]), 0)} | "
              f"{f(med(js), 4)} | {len(allp):,} ({100.0 * len(allp) / len(anyp):.1f}% of union) |")
        if ipA and ipB:
            ua = set.union(*[set(x["pages"]) for x in ipA])
            ub = set.union(*[set(x["pages"]) for x in ipB])
            jab = jaccard(ua, ub)
            w(f"| **A union vs B union** | - | A {len(ua):,} / B {len(ub):,} | "
              f"**{f(jab, 4)}** | {len(ua & ub):,} shared |")
            stab["A_vs_B_jaccard"] = jab
            stab["A_union"] = len(ua)
            stab["B_union"] = len(ub)
            stab["A_and_B"] = len(ua & ub)
        w()
        R["stability_both_paths"] = stab

        # temporal
        # cumulative arrival curve straight off the handler trace (its own clock, so no
        # cross-clock alignment is involved) -- this is the "front-loaded vs spread" answer
        w("### 6d-0. Cumulative fault arrival (path B handler trace, median over clones)")
        w()
        marks_ms = [10, 25, 50, 100, 150, 200, 300, 400, 500, 750, 1000, 1500, 2000]
        curves = []
        for r in fB:
            fp = r.get("uffd_trace_path")
            if not fp:
                continue
            fl = read_faults(fp)
            if not fl:
                continue
            t0 = fl[0][1]
            times = [(b - t0) / 1e6 for _, b, _ in fl]
            n = len(times)
            import bisect
            curves.append([100.0 * bisect.bisect_right(times, m) / n for m in marks_ms])
        if curves:
            w("| elapsed since first fault | % of the request's faults already taken |")
            w("|---|---|")
            for i, m in enumerate(marks_ms):
                w(f"| {m} ms | {med([c[i] for c in curves]):.1f}% |")
            w()
            R["arrival_curve"] = {str(m): med([c[i] for c in curves])
                                  for i, m in enumerate(marks_ms)}

        w("### 6d. Temporal shape — when the faults arrive")
        w()
        w("Faults attributed to stages by aligning the handler trace to the stage-2 abort "
          "stream (both record the same events; origin matched on the first fault).")
        w()
        rows = []
        for r in fB:
            k = r.get("kvm_ftrace") or {}
            seq = read_ipaseq(k.get("seq_file") or "")
            mm = r.get("marks_mono") or {}
            if not seq or not mm:
                continue
            bounds = [("restore->GO", "restore_begin", "go_sent"),
                      ("GO->exec up", "go_sent", "exec_up"),
                      ("net check", "exec_up", "net_up"),
                      ("render", "net_up", "render_ok"),
                      ("probes", "render_ok", "hold_start"),
                      ("hold+teardown", "hold_start", "exit")]
            row = {}
            for lab, a, b in bounds:
                if a in mm and b in mm:
                    row[lab] = len(window(seq, mm[a], mm[b]))
            rows.append(row)
        if rows:
            w("| stage | A stage-2 aborts | B stage-2 aborts |")
            w("|---|---|---|")
            rowsA = []
            for r in fA:
                k = r.get("kvm_ftrace") or {}
                seq = read_ipaseq(k.get("seq_file") or "")
                mm = r.get("marks_mono") or {}
                if not seq or not mm:
                    continue
                rr = {}
                for lab, a, b in [("restore->GO", "restore_begin", "go_sent"),
                                  ("GO->exec up", "go_sent", "exec_up"),
                                  ("net check", "exec_up", "net_up"),
                                  ("render", "net_up", "render_ok"),
                                  ("probes", "render_ok", "hold_start"),
                                  ("hold+teardown", "hold_start", "exit")]:
                    if a in mm and b in mm:
                        rr[lab] = len(window(seq, mm[a], mm[b]))
                rowsA.append(rr)
            temporal = {}
            for lab in ["restore->GO", "GO->exec up", "net check", "render", "probes",
                        "hold+teardown"]:
                va = med([x.get(lab) for x in rowsA])
                vb = med([x.get(lab) for x in rows])
                temporal[lab] = {"A": va, "B": vb}
                w(f"| {lab} | {f(va, 0)} | {f(vb, 0)} |")
            w()
            R["temporal"] = temporal

    if args.md:
        Path(args.md).write_text("\n".join(lines) + "\n")
    json.dump(R, open(out / "analysis.json", "w"), indent=1, default=str)
    print("\n".join(lines))


if __name__ == "__main__":
    main()
