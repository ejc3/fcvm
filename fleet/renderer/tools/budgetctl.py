"""The grid-wide budget controller (PROTOCOL.md, "Budget"), for tests and as the
reference for the fleet's coordinator.

    python3 tools/budgetctl.py [--period S] [--lead S] [--rounds N] HOST:PORT ...

Every period it reads /health of every tile, takes the smallest budget_fit
(the slowest tile governs), rounds it down to the ladder 250,000 x 2^(n/4)
with the hysteresis in Controller, and posts that budget to every tile with
one start time, `lead` seconds past the newest frame any tile has drawn, so
every tile switches at the same slot. The budget in force is posted every round, since
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
    """Hysteresis on the slowest tile's fit F (smoothed over rounds):
    - down when the budget in force exceeds F by more than 5%, to the rung
      under F; at once when any tile's frame took over 90% of its slot;
    - up when the rung under F / 1.15 is above the budget in force for three
      rounds in a row, to that rung.
    Once up, F must fall about 17% before the budget comes down again, and a
    rung is 19%, so a steady load holds one budget."""

    def __init__(self):
        self.current = None  # the budget in force
        self.f = None        # smoothed slowest fit
        self.up_rounds = 0

    def decide(self, fits, overloaded=False):
        """The new budget this round, or None to keep the one in force."""
        fits = [f for f in fits if f and f > 0]
        if not fits:
            return None
        m = min(fits)
        self.f = m if self.f is None else self.f + (m - self.f) * 0.5
        if self.current is None:
            self.current = ladder_floor(self.f / 1.15)
            return self.current
        if overloaded or self.f < 0.95 * self.current:
            self.up_rounds = 0
            b = ladder_floor(min(self.f, m))
            if overloaded:  # a frame near its slot: at least one rung down
                b = min(b, ladder_floor(self.current * 0.999))
            if b < self.current:
                self.current = b
                return b
            return None
        up = ladder_floor(self.f / 1.15)
        if up > self.current:
            self.up_rounds += 1
            if self.up_rounds >= 3:
                self.up_rounds = 0
                self.current = up
                return up
        else:
            self.up_rounds = 0
        return None


def self_test():
    assert ladder_floor(250000) == 250000 and ladder_floor(297302) == 297302 and ladder_floor(297300) == 250000
    c = Controller()
    assert c.decide([1e6, 5e5]) == ladder_floor(5e5 / 1.15)       # the slowest tile governs, with margin
    cur = c.current
    for f in (5.2e5, 4.6e5, 5.4e5, 4.7e5, 5.1e5):                  # +-8% around one load: no change
        assert c.decide([2e6, f]) is None, f
    assert c.current == cur
    for _ in range(2):
        assert c.decide([2e6, 2e6]) is None                       # up only after three rounds
    assert c.decide([2e6, 2e6]) is not None and c.current > cur
    hi = c.current
    assert c.decide([3e5, 2e6]) is None or c.current < hi         # a drop below 95% brings it down
    c2 = Controller(); c2.decide([1e6]); top = c2.current
    assert c2.decide([1e6], overloaded=True) == ladder_floor(top * 0.999)  # overload: one rung down
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
        over = any((x.get("frames_2s") or {}).get("render_ms_max", 0) > 0.9 * 1000 / 30 for x in hs if x)
        # every tile must report a fit: a budget chosen without the slowest one
        # could be one it cannot draw. A tile whose fit is not known yet (0:
        # its window is younger than 60 s, as after it rejoins) holds the
        # budget in force, as fleet-control's port does.
        known = len(fits) == len(tiles) and all(f and f > 0 for f in fits)
        b = c.decide(fits, over) if known else None
        tnow = max((x["t"] for x in hs if x), default=0)
        line = {"round": n, "fits": fits, "budget_in_use": [x.get("budget") for x in hs if x],
                "render_ms": [x.get("render_ms") for x in hs if x]}
        # the current budget goes out every round (a new scene clears a tile's
        # schedule); a changed one takes effect at the same slot everywhere
        if c.current is not None:
            t0 = math.ceil((tnow + lead) * 30) / 30
            for (h, p), x in zip(tiles, hs):
                if x:  # a tile not answering gets it when it does
                    post_budget(h, p, c.current, t0)
            line.update(posted=c.current, changed=b is not None, from_t=t0)
        print(json.dumps(line), flush=True)
        time.sleep(period)


if __name__ == "__main__":
    main()
