#!/usr/bin/env python3
"""Measure a running fleet through its coordinator: wait for every fcvm tile, relaunch all
RUNS times (click to the last tile's first frame, and each slot's phases), relaunch one
tile three times, then stitch a mosaic from the frame URLs the coordinator publishes.
Results go to OUT as they are taken (jsonl), so a box that dies keeps what it had.

    fleetrun.py ws://<coordinator>/live OUT RUNS

Needs the renderer's tools (fleet/renderer/tools, PR #1144) for the frame client, Pillow
and the ffmpeg CLI. fleet/tools/gpurun.sh is the run on a GPU host.
"""
import base64, json, os, socket, statistics, struct, sys, threading, time, uuid
sys.path.insert(0, os.environ.get("FLEETCLIENT_DIR", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "renderer", "tools")))
import fleetclient as fc  # noqa: E402

URL, OUT, RUNS = sys.argv[1], sys.argv[2], int(sys.argv[3])
host, port = URL[len("ws://"):].split("/")[0].split(":")


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def append(name, obj):
    with open(os.path.join(OUT, name), "a") as f:
        f.write(json.dumps(obj) + "\n")


class Live:
    """One viewer: a /live socket, the latest snapshot and the receipts."""

    def __init__(self):
        self.s = socket.create_connection((host, int(port)), timeout=600)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET /live HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
                        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.s.recv(1)
        assert b" 101 " in head.split(b"\r\n")[0], head
        self.snap, self.receipts, self.cv, self.closed = None, {}, threading.Condition(), False
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        buf = b""
        try:
            while True:
                b0, b1 = fc._recv_exact(self.s, 2)
                n = b1 & 0x7F
                if n == 126:
                    n = struct.unpack(">H", fc._recv_exact(self.s, 2))[0]
                elif n == 127:
                    n = struct.unpack(">Q", fc._recv_exact(self.s, 8))[0]
                p = fc._recv_exact(self.s, n)
                op = b0 & 0x0F
                if op == 8:
                    break
                if op in (1, 0):
                    buf += p
                    if not b0 & 0x80:
                        continue
                    m = json.loads(buf)
                    buf = b""
                    with self.cv:
                        if m["type"] == "snapshot":
                            if self.snap is None or m["version"] > self.snap["version"]:
                                self.snap = m
                        elif m["type"] == "receipt":
                            self.receipts.setdefault(m["id"], m)
                        self.cv.notify_all()
        except (OSError, ConnectionError):
            pass  # closed by close() or by the coordinator
        finally:
            with self.cv:
                self.closed = True
                self.cv.notify_all()

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        hdr = bytes([0x81])
        if len(data) < 126:
            hdr += bytes([0x80 | len(data)])
        else:
            hdr += bytes([0x80 | 126]) + struct.pack(">H", len(data))
        self.s.sendall(hdr + mask + bytes(c ^ mask[i & 3] for i, c in enumerate(data)))

    def until(self, pred, timeout):
        end = time.time() + timeout
        with self.cv:
            while True:
                if self.snap and pred(self.snap["state"]):
                    return self.snap["state"]
                left = end - time.time()
                if left <= 0 or self.closed:
                    raise TimeoutError("condition not met")
                self.cv.wait(left)

    def intent(self, name, args):
        iid = str(uuid.uuid4())
        self.send({"type": "intent", "id": iid, "name": name, "args": args})
        with self.cv:
            while iid not in self.receipts:
                self.cv.wait(30)
            return self.receipts[iid]

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def fcvm_tiles(st):
    return [t for t in st["tiles"] if t["half"] == "fcvm"]


def all_running(st, n=None):
    ts = fcvm_tiles(st)
    return len(ts) > 0 and (n is None or len(ts) == n) and all(t["phase"] == "running" for t in ts)


v = Live()
st = v.until(lambda s: s["fleet"]["gpus"] > 0, 300)
n_gpus = st["fleet"]["gpus"]
log("hosts", [(h["name"], len(h["gpus"])) for h in st["fleet"]["hosts"]], "n", st["fleet"]["n"])
t0 = time.time()
st = v.until(lambda s: all_running(s, n_gpus), 1800)
log(f"all {n_gpus} fcvm tiles running {time.time() - t0:.1f}s after the first snapshot")
for t in fcvm_tiles(st):
    append("first_launch.jsonl", {"tile": t["id"], "slot": t["slot"], "gpu": t["gpu"], "frame_url": t["frame_url"],
                                  "rect": t["rect"]})
v.close()


def relaunch_all(run):
    v = Live()
    v.until(lambda s: all_running(s), 600)
    time.sleep(20)  # settle: every renderer streaming nothing, GPUs idle
    r = v.intent("relaunch_all", {})
    assert r["outcome"] == "committed", r
    with v.cv:
        while v.snap["version"] < r["version"]:
            v.cv.wait(30)
        started = v.snap["state"]["relaunch_all"]["started_at"]
    st = v.until(lambda s: s["relaunch_all"]["started_at"] == started and s["relaunch_all"]["fcvm_done_at"] is not None, 600)
    ra = st["relaunch_all"]
    rec = {"run": run, "started_at": ra["started_at"], "fcvm_done_at": ra["fcvm_done_at"],
           "click_to_last_first_frame_ms": ra["fcvm_done_at"] - ra["started_at"], "last": ra["fcvm_last"],
           "slots": [{"tile": t["id"], "slot": t["slot"], "gpu": t["gpu"]["sbdf"],
                      "click_to_first_frame_ms": t["last_relaunch"]["first_frame_at"] - t["last_relaunch"]["clicked_at"],
                      "phases": t["last_relaunch"]["phases"]} for t in fcvm_tiles(st)]}
    append("relaunch_all.jsonl", rec)
    log(f"relaunch_all run {run}: click to last first frame {rec['click_to_last_first_frame_ms']} ms, last {rec['last']}")
    v.close()
    return rec


recs = []
for run in range(1, RUNS + 1):
    try:
        recs.append(relaunch_all(run))
    except Exception as e:  # keep going; the record says what failed
        log(f"relaunch_all run {run} failed: {e!r}")
        append("relaunch_all.jsonl", {"run": run, "error": repr(e)})

singles = []
for run in range(1, 4):
    try:
        v = Live()
        v.until(lambda s: all_running(s), 600)
        time.sleep(15)
        before = {t["id"]: t["generation"] for t in fcvm_tiles(v.snap["state"])}
        r = v.intent("relaunch", {"tile": "fcvm:1"})
        assert r["outcome"] == "committed", r
        st = v.until(lambda s: fcvm_tiles(s)[0]["generation"] > before["fcvm:1"] and fcvm_tiles(s)[0]["phase"] == "running"
                     and fcvm_tiles(s)[0]["last_relaunch"]["first_frame_at"] is not None, 600)
        t = fcvm_tiles(st)[0]
        rec = {"run": run, "tile": t["id"], "slot": t["slot"],
               "click_to_first_frame_ms": t["last_relaunch"]["first_frame_at"] - t["last_relaunch"]["clicked_at"],
               "phases": t["last_relaunch"]["phases"]}
        append("relaunch_one.jsonl", rec)
        singles.append(rec)
        log(f"relaunch one run {run}: {rec['click_to_first_frame_ms']} ms")
        v.close()
    except Exception as e:
        log(f"relaunch one run {run} failed: {e!r}")

# A light and an evening sun for the picture, then let the hold converge.
v = Live()
v.until(lambda s: all_running(s), 600)
log("light", v.intent("light", {"pos": [-36.0, 1.75, -61.0], "color": [1.0, 0.45, 0.15]})["outcome"])
log("time_of_day", v.intent("time_of_day", {"v": 0.32})["outcome"])
time.sleep(25)
st = v.snap["state"]
json.dump(st, open(os.path.join(OUT, "snapshot-at-mosaic.json"), "w"), indent=1)
v.close()

H = int(os.environ.get("MOSAIC_H", "360"))
tiles = [t for t in fcvm_tiles(st) if t["frame_url"]]
results = [None] * len(tiles)


def grab(k):
    try:
        grab_one(k)
    except Exception as e:
        log(f"tile {tiles[k]['id']}: no frames: {e!r}")


def grab_one(k):
    u = tiles[k]["frame_url"][len("ws://"):].split("/")[0]
    h, p = u.rsplit(":", 1)
    s = fc.Stream(h, int(p), H)
    hdr, fr, start = None, [], time.time()
    try:
        while time.time() - start < 6:
            op, pl = s.recv()
            if op == 1:
                hdr, fr = json.loads(pl), []
            elif op == 2:
                fr.append(fc.parse_fcvf(pl))
    finally:
        s.close()
    results[k] = (hdr, fr, (h, int(p)))


ths = [threading.Thread(target=grab, args=(k,)) for k in range(len(tiles))]
[t.start() for t in ths]
[t.join() for t in ths]
from PIL import Image, ImageDraw  # noqa: E402
got = [k for k in range(len(tiles)) if results[k] and results[k][1]]
log(f"frames from {len(got)} of {len(tiles)} tiles")
tiles = [tiles[k] for k in got]
results = [results[k] for k in got]
common = set.intersection(*[set(m["t"] for m, _ in fr) for _, fr, _ in results])
t_pick = max(common)
hdr0 = results[0][0]
canvas = Image.new("RGB", (hdr0["full_width"], hdr0["full_height"]), (12, 12, 14))
meta = []
for k, (hdr, fr, ep) in enumerate(results):
    idx = [i for i, (m, _) in enumerate(fr) if m["t"] == t_pick][0]
    imgs = fc.decode([a for _, a in fr[:idx + 1]], hdr["width"], hdr["height"])
    canvas.paste(Image.frombytes("RGB", (hdr["width"], hdr["height"]), imgs[idx]), (hdr["x0"], hdr["y0"]))
    hl = fc.health(*ep)
    meta.append({"tile": tiles[k]["id"], "slot": tiles[k]["slot"], "gpu": tiles[k]["gpu"], "rect": tiles[k]["rect"],
                 "x0": hdr["x0"], "y0": hdr["y0"], "w": hdr["width"], "h": hdr["height"], "frame": fr[idx][0]["frame"],
                 "fps": hl["fps"], "spp": hl.get("spp"), "hold_samples": hl.get("hold_samples"), "gpu_health": hl["gpu"]})
d = ImageDraw.Draw(canvas)
W2 = hdr0["full_width"] // 2
d.line([(W2, 0), (W2, hdr0["full_height"])], fill=(90, 90, 90), width=2)
d.text((W2 + 40, hdr0["full_height"] // 2), "EC2 half: fake EC2 driver, no renderer (waiting for capacity)",
       fill=(150, 150, 150))
canvas.save(os.path.join(OUT, "mosaic.png"))
json.dump({"t": t_pick, "tile_h": H, "tiles": meta}, open(os.path.join(OUT, "mosaic.json"), "w"), indent=1)
log(f"mosaic of {len(tiles)} tiles at t={t_pick} ({canvas.size[0]}x{canvas.size[1]})")

ok = [r for r in recs if "click_to_last_first_frame_ms" in r]
if ok:
    xs = [r["click_to_last_first_frame_ms"] for r in ok]
    log("relaunch_all click to last first frame ms:", xs, "median", statistics.median(xs))
log("FLEETRUN_DONE")
