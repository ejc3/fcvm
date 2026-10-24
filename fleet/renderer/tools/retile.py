"""A re-tile mid-stream across four renderers (the coordinator's layout generations).

    python3 tools/retile.py OUT_PREFIX SCENE_JSON HOST:PORT x4

Posts the scene as layout generation 1 (a 2 x 2 grid), then a budget with a
past `t`, and streams every tile at 270 px. Then posts generation 2 (four
columns) with `previous_rect` (the tile's cell in the grid) and
`rect_from_frame` K 2.5 s ahead, and the budget again, as the coordinator does
after every scene. Captures every tile's unencoded frame at K-1 and K+6
(GET /raw) and stitches each under its own layout: OUT_PREFIX-old.ppm and
OUT_PREFIX-new.ppm. Records in OUT_PREFIX.json, per stream, the headers seen and
the first and last frame and the missing frames under each, and the budget each tile reported after
the budget post after the re-tile. Then, on tile 0 while it streams, posts a
scene on the same clock and one on a new clock, each followed 1 s later by the
budget, and records the budget of every frame drawn in that second.
"""
import json
import math
import sys
import threading
import time
import urllib.request

import fleetclient as fc

out, scene_path = sys.argv[1], sys.argv[2]
ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[3:]]
assert len(ends) == 4, "four renderers"
BUDGET = 500000
grid = [{"x": 0.5 * (k % 2), "y": 0.5 * (k // 2), "w": 0.5, "h": 0.5} for k in range(4)]
cols = [{"x": 0.25 * k, "y": 0, "w": 0.25, "h": 1} for k in range(4)]
scene = json.load(open(scene_path))
scene["epoch_unix_ms"] = int(time.time() * 1000)
for k in ("previous_rect", "rect_from_frame", "layout_generation", "budget"):
    scene.pop(k, None)


def post(path, h, p, body):
    req = urllib.request.Request(f"http://{h}:{p}{path}", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def budget_now():
    time.sleep(0.5)
    return [{k: fc.health(h, p).get(k) for k in ("budget", "budget_source", "layout_generation")} for h, p in ends]


rec = {"budget": BUDGET}
for k, (h, p) in enumerate(ends):
    post("/scene", h, p, dict(scene, rect=grid[k], layout_generation=1))
for h, p in ends:
    post("/budget", h, p, {"budget": BUDGET, "t": 0})

streams = [[] for _ in ends]  # per stream: [header, [frame t...]]
stop = [1e18]


def run(k):
    st = fc.Stream(ends[k][0], ends[k][1], 270)
    try:
        while True:
            op, pl = st.recv()
            if op == 1:
                streams[k].append([json.loads(pl), []])
            elif op == 2:
                m, _ = fc.parse_fcvf(pl)
                streams[k][-1][1].append(m["t"])
                if m["t"] > stop[0]:
                    break
    finally:
        st.close()


ths = [threading.Thread(target=run, args=(k,)) for k in range(4)]
for t in ths:
    t.start()
time.sleep(4)
tnow = max(fc.health(h, p)["t"] for h, p in ends)
K = math.ceil(tnow * 30) + 75
for k, (h, p) in enumerate(ends):
    post("/scene", h, p, dict(scene, rect=cols[k], previous_rect=grid[k], rect_from_frame=K, layout_generation=2))
for h, p in ends:
    post("/budget", h, p, {"budget": BUDGET, "t": 0})
rec["after_second_budget"] = budget_now()
rec["K"] = K
stop[0] = (K + 10) / 30

raw = {}


def grab(k, which, kk):
    with urllib.request.urlopen(f"http://{ends[k][0]}:{ends[k][1]}/raw?t={kk / 30!r}", timeout=30) as r:
        raw[(k, which)] = (json.loads(r.headers["X-Fleet-Tile"]), r.read())


gs = [threading.Thread(target=grab, args=(k, w, kk)) for k in range(4) for w, kk in (("old", K - 1), ("new", K + 6))]
for g in gs:
    g.start()
for g in gs:
    g.join()
for t in ths:
    t.join(timeout=30)


def stitch(which):
    m0 = raw[(0, which)][0]
    W, H = m0["full_width"], m0["full_height"]
    canvas = bytearray(W * H * 3)
    for k in range(4):
        meta, ppm = raw[(k, which)]
        px = ppm[ppm.index(b"255\n") + 4:]
        for y in range(meta["height"]):
            Y = meta["y0"] + y
            if Y >= H:
                continue
            n = min(meta["width"], W - meta["x0"])
            canvas[(Y * W + meta["x0"]) * 3:(Y * W + meta["x0"] + n) * 3] = px[y * meta["width"] * 3:(y * meta["width"] + n) * 3]
    open(f"{out}-{which}.ppm", "wb").write(b"P6\n%d %d\n255\n" % (W, H) + bytes(canvas))
    return [raw[(k, which)][0] for k in range(4)]




def gap(new_clock):
    """Post a scene to tile 0 while it streams, wait 1 s before re-posting the
    budget, and record the budget of every frame drawn in that second (from
    /health, which reports the last frame's)."""
    h, p = ends[0]
    seen = []
    st = fc.Stream(h, p, 270)
    done = [False]

    def drain():
        try:
            while not done[0]:
                st.recv()
        except Exception:
            pass

    th = threading.Thread(target=drain)
    th.start()
    time.sleep(2)
    body = dict(scene, rect=cols[0], layout_generation=2)
    if new_clock:
        body["epoch_unix_ms"] = scene["epoch_unix_ms"] + 1000
    post("/scene", h, p, body)
    t0 = time.time()
    while time.time() - t0 < 1.0:
        hh = fc.health(h, p)
        seen.append((hh["frame"], hh["budget"], hh["budget_source"]))
        time.sleep(0.03)
    post("/budget", h, p, {"budget": BUDGET, "t": 0})
    time.sleep(0.3)
    hh = fc.health(h, p)
    seen.append((hh["frame"], hh["budget"], hh["budget_source"]))
    done[0] = True
    st.close()
    th.join(timeout=5)
    return sorted(set(seen))


rec["gap_same_clock"] = gap(False)
rec["gap_new_clock"] = gap(True)
for kname in ("gap_same_clock", "gap_new_clock"):
    print(kname, "frames", len(rec[kname]), "budgets", sorted({(b, src) for _, b, src in rec[kname]}))

rec["old"] = stitch("old")
rec["new"] = stitch("new")
rec["streams"] = [[{"header": {k: hd[k] for k in ("width", "height", "full_width", "full_height", "x0", "y0", "layout_generation")},
                    "first_t": ts[0] if ts else None, "last_t": ts[-1] if ts else None, "frames": len(ts),
                    "missing": [k for k in range(round(ts[0] * 30), round(ts[-1] * 30) + 1) if k not in {round(x * 30) for x in ts}] if ts else []}
                   for hd, ts in s] for s in streams]
whole = dict(scene, rect={"x": 0, "y": 0, "w": 1, "h": 1})
rec["scene"] = whole
json.dump(rec, open(out + ".json", "w"), indent=1)
print(json.dumps({"K": K, "after_second_budget": rec["after_second_budget"],
                  "old_spp": [m["spp"] for m in rec["old"]], "new_spp": [m["spp"] for m in rec["new"]],
                  "old_gen": [m["layout_generation"] for m in rec["old"]], "new_gen": [m["layout_generation"] for m in rec["new"]]}))
for k, s in enumerate(rec["streams"]):
    print(k, [(x["header"]["layout_generation"], x["header"]["width"], x["header"]["height"], x["header"]["full_height"],
               round(x["first_t"] * 30) if x["first_t"] else None, round(x["last_t"] * 30) if x["last_t"] else None,
               "missing", x["missing"]) for x in s])
