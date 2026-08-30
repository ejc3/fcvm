#!/usr/bin/env python3
"""Set the VM arm and the host-container arms side by side, per URL and overall.

Reads only records: the campaign's reqbench.jsonl + analysis.json (the VM arm,
already through its publication gate) and hostcdp.jsonl + run.json (the host
arms). Prints two tables and the ratios, and writes comparison.json.

Two quantities are compared, never mixed:
  caller-visible   VM blocking_ms (spawn -> image in hand) against host wall_ms
                   (the driver invocation). The host side carries a python
                   interpreter start per rep that the VM side does not, because
                   reqbench imports cdpdrive and calls drive() in-process.
  driver total     cdpdrive's own total_ms on both sides: the same code, timed
                   the same way, with no interpreter start and no clone
                   lifecycle in either.
"""
import argparse, json, os, statistics, sys


def pct(v, p):
    """p50 is statistics.median, the convention reqanalyze uses for every
    published median (median_ci), so a ratio taken here is between two numbers
    computed the same way. Other percentiles are nearest-rank."""
    v = sorted(v)
    if not v:
        return None
    if p == 50:
        return statistics.median(v)
    return v[max(0, -(-p * len(v) // 100) - 1)]


def load_vm(run_dir):
    recs = []
    with open(os.path.join(run_dir, "reqbench.jsonl")) as f:
        for line in f:
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("arm") and not r.get("warmup") and r.get("ok") is not False:
                recs.append(r)
    return recs


def driver_total(rec):
    st = (rec.get("render") or {}).get("stages") or rec.get("stages") or {}
    return st.get("total_ms")


def nav_load(rec):
    nav = (rec.get("render") or {}).get("nav") or rec.get("nav") or {}
    return nav.get("load_ms")


def load_host(d):
    rows = []
    with open(os.path.join(d, "hostcdp.jsonl")) as f:
        for line in f:
            r = json.loads(line)
            if r.get("warmup") or not r.get("ok"):
                continue
            drv = {}
            try:
                drv = json.loads(r["driver"])
            except (KeyError, ValueError):
                pass
            rows.append({"url": r.get("url"), "wall_ms": r["wall_ms"],
                         "total_ms": (drv.get("stages") or {}).get("total_ms"),
                         "load_ms": (drv.get("nav") or {}).get("load_ms")})
    return rows


def summarize(rows, key):
    vals = [r[key] for r in rows if r.get(key) is not None]
    return {"n": len(vals), "p50": round(pct(vals, 50), 1) if vals else None,
            "p95": round(pct(vals, 95), 1) if vals else None,
            "mean": round(statistics.mean(vals), 1) if vals else None}


def per_url(rows, key):
    out = {}
    for r in rows:
        if r.get(key) is None:
            continue
        out.setdefault(r["url"], []).append(r[key])
    return {u: {"n": len(v), "p50": round(pct(v, 50), 1)} for u, v in out.items()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--vm-run", required=True)
    ap.add_argument("--host", action="append", default=[], metavar="LABEL=DIR")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    analysis = json.load(open(os.path.join(a.vm_run, "analysis.json")))
    if not analysis.get("publishable") or not analysis.get("gate", {}).get("passed"):
        sys.exit("REFUSING: the VM run did not pass its publication gate; its numbers are not quotable")
    vm_all = load_vm(a.vm_run)
    vm = [dict(r, total_ms=driver_total(r), load_ms=nav_load(r)) for r in vm_all if r["arm"] == "cdp"]
    noop = [r for r in vm_all if r["arm"] == "noop"]

    out = {"vm_run": os.path.abspath(a.vm_run), "run_id": analysis.get("run_id"),
           "cell": {k: analysis["cell"][k] for k in
                    ("cpu", "memory_mib", "backend", "uffd_mode", "snapshot", "image_id",
                     "source_revision", "fcvm_sha256", "runtime_bundle_sha256",
                     "host_kernel_release", "host_machine")},
           "vm": {"arm": "cdp",
                  "blocking_ms": summarize(vm, "blocking_ms"),
                  "wall_ms": summarize(vm, "wall_ms"),
                  "driver_total_ms": summarize(vm, "total_ms"),
                  "load_event_ms": summarize(vm, "load_ms"),
                  "per_url_blocking_p50": per_url(vm, "blocking_ms"),
                  "per_url_load_p50": per_url(vm, "load_ms")},
           "vm_noop": {"blocking_ms": summarize(noop, "blocking_ms"),
                       "wall_ms": summarize(noop, "wall_ms")},
           "hosts": {}, "ratios": {}}

    for spec in a.host:
        label, _, d = spec.partition("=")
        meta = json.load(open(os.path.join(d, "run.json")))
        rows = load_host(d)
        out["hosts"][label] = {
            "dir": os.path.abspath(d), "cpus": meta.get("cpus"),
            "image_id": meta.get("image_id"), "resolve_all_to": meta.get("resolve_all_to"),
            "reps": meta.get("reps"), "warmup": meta.get("warmup"),
            "wall_ms": summarize(rows, "wall_ms"),
            "driver_total_ms": summarize(rows, "total_ms"),
            "load_event_ms": summarize(rows, "load_ms"),
            "per_url_wall_p50": per_url(rows, "wall_ms"),
            "per_url_load_p50": per_url(rows, "load_ms")}
        h = out["hosts"][label]
        out["ratios"][label] = {
            "vm_blocking_over_host_wall": round(out["vm"]["blocking_ms"]["p50"] / h["wall_ms"]["p50"], 2)
            if h["wall_ms"]["p50"] else None,
            "vm_driver_total_over_host_driver_total": round(
                out["vm"]["driver_total_ms"]["p50"] / h["driver_total_ms"]["p50"], 2)
            if h["driver_total_ms"]["p50"] else None,
            "vm_load_event_over_host_load_event": round(
                out["vm"]["load_event_ms"]["p50"] / h["load_event_ms"]["p50"], 2)
            if h["load_event_ms"]["p50"] else None,
        }

    json.dump(out, open(a.out, "w"), indent=1)
    print(json.dumps({k: out[k] for k in ("cell", "vm", "vm_noop", "ratios")}, indent=1)[:6000])
    print("\nper-URL wall/blocking p50 (ms)")
    urls = list(out["vm"]["per_url_blocking_p50"])
    hdr = f"{'url':60s} {'VM blocking':>12s} {'VM load':>9s}"
    for label in out["hosts"]:
        hdr += f" {label + ' wall':>14s} {label + ' load':>13s}"
    print(hdr)
    for u in urls:
        line = f"{u[:60]:60s} {out['vm']['per_url_blocking_p50'][u]['p50']:12.1f} " \
               f"{out['vm']['per_url_load_p50'].get(u, {}).get('p50', float('nan')):9.1f}"
        for label, h in out["hosts"].items():
            line += f" {h['per_url_wall_p50'].get(u, {}).get('p50', float('nan')):14.1f}" \
                    f" {h['per_url_load_p50'].get(u, {}).get('p50', float('nan')):13.1f}"
        print(line)
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    main()
