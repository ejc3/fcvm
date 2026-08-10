#!/usr/bin/env python3
"""cpuanalyze.py — turn cpuprof.py raw records into a CPU attribution table.

The rule this file exists to enforce: the columns must SUM to the total. Every
table prints an `unattributed` column (cgroup total minus the per-thread sum) and
a `machine_residual` column (whole-machine busy minus everything we can name).
Neither is allowed to be silently dropped.
"""

import json
import os
import statistics as st
import sys
from collections import defaultdict

# Thread-role classification. Firecracker names its threads, so the guest/host
# split does not need guessing:
#   "fc_vcpu N"     -> a vCPU thread; its guest_time is literal guest execution
#   "fc_api"        -> the HTTP API thread (restore/resume calls)
#   "firecracker"   -> the main thread == the device event loop (virtio, vsock)
FC_VCPU = "fc_vcpu"


def pct(vals, p):
    if not vals:
        return None
    s = sorted(vals)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def med(v):
    return st.median(v) if v else None


def load(d, kind="meas"):
    cells = defaultdict(list)
    for fn in sorted(os.listdir(d)):
        if not fn.startswith(kind + "-") or not fn.endswith(".json"):
            continue
        body = fn[len(kind) + 1:-5]
        # <cell>-<page>-<i>; cell may contain '-' (uffd-copy), page never does
        parts = body.split("-")
        i = parts[-1]
        page = parts[-2]
        cell = "-".join(parts[:-2])
        try:
            with open(os.path.join(d, fn)) as f:
                r = json.load(f)
        except Exception as e:
            print("skip %s: %s" % (fn, e), file=sys.stderr)
            continue
        if r.get("rc") not in (0, None):
            print("skip %s: rc=%s" % (fn, r.get("rc")), file=sys.stderr)
            continue
        r["_i"] = i
        cells[(cell, page)].append(r)
    return cells


def breakdown(r):
    """One record -> named CPU buckets in seconds. Buckets are disjoint and sum
    to `attributed`; `unattributed` closes the gap to the cgroup total."""
    b = defaultdict(float)
    guest = 0.0
    for pid, p in r.get("processes", {}).items():
        comm = p.get("comm", "?")
        for tid, t in p["threads"].items():
            u, s, g = t["user_s"], t["system_s"], t["guest_s"]
            tc = t["comm"]
            if comm.startswith("firecracker"):
                # Host-side minor faults taken by the VMM process. On the
                # file-backed arm this IS the count of guest pages materialised
                # out of the page cache, i.e. the working set the UFFD arm has to
                # serve one ioctl at a time.
                b["fc_minflt"] += t.get("minflt", 0)
                b["fc_majflt"] += t.get("majflt", 0)
            if comm.startswith("firecracker") or tc.startswith(FC_VCPU) or tc == "fc_api":
                if tc.startswith(FC_VCPU):
                    b["fc_vcpu_guest"] += g
                    b["fc_vcpu_user"] += max(0.0, u - g)
                    b["fc_vcpu_sys"] += s
                    guest += g
                elif tc == "fc_api":
                    b["fc_api"] += u + s
                else:
                    b["fc_evloop_user"] += max(0.0, u - g)
                    b["fc_evloop_sys"] += s
                    guest += g
            elif comm.startswith("fcvm"):
                b["fcvm_user"] += u
                b["fcvm_sys"] += s
            elif comm.startswith("pasta"):
                b["pasta"] += u + s
            elif comm in ("unshare", "sleep"):
                b["nsholder"] += u + s
            else:
                b["harness_wrap"] += u + s   # sh, timeout, env, nsenter, ip, ...
    b["guest_total"] = guest
    b["attributed"] = sum(v for k, v in b.items()
                          if k not in ("guest_total", "fc_minflt", "fc_majflt"))
    cg = r.get("cgroup_cpu_s") or {}
    b["cgroup_total"] = cg.get("usage") or 0.0
    b["cgroup_user"] = cg.get("user") or 0.0
    b["cgroup_sys"] = cg.get("system") or 0.0
    b["unattributed"] = b["cgroup_total"] - b["attributed"]

    sv = r.get("serve") or {}
    b["serve"] = (sv.get("delta_user_s") or 0.0) + (sv.get("delta_system_s") or 0.0)
    b["serve_user"] = sv.get("delta_user_s") or 0.0
    b["serve_sys"] = sv.get("delta_system_s") or 0.0

    b["request_total"] = b["cgroup_total"] + b["serve"]

    mach = r.get("machine_cpu_s") or {}
    b["machine_busy"] = mach.get("busy") or 0.0
    b["machine_softirq"] = mach.get("softirq") or 0.0
    b["machine_irq"] = mach.get("irq") or 0.0
    b["profiler"] = r.get("profiler_cpu_s") or 0.0
    b["machine_residual"] = (b["machine_busy"] - b["cgroup_total"]
                             - b["serve"] - b["profiler"])

    ar = r.get("cgroup_cpu_after_release_s") or {}
    b["teardown_cpu"] = ar.get("usage") or 0.0
    b["teardown_sys"] = ar.get("system") or 0.0
    b["wall_total"] = r.get("wall_total_s") or 0.0
    b["wall_gone"] = r.get("wall_to_gone_s") or 0.0
    b["wall_teardown"] = r.get("wall_teardown_after_caller_s") or 0.0
    b["faults"] = r.get("uffd_fault_count") or 0
    return b


ORDER = [
    ("fc_vcpu_guest", "firecracker vCPU: GUEST"),
    ("fc_vcpu_user", "firecracker vCPU: user (VMM)"),
    ("fc_vcpu_sys", "firecracker vCPU: system (KVM/faults)"),
    ("fc_evloop_user", "firecracker evloop+aux: user"),
    ("fc_evloop_sys", "firecracker evloop+aux: system"),
    ("fc_api", "firecracker API thread"),
    ("fcvm_user", "fcvm supervisor: user"),
    ("fcvm_sys", "fcvm supervisor: system"),
    ("pasta", "pasta"),
    ("nsholder", "unshare namespace holder"),
    ("harness_wrap", "in-cgroup misc (sh/timeout/env/nsenter/ip)"),
    ("unattributed", "UNATTRIBUTED (in cgroup, missed by sampler)"),
    ("cgroup_total", "= cgroup cpu.stat total"),
    ("serve", "snapshot-serve process (outside cgroup)"),
    ("request_total", "= TOTAL CPU per request"),
    ("machine_residual", "machine residual (kernel threads/IRQ/other)"),
]


def fmt_ms(v):
    return "-" if v is None else "%.1f" % (v * 1000.0)


def main():
    d = sys.argv[1]
    raw = os.path.join(d, "raw")
    cells = load(raw)
    out = {}
    lines = []

    keys = sorted(cells.keys(), key=lambda k: (k[1], k[0]))
    for k in keys:
        recs = cells[k]
        bs = [breakdown(r) for r in recs]
        agg = {}
        for name in (set().union(*[set(b) for b in bs])):
            vals = [b[name] for b in bs]
            agg[name] = {"med": med(vals), "p10": pct(vals, 10), "p90": pct(vals, 90),
                         "n": len(vals)}
        out["%s/%s" % k] = agg

    # ---- main table ----
    hdr = ["component"] + ["%s / %s" % (c, p) for (c, p) in keys]
    rows = [hdr]
    for key, label in ORDER:
        row = [label]
        for k in keys:
            a = out["%s/%s" % k].get(key)
            row.append(fmt_ms(a["med"]) if a else "-")
        rows.append(row)

    w = [max(len(r[i]) for r in rows) for i in range(len(hdr))]
    lines.append("PER-REQUEST CPU, milliseconds of CPU time (median, n=%d/cell)"
                 % len(cells[keys[0]]))
    for ri, r in enumerate(rows):
        lines.append("  " + "  ".join(r[i].ljust(w[i]) if i == 0 else r[i].rjust(w[i])
                                      for i in range(len(r))))
        if ri == 0:
            lines.append("  " + "  ".join("-" * w[i] for i in range(len(hdr))))

    # ---- derived ----
    lines.append("")
    lines.append("DERIVED")
    for k in keys:
        a = out["%s/%s" % k]
        tot = a["request_total"]["med"]
        guest = a["guest_total"]["med"]
        cg = a["cgroup_total"]["med"]
        sysc = (a["cgroup_sys"]["med"] or 0) + (a["serve_sys"]["med"] or 0)
        lines.append("  %-22s total=%7.1f ms  guest=%6.1f ms (%4.1f%%)  "
                     "overhead=%7.1f ms (%.2fx guest)  system=%.0f%%  "
                     "wall=%.0f ms  ceiling@64c=%.1f rps"
                     % ("%s/%s" % k, tot * 1000, guest * 1000,
                        100.0 * guest / tot if tot else 0,
                        (tot - guest) * 1000,
                        (tot - guest) / guest if guest else float("nan"),
                        100.0 * sysc / tot if tot else 0,
                        a["wall_total"]["med"] * 1000,
                        64.0 / tot if tot else 0))

    # ---- uffd ----
    lines.append("")
    lines.append("FAULT ACCOUNTING  (uffd faults = served by the handler;")
    lines.append("                   fc_minflt  = host minor faults taken by the Firecracker process)")
    for k in keys:
        a = out["%s/%s" % k]
        f = a["faults"]["med"] or 0
        mn = a["fc_minflt"]["med"] or 0
        sv = a["serve"]["med"] or 0.0
        cpf = ("%6.2f us" % (sv / f * 1e6)) if f else "     n/a"
        lines.append("  %-22s uffd_faults=%8.0f  fc_minflt=%9.0f  "
                     "serve_cpu=%7.2f ms  cpu/fault=%s  ws=%6.1f MiB"
                     % ("%s/%s" % k, f, mn, sv * 1000, cpf,
                        (f or mn) * 4096 / 1048576.0))

    # ---- teardown ----
    lines.append("")
    lines.append("TEARDOWN")
    for k in keys:
        a = out["%s/%s" % k]
        lines.append("  %-22s caller_blocking_wall=%6.1f ms  to_truly_gone=%6.1f ms  "
                     "after_release_wall=%5.1f ms  after_release_CPU=%6.1f ms (sys %.1f)"
                     % ("%s/%s" % k, a["wall_total"]["med"] * 1000,
                        a["wall_gone"]["med"] * 1000,
                        a["wall_teardown"]["med"] * 1000,
                        a["teardown_cpu"]["med"] * 1000,
                        a["teardown_sys"]["med"] * 1000))

    txt = "\n".join(lines)
    print(txt)
    with open(os.path.join(d, "cpu-table.txt"), "w") as f:
        f.write(txt + "\n")
    with open(os.path.join(d, "cpu-summary.json"), "w") as f:
        json.dump(out, f, indent=1)


if __name__ == "__main__":
    main()
