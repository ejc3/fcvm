#!/usr/bin/env python3
"""Prove Access, fresh launch names and real renderer frames over IPv4 on staging."""
import base64
import hashlib
import ipaddress
import json
import os
import socket
import ssl
import struct
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


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
        self.s.sendall((f"GET {parsed.path or '/'} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n"
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
                raise RuntimeError("WebSocket closed")
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
                raise RuntimeError("WebSocket closed")
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


def prove_frame(tile):
    """A DNS record alone is insufficient: require a binary renderer packet over IPv4/TLS."""
    url = tile["frame_url_v4"]
    frame_host(url)
    connection = Socket(url)  # No Access credentials are ever sent to a renderer.
    try:
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            opcode, payload = connection.message(deadline)
            if opcode == 2 and payload:
                return
        raise TimeoutError("renderer sent no binary frames")
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


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def get(url, headers=None):
    opener = urllib.request.build_opener(NoRedirect)
    try:
        with opener.open(urllib.request.Request(url, headers=headers or {}), timeout=30) as response:
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
        deadline = time.monotonic() + 720
        state = {}
        def until(predicate):
            nonlocal state
            while time.monotonic() < deadline:
                if predicate(state):
                    return state
                opcode, payload = connection.message(deadline)
                if opcode != 1:
                    continue
                message = json.loads(payload)
                if message.get("type") == "snapshot":
                    state = message["state"]
            raise TimeoutError("staging did not reach the required state")
        for half in ("fcvm", "ec2"):
            until(lambda s: ready(s, half))
            old = dict(ready(state, half))
            prove_frame(old)
            intent = str(uuid.uuid4())
            connection.send({"type": "intent", "id": intent, "name": "relaunch", "args": {"tile": old["id"]}})
            # A refused intent must fail explicitly, rather than wait for an unrelated scheduled race.
            while True:
                opcode, payload = connection.message(deadline)
                if opcode != 1:
                    continue
                message = json.loads(payload)
                if message.get("type") == "snapshot":
                    state = message["state"]
                if message.get("type") == "receipt" and message.get("id") == intent:
                    if message.get("outcome") != "committed":
                        raise AssertionError("staging refused the relaunch")
                    break
            until(lambda s: ready(s, half, slot=old["slot"], after=old["generation"]))
            new = ready(state, half, slot=old["slot"], after=old["generation"])
            fresh(old, new)
            prove_frame(new)
            print(json.dumps({"half": half, "freshLaunchNames": True, "ipv4RendererFrames": True}), flush=True)
    finally:
        connection.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"status": "failed", "errorType": type(error).__name__}))
        raise SystemExit(1)
