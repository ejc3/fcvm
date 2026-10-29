#!/usr/bin/env python3
"""Prove Access, fresh launch names that resolve at once, and renderer frames over IPv4 on staging."""
import base64
import hashlib
import ipaddress
import json
import os
import re
import socket
import ssl
import struct
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path


def ipv4(host, port):
    """Use the same address family as an IPv4-only viewer; reject private endpoints."""
    addresses = socket.getaddrinfo(host, port, socket.AF_INET, socket.SOCK_STREAM)
    for _, _, _, _, address in addresses:
        if not ipaddress.ip_address(address[0]).is_global:
            continue
        try:
            raw = socket.create_connection(address, timeout=20)
            return ssl.create_default_context().wrap_socket(raw, server_hostname=host)
        except OSError:
            continue
    raise RuntimeError("no reachable public IPv4 endpoint")


class WebSocketClosed(RuntimeError):
    """The peer closed the connection, distinct from a malformed WebSocket message."""


class Socket:
    def __init__(self, url, headers=None):
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme != "wss" or parsed.username or parsed.password or parsed.fragment:
            raise ValueError("a secure WebSocket URL is required")
        self.s = ipv4(parsed.hostname, parsed.port or 443)
        key = base64.b64encode(os.urandom(16)).decode()
        authority = parsed.netloc
        extra = ""
        for name, value in (headers or {}).items():
            if any(c in name + value for c in "\r\n"):
                raise ValueError("invalid header")
            extra += name + ": " + value + "\r\n"
        target = (parsed.path or "/") + ("?" + parsed.query if parsed.query else "")
        self.s.sendall((f"GET {target} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n").encode())
        head = b""
        while not head.endswith(b"\r\n\r\n"):
            chunk = self.s.recv(1)
            if not chunk or len(head) > 16384:
                raise RuntimeError("invalid WebSocket upgrade")
            head += chunk
        lines = head.decode().split("\r\n")
        accepted = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        response = {k.lower(): v.strip() for k, v in (line.split(":", 1) for line in lines[1:] if ":" in line)}
        if " 101 " not in lines[0] or response.get("sec-websocket-accept") != accepted:
            raise RuntimeError("WebSocket upgrade refused")

    def exact(self, count, deadline):
        buf = b""
        while len(buf) < count:
            if time.monotonic() >= deadline:
                raise TimeoutError("WebSocket response timeout")
            self.s.settimeout(min(20, max(0.1, deadline - time.monotonic())))
            try:
                part = self.s.recv(count - len(buf))
            except socket.timeout:
                continue
            if not part:
                raise WebSocketClosed("WebSocket closed")
            buf += part
        return buf

    def send(self, payload, opcode=1):
        if isinstance(payload, dict):
            payload = json.dumps(payload).encode()
        mask = os.urandom(4)
        n = len(payload)
        size = bytes([0x80 | n]) if n < 126 else b"\xfe" + struct.pack(">H", n)
        self.s.sendall(bytes([0x80 | opcode]) + size + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def message(self, deadline):
        data, first_opcode = b"", None
        while True:
            a, b = self.exact(2, deadline)
            opcode, n = a & 15, b & 127
            if b & 128:
                raise RuntimeError("masked server frame")
            if n == 126:
                n = struct.unpack(">H", self.exact(2, deadline))[0]
            elif n == 127:
                n = struct.unpack(">Q", self.exact(8, deadline))[0]
            if n + len(data) > 8 * 1024 * 1024:
                raise RuntimeError("oversized WebSocket message")
            payload = self.exact(n, deadline)
            if opcode == 8:
                raise WebSocketClosed("WebSocket closed")
            if opcode == 9:
                self.send(payload, 10)
                continue
            if opcode == 10:
                continue
            if opcode in (1, 2):
                if first_opcode is not None:
                    raise RuntimeError("interleaved WebSocket messages")
                first_opcode = opcode
            elif opcode != 0 or first_opcode is None:
                raise RuntimeError("invalid WebSocket continuation")
            data += payload
            if a & 128:
                return first_opcode, data

    def close(self):
        self.s.close()


def frame_host(url):
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "wss" or not parsed.hostname or not parsed.hostname.endswith(".fcvm.dev")
            or parsed.username or parsed.password or parsed.path != "/frames" or parsed.query or parsed.fragment
            or (parsed.port or 443) not in (443, *range(20200, 20208))):
        raise ValueError("unexpected frame endpoint")
    return parsed.hostname


def prove_frame(tile, admit=None):
    """A DNS record alone is insufficient: require a binary renderer packet over IPv4/TLS.
    With admission on, the direct token goes as the first message (never in the URL)."""
    url = tile["frame_url_v4"]
    frame_host(url)
    connection = Socket(url + "?h=180")  # No Access credentials are ever sent to a renderer.
    try:
        if admit is not None:
            connection.send({"admit": admit})
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            opcode, payload = connection.message(deadline)
            if opcode == 2 and payload:
                return
        raise TimeoutError("renderer sent no binary frames")
    finally:
        connection.close()


def prove_refused(tile):
    """With admission on, a socket that sends no direct token gets no frame: the renderer
    closes it (1008) once its 5 s wait for the token ends."""
    url = tile["frame_url_v4"]
    frame_host(url)
    connection = Socket(url + "?h=180")
    try:
        deadline = time.monotonic() + 15
        while True:
            try:
                opcode, payload = connection.message(deadline)
            except WebSocketClosed:
                return
            except TimeoutError:
                raise AssertionError("a socket without a direct token was left open")
            if opcode == 2:
                raise AssertionError("a renderer streamed to a socket without a direct token")
    finally:
        connection.close()


def ready(state, half, *, slot=None, after=None):
    for tile in state.get("tiles", []):
        if (tile.get("half") == half and tile.get("phase") == "running"
                and tile.get("frame_url") and tile.get("frame_url_v4")
                and (slot is None or tile.get("slot") == slot)
                and (after is None or (tile.get("generation") or 0) > after)):
            return tile
    return None


def fresh(old, new):
    if new.get("generation", 0) <= old.get("generation", 0):
        raise AssertionError("relaunch did not advance the generation")
    for key in ("frame_url", "frame_url_v4"):
        if frame_host(old[key]) == frame_host(new[key]):
            raise AssertionError("a launch reused its predecessor's DNS name")


# A fresh launch name must answer at each of these on its first query, within RESOLVE_WITHIN
# seconds including the resolver's walk of the delegation: the coordinator serves the names
# itself and publishes one only once it answers.
PUBLIC_RESOLVERS = ("1.1.1.1", "8.8.8.8", "9.9.9.9")
RESOLVE_WITHIN = 2.0
DNS_PORT = 53


def dns_query(name, qtype):
    """A recursive query for name (RD set, EDNS0 with a 1232-byte payload); returns (id, packet)."""
    ident = os.urandom(2)
    packet = ident + b"\x01\x00\x00\x01\x00\x00\x00\x00\x00\x01"
    for label in name.rstrip(".").split("."):
        if not 0 < len(label) < 64:
            raise ValueError("invalid DNS name")
        packet += bytes([len(label)]) + label.encode("ascii")
    packet += b"\x00" + struct.pack(">HH", qtype, 1)
    packet += b"\x00" + struct.pack(">HHIH", 41, 1232, 0, 0)
    return ident, packet


def skip_name(m, i):
    for _ in range(128):
        if i >= len(m):
            raise ValueError("short DNS reply")
        n = m[i]
        if n & 0xC0 == 0xC0:
            return i + 2
        if n == 0:
            return i + 1
        if n > 63:
            raise ValueError("invalid DNS name")
        i += 1 + n
    raise ValueError("invalid DNS name")


def addresses(reply, ident, query, qtype):
    """The qtype addresses of a resolver's reply to query; a failed, truncated or foreign reply raises."""
    if len(reply) < 12 or reply[:2] != ident:
        raise ValueError("not the reply to this query")
    flags, qd, an = struct.unpack(">HHH", reply[2:8])
    if not flags & 0x8000 or flags & 0x0200 or flags & 0xF:
        raise AssertionError("the resolver did not answer the name")
    question = query[12:len(query) - 11]
    if qd != 1 or reply[12:12 + len(question)].lower() != question.lower():
        raise ValueError("the reply is for another question")
    i, found = 12 + len(question), []
    for _ in range(an):
        i = skip_name(reply, i)
        if i + 10 > len(reply):
            raise ValueError("short DNS reply")
        kind, _, _, n = struct.unpack(">HHIH", reply[i:i + 10])
        rdata = reply[i + 10:i + 10 + n]
        if len(rdata) != n:
            raise ValueError("short DNS reply")
        if kind == qtype and n in (4, 16):
            found.append(ipaddress.ip_address(rdata))
        i += 10 + n
    return found


def resolve(resolver, name, qtype, timeout):
    """One query to a public resolver; the addresses it answers within timeout seconds."""
    ident, query = dns_query(name, qtype)
    deadline = time.monotonic() + timeout
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.connect((resolver, DNS_PORT))
        s.send(query)
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("a public resolver did not answer in time")
            s.settimeout(left)
            try:
                reply = s.recv(4096)
            except socket.timeout:
                continue
            if reply[:2] == ident:
                return addresses(reply, ident, query, qtype)


def prove_resolves(tile):
    """Both of a fresh launch's names answer at every public resolver on the first query; the slowest, in s."""
    slowest = 0.0
    for key, qtype in (("frame_url", 28), ("frame_url_v4", 1)):
        host = frame_host(tile[key])
        for resolver in PUBLIC_RESOLVERS:
            start = time.monotonic()
            found = resolve(resolver, host, qtype, RESOLVE_WITHIN)
            took = time.monotonic() - start
            if not found or took > RESOLVE_WITHIN:
                raise AssertionError("a fresh launch name did not resolve on its first query")
            slowest = max(slowest, took)
    return slowest


def relaunch_interval_seconds():
    # The image and this probe come from the same commit. Read its server default
    # instead of copying a rate limit that could drift when the core changes.
    source = (Path(__file__).resolve().parent.parent / "core/src/state.rs").read_text()
    defaults = source.split("impl Default for Config", 1)[1]
    value = re.search(r"relaunch_interval_ms:\s*([0-9_]+)", defaults)
    if value is None:
        raise ValueError("coordinator relaunch interval unavailable")
    return int(value[1].replace("_", "")) / 1000


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def get(url, headers=None):
    opener = urllib.request.build_opener(NoRedirect)
    request_headers = {"User-Agent": "fcvm-stage-smoke/1", **(headers or {})}
    try:
        with opener.open(urllib.request.Request(url, headers=request_headers), timeout=30) as response:
            return response.status, response.headers, response.read(65536)
    except urllib.error.HTTPError as error:
        return error.code, error.headers, b""


def main():
    api, origin = os.environ["STAGING_API_URL"].rstrip("/"), os.environ["STAGING_ORIGIN"].rstrip("/")
    headers = {"CF-Access-Client-Id": os.environ["CF_ACCESS_CLIENT_ID"],
               "CF-Access-Client-Secret": os.environ["CF_ACCESS_CLIENT_SECRET"], "Origin": origin}
    for url in (api + "/healthz", origin + "/"):
        code, response_headers, _ = get(url)
        location = urllib.parse.urlsplit(response_headers.get("Location", ""))
        if code != 302 or location.scheme != "https" or not (location.hostname or "").endswith(".cloudflareaccess.com"):
            raise AssertionError("staging did not require Access")
        code, _, _ = get(url, headers)
        if code != 200:
            raise AssertionError("authenticated staging request failed")
    code, _, _ = get(api + "/agent", headers)
    if code not in (401, 403, 302):
        raise AssertionError("the CI token reached the agents-only endpoint")
    connection = Socket(api.replace("https://", "wss://", 1) + "/live", headers)
    try:
        # The first message on /live: a page's seat ticket (none here).
        connection.send({"type": "seat", "ticket": None})
        deadline = time.monotonic() + 720
        interval, next_relaunch = relaunch_interval_seconds(), 0.0
        state, admission = {}, {}
        seat, reconnects = None, 0
        def take(message):
            nonlocal state, admission, seat
            if message.get("type") == "snapshot":
                state = message["state"]
            elif message.get("type") == "admission":
                admission = message
                seat = message.get("seat")
        def until(predicate):
            nonlocal connection, state, admission, reconnects
            while time.monotonic() < deadline:
                if admission and predicate(state):
                    return state
                try:
                    opcode, payload = connection.message(deadline)
                except WebSocketClosed:
                    if reconnects >= 3 or time.monotonic() >= deadline:
                        raise
                    # A readiness wait can resume; an intent awaiting its receipt cannot.
                    connection.close()
                    state, admission = {}, {}
                    reconnects += 1
                    connection = Socket(api.replace("https://", "wss://", 1) + "/live", headers)
                    connection.send({"type": "seat", "ticket": seat})
                    print(json.dumps({"reconnect": reconnects}), flush=True)
                    continue
                if opcode != 1:
                    continue
                take(json.loads(payload))
            raise TimeoutError("staging did not reach the required state")
        def direct():
            """The direct token, when the coordinator gates direct streams (else None)."""
            if admission.get("capacity") is None:
                return None
            token = (admission.get("direct") or {}).get("value")
            if not token:
                raise AssertionError("the smoke connection was given no direct token")
            return token
        for half in ("fcvm", "ec2"):
            until(lambda s: ready(s, half))
            old = dict(ready(state, half))
            if direct() is not None:
                prove_refused(old)
            prove_frame(old, direct())
            delay = next_relaunch - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            intent = str(uuid.uuid4())
            connection.send({"type": "intent", "id": intent, "name": "relaunch", "args": {"tile": old["id"]}})
            # A refused intent must fail explicitly, rather than wait for an unrelated scheduled race.
            while True:
                opcode, payload = connection.message(deadline)
                if opcode != 1:
                    continue
                message = json.loads(payload)
                take(message)
                if message.get("type") == "receipt" and message.get("id") == intent:
                    if message.get("outcome") != "committed":
                        raise AssertionError("staging refused the relaunch")
                    # Start the interval at the receipt, after the server accepted
                    # the intent, so varying network latency cannot shorten it.
                    next_relaunch = time.monotonic() + interval
                    break
            until(lambda s: ready(s, half, slot=old["slot"], after=old["generation"]))
            new = ready(state, half, slot=old["slot"], after=old["generation"])
            fresh(old, new)
            # Before anything else looks the names up: each resolver's first query.
            slowest = prove_resolves(new)
            prove_frame(new, direct())
            print(json.dumps({"half": half, "freshLaunchNames": True, "freshNamesResolved": True,
                              "resolveMsMax": round(slowest * 1000), "ipv4RendererFrames": True,
                              "directAdmission": direct() is not None}), flush=True)
    finally:
        connection.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"status": "failed", "errorType": type(error).__name__}))
        raise SystemExit(1)
