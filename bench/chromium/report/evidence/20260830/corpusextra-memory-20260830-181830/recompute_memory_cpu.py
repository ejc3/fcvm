#!/usr/bin/env python3
"""Recompute every memory and CPU figure quoted in memory-cpu-analysis.md.

One convention: p50 is statistics.median. A mean is called a mean and is only
compared against a mean. Reads only the records in this directory (plus the
sibling hostcdp run for the p50 convention string); writes nothing.
"""
import json, os, statistics, sys

HERE = os.path.dirname(os.path.abspath(__file__))
MEM = os.path.join(HERE, "memory")
summary = json.load(open(os.path.join(MEM, "summary.json")))
cput = json.load(open(os.path.join(MEM, "cputime.json")))
run = json.load(open(os.path.join(MEM, "run.json")))
samples = [json.loads(l) for l in open(os.path.join(MEM, "samples.jsonl"))]

BASES = ["cgroup_mib", "pss_mib", "mem_available_delta_mib"]
SIDES = ["fcvm-clone", "host-container"]
NS = [1, 2, 4, 8]
URLS = run["urls"]

def cells(side, n):
    return [c for c in summary["cells"] if c["side"] == side and c["n"] == n]

def p50(vals):
    return statistics.median(sorted(vals))

def per_inst(side, n, basis):
    return [c[basis] / c["instances_counted"] for c in cells(side, n)]

def total(side, n, basis):
    return [c[basis] for c in cells(side, n)]

def h(title):
    print("\n" + "=" * 78 + f"\n{title}\n" + "=" * 78)

h("A. PER-INSTANCE, p50 over 3 reps (statistics.median), with [min-max]")
tab = {}
for basis in BASES:
    print(f"\n--- {basis} (MiB per instance) ---")
    print(f"{'N':>3} {'fcvm p50':>9} {'fcvm min-max':>17} {'ctr p50':>9} {'ctr min-max':>17} "
          f"{'fcvm-ctr':>9} {'sign':>10} {'ranges overlap':>15}")
    for n in NS:
        f = sorted(per_inst("fcvm-clone", n, basis)); c = sorted(per_inst("host-container", n, basis))
        fm, cm = statistics.median(f), statistics.median(c)
        d = fm - cm
        overlap = not (f[-1] < c[0] or c[-1] < f[0])
        tab[(basis, n)] = (fm, f[0], f[-1], cm, c[0], c[-1], d, overlap)
        print(f"{n:>3} {fm:9.1f} {f[0]:8.1f}-{f[-1]:<8.1f} {cm:9.1f} {c[0]:8.1f}-{c[-1]:<8.1f} "
              f"{d:+9.1f} {('fcvm' if d<0 else 'container'):>10} {('YES' if overlap else 'no'):>15}")

h("B. THE SAME, WITH THE SHARED UFFD SERVE AMORTISED OVER N")
print("The serve sits OUTSIDE the summed leaf cgroup, so cells[].cgroup_mib and")
print("cells[].pss_mib both exclude it, exactly as the MemAvailable baseline does.")
print("cells[].serve_cgroup_mib / serve_pss_mib record it in every steady fcvm cell.\n")
serve = {}
for n in NS:
    serve[n] = (p50(c["serve_cgroup_mib"] for c in cells("fcvm-clone", n)),
                p50(c["serve_pss_mib"] for c in cells("fcvm-clone", n)))
    print(f"  N={n}  serve cgroup {serve[n][0]:7.1f} MiB   serve PSS {serve[n][1]:5.1f} MiB")
for basis, key in (("cgroup_mib", 0), ("pss_mib", 1)):
    print(f"\n--- {basis} per instance, fcvm with serve/N added ---")
    print(f"{'N':>3} {'fcvm bare':>10} {'+serve/N':>10} {'fcvm+serve':>11} {'container':>10} "
          f"{'lower bare':>11} {'lower +serve':>13}")
    for n in NS:
        fm, cm = tab[(basis, n)][0], tab[(basis, n)][3]
        add = serve[n][key] / n
        print(f"{n:>3} {fm:10.1f} {add:10.1f} {fm+add:11.1f} {cm:10.1f} "
              f"{('fcvm' if fm<cm else 'container'):>11} {('fcvm' if fm+add<cm else 'container'):>13}")

h("C. ORDERING AGREEMENT ACROSS BASES")
for label, add in (("as recorded (serve excluded on both attributed bases)", False),
                   ("with the serve amortised into the fcvm attributed bases", True)):
    print(f"\n{label}:")
    for n in NS:
        v = {}
        for basis, key in (("cgroup_mib", 0), ("pss_mib", 1), ("mem_available_delta_mib", None)):
            fm, cm = tab[(basis, n)][0], tab[(basis, n)][3]
            if add and key is not None:
                fm += serve[n][key] / n
            v[basis] = "fcvm" if fm < cm else "container"
        att = {k: v[k] for k in ("cgroup_mib", "pss_mib")}
        print(f"  N={n}: cgroup->{v['cgroup_mib']:<9} pss->{v['pss_mib']:<9} "
              f"memavail->{v['mem_available_delta_mib']:<9} attributed: "
              + ("AGREE" if len(set(att.values())) == 1 else "DISAGREE"))

h("D. TOTALS, STEPWISE MARGINALS WITH THEIR ENVELOPE, AND LINEAR FITS")
for basis in BASES:
    print(f"\n--- {basis} (MiB, whole set of N instances) ---")
    for side in SIDES:
        t = [p50(total(side, n, basis)) for n in NS]
        line = f"  {side:<15} " + "  ".join(f"N={n} {x:8.1f}" for n, x in zip(NS, t))
        print(line)
        parts = []
        for a, b in zip(NS, NS[1:]):
            va, vb = sorted(total(side, a, basis)), sorted(total(side, b, basis))
            mid = (p50(vb) - p50(va)) / (b - a)
            lo, hi = (vb[0] - va[-1]) / (b - a), (vb[-1] - va[0]) / (b - a)
            parts.append(f"{a}->{b} {mid:7.1f} [{lo:.1f},{hi:.1f}]")
        print(f"  {'':<15} marginal per added instance: " + " | ".join(parts))
        pts = [(c["n"], c[basis]) for n in NS for c in cells(side, n)]
        xs = [p[0] for p in pts]; ys = [p[1] for p in pts]
        mx, my = statistics.mean(xs), statistics.mean(ys)
        slope = sum((x - mx) * (y - my) for x, y in pts) / sum((x - mx) ** 2 for x in xs)
        icept = my - slope * mx
        pred = [slope * x + icept for x in xs]
        ss_res = sum((y - p) ** 2 for y, p in zip(ys, pred))
        ss_tot = sum((y - my) ** 2 for y in ys)
        resid_by_n = {n: statistics.mean([y - (slope * x + icept) for x, y in pts if x == n]) for n in NS}
        print(f"  {'':<15} least squares: slope {slope:7.1f} intercept {icept:8.1f} "
              f"R2 {1 - ss_res/ss_tot:.4f} mean residual by N " +
              " ".join(f"{n}:{resid_by_n[n]:+7.1f}" for n in NS))

h("E. WHAT EACH CELL ACTUALLY RENDERED (the N axis is not a pure scale axis)")
print("corpus_mem.py: instance i of a cell renders urls[i % len(urls)], i in range(n).")
print("So cell N renders urls[0..N-1], a different workload at every N.\n")
hp = os.path.join(os.path.dirname(HERE), "corpusextra-hostcdp-20260830-172413",
                  "hostcdp-cpu2", "summary.json")
host_url = json.load(open(hp))["per_url"] if os.path.exists(hp) else {}
for n in NS:
    used = URLS[:n]
    lat = [host_url[u]["p50_ms"] for u in used if u in host_url]
    extra = f"  host warm p50 mean {statistics.mean(lat):7.1f} ms" if lat else ""
    print(f"  N={n}: {len(used)} url(s){extra}")
    for u in used:
        mark = "  <- added at this N" if u in URLS[NS[NS.index(n)-1]:n] and n > 1 else ""
        print(f"        {host_url.get(u, {}).get('p50_ms', float('nan')):8.1f} ms  {u}{mark}")

h("F. RUN SHAPE: the two sides are BLOCKED in time, not interleaved")
rows = sorted(samples, key=lambda r: r["ts"])
t0 = rows[0]["ts"]
blocks = []
for r in rows:
    if not blocks or blocks[-1][0] != r["side"]:
        blocks.append([r["side"], r["ts"] - t0, r["ts"] - t0])
    else:
        blocks[-1][2] = r["ts"] - t0
for b in blocks:
    print(f"  {b[0]:<15} t = {b[1]:7.1f} .. {b[2]:7.1f} s")
print(f"  side switches across the whole run: {len(blocks) - 1}")

h("G. SERVE RESIDENCY IN THE PRE-SAMPLES (the MemAvailable baseline)")
fs = [s for s in samples if s["side"] == "fcvm-clone" and s["phase"] == "pre"]
hs = [s for s in samples if s["side"] == "host-container" and s["phase"] == "pre"]
sv = [s["serve_cgroup_kb"] / 1024 for s in fs]; sp = [s["serve_pss_kb"] / 1024 for s in fs]
print(f"  fcvm pre-samples n={len(fs)}: serve cgroup {min(sv):.1f}-{max(sv):.1f} MiB "
      f"(p50 {statistics.median(sv):.1f}), serve PSS {min(sp):.1f}-{max(sp):.1f} MiB, "
      f"clones {sorted({s['clones'] for s in fs})}")
print(f"  container pre-samples n={len(hs)}: pool_containers {sorted({s['pool_containers'] for s in hs})}, "
      f"pool_cgroup_kb {sorted({s['pool_cgroup_kb'] for s in hs})}")
fa = [s["mem_available_kb"] / 1024 for s in fs]; ha = [s["mem_available_kb"] / 1024 for s in hs]
print(f"  pre-sample MemAvailable p50: fcvm {statistics.median(fa):.1f} MiB, "
      f"container {statistics.median(ha):.1f} MiB, difference {statistics.median(fa)-statistics.median(ha):+.1f} MiB "
      f"({statistics.median(fa)/1024:.1f} GiB scale)")

h("H. WITHIN-CELL SPREAD (max-min over 3 reps, per instance, MiB)")
print(f"  {'basis':<26} {'side':<15} " + "".join(f"N={n:<8}" for n in NS))
for basis in BASES:
    for side in SIDES:
        sp2 = [max(per_inst(side, n, basis)) - min(per_inst(side, n, basis)) for n in NS]
        print(f"  {basis:<26} {side:<15} " + "".join(f"{x:<10.1f}" for x in sp2))

h("I. GAP BETWEEN BASES ON THE SAME SIDE (per instance, p50 MiB)")
for n in NS:
    for side, idx in (("fcvm-clone", 0), ("host-container", 3)):
        cg = tab[("cgroup_mib", n)][idx]; ps = tab[("pss_mib", n)][idx]
        ma = tab[("mem_available_delta_mib", n)][idx]
        print(f"  N={n} {side:<15} cgroup {cg:7.1f}  pss {ps:7.1f}  memavail {ma:7.1f}"
              f"   pss-memavail {ps-ma:+7.1f}")
d1 = tab[("pss_mib", 1)][0] - tab[("mem_available_delta_mib", 1)][0]
d2 = tab[("pss_mib", 1)][3] - tab[("mem_available_delta_mib", 1)][3]
print(f"  side-dependence of that gap at N=1: {d1:.1f} - {d2:.1f} = {d1-d2:.1f} MiB")
big = max(abs(tab[(b, n)][6]) for b in BASES for n in NS)
print(f"  largest side-to-side p50 difference anywhere in the tables: {big:.1f} MiB")

h("J. DERIVED FIGURES QUOTED IN PROSE")
for basis in ("cgroup_mib", "pss_mib"):
    for n in NS:
        fm, cm = tab[(basis, n)][0], tab[(basis, n)][3]
        print(f"  {basis:<22} N={n}  fcvm vs container: {(fm-cm)/cm*100:+.1f}%")
r = tab[("pss_mib", 1)][0] / tab[("cgroup_mib", 1)][0]
print(f"  fcvm N=1 PSS / fcvm N=1 cgroup = {tab[('pss_mib',1)][0]:.1f} / "
      f"{tab[('cgroup_mib',1)][0]:.1f} = {r:.2f}")
print("  fcvm per-instance PSS across N: " +
      ", ".join(f"{tab[('pss_mib',n)][0]:.1f}" for n in NS) + "  (turns UP at N=8)")
print("  container per-instance PSS across N: " +
      ", ".join(f"{tab[('pss_mib',n)][3]:.1f}" for n in NS) + "  (turns UP at N=8)")
print("  fcvm per-instance cgroup across N: " +
      ", ".join(f"{tab[('cgroup_mib',n)][0]:.1f}" for n in NS))
print("  container per-instance cgroup across N: " +
      ", ".join(f"{tab[('cgroup_mib',n)][3]:.1f}" for n in NS))

h("K. CPU TIME")
f, hh = cput["fcvm"], cput["host"]
vals = sorted(r["cpu_ms"] for r in f["records"])
med, mean = statistics.median(vals), statistics.mean(vals)
hmean = hh["total_cpu_ms"] / hh["n"]
print(f"  fcvm  n={len(vals)}  basis: {f['basis']}")
print(f"        statistics.median {med:.1f} ms   mean {mean:.1f} ms   min {vals[0]:.1f}   max {vals[-1]:.1f}")
print(f"        record field per_request_cpu_ms_p50 = {f['per_request_cpu_ms_p50']} "
      f"= sorted[len//2] = sorted[21]  (sorted[20]={vals[20]:.1f}, sorted[21]={vals[21]:.1f})")
print(f"        serve_cpu_ms_per_request {f['serve_cpu_ms_per_request']} ms")
print(f"  host  n={hh['n']}  basis: {hh['basis']}")
print(f"        total_cpu_ms {hh['total_cpu_ms']} / n = {hmean:.1f} ms (a MEAN); no per-render records exist")
print(f"\n  mean vs mean                    : {mean:.1f} / {hmean:.1f} = {mean/hmean:.2f}x")
print(f"  mean+serve vs mean              : {mean+f['serve_cpu_ms_per_request']:.1f} / {hmean:.1f} = "
      f"{(mean+f['serve_cpu_ms_per_request'])/hmean:.2f}x")
print(f"  NOT LIKE FOR LIKE, record p50 vs host mean : {f['per_request_cpu_ms_p50']:.1f} / {hmean:.1f} = "
      f"{f['per_request_cpu_ms_p50']/hmean:.2f}x")
print(f"  NOT LIKE FOR LIKE, true median vs host mean: {med:.1f} / {hmean:.1f} = {med/hmean:.2f}x")
byu = {}
for r in f["records"]:
    byu.setdefault(r["url"], []).append(r["cpu_ms"])
print("\n  per-URL fcvm CPU p50 (3 reps each):")
for u in URLS:
    print(f"    {p50(byu[u]):9.1f} ms  n={len(byu[u])}  {u}")

h("L. THE MEAN CONVENTION THE EARLIER WRITE-UP USED (for Correction 1 only)")
print("Not this document's convention. Printed so the three figures quoted in")
print("Correction 1 are checkable against the records like everything else.")
for basis in ("pss_mib", "mem_available_delta_mib"):
    f = statistics.mean(per_inst("fcvm-clone", 1, basis))
    c = statistics.mean(per_inst("host-container", 1, basis))
    print(f"  N=1 {basis:<26} mean: fcvm {f:7.1f}  container {c:7.1f}  "
          f"lower: {'fcvm' if f < c else 'container'}")
