"""A scripted viewer of the coordinator's /live WebSocket (Python standard library), shared by
fleetrun.py and aggrun.py."""
import base64, json, os, socket, struct, threading, time, uuid
import fleetclient as fc

host, port = None, None


def connect_to(url):
    """Point Live at ws://host:port/live."""
    global host, port
    host, port = url[len("ws://"):].split("/")[0].split(":")


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
