"""Stitch one image from several renderers.

    python3 mosaic.py OUT_PREFIX COLS ROWS TILE_H SECONDS SCENE_JSON HOST:PORT...

Gives renderer k the rect of grid cell k (row-major), posts the same scene and
epoch to all of them, subscribes to each at TILE_H for SECONDS, then picks the
latest frame time t that every stream delivered, decodes each stream up to that
frame and pastes every tile at its whole-image pixel offset (x0, y0 from the
stream header), drawing only each stream's inner rectangle (its `crop`). Writes OUT_PREFIX.png and OUT_PREFIX.json (per tile: t, frame,
GPU, size).
Q=hi|std|lo in the environment selects the stream quality (default std).
"""
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
for k, (h, p) in enumerate(ends):
    s = dict(scene)
    s["rect"] = {"x": (k % cols) / cols, "y": (k // cols) / rows, "w": 1 / cols, "h": 1 / rows}
    fc.post_scene(h, p, s)

results = [None] * len(ends)
rates = [0.0] * len(ends)


def run(k):
    st = fc.Stream(ends[k][0], ends[k][1], tile_h)
    hdr, fr, t0 = None, [], time.time()
    try:
        while time.time() - t0 < secs:
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
    hl = fc.health(*ends[k])
    meta.append({"cell": k, "x0": hdr["x0"], "y0": hdr["y0"], "w": hdr["width"], "h": hdr["height"], "crop": c,
                 "t": t_pick, "frame": fr[idx][0]["frame"], "gpu": hl["gpu"], "fps": hl["fps"],
                 "spp": hl["spp"], "hold_samples": hl["hold_samples"], "bits_per_s": round(rates[k]), "q": hdr.get("q")})
canvas.save(out + ".png")
json.dump({"t": t_pick, "epoch_unix_ms": scene["epoch_unix_ms"], "tiles": meta}, open(out + ".json", "w"), indent=1)
print(f"stitched {len(ends)} tiles at t={t_pick} into {out}.png ({canvas.size[0]}x{canvas.size[1]})")
