#!/usr/bin/env python3
"""Analyse the four-arm (driver x memory-backend) profile into one markdown report.

The four arms are exec/file, exec/uffd, cdp/file, cdp/uffd, interleaved request by request
in a single session against a single golden. That design is what lets this report make two
DIFFERENT comparisons without either contaminating the other:

  BACKEND GAP   uffd - file, computed WITHIN a driver. The question "how expensive is the
                memory server" only has one answer per request path, because the request
                path decides how many pages the guest touches.
  DRIVER DELTA  cdp - exec, computed WITHIN a backend. This is the correction: the exec
                driver starts a Python interpreter inside the guest, and the pages that
                interpreter touches are charged to the backend gap on the uffd arm.

Fault numbers are WINDOWED to spawn->render_ok, not taken as per-request totals, because
the tail after the screenshot (probes, hold, teardown) is not part of serving the request
and differs in length between drivers.
"""

from __future__ import annotations

import argparse
import json
import statistics
import struct
from pathlib import Path

PAGE = 4096
ARMS = [("exec", "A-file"), ("exec", "B-uffd"), ("cdp", "A-file"), ("cdp", "B-uffd")]
LABEL = {("exec", "A-file"): "exec/file", ("exec", "B-uffd"): "exec/uffd",
         ("cdp", "A-file"): "cdp/file", ("cdp", "B-uffd"): "cdp/uffd"}


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def mmm(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return (None, None, None, 0)
    return (statistics.median(xs), min(xs), max(xs), len(xs))


def f(v, nd=1):
    return "-" if v is None else f"{v:,.{nd}f}"


def rng(xs, nd=1):
    m, lo, hi, n = mmm(xs)
    return "-" if m is None else f"{m:,.{nd}f} [{lo:,.{nd}f}-{hi:,.{nd}f}]"


def sel(recs, pas, driver, path, ok_only=True):
    r = [x for x in recs if x["pass"] == pas and x.get("driver") == driver
         and x["path"] == path and not x["warmup"]]
    return [x for x in r if x.get("ok")] if ok_only else r


def read_ipaseq(p):
    try:
        buf = Path(p).read_bytes()
    except OSError:
        return []
    n = len(buf) // 16
    if not n:
        return []
    v = struct.unpack_from(f"<{n * 2}Q", buf)
    return list(zip(v[0::2], v[1::2]))


def window(seq, t_lo, t_hi):
    return [x for x in seq if t_lo <= x[0] / 1e6 <= t_hi]


def working_set(rec):
    """Distinct guest pages touched between spawn and the screenshot being in hand.

    The window ends at render_ok, so the probes/hold/teardown tail -- which is not part of
    serving the request, and whose length differs by driver -- cannot inflate it.
    """
    k = rec.get("kvm_ftrace") or {}
    mm = rec.get("marks_mono") or {}
    seq = read_ipaseq(k.get("seq_file") or "")
    if not seq or "t0" not in mm or "render_ok" not in mm:
        return None, None
    w = window(seq, mm["t0"], mm["render_ok"])
    return len(w), len({p for _, p in w})


def cgroup_cpu(rec):
    a, b = rec["cgroup"]["before"], rec["cgroup"]["after"]
    if not a or not b or "usage_usec" not in a or "usage_usec" not in b:
        return None
    return (b["usage_usec"] - a["usage_usec"]) / 1000.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--md", default=None)
    args = ap.parse_args()
    out = Path(args.out)
    recs = [json.loads(x) for x in open(out / "requests.jsonl") if x.strip()]
    meta = json.load(open(out / "meta.json"))

    T = {a: sel(recs, "timing", *a) for a in ARMS}
    F = {a: sel(recs, "faults", *a) for a in ARMS}

    # ---------------------------------------------------------------- integrity
    integ = {
        "n_records": len(recs),
        "warmups_discarded": sum(1 for r in recs if r["warmup"]),
        "failed": [r["name"] for r in recs if not r.get("ok") and not r["warmup"]],
        "contended": [r["name"] for r in recs if r.get("contended")],
        "foreign_fc_max": max((r.get("foreign_fc_max") or 0) for r in recs),
        "load_max": max((r.get("load_max") or 0) for r in recs),
        "rustc_during": [r["name"] for r in recs if r.get("rustc_during")],
        "leaked": [r["name"] for r in recs if r.get("leaked_firecracker")
                   or r.get("leaked_state")],
        "ftrace_overruns": sum((r.get("ftrace_overruns") or 0) for r in recs),
        "swap_escaped": [r["name"] for r in recs if r.get("swap_move_escaped_cgroup")],
    }
    # THE EXCLUSIVITY PROOF. Every fault-pass trace must contain kvm_guest_fault events
    # from exactly two vCPU TIDs -- our clone has 2 vCPUs, so two is the whole VM and
    # nothing else. Three or more means another VM's faults landed in our (machine-wide)
    # ftrace instance and every fault number in this report would be someone else's too.
    vt = []
    for r in recs:
        k = r.get("kvm_ftrace") or {}
        if not k:
            continue
        vt.append({"name": r["name"], "vcpu_tids": k.get("vcpu_tids") or [],
                   "n": len(k.get("vcpu_tids") or []),
                   "foreign_filtered": k.get("foreign_events_filtered"),
                   "by_comm": k.get("by_comm")})
    integ["vcpu_tid_counts"] = sorted({x["n"] for x in vt})
    integ["traces_checked"] = len(vt)
    integ["traces_with_2_vcpus"] = sum(1 for x in vt if x["n"] == 2)
    integ["foreign_events_filtered_total"] = sum(x["foreign_filtered"] or 0 for x in vt)

    # ------------------------------------------------------------------- stages
    def stage_rows(driver):
        names = []
        for r in T[(driver, "A-file")] + T[(driver, "B-uffd")]:
            for k in r["stages"]:
                if k not in names:
                    names.append(k)
        rows = []
        for s in names:
            a = [r["stages"].get(s) for r in T[(driver, "A-file")]]
            b = [r["stages"].get(s) for r in T[(driver, "B-uffd")]]
            ma, mb = med(a), med(b)
            rows.append({"stage": s, "A": rng(a), "B": rng(b),
                         "A_med": ma, "B_med": mb,
                         "gap": (mb - ma) if (ma is not None and mb is not None) else None})
        return rows

    totals = {}
    for a in ARMS:
        totals[a] = mmm([r["wall_total_ms"] for r in T[a]])

    # ---------------------------------------------------------------------- CPU
    cpu = {}
    for a in ARMS:
        cg = [cgroup_cpu(r) for r in T[a]]
        srv = [r.get("serve_cpu_s") for r in T[a]] if a[1] == "B-uffd" else []
        srv_ms = [s * 1000.0 for s in srv if s is not None]
        mcg, msrv = med(cg), med(srv_ms)
        cpu[a] = {
            "cgroup_ms": mcg,
            "serve_ms": msrv,
            # The memory server sits OUTSIDE the clone's cgroup, so a cgroup-only basis
            # understates the uffd arm. Both are reported and then summed.
            "total_ms": (mcg or 0) + (msrv or 0) if mcg is not None else None,
            "serve_pct": (100.0 * msrv / ((mcg or 0) + msrv)) if msrv else None,
        }

    # ------------------------------------------------------------------- faults
    faults = {}
    for a in ARMS:
        evs, pgs = [], []
        for r in F[a]:
            e, p = working_set(r)
            if e is not None:
                evs.append(e)
                pgs.append(p)
        uf = [(r.get("uffd_trace") or {}).get("faults") for r in F[a]]
        tot = [(r.get("kvm_ftrace") or {}).get("events") for r in F[a]]
        mp = med(pgs)
        faults[a] = {
            "aborts_to_render": med(evs), "pages_to_render": mp,
            "ws_mib": (mp * PAGE / (1 << 20)) if mp else None,
            "ws_pct": (100.0 * mp * PAGE / (meta["guest_mib"] << 20)) if mp else None,
            "aborts_whole": med(tot),
            "uffd_faults": med([x for x in uf if x]),
            "n": len(pgs),
        }

    # ------------------------------------------------------- render sub-timings
    rend = {}
    for a in ARMS:
        rend[a] = {k: med([r.get("r_" + k) for r in T[a]])
                   for k in ("connect_ms", "navigate_ms", "screenshot_ms", "total_ms")}
    cdpstg = {}
    for a in (("cdp", "A-file"), ("cdp", "B-uffd")):
        keys = ("resolve_ms", "tcp_ms", "upgrade_ms", "enable_ms", "navigate_ms",
                "screenshot_ms", "decode_ms", "nav_timing_ms")
        cdpstg[a] = {k: med([(r.get("cdp_stages") or {}).get(k) for r in T[a]]) for k in keys}
        cdpstg[a]["port_wait_ms"] = med([r.get("cdp_port_wait_ms") for r in T[a]])
        cdpstg[a]["state_wait_ms"] = med([r.get("cdp_state_wait_ms") for r in T[a]])

    analysis = {"meta": meta, "integrity": integ, "totals": {LABEL[a]: totals[a] for a in ARMS},
                "cpu": {LABEL[a]: cpu[a] for a in ARMS},
                "faults": {LABEL[a]: faults[a] for a in ARMS},
                "render": {LABEL[a]: rend[a] for a in ARMS},
                "cdp_stages": {LABEL[a]: cdpstg[a] for a in cdpstg},
                "stages_exec": stage_rows("exec"), "stages_cdp": stage_rows("cdp"),
                "vcpu_tid_detail": vt}
    json.dump(analysis, open(out / "analysis.json", "w"), indent=1)

    if not args.md:
        print(json.dumps({k: analysis[k] for k in ("integrity", "totals", "cpu", "faults")},
                         indent=1))
        return

    # An arm with no successful non-warmup requests cannot be reported on. Fail loudly
    # rather than emit a table full of "-" that a reader would take for a measurement.
    empty = [LABEL[a] for a in ARMS if totals[a][3] == 0]
    if empty:
        raise SystemExit(f"REFUSING to write a report: arms with zero successful "
                         f"non-warmup requests: {empty}. Failed: {integ['failed']}")

    L = []
    w = L.append
    tex_a, tex_b = totals[("exec", "A-file")][0], totals[("exec", "B-uffd")][0]
    tcd_a, tcd_b = totals[("cdp", "A-file")][0], totals[("cdp", "B-uffd")][0]
    gap_exec, gap_cdp = tex_b - tex_a, tcd_b - tcd_a

    w("# CDP-path re-baseline: file-backed restore vs UFFD memory server\n")
    w(f"Machine: `{meta['uname']}`, {meta['nproc']} cores. Snapshot `{meta['tag']}`, "
      f"guest {meta['guest_mib']} MiB, page `{Path(meta['url']).name}`, screenshot "
      f"{meta['fmt']} q{meta['qual']}, egress rootless. Drivers: {meta.get('drivers')}.")
    w(f"Raw records: `requests.jsonl` ({integ['n_records']} rows), `meta.json`, "
      f"`requests/*.log`, `traces/`. Warmups discarded explicitly: "
      f"{integ['warmups_discarded']}. Failed non-warmup requests: {len(integ['failed'])}.")
    w(f"Load before {meta['quiet_before']}, after {meta.get('quiet_after')}.\n")

    w("## 0. Integrity gates\n")
    w("| gate | result |")
    w("|---|---|")
    w(f"| fault traces checked | {integ['traces_checked']} |")
    w(f"| traces containing EXACTLY 2 vCPU TIDs | **{integ['traces_with_2_vcpus']}/"
      f"{integ['traces_checked']}** |")
    w(f"| distinct vCPU-TID counts seen | {integ['vcpu_tid_counts']} |")
    w(f"| foreign kvm events filtered out | {integ['foreign_events_filtered_total']} |")
    w(f"| requests flagged contended | {len(integ['contended'])} |")
    w(f"| max foreign firecrackers during any request | {integ['foreign_fc_max']} |")
    w(f"| rustc seen during any request | {len(integ['rustc_during'])} |")
    w(f"| max load1 during any request | {integ['load_max']:.2f} |")
    w(f"| leaked firecracker / state | {len(integ['leaked'])} |")
    w(f"| ftrace buffer overruns (0 = nothing lost) | {integ['ftrace_overruns']} |")
    w(f"| firecracker escaped the leaf cgroup | {len(integ['swap_escaped'])} |")
    w("\nEvery clone has 2 vCPUs, so \"exactly 2 vCPU TIDs in the trace\" is the statement "
      "that the machine-wide ftrace instance saw one VM during that request: ours.\n")

    w("## 1. The headline: totals per arm\n")
    w("| arm | total wall (ms) | vs exec, same backend |")
    w("|---|---|---|")
    for a in ARMS:
        m, lo, hi, n = totals[a]
        base = totals[("exec", a[1])][0]
        d = "-" if a[0] == "exec" else f"**{m - base:+,.1f}**"
        w(f"| {LABEL[a]} | {m:,.1f} [{lo:,.1f}-{hi:,.1f}] (n={n}) | {d} |")
    w("")
    w("| backend gap (uffd - file) | ms |")
    w("|---|---|")
    w(f"| on the **exec** request path | {gap_exec:,.1f} |")
    w(f"| on the **cdp** request path | **{gap_cdp:,.1f}** |")
    w(f"| shrink | {gap_exec - gap_cdp:,.1f} ({100 * (gap_exec - gap_cdp) / gap_exec:.0f}% "
      f"of the exec-path gap) |")
    w("")

    for drv, rows in (("exec", analysis["stages_exec"]), ("cdp", analysis["stages_cdp"])):
        w(f"## 2{'a' if drv == 'exec' else 'b'}. Stage-by-stage wall clock — {drv} driver\n")
        w("Median [min-max] ms, timing pass (no ftrace).\n")
        w("| stage | A file-backed | B memory-server | B-A |")
        w("|---|---|---|---|")
        for r in rows:
            w(f"| {r['stage']} | {r['A']} | {r['B']} | {f(r['gap'])} |")
        ta = totals[(drv, "A-file")]
        tb = totals[(drv, "B-uffd")]
        w(f"| **TOTAL** | **{ta[0]:,.1f}** [{ta[1]:,.1f}-{ta[2]:,.1f}] | "
          f"**{tb[0]:,.1f}** [{tb[1]:,.1f}-{tb[2]:,.1f}] | **{tb[0] - ta[0]:,.1f}** |")
        w(f"\nn = {ta[3]} (A) / {tb[3]} (B) non-warmup requests.\n")

    w("## 3. CPU — two bases, reconciled\n")
    w("The memory server runs OUTSIDE the clone's leaf cgroup, so a cgroup-only basis "
      "understates the uffd arms. Both are reported and summed.\n")
    w("| arm | leaf cgroup CPU (ms) | memory-server CPU (ms) | total (ms) | server share |")
    w("|---|---|---|---|---|")
    for a in ARMS:
        c = cpu[a]
        share = "-" if c["serve_pct"] is None else f"{c['serve_pct']:.0f}%"
        w(f"| {LABEL[a]} | {f(c['cgroup_ms'])} | {f(c['serve_ms'])} | "
          f"{f(c['total_ms'])} | {share} |")
    w("")
    # THE POINT of carrying both bases: they disagree about how much more CPU the memory
    # server costs, and the cgroup-only answer is the flattering one.
    w("**What the omission costs.** uffd CPU as a multiple of the file arm, same driver:\n")
    w("| driver | cgroup basis only | cgroup + memory server |")
    w("|---|---|---|")
    for drv in ("exec", "cdp"):
        fa, fb = cpu[(drv, "A-file")], cpu[(drv, "B-uffd")]
        if fa["cgroup_ms"] and fb["cgroup_ms"]:
            r_cg = fb["cgroup_ms"] / fa["cgroup_ms"]
            r_tot = (fb["total_ms"] or 0) / fa["cgroup_ms"]
            w(f"| {drv} | {r_cg:.2f}x (+{100 * (r_cg - 1):.0f}%) | "
              f"**{r_tot:.2f}x (+{100 * (r_tot - 1):.0f}%)** |")
    w("")

    w("## 4. Guest page faults and working set (fault pass, windowed spawn->render_ok)\n")
    w("| measure | " + " | ".join(LABEL[a] for a in ARMS) + " |")
    w("|---|" + "---|" * len(ARMS))
    for key, lab, nd in (("aborts_to_render", "stage-2 aborts, spawn->render_ok", 0),
                         ("pages_to_render", "distinct guest pages, spawn->render_ok", 0),
                         ("ws_mib", "working set (MiB)", 1),
                         ("ws_pct", "as % of guest RAM", 1),
                         ("uffd_faults", "UFFD faults served", 0),
                         ("aborts_whole", "stage-2 aborts, whole request", 0)):
        w(f"| {lab} | " + " | ".join(f(faults[a][key], nd) for a in ARMS) + " |")
    w("")

    w("## 5. In-request render breakdown\n")
    w("| phase | " + " | ".join(LABEL[a] for a in ARMS) + " |")
    w("|---|" + "---|" * len(ARMS))
    for k in ("connect_ms", "navigate_ms", "screenshot_ms", "total_ms"):
        w(f"| {k[:-3]} | " + " | ".join(f(rend[a][k]) for a in ARMS) + " |")
    w("\n### cdp driver, per-hop (host-side, cdpdrive.py's own timers)\n")
    w("| hop | cdp/file | cdp/uffd |")
    w("|---|---|---|")
    for k in ("state_wait_ms", "port_wait_ms", "resolve_ms", "tcp_ms", "upgrade_ms",
              "enable_ms", "navigate_ms", "screenshot_ms", "decode_ms", "nav_timing_ms"):
        w(f"| {k[:-3]} | {f(cdpstg[('cdp', 'A-file')].get(k))} | "
          f"{f(cdpstg[('cdp', 'B-uffd')].get(k))} |")
    w("")

    # Is the /json/list round trip removable? Only if the target id is stable across
    # clones, in which case it can be resolved once before the snapshot and passed as
    # --ws-url. Reported as a fact about the data, not as a recommendation.
    tids = [r.get("cdp_target_id") for r in recs
            if r.get("driver") == "cdp" and r.get("cdp_target_id")]
    uniq = sorted(set(tids))
    resolve_med = med([cdpstg[a].get("resolve_ms") for a in cdpstg])
    w("## 5b. What `cdp_resolve` actually contains — and why it is NOT free to remove\n")
    w(f"- distinct CDP target ids across {len(tids)} cdp requests: **{len(uniq)}**"
      + (f" (`{uniq[0]}`)" if len(uniq) == 1 else ""))
    w(f"- `resolve_ms` (first HTTP round trip to the guest), median: **{f(resolve_med)} ms**")
    w("")
    w("The obvious inference — \"the target id is stable, so pre-wire `--ws-url` and save "
      "the whole of `cdp_resolve`\" — is WRONG, and `probe_readiness.py` is why. Racing a "
      "TCP connect against an HTTP GET on the same fresh clone:\n")
    w("| event | when |")
    w("|---|---|")
    w("| first successful TCP connect to the forwarded port | 8.7-39 ms after spawn |")
    w("| first successful `/json/list` | a further **60-64 ms** later |")
    w("")
    w("pasta owns the host-side listener and completes the handshake itself, so a "
      "successful connect says nothing about the guest. That ~60 ms is the guest becoming "
      "able to SERVE, and it is charged to whichever request op happens to go first. "
      "Pre-wiring `--ws-url` would delete the `/json/list` round trip but move the "
      "readiness wait into the WebSocket upgrade rather than removing it. The removable "
      "part is one round trip; the rest is guest readiness and needs a different fix.")
    w("")

    # Not every millisecond of the exec->cdp delta is a request-path saving. The two
    # drivers do slightly different amounts of INSTRUMENTATION, and pretending otherwise
    # would inflate the headline.
    w("## 5c. What is NOT a like-for-like saving\n")
    w("The exec driver's `probes` stage does more work than the cdp driver's: it opens a "
      "SECOND, in-guest CDP connection for nav timing and then scans `/proc` for Chromium "
      "PSS. The cdp driver's probe is one `Runtime.evaluate` on the connection it already "
      "has, and no PSS scan. That difference is measurement overhead, not request path, "
      "and it must be subtracted before calling the rest a win.\n")
    w("| driver | probes (ms, file) | probes (ms, uffd) |")
    w("|---|---|---|")
    for drv in ("exec", "cdp"):
        rows = analysis["stages_exec"] if drv == "exec" else analysis["stages_cdp"]
        pr = next((r for r in rows if r["stage"] == "probes"), None)
        w(f"| {drv} | {f(pr['A_med']) if pr else '-'} | {f(pr['B_med']) if pr else '-'} |")
    pe = next((r for r in analysis["stages_exec"] if r["stage"] == "probes"), None)
    pc = next((r for r in analysis["stages_cdp"] if r["stage"] == "probes"), None)
    if pe and pc and pe["A_med"] and pc["A_med"]:
        da = pe["A_med"] - pc["A_med"]
        db = (pe["B_med"] or 0) - (pc["B_med"] or 0)
        w(f"\nSo **{da:,.1f} ms** (file) and **{db:,.1f} ms** (uffd) of the exec->cdp "
          f"delta is instrumentation, not request path. Adjusted driver delta: "
          f"file {tcd_a - tex_a + da:+,.1f} ms, uffd {tcd_b - tex_b + db:+,.1f} ms.")
    w("")

    w("## 6. Largest single stage on the new baseline\n")
    for drv in ("cdp",):
        for pathname, lab in (("A-file", "cdp/file"), ("B-uffd", "cdp/uffd")):
            rows = [(r["stage"], r["A_med"] if pathname == "A-file" else r["B_med"])
                    for r in analysis["stages_cdp"]]
            rows = [(s, v) for s, v in rows if v is not None]
            rows.sort(key=lambda x: -x[1])
            tot = totals[(drv, pathname)][0]
            w(f"\n**{lab}** — total {tot:,.1f} ms\n")
            w("| rank | stage | ms | % of total |")
            w("|---|---|---|---|")
            for i, (s, v) in enumerate(rows[:5], 1):
                w(f"| {i} | {s} | {v:,.1f} | {100 * v / tot:.1f}% |")
    w("")

    Path(args.md).write_text("\n".join(L) + "\n")
    print(f"wrote {args.md}")


if __name__ == "__main__":
    main()
