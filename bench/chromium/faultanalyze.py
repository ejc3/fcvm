#!/usr/bin/env python3
"""Turn faultbench raw output into the five ground-truth numbers.

  1. fault count (three instruments, each labelled with what it actually counts)
  2. bytes faulted vs guest RAM  -> the real working set
  3. per-fault cost              -> serve CPU / fault, and the in-handler ioctl time
  4. locality                    -> run-length of the faulted SET, sequentiality of the ORDER
  5. stability across clones     -> Jaccard, plus leave-one-out coverage/waste, which is
                                    what a prefetcher would actually experience

Reads: <out>/requests.jsonl, <out>/traces/<cell>/*.faults, <out>/traces/kvm/*.trace
Writes: <out>/summary.json and a text report on stdout.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics as stats
import struct
from collections import defaultdict
from pathlib import Path

PAGE = 4096
CLK_TCK = 100  # kernel USER_HZ on aarch64 Linux


# ---------------------------------------------------------------------------
# parsers
# ---------------------------------------------------------------------------


def read_fault_trace(path: Path):
    """[(file_offset, t_before_ns, t_after_ns)] from the in-handler UFFD trace."""
    data = path.read_bytes()
    n = len(data) // 24
    out = []
    for i in range(n):
        off, t0, t1 = struct.unpack_from("<QQQ", data, i * 24)
        out.append((off, t0, t1))
    return out


# firecracker names its vCPU threads "fc_vcpu 0" -- with a space -- so the comm field
# must be matched non-greedily, not as \S+, or every vCPU fault line is silently dropped.
KVM_RE = re.compile(
    r"^\s*(.*?)-(\d+)\s+\[(\d+)\]\s+\S+\s+([\d.]+):\s+kvm_guest_fault:\s+ipa 0x([0-9a-f]+)")


def read_kvm_trace(path: Path):
    """[(comm, pid, cpu, ts, ipa)] from an ftrace text dump of kvm:kvm_guest_fault."""
    out = []
    with open(path, "r", errors="replace") as f:
        for line in f:
            if "kvm_guest_fault" not in line:
                continue
            m = KVM_RE.match(line)
            if m:
                out.append((m.group(1), int(m.group(2)), int(m.group(3)),
                            float(m.group(4)), int(m.group(5), 16)))
    return out


# ---------------------------------------------------------------------------
# metrics
# ---------------------------------------------------------------------------


def runs_of(sorted_offsets, granule):
    """Maximal runs of consecutive granules in a sorted offset list -> [run_len_in_granules]."""
    if not sorted_offsets:
        return []
    runs, cur = [], 1
    for a, b in zip(sorted_offsets, sorted_offsets[1:]):
        if b == a + granule:
            cur += 1
        else:
            runs.append(cur)
            cur = 1
    runs.append(cur)
    return runs


def locality(offsets_in_order, granule):
    """Spatial clustering of the SET and sequentiality of the ORDER.

    The two answer different questions. Run-length of the sorted set says how big a
    fault-around window could be useful. Sequentiality of the temporal order says
    whether a naive read-ahead-after-fault would hit anything.
    """
    uniq = sorted(set(offsets_in_order))
    rl = runs_of(uniq, granule)
    total = len(uniq)
    if total == 0:
        return {}

    def frac_in_runs_at_least(k):
        return sum(r for r in rl if r >= k) / total

    fwd = near16 = 0
    for a, b in zip(offsets_in_order, offsets_in_order[1:]):
        d = b - a
        if d == granule:
            fwd += 1
        if 0 < abs(d) <= 16 * granule:
            near16 += 1
    npairs = max(1, len(offsets_in_order) - 1)
    return {
        "unique_granules": total,
        "runs": len(rl),
        "run_len_mean": sum(rl) / len(rl),
        "run_len_median": stats.median(rl),
        "run_len_p90": sorted(rl)[int(0.9 * (len(rl) - 1))],
        "run_len_max": max(rl),
        "frac_in_runs_ge2": frac_in_runs_at_least(2),
        "frac_in_runs_ge4": frac_in_runs_at_least(4),
        "frac_in_runs_ge16": frac_in_runs_at_least(16),
        "frac_in_runs_ge64": frac_in_runs_at_least(64),
        "frac_in_runs_ge512": frac_in_runs_at_least(512),
        "order_frac_next_is_plus1": fwd / npairs,
        "order_frac_next_within_16": near16 / npairs,
    }


def jaccard(a, b):
    if not a and not b:
        return 1.0
    return len(a & b) / len(a | b)


def stability(sets):
    """Pairwise Jaccard + the leave-one-out numbers a prefetcher would actually see."""
    if len(sets) < 2:
        return {}
    js = [jaccard(sets[i], sets[j]) for i in range(len(sets)) for j in range(i + 1, len(sets))]
    inter = set.intersection(*sets)
    union = set.union(*sets)
    cov, waste = [], []
    for i, s in enumerate(sets):
        others = set.union(*[t for j, t in enumerate(sets) if j != i])
        cov.append(len(s & others) / max(1, len(s)))
        waste.append(len(others - s) / max(1, len(others)))
    return {
        "n_runs": len(sets),
        "set_sizes": [len(s) for s in sets],
        "jaccard_mean": sum(js) / len(js),
        "jaccard_min": min(js),
        "jaccard_max": max(js),
        "core_size": len(inter),
        "union_size": len(union),
        "core_frac_of_mean_set": len(inter) / (sum(len(s) for s in sets) / len(sets)),
        "loo_coverage_mean": sum(cov) / len(cov),
        "loo_coverage_min": min(cov),
        "loo_waste_mean": sum(waste) / len(waste),
    }


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


# ---------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--include-warmup", action="store_true")
    args = ap.parse_args()
    out = Path(args.out)

    recs = [json.loads(l) for l in open(out / "requests.jsonl")]
    if not args.include_warmup:
        recs = [r for r in recs if not r.get("warmup")]
    recs = [r for r in recs if r.get("rc") == 0]

    # UFFD traces are written per (serve_pid, vm_id) and the vm_ids increment per serve
    # process, so bind them to requests by mtime order within a cell.
    trace_by_cell = defaultdict(list)
    for cell_dir in (out / "traces").iterdir() if (out / "traces").exists() else []:
        if cell_dir.is_dir() and cell_dir.name != "kvm":
            for f in sorted(cell_dir.glob("*.faults"), key=lambda p: p.stat().st_mtime):
                trace_by_cell[cell_dir.name].append(f)

    by_cell = defaultdict(list)
    for r in recs:
        by_cell[(r["cell"], r.get("page", "?"))].append(r)

    summary = {}
    for (cell, page), rs in sorted(by_cell.items()):
        rs.sort(key=lambda r: r["t0"])
        granule = rs[0].get("granule", PAGE)
        guest_bytes = rs[0].get("guest_bytes", 2 << 30)
        traces = trace_by_cell.get(cell, [])
        # traces accumulate across pages within a cell; align by chronological order
        entry = {"cell": cell, "page": page, "n": len(rs), "granule": granule,
                 "guest_bytes": guest_bytes, "requests": []}

        sets_uffd, sets_pm, sets_kvm = [], [], []
        for idx, r in enumerate(rs):
            item = {"name": r["name"], "wall_ms": r["wall_ms"], "rep": r.get("rep")}

            # --- instrument 1: pagemap resident granules (works for every backend)
            snap = r.get("hold_snapshot")
            if snap and snap.get("pagemap"):
                # Key pages as (vma_rank_by_start, granule_index). Host virtual addresses
                # differ per clone, so an absolute address is not comparable between runs;
                # the VMA layout of a given snapshot is, so its rank is a stable key.
                present = set()
                nres = 0
                for vi, v in enumerate(sorted(snap["pagemap"], key=lambda x: x["start"])):
                    for pidx in v["present"]:
                        present.add((vi, (pidx * PAGE) // granule))
                        nres += 1
                item["pagemap_resident_4k"] = nres
                item["pagemap_resident_bytes"] = nres * PAGE
                item["pagemap_frac_of_guest"] = nres * PAGE / guest_bytes
                item["pagemap_resident_granules"] = len(present)
                sets_pm.append(present)
                item["vma_total_bytes"] = sum(v["size"] for v in snap["vmas"])
                item["vma_paths"] = [v["path"] for v in snap["vmas"]]

            # --- instrument 2: min_flt on firecracker (guest RAM is NOT in here; see note)
            ser = r.get("stat_series") or []
            if ser:
                item["fc_minflt_total"] = ser[-1][1] - ser[0][1]
                item["fc_majflt_total"] = ser[-1][2] - ser[0][2]
                item["fc_cpu_ms"] = ((ser[-1][3] + ser[-1][4]) - (ser[0][3] + ser[0][4])) * 1000.0 / CLK_TCK

            # --- instrument 3: UFFD handler trace (exact, UFFD arms only)
            # Bind the trace to the request by mtime falling inside the request window,
            # not by position: one failed request would shift a positional mapping and
            # silently attribute every later trace to the wrong run.
            tf = None
            if r["memarm"] == "uffd":
                cands = [f for f in traces if r["t0"] <= f.stat().st_mtime <= r["t1"] + 5]
                if len(cands) == 1:
                    tf = cands[0]
                elif cands:
                    item["trace_ambiguous"] = [c.name for c in cands]
                    tf = cands[-1]
            if tf is not None:
                tr = read_fault_trace(tf)
                item["uffd_trace_file"] = tf.name
                item["uffd_faults"] = len(tr)
                item["uffd_bytes"] = len(tr) * granule
                item["uffd_frac_of_guest"] = len(tr) * granule / guest_bytes
                svc = [(t1 - t0) / 1000.0 for _, t0, t1 in tr]  # us, ioctl service time
                if svc:
                    item["uffd_ioctl_us_p50"] = pct(svc, 50)
                    item["uffd_ioctl_us_p90"] = pct(svc, 90)
                    item["uffd_ioctl_us_p99"] = pct(svc, 99)
                    item["uffd_ioctl_us_mean"] = sum(svc) / len(svc)
                    item["uffd_ioctl_us_total"] = sum(svc)
                # The ioctl time is only the RESOLUTION. What the guest actually pays per
                # fault is the whole round trip: vCPU traps -> kernel queues the event ->
                # this process is woken through epoll -> read_event -> ioctl -> vCPU resumes.
                # The gap between one fault's resolution and the next fault's arrival
                # bounds the part the ioctl time does not see, and span/count is the
                # end-to-end mean while the guest is fault-bound.
                gaps = [(tr[i + 1][1] - tr[i][2]) / 1000.0 for i in range(len(tr) - 1)]
                if gaps:
                    item["uffd_interfault_us_p50"] = pct(gaps, 50)
                    item["uffd_interfault_us_mean"] = sum(gaps) / len(gaps)
                offs = [o for o, _, _ in tr]
                item["locality"] = locality(offs, granule)
                item["fault_span_ms"] = (tr[-1][2] - tr[0][1]) / 1e6 if tr else 0.0
                if tr:
                    item["uffd_wall_us_per_fault"] = item["fault_span_ms"] * 1000.0 / len(tr)
                sets_uffd.append(set(offs))

            # serve-process CPU attributable to this request (requests are serial)
            sb, sa = r.get("serve_stat_before"), r.get("serve_stat_after")
            if sb and sa:
                item["serve_cpu_ms"] = ((sa[2] + sa[3]) - (sb[2] + sb[3])) * 1000.0 / CLK_TCK
                if item.get("uffd_faults"):
                    item["serve_cpu_us_per_fault"] = item["serve_cpu_ms"] * 1000.0 / item["uffd_faults"]

            # --- instrument 4: kvm:kvm_guest_fault (exact, every backend)
            if r.get("kvm_trace") and Path(r["kvm_trace"]).exists():
                ev = read_kvm_trace(Path(r["kvm_trace"]))
                item["kvm_guest_faults"] = len(ev)
                if ev:
                    ipas = [e[4] for e in ev]
                    base = min(ipas)
                    item["kvm_ipa_min"] = hex(base)
                    item["kvm_ipa_max"] = hex(max(ipas))
                    item["kvm_unique_4k"] = len({i // PAGE for i in ipas})
                    item["kvm_unique_granule"] = len({i // granule for i in ipas})
                    item["kvm_locality"] = locality([(i - base) // granule * granule for i in ipas], granule)
                    sets_kvm.append({(i - base) // granule for i in ipas})
                    item["kvm_bytes_unique_granule"] = item["kvm_unique_granule"] * granule
                    item["kvm_frac_of_guest"] = item["kvm_bytes_unique_granule"] / guest_bytes

            entry["requests"].append(item)

        entry["stability_uffd_offsets"] = stability(sets_uffd) if len(sets_uffd) >= 2 else {}
        entry["stability_pagemap"] = stability(sets_pm) if len(sets_pm) >= 2 else {}
        entry["stability_kvm_ipa"] = stability(sets_kvm) if len(sets_kvm) >= 2 else {}

        def agg(key):
            vs = [i[key] for i in entry["requests"] if i.get(key) is not None]
            if not vs:
                return None
            return {"mean": sum(vs) / len(vs), "min": min(vs), "max": max(vs),
                    "median": stats.median(vs), "n": len(vs)}

        entry["agg"] = {k: agg(k) for k in (
            "wall_ms", "uffd_faults", "uffd_bytes", "uffd_frac_of_guest",
            "uffd_ioctl_us_p50", "uffd_ioctl_us_mean", "uffd_ioctl_us_total",
            "uffd_interfault_us_p50", "uffd_interfault_us_mean", "uffd_wall_us_per_fault",
            "serve_cpu_ms", "serve_cpu_us_per_fault", "fault_span_ms",
            "pagemap_resident_4k", "pagemap_resident_bytes", "pagemap_frac_of_guest",
            "fc_minflt_total", "fc_majflt_total", "fc_cpu_ms",
            "kvm_guest_faults", "kvm_unique_4k", "kvm_unique_granule", "kvm_frac_of_guest",
        )}
        summary[f"{cell}|{page}"] = entry

    (out / "summary.json").write_text(json.dumps(summary, indent=2, default=str))

    # ---- text report
    print(f"\n{'cell':<18} {'page':<14} {'n':>3} {'wall ms':>9} {'uffd flt':>9} "
          f"{'kvm flt':>9} {'pm 4k pg':>9} {'MiB in':>8} {'%RAM':>6} {'us/flt':>7} {'srv us/f':>9}")
    print("-" * 120)
    for k, e in summary.items():
        a = e["agg"]

        def g(name, f="{:.0f}"):
            v = a.get(name)
            return f.format(v["mean"]) if v else "-"

        pm = a.get("pagemap_resident_bytes")
        mib = f"{pm['mean']/1024/1024:.0f}" if pm else "-"
        fr = a.get("pagemap_frac_of_guest")
        frs = f"{fr['mean']*100:.1f}" if fr else "-"
        print(f"{e['cell']:<18} {e['page']:<14} {e['n']:>3} {g('wall_ms'):>9} {g('uffd_faults'):>9} "
              f"{g('kvm_guest_faults'):>9} {g('pagemap_resident_4k'):>9} {mib:>8} {frs:>6} "
              f"{g('uffd_ioctl_us_p50','{:.1f}'):>7} {g('serve_cpu_us_per_fault','{:.1f}'):>9}")

    print("\n=== stability across clones (same snapshot, repeated restores) ===")
    for k, e in summary.items():
        for label, st in (("uffd-offsets", e["stability_uffd_offsets"]),
                          ("kvm-ipa", e["stability_kvm_ipa"]),
                          ("pagemap", e["stability_pagemap"])):
            if st:
                print(f"{k:<28} {label:<13} n={st['n_runs']} jaccard={st['jaccard_mean']:.4f} "
                      f"[{st['jaccard_min']:.4f}-{st['jaccard_max']:.4f}] "
                      f"core={st['core_size']} union={st['union_size']} "
                      f"loo_cov={st['loo_coverage_mean']:.4f} loo_waste={st['loo_waste_mean']:.4f}")

    print("\n=== locality ===")
    for k, e in summary.items():
        for r in e["requests"][:1]:
            for label in ("locality", "kvm_locality"):
                L = r.get(label)
                if L:
                    print(f"{k:<28} {label:<13} uniq={L['unique_granules']} runs={L['runs']} "
                          f"mean_run={L['run_len_mean']:.1f} p90={L['run_len_p90']} max={L['run_len_max']} "
                          f"ge4={L['frac_in_runs_ge4']:.3f} ge16={L['frac_in_runs_ge16']:.3f} "
                          f"ge64={L['frac_in_runs_ge64']:.3f} seq+1={L['order_frac_next_is_plus1']:.3f} "
                          f"near16={L['order_frac_next_within_16']:.3f}")
    print(f"\nsummary.json -> {out / 'summary.json'}")


if __name__ == "__main__":
    main()
