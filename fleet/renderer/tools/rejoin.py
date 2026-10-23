"""A renderer leaves a moving grid and a fresh one rejoins: is its tile back at
once, every frame, with full history?

    python3 tools/rejoin.py OUT_PREFIX SCENE_JSON START_CMD HOST:PORT x4

Four renderers draw a moving 2 x 2 grid at BUDGET (707,107) pixel-samples, each
streamed at 270 px. After 8 s the renderer of cell 1 is stopped (its VM
killed), and START_CMD (a shell command) starts a fresh one; it prints the
new renderer's HOST:PORT on its last line once that answers /health. The
scene and the budget are posted to it as the coordinator does, every
renderer's unencoded tile is asked for (GET /raw) for 6 s of slots from just
before, and a stream subscribes at 270 px. Records in OUT_PREFIX.json the
times (stop, ready, subscribe, first frame), the rejoined stream's frames and
the slots missing after its first, the slots each renderer drew, and frames
F..F+7 (F: the rejoined stream's first): the rejoined tile alone
(OUT_PREFIX-alone-<k>.ppm, with its place in "alone") and, where all four drew
them, stitched (OUT_PREFIX-<k>.ppm), to compare with the same frames drawn
whole.
"""
import json
import math
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

import fleetclient as fc

out, scene_path, start_cmd = sys.argv[1], sys.argv[2], sys.argv[3]
ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[4:]]
assert len(ends) == 4
BUDGET, H, SLOTS, FRAMES, GONE = int(os.environ.get("BUDGET", 707107)), 270, 180, 8, 1
grid = [{"x": 0.5 * (k % 2), "y": 0.5 * (k // 2), "w": 0.5, "h": 0.5} for k in range(4)]
scene = json.load(open(scene_path))
for k in ("previous_rect", "rect_from_frame", "layout_generation", "budget"):
    scene.pop(k, None)
scene["epoch_unix_ms"] = int(time.time() * 1000)


def now_t():
    return (time.time() * 1000 - scene["epoch_unix_ms"]) / 1000


def post(path, h, p, body):
    req = urllib.request.Request(f"http://{h}:{p}{path}", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def join(k):
    post("/scene", ends[k][0], ends[k][1], dict(scene, rect=grid[k]))
    post("/budget", ends[k][0], ends[k][1], {"budget": BUDGET, "t": 0})


for k in range(4):
    join(k)
frames = [[] for _ in range(4)]
stops = [False] * 4


def run(k):
    st = fc.Stream(ends[k][0], ends[k][1], H)
    try:
        while not stops[k]:
            op, pl = st.recv()
            if op == 2:
                m, _ = fc.parse_fcvf(pl)
                frames[k].append((round(m["t"] * 30), time.time()))
    except Exception:  # noqa: BLE001 (the stopped renderer's stream breaks)
        pass
    finally:
        st.close()


ths = [threading.Thread(target=run, args=(k,), daemon=True) for k in range(4)]
for th in ths:
    th.start()
time.sleep(8)
rec = {"budget": BUDGET}
rec["stop_t"] = now_t()
stops[GONE] = True
start = subprocess.run(start_cmd, shell=True, capture_output=True, text=True, timeout=300)
last = start.stdout.strip().splitlines()[-1]
ends[GONE] = (last.rsplit(":", 1)[0], int(last.rsplit(":", 1)[1]))
rec["ready_t"] = now_t()
join(GONE)

raw = {}
S0 = math.ceil(now_t() * 30) + 2


def grab(k, kk):
    try:
        with urllib.request.urlopen(f"http://{ends[k][0]}:{ends[k][1]}/raw?t={kk / 30!r}", timeout=30) as r:
            raw[(k, kk)] = (json.loads(r.headers["X-Fleet-Tile"]), r.read())
    except urllib.error.HTTPError:
        raw[(k, kk)] = None


# the rejoined renderer's clock can run behind the host's for a while after
# its VM starts (0.5 s measured), so its own tiles are asked for from 2 s back
gs = [threading.Thread(target=grab, args=(k, kk)) for k in range(4)
      for kk in range(S0 - (60 if k == GONE else 0), S0 + SLOTS)]
for g in gs:
    g.start()
time.sleep(0.2)
rec["subscribe_t"] = now_t()
frames[GONE] = []
stops[GONE] = False
th = threading.Thread(target=run, args=(GONE,), daemon=True)
th.start()
time.sleep(5.5)
for k in range(4):
    stops[k] = True
for g in gs:
    g.join()

mine = [f for f, _ in frames[GONE]]
F = mine[0] if mine else None
rec["first_frame"] = F
rec["first_frame_after_s"] = round(frames[GONE][0][1] - (scene["epoch_unix_ms"] / 1000 + rec["subscribe_t"]), 3) if mine else None
rec["frames"] = len(mine)
rec["missing_after_first"] = [k for k in range(F, mine[-1] + 1) if k not in set(mine)] if mine else []
rec["drawn"] = [[kk for kk in range(S0 - 60, S0 + SLOTS) if raw.get((k, kk))] for k in range(4)]
# the rejoined tile alone, frames F..F+7, to compare with the same region of
# the frames drawn whole (works whatever the other tiles were drawing then)
rec["alone"] = []
for kk in range(F, F + FRAMES) if F else []:
    x = raw.get((GONE, kk))
    if x is None:
        rec["alone"].append({"k": kk, "missing": True})
        continue
    open(f"{out}-alone-{kk}.ppm", "wb").write(x[1])
    rec["alone"].append(dict(x[0], k=kk))
rec["stitched"] = []
for kk in range(F, F + FRAMES) if F else []:
    tiles = [raw.get((k, kk)) for k in range(4)]
    if any(x is None for x in tiles):
        rec["stitched"].append({"k": kk, "missing_tiles": [k for k in range(4) if tiles[k] is None]})
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
    rec["stitched"].append({"k": kk, "spp": [x[0]["spp"] for x in tiles], "hq": [x[0]["hq"] for x in tiles], "height": Hh})
rec["scene"] = dict(scene, rect={"x": 0, "y": 0, "w": 1, "h": 1})
json.dump(rec, open(out + ".json", "w"), indent=1)
print(json.dumps({k: rec[k] for k in ("stop_t", "ready_t", "subscribe_t", "first_frame", "first_frame_after_s", "frames",
                                      "missing_after_first")}))
print(json.dumps({"alone_drawn": [a["k"] for a in rec["alone"] if not a.get("missing")]}))
print(json.dumps([{k: v for k, v in s.items() if k != "hq"} for s in rec["stitched"]]))
