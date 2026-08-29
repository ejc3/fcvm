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
    navigate_command_ms  Page.navigate send -> command response
    navigate_load_event_ms command response -> Page.loadEventFired
    navigate_ms  Sum of the two navigation phases above
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

`--net-trace PATH` is a diagnostic, not a measurement. It sends `Network.enable`
before the navigate, collects requestWillBeSent / requestServedFromCache /
responseReceived / loadingFinished / loadingFailed until Page.loadEventFired,
keeps draining for `--net-trace-drain-ms` (default 5000) so requests still
open at the load event show how they end, and writes PATH whole (temp file,
then rename) as
{"requests": [...], "summary": {...}}. The record gains a "net_trace" key
holding the summary; a PATH that could not be written puts "net_trace_error"
on the record and exits 1. Without the flag not one extra CDP message is
sent; test_net_trace.py pins the wire sequence.
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
from collections import Counter
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
    at the request path being measured — an uncontrolled load generated by the
    measurement itself.

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


def socket_diagnostics(sock) -> dict:
    """The three fields that identify WHICH connection failed.

    Each is read independently: a socket can answer getsockname() and not
    getpeername() (never connected), and every one of them raises once the
    socket is closed — so this must run while it is still open.
    """
    out = {}
    try:
        out["socket_local"] = list(sock.getsockname())
    except OSError:
        pass
    try:
        out["socket_peer"] = list(sock.getpeername())
    except OSError:
        pass
    try:
        out["socket_so_error"] = sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
    except OSError:
        pass
    return out


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
        # A connect failure has no socket to describe. Everything after this
        # line does, and the handler in drive() reads the diagnostics off the
        # EXCEPTION rather than off `ws` — the name `ws` is only bound once the
        # constructor returns, so an upgrade failure would otherwise report a
        # WebSocket failure with no socket_local/socket_peer/socket_so_error.
        self.sock = socket.create_connection(
            (host, port), timeout=max(0.05, deadline - time.monotonic())
        )
        try:
            self._handshake(render_mod, u, host, port, deadline, t)
        except BaseException as e:
            # Read the diagnostics off the LIVE socket and attach the values,
            # not the socket: closing it first makes every getsockname/
            # getpeername/getsockopt raise EBADF, which drive()'s handler
            # swallows, so handing over a closed socket reports nothing at all.
            # Then close, because an abandoned CLOSE_WAIT socket per failed rep
            # is a leak.
            e.fcvm_socket_diagnostics = socket_diagnostics(self.sock)
            try:
                self.sock.close()
            except OSError:
                pass
            raise

    def _handshake(self, render_mod, u, host, port, deadline: float, t: float):
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


def transport_signal(error: BaseException, render_mod) -> str:
    """Classify the observable close signal without guessing its network hop."""
    if isinstance(error, ConnectionResetError) or getattr(error, "errno", None) == 104:
        return "tcp-rst"
    if isinstance(error, BrokenPipeError) or getattr(error, "errno", None) == 32:
        return "tcp-write-closed"
    if isinstance(error, render_mod.WsClosed):
        if "close frame" in str(error):
            return "websocket-close-frame"
        # recv() returned EOF. This proves an orderly TCP close reached the
        # client, but not whether Chromium, pasta, or another hop originated it.
        return "tcp-eof"
    if isinstance(error, TimeoutError):
        return "local-deadline"
    if isinstance(error, OSError):
        return "socket-os-error"
    return "not-transport"


def _timing_ms(timing: dict, start_key: str, end_key: str):
    """A ResourceTiming interval in ms, or None when Chromium reports -1 (not applicable)."""
    start, end = timing.get(start_key, -1), timing.get(end_key, -1)
    if not isinstance(start, (int, float)) or not isinstance(end, (int, float)):
        return None
    if start < 0 or end < 0:
        return None
    return round(end - start, 3)


def reduce_net_trace(events: list, load_ts, drain_ms: float) -> dict:
    """Fold one navigation's Network.* events into per-request rows and a summary.

    Times are browser-clock seconds (Network.MonotonicTime), reported in ms
    relative to the earliest requestWillBeSent, which is the navigation's own
    document request and so sits inside the navigate command's round trip.
    `load_ts` is Page.loadEventFired's timestamp on the same clock, or None
    when the event never arrived; then "pending at load" means still open when
    observation ended, which is the question a stalled load asks. A redirect
    re-announces its requestId with the next url; the row keeps its first url
    and start and counts the hop. The summary ranks slowest_10 by duration
    (end_ms minus start_ms), not by completion time, and lists the rows that
    had no end before the load event as pending_at_load, capped at ten each.
    """
    rows: dict = {}
    order: list = []
    for event in events:
        method = event.get("method", "")
        params = event.get("params", {})
        rid = params.get("requestId")
        if not method.startswith("Network.") or rid is None:
            continue
        row = rows.get(rid)
        if method == "Network.requestWillBeSent":
            if row is None:
                rows[rid] = {
                    "request_id": rid,
                    "url": params.get("request", {}).get("url", ""),
                    "start_ts": params.get("timestamp"),
                    "end_ts": None,
                    "remote_ip": "",
                    "remote_port": None,
                    "protocol": "",
                    "status": None,
                    "timing": None,
                    "failed": False,
                    "canceled": False,
                    "error_text": "",
                    "redirects": 0,
                    # Why a row can have no remote address: a cache or a
                    # service worker answered it, so there was no network hop
                    # to name one. Without these the diag cannot tell such a
                    # row from a request that never got a response.
                    "from_cache": False,
                    "from_service_worker": False,
                }
                order.append(rid)
            else:
                row["redirects"] += 1
            continue
        if row is None:
            continue  # announced before Network.enable; nothing to attach to
        if method == "Network.requestServedFromCache":
            # The memory cache. It arrives before the response, which then
            # carries fromDiskCache false and no remote address.
            row["from_cache"] = True
        elif method == "Network.responseReceived":
            response = params.get("response", {})
            row.update(
                remote_ip=response.get("remoteIPAddress", ""),
                remote_port=response.get("remotePort"),
                protocol=response.get("protocol", ""),
                status=response.get("status"),
                timing=response.get("timing"),
                from_cache=(
                    row["from_cache"]
                    or bool(response.get("fromDiskCache"))
                    or bool(response.get("fromPrefetchCache"))
                ),
                from_service_worker=bool(response.get("fromServiceWorker")),
            )
        elif method == "Network.loadingFinished":
            if row["end_ts"] is None:
                row["end_ts"] = params.get("timestamp")
        elif method == "Network.loadingFailed":
            if row["end_ts"] is None:
                row["end_ts"] = params.get("timestamp")
            row["failed"] = True
            row["canceled"] = bool(params.get("canceled", False))
            row["error_text"] = params.get("errorText", "")

    starts = [r["start_ts"] for r in rows.values() if r["start_ts"] is not None]
    base = min(starts) if starts else None

    def rel_ms(ts):
        if ts is None or base is None:
            return None
        return round((ts - base) * 1000, 3)

    requests = []
    for rid in order:
        r = rows[rid]
        ended = r["end_ts"] is not None
        finished_before_load = (
            ended and not r["failed"] and load_ts is not None and r["end_ts"] <= load_ts
        )
        started_by_load = load_ts is None or (
            r["start_ts"] is not None and r["start_ts"] <= load_ts
        )
        pending_at_load = started_by_load and (
            not ended or (load_ts is not None and r["end_ts"] > load_ts)
        )
        timing = r["timing"] if isinstance(r["timing"], dict) else {}
        start_ms = rel_ms(r["start_ts"])
        end_ms = rel_ms(r["end_ts"])
        duration_ms = None
        if start_ms is not None and end_ms is not None:
            duration_ms = round(end_ms - start_ms, 3)
        requests.append({
            "request_id": rid,
            "url": r["url"],
            "start_ms": start_ms,
            "end_ms": end_ms,
            "duration_ms": duration_ms,
            "remote_ip": r["remote_ip"],
            "remote_port": r["remote_port"],
            "from_cache": r["from_cache"],
            "from_service_worker": r["from_service_worker"],
            "protocol": r["protocol"],
            "status": r["status"],
            "finished_before_load": finished_before_load,
            "pending_at_load": pending_at_load,
            "failed": r["failed"],
            "canceled": r["canceled"],
            "error_text": r["error_text"],
            "redirects": r["redirects"],
            "dns_ms": _timing_ms(timing, "dnsStart", "dnsEnd"),
            "connect_ms": _timing_ms(timing, "connectStart", "connectEnd"),
            "timing": r["timing"],
        })

    remote_ips = Counter(row["remote_ip"] for row in requests if row["remote_ip"])
    errors = Counter(row["error_text"] for row in requests if row["failed"])
    # By duration, not by end_ms: end_ms is completion relative to the first
    # request, so a request that started late and finished fast would outrank
    # an earlier, longer one. Ties go to the earlier start.
    slowest = sorted(
        (row for row in requests if row["duration_ms"] is not None),
        key=lambda row: (-row["duration_ms"], row["start_ms"]),
    )
    # What held the load, earliest start first: the rows a stall
    # investigation reads before any completed one.
    pending = sorted(
        (row for row in requests if row["pending_at_load"]),
        key=lambda row: row["start_ms"] if row["start_ms"] is not None else float("inf"),
    )

    def by_count(item):
        return (-item[1], item[0])

    summary = {
        "n_requests": len(requests),
        "n_finished_before_load": sum(row["finished_before_load"] for row in requests),
        "n_failed": sum(row["failed"] for row in requests),
        "n_pending_at_load": sum(row["pending_at_load"] for row in requests),
        "remote_ips": dict(sorted(remote_ips.items(), key=by_count)),
        "errors": dict(sorted(errors.items(), key=by_count)),
        "slowest_10": [{"url": row["url"], "end_ms": row["end_ms"],
                        "duration_ms": row["duration_ms"]} for row in slowest[:10]],
        "pending_at_load": [{"url": row["url"], "start_ms": row["start_ms"]}
                            for row in pending[:10]],
        "load_event_ms": rel_ms(load_ts),
        "drain_ms": drain_ms,
    }
    return {"requests": requests, "summary": summary}


def drive(args) -> dict:
    render = load_render(args.render_module)
    # getattr, for the same reason as host_header below: reqbench builds a
    # closed Namespace. A missing attribute or None means the trace is off,
    # and off sends nothing the untraced path does not send.
    trace_path = getattr(args, "net_trace", None)
    trace_drain_ms = float(getattr(args, "net_trace_drain_ms", 5000.0))
    t0 = time.monotonic()
    deadline = t0 + args.timeout
    out: dict = {"ok": False, "cdp_host": args.cdp_host, "url": args.url, "format": args.format}
    stages: dict[str, float] = {}
    stage = "resolve"
    stage_started = t0
    ws = None
    cdp = None
    load_ts = None
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
        stage_started = time.monotonic()
        ws = TimedWs(render, ws_url, deadline)
        stages["tcp_ms"] = ws.tcp_ms
        stages["upgrade_ms"] = ws.upgrade_ms
        cdp = render.Cdp(ws)

        stage = "enable"
        stage_started = time.monotonic()
        t = time.monotonic()
        cdp.cmd("Page.enable", deadline=deadline)
        if args.idle_wait_ms > 0:
            cdp.cmd("Page.setLifecycleEventsEnabled", {"enabled": True}, deadline=deadline)
        if trace_path:
            # The one message the trace adds before the navigate. Measured
            # arms never set the flag and stay byte-identical on the wire.
            cdp.cmd("Network.enable", deadline=deadline)
        stages["enable_ms"] = (time.monotonic() - t) * 1000
        stages["connect_total_ms"] = (time.monotonic() - t0) * 1000

        stage = "navigate-command-response"
        stage_started = time.monotonic()
        t = stage_started
        nav = cdp.cmd("Page.navigate", {"url": args.url}, deadline=deadline)
        stages["navigate_command_ms"] = (time.monotonic() - stage_started) * 1000
        if "errorText" in nav:
            raise RuntimeError(f"navigation failed: {nav['errorText']}")
        loader = nav.get("loaderId")

        stage = "navigate-load-event"
        stage_started = time.monotonic()
        load_event = cdp.wait_event(lambda ev: ev["method"] == "Page.loadEventFired", deadline)
        stages["navigate_load_event_ms"] = (time.monotonic() - stage_started) * 1000
        stages["navigate_ms"] = (time.monotonic() - t) * 1000

        if trace_path:
            load_ts = load_event.get("params", {}).get("timestamp")
            stage = "net-trace-drain"
            stage_started = time.monotonic()
            t = stage_started
            if trace_drain_ms > 0:
                # Keep reading so requests still open at the load event show
                # how they end. wait_event stashes every event it reads; the
                # bound is the socket timeout, the same mechanism as the idle
                # wait above.
                try:
                    cdp.wait_event(
                        lambda _ev: False, min(deadline, t + trace_drain_ms / 1000)
                    )
                except TimeoutError:
                    pass
            stages["net_trace_drain_ms"] = (time.monotonic() - t) * 1000

        stage = "network-idle"
        stage_started = time.monotonic()
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

        if getattr(args, "op", "screenshot") == "html":
            # HTML-extraction op: the page's serialized DOM instead of pixels —
            # the second operation Kitesurf's table prices. Same connect and
            # navigate path; the terminal stage swaps.
            stage = "extract"
            stage_started = time.monotonic()
            t = time.monotonic()
            result = cdp.cmd(
                "Runtime.evaluate",
                {
                    "expression": "document.documentElement.outerHTML",
                    "returnByValue": True,
                },
                deadline=deadline,
            )
            html = result.get("result", {}).get("value")
            if not isinstance(html, str) or "</html>" not in html.lower():
                raise RuntimeError(
                    "outerHTML extraction returned no closed document "
                    f"({type(html).__name__}, {len(html) if isinstance(html, str) else 0} chars)"
                )
            encoded = html.encode()
            out.update(
                html_bytes=len(encoded),
                html_sha256=hashlib.sha256(encoded).hexdigest(),
            )
            stages["extract_ms"] = (time.monotonic() - t) * 1000
            if args.out_prefix:
                with open(f"{args.out_prefix}.html", "w") as f:
                    f.write(html)
        else:
            stage = "screenshot"
            stage_started = time.monotonic()
            t = time.monotonic()
            params = {"format": args.format}
            if args.format == "jpeg":
                params["quality"] = args.quality
            shot = cdp.cmd("Page.captureScreenshot", params, deadline=deadline)
            stages["screenshot_ms"] = (time.monotonic() - t) * 1000

            stage = "decode"
            stage_started = time.monotonic()
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
            stage_started = time.monotonic()
            t = time.monotonic()
            out["nav"] = nav_timing(cdp, deadline)
            stages["nav_timing_ms"] = (time.monotonic() - t) * 1000

        out["ok"] = True
    except (OSError, RuntimeError, TimeoutError, KeyError, ValueError, render.WsClosed) as e:
        out["error"] = f"{type(e).__name__}: {e}"
        out["stage"] = stage
        out["failure_operation"] = {
            "navigate-command-response": "Page.navigate response",
            "navigate-load-event": "Page.loadEventFired wait",
        }.get(stage, stage)
        out["failure_phase_elapsed_ms"] = (time.monotonic() - stage_started) * 1000
        out["transport_signal"] = transport_signal(e, render)
        if isinstance(e, OSError):
            out["socket_errno"] = e.errno
        # `ws` is bound only after the constructor RETURNS, so an upgrade-phase
        # failure leaves it None. The constructor reads the same three fields
        # off its live socket and attaches them to the exception for that case.
        sock = getattr(ws, "sock", None)
        if sock is not None:
            out.update(socket_diagnostics(sock))
        else:
            out.update(getattr(e, "fcvm_socket_diagnostics", {}))
        # Classify, so downstream can gate on it instead of substring-matching the
        # message. A `WsClosed` is the PEER closing the TCP connection — it is NOT
        # this driver's own timeout (render.py raises TimeoutError for that), so a
        # failure at ~5 s against a 120 s deadline is a peer-side transport close,
        # not this driver's deadline. The withdrawn earlier A/B did not identify
        # which component closed it, and the current path has no socat relay.
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
        if trace_path and cdp is not None:
            # Written on failure too: a load event that never fired is the
            # case the trace exists for. A filesystem error goes on the record
            # rather than out of drive(), which never raises for its own
            # faults; main() then exits non-zero, since a trace that was
            # asked for and not written is a failed invocation. The file is
            # renamed into place whole: a reader never sees a truncated one.
            trace = reduce_net_trace(cdp.events, load_ts, trace_drain_ms)
            temp_path = f"{trace_path}.{os.getpid()}.tmp"
            try:
                with open(temp_path, "w") as f:
                    json.dump(trace, f, separators=(",", ":"))
                    f.write("\n")
                os.replace(temp_path, trace_path)
                out["net_trace"] = trace["summary"]
            except OSError as e:
                out["net_trace_error"] = f"{type(e).__name__}: {e}"
                try:
                    os.unlink(temp_path)
                except OSError:
                    pass
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
    p.add_argument("--net-trace", default=None, metavar="PATH",
                   help="write the navigation's Network.* events as per-request rows plus "
                        "a summary to PATH; adds Network.enable and a post-load drain, "
                        "so never on a measured arm")
    p.add_argument("--net-trace-drain-ms", type=float, default=5000.0,
                   help="with --net-trace: keep collecting events this long after "
                        "Page.loadEventFired")
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
    return 0 if out["ok"] and "net_trace_error" not in out else 1


if __name__ == "__main__":
    sys.exit(main())
