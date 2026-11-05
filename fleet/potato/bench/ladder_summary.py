#!/usr/bin/env python3
"""Per size, from potatobench.py's ladder.jsonl: the record (the shortest hold of a lap that
completed), where and why the ball dropped, and the wakes and sleeps in the round.

  ladder_summary.py OUT/ladder.jsonl      one JSON line per size
"""
import json, sys, statistics as st
def pct(xs, p):
    xs = sorted(xs)
    return round(xs[min(len(xs)-1, int(len(xs)*p/100))], 1) if xs else None
rows = [json.loads(l) for l in open(sys.argv[1])]
out = [r for r in rows if r.get("type") == "size_done"]
for i, d in enumerate(out, start=1):
    caught = [r for r in rows if r.get("type") == "caught" and r.get("round") == i]
    woke = [r for r in rows if r.get("type") == "woke" and r.get("round") == i]
    slept = [r for r in rows if r.get("type") == "slept" and r.get("round") == i]
    n = d["size"]
    # completed laps: a catch at seq % n == 0 (seq > 0) closes the lap before it
    laps = sorted({c["seq"] // n for c in caught})
    holds = {}
    for c in caught:
        holds.setdefault(c["seq"] // n, c["hold_ms"])
    completed = [holds[l - 1] for l in laps if l >= 1 and (l - 1) in holds]
    # wakes after the round's opening wake of every player
    later = woke[n:] if len(woke) > n else []
    print(json.dumps({
        "size": n, "start_hold_ms": d.get("start_hold_ms"), "record_hold_ms": min(completed) if completed else None,
        "dropped_at_hold_ms": d.get("hold_ms"), "lap": d.get("lap"), "catches": d.get("catches"),
        "reason": d.get("reason"), "catcher_phase": d.get("catcher_phase"), "error": (d.get("error") or "")[:60],
        "listening_after_drop_ms": d.get("catcher_listening_after_drop_ms"),
        "wake_ms_after_opening": {"n": len(later), "p50": pct([w["restore_to_listen_ms"] for w in later], 50), "p99": pct([w["restore_to_listen_ms"] for w in later], 99), "max": pct([w["restore_to_listen_ms"] for w in later], 100)},
        "sleep_ms": {"n": len(slept), "p50": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 50), "max": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 100)},
        "awake_max": max((c.get("awake", 0) for c in caught), default=None),
    }))
