#!/usr/bin/env python3
"""Host-side CDP driver: drive a clone's Chromium over fcvm's port forwarding.

This is the request path. There is no server of ours in it — Chromium's DevTools
Protocol endpoint is already a fully specified request server that returns the
screenshot as base64 inside the CDP response, so the only thing that needs
writing is a client.

    cdpdrive.py 127.0.0.19:9222 http://10.0.1.49:18578/medium.html --format jpeg

Prints one machine-parsable JSON line. Every hop of the connection is timed
separately, because with the resident-render design the connection IS the
per-request setup cost and a lumped number would hide which hop to attack:

    resolve_ms   HTTP GET /json/list  -> which page target to attach to
    tcp_ms       TCP connect to the WebSocket endpoint
    upgrade_ms   RFC 6455 handshake (the 101 exchange)
    enable_ms    Page.enable + Page.setLifecycleEventsEnabled
    navigate_ms  Page.navigate -> Page.loadEventFired
    screenshot_ms Page.captureScreenshot (base64 image arrives here)
    decode_ms    base64 decode + magic/dimension check on the host

`--ws-url` skips `resolve_ms` entirely. The page target id is baked into the
golden snapshot, so every clone restored from it should present the SAME target
id — if that holds, the /json/list lookup can be done ONCE before snapshotting
instead of once per request. `--print-target` dumps the id so the claim can be
checked across clones rather than assumed.

WebSocket framing comes from render.py so the host-driven arm and the in-guest
per-request arm speak CDP through identical code; a divergence there would
silently invalidate the A/B. Only the connect path is reimplemented here, to time
its two halves separately.
"""

import argparse
import base64
import hashlib
import importlib.util
import json
import os
import socket
import struct
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlparse

HERE = os.path.dirname(os.path.abspath(__file__))


def load_render(path: str):
    spec = importlib.util.spec_from_file_location("bench_render", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load render module at {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def image_dimensions(data: bytes) -> tuple[int, int]:
    """Width/height from the encoded bytes — no extra CDP round trip.

    `Page.getLayoutMetrics` would cost a full request/response purely to report a
    number the image already carries, and this driver exists to remove round
    trips. Returns (0, 0) on anything unparsable: a dimension probe must never
    fail a render that succeeded.
    """
    try:
        if data.startswith(b"\x89PNG\r\n\x1a\n"):
            w, h = struct.unpack(">II", data[16:24])  # IHDR is always the first chunk
            return int(w), int(h)
        if data.startswith(b"\xff\xd8"):
            i, n = 2, len(data)
            while i + 9 < n:
                if data[i] != 0xFF:
                    i += 1
                    continue
                marker = data[i + 1]
                if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
                    i += 2
                    continue
                (seglen,) = struct.unpack(">H", data[i + 2 : i + 4])
                if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
                    h, w = struct.unpack(">HH", data[i + 5 : i + 9])
                    return int(w), int(h)
                i += 2 + seglen
    except (struct.error, IndexError):
        pass
    return 0, 0


RESOLVE_RETRY_S = 0.05


class TargetNotReady(ConnectionError):
    """`/json/list` answered, but presented no usable page target in time.

    Distinct from a transport failure ON PURPOSE. `resolve_target` used to raise a
    bare `ConnectionError` whatever the cause, and `drive()` classifies
    `ConnectionError` as `transport` — so "no page target among 3", which is pure
    RESTORE READINESS, inflated the transport-failure count that REVIEW.md's whole
    `WsClosed` diagnosis rests on. Two different defects must not share a bucket.
    """


def resolve_target(cdp_host: str, deadline: float, retries: int,
                   host_header: str = "", stats: dict | None = None) -> dict:
    """GET /json/list and pick the page target. Bounded by the DEADLINE, not by burst.

    Chromium's DevTools HTTP handler validates the Host header and rejects values
    that are neither `localhost` nor an IP literal (a 403 that reads like a
    network fault, not an auth fault). We connect by IP, so urllib's default
    `Host: <ip>:<port>` satisfies the IP-literal branch. `--host-header` overrides
    it, to prove that specific failure mode rather than argue about it.

    IT SLEEPS BETWEEN ATTEMPTS. The loop had none: the `timeout` below is
    urlopen's SOCKET timeout, not an inter-attempt delay, so the retry budget was
    consumed at line rate and the DEADLINE — the variable that actually expresses
    the readiness budget — went unused. Measured on this box against a closed port
    with retries=200 and a 30 s deadline:

        attempts=200  elapsed=40.7 ms  rate=4919 req/s
        retry budget consumed in 0.041s of a 30.0s deadline -> 0.14% of the window

    Three consequences. `resolve_ms` is a PUBLISHED stage, and the burst happens
    inside the measured window. A clone 100 ms from ready is recorded as a hard
    CDP failure, and `reqanalyze` sets `pub = n_bad == 0`, so a single spurious
    exhaustion censors the arm's median. And 4900 req/s of HTTP is aimed straight
    at the socat+pasta relay whose drops are under investigation — an uncontrolled
    variable inside the arm being diagnosed.

    A deterministic 4xx is NOT retried: `urllib.error.HTTPError` subclasses
    `OSError`, so a 403 from the Host-validation branch used to be retried 200
    times as though it were a transient socket error.
    """
    last = None
    attempts = 0
    while True:
        attempts += 1
        if stats is not None:
            stats["resolve_attempts"] = attempts
        try:
            timeout = max(0.05, deadline - time.monotonic())
            req = urllib.request.Request(f"http://{cdp_host}/json/list")
            if host_header:
                req.add_header("Host", host_header)
            with urllib.request.urlopen(req, timeout=timeout) as r:
                targets = json.load(r)
            pages = [
                t
                for t in targets
                if t.get("type") == "page" and not str(t.get("url", "")).startswith("devtools://")
            ]
            if pages:
                return pages[0]
            last = RuntimeError(f"no page target among {len(targets)}")
        except urllib.error.HTTPError as e:
            last = e
            if 400 <= e.code < 500:
                raise ConnectionError(
                    f"CDP target resolution rejected after {attempts} attempt(s): {e} "
                    f"(deterministic {e.code}; not retryable)"
                ) from e
        except OSError as e:
            last = e
        now = time.monotonic()
        if now >= deadline or attempts >= max(1, retries):
            break
        time.sleep(min(RESOLVE_RETRY_S, deadline - now))
    msg = f"CDP target resolution failed after {attempts} attempts: {last}"
    if isinstance(last, RuntimeError):
        # The endpoint ANSWERED; it just had no page yet. Readiness, not transport.
        raise TargetNotReady(msg)
    raise ConnectionError(msg)


class TimedWs:
    """RFC 6455 client whose TCP connect and HTTP upgrade are timed separately.

    Frame codec (`send_text`/`recv_text`/`_recv_exact`/`_send_frame`) is render.py's
    — this class only owns the connect path, because that is the part we need
    split into two numbers.
    """

    def __init__(self, render_mod, ws_url: str, deadline: float):
        u = urlparse(ws_url)
        host, port = u.hostname, u.port or 80

        t = time.monotonic()
        self.sock = socket.create_connection(
            (host, port), timeout=max(0.05, deadline - time.monotonic())
        )
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.tcp_ms = (time.monotonic() - t) * 1000

        t = time.monotonic()
        key = base64.b64encode(os.urandom(16)).decode()
        path = u.path + ("?" + u.query if u.query else "")
        self.sock.sendall(
            (
                f"GET {path} HTTP/1.1\r\n"
                f"Host: {host}:{port}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        self._buf = b""
        response = render_mod.WsClient._recv_until(self, b"\r\n\r\n", deadline)
        status = response.split(b"\r\n", 1)[0]
        if b" 101" not in status:
            raise ConnectionError(f"ws handshake rejected: {status.decode(errors='replace')}")
        expect = base64.b64encode(
            hashlib.sha1((key + render_mod.WS_GUID).encode()).digest()
        )
        if expect not in response:
            raise ConnectionError("ws handshake: bad Sec-WebSocket-Accept")
        self.upgrade_ms = (time.monotonic() - t) * 1000

        # Borrow render.py's frame codec verbatim.
        for m in ("_recv_until", "_recv_exact", "_send_frame", "send_text", "recv_text", "close"):
            setattr(self, m, getattr(render_mod.WsClient, m).__get__(self, TimedWs))


def nav_timing(cdp, deadline: float) -> dict:
    expr = 'JSON.stringify(performance.getEntriesByType("navigation")[0]||{})'
    res = cdp.cmd("Runtime.evaluate", {"expression": expr, "returnByValue": True}, deadline=deadline)
    nav = json.loads(res["result"]["value"])

    def d(a, b):
        return max(0.0, nav.get(a, 0) - nav.get(b, 0))

    tls = d("connectEnd", "secureConnectionStart") if nav.get("secureConnectionStart", 0) else 0.0
    return {
        "dns_ms": d("domainLookupEnd", "domainLookupStart"),
        "connect_ms": d("connectEnd", "connectStart"),
        "tls_ms": tls,
        "ttfb_ms": d("responseStart", "requestStart"),
        "resp_ms": d("responseEnd", "responseStart"),
        "load_ms": nav.get("loadEventEnd", 0.0),
    }


def drive(args) -> dict:
    render = load_render(args.render_module)
    t0 = time.monotonic()
    deadline = t0 + args.timeout
    out: dict = {"ok": False, "cdp_host": args.cdp_host, "url": args.url, "format": args.format}
    stages: dict[str, float] = {}
    stage = "resolve"
    ws = None
    try:
        if args.ws_url:
            ws_url = args.ws_url
            target_id = ""
            stages["resolve_ms"] = 0.0
            out["target_prewired"] = True
        else:
            t = time.monotonic()
            # getattr, not args.host_header: reqbench.run_cdp_request hand-builds
            # an explicit, CLOSED Namespace, and AttributeError is not in this
            # function's except tuple — it would escape drive(), be swallowed by
            # run_cdp_request's `except Exception`, and fail EVERY cdp rep. The
            # Namespace also carries the field now; both halves, deliberately.
            target = resolve_target(args.cdp_host, deadline, args.connect_retries,
                                    getattr(args, "host_header", ""), out)
            ws_url = target["webSocketDebuggerUrl"]
            target_id = target.get("id", "")
            stages["resolve_ms"] = (time.monotonic() - t) * 1000
            out["target_prewired"] = False
        out["target_id"] = target_id
        out["ws_url"] = ws_url

        stage = "connect"
        ws = TimedWs(render, ws_url, deadline)
        stages["tcp_ms"] = ws.tcp_ms
        stages["upgrade_ms"] = ws.upgrade_ms
        cdp = render.Cdp(ws)

        stage = "enable"
        t = time.monotonic()
        cdp.cmd("Page.enable", deadline=deadline)
        if args.idle_wait_ms > 0:
            cdp.cmd("Page.setLifecycleEventsEnabled", {"enabled": True}, deadline=deadline)
        stages["enable_ms"] = (time.monotonic() - t) * 1000
        stages["connect_total_ms"] = (time.monotonic() - t0) * 1000

        stage = "navigate"
        t = time.monotonic()
        nav = cdp.cmd("Page.navigate", {"url": args.url}, deadline=deadline)
        if "errorText" in nav:
            raise RuntimeError(f"navigation failed: {nav['errorText']}")
        loader = nav.get("loaderId")
        cdp.wait_event(lambda ev: ev["method"] == "Page.loadEventFired", deadline)
        stages["navigate_ms"] = (time.monotonic() - t) * 1000

        stage = "network-idle"
        t = time.monotonic()
        out["idle_timeout"] = 0
        if args.idle_wait_ms > 0:
            try:
                cdp.wait_event(
                    lambda ev: ev["method"] == "Page.lifecycleEvent"
                    and ev["params"].get("name") == "networkIdle"
                    and ev["params"].get("loaderId") == loader,
                    min(deadline, t + args.idle_wait_ms / 1000),
                )
            except TimeoutError:
                out["idle_timeout"] = 1
        stages["idle_ms"] = (time.monotonic() - t) * 1000

        stage = "screenshot"
        t = time.monotonic()
        params = {"format": args.format}
        if args.format == "jpeg":
            params["quality"] = args.quality
        shot = cdp.cmd("Page.captureScreenshot", params, deadline=deadline)
        stages["screenshot_ms"] = (time.monotonic() - t) * 1000

        stage = "decode"
        t = time.monotonic()
        raw = base64.b64decode(shot["data"])
        magic = b"\x89PNG" if args.format == "png" else b"\xff\xd8\xff"
        if not raw.startswith(magic):
            raise RuntimeError(f"captureScreenshot returned non-{args.format.upper()} data")
        w, h = image_dimensions(raw)
        out.update(
            image_bytes=len(raw),
            image_sha256=hashlib.sha256(raw).hexdigest(),
            width=w,
            height=h,
            quality=args.quality if args.format == "jpeg" else 0,
        )
        stages["decode_ms"] = (time.monotonic() - t) * 1000
        if args.out_prefix:
            with open(f"{args.out_prefix}.{args.format}", "wb") as f:
                f.write(raw)

        if args.nav_timing:
            stage = "nav-timing"
            t = time.monotonic()
            out["nav"] = nav_timing(cdp, deadline)
            stages["nav_timing_ms"] = (time.monotonic() - t) * 1000

        out["ok"] = True
    except (OSError, RuntimeError, TimeoutError, KeyError, ValueError, render.WsClosed) as e:
        out["error"] = f"{type(e).__name__}: {e}"
        out["stage"] = stage
        # Classify, so downstream can gate on it instead of substring-matching the
        # message. A `WsClosed` is the PEER closing the TCP connection — it is NOT
        # this driver's own timeout (render.py raises TimeoutError for that), so a
        # failure at ~5 s against a 120 s deadline is a real transport drop in the
        # socat+pasta relay this arm adds and the exec arm does not have.
        #
        # `readiness` is checked FIRST and is its own bucket: TargetNotReady
        # subclasses ConnectionError, so folding it into `transport` would keep
        # inflating the count REVIEW.md's WsClosed investigation depends on with
        # clones that were merely still restoring.
        out["failure_class"] = (
            "readiness" if isinstance(e, TargetNotReady)
            else "transport" if isinstance(e, (render.WsClosed, ConnectionError))
            else "render"
        )
    finally:
        if ws is not None:
            try:
                ws.close()
            except OSError:
                pass
    stages["total_ms"] = (time.monotonic() - t0) * 1000
    out["stages"] = stages
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("cdp_host", help="clone's forwarded CDP endpoint, e.g. 127.0.0.19:9222")
    p.add_argument("url", help="fixture URL for the guest to render")
    p.add_argument("--format", choices=("png", "jpeg"), default="png")
    p.add_argument("--quality", type=int, default=80)
    p.add_argument("--timeout", type=float, default=30.0)
    p.add_argument("--idle-wait-ms", type=float, default=0.0)
    p.add_argument("--out-prefix", default="", help="also write the image to <prefix>.<fmt>")
    p.add_argument("--ws-url", default="", help="pre-resolved target; skips /json/list")
    p.add_argument("--connect-retries", type=int, default=200)
    p.add_argument("--nav-timing", action="store_true")
    p.add_argument("--print-target", action="store_true",
                   help="print the page target id and exit; nothing else is done")
    p.add_argument("--host-header", default="",
                   help="override the Host header on /json/list; proves Chromium's "
                        "DevTools host validation rejects non-IP, non-localhost names")
    p.add_argument("--render-module", default=os.path.join(HERE, "render.py"))
    args = p.parse_args()
    if args.print_target:
        # The docstring above has promised this flag since the file was written,
        # and argparse did not have it — so `--print-target` exited 2 with
        # "unrecognized arguments", and NEITHER of the two places that claim the
        # target id can be checked across clones could actually check it.
        # `--host-header` was the SECOND claim in the same docstring and survived
        # that round untouched, because the fix did not audit the paragraph it was
        # editing for the same class of promise.
        target = resolve_target(args.cdp_host, time.monotonic() + args.timeout,
                                args.connect_retries, args.host_header)
        print(target.get("id", ""), flush=True)
        return 0
    out = drive(args)
    print(json.dumps(out, separators=(",", ":")), flush=True)
    return 0 if out["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
