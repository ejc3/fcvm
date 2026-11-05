#!/usr/bin/env python3
"""Percentiles of every stage in potatobench.py's cycles.jsonl files.

  summarize.py OUT/cycles.jsonl [...]      one table per file, as JSON and as text
"""
import json, sys

STAGES = [
    ("ball_in_ms", "ball in (host to VM, connect + send + ack)"),
    ("throw_send_ms", "throw (VM to host sink, connect + send + ack)"),
    ("snapshot_ms", "sleep: fcvm snapshot create, whole command"),
    ("create_setup_ms", "  setup before the boundary (locks, staging)"),
    ("create_boundary_to_disk_ms", "  close network gate + pause + memory save"),
    ("vmm_pause_ms", "    pause (Firecracker)"),
    ("vmm_save_ms", "    memory save (Firecracker)"),
    ("create_disk_and_resume_ms", "  disk subvolume + resume + reopen gate"),
    ("create_finish_ms", "  finish (punch zero runs, publish)"),
    ("teardown_ms", "sleep: teardown (SIGTERM to exit)"),
    ("restore_to_listen_ms", "wake: fcvm snapshot run to ball port accepting"),
    ("restore_to_hello_ms", "wake: fcvm snapshot run to first guest report"),
    ("wake_network_ms", "  routed network setup"),
    ("wake_disk_ms", "  disk subvolume snapshot"),
    ("wake_vmm_to_loaded_ms", "  Firecracker start + snapshot load"),
    ("wake_guest_to_ack_ms", "  guest restore until fc-agent acknowledges"),
    ("cycle_ms", "whole cycle (ball in to awake again)"),
]


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return round(xs[lo] + (xs[hi] - xs[lo]) * (k - lo), 1)


def summarize(path):
    rows = [json.loads(l) for l in open(path)]
    out = {"file": path, "cycles": len(rows), "proof_ok": sum(r.get("proof_ok") is True for r in rows),
           "kinds": sorted({r.get("create_kind") for r in rows}), "stages": {}}
    for key, _ in STAGES:
        xs = [r[key] for r in rows if isinstance(r.get(key), (int, float))]
        if xs:
            out["stages"][key] = {"n": len(xs), "p50": pct(xs, 50), "p90": pct(xs, 90), "p99": pct(xs, 99),
                                  "max": round(max(xs), 1)}
    w = [r["diff_written_bytes"] / 2**20 for r in rows if r.get("diff_written_bytes")]
    if w:
        out["diff_written_mib"] = {"p50": pct(w, 50), "p99": pct(w, 99), "max": round(max(w), 1)}
    return out


def main():
    for path in sys.argv[1:]:
        s = summarize(path)
        print(json.dumps(s))
        print(f"\n{path}: {s['cycles']} cycles, proof ok {s['proof_ok']}, snapshot kinds {s['kinds']}")
        print(f"{'stage':58s} {'p50':>8s} {'p90':>8s} {'p99':>8s} {'max':>8s}")
        for key, label in STAGES:
            st = s["stages"].get(key)
            if st:
                print(f"{label:58s} {st['p50']:8.1f} {st['p90']:8.1f} {st['p99']:8.1f} {st['max']:8.1f}")
        if "diff_written_mib" in s:
            d = s["diff_written_mib"]
            print(f"{'memory written per sleep, MiB':58s} {d['p50']:8.1f} {'':8s} {d['p99']:8.1f} {d['max']:8.1f}")


if __name__ == "__main__":
    main()
