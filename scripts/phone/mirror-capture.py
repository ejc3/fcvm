#!/usr/bin/env python3
"""A phone streamer's GET /frames and GET /mirror captured at once, as the page receives them
(docs/fleet/ARCHITECTURE.md, "The GPU mirror"). scripts/phone/mirror-capture.sh runs it.

  mirror-capture.py <host:port> <seconds> <out prefix> [--no-video] [--no-mirror]

Posts a scene naming a test viewer as the phone's holder, with an admission key made for the
run, opens both streams with that viewer's direct token, and writes:
  <prefix>.tap   the mirror's tap stream, inflated (e2e/mirror.spec.ts replays it)
  <prefix>.json  each stream's wire bytes, frames and replies per second, and the totals
"""
import base64, hashlib, hmac, json, os, socket, struct, sys, threading, time, zlib

addr, secs, prefix = sys.argv[1], float(sys.argv[2]), sys.argv[3]
host, port = addr.rsplit(":", 1)
port = int(port)
KEY = os.urandom(32)
VIEWER = "v-capture"


def token():
    signed = f"v1.{int(time.time()) + 3600}.{VIEWER}"
    mac = hmac.new(KEY, signed.encode(), hashlib.sha256).digest()
    return signed + "." + base64.urlsafe_b64encode(mac).rstrip(b"=").decode()


def http(method, path, body=b""):
    s = socket.create_connection((host, port), timeout=10)
    req = f"{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body
    s.sendall(req)
    out = b""
    while True:
        b = s.recv(65536)
        if not b:
            break
        out += b
    s.close()
    return out.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in out else out


def ws_open(path):
    s = socket.create_connection((host, port), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += s.recv(4096)
    head, rest = buf.split(b"\r\n\r\n", 1)
    assert b" 101 " in head.split(b"\r\n")[0], head
    msg = json.dumps({"admit": token()}).encode()
    mask = os.urandom(4)
    hdr = bytes([0x81, 0x80 | len(msg)]) if len(msg) < 126 else bytes([0x81, 0xFE]) + struct.pack(">H", len(msg))
    s.sendall(hdr + mask + bytes(b ^ mask[i & 3] for i, b in enumerate(msg)))
    return s, rest


def ws_frames(s, rest, end):
    buf = rest
    s.settimeout(0.5)
    while time.time() < end:
        while len(buf) >= 2:
            op = buf[0] & 0x0F
            n = buf[1] & 0x7F
            at = 2
            if n == 126:
                if len(buf) < 4:
                    break
                n = struct.unpack(">H", buf[2:4])[0]
                at = 4
            elif n == 127:
                if len(buf) < 10:
                    break
                n = struct.unpack(">Q", buf[2:10])[0]
                at = 10
            if len(buf) < at + n:
                break
            yield op, buf[at:at + n]
            buf = buf[at + n:]
        try:
            b = s.recv(1 << 20)
        except socket.timeout:
            continue
        if not b:
            return
        buf += b


body = json.dumps({"epoch_unix_ms": int(time.time() * 1000), "rect": {"x": 0, "y": 0, "w": 1, "h": 1}, "image_aspect": 0.5625, "camera": [], "words": [], "lights": [], "time_of_day": 0.5, "admit_keys": [KEY.hex()], "mirror_viewer": VIEWER}).encode()
print("scene:", http("POST", "/scene", body).decode()[:200], flush=True)
time.sleep(1)
t0 = time.time()
end = t0 + secs
per = {"video": {}, "mirror": {}}
totals = {"video": {"wire_bytes": 0, "frames": 0}, "mirror": {"wire_bytes": 0, "tap_bytes": 0, "replies": 0, "data_bytes": 0, "records": 0}}
closes = {}


def sec():
    return int(time.time() - t0)


def video():
    if "--no-video" in sys.argv:
        return
    s, rest = ws_open("/frames?h=1280&q=std")
    for op, pl in ws_frames(s, rest, end):
        if op == 8:
            closes["video"] = pl[2:].decode(errors="replace")
            return
        if op != 2:
            continue
        d = per["video"].setdefault(sec(), {"wire_bytes": 0, "frames": 0})
        d["wire_bytes"] += len(pl)
        d["frames"] += 1
        totals["video"]["wire_bytes"] += len(pl)
        totals["video"]["frames"] += 1


def mirror():
    if "--no-mirror" in sys.argv:
        return
    s, rest = ws_open("/mirror")
    z = zlib.decompressobj(-15)
    tap = open(prefix + ".tap", "wb")
    pending = b""
    for op, pl in ws_frames(s, rest, end):
        if op == 8:
            closes["mirror"] = pl[2:].decode(errors="replace")
            break
        if op != 2:
            continue
        raw = z.decompress(pl)
        tap.write(raw)
        k = sec()
        d = per["mirror"].setdefault(k, {"wire_bytes": 0, "tap_bytes": 0, "replies": 0, "data_bytes": 0})
        d["wire_bytes"] += len(pl)
        d["tap_bytes"] += len(raw)
        totals["mirror"]["wire_bytes"] += len(pl)
        totals["mirror"]["tap_bytes"] += len(raw)
        pending += raw
        while len(pending) >= 24:
            typ, ch, n = struct.unpack("<III", pending[:12])
            if len(pending) < 24 + n:
                break
            totals["mirror"]["records"] += 1
            if typ == 5:
                d["replies"] += 1
                totals["mirror"]["replies"] += 1
            if typ == 4:
                d["data_bytes"] += n
                totals["mirror"]["data_bytes"] += n
            pending = pending[24 + n:]
    tap.close()


ts = [threading.Thread(target=f) for f in (video, mirror)]
for t in ts:
    t.start()
for t in ts:
    t.join()
el = time.time() - t0
out = {"seconds": el, "totals": totals, "per_second": per, "closes": closes}
for k in ("video", "mirror"):
    w = totals[k]["wire_bytes"]
    out[k + "_mbit_s"] = w * 8 / 1e6 / el
out["video_fps"] = totals["video"]["frames"] / el
out["mirror_replies_per_s"] = totals["mirror"]["replies"] / el
if totals["mirror"]["tap_bytes"]:
    out["mirror_deflate_ratio"] = totals["mirror"]["tap_bytes"] / max(1, totals["mirror"]["wire_bytes"])
json.dump(out, open(prefix + ".json", "w"), indent=1)
print(json.dumps({k: v for k, v in out.items() if k != "per_second"}, indent=1))
