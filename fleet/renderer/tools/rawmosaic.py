"""Stitch unencoded frames from several renderers at one frame time.

    python3 rawmosaic.py OUT_PREFIX COLS ROWS TILE_H SCENE_JSON HOST:PORT...

Like mosaic.py, but instead of decoding the H.264 streams it asks each renderer
for its unencoded tile of one future frame (GET /raw?t=T, a debug route), so
the stitched image can be compared pixel for pixel with a whole image drawn in
one piece (fleet-renderer still --seq). Streams at TILE_H stay subscribed the
whole time so every renderer draws at that height. Writes OUT_PREFIX.ppm and
OUT_PREFIX.json (scene with its epoch, t, per-tile geometry and spp).
Q=hi|std|lo in the environment selects the stream quality (default std).
GRID=C,R: the whole image is a C x R grid and the renderers take its top-left
COLS x ROWS cells; the output covers that block. SETTLE=S: seconds of
streaming before the frame time is chosen (default 4), e.g. for a budget
controller to settle.
"""
import os
import json
import sys
import threading
import time
import urllib.request

import fleetclient as fc

out, cols, rows, tile_h, scene_path = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
ends = [(a.rsplit(":", 1)[0], int(a.rsplit(":", 1)[1])) for a in sys.argv[6:]]
scene = json.load(open(scene_path))
scene["epoch_unix_ms"] = int(time.time() * 1000)
gc, gr = (int(v) for v in os.environ["GRID"].split(",")) if os.environ.get("GRID") else (cols, rows)
for k, (h, p) in enumerate(ends):
    s = dict(scene)
    s["rect"] = {"x": (k % cols) / gc, "y": (k // cols) / gr, "w": 1 / gc, "h": 1 / gr}
    fc.post_scene(h, p, s)
stop = False


streams = [None] * len(ends)


def drain(k):
    # keep the stream (header and frames) so the decoded frame at T can be
    # stitched beside the unencoded one
    st = fc.Stream(ends[k][0], ends[k][1], tile_h)
    hdr, fr = None, []
    try:
        while not stop:
            op, pl = st.recv()
            if op == 1:
                hdr, fr = json.loads(pl), []
            elif op == 2:
                fr.append(fc.parse_fcvf(pl))
                if fr[-1][0]["t"] >= T_box[0]:
                    break
    finally:
        st.close()
    streams[k] = (hdr, fr)


T_box = [1e18]


ths = [threading.Thread(target=drain, args=(k,), daemon=True) for k in range(len(ends))]
for t in ths:
    t.start()
time.sleep(float(os.environ.get("SETTLE", "4")))
T = (int((time.time() * 1000 - scene["epoch_unix_ms"]) / 1000 * 30) + 60) / 30.0
T_box[0] = T
tiles = [None] * len(ends)


def grab(k):
    with urllib.request.urlopen(f"http://{ends[k][0]}:{ends[k][1]}/raw?t={T!r}", timeout=30) as r:
        tiles[k] = (json.loads(r.headers["X-Fleet-Tile"]), r.read())


gs = [threading.Thread(target=grab, args=(k,)) for k in range(len(ends))]
for g in gs:
    g.start()
for g in gs:
    g.join()
for t in ths:
    t.join(timeout=20)
stop = True
meta0 = tiles[0][0]
W = min(meta0["full_width"], max(m["x0"] + m["width"] for m, _ in tiles))
H = min(meta0["full_height"], max(m["y0"] + m["height"] for m, _ in tiles))
canvas = bytearray(W * H * 3)
for meta, ppm in tiles:
    hdr_end = ppm.index(b"255\n") + 4
    px = ppm[hdr_end:]
    for y in range(meta["height"]):
        Y = meta["y0"] + y
        if Y >= H:
            continue
        n = min(meta["width"], W - meta["x0"])
        canvas[(Y * W + meta["x0"]) * 3:(Y * W + meta["x0"] + n) * 3] = px[y * meta["width"] * 3:(y * meta["width"] + n) * 3]
open(out + ".ppm", "wb").write(b"P6\n%d %d\n255\n" % (W, H) + bytes(canvas))
# the same frame as the browser gets it: decoded from each tile's stream
dec = bytearray(W * H * 3)
for (meta, _), (hdr, fr) in zip(tiles, streams):
    idx = [i for i, (m, _) in enumerate(fr) if m["t"] == T][0]
    img = fc.decode([a for _, a in fr[:idx + 1]], hdr["width"], hdr["height"])[idx]
    c = hdr.get("crop", {"left": 0, "top": 0, "right": 0, "bottom": 0})  # the inner rectangle, as a page draws it
    iw, ih = hdr["width"] - c["left"] - c["right"], hdr["height"] - c["top"] - c["bottom"]
    for y in range(ih):
        Y = hdr["y0"] + y
        if Y >= H:
            continue
        n = min(iw, W - hdr["x0"])
        row = (y + c["top"]) * hdr["width"] + c["left"]
        dec[(Y * W + hdr["x0"]) * 3:(Y * W + hdr["x0"] + n) * 3] = img[row * 3:(row + n) * 3]
open(out + "-decoded.ppm", "wb").write(b"P6\n%d %d\n255\n" % (W, H) + bytes(dec))
s = dict(scene)
s["rect"] = {"x": 0, "y": 0, "w": 1, "h": 1}
json.dump({"t": T, "scene": s, "tiles": [m for m, _ in tiles]}, open(out + ".json", "w"), indent=1)
print(f"stitched {len(ends)} unencoded tiles at t={T} into {out}.ppm ({W}x{H}); spp {[m['spp'] for m, _ in tiles]}")
