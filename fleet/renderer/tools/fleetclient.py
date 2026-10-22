"""Minimal client for the renderer's routes (Python stdlib only).

    frames(host, port, h, count) -> (header_dict, [(meta, annexb_bytes), ...])
"""
import base64
import json
import os
import socket
import struct
import time
import urllib.request


def _recv_exact(s, n):
    b = bytearray()
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise ConnectionError("closed")
        b += c
    return bytes(b)


class Stream:
    def __init__(self, host, port, h, timeout=30.0, q=None):
        self.s = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        q = q or os.environ.get("Q")  # stream quality: hi, std or lo
        req = (f"GET /frames?h={h}{'&q=' + q if q else ''} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
               f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.s.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            c = self.s.recv(1)
            if not c:
                raise ConnectionError("closed during handshake")
            head += c
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(head.decode(errors="replace"))

    def recv(self):
        """Returns (opcode, payload)."""
        b0, b1 = _recv_exact(self.s, 2)
        n = b1 & 0x7F
        if n == 126:
            n = struct.unpack(">H", _recv_exact(self.s, 2))[0]
        elif n == 127:
            n = struct.unpack(">Q", _recv_exact(self.s, 8))[0]
        return b0 & 0x0F, _recv_exact(self.s, n)

    def send_text(self, text):
        data = text.encode()
        mask = os.urandom(4)
        hdr = bytes([0x81])
        if len(data) < 126:
            hdr += bytes([0x80 | len(data)])
        else:
            hdr += bytes([0x80 | 126]) + struct.pack(">H", len(data))
        self.s.sendall(hdr + mask + bytes(c ^ mask[i & 3] for i, c in enumerate(data)))

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def parse_fcvf(p):
    if p[:4] != b"FCVF" or p[4] != 1:
        raise ValueError("not an FCVF frame")
    w, h = struct.unpack("<HH", p[6:10])
    t = struct.unpack("<d", p[10:18])[0]
    n = struct.unpack("<I", p[18:22])[0]
    return {"key": bool(p[5] & 1), "w": w, "h": h, "t": t, "frame": n}, p[22:]


def frames(host, port, h, count=None, until_t=None, timeout=30.0, q=None):
    st = Stream(host, port, h, timeout, q)
    header, out = None, []
    try:
        while True:
            op, p = st.recv()
            if op == 1:
                header = json.loads(p)
            elif op == 2:
                meta, au = parse_fcvf(p)
                meta["recv_unix"] = time.time()
                out.append((meta, au))
                if count is not None and len(out) >= count:
                    break
                if until_t is not None and meta["t"] >= until_t:
                    break
            elif op == 8:
                break
    finally:
        st.close()
    return header, out


def post_scene(host, port, scene, timeout=10.0):
    req = urllib.request.Request(f"http://{host}:{port}/scene", data=json.dumps(scene).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def health(host, port, timeout=5.0):
    with urllib.request.urlopen(f"http://{host}:{port}/health", timeout=timeout) as r:
        return json.loads(r.read())


def decode(annexb_list, w, h):
    """Decode a list of Annex-B access units (first must be a keyframe) to RGB frames with ffmpeg."""
    import subprocess
    data = b"".join(annexb_list)
    r = subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "h264", "-i", "-", "-f", "rawvideo",
                        "-pix_fmt", "rgb24", "-"], input=data, capture_output=True, check=True)
    n = w * h * 3
    return [r.stdout[i:i + n] for i in range(0, len(r.stdout) - n + 1, n)]
