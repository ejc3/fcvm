"""Frames per second per height against a running renderer, with the camera moving.

    python3 measure.py HOST PORT [seconds] [heights...]

Posts a scene whose camera never holds (so every frame is a moving frame, the
expensive case), subscribes at each height in turn, and prints one CSV row per
height from what the client received and what /health reports.
"""
import json
import sys
import time

import fleetclient as fc

host, port = sys.argv[1], int(sys.argv[2])
secs = float(sys.argv[3]) if len(sys.argv) > 3 else 10
heights = [int(h) for h in sys.argv[4:]] or [136, 180, 240, 360, 1080]
now = int(time.time() * 1000)
cam = [{"t": float(i * 20), "pos": [-12 - 2.0 * i, 0.4 + 0.25 * i, -8 - 6.0 * i],
        "look": [-40 - 2.0 * i, 1.2, -80 - 6.0 * i], "fov_deg": 60.0} for i in range(400)]
print("height,width,rect,frames,client_fps,spp,render_ms,encode_ms,gpu_util_pct,power_w")
for h in heights:
    # wall tiles are 1/16 x 1/9 of a 16:9 image; the detail view is the whole image
    rect = {"x": 0.5, "y": 0.333, "w": 0.0625, "h": 1 / 9} if h <= 400 else {"x": 0, "y": 0, "w": 1, "h": 1}
    fc.post_scene(host, port, {"epoch_unix_ms": now, "rect": rect, "image_aspect": 16 / 9, "camera": cam,
                               "time_of_day": 0.3, "words": [], "lights": []})
    st = fc.Stream(host, port, h)
    hdr, got, t0 = None, [], None
    samples = []
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
                    if len(got) % 15 == 0:
                        samples.append(fc.health(host, port))
                if t0 is not None and tnow - t0 >= secs:
                    break
    finally:
        st.close()
    fps = (len(got) - 1) / (got[-1] - got[0]) if len(got) > 1 else 0
    avg = lambda k: sum(s[k] for s in samples) / max(1, len(samples))
    gavg = lambda k: sum((s["gpu"][k] or 0) for s in samples) / max(1, len(samples))
    print(f"{hdr['height']},{hdr['width']},{'tile' if h <= 400 else 'whole'},{len(got)},{fps:.1f},{avg('spp'):.1f},"
          f"{avg('render_ms'):.1f},{avg('encode_ms'):.2f},{gavg('util_pct'):.0f},{gavg('power_w'):.0f}", flush=True)
