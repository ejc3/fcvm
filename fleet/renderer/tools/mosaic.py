"""Stitch one image from several renderers.

    python3 mosaic.py OUT_PREFIX COLS ROWS TILE_H SECONDS SCENE_JSON HOST:PORT...

Gives renderer k the rect of grid cell k (row-major), posts the same scene and
epoch to all of them, subscribes to each at TILE_H for SECONDS, then picks the
latest frame time t that every stream delivered, decodes each stream up to that
frame and pastes every tile at its whole-image pixel offset (x0, y0 from the
stream header), drawing only each stream's inner rectangle (its `crop`). Writes OUT_PREFIX.png and OUT_PREFIX.json (per tile: t, frame,
GPU, size).
Q=hi|std|lo in the environment selects the stream quality (default std).
GRID=C,R in the environment makes the whole image a C x R grid of which the
renderers take a COLS x ROWS block of its cells (a block of a larger wall),
from cell GRID_AT=C0,R0 (default 0,0), and crops the output to that block. KEEP_SCENE=1 posts no scene (keeps the one the
renderers have). The JSON records, per tile, /health read while streaming
(fps, render ms, budget, its source, fit, hq) and the frames per second the
client received over the last half of SECONDS.
"""
import os
import json
import sys
import threading
import time

from PIL import Image

import fleetclient as fc

out, cols, rows, tile_h, secs, scene_path = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), \
    float(sys.argv[5]), sys.argv[6]
ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[7:]]
assert len(ends) == cols * rows, "need one renderer per cell"
scene = json.load(open(scene_path))
scene["epoch_unix_ms"] = scene.get("epoch_unix_ms") or int(time.time() * 1000)
gc, gr = (int(v) for v in os.environ["GRID"].split(",")) if os.environ.get("GRID") else (cols, rows)
c0, r0 = (int(v) for v in os.environ.get("GRID_AT", "0,0").split(","))
if not os.environ.get("KEEP_SCENE"):
    for k, (h, p) in enumerate(ends):
        s = dict(scene)
        s["rect"] = {"x": (c0 + k % cols) / gc, "y": (r0 + k // cols) / gr, "w": 1 / gc, "h": 1 / gr}
        fc.post_scene(h, p, s)

results = [None] * len(ends)
rates = [0.0] * len(ends)
live = [None] * len(ends)  # /health read while the stream runs (at 3/4 of SECONDS)


def run(k):
    st = fc.Stream(ends[k][0], ends[k][1], tile_h)
    hdr, fr, t0 = None, [], time.time()
    try:
        while time.time() - t0 < secs:
            if live[k] is None and time.time() - t0 > 0.75 * secs:
                live[k] = fc.health(*ends[k])
            op, pl = st.recv()
            if op == 1:
                hdr, fr = json.loads(pl), []  # a new header restarts the stream
            elif op == 2:
                fr.append(fc.parse_fcvf(pl))
    finally:
        st.close()
    results[k] = (hdr, fr)
    rates[k] = sum(len(a) for _, a in fr) * 8 / max(1e-3, (fr[-1][0]["t"] - fr[0][0]["t"])) if len(fr) > 1 else 0


ths = [threading.Thread(target=run, args=(k,)) for k in range(len(ends))]
for t in ths:
    t.start()
for t in ths:
    t.join()
common = set.intersection(*[set(m["t"] for m, _ in fr) for _, fr in results])
if not common:
    sys.exit("no frame time common to every stream")
t_pick = max(common)
hdr0 = results[0][0]
canvas = Image.new("RGB", (hdr0["full_width"], hdr0["full_height"]))
meta = []
for k, (hdr, fr) in enumerate(results):
    idx = [i for i, (m, _) in enumerate(fr) if m["t"] == t_pick][0]
    imgs = fc.decode([a for _, a in fr[:idx + 1]], hdr["width"], hdr["height"])
    tile = Image.frombytes("RGB", (hdr["width"], hdr["height"]), imgs[idx])
    c = hdr.get("crop", {"left": 0, "top": 0, "right": 0, "bottom": 0})  # draw only the inner rectangle
    tile = tile.crop((c["left"], c["top"], hdr["width"] - c["right"], hdr["height"] - c["bottom"]))
    canvas.paste(tile, (hdr["x0"], hdr["y0"]))
    hl = live[k] or fc.health(*ends[k])
    late = [m["t"] for m, _ in fr if m["t"] >= fr[-1][0]["t"] - secs / 2]
    client_fps = (len(late) - 1) / (late[-1] - late[0]) if len(late) > 1 and late[-1] > late[0] else 0
    meta.append({"cell": k, "x0": hdr["x0"], "y0": hdr["y0"], "w": hdr["width"], "h": hdr["height"], "crop": c,
                 "t": t_pick, "frame": fr[idx][0]["frame"], "gpu": hl["gpu"], "fps": hl["fps"],
                 "spp": hl["spp"], "hold_samples": hl["hold_samples"], "bits_per_s": round(rates[k]), "q": hdr.get("q"),
                 "render_ms": hl.get("render_ms"), "budget": hl.get("budget"), "budget_source": hl.get("budget_source"),
                 "budget_fit": hl.get("budget_fit"), "hq": hl.get("hq"), "client_fps": round(client_fps, 1)})
if (gc, gr) != (cols, rows):
    x1 = max(m["x0"] + m["w"] - m["crop"]["left"] - m["crop"]["right"] for m in meta)
    y1 = max(m["y0"] + m["h"] - m["crop"]["top"] - m["crop"]["bottom"] for m in meta)
    canvas = canvas.crop((min(m["x0"] for m in meta), min(m["y0"] for m in meta), x1, y1))
canvas.save(out + ".png")
json.dump({"t": t_pick, "epoch_unix_ms": scene["epoch_unix_ms"], "tiles": meta}, open(out + ".json", "w"), indent=1)
print(f"stitched {len(ends)} tiles at t={t_pick} into {out}.png ({canvas.size[0]}x{canvas.size[1]})")
