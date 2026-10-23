"""Frames per second per height against a running renderer, with the camera moving.

    python3 measure.py HOST PORT [seconds] [heights...]

Posts a scene whose camera never holds (so every frame is a moving frame, the
expensive case), subscribes at each height in turn, and prints one CSV row per
height from what the client received and what /health reports.
"""
import json
import os
import sys
import time

import fleetclient as fc

host, port = sys.argv[1], int(sys.argv[2])
secs = float(sys.argv[3]) if len(sys.argv) > 3 else 10
heights = [int(h) for h in sys.argv[4:]] or [136, 180, 240, 360, 1080]
# rows: (height, rect); a wall tile is 1/16 x 1/9 of a 16:9 image, the detail
# view opens one tile at 1080, and 720 and 1080 are also drawn as the whole image
tile_rect = {"x": float(os.environ.get("TILE_X", 0.5)), "y": float(os.environ.get("TILE_Y", 0.333)), "w": 0.0625, "h": 1 / 9}
whole_rect = {"x": 0, "y": 0, "w": 1, "h": 1}
rows = [(h, tile_rect, "tile") for h in heights if h <= 400]
rows += [(h, whole_rect, "whole") for h in heights if h > 400]
rows += [(1080, tile_rect, "detail")] if 1080 in heights else []
now = int(time.time() * 1000)
cam = [{"t": float(i * 20), "pos": [-12 - 2.0 * i, 0.4 + 0.25 * i, -8 - 6.0 * i],
        "look": [-40 - 2.0 * i, 1.2, -80 - 6.0 * i], "fov_deg": 60.0} for i in range(400)]
print("inner_height,inner_width,stream,rect,frames,client_fps,spp,checkerboard,render_ms,encode_ms,gpu_util_pct,power_w,"
      "slots_missed,slot_gaps,frame_ms_p99,frame_ms_max,render_ms_max,encode_ms_max")
for h, rect, kind in rows:
    scene = {"epoch_unix_ms": now, "rect": rect, "image_aspect": 16 / 9, "camera": cam,
             "time_of_day": 0.3, "words": [], "lights": []}
    if os.environ.get("TEXT_STYLE"):  # how the clock and words are drawn
        scene["text_style"] = os.environ["TEXT_STYLE"]
    if os.environ.get("WORD"):  # a word on screen for the whole row
        scene["words"] = [{"text": os.environ["WORD"], "t": (time.time() * 1000 - now) / 1000 - 10}]
    if os.environ.get("BUDGET"):  # the scene's per-frame budget (pixel-samples) instead of the renderer's
        scene["budget"] = int(os.environ["BUDGET"])
    fc.post_scene(host, port, scene)
    st = fc.Stream(host, port, h)
    hdr, got, t0 = None, [], None
    samples, slots = [], []
    miss0 = fc.health(host, port).get("slots_missed", 0)
    try:
        while True:
            op, p = st.recv()
            if op == 1:
                hdr = json.loads(p)
            elif op == 2:
                m, _ = fc.parse_fcvf(p)
                tnow = time.time()
                if t0 is None:
                    t0 = tnow + 2.0  # let the sample controller settle
                if tnow >= t0:
                    got.append(tnow)
                    slots.append(m["t"])
                    if len(got) % 15 == 0:
                        samples.append(fc.health(host, port))
                if t0 is not None and tnow - t0 >= secs:
                    break
    finally:
        st.close()
    fps = (len(got) - 1) / (got[-1] - got[0]) if len(got) > 1 else 0
    miss = (samples[-1].get("slots_missed", 0) - miss0) if samples else 0
    gaps = sum(1 for a, b in zip(slots, slots[1:]) if b - a > 1.5 / 30)  # slots the stream skipped
    fmax = lambda k: max(((s.get("frames_2s") or {}).get(k, 0) for s in samples), default=0)
    avg = lambda k: sum(s[k] for s in samples) / max(1, len(samples))
    gavg = lambda k: sum((s["gpu"][k] or 0) for s in samples) / max(1, len(samples))
    c = hdr.get("crop", {"left": 0, "top": 0, "right": 0, "bottom": 0})
    iw, ih = hdr["width"] - c["left"] - c["right"], hdr["height"] - c["top"] - c["bottom"]
    print(f"{ih},{iw},{hdr['width']}x{hdr['height']},{kind},{len(got)},{fps:.1f},{avg('spp'):.1f},{samples[-1].get('checkerboard') if samples else ''},"
          f"{avg('render_ms'):.1f},{avg('encode_ms'):.2f},{gavg('util_pct'):.0f},{gavg('power_w'):.0f},"
          f"{miss},{gaps},{fmax('frame_ms_p99'):.1f},{fmax('frame_ms_max'):.1f},{fmax('render_ms_max'):.1f},{fmax('encode_ms_max'):.1f}", flush=True)
