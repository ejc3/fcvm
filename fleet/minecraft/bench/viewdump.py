#!/usr/bin/env python3
"""Read one tile's view socket (hello, map, a status) and print them as one JSON object."""
import base64, json, os, socket, struct, sys, time

ip = sys.argv[1]
s = socket.create_connection((ip, 8080), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall(f"GET /view HTTP/1.1\r\nHost: {ip}:8080\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
buf = b""
while b"\r\n\r\n" not in buf:
    d = s.recv(65536)
    if not d:
        sys.exit(f"closed before the upgrade's headers: {buf[:200]!r}")
    buf += d
head, buf = buf.split(b"\r\n\r\n", 1)
if b" 101 " not in head.split(b"\r\n")[0]:
    sys.exit(f"no upgrade: {head[:200]!r}")


def need(n):
    global buf
    while len(buf) < n:
        d = s.recv(1 << 20)
        if not d:
            sys.exit("closed")
        buf += d
    out, buf = buf[:n], buf[n:]
    return out


def frame():
    b0, b1 = need(2)
    n = b1 & 127
    if n == 126:
        n = struct.unpack(">H", need(2))[0]
    elif n == 127:
        n = struct.unpack(">Q", need(8))[0]
    return b0 & 15, bool(b0 & 128), need(n)


def message():
    data, op0 = b"", None
    while True:
        op, fin, p = frame()
        if op in (8, 9, 10):
            if op == 8:
                sys.exit(f"closed by tile: {p!r}")
            continue
        op0 = op0 or op
        data += p
        if fin:
            return data.decode()


p = json.dumps({"viewer": "v-density"}).encode()
mask = os.urandom(4)
s.sendall(bytes([0x81, 0x80 | len(p)]) + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(p)))
out, end = {}, time.time() + 20
while time.time() < end and not all(k in out for k in ("hello", "map", "status")):
    # A tile that goes quiet ends the collection, and what came before is still printed.
    s.settimeout(max(0.1, end - time.time()))
    try:
        m = json.loads(message())
    except socket.timeout:
        break
    if m.get("type") in ("hello", "map"):
        out[m["type"]] = m
    elif m.get("type") == "status" and m.get("players"):
        out["status"] = m
print(json.dumps(out))
