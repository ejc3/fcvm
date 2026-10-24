"""The coordinator's directed camera on a grid of renderers, with the grid
budget controller, to count budget changes.

    python3 tools/pathrun.py OUT_PREFIX SECONDS COLS ROWS H HOST:PORT ...

Builds the camera path fleet-control's director flies with its default seed
(fleet/core/src/director.rs: holds of 7 s at eight named viewpoints joined by
flights, each flight setting the next viewpoint's time of day), posts it to
every tile as one scene (the tile's cell of a COLS x ROWS grid), re-posts the
scene with the next time of day when each flight starts (same epoch, as the
coordinator does), streams every tile at H px, and runs tools/budgetctl.py
(period 2 s) against all tiles. Records in OUT_PREFIX.json the budget each
round posted and every tile's /health every 2 s.
"""
import json
import math
import os
import subprocess
import sys
import threading
import time
import urllib.request

import fleetclient as fc

HOLD = 7.0
VIEWPOINTS = [  # name, pos, look, fov, time of day, via (director.rs)
    ("peak-seam", [-47.0, 2.6, -101.0], [-56.0, 2.75, -114.75], 55.0, 0.40, None),
    ("sunset-ridge", [-36.0, 2.4, -70.0], [-73.5, 2.4, -121.75], 60.0, 0.735, None),
    ("dawn-valley", [-18.0, 1.8, -92.0], [-10.75, 2.6, -105.5], 62.0, 0.265, None),
    ("horizon", [-30.0, 38.0, -60.0], [-75.0, 28.0, -170.0], 70.0, 0.45, [-40.0, 20.0, -80.0]),
    ("coast", [-100.0, 0.35, -90.0], [-63.5, 2.2, -87.0], 60.0, 0.33, None),
    ("peak-close", [-62.0, 3.5, -105.0], [-56.0, 2.9, -114.75], 50.0, 0.70, None),
    ("high-lakes", [-25.0, 2.4, -140.0], [-21.25, 2.3, -150.0], 65.0, 0.52, None),
    ("peak-seam-low-sun", [-66.0, 2.5, -101.0], [-56.0, 2.75, -114.75], 55.0, 0.255, None),
]
M64 = (1 << 64) - 1


def splitmix(state):
    state = (state + 0x9E3779B97F4A7C15) & M64
    z = state
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return state, z ^ (z >> 31)


def order(seed, n):
    o = list(range(n))
    s = seed
    for i in range(n - 1, 0, -1):
        s, r = splitmix(s)
        j = r % (i + 1)
        o[i], o[j] = o[j], o[i]
    return o


def dist(a, b):
    return math.sqrt(sum((x - y) ** 2 for x, y in zip(a, b)))


def flight_secs(a, b):
    km = dist(a[1], b[5]) + dist(b[5], b[1]) if b[5] else dist(a[1], b[1])
    lo, hi, speed = (14.0, 24.0, 4.0) if b[5] else (8.0, 18.0, 2.5)
    return float(round(min(max(km / speed, lo), hi)))


def path(seed, seconds):
    """Keyframes and the (t, time_of_day) changes, the default order with no votes."""
    o = order(seed, len(VIEWPOINTS))
    keys, tods = [], [(0.0, VIEWPOINTS[o[0]][4])]
    t, step = 0.0, 0
    while t < seconds + 30:
        cur, nxt = VIEWPOINTS[o[step % len(o)]], VIEWPOINTS[o[(step + 1) % len(o)]]
        for tt in (t, t + HOLD):
            keys.append({"t": tt, "pos": cur[1], "look": cur[2], "fov_deg": cur[3]})
        fly = flight_secs(cur, nxt)
        if nxt[5]:
            keys.append({"t": t + HOLD + fly / 2, "pos": nxt[5], "look": nxt[2], "fov_deg": (cur[3] + nxt[3]) / 2})
        tods.append((t + HOLD, nxt[4]))
        t += HOLD + fly
        step += 1
    return keys, tods


def main():
    out, secs, cols, rows, h = sys.argv[1], float(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
    ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[6:]]
    assert len(ends) == cols * rows
    keys, tods = path(1, secs)
    epoch = int(time.time() * 1000)
    rects = [{"x": (k % cols) / cols, "y": (k // cols) / rows, "w": 1 / cols, "h": 1 / rows} for k in range(len(ends))]

    def post_all(tod):
        for k, (hh, p) in enumerate(ends):
            body = {"epoch_unix_ms": epoch, "rect": rects[k], "image_aspect": 16 / 9, "camera": keys,
                    "time_of_day": tod, "words": [], "lights": []}
            req = urllib.request.Request(f"http://{hh}:{p}/scene", data=json.dumps(body).encode(),
                                         headers={"Content-Type": "application/json"}, method="POST")
            urllib.request.urlopen(req, timeout=10).read()

    post_all(tods[0][1])
    stop = [False]

    def stream(k):
        st = fc.Stream(ends[k][0], ends[k][1], h)
        try:
            while not stop[0]:
                st.recv()
        finally:
            st.close()

    ths = [threading.Thread(target=stream, args=(k,), daemon=True) for k in range(len(ends))]
    for th in ths:
        th.start()
    ctl = subprocess.Popen([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "budgetctl.py"),
                            "--period", "2"] + [f"{hh}:{p}" for hh, p in ends], stdout=subprocess.PIPE, text=True)
    rounds = []

    def read_ctl():
        for line in ctl.stdout:
            if line.startswith("{"):
                rounds.append(dict(json.loads(line), at=time.time() * 1000 - epoch))

    rt = threading.Thread(target=read_ctl, daemon=True)
    rt.start()
    health = []
    next_tod = 1
    t0 = time.time()
    while time.time() - t0 < secs:
        st = (time.time() * 1000 - epoch) / 1000
        if next_tod < len(tods) and st >= tods[next_tod][0]:
            post_all(tods[next_tod][1])
            next_tod += 1
        snap = []
        for hh, p in ends:
            try:
                x = fc.health(hh, p)
                y = {k: x.get(k) for k in ("t", "budget", "budget_source", "budget_fit", "budget_fit_now", "render_ms",
                                           "slots_missed", "fps")}
                y["render_ms_max"] = (x.get("frames_2s") or {}).get("render_ms_max")  # over the last 2 s of frames
                snap.append(y)
            except Exception as e:  # noqa: BLE001
                snap.append({"error": str(e)})
        health.append({"at": st, "tiles": snap})
        time.sleep(2.0)
    stop[0] = True
    ctl.terminate()
    ctl.wait(timeout=10)
    json.dump({"tods": tods, "rounds": rounds, "health": health}, open(out + ".json", "w"))
    posted = [r.get("posted") for r in rounds if r.get("posted")]
    changes = [(round(r["at"] / 1000, 1), r["posted"]) for i, r in enumerate(rounds)
               if r.get("posted") and i > 0 and rounds[i - 1].get("posted") and r["posted"] != rounds[i - 1]["posted"]]
    rmax = max((x.get("render_ms_max") or 0) for s in health for x in s["tiles"] if s["at"] > 60)
    print(json.dumps({"rounds": len(rounds), "budgets": sorted(set(posted)), "changes": changes,
                      "render_ms_max_after_60s": rmax}))


if __name__ == "__main__":
    main()
