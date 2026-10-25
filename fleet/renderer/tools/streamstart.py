"""Four fresh streams on a moving 2 x 2 grid: do their first frames stitch?

    python3 tools/streamstart.py OUT_PREFIX SCENE_JSON HOST:PORT x4

Posts the scene to four renderers (one cell of a 2 x 2 grid each) and a budget
with a past `t` (353,553 pixel-samples, a rung the slowest cell draws in about
70% of a slot, as the controller would set it), waits while nothing streams, then subscribes one stream per
renderer at 270 px, 70 ms apart, as clients that connect at different moments
do. Meanwhile it asks every renderer for the unencoded tile (GET /raw) of every
slot for 4 s from just before the first subscription. F is the first frame
every stream has delivered. Frames F to F+7 are the ones whose history covers
a stream's start; each is stitched from the four tiles into
OUT_PREFIX-<k>.ppm, to be compared with the same frame drawn whole. Records in
OUT_PREFIX.json the slots, each stream's first frame and missing frames, the
tiles' metadata, and the frames a tile did not draw (a tile that skipped one
of F..F+7 fails the test).
"""
import json
import math
import sys
import threading
import time
import urllib.error
import urllib.request

import fleetclient as fc

out, scene_path = sys.argv[1], sys.argv[2]
ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[3:]]
assert len(ends) == 4, "four renderers"
BUDGET, H, STAGGER, SLOTS, FRAMES = 353553, 270, 0.07, 120, 8
grid = [{"x": 0.5 * (k % 2), "y": 0.5 * (k // 2), "w": 0.5, "h": 0.5} for k in range(4)]
scene = json.load(open(scene_path))
for k in ("previous_rect", "rect_from_frame", "layout_generation", "budget"):
    scene.pop(k, None)
scene["epoch_unix_ms"] = int(time.time() * 1000)


def post(path, h, p, body):
    req = urllib.request.Request(f"http://{h}:{p}{path}", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


for k, (h, p) in enumerate(ends):
    post("/scene", h, p, dict(scene, rect=grid[k]))
    post("/budget", h, p, {"budget": BUDGET, "t": 0})
time.sleep(3)  # nothing streams: the renderers draw idle frames twice a second

tnow = (time.time() * 1000 - scene["epoch_unix_ms"]) / 1000
S0 = math.ceil(tnow * 30) + 3
raw = {}


def grab(k, kk):
    try:
        with urllib.request.urlopen(f"http://{ends[k][0]}:{ends[k][1]}/raw?t={kk / 30!r}", timeout=30) as r:
            raw[(k, kk)] = (json.loads(r.headers["X-Fleet-Tile"]), r.read())
    except urllib.error.HTTPError:
        raw[(k, kk)] = None  # the renderer did not draw that slot


gs = [threading.Thread(target=grab, args=(k, kk)) for k in range(4) for kk in range(S0, S0 + SLOTS)]
for g in gs:
    g.start()
time.sleep(0.2)

firsts = [[] for _ in ends]


def run(k):
    # every stream stays until all four have delivered F+FRAMES (a renderer
    # whose stream closes draws idle frames at another size)
    time.sleep(k * STAGGER)
    st = fc.Stream(ends[k][0], ends[k][1], H)
    try:
        while len(firsts[k]) < 300:
            op, pl = st.recv()
            if op == 2:
                m, _ = fc.parse_fcvf(pl)
                firsts[k].append(round(m["t"] * 30))
                if all(firsts) and all(f[-1] >= max(g[0] for g in firsts) + FRAMES for f in firsts):
                    break
    finally:
        st.close()


ths = [threading.Thread(target=run, args=(k,)) for k in range(4)]
for t in ths:
    t.start()
for t in ths:
    t.join(timeout=60)
for g in gs:
    g.join()

F = max(f[0] for f in firsts)
rec = {"S0": S0, "F": F, "budget": BUDGET,
       "streams": [{"first": f[0], "missing": [k for k in range(f[0], f[-1] + 1) if k not in f]} for f in firsts],
       "drawn": [[kk for kk in range(S0, S0 + SLOTS) if raw.get((k, kk))] for k in range(4)],
       "frames": []}
for kk in range(F, F + FRAMES):
    tiles = [raw.get((k, kk)) for k in range(4)]
    if any(x is None for x in tiles):
        rec["frames"].append({"k": kk, "missing_tiles": [k for k in range(4) if tiles[k] is None]})
        continue
    m0 = tiles[0][0]
    W, Hh = m0["full_width"], m0["full_height"]
    canvas = bytearray(W * Hh * 3)
    for meta, ppm in tiles:
        px = ppm[ppm.index(b"255\n") + 4:]
        for y in range(meta["height"]):
            Y = meta["y0"] + y
            if Y >= Hh:
                continue
            n = min(meta["width"], W - meta["x0"])
            canvas[(Y * W + meta["x0"]) * 3:(Y * W + meta["x0"] + n) * 3] = px[y * meta["width"] * 3:(y * meta["width"] + n) * 3]
    open(f"{out}-{kk}.ppm", "wb").write(b"P6\n%d %d\n255\n" % (W, Hh) + bytes(canvas))
    rec["frames"].append({"k": kk, "spp": [x[0]["spp"] for x in tiles], "hq": [x[0]["hq"] for x in tiles],
                          "height": Hh})
rec["scene"] = dict(scene, rect={"x": 0, "y": 0, "w": 1, "h": 1})
json.dump(rec, open(out + ".json", "w"), indent=1)
print(json.dumps({"S0": S0, "F": F, "streams": rec["streams"],
                  "frames": [{k: v for k, v in f.items() if k != "hq"} for f in rec["frames"]]}))
