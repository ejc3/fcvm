#!/usr/bin/env python3
"""Per-request render driver: drive the warm Chromium CDP endpoint in a clone.

Navigates the existing (warm) page target to a fixture URL, waits for the load
event (plus, with --idle-wait-ms > 0, a network-idle-ish lifecycle event),
captures a PNG screenshot and a DOM dump, and prints one machine-parsable
RENDER_OK line with per-phase timings.

    render.py http://127.0.0.1:8000/medium.html --out-prefix /tmp/medium

The idle wait defaults OFF: every in-repo fixture is subresource-complete at
the load event (verified byte-identical artifacts either way), CDP's
networkIdle needs a 500ms quiet window (~1s constant tax per request), and
after a main-thread-saturating load (heavy.html) the event may never fire at
all, eating the whole budget. Turn it on only for pages that fetch after load.

Pure python3 stdlib. The WebSocket client is implemented inline rather than
apt-installing python3-websockets: that package is asyncio-based (event-loop
ceremony for a strictly sequential request/response protocol), varies by
Debian release, and adds image weight; CDP over loopback needs only the
RFC 6455 client handshake, masked text frames, fragment reassembly, and
ping/pong — ~90 lines, no TLS, no extensions.
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import time
import urllib.request
from urllib.parse import urlparse

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WsClosed(Exception):
    pass


class WsClient:
    """Minimal RFC 6455 client: handshake, masked sends, fragment reassembly."""

    def __init__(self, ws_url: str, deadline: float):
        u = urlparse(ws_url)
        self.sock = socket.create_connection(
            (u.hostname, u.port or 80), timeout=max(0.05, deadline - time.monotonic())
        )
        self._buf = b""  # BEFORE the handshake: _recv_until parks leftovers here
        key = base64.b64encode(os.urandom(16)).decode()
        path = u.path + ("?" + u.query if u.query else "")
        self.sock.sendall(
            (
                f"GET {path} HTTP/1.1\r\n"
                f"Host: {u.hostname}:{u.port or 80}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        response = self._recv_until(b"\r\n\r\n", deadline)
        status = response.split(b"\r\n", 1)[0]
        if b" 101" not in status:
            raise ConnectionError(f"ws handshake rejected: {status.decode(errors='replace')}")
        expect = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest())
        if expect not in response:
            raise ConnectionError("ws handshake: bad Sec-WebSocket-Accept")

    def _recv_until(self, marker: bytes, deadline: float) -> bytes:
        """Read through `marker`, KEEPING whatever followed it in `self._buf`.

        It used to accumulate into a local and return it, so any bytes the peer
        coalesced into the same segment as the 101 response were DROPPED — and
        `__init__` then reset `self._buf = b""`, discarding them a second time.
        The next `_recv_exact(2)` would start reading in the middle of a frame,
        the header parse would desync, and the symptom is precisely
        `WsClosed("connection closed mid-frame")`. This is a free correctness fix
        for a failure mode the CDP arm reports at 1.7%/10.0% and the exec arm
        (which speaks guest loopback with no socat relay in the path) never hits;
        it is a variable removed, NOT a diagnosis of that drop.
        """
        data = self._buf
        self._buf = b""
        while marker not in data:
            self.sock.settimeout(max(0.05, deadline - time.monotonic()))
            chunk = self.sock.recv(4096)
            if not chunk:
                raise WsClosed("connection closed during handshake")
            data += chunk
        head, _, rest = data.partition(marker)
        self._buf = rest
        return head + marker

    def _recv_exact(self, n: int, deadline: float) -> bytes:
        while len(self._buf) < n:
            self.sock.settimeout(max(0.05, deadline - time.monotonic()))
            chunk = self.sock.recv(min(262144, n - len(self._buf) + 4096))
            if not chunk:
                raise WsClosed("connection closed mid-frame")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)  # client frames must be masked (RFC 6455 5.3)
        n = len(payload)
        if n < 126:
            header = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 65536:
            header = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            header = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def send_text(self, text: str) -> None:
        self._send_frame(0x1, text.encode())

    def recv_text(self, deadline: float) -> str:
        """Return the next complete text message, transparently answering pings."""
        message = b""
        while True:
            b1, b2 = self._recv_exact(2, deadline)
            fin, opcode = b1 & 0x80, b1 & 0x0F
            n = b2 & 0x7F
            if n == 126:
                (n,) = struct.unpack("!H", self._recv_exact(2, deadline))
            elif n == 127:
                (n,) = struct.unpack("!Q", self._recv_exact(8, deadline))
            if b2 & 0x80:  # server frames are never masked
                raise ConnectionError("unexpected masked frame from server")
            payload = self._recv_exact(n, deadline)
            if opcode == 0x8:
                raise WsClosed("server sent close frame")
            if opcode == 0x9:
                self._send_frame(0xA, payload)  # ping -> pong
                continue
            if opcode == 0xA:
                continue  # unsolicited pong
            message += payload  # text (0x1) or continuation (0x0)
            if fin:
                return message.decode()

    def close(self) -> None:
        try:
            self._send_frame(0x8, b"")
            self.sock.close()
        except OSError:
            pass


class Cdp:
    """Sequential CDP session over one WsClient, stashing events as they arrive."""

    def __init__(self, ws: WsClient):
        self.ws = ws
        self.events = []
        self.next_id = 0

    def cmd(self, method: str, params: dict | None = None, deadline: float = 0):
        self.next_id += 1
        cid = self.next_id
        self.ws.send_text(json.dumps({"id": cid, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(self.ws.recv_text(deadline))
            if msg.get("id") == cid:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
            if "method" in msg:
                self.events.append(msg)

    def wait_event(self, pred, deadline: float):
        """Return the first event matching pred, checking stashed events first."""
        for i, ev in enumerate(self.events):
            if pred(ev):
                return self.events.pop(i)
        while True:
            if time.monotonic() >= deadline:
                raise TimeoutError("timed out waiting for event")
            msg = json.loads(self.ws.recv_text(deadline))
            if "method" not in msg:
                continue
            if pred(msg):
                return msg
            self.events.append(msg)


def find_page_ws_url(cdp_host: str, deadline: float) -> str:
    """Pick the warm page target (retrying briefly: a sample lost to one refused
    connect during fan-out would be pure noise)."""
    last_err = None
    for _ in range(10):
        try:
            with urllib.request.urlopen(
                f"http://{cdp_host}/json/list", timeout=max(0.1, deadline - time.monotonic())
            ) as r:
                targets = json.load(r)
            pages = [
                t
                for t in targets
                if t.get("type") == "page" and not t.get("url", "").startswith("devtools://")
            ]
            if pages:
                return pages[0]["webSocketDebuggerUrl"]
            last_err = RuntimeError("no page target in /json/list")
        except OSError as e:
            last_err = e
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    raise ConnectionError(f"CDP discovery failed: {last_err}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", help="fixture URL to render")
    parser.add_argument("--out-prefix", default="/tmp/render", help="writes <prefix>.<fmt> + <prefix>.dom.html")
    parser.add_argument(
        "--format",
        choices=("png", "jpeg"),
        default="png",
        help="screenshot encoding. jpeg is lossy but encodes far cheaper than png's "
        "lossless deflate; the bench measures both to size the artifact-stage effect",
    )
    parser.add_argument(
        "--quality",
        type=int,
        default=80,
        help="jpeg quality 0-100 (ignored for png)",
    )
    parser.add_argument("--cdp-host", default="127.0.0.1:9222")
    parser.add_argument("--timeout", type=float, default=30.0, help="overall deadline (s)")
    parser.add_argument(
        "--idle-wait-ms",
        type=float,
        default=0.0,
        help="wait up to this long for networkIdle after load (0 = capture at load; "
        "see module docstring)",
    )
    parser.add_argument("--then-blank", action="store_true", help="navigate to about:blank after capture (warmup)")
    args = parser.parse_args()

    t0 = time.monotonic()
    deadline = t0 + args.timeout
    stage = "connect"
    try:
        ws = WsClient(find_page_ws_url(args.cdp_host, deadline), deadline)
        cdp = Cdp(ws)
        cdp.cmd("Page.enable", deadline=deadline)
        cdp.cmd("Page.setLifecycleEventsEnabled", {"enabled": True}, deadline=deadline)
        connect_ms = (time.monotonic() - t0) * 1000

        stage = "navigate"
        t_nav = time.monotonic()
        nav = cdp.cmd("Page.navigate", {"url": args.url}, deadline=deadline)
        if "errorText" in nav:
            raise RuntimeError(f"navigation failed: {nav['errorText']}")
        loader = nav.get("loaderId")
        cdp.wait_event(lambda ev: ev["method"] == "Page.loadEventFired", deadline)
        navigate_ms = (time.monotonic() - t_nav) * 1000

        stage = "network-idle"
        t_idle = time.monotonic()
        idle_timeout = 0
        if args.idle_wait_ms > 0:
            try:
                cdp.wait_event(
                    lambda ev: ev["method"] == "Page.lifecycleEvent"
                    and ev["params"].get("name") == "networkIdle"
                    and ev["params"].get("loaderId") == loader,
                    min(deadline, t_idle + args.idle_wait_ms / 1000),
                )
            except TimeoutError:
                idle_timeout = 1  # idle-ish: proceed after the budget
        idle_ms = (time.monotonic() - t_idle) * 1000

        stage = "screenshot"
        t_shot = time.monotonic()
        shot_params = {"format": args.format}
        if args.format == "jpeg":
            shot_params["quality"] = args.quality
        shot = cdp.cmd("Page.captureScreenshot", shot_params, deadline=deadline)
        png = base64.b64decode(shot["data"])
        # Magic-byte check per format: a silently-wrong encoding would make the
        # png-vs-jpeg comparison meaningless.
        magic = b"\x89PNG" if args.format == "png" else b"\xff\xd8\xff"
        if not png.startswith(magic):
            raise RuntimeError(f"captureScreenshot returned non-{args.format.upper()} data")
        png_path = args.out_prefix + ("." + args.format)
        with open(png_path, "wb") as f:
            f.write(png)
        screenshot_ms = (time.monotonic() - t_shot) * 1000

        stage = "dom"
        t_dom = time.monotonic()
        result = cdp.cmd(
            "Runtime.evaluate",
            {"expression": "document.documentElement.outerHTML", "returnByValue": True},
            deadline=deadline,
        )
        dom = result["result"]["value"]
        if not dom:
            raise RuntimeError("empty DOM dump")
        dom_path = args.out_prefix + ".dom.html"
        with open(dom_path, "w") as f:
            f.write(dom)
        dom_ms = (time.monotonic() - t_dom) * 1000

        if args.then_blank:
            stage = "blank"
            cdp.cmd("Page.navigate", {"url": "about:blank"}, deadline=deadline)
            try:
                cdp.wait_event(
                    lambda ev: ev["method"] == "Page.loadEventFired",
                    min(deadline, time.monotonic() + 2),
                )
            except TimeoutError:
                pass  # blank settle is best-effort
        ws.close()

        total_ms = (time.monotonic() - t0) * 1000
        print(
            f"RENDER_OK url={args.url} connect_ms={connect_ms:.1f} "
            f"navigate_ms={navigate_ms:.1f} idle_ms={idle_ms:.1f} idle_timeout={idle_timeout} "
            f"screenshot_ms={screenshot_ms:.1f} dom_ms={dom_ms:.1f} total_ms={total_ms:.1f} "
            f"shot_fmt={args.format} shot_quality={args.quality if args.format == 'jpeg' else 0} "
            f"png_bytes={len(png)} dom_bytes={len(dom)} png={png_path} dom={dom_path}",
            flush=True,
        )
        return 0
    except (OSError, RuntimeError, TimeoutError, WsClosed, KeyError) as e:
        print(f'RENDER_FAIL url={args.url} stage={stage} error="{e}"', flush=True)
        return 1


if __name__ == "__main__":
    sys.exit(main())
