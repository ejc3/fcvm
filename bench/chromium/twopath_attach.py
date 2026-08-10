#!/usr/bin/env python3
"""Attach UFFD handler traces to their requests, after the fact.

WHY THIS IS A SEPARATE STEP. `FaultTrace` flushes in `Drop`, and the per-VM handler
tasks live in the serve's `JoinSet` — so the `.faults` files do not appear when a clone
disconnects, they appear when the SERVE process shuts down. Polling for a new file after
each request therefore finds nothing and silently records `uffd_trace: null`.

The files are named `<serve_pid>-vm-<N>.faults`, where N is the serve's own accept
counter (`src/uffd/server.rs`: `let vm_id = format!("vm-{}", next_vm_id); next_vm_id += 1`).
Requests in a pass are strictly serialised, so N is exactly the order in which this pass's
path-B requests ran — INCLUDING the warmups, which also connect and consume an index.

The match is verified, not assumed: the file count must equal the path-B request count for
that pass, or nothing is attached and the mismatch is reported.
"""

from __future__ import annotations

import gzip
import json
import re
import struct
import sys
from pathlib import Path


def parse_fault_trace(path: Path):
    buf = path.read_bytes()
    n = len(buf) // 24
    if n == 0:
        return {"faults": 0}
    recs = struct.unpack_from(f"<{n * 3}Q", buf)
    offs, before, after = recs[0::3], recs[1::3], recs[2::3]
    svc = sorted(after[i] - before[i] for i in range(n))
    gaps = sorted(before[i + 1] - after[i] for i in range(n - 1)
                  if before[i + 1] >= after[i])

    def q(a, p):
        return a[min(len(a) - 1, int(p * (len(a) - 1)))] if a else 0

    return {
        "faults": n,
        "unique_offsets": len(set(offs)),
        "span_ns": after[-1] - before[0],
        "svc_ns_sum": sum(svc), "svc_ns_p50": q(svc, 0.5),
        "svc_ns_p90": q(svc, 0.9), "svc_ns_max": svc[-1],
        "gap_ns_sum": sum(gaps), "gap_ns_p50": q(gaps, 0.5), "gap_ns_p90": q(gaps, 0.9),
        "min_offset": min(offs), "max_offset": max(offs),
    }


# ftrace TASK-pid field. The comm can contain spaces ("fc_vcpu 0-12345"), so the comm is
# matched non-greedily up to the LAST '-' before the pid.
TRACE_LINE = re.compile(
    r"^\s*(.+?)-(\d+)\s+\[\d+\]\s+\S+\s+([\d.]+):\s+kvm_guest_fault:\s+ipa\s+(0x[0-9a-f]+)")


def reparse_kvm(path: Path, mine: set[int]):
    """Re-count kvm_guest_fault events, keeping ONLY this request's threads.

    `kvm:kvm_guest_fault` is machine-wide: every VM on the box lands in the same ring
    buffer. On a shared box a neighbouring agent's firecracker doubles the count and the
    comm is no help (its vCPU threads are also called "fc_vcpu N"). The only safe filter
    is the set of thread ids belonging to THIS request's firecracker, which the sampler
    recorded independently.
    """
    n = mine_n = 0
    ipas = set()
    by_comm = {}
    first = last = None
    foreign_pids = set()
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt", errors="replace") as f:
        for line in f:
            m = TRACE_LINE.match(line)
            if not m:
                continue
            comm, pid, ts, ipa = m.groups()
            n += 1
            if int(pid) not in mine:
                foreign_pids.add(int(pid))
                continue
            mine_n += 1
            ipas.add(int(ipa, 16) & ~0xFFF)
            by_comm[comm] = by_comm.get(comm, 0) + 1
            ts = float(ts)
            first = ts if first is None else first
            last = ts
    return {
        "events": mine_n,
        "unique_pages": len(ipas),
        "by_comm": by_comm,
        "events_all_vms_on_box": n,
        "events_foreign": n - mine_n,
        "foreign_pid_count": len(foreign_pids),
        "span_ms": (last - first) * 1000.0 if first is not None else None,
    }


def main():
    out = Path(sys.argv[1])
    recs = [json.loads(l) for l in open(out / "requests.jsonl") if l.strip()]

    # ---- kvm ftrace: re-count with a per-request thread filter ----
    fixed = 0
    for r in recs:
        base = out / "traces" / "kvm" / r["name"]
        tp = next((q for q in (base.with_suffix(".trace"),
                               base.with_suffix(".trace.gz")) if q.exists()), None)
        if tp is None:
            continue
        mine = {t["tid"] for t in r.get("tasks", [])
                if t["comm"].startswith(("firecracker", "fc_"))}
        if not mine:
            continue
        r["kvm_ftrace_raw"] = r.get("kvm_ftrace")
        r["kvm_ftrace"] = reparse_kvm(tp, mine)
        fixed += 1
    if fixed:
        tot = sum(r["kvm_ftrace"].get("events_foreign", 0) for r in recs
                  if isinstance(r.get("kvm_ftrace"), dict))
        print(f"[attach] re-counted {fixed} kvm traces with a per-request thread filter; "
              f"discarded {tot:,} events belonging to other VMs on the box")

    passes = sorted({r["pass"] for r in recs})
    attached = 0
    for pas in passes:
        tdir = out / "traces" / "uffd" / pas
        if not tdir.exists():
            continue
        files = {}
        for p in tdir.glob("*.faults"):
            m = re.match(r"^(\d+)-vm-(\d+)\.faults$", p.name)
            if m:
                files[(int(m.group(1)), int(m.group(2)))] = p
        if not files:
            print(f"[attach] {pas}: no trace files", file=sys.stderr)
            continue
        # group by serve pid (one serve per pass, but be explicit)
        for spid in sorted({k[0] for k in files}):
            ordered = [files[k] for k in sorted(files) if k[0] == spid]
            reqs = [r for r in recs
                    if r["pass"] == pas and r["path"] == "B-uffd"
                    and r.get("serve_pid") == spid]
            reqs.sort(key=lambda r: r["t_spawn"])
            if len(ordered) != len(reqs):
                print(f"[attach] {pas} serve={spid}: MISMATCH "
                      f"{len(ordered)} traces vs {len(reqs)} path-B requests — "
                      "not attaching (order cannot be trusted)", file=sys.stderr)
                continue
            for r, p in zip(reqs, ordered):
                r["uffd_trace"] = parse_fault_trace(p)
                r["uffd_trace_path"] = str(p)
                attached += 1
            print(f"[attach] {pas} serve={spid}: attached {len(ordered)} traces "
                  f"(faults {min(r['uffd_trace']['faults'] for r in reqs):,}–"
                  f"{max(r['uffd_trace']['faults'] for r in reqs):,})")

    with open(out / "requests.jsonl", "w") as f:
        for r in recs:
            f.write(json.dumps(r) + "\n")
    print(f"[attach] rewrote {out/'requests.jsonl'} ({attached} traces attached)")


if __name__ == "__main__":
    main()
