"""Smoke check against a running renderer: health, a scene post, frames at two heights."""
import json
import sys
import time

import fleetclient as fc

host, port = sys.argv[1], int(sys.argv[2])
print("health", json.dumps(fc.health(host, port)))
hdr, fr = fc.frames(host, port, 240, count=60)
print("header", hdr)
keys = [m["frame"] for m, _ in fr if m["key"]]
ts = [m["t"] for m, _ in fr]
print(f"{len(fr)} frames, keyframes at {keys}, t {ts[0]:.3f}..{ts[-1]:.3f}, first key {fr[0][0]['key']}")
dt = [b - a for a, b in zip(ts, ts[1:])]
print("t steps", sorted(set(round(x, 4) for x in dt)))
imgs = fc.decode([a for _, a in fr], hdr["width"], hdr["height"])
print("decoded", len(imgs), "frames of", hdr["width"], "x", hdr["height"])
print("health", json.dumps(fc.health(host, port)))

# GET /still: with no stream, a JPEG of the frame drawn for the requested tile height
import urllib.error
import urllib.request

with urllib.request.urlopen(f"http://{host}:{port}/still?h=360&quality=85", timeout=20) as r:
    body, tile = r.read(), json.loads(r.headers["X-Fleet-Tile"])
    assert r.headers["Content-Type"] == "image/jpeg", r.headers["Content-Type"]
assert body[:2] == b"\xff\xd8" and body[-2:] == b"\xff\xd9", "not a JPEG"
assert tile["height"] == 360 and tile["width"] > 0, tile
print("still", len(body), "bytes", tile)
try:
    urllib.request.urlopen(f"http://{host}:{port}/still?h=9", timeout=10)
    raise AssertionError("h=9 was accepted")
except urllib.error.HTTPError as e:
    assert e.code == 400, e.code
