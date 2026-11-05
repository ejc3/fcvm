#!/usr/bin/env python3
"""Per size, from potatobench.py's ladder.jsonl: the record (the shortest hold of a lap that
completed), where and why the ball dropped, and the wakes and sleeps in the round.

  ladder_summary.py OUT/ladder.jsonl [SHRINK_PERMILLE MIN_HOLD_MS]   one JSON line per size

The record is the hold of the lap before the one the ball dropped in, from the round's
start and the shrink (potatobench.hold_at): a catch's logged hold can be a lap behind, since
the throw that set it is reported on another connection.
"""
import json, os, sys, statistics as st

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from potatobench import hold_at  # noqa: E402

SHRINK = int(sys.argv[2]) if len(sys.argv) > 2 else 100
MIN_HOLD = int(sys.argv[3]) if len(sys.argv) > 3 else 2
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
    # the ball dropped in lap `lap`, so every lap before it completed; the last one's hold
    # follows from the start and the shrink
    lap = d.get("lap")
    start = d.get("start_hold_ms")
    record = hold_at(start, SHRINK, MIN_HOLD, 1, lap - 1) if lap and start else None
    # wakes after the round's opening wake of every player
    later = woke[n:] if len(woke) > n else []
    print(json.dumps({
        "size": n, "start_hold_ms": d.get("start_hold_ms"), "record_hold_ms": record,
        "dropped_in_lap_hold_ms": hold_at(start, SHRINK, MIN_HOLD, 1, lap) if lap is not None and start else None,
        "lap": d.get("lap"), "catches": d.get("catches"),
        "reason": d.get("reason"), "catcher_phase": d.get("catcher_phase"), "error": (d.get("error") or "")[:60],
        "listening_after_drop_ms": d.get("catcher_listening_after_drop_ms"),
        "wake_ms_after_opening": {"n": len(later), "p50": pct([w["restore_to_listen_ms"] for w in later], 50), "p99": pct([w["restore_to_listen_ms"] for w in later], 99), "max": pct([w["restore_to_listen_ms"] for w in later], 100)},
        "sleep_ms": {"n": len(slept), "p50": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 50), "max": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 100)},
        "awake_max": max((c.get("awake", 0) for c in caught), default=None),
    }))
