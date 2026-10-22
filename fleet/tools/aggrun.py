#!/usr/bin/env python3
"""Measure a host aggregator through the coordinator: latency added over the tiles, CPU,
output bitrate, and whether the joined region is seamless.

    aggrun.py ws://<coordinator>/live OUT [SECONDS]

Needs the renderer's tools (fleet/renderer/tools) for its frame client, Pillow and the
ffmpeg CLI. Writes OUT/aggregate.json and the images it compares.
"""
import json, os, struct, subprocess, sys, threading, time, urllib.request
sys.path.insert(0, os.environ.get("FLEETCLIENT_DIR", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "renderer", "tools")))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fleetclient as fc  # noqa: E402
import fleetlive  # noqa: E402
from fleetlive import Live, all_running, fcvm_tiles  # noqa: E402
from PIL import Image  # noqa: E402

URL, OUT = sys.argv[1], sys.argv[2]
SECS = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0
os.makedirs(OUT, exist_ok=True)
fleetlive.connect_to(URL)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def hostport(url):
    hp = url[len("ws://"):].split("/")[0]
    h, p = hp.rsplit(":", 1)
    return h, int(p)


def proc_cpu(pid):
    """utime+stime in seconds of pid and its children (ffmpeg)."""
    tck = os.sysconf("SC_CLK_TCK")
    total = 0.0
    pids = [pid] + [int(c) for c in open(f"/proc/{pid}/task/{pid}/children").read().split()]
    for p in pids:
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            total += (int(f[11]) + int(f[12])) / tck
        except OSError:
            pass
    return total, len(pids)


def pid_listening(port):
    out = subprocess.run(["ss", "-ltnpH", f"sport = :{port}"], capture_output=True, text=True).stdout
    for part in out.split("pid=")[1:]:
        return int(part.split(",")[0])
    return None


class Recorder:
    """A frame stream: receive time per t, bytes, header, (optionally) frames kept."""

    def __init__(self, host, port, h, keep_until=None):
        self.host, self.port, self.h, self.keep_until = host, port, h, keep_until
        self.recv, self.bytes, self.header, self.frames, self.stop = {}, 0, None, [], False
        self.t = threading.Thread(target=self.run, daemon=True)
        self.t.start()

    def run(self):
        s = fc.Stream(self.host, self.port, self.h)
        try:
            while not self.stop:
                op, pl = s.recv()
                now = time.time()
                if op == 1:
                    self.header = json.loads(pl)
                elif op == 2:
                    m, au = fc.parse_fcvf(pl)
                    self.recv.setdefault(m["t"], now)
                    self.bytes += len(pl)
                    if self.keep_until is not None and (not self.frames or self.frames[-1][0]["t"] < self.keep_until):
                        self.frames.append((m, au))
        finally:
            s.close()


v = Live()
st = v.until(lambda s: all_running(s) and any(a["frame_url"] for a in s["aggregates"] if a["half"] == "fcvm"), 1800)
agg = [a for a in st["aggregates"] if a["half"] == "fcvm" and a["frame_url"]][0]
tiles = {t["id"]: t for t in fcvm_tiles(st)}
ah, ap = hostport(agg["frame_url"])
ah = "127.0.0.1"
log("aggregate", agg["id"], "rect", agg["rect"], "tiles", agg["tiles"])
agg_pid = pid_listening(ap)
image_aspect = st["image_aspect"]
layout = json.loads(urllib.request.urlopen(f"http://{ah}:{ap}/layout", timeout=5).read())

# --- latency, bitrate, CPU over SECS ---------------------------------------------------
first = tiles[agg["tiles"][0]]
th, tp = hostport(first["frame_url"])
th = "127.0.0.1"
tile_h = round(1080 * first["rect"]["h"]) & ~1
a = Recorder(ah, ap, 1080)
d = Recorder(th, tp, tile_h)
time.sleep(3)  # let both streams settle
a.recv.clear(); d.recv.clear(); a.bytes = 0; d.bytes = 0
c0, n0 = proc_cpu(agg_pid)
t0 = time.time()
time.sleep(SECS)
c1, n1 = proc_cpu(agg_pid)
span = time.time() - t0
stats = json.loads(urllib.request.urlopen(f"http://{ah}:{ap}/stats", timeout=5).read())
a.stop = d.stop = True
common = sorted(set(a.recv) & set(d.recv))
added = sorted((a.recv[t] - d.recv[t]) * 1000 for t in common)
pct = lambda xs, q: xs[min(len(xs) - 1, round((len(xs) - 1) * q))] if xs else None
res = {
    "region": agg["rect"], "tiles": len(agg["tiles"]), "header": a.header, "seconds": round(span, 1),
    "aggregate_kbps": round(a.bytes * 8 / span / 1000), "tile_kbps": round(d.bytes * 8 / span / 1000),
    "aggregate_frames": len(a.recv), "tile_frames": len(d.recv),
    "added_ms_vs_direct_tile": {"p50": pct(added, 0.5), "p95": pct(added, 0.95), "max": pct(added, 1.0), "n": len(added)},
    "cpu_cores": round((c1 - c0) / span, 2), "processes": n1, "aggregator_stats": stats,
}
log("aggregate", res["aggregate_kbps"], "kbps;", "added over a direct tile p50", res["added_ms_vs_direct_tile"]["p50"],
    "ms; cpu", res["cpu_cores"], "cores")

# --- the same tiles at about 1080p worth of pixels -----------------------------------
# A second aggregator for the same layout, with the image tall enough that the region
# holds about 1920 x 1080 pixels.
fcvm_bin = os.environ.get("FCVM", "fcvm")
region = agg["rect"]
# region pixels = (region.w * aspect * H) x (region.h * H)
H2 = int((1920 * 1080 / (region["w"] * image_aspect * region["h"])) ** 0.5) & ~1
p2 = subprocess.Popen([fcvm_bin, "fleet", "aggregate", "--listen", "127.0.0.1:20202", "--image-height", str(H2)],
                      stdout=subprocess.DEVNULL, stderr=open(os.path.join(OUT, "aggregate-1080p.log"), "w"))
for _ in range(100):
    try:
        urllib.request.urlopen("http://127.0.0.1:20202/healthz", timeout=1)
        break
    except OSError:
        time.sleep(0.1)
req = urllib.request.Request("http://127.0.0.1:20202/layout", data=json.dumps(layout).encode(),
                             headers={"Content-Type": "application/json"}, method="POST")
urllib.request.urlopen(req, timeout=5)
b = Recorder("127.0.0.1", 20202, H2)
time.sleep(5)
b.bytes = 0
cb0, _ = proc_cpu(p2.pid)
tb = time.time()
time.sleep(SECS / 2)
cb1, _ = proc_cpu(p2.pid)
spanb = time.time() - tb
stats_b = json.loads(urllib.request.urlopen("http://127.0.0.1:20202/stats", timeout=5).read())
b.stop = True
res["at_1080p_pixels"] = {"image_height": H2, "header": b.header, "kbps": round(b.bytes * 8 / spanb / 1000),
                          "cpu_cores": round((cb1 - cb0) / spanb, 2), "stats": stats_b}
p2.terminate()
log("1080p-equivalent:", b.header and (b.header["width"], b.header["height"]), res["at_1080p_pixels"]["kbps"], "kbps")

# --- seams: the aggregate frame at T against the renderers' unencoded tiles ----------
T = round((time.time() * 1000 - st["scene"]["epoch_unix_ms"]) / 1000 * 30 + 5 * 30) / 30
rec = Recorder(ah, ap, 1080, keep_until=T)
raws = {}


def raw(tid):
    h, p = hostport(tiles[tid]["frame_url"])
    r = urllib.request.urlopen(f"http://127.0.0.1:{p}/raw?t={T!r}", timeout=30)
    meta = json.loads(r.headers["X-Fleet-Tile"])
    raws[tid] = (meta, Image.open(r))


ths = [threading.Thread(target=raw, args=(tid,)) for tid in agg["tiles"]]
[t.start() for t in ths]
[t.join() for t in ths]
while not rec.frames or rec.frames[-1][0]["t"] < T:
    time.sleep(0.05)
rec.stop = True
hdr = rec.header
fr = rec.frames
idx = [i for i, (m, _) in enumerate(fr) if m["t"] == T][0]
start = max(i for i in range(idx + 1) if fr[i][0]["key"])
imgs = fc.decode([au for _, au in fr[start:idx + 1]], hdr["width"], hdr["height"])
agg_img = Image.frombytes("RGB", (hdr["width"], hdr["height"]), imgs[-1])
raw_img = Image.new("RGB", (hdr["width"], hdr["height"]))
bounds_x, bounds_y = set(), set()
for tid, (meta, im) in raws.items():
    dx, dy = meta["x0"] - hdr["x0"], meta["y0"] - hdr["y0"]
    raw_img.paste(im.convert("RGB"), (dx, dy))
    # Tile edges inside the region, from the tile's rect in the whole-image grid.
    r = tiles[tid]["rect"]
    ex = round(r["x"] * hdr["full_width"]) - hdr["x0"]
    ey = round(r["y"] * hdr["full_height"]) - hdr["y0"]
    if ex > 0:
        bounds_x.add(ex)
    if ey > 0:
        bounds_y.add(ey)
agg_img.save(os.path.join(OUT, "aggregate-frame.png"))
raw_img.save(os.path.join(OUT, "raw-mosaic.png"))
W, H = hdr["width"], hdr["height"]
A = agg_img.convert("L").load()
R = raw_img.convert("L").load()
import math  # noqa: E402


def psnr(pairs):
    mse = sum((a - b) ** 2 for a, b in pairs) / max(1, len(pairs))
    return 10 * math.log10(255 * 255 / max(mse, 1e-9))


seam = lambda x, y: any(abs(x - b) <= 2 for b in bounds_x) or any(abs(y - b) <= 2 for b in bounds_y)
near, far = [], []
for y in range(0, H):
    for x in range(0, W):
        (near if seam(x, y) else far).append((A[x, y], R[x, y]))


def ratio(img, vertical, b, lo, hi):
    P = img
    if vertical:
        d = lambda x: sum(abs(P[x - 1, y] - P[x, y]) for y in range(lo, hi)) / (hi - lo)
    else:
        d = lambda y: sum(abs(P[x, y - 1] - P[x, y]) for x in range(lo, hi)) / (hi - lo)
    ref = sum(d(b + o) for o in list(range(-12, -2)) + list(range(3, 13))) / 20
    return d(b) / max(ref, 1e-9)


ratios = [round(ratio(A, True, bx, 0, H), 3) for bx in sorted(bounds_x)] + \
         [round(ratio(A, False, by, 0, W), 3) for by in sorted(bounds_y)]
res["seams"] = {"t": T, "tile_boundaries_x": sorted(bounds_x), "tile_boundaries_y": sorted(bounds_y),
                "psnr_luma_vs_unencoded_db": {"within_2px_of_a_tile_edge": round(psnr(near), 2),
                                              "elsewhere": round(psnr(far), 2)},
                "seam_ratio_aggregate": ratios}
log("seams:", res["seams"])
json.dump(res, open(os.path.join(OUT, "aggregate.json"), "w"), indent=1)

# --- a tile relaunching inside the region: the stream goes on, the tile black ---------
rel = Recorder(ah, ap, 1080)
time.sleep(2)
before = json.loads(urllib.request.urlopen(f"http://{ah}:{ap}/stats", timeout=5).read())
v2 = Live()
v2.until(lambda s: all_running(s), 60)
r = v2.intent("relaunch", {"tile": agg["tiles"][0]})
t_click = time.time()
v2.until(lambda s: [t for t in fcvm_tiles(s) if t["id"] == agg["tiles"][0]][0]["phase"] == "running"
         and [t for t in fcvm_tiles(s) if t["id"] == agg["tiles"][0]][0]["generation"] > tiles[agg["tiles"][0]]["generation"], 120)
t_back = time.time()
time.sleep(2)
rel.stop = True
gaps = sorted(b - a for a, b in zip(sorted(rel.recv.values()), sorted(rel.recv.values())[1:]))
win = [t for t, at in rel.recv.items() if t_click <= at <= t_back]
res["relaunch_inside_region"] = {
    "relaunch_s": round(t_back - t_click, 2), "frames_during": len(win),
    "expected_frames": round((t_back - t_click) * 30), "largest_gap_ms": round(gaps[-1] * 1000, 1) if gaps else None,
    "tiles_black_in_window": json.loads(urllib.request.urlopen(f"http://{ah}:{ap}/stats", timeout=5).read())["tiles_black"],
}
log("relaunch inside the region:", res["relaunch_inside_region"])
json.dump(res, open(os.path.join(OUT, "aggregate.json"), "w"), indent=1)
log("AGGRUN_DONE")
