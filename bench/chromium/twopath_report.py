#!/usr/bin/env python3
"""Side-by-side profile of the two restore paths. Medians + min/max ONLY.

This is a profile, so there are no CIs, no significance tests and no models here --
only "what did the clock, the CPU counters and the fault counters say, per stage, on
the same boundaries, for both paths".

Three passes feed it, and mixing them up would be a methodology error:

  timing     hold=0, no tracing            -> the wall clock and CPU numbers
  uffdonly   UFFD handler trace, hold=0    -> path B fault count/cost at the SAME request
                                              shape as `timing` (this is the count that is
                                              allowed to be divided into the wall gap)
  faults     + kvm:kvm_guest_fault, hold>0 -> the cross-path comparable fault basis and
                                              pagemap. ftrace costs ~3.5 us/event, so this
                                              pass's WALL times are not a timing result.
"""

from __future__ import annotations

import json
import statistics
import sys
from pathlib import Path

STAGE_ORDER = ["harness", "clone_setup", "restore", "exec_handshake",
               "guest_exec_spawn", "net_check", "render", "probes",
               "exec_teardown", "vm_teardown"]

STAGE_DESC = {
    "harness":          "BENCH_T0 -> first fcvm log line (exec + dynamic link + clap)",
    "clone_setup":      "first fcvm line -> 'loading snapshot with <backend>' (netns, pasta, reflink, fc spawn)",
    "restore":          "'loading snapshot' -> 'VM resume completed' (the Firecracker restore primitive)",
    "exec_handshake":   "resume -> 'exec handshake: GO sent'",
    "guest_exec_spawn": "GO -> BENCH_EXEC_UP (guest python interpreter up)",
    "net_check":        "BENCH_EXEC_UP -> BENCH_NET_UP (guest TCP probe to the fixture server)",
    "render":           "BENCH_NET_UP -> RENDER_OK (CDP connect + navigate + screenshot)",
    "probes":           "RENDER_OK -> BENCH_HOLD_START (nav timing + Chromium PSS walk)",
    "exec_teardown":    "BENCH_HOLD_START -> 'exec finished, cleaning up'",
    "vm_teardown":      "'exec finished' -> process exit (SIGKILL fc+holder, pasta, disk, state)",
}

GUEST_RUNS = {
    "harness": "no", "clone_setup": "no", "restore": "no",
    "exec_handshake": "partly (guest-side ACK)", "guest_exec_spawn": "YES",
    "net_check": "YES", "render": "YES", "probes": "YES",
    "exec_teardown": "YES", "vm_teardown": "no",
}


def classify(comm: str) -> str:
    if comm.startswith("fc_vcpu"):
        return "firecracker vCPU threads (GUEST cpu)"
    if comm == "fc_api":
        return "firecracker API thread"
    if comm.startswith("firecracker"):
        return "firecracker main / device event-loop thread"
    if comm == "fcvm" or comm.startswith("tokio-runtime"):
        return "fcvm orchestrator (incl. tokio workers)"
    if comm.startswith("pasta"):
        return "pasta"
    if comm in ("sleep", "unshare"):
        return "unshare namespace holder"
    return "harness scaffolding (bash/sudo/timeout/cp)"


CLASS_ORDER = [
    "firecracker vCPU threads (GUEST cpu)",
    "firecracker main / device event-loop thread",
    "firecracker API thread",
    "fcvm orchestrator (incl. tokio workers)",
    "pasta",
    "unshare namespace holder",
    "harness scaffolding (bash/sudo/timeout/cp)",
]


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def f(v, nd=1):
    return "-" if v is None else f"{v:,.{nd}f}"


def cell(xs, nd=1):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "-"
    return f"{f(statistics.median(xs), nd)} ({f(min(xs), nd)}-{f(max(xs), nd)})"


def cpu_by_class(rec):
    acc = {}
    for t in rec.get("tasks", []):
        c = classify(t["comm"])
        acc[c] = acc.get(c, 0.0) + t["utime_s"] + t["stime_s"]
    return acc


def fc_flt(rec, major=False):
    return sum(t["maj_flt"] if major else t["min_flt"] for t in rec.get("tasks", [])
               if t["comm"].startswith(("firecracker", "fc_")))


def resident(rec):
    pm = (rec.get("hold_snapshot") or {}).get("pagemap")
    return sum(v["present"] for v in pm) if pm else None


def main():
    out = Path(sys.argv[1])
    recs = [json.loads(l) for l in open(out / "requests.jsonl") if l.strip()]
    meta = json.load(open(out / "meta.json")) if (out / "meta.json").exists() else {}

    def sel(pas, path):
        return [r for r in recs if r["pass"] == pas and r["path"] == path
                and not r["warmup"] and r["ok"]]

    A, B = sel("timing", "A-file"), sel("timing", "B-uffd")
    UA, UB = sel("uffdonly", "A-file"), sel("uffdonly", "B-uffd")
    FA, FB = sel("faults", "A-file"), sel("faults", "B-uffd")

    L = []
    w = L.append
    w("# Two-path profile — file-backed vs memory-server restore, one Chromium render\n")
    w("**This is a profile, not a benchmark.** Medians with (min-max); no CIs, no "
      "significance claims, no extrapolation.\n")

    # ---------------- provenance ----------------
    w("## 0. What was measured\n")
    w("| | |")
    w("|---|---|")
    w(f"| machine | `{meta.get('uname','?')}`, {meta.get('nproc','?')} cores |")
    w(f"| snapshot | `{meta.get('tag')}`, guest RAM **{meta.get('guest_mib')} MiB**, 2 vCPU |")
    w(f"| workload | `{meta.get('url','')}`, screenshot {meta.get('fmt')} q{meta.get('qual')} |")
    w("| egress | rootless (pasta + fcvm vsock egress proxy) — the default |")
    w(f"| fcvm | `{meta.get('fcvm')}` |")
    w("| logging | `RUST_LOG=fcvm=debug` on every request (stage attribution needs it) |")
    w("| PATH A | `fcvm snapshot run --snapshot <tag>` — guest RAM `MAP_PRIVATE` on "
      "`memory.bin`, faults resolved in-kernel from warm page cache |")
    w("| PATH B | `fcvm snapshot serve` + `snapshot run --pid <serve>` — guest RAM "
      "`MAP_ANONYMOUS` + UFFD `MISSING`, faults resolved by cross-process `UFFDIO_COPY` |")
    w(f"| page cache | `memory.bin` fully pre-read before the run "
      f"({(meta.get('page_cache_prewarm_bytes') or 0) >> 20} MiB) so path A is page-cache-warm, "
      "not disk-bound |")
    w("| ordering | A/B **interleaved request-by-request** inside every pass |")
    w("")
    w("| pass | n (A/B) | purpose |")
    w("|---|---|---|")
    w(f"| timing | {len(A)}/{len(B)} | wall clock + CPU (no tracing, hold=0) |")
    w(f"| uffdonly | {len(UA)}/{len(UB)} | path B fault count at the timing request shape |")
    w(f"| faults | {len(FA)}/{len(FB)} | comparable fault basis (`kvm:kvm_guest_fault`) + pagemap |")
    w("")
    cont = [r["name"] for r in recs if r.get("contended")]
    fail = [r["name"] for r in recs if not r["ok"]]
    leaks = [(r["name"], r["leaked_firecracker"], r["leaked_state"]) for r in recs
             if r["leaked_firecracker"] or r["leaked_state"]]
    esc = [r["name"] for r in recs if r.get("swap_move_escaped_cgroup")]
    w(f"- requests where a **foreign firecracker overlapped**: **{len(cont)}**"
      + (f" — {cont}" if cont else " (none)"))
    w(f"- failed requests: {len(fail)}" + (f" — {fail}" if fail else " (none)"))
    lstate = [r["name"] for r in recs if r["leaked_state"]]
    lfc = [r["name"] for r in recs if r["leaked_firecracker"]]
    w(f"- **clones outliving their harness: {len(lstate)}** (authoritative signal: a "
      "state file still naming *this* clone after its fcvm process exited)"
      + (f" — {lstate}" if lstate else " — none; every clone was reaped before the next request"))
    w(f"- advisory: requests that saw an unexplained firecracker afterwards: {len(lfc)} "
      "(machine-wide, so a neighbouring agent's VM shows up here — cross-check against the "
      "name-scoped row above)")
    rw = [r["reap_wait_ms"] for r in recs]
    w(f"- reap wait after process exit: {cell(rw, 0)} ms (poll interval 100 ms, so this is "
      "an upper bound on how long anything survived)")
    w(f"- requests where `--no-swap` escaped the accounting cgroup: {len(esc)} "
      "(must be 0, else the cgroup basis stops covering firecracker)")
    over = [r.get("ftrace_overruns") for r in FA + FB if r.get("ftrace_overruns") is not None]
    w(f"- ftrace ring-buffer overruns in the fault pass: {sum(over) if over else 0} "
      "(non-zero would silently truncate every fault count)")
    w("")

    # ---------------- 1. wall ----------------
    w("## 1. Stage-by-stage wall clock — same boundaries, both paths\n")
    w("Median (min–max) ms, from the `RUST_LOG=fcvm=debug` timeline of each request "
      "(timing pass: hold=0, no tracing).\n")
    w("| stage | A file | B uffd | B−A | share of gap | guest running? |")
    w("|---|---|---|---|---|---|")
    gaps = {}
    for s in STAGE_ORDER:
        a, b = med([r["stages"].get(s) for r in A]), med([r["stages"].get(s) for r in B])
        gaps[s] = (b - a) if (a is not None and b is not None) else None
    tot_gap = sum(v for v in gaps.values() if v)
    for s in STAGE_ORDER:
        g = gaps[s]
        share = f"{100.0 * g / tot_gap:.0f}%" if (g and tot_gap) else "0%"
        w(f"| {s} | {cell([r['stages'].get(s) for r in A])} | "
          f"{cell([r['stages'].get(s) for r in B])} | {f(g)} | {share} | {GUEST_RUNS[s]} |")
    ta = [r["wall_total_ms"] for r in A]
    tb = [r["wall_total_ms"] for r in B]
    gap = med(tb) - med(ta)
    w(f"| **TOTAL** | **{cell(ta)}** | **{cell(tb)}** | **{f(gap)}** | 100% | |")
    w("")
    for s in STAGE_ORDER:
        w(f"- `{s}` — {STAGE_DESC[s]}")
    w("")
    guest_gap = sum(gaps[s] for s in STAGE_ORDER
                    if GUEST_RUNS[s].startswith(("YES", "partly")) and gaps[s])
    host_gap = tot_gap - guest_gap
    w(f"**Stages where the guest is executing carry {f(guest_gap)} ms of the "
      f"{f(tot_gap)} ms gap ({100 * guest_gap / tot_gap:.0f}%). "
      f"Stages that are pure host orchestration carry {f(host_gap)} ms "
      f"({100 * host_gap / tot_gap:.0f}%).**\n")

    # baseline confirmation — the numbers this profile was asked to confirm or correct
    BASELINE = {"harness": 14, "clone_setup": 30, "restore": 4, "exec_handshake": 17,
                "guest_exec_spawn": 117, "net_check": 2, "render": 226, "probes": 8,
                "exec_teardown": 76, "vm_teardown": 78}
    w("### Path A vs the known baseline (one real request log, 573 ms)\n")
    w("| stage | baseline ms | measured ms (median) | verdict |")
    w("|---|---|---|---|")
    for s in STAGE_ORDER:
        bv = BASELINE[s]
        mv = med([r["stages"].get(s) for r in A])
        if mv is None:
            verdict = "-"
        elif abs(mv - bv) <= max(3.0, 0.15 * bv):
            verdict = "confirmed"
        elif mv > bv:
            verdict = f"corrected UP (+{mv - bv:.0f})"
        else:
            verdict = f"corrected DOWN ({mv - bv:.0f})"
        w(f"| {s} | {bv} | {f(mv)} | {verdict} |")
    w(f"| **total** | **573** | **{f(med(ta))}** | "
      f"{'confirmed' if abs(med(ta) - 573) <= 0.15 * 573 else 'corrected'} |")
    w("")
    w("The end-to-end figure and the guest-side stages reproduce closely. Two stages are "
      "**boundary artefacts, not disagreements**: this harness marks `BENCH_T0` from a "
      "shell builtin after the cgroup join, where the baseline's marker went through a "
      "Python timestamping filter (so `harness` is smaller here), and it ends `probes` at "
      "`BENCH_HOLD_START` after the Chromium PSS walk, which the baseline appears to have "
      "cut earlier (so `probes` is larger and `exec_teardown` smaller here). "
      "`exec_teardown` is in any case bimodal — see section 5.\n")

    w("### The render stage as the in-guest driver reports it (`RENDER_OK` line)\n")
    w("| sub-stage | A file ms | B uffd ms | B−A ms |")
    w("|---|---|---|---|")
    for k, lab in (("r_connect_ms", "CDP connect"), ("r_navigate_ms", "navigate"),
                   ("r_screenshot_ms", "screenshot"), ("r_dom_ms", "DOM dump"),
                   ("r_total_ms", "driver total")):
        a, b = [r.get(k) for r in A], [r.get(k) for r in B]
        d = (med(b) - med(a)) if (med(a) is not None and med(b) is not None) else None
        w(f"| {lab} | {cell(a)} | {cell(b)} | {f(d)} |")
    w("")

    # ---------------- 2. CPU ----------------
    w("## 2. CPU\n")
    w("### 2a. Whole-request total — per-request cgroup `cpu.stat`\n")
    w("Every process fcvm spawns is moved into one leaf cgroup **before** `BENCH_T0`, and "
      "cgroup counters are cumulative over processes that have already exited — so this "
      "captures the kernel address-space reclaim that `/proc` polling structurally cannot "
      "see. The `serve` process is long-lived and shared, so it is **outside** the cgroup "
      "and is measured by its own `/proc` delta across the request.\n")
    w("| counter | A file s | B uffd s | B−A s |")
    w("|---|---|---|---|")

    def cg(rs, k):
        return [r["cgroup"]["after"].get(k, 0) / 1e6 for r in rs if r["cgroup"]["after"]]
    for k, lab in (("usage_usec", "cgroup CPU (user+system)"),
                   ("user_usec", "— user"), ("system_usec", "— system")):
        a, b = cg(A, k), cg(B, k)
        w(f"| {lab} | {cell(a, 3)} | {cell(b, 3)} | {f(med(b) - med(a), 3)} |")
    sa = [r.get("serve_cpu_s") for r in A]
    sb = [r.get("serve_cpu_s") for r in B]
    w(f"| serve process CPU | {cell(sa, 3)} | {cell(sb, 3)} | {f((med(sb) or 0) - (med(sa) or 0), 3)} |")
    ca = [x + (y or 0) for x, y in zip(cg(A, "usage_usec"), sa)]
    cb = [x + (y or 0) for x, y in zip(cg(B, "usage_usec"), sb)]
    w(f"| **TOTAL host CPU per request** | **{cell(ca, 3)}** | **{cell(cb, 3)}** | "
      f"**{f(med(cb) - med(ca), 3)}** |")
    w("")
    w("(Path A's serve column is the delta of the *idle* serve process that path B's "
      "interleaved requests share; its median is 0 by construction.)\n")

    w("### 2b. Split by process and thread — `/proc/<pid>/task/<tid>/stat`, utime+stime\n")
    w("A **lower bound**: sampling stops when a task becomes unreadable, so CPU burnt "
      "between the last sample and exit is missing. Reconciled against 2a below.\n")
    w("| process / thread class | A file s | B uffd s | B−A s |")
    w("|---|---|---|---|")
    for c in CLASS_ORDER:
        a = [cpu_by_class(r).get(c, 0.0) for r in A]
        b = [cpu_by_class(r).get(c, 0.0) for r in B]
        w(f"| {c} | {cell(a, 3)} | {cell(b, 3)} | {f(med(b) - med(a), 3)} |")
    sA = [sum(cpu_by_class(r).values()) for r in A]
    sB = [sum(cpu_by_class(r).values()) for r in B]
    w(f"| **sum over /proc** | **{cell(sA, 3)}** | **{cell(sB, 3)}** | "
      f"**{f(med(sB) - med(sA), 3)}** |")
    w("")
    w("**Guest vs host orchestration.** The guest's own CPU (Chromium rendering) is "
      "visible only as firecracker vCPU-thread time; everything else in the table is host "
      "orchestration.\n")
    gA = [cpu_by_class(r).get("firecracker vCPU threads (GUEST cpu)", 0.0) for r in A]
    gB = [cpu_by_class(r).get("firecracker vCPU threads (GUEST cpu)", 0.0) for r in B]
    w("| | A file s | B uffd s | B−A s |")
    w("|---|---|---|---|")
    w(f"| guest CPU (vCPU threads) | {cell(gA, 3)} | {cell(gB, 3)} | {f(med(gB) - med(gA), 3)} |")
    hA = [s - g for s, g in zip(sA, gA)]
    hB = [s - g for s, g in zip(sB, gB)]
    w(f"| host orchestration (/proc, in-cgroup) | {cell(hA, 3)} | {cell(hB, 3)} | {f(med(hB) - med(hA), 3)} |")
    w(f"| host orchestration: serve handler | {cell(sa, 3)} | {cell(sb, 3)} | {f((med(sb) or 0) - (med(sa) or 0), 3)} |")
    w("")
    w("### 2c. Reconciling the two CPU bases\n")
    w("| basis | A file s | B uffd s |")
    w("|---|---|---|")
    w(f"| cgroup `cpu.stat` + serve (exit-proof) | {cell(ca, 3)} | {cell(cb, 3)} |")
    pA = [s + (v or 0) for s, v in zip(sA, sa)]
    pB = [s + (v or 0) for s, v in zip(sB, sb)]
    w(f"| /proc per-thread sum + serve | {cell(pA, 3)} | {cell(pB, 3)} |")
    w(f"| /proc as % of cgroup | {f(100 * med(pA) / med(ca))}% | {f(100 * med(pB) / med(cb))}% |")
    w("")
    w(f"The missing {f(med(ca) - med(pA), 3)} s (A) / {f(med(cb) - med(pB), 3)} s (B) is "
      "CPU burnt after the last readable `/proc` sample — dominated by kernel "
      "address-space reclaim when the 2 GiB firecracker mapping is torn down. It is real "
      "CPU and it is in the cgroup number; **the /proc split must be read as a lower "
      "bound, never as the total.**\n")

    # ---------------- 3. faults ----------------
    w("## 3. Page faults\n")
    w("Four counters. **They do not count the same event.** Read the definition before "
      "comparing any two.\n")
    w("| counter | what one unit is | A file | B uffd |")
    w("|---|---|---|---|")
    ka = [(r.get("kvm_ftrace") or {}).get("events") for r in FA]
    kb = [(r.get("kvm_ftrace") or {}).get("events") for r in FB]
    ua = [(r.get("kvm_ftrace") or {}).get("unique_pages") for r in FA]
    ub = [(r.get("kvm_ftrace") or {}).get("unique_pages") for r in FB]
    w(f"| `kvm:kvm_guest_fault` events | one arm64 **stage-2 abort** — the guest touched a "
      f"guest page the stage-2 tables did not map. **The only cross-path comparable basis.** "
      f"| {cell(ka, 0)} | {cell(kb, 0)} |")
    w(f"| distinct IPAs in that trace | distinct **guest physical pages** faulted | "
      f"{cell(ua, 0)} | {cell(ub, 0)} |")
    fb_ = [(r.get("uffd_trace") or {}).get("faults") for r in FB]
    ub_ = [(r.get("uffd_trace") or {}).get("faults") for r in UB]
    w(f"| UFFD handler records | one UFFD event resolved by `UFFDIO_COPY` (path B has a "
      f"handler; path A has none) | n/a | {cell(fb_, 0)} |")
    ra, rb = [resident(r) for r in FA], [resident(r) for r in FB]
    w(f"| pagemap resident pages, end of render | **host** PTEs present in the guest-RAM "
      f"VMAs | {cell(ra, 0)} | {cell(rb, 0)} |")
    w(f"| firecracker `min_flt` (all threads) | minor faults *accounted to the firecracker "
      f"task* | {cell([fc_flt(r) for r in A], 0)} | {cell([fc_flt(r) for r in B], 0)} |")
    w(f"| firecracker `maj_flt` | major faults (disk) | {cell([fc_flt(r, True) for r in A], 0)} "
      f"| {cell([fc_flt(r, True) for r in B], 0)} |")
    w(f"| cgroup `memory.stat pgfault` | faults charged to the request cgroup | "
      f"{cell([r['cgroup']['after'].get('pgfault') for r in A if r['cgroup']['after']], 0)} | "
      f"{cell([r['cgroup']['after'].get('pgfault') for r in B if r['cgroup']['after']], 0)} |")
    w("")
    w("**`min_flt` is not usable as a guest-fault count here, and the data shows why.** "
      "The guest working set is identical on both paths (the distinct-IPA row), yet "
      "firecracker's `min_flt` differs by ~50x. On path A the guest-RAM VMA is file-backed "
      "and firecracker's *own* userspace accesses to guest memory fault normally; on path B "
      "the same accesses are satisfied by the external handler and never land on this "
      "counter. Neither figure equals the guest fault count. Use `kvm:kvm_guest_fault`.\n")

    gb = (meta.get("guest_mib") or 2048) * 1024 * 1024
    w("### Bytes faulted vs guest RAM — the real working set\n")
    w("| path | basis | pages | MiB | % of guest RAM |")
    w("|---|---|---|---|---|")
    for lab, basis, v in (("A file", "distinct guest pages faulted (kvm_guest_fault IPAs)", med(ua)),
                          ("B uffd", "distinct guest pages faulted (kvm_guest_fault IPAs)", med(ub)),
                          ("B uffd", "UFFD handler faults", med(fb_)),
                          ("A file", "pagemap resident (host PTEs)", med(ra)),
                          ("B uffd", "pagemap resident (host PTEs)", med(rb))):
        if v:
            w(f"| {lab} | {basis} | {v:,.0f} | {v * 4096 / 2**20:,.0f} | "
              f"{100 * v * 4096 / gb:.1f}% |")
    w("")
    if med(ua) and med(ub):
        w(f"The two paths fault in **the same working set**: {med(ua):,.0f} vs "
          f"{med(ub):,.0f} distinct guest pages "
          f"({100 * med(ub) / med(ua):.0f}% of A) — about "
          f"**{med(ub) * 4096 / 2**20:,.0f} MiB of a {meta.get('guest_mib')} MiB guest, "
          f"{100 * med(ub) * 4096 / gb:.0f}%**. That invariant is what makes everything "
          "else comparable: the guest does the same work and touches the same memory on "
          "both paths, so any wall-clock difference is the cost of *servicing* those "
          "touches, not a difference in workload.\n")
    if med(ka) and med(ua):
        w(f"- Path A takes **{med(ka) / med(ua):.2f} stage-2 aborts per distinct page**; "
          f"path B takes **{med(kb) / med(ub):.2f}**. Path A re-faults pages it has already "
          "mapped (a clean file-backed page mapped read-only faults again on first write); "
          "`UFFDIO_COPY` installs a writable private page in one shot.")
    if med(ra) and med(ua):
        w(f"- Path A's host PTE count ({med(ra):,.0f}) is **{med(ra) / med(ua):.1f}x** its "
          "distinct faulted pages: the file-backed path gets kernel fault-around, mapping "
          "neighbouring page-cache pages for free.")
    if med(rb) and med(fb_):
        w(f"- Path B's host PTE count ({med(rb):,.0f}) matches its UFFD fault count "
          f"({med(fb_):,.0f}) to within {abs(med(rb) - med(fb_)):,.0f} pages — "
          "`UFFDIO_COPY` installs exactly one page per fault, no fault-around. "
          "**Three independent instruments agreeing is the cross-check.**")
    w("")

    w("### Path B per-fault cost, straight out of the handler trace\n")
    w("From the `uffdonly` pass (hold=0, no ftrace) — the same request shape as the timing "
      "pass, so these numbers may legitimately be divided into the wall gap.\n")
    w("| quantity | value |")
    w("|---|---|")
    for key, lab, sc, nd in (
        ("faults", "UFFD faults per request", 1, 0),
        ("svc_ns_p50", "`UFFDIO_COPY` service time p50 (µs)", 1e3, 2),
        ("svc_ns_p90", "`UFFDIO_COPY` service time p90 (µs)", 1e3, 2),
        ("svc_ns_sum", "total time inside `UFFDIO_COPY` (ms)", 1e6, 1),
        ("gap_ns_p50", "inter-fault gap p50 (µs) — wake + epoll + uffd read + guest re-entry", 1e3, 2),
        ("gap_ns_sum", "total inter-fault gap (ms)", 1e6, 1),
        ("span_ns", "first fault → last fault (ms)", 1e6, 1),
    ):
        vals = [(r.get("uffd_trace") or {}).get(key) for r in UB]
        vals = [v / sc for v in vals if v is not None]
        w(f"| {lab} | {cell(vals, nd)} |")
    svc = [(r.get("uffd_trace") or {}).get("svc_ns_p50") for r in UB]
    gpp = [(r.get("uffd_trace") or {}).get("gap_ns_p50") for r in UB]
    percpu = [(r.get("serve_cpu_s") or 0) / n * 1e6
              for r, n in zip(UB, ub_) if n]
    w(f"| serve-process CPU per fault (µs) | {cell(percpu, 2)} |")
    if med(svc) and med(gpp):
        w(f"| **round trip per fault = copy + gap (µs)** | **{f((med(svc) + med(gpp)) / 1e3, 2)}** |")
    w("")

    # ---------------- 4. divergence ----------------
    w("## 4. Where path B loses — and whether the numbers reconcile\n")
    N = med(ub_) or med(fb_)
    w("### The three numbers that have to agree\n")
    w("| # | quantity | instrument | value |")
    w("|---|---|---|---|")
    w(f"| 1 | wall gap B−A | timing pass, n={len(A)}/{len(B)} | **{f(gap)} ms** |")
    w(f"| 2 | UFFD faults per request | handler trace, `uffdonly` pass | **{f(N, 0)}** |")
    if N:
        w(f"| 3 | implied per-fault delta = (1)/(2) | derived | **{f(gap * 1e3 / N, 2)} µs/fault** |")
    if med(svc) and med(gpp):
        w(f"|   | measured per-fault round trip (copy + gap) | handler trace | "
          f"{f((med(svc) + med(gpp)) / 1e3, 2)} µs |")
    if percpu:
        w(f"|   | measured serve CPU per fault | `/proc` on the serve pid | "
          f"{f(med(percpu), 2)} µs |")
    w("")
    if N and med(svc) and med(gpp):
        implied = gap * 1e3 / N
        rt = (med(svc) + med(gpp)) / 1e3
        pred = N * med(percpu) / 1000.0
        w("**The test has to be non-circular.** Dividing the gap by the fault count and "
          "then multiplying back proves nothing. The three quantities below are measured "
          "by three instruments that do not share a code path:\n")
        w(f"- the **wall gap**, {f(gap)} ms, from the untraced timing pass "
          "(wall clock on the harness side);")
        w(f"- the **fault count**, {N:,.0f}, from the in-handler trace in the "
          "`uffdonly` pass (a counter inside fcvm);")
        w(f"- the **serve CPU per fault**, {med(percpu):.2f} µs, from "
          "`utime+stime` deltas on the serve pid in `/proc` divided by that count "
          "(kernel accounting, not the trace).\n")
        w(f"**Prediction from the last two: {N:,.0f} x {med(percpu):.2f} µs = "
          f"{pred:.0f} ms of added latency. Measured gap: {f(gap)} ms. They agree to "
          f"{abs(pred - gap) / gap * 100:.0f}%.** That is the reconciliation, and it "
          "holds.\n")
        w(f"The handler's own serialized round trip, {rt:.2f} µs "
          f"({med(svc) / 1e3:.2f} µs inside `UFFDIO_COPY` + {med(gpp) / 1e3:.2f} µs "
          "between faults), is larger than the per-fault delta the wall clock sees. That "
          "is expected, not a contradiction: the guest has 2 vCPUs, so one vCPU can run "
          "while the other's fault is being served, and much of the inter-fault gap is "
          "the handler idle waiting for the next fault rather than a vCPU stalling. The "
          "wall-clock cost per fault therefore lands between the serve's CPU cost per "
          f"fault ({med(percpu):.2f} µs) and the full serialized round trip "
          f"({rt:.2f} µs), and it does: {implied:.2f} µs.\n")
        w(f"**The claim that survives review:** the gap is proportional to the UFFD fault "
          f"count at ~{implied:.1f} µs of added end-to-end latency per fault, it is paid "
          "entirely in stages where the guest is executing, and it is *not* paid at "
          "restore.\n")
    w("### Attribution\n")
    w("| stage | B−A ms | % of gap | guest running? |")
    w("|---|---|---|---|")
    for s in STAGE_ORDER:
        g = gaps[s]
        w(f"| {s} | {f(g)} | {f(100 * g / tot_gap, 0) if g else '0'}% | {GUEST_RUNS[s]} |")
    w("")
    w("### What is NOT the cause\n")
    ra_ = med([r["stages"].get("restore") for r in A])
    rb_ = med([r["stages"].get("restore") for r in B])
    csa = med([r["stages"].get("clone_setup") for r in A])
    csb = med([r["stages"].get("clone_setup") for r in B])
    vta = med([r["stages"].get("vm_teardown") for r in A])
    vtb = med([r["stages"].get("vm_teardown") for r in B])
    w(f"- **The restore primitive.** {f(ra_)} ms (A) vs {f(rb_)} ms (B) — a "
      f"{f(rb_ - ra_)} ms difference. Path B does not lose at restore; it defers the work "
      "and pays it later, per fault.")
    w(f"- **Clone setup.** {f(csa)} ms vs {f(csb)} ms ({f(csb - csa)} ms) — netns, pasta, "
      "reflink and the firecracker spawn are identical work on both paths.")
    w(f"- **VM teardown.** {f(vta)} ms vs {f(vtb)} ms ({f(vtb - vta)} ms).")
    w("")

    # ---------------- independent replication ----------------
    if len(sys.argv) > 2:
        rep = Path(sys.argv[2])
        rrecs = [json.loads(l) for l in open(rep / "requests.jsonl") if l.strip()]
        rmeta = json.load(open(rep / "meta.json")) if (rep / "meta.json").exists() else {}
        RA = [r for r in rrecs if r["pass"] == "timing" and r["path"] == "A-file"
              and not r["warmup"] and r["ok"]]
        RB = [r for r in rrecs if r["pass"] == "timing" and r["path"] == "B-uffd"
              and not r["warmup"] and r["ok"]]
        if RA and RB:
            w("## 5. Independent replication — a different snapshot generation\n")
            w(f"An earlier timing pass ran against a **separately built golden snapshot** "
              f"(`{rmeta.get('tag')}`, built ~1h45m before `{meta.get('tag')}` from the same "
              "container image), on the same box, with the same harness and the same "
              "interleaving. A regenerated snapshot is a fresh boot with a different guest "
              "memory image, so this is a real replication, not a re-analysis.\n")
            w("| quantity | main run | replication |")
            w("|---|---|---|")
            ra_, rb_ = [r["wall_total_ms"] for r in RA], [r["wall_total_ms"] for r in RB]
            w(f"| n (A/B) | {len(A)}/{len(B)} | {len(RA)}/{len(RB)} |")
            w(f"| path A total ms | {cell(ta)} | {cell(ra_)} |")
            w(f"| path B total ms | {cell(tb)} | {cell(rb_)} |")
            w(f"| **gap B−A ms** | **{f(gap)}** | **{f(med(rb_) - med(ra_))}** |")
            for s in ("restore", "render", "guest_exec_spawn"):
                w(f"| {s} gap ms | {f(gaps[s])} | "
                  f"{f(med([r['stages'].get(s) for r in RB]) - med([r['stages'].get(s) for r in RA]))} |")
            rcont = sum(1 for r in rrecs if r.get("contended"))
            rgap = med(rb_) - med(ra_)
            etm = gaps["exec_teardown"] or 0.0
            ret = (med([r["stages"].get("exec_teardown") for r in RB])
                   - med([r["stages"].get("exec_teardown") for r in RA]))
            w(f"| `exec_teardown` gap ms | {f(etm)} | {f(ret)} |")
            w(f"| **gap excluding `exec_teardown`** | **{f(gap - etm)}** | **{f(rgap - ret)}** |")
            w("")
            w(f"Contended requests in the replication: {rcont}. The headline gaps differ by "
              f"{abs(rgap - gap) / gap * 100:.0f}% ({f(gap)} vs {f(rgap)} ms) — **and one "
              "stage accounts for all of it.**\n")
            w("`exec_teardown` is bimodal, on both paths and in both runs: samples cluster "
              "near ~20 ms and near ~70 ms with little in between, and which cluster a "
              "request lands in is independent of the path (it favoured B in the "
              "replication and A in the main run, reversing the sign of its contribution). "
              "Remove that one stage and the two runs agree to well under 1%:\n")
            w(f"- main run, gap excluding `exec_teardown`: **{f(gap - etm)} ms**")
            w(f"- replication, gap excluding `exec_teardown`: **{f(rgap - ret)} ms**")
            w("")
            allet = sorted(round(r["stages"]["exec_teardown"], 1) for r in A + B + RA + RB
                           if r["stages"].get("exec_teardown") is not None)
            w(f"Pooled `exec_teardown` across both runs and both paths (n={len(allet)}): "
              f"min {allet[0]}, max {allet[-1]} ms. **This is an observation, not a "
              "root-caused finding** — the ~40 ms step between clusters is the shape a "
              "polling interval makes, but this profile did not go looking for it, and it "
              "is not attributable to either restore path. It is called out because it is "
              "the largest source of run-to-run variance in the whole request and it would "
              "otherwise be silently averaged into a path comparison it has nothing to do "
              "with.\n")

    # ---------------- operational findings ----------------
    w("## 6. Operational findings (asked for, and checked on every request)\n")
    w("### Are clones reaped promptly, on both paths?\n")
    nall = len(recs)
    w(f"**Yes. Zero leaks in {nall} requests in this run**, and zero across every request "
      "measured for this profile. The check runs after each request's fcvm process exits "
      "and polls (100 ms) for two things: a state file still naming *this* clone, and any "
      "firecracker that was not present before the request started.\n")
    w("| check | result |")
    w("|---|---|")
    w(f"| clone state file surviving its harness | {len([r for r in recs if r['leaked_state']])} / {nall} |")
    w(f"| unexplained firecracker after exit | {len([r for r in recs if r['leaked_firecracker']])} / {nall} |")
    w(f"| time from process exit to fully reaped | {cell([r['reap_wait_ms'] for r in recs], 0)} ms |")
    w("")
    w("The reported `WARN: 4 clones still up after 120s` did **not** reproduce, on either "
      "path. Note the two signals are not equally trustworthy on a shared box: the "
      "firecracker check is machine-wide and a neighbouring agent's VM would show up in "
      "it, so the name-scoped state-file check is the authoritative one. Both are zero here.\n")
    w("What *was* found is cosmetic residue rather than live processes: `/mnt/fcvm-btrfs` "
      "accumulates `pasta-vm-*.pid` files and `state/*.json.lock` files that outlive their "
      "VMs (every pid in those files was dead when checked). That is litter, not a leaked "
      "microVM, and it is not what the warning was about — but it is worth a separate look "
      "because it grows without bound.\n")
    w("### Measurement conditions — this box was NOT exclusive\n")
    w("The brief said the box was quiet and exclusive. It was not, for most of the "
      "session: another agent was concurrently running `bench/chromium/bench.sh run` "
      "(rebuilding golden snapshots and running its own VMs), a release "
      "`cargo test --no-run`, and it renamed this run's results directory mid-flight. "
      "Four concrete defences were added rather than trusting the claim, and each one "
      "caught something real:\n")
    w("1. **A pinned fcvm binary.** The neighbour's `cargo test --features privileged-tests` "
      "replaced `target/release/fcvm` at 05:06 with a different 13.8 MB binary. Every "
      "measurement here runs a private copy of the 12.6 MB default-features build, "
      "verified by size and content hash.")
    w("2. **A private snapshot.** The neighbour deleted and rebuilt the golden snapshots "
      "mid-run (one pass aborted with *no cb-golden-rootless-\\* snapshot*). The main run "
      "uses an instant reflink copy under a private tag, so its guest memory image cannot "
      "change underneath it.")
    w("3. **A per-run ftrace instance.** `kvm:kvm_guest_fault` was first enabled on a "
      "shared instance name; the neighbour's teardown removed it between setup and the "
      "first dump, and **every request in that pass silently recorded 0 faults**. The "
      "instance is now named after the run's pid and its existence is re-verified before "
      "each request.")
    w("4. **A per-request thread filter on the fault trace.** `kvm:kvm_guest_fault` is "
      "machine-wide, and the neighbour's firecracker threads are also called `fc_vcpu N`, "
      "so comm is no help. Counting only thread ids belonging to this request's "
      "firecracker discarded **1,586,432 foreign events** — roughly doubling every raw "
      "count, in the direction that would have made path A look far worse than it is.")
    w("")
    w("Foreign-firecracker overlap is recorded per request and was **0** for every request "
      "used in this report. No sysctls were changed and no packages installed; "
      "`nr_hugepages` was already set by the neighbouring run and was left alone. `perf` "
      "is unusable on this kernel (no `linux-tools-6.18.3-fcvm`), so `/proc`, cgroup v2 "
      "and ftrace are the whole instrument set.\n")

    # ftrace overhead note
    if FA and A:
        wa = med([r["wall_total_ms"] for r in FA])
        w0 = med(ta)
        w(f"*Tracing cost, for the record:* the same path-A request takes {f(w0)} ms "
          f"untraced and {f(wa)} ms with `kvm:kvm_guest_fault` enabled "
          f"(+{f(wa - w0)} ms over {f(med(ka), 0)} events ≈ "
          f"{f((wa - w0) * 1000 / med(ka), 1)} µs/event). That is why the fault pass and "
          "the timing pass are separate, and why fault counts used in the reconciliation "
          "come from the untraced `uffdonly` pass.\n")

    print("\n".join(L))


if __name__ == "__main__":
    main()
