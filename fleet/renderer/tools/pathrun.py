"""The coordinator's directed camera on a grid of renderers, with the grid
budget controller, to count budget changes.

    make fleet-program SEED=1 SECS=900 OUT=program.json
    python3 tools/pathrun.py PROGRAM.json OUT_PREFIX SECONDS COLS ROWS H HOST:PORT ...

PROGRAM.json is the path fleet-control's director flies with no votes
(fleet/core/examples/program.rs): its keyframes and the time of day at each
second. Posts the keyframes to every tile as one scene (the tile's cell of a
COLS x ROWS grid), re-posts the scene whenever the time of day changes (same
epoch, as the coordinator's tick does), streams every tile at H px, and runs
tools/budgetctl.py (period 2 s) against all tiles. Records in OUT_PREFIX.json
the budget each round posted and every tile's /health every 2 s.
"""
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request

import fleetclient as fc


def path(program):
    """Keyframes and the (t, time_of_day) changes."""
    p = json.load(open(program))
    tods, last = [], None
    for t, v in p["time_of_day"]:
        if v != last:
            tods.append((float(t), v))
            last = v
    return p["camera"], tods


def main():
    program, out, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
    cols, rows, h = int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
    ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[7:]]
    assert len(ends) == cols * rows
    keys, tods = path(program)
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
