#!/usr/bin/env python3
"""The potato check on a host: a coordinator with a local agent running players, watched
through /live (docs/fleet/ARCHITECTURE.md, "Potato mode").

  potatorun.py ws://127.0.0.1:<port>/live OUT [SECONDS]

Records the snapshot's `potato` section once a second to OUT/potato.jsonl, presses "faster"
once in the first round with catches, and exits 0 only if within SECONDS (default 600):

- rounds of at least two ladder sizes ended, each with what broke it;
- no round ended on a proof mismatch (every wake kept its colour, counter and chain);
- every player's counter only went up between snapshots;
- the faster intent committed.
"""
import json, os, sys, time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../tools"))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../renderer/tools"))
import fleetlive  # noqa: E402


def main():
    url, out = sys.argv[1], sys.argv[2]
    seconds = int(sys.argv[3]) if len(sys.argv) > 3 else 600
    os.makedirs(out, exist_ok=True)
    rec = open(os.path.join(out, "potato.jsonl"), "w")
    fleetlive.connect_to(url)
    live = fleetlive.Live()
    end = time.time() + seconds
    counters, failures, faster = {}, [], None
    ended = {}
    version = -1
    while time.time() < end:
        try:
            state = live.until(lambda s: s.get("potato") is not None, 5)
        except TimeoutError:
            continue
        v = live.snap["version"]
        if v == version:
            time.sleep(1)
            continue
        version = v
        p = state["potato"]
        rec.write(json.dumps({"t": time.time(), "version": v, "potato": p}) + "\n")
        rec.flush()
        for pl in p["players"]:
            last = counters.get(pl["player"])
            if last is not None and pl["counter"] < last and pl["counter"] != 0:
                failures.append(f"player {pl['player']}'s counter went from {last} to {pl['counter']}")
            counters[pl["player"]] = pl["counter"]
        for rung in p["ladder"]:
            b = rung.get("broke")
            if b and b["round"] not in ended:
                ended[b["round"]] = b
                print(f"round {b['round']} size {b['size']}: {b['reason']} at lap {b['lap']}, hold {b['hold_ms']} ms; "
                      f"player {b['player']} {b['stage']} {b['stage_ms']} ms; record {rung.get('record_hold_ms')}", flush=True)
                if b["reason"] == "proof":
                    failures.append(f"round {b['round']} ended on a proof mismatch: {b['detail']}")
        r = p.get("round")
        if faster is None and r and r.get("seq") is not None and r["seq"] >= 1:
            faster = live.intent("potato_faster", {})
            print("faster:", faster.get("outcome"), faster.get("reason"), flush=True)
        if len({b["size"] for b in ended.values()}) >= 2 and faster is not None:
            break
        time.sleep(1)
    live.close()
    sizes = {b["size"] for b in ended.values()}
    if len(sizes) < 2:
        failures.append(f"rounds of {sorted(sizes)} ended; two sizes were expected in {seconds} s")
    if faster is None or faster.get("outcome") != "committed":
        failures.append(f"faster: {faster}")
    for f in failures:
        print("POTATO CHECK FAILED:", f, flush=True)
    print("POTATO CHECK", "FAILED" if failures else "PASSED", flush=True)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
