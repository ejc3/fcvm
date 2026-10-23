"""The grid-wide budget controller (PROTOCOL.md, "Budget"), for tests and as the
reference for the fleet's coordinator.

    python3 tools/budgetctl.py [--period S] [--lead S] [--rounds N] HOST:PORT ...

Every period it reads /health of every tile, takes the smallest budget_fit
(the slowest tile governs), rounds it down to the ladder 250,000 x 2^(n/4),
and posts that budget to every tile with one start time, `lead` seconds past
the newest frame any tile has drawn, so every tile switches at the same slot.
A lower rung is taken at once; a higher one only after two rounds in a row
ask for at least that much. The budget in force is posted every round, since
a new scene clears a tile's schedule. Prints one line per round.
"""
import json
import math
import os
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fleetclient as fc  # noqa: E402


def ladder_floor(fit):
    best = 250000 * 2 ** (-8 / 4)
    for n in range(-8, 41):
        r = 250000 * 2 ** (n / 4)
        if r <= fit * (1 + 1e-9):
            best = r
    return round(best)


def post_budget(host, port, budget, t):
    req = urllib.request.Request(f"http://{host}:{port}/budget", data=json.dumps({"budget": budget, "t": t}).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.loads(r.read())


class Controller:
    def __init__(self):
        self.current = None  # the budget last posted
        self.want_up = None  # a higher rung asked for in the previous round

    def decide(self, fits):
        """The budget to post this round (or None), from every tile's fit."""
        fits = [f for f in fits if f and f > 0]
        if not fits:
            return None
        b = ladder_floor(min(fits))
        if self.current is None or b < self.current:
            self.want_up = None
            self.current = b
            return b
        if b > self.current:
            if self.want_up is not None:
                up = min(b, self.want_up)
                self.want_up = None
                self.current = up
                return up
            self.want_up = b
            return None
        self.want_up = None
        return None


def self_test():
    assert ladder_floor(250000) == 250000 and ladder_floor(297302) == 297302 and ladder_floor(297300) == 250000
    c = Controller()
    assert c.decide([1e6, 5e5]) == ladder_floor(5e5)          # the slowest tile governs
    assert c.decide([2e6, 2e6]) is None                       # up only after two rounds
    assert c.decide([2e6, 1.5e6]) == ladder_floor(1.5e6)      # then to the lower of the two asks
    assert c.decide([3e5, 2e6]) == ladder_floor(3e5)          # down at once
    print("budgetctl self-test ok")


def main():
    args = sys.argv[1:]
    if args == ["--self-test"]:
        return self_test()
    period, lead, rounds = 2.0, 1.0, None
    while args and args[0].startswith("--"):
        k, v = args[0], args[1]
        args = args[2:]
        if k == "--period":
            period = float(v)
        elif k == "--lead":
            lead = float(v)
        elif k == "--rounds":
            rounds = int(v)
    tiles = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in args]
    c = Controller()
    n = 0
    while rounds is None or n < rounds:
        n += 1
        hs = []
        for h, p in tiles:
            try:
                hs.append(fc.health(h, p))
            except OSError:
                hs.append(None)
        fits = [x.get("budget_fit") for x in hs if x and x.get("ready")]
        # every tile must report: a budget chosen without the slowest one
        # could be one it cannot draw
        b = c.decide(fits) if len(fits) == len(tiles) else None
        tnow = max((x["t"] for x in hs if x), default=0)
        line = {"round": n, "fits": fits, "budget_in_use": [x.get("budget") for x in hs if x],
                "render_ms": [x.get("render_ms") for x in hs if x]}
        # the current budget goes out every round (a new scene clears a tile's
        # schedule); a changed one takes effect at the same slot everywhere
        if c.current is not None and len(fits) == len(tiles):
            t0 = math.ceil((tnow + lead) * 30) / 30
            for h, p in tiles:
                post_budget(h, p, c.current, t0)
            line.update(posted=c.current, changed=b is not None, from_t=t0)
        print(json.dumps(line), flush=True)
        time.sleep(period)


if __name__ == "__main__":
    main()
