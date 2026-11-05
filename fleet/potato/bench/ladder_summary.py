#!/usr/bin/env python3
"""Per size, from potatobench.py's ladder.jsonl: the record (the shortest hold of a lap that
completed), where and why the ball dropped, and the wakes and sleeps in the round.

  ladder_summary.py OUT/ladder.jsonl [SHRINK_PERMILLE MIN_HOLD_MS]   one JSON line per size

The record is the hold of the last lap that completed, from the round's start and the shrink
(potatobench.hold_at): a catch's logged hold can be a lap behind, since the throw that set it
is reported on another connection. A drop names the lap it happened in; a round that ended
otherwise (a timeout, a proof mismatch, a failed wake) is in the lap after its last verified
catch.
"""
import json, os, sys, statistics as st

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from potatobench import hold_at  # noqa: E402


def ended_in_lap(d, caught):
    """The lap the round ended in: the drop's own, else the lap after the catches verified
    without a gap from seq 0 (as the coordinator counts laps). `caught` holds only the catches
    logged before the round's `size_done`, not those of its cleanup."""
    if d.get("lap") is not None:
        return d["lap"]
    ok = {c["seq"]: c.get("proof_ok", True) for c in caught}
    through = 0
    while ok.get(through):
        through += 1
    return through // d["size"] if through else None


def record_hold(d, caught, shrink, min_hold):
    """The hold of the last lap that completed before the round ended, or None."""
    lap, start = ended_in_lap(d, caught), d.get("start_hold_ms")
    return hold_at(start, shrink, min_hold, 1, lap - 1) if lap and start else None


def pct(xs, p):
    xs = sorted(xs)
    return round(xs[min(len(xs)-1, int(len(xs)*p/100))], 1) if xs else None


def main():
    shrink = int(sys.argv[2]) if len(sys.argv) > 2 else 100
    min_hold = int(sys.argv[3]) if len(sys.argv) > 3 else 2
    rows = [json.loads(l) for l in open(sys.argv[1])]
    out = [(j, r) for j, r in enumerate(rows) if r.get("type") == "size_done"]
    for i, (j, d) in enumerate(out, start=1):
        caught = [r for r in rows if r.get("type") == "caught" and r.get("round") == i]
        before = [r for r in rows[:j] if r.get("type") == "caught" and r.get("round") == i]
        woke = [r for r in rows if r.get("type") == "woke" and r.get("round") == i]
        slept = [r for r in rows if r.get("type") == "slept" and r.get("round") == i]
        n = d["size"]
        lap = ended_in_lap(d, before)
        start = d.get("start_hold_ms")
        record = record_hold(d, before, shrink, min_hold)
        # wakes after the round's opening wake of every player
        later = woke[n:] if len(woke) > n else []
        print(json.dumps({
            "size": n, "start_hold_ms": d.get("start_hold_ms"), "record_hold_ms": record,
            "dropped_in_lap_hold_ms": hold_at(start, shrink, min_hold, 1, lap) if lap is not None and start else None,
            "lap": lap, "catches": d.get("catches"),
            "reason": d.get("reason"), "catcher_phase": d.get("catcher_phase"), "error": (d.get("error") or "")[:60],
            "listening_after_drop_ms": d.get("catcher_listening_after_drop_ms"),
            "wake_ms_after_opening": {"n": len(later), "p50": pct([w["restore_to_listen_ms"] for w in later], 50), "p99": pct([w["restore_to_listen_ms"] for w in later], 99), "max": pct([w["restore_to_listen_ms"] for w in later], 100)},
            "sleep_ms": {"n": len(slept), "p50": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 50), "max": pct([s["snapshot_ms"] + s["teardown_ms"] for s in slept], 100)},
            "awake_max": max((c.get("awake", 0) for c in caught), default=None),
        }))


if __name__ == "__main__":
    main()
