#!/usr/bin/env python3
"""The viewer-path smoke: what a viewer's browser gets from every tile of every mode.

Proves Access, fresh launch names that resolve at once, and, for each mode (the wall's
renderers, phones, Windows sandboxes, Minecraft servers, the potato game), that every tile the
coordinator reports running answers a viewer over the address family checked: through public
DNS and the published port to a decoded, non-flat frame (or a Minecraft map, or a verified potato
catch), with one take-over round trip where staging allows it. The tiles, their URLs and their
ports come from the coordinator's own state; each port must be one fleet/published-ports.json
declares for its mode and family. A tile that does not answer, or a family it has no URL for,
fails the check, and a check that cannot run (no ffmpeg, a viewer that does not have the family)
blocks.

    stage-smoke.py [--family v4|v6] [--single-family] [--modes wall,phones,...] [--relaunch]
                   [--takeover]

Failure output carries no URL, host name or token: tile ids, the mode, the family and fixed text.
"""
import ast
import base64
import errno
import hashlib
import ipaddress
import json
import os
import re
import shutil
import socket
import ssl
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path


# The step the smoke is on, printed when it fails: fixed text naming the socket and the half.
STEP = "start"


def step(name):
    global STEP
    STEP = name


def own_messages():
    """The literal messages of this file's own raise statements: the only error text a
    failure prints, so the log never carries a URL, a host name or a token."""
    tree = ast.parse(Path(__file__).read_text())
    return {node.exc.args[0].value for node in ast.walk(tree)
            if isinstance(node, ast.Raise) and isinstance(node.exc, ast.Call) and node.exc.args
            and isinstance(node.exc.args[0], ast.Constant) and isinstance(node.exc.args[0].value, str)}


def describe(error):
    """Fixed text for an exception: one of this file's own messages, or an OS error's errno
    name; None for anything else (its text could name an endpoint)."""
    message = str(error)
    if message in own_messages():
        return message
    if isinstance(error, OSError) and error.errno in errno.errorcode:
        return errno.errorcode[error.errno]
    return None


# The two address families a viewer can have: the socket family, the DNS record type, and the
# state field holding a tile's URL in that family.
FAMILIES = {"v4": (socket.AF_INET, 1, "frame_url_v4"), "v6": (socket.AF_INET6, 28, "frame_url")}


def ipv4(host, port):
    """Use the same address family as an IPv4-only viewer; reject private endpoints."""
    return public(host, port, "v4")


def public(host, port, family):
    """A TLS socket to host over one family only, through the system resolver; private
    endpoints are refused. For the coordinator's own names, which are not launch names."""
    addresses = socket.getaddrinfo(host, port, FAMILIES[family][0], socket.SOCK_STREAM)
    for _, _, _, _, address in addresses:
        if not ipaddress.ip_address(address[0]).is_global:
            continue
        try:
            raw = socket.create_connection(address[:2], timeout=20)
            return ssl.create_default_context().wrap_socket(raw, server_hostname=host)
        except OSError:
            continue
    raise RuntimeError("no reachable public endpoint in this family")


class WebSocketClosed(RuntimeError):
    """The peer closed the connection, distinct from a malformed WebSocket message."""


class Socket:
    def __init__(self, url, headers=None, opener=None):
        """A WebSocket over TLS. `opener(host, port)` gives the TLS socket: by default an
        IPv4 one through the system resolver."""
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme != "wss" or parsed.username or parsed.password or parsed.fragment:
            raise ValueError("a secure WebSocket URL is required")
        self.s = (opener or ipv4)(parsed.hostname, parsed.port or 443)
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


# The zone every launch name is under, and the resolvers that are asked for it: staging's,
# or a host-image acceptance's own coordinator (fleet/tools/viewer-acceptance.sh).
ZONE = os.environ.get("SMOKE_ZONE", "fcvm.dev")


def published_ports():
    """fleet/published-ports.json: the one declaration of every port a viewer connects to."""
    return json.loads((Path(__file__).resolve().parent.parent / "published-ports.json").read_text())


# The range of fleet/published-ports.json each mode's GPU-host IPv4 port is in, and the path of
# its viewer socket.
RANGE_OF = {"wall": "slot", "phones": "slot", "windows": "windows", "minecraft": "minecraft",
            "aggregate": "aggregate"}
PATH_OF = {"minecraft": "/view"}


def declared_ports(mode, family, half="fcvm"):
    """The ports a viewer may be sent to for a tile of `mode` over `family`."""
    ports = published_ports()
    if family == "v6":
        return {ports["ipv6_port"]}
    if half == "ec2":
        return {ports["ec2_tile_ipv4_port"]}
    host = ports["gpu_host_ipv4"]
    r = {x["mode"]: x for x in host["ranges"]}[RANGE_OF[mode]]
    first = host["base"] + r["offset"]
    return set(range(first, first + r["count"]))


def checked(url, path, ports):
    """The host and port of a viewer URL, refused unless it is a secure WebSocket on a launch
    name of the zone, at `path`, on one of `ports`."""
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "wss" or not parsed.hostname or not parsed.hostname.endswith("." + ZONE)
            or parsed.username or parsed.password or parsed.path != path or parsed.query or parsed.fragment):
        raise ValueError("unexpected frame endpoint")
    if (parsed.port or 443) not in ports:
        raise ValueError("a port outside fleet/published-ports.json for its mode and family")
    return parsed.hostname, parsed.port or 443


def endpoint(url, mode, family, half="fcvm"):
    """A published viewer URL of a tile of `mode` over `family`: on its mode's path and on a port
    the declaration gives its mode and family."""
    return checked(url, PATH_OF.get(mode, "/frames"), declared_ports(mode, family, half))


def frame_host(url):
    """A wall tile's frame endpoint, in either family and either half: its host name."""
    ports = declared_ports("wall", "v4") | declared_ports("wall", "v4", "ec2") | declared_ports("wall", "v6")
    return checked(url, "/frames", ports)[0]


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


def resolver_address(resolver):
    """A resolver as `ip`, `ip:port` or `[ip6]:port`: (family, ip, port)."""
    port, ip = DNS_PORT, resolver
    if resolver.startswith("["):
        ip, _, rest = resolver[1:].partition("]")
        port = int(rest.lstrip(":")) if rest else DNS_PORT
    elif resolver.count(":") == 1:
        ip, port = resolver.split(":")[0], int(resolver.split(":")[1])
    family = socket.AF_INET6 if ":" in ip else socket.AF_INET
    return family, ip, port


def resolve(resolver, name, qtype, timeout):
    """One query to a resolver; the addresses it answers within timeout seconds."""
    ident, query = dns_query(name, qtype)
    deadline = time.monotonic() + timeout
    family, ip, port = resolver_address(resolver)
    with socket.socket(family, socket.SOCK_DGRAM) as s:
        s.connect((ip, port))
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


# ---- The viewer path: every mode, every tile, one address family ---------------------------

# The modes a viewer can open, and where the coordinator's state lists their tiles.
MODES = ("wall", "phones", "windows", "minecraft", "potato")
# A frame decoded to a flat image is a blank tile: its luma spread (brightest less darkest, of
# 255) must reach this. A rendered scene, a phone's screen and a desktop all exceed it.
FLAT_SPREAD = 16
# How long one tile may take to give its first payload, and to accept TCP and finish TLS.
PROBE_SECONDS = 40
CONNECT_SECONDS = 10
# Control endpoints that say whether this viewer has a family at all: Cloudflare's resolver,
# on 443, in each family.
CONTROL = {"v4": ("1.1.1.1", 443), "v6": ("2606:4700:4700::1111", 443)}


def reaches(family, timeout=10):
    """Whether this viewer opens a TCP connection over `family` to a public control endpoint."""
    try:
        with socket.socket(FAMILIES[family][0], socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(CONTROL[family])
        return True
    except OSError:
        return False


def prove_viewer(family, single):
    """A check of one family needs a viewer that has it; with `single`, one that has no other,
    so a pass is a pass for viewers that have only this family."""
    if not reaches(family):
        raise RuntimeError("this viewer has no route in the family it checks")
    other = "v6" if family == "v4" else "v4"
    if single and reaches(other):
        raise AssertionError("this viewer is not single-family: it reaches the other family too")


def tiles_of(state, mode):
    """The tiles the coordinator reports for `mode`, as (half, tile); None when it reports no
    such mode at all."""
    if mode == "wall":
        return [(t.get("half"), t) for t in state.get("tiles") or []] or None
    if mode == "aggregate":
        return [(a.get("half"), a) for a in state.get("aggregates") or []]
    view = state.get(mode)
    if not isinstance(view, dict) or not view.get("tiles"):
        return None
    return [("fcvm", t) for t in view["tiles"]]


def mine(tile, viewer):
    return bool(tile.get("borrowed")) and (tile["borrowed"] or {}).get("viewer") == viewer


def private(tile, viewer):
    """Lent to another viewer with its stream withdrawn from everyone else (a wall tile, a phone,
    a sandbox): not this viewer's to check. A lent Minecraft server keeps its URLs, since everyone
    watches it, so it is checked like any other."""
    return (bool(tile.get("borrowed")) and not mine(tile, viewer)
            and not tile.get("frame_url") and not tile.get("frame_url_v4"))


def settled(state, modes, viewer=None):
    """Every tile of every mode is running (a tile lent to another viewer is left out: its stream
    is its viewer's), and the potato game has players."""
    for mode in modes:
        if mode == "potato":
            potato = state.get("potato")
            if not isinstance(potato, dict) or not potato.get("capacity"):
                return False
            continue
        tiles = tiles_of(state, mode)
        if not tiles:
            return False
        for _, t in tiles:
            if t.get("phase") not in ("running", "borrowed") and not private(t, viewer):
                return False
    return True


class Probe:
    """One viewer-path check's outcome: the mode, the tile id, the family, what was proved or the
    step that failed and its fixed text. Printed as one JSON line; no URL, host or token."""

    def __init__(self, mode, tile, family):
        self.mode, self.tile, self.family = mode, tile, family
        self.step, self.ok, self.error, self.detail = "start", False, None, {}

    def line(self):
        out = {"mode": self.mode, "tile": self.tile, "family": self.family, "ok": self.ok}
        if self.ok:
            out.update(self.detail)
        else:
            out.update(step=self.step, errorType=self.error[0], error=self.error[1])
        return out


def fixed(error):
    """Fixed text for a probe's exception (describe(), plus timeouts and TLS's own reason code)."""
    text = describe(error)
    if text is None and isinstance(error, (TimeoutError, socket.timeout)):
        text = "timed out"
    if text is None and isinstance(error, ssl.SSLError) and re.fullmatch(r"[A-Z0-9_]{1,64}", str(error.reason or "")):
        text = "TLS " + error.reason
    return type(error).__name__, text


def lookup(host, family, resolvers):
    """The global addresses of a launch name in `family`, from the first resolver that answers
    with any: a viewer's browser gets them through public DNS too."""
    qtype = FAMILIES[family][1]
    for resolver in resolvers:
        try:
            found = resolve(resolver, host, qtype, 5)
        except (OSError, ValueError, AssertionError):
            continue
        found = [a for a in found if a.is_global]
        if found:
            return found
    raise AssertionError("the launch name has no global address in this family")


def tls_context():
    """The trust a viewer has: the system's, or a host-image acceptance's throwaway CA."""
    ca = os.environ.get("SMOKE_CA_FILE")
    return ssl.create_default_context(cafile=ca) if ca else ssl.create_default_context()


def opener(family, resolvers):
    """A Socket opener that goes through public DNS to an address of `family` only."""
    def open_tls(host, port):
        last = None
        for address in lookup(host, family, resolvers):
            try:
                raw = socket.create_connection((str(address), port), timeout=CONNECT_SECONDS)
            except OSError as error:
                last = error
                continue
            try:
                raw.settimeout(CONNECT_SECONDS)
                return tls_context().wrap_socket(raw, server_hostname=host)
            except OSError as error:
                raw.close()
                last = error
        raise last or RuntimeError("no reachable public endpoint in this family")
    return open_tls


def decode(units, width, height):
    """The first picture of `units` (Annex-B access units from a keyframe) as 8-bit luma,
    through ffmpeg; the decoded bytes."""
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        raise RuntimeError("ffmpeg is not installed: frames cannot be decoded")
    out = subprocess.run([ffmpeg, "-v", "error", "-f", "h264", "-i", "pipe:0", "-frames:v", "1",
                          "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"],
                         input=b"".join(units), capture_output=True, timeout=30)
    if out.returncode != 0 or len(out.stdout) < width * height:
        raise AssertionError("the tile's frames did not decode to a picture")
    return out.stdout[:width * height]


def prove_stream(connection, deadline, probe):
    """A frame stream: its header, then FCVF frames from a keyframe that decode to a picture
    that is not flat. Returns the picture's size and luma spread."""
    header, units, size = None, [], None
    probe.step = "stream header"
    while time.monotonic() < deadline:
        opcode, payload = connection.message(deadline)
        if opcode == 1 and header is None:
            try:
                header = json.loads(payload)
            except ValueError:
                raise AssertionError("the stream's header is not JSON")
            if not isinstance(header, dict) or not re.fullmatch(r"avc1\.[0-9A-Fa-f]{6}", str(header.get("codec"))):
                raise AssertionError("the stream's header names no H.264 codec")
            probe.step = "first keyframe"
            continue
        if opcode != 2:
            continue
        if header is None:
            raise AssertionError("a frame arrived before the stream's header")
        if len(payload) < 26 or payload[:5] != b"FCVF\x01":
            raise AssertionError("a frame message without FCVF framing")
        keyframe, (width, height) = payload[5] & 1, struct.unpack("<HH", payload[6:10])
        if not units and not keyframe:
            continue
        if not width or not height:
            raise AssertionError("a frame of zero size")
        size = size or (width, height)
        units.append(payload[22:])
        probe.step = "decode"
        if len(units) >= 4:
            break
    if not units:
        raise TimeoutError("the tile sent no keyframe in time")
    picture = decode(units, *size)
    spread = max(picture) - min(picture)
    if spread < FLAT_SPREAD:
        raise AssertionError("the tile's first picture is flat: a blank tile")
    return {"width": size[0], "height": size[1], "lumaSpread": spread}


def prove_view(connection, slot, deadline, probe):
    """A Minecraft view socket: its hello for this server, then the map it draws."""
    probe.step = "view hello"
    hello = None
    while time.monotonic() < deadline:
        opcode, payload = connection.message(deadline)
        if opcode != 1:
            continue
        try:
            message = json.loads(payload)
        except ValueError:
            raise AssertionError("a view message is not JSON")
        kind = message.get("type") if isinstance(message, dict) else None
        if hello is None:
            if kind != "hello" or message.get("tile") != slot:
                raise AssertionError("the view socket's first message is not this server's hello")
            hello = message
            probe.step = "view map"
            continue
        if kind != "map":
            continue
        region = message.get("region") or {}
        w, h = region.get("w"), region.get("h")
        if not (isinstance(w, int) and isinstance(h, int) and 0 < w * h <= 1 << 20):
            raise AssertionError("the view's map region is not a grid")
        try:
            cells = base64.b64decode(message.get("cells") or "", validate=True)
        except ValueError:
            raise AssertionError("the view's map cells are not base64")
        if len(cells) != w * h * 2:
            raise AssertionError("the view's map cells do not fill its region")
        # A column the tile has not read is material 0 at height 0: a map of only those is a
        # blank tile.
        if not any(cells):
            raise AssertionError("the view's map holds no column the server has read: a blank tile")
        return {"mapColumns": w * h, "bots": hello.get("bots")}
    raise TimeoutError("the server sent no map in time")


def drive_round_trip(connection, deadline, probe):
    """A take-over's round trip: a drive message (no keys held) that the server acknowledges
    as its driver's. Until the borrow reaches the server it answers not ok; retried."""
    probe.step = "drive acknowledged"
    seq = 0
    while time.monotonic() < deadline:
        seq += 1
        connection.send({"type": "drive", "seq": seq, "keys": [], "yaw": 0})
        start = time.monotonic()
        while time.monotonic() < min(deadline, start + 2):
            try:
                opcode, payload = connection.message(min(deadline, start + 2))
            except TimeoutError:
                break
            if opcode != 1:
                continue
            message = json.loads(payload)
            if message.get("type") == "drive_ack" and message.get("seq") == seq:
                if message.get("ok") is True:
                    return {"driveAckMs": round((time.monotonic() - start) * 1000)}
                break
        time.sleep(0.5)
    raise TimeoutError("the server never acknowledged the driver")


def probe_tile(mode, half, tile, family, admit, viewer, resolvers, takeover=None):
    """Prove one tile over one family; the Probe. `takeover` drives a Minecraft server this
    viewer has borrowed."""
    probe = Probe(mode, tile.get("id"), family)
    try:
        probe.step = "published URL"
        url = tile.get(FAMILIES[family][2])
        if not url:
            raise AssertionError("the coordinator publishes no URL for this tile in this family")
        endpoint(url, mode, family, half)
        probe.step = "connect"
        socket_url = url if mode == "minecraft" else url + "?h=180"
        connection = Socket(socket_url, opener=opener(family, resolvers))
        try:
            deadline = time.monotonic() + PROBE_SECONDS
            if admit is not None:
                connection.send({"admit": admit})
            elif mode == "minecraft":
                connection.send({"viewer": viewer})
            if mode == "minecraft":
                probe.detail = prove_view(connection, tile.get("slot"), deadline, probe)
                if takeover:
                    probe.detail.update(drive_round_trip(connection, deadline + 20, probe))
            else:
                probe.detail = prove_stream(connection, deadline, probe)
        finally:
            connection.close()
        probe.ok = True
    except Exception as error:
        probe.error = fixed(error)
    return probe


def probe_all(jobs):
    """Run every (args) of probe_tile at once; the Probes, in order."""
    out = [None] * len(jobs)
    def run(i):
        out[i] = probe_tile(*jobs[i])
    threads = [threading.Thread(target=run, args=(i,), name=f"probe-{i}") for i in range(len(jobs))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return out


def sweep_jobs(state, modes, family, admit, viewer, resolvers):
    """A probe per tile of every mode (and per serving aggregate), and a Probe failed already
    for each tile the coordinator has no running stream of and each mode it reports nothing of."""
    jobs, failed = [], []
    for mode in modes + ("aggregate",):
        if mode == "potato":
            continue
        tiles = tiles_of(state, mode)
        if tiles is None:
            p = Probe(mode, None, family)
            p.step, p.error = "coordinator state", ("AssertionError", "the coordinator reports no tiles of this mode")
            failed.append(p)
            continue
        for half, t in tiles:
            if mode == "aggregate" and not (t.get("frame_url") or t.get("frame_url_v4")):
                continue  # an aggregator that serves no browser: aggregation is off
            if private(t, viewer):
                continue
            if t.get("phase", "running") not in ("running", "borrowed"):
                p = Probe(mode, t.get("id"), family)
                p.step, p.error = "coordinator state", ("AssertionError", "the tile is not running")
                failed.append(p)
                continue
            jobs.append((mode, half, t, family, admit, viewer, resolvers))
    return jobs, failed


def potato_probe(family, caught):
    """The potato game has no viewer socket: the page draws /live. A verified catch while the
    smoke watched (the round's latest catch moved) is its first real payload."""
    p = Probe("potato", None, family)
    if caught:
        p.ok, p.detail = True, {"verifiedCatch": True}
    else:
        p.step, p.error = "verified catch", ("AssertionError", "no verified catch while the smoke watched")
    return p


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


def live_target():
    """Where /live is and how to reach it: staging through Access, or a host-image acceptance's
    own coordinator on loopback (SMOKE_LIVE_URL, plain ws://, no Access)."""
    local = os.environ.get("SMOKE_LIVE_URL")
    if local:
        parsed = urllib.parse.urlsplit(local)
        if parsed.scheme != "ws" or not ipaddress.ip_address(parsed.hostname).is_loopback:
            raise ValueError("SMOKE_LIVE_URL must be a ws:// URL on loopback")
        return None, local, {}
    api, origin = os.environ["STAGING_API_URL"].rstrip("/"), os.environ["STAGING_ORIGIN"].rstrip("/")
    headers = {"CF-Access-Client-Id": os.environ["CF_ACCESS_CLIENT_ID"],
               "CF-Access-Client-Secret": os.environ["CF_ACCESS_CLIENT_SECRET"], "Origin": origin}
    return (api, origin), api.replace("https://", "wss://", 1) + "/live", headers


def plain(host, port):
    """A plain TCP socket to a loopback coordinator (host-image acceptance only)."""
    if not ipaddress.ip_address(host).is_loopback:
        raise ValueError("a plain socket is for a loopback coordinator only")
    return socket.create_connection((host, port), timeout=20)


def main(family="v4", single=False, modes=(), relaunch=True, takeover=False):
    """The smoke. With no `modes` it is the deploy probe alone (Access, the relaunch and fresh
    names); `modes` adds the viewer path over `family` for every tile of each mode."""
    if modes:
        step(f"viewer: this runner as a {family} viewer")
        prove_viewer(family, single)
    stage, live_url, headers = live_target()
    resolvers = [r for r in os.environ.get("SMOKE_RESOLVERS", "").split(",") if r] or list(PUBLIC_RESOLVERS)
    if stage is not None:
        api, origin = stage
        step("Access check")
        for url in (api + "/healthz", origin + "/"):
            code, response_headers, _ = get(url)
            location = urllib.parse.urlsplit(response_headers.get("Location", ""))
            if code != 302 or location.scheme != "https" or not (location.hostname or "").endswith(".cloudflareaccess.com"):
                raise AssertionError("staging did not require Access")
            code, _, _ = get(url, headers)
            if code != 200:
                raise AssertionError("authenticated staging request failed")
        step("agents-only endpoint check")
        code, _, _ = get(api + "/agent", headers)
        if code not in (401, 403, 302):
            raise AssertionError("the CI token reached the agents-only endpoint")
    def live():
        if stage is None:
            parsed = urllib.parse.urlsplit(live_url)
            return Socket("wss" + live_url[2:], headers, opener=lambda h, p: plain(parsed.hostname, parsed.port or 80))
        if modes:
            return Socket(live_url, headers, opener=lambda h, p: public(h, p, family))
        return Socket(live_url, headers)
    step("/live connect")
    connection = live()
    failures = 0
    try:
        # The first message on /live: a page's seat ticket (none here).
        connection.send({"type": "seat", "ticket": None})
        deadline = time.monotonic() + 720 + (900 if modes else 0)
        interval, next_relaunch = relaunch_interval_seconds(), 0.0
        state, admission = {}, {}
        seat, reconnects, viewer = None, 0, None
        def take(message):
            nonlocal state, admission, seat, viewer
            if message.get("type") == "snapshot":
                state = message["state"]
            elif message.get("type") == "admission":
                admission = message
                seat = message.get("seat")
            elif message.get("type") == "hello":
                viewer = message.get("viewer")
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
                    connection = live()
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
        def commit(name, args):
            """Send an intent and wait for its receipt; whether it committed."""
            intent = str(uuid.uuid4())
            connection.send({"type": "intent", "id": intent, "name": name, "args": args})
            # A refused intent must fail explicitly, rather than wait for an unrelated change.
            while True:
                opcode, payload = connection.message(deadline)
                if opcode != 1:
                    continue
                message = json.loads(payload)
                take(message)
                if message.get("type") == "receipt" and message.get("id") == intent:
                    return message.get("outcome") == "committed"
        if modes:
            failures = viewer_path(family, modes, takeover, resolvers, until, commit,
                                   lambda: (state, admission, viewer))
        if not relaunch:
            return failures
        for half in ("fcvm", "ec2"):
            step(f"/live: wait for an {half} tile to run")
            until(lambda s: ready(s, half))
            old = dict(ready(state, half))
            if direct() is not None:
                step(f"renderer socket: {half} tile refuses a socket without a token")
                prove_refused(old)
            step(f"renderer socket: {half} tile frames")
            prove_frame(old, direct())
            delay = next_relaunch - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            step(f"/live: {half} relaunch receipt")
            if not commit("relaunch", {"tile": old["id"]}):
                raise AssertionError("staging refused the relaunch")
            # Start the interval at the receipt, after the server accepted
            # the intent, so varying network latency cannot shorten it.
            next_relaunch = time.monotonic() + interval
            step(f"/live: wait for the relaunched {half} tile to run")
            until(lambda s: ready(s, half, slot=old["slot"], after=old["generation"]))
            new = ready(state, half, slot=old["slot"], after=old["generation"])
            step(f"/live: relaunched {half} tile has fresh names")
            fresh(old, new)
            # Before anything else looks the names up: each resolver's first query.
            step(f"public resolvers: relaunched {half} tile names")
            slowest = prove_resolves(new)
            step(f"renderer socket: relaunched {half} tile frames")
            prove_frame(new, direct())
            print(json.dumps({"half": half, "freshLaunchNames": True, "freshNamesResolved": True,
                              "resolveMsMax": round(slowest * 1000), "ipv4RendererFrames": True,
                              "directAdmission": direct() is not None}), flush=True)
    finally:
        connection.close()
    return failures


def viewer_path(family, modes, takeover, resolvers, until, commit, now):
    """Every tile of every mode over `family`, as a viewer's browser reaches it; prints a line per
    tile and returns how many failed. `until`, `commit` and `now` are main's /live helpers."""
    step("/live: wait for every mode's tiles to run")
    settle_by = time.monotonic() + 600
    until(lambda s: settled(s, modes, now()[2]) or time.monotonic() > settle_by)
    state, admission, viewer = now()
    token = (admission.get("direct") or {}).get("value")
    catches = ((state.get("potato") or {}).get("round") or {}).get("seq")
    step("viewer path")
    jobs, probes = sweep_jobs(state, modes, family, token, viewer, resolvers)
    probes = probe_all(jobs) + probes
    if takeover and "minecraft" in modes:
        probes.append(minecraft_takeover(family, token, viewer, resolvers, until, commit, now))
    if "potato" in modes:
        step("/live: a verified potato catch")
        watch_by = time.monotonic() + 90
        def caught(s):
            seq = ((s.get("potato") or {}).get("round") or {}).get("seq")
            return (seq is not None and seq != catches) or time.monotonic() > watch_by
        until(caught)
        seq = ((now()[0].get("potato") or {}).get("round") or {}).get("seq")
        probes.append(potato_probe(family, seq is not None and seq != catches))
    for p in probes:
        print(json.dumps(p.line()), flush=True)
    failed = sum(not p.ok for p in probes)
    print(json.dumps({"family": family, "modes": list(modes), "probes": len(probes), "failed": failed}), flush=True)
    return failed


def minecraft_takeover(family, token, viewer, resolvers, until, commit, now):
    """Borrow a server, drive it once through its view socket, give it back."""
    p = Probe("minecraft", None, family)
    p.step = "take over"
    if not commit("claim_minecraft", {}):
        p.error = ("AssertionError", "the coordinator refused the take-over")
        return p
    try:
        held_by = time.monotonic() + 30
        def held(s):
            return any(mine(t, viewer) for _, t in tiles_of(s, "minecraft") or []) or time.monotonic() > held_by
        until(held)
        tile = next((t for _, t in tiles_of(now()[0], "minecraft") or [] if mine(t, viewer)), None)
        if tile is None:
            p.error = ("AssertionError", "the borrowed server never showed as this viewer's")
            return p
        p = probe_tile("minecraft", "fcvm", tile, family, token, viewer, resolvers, takeover=True)
        p.tile = (p.tile or "") + " take-over"
        return p
    finally:
        commit("release_minecraft", {})



def arguments(argv):
    import argparse
    parser = argparse.ArgumentParser(description="The viewer-path smoke (module docstring).")
    parser.add_argument("--family", choices=sorted(FAMILIES), default="v4")
    parser.add_argument("--single-family", action="store_true",
                        help="require a viewer that has no other family (an IPv4-only runner)")
    parser.add_argument("--modes", default="", help="comma separated, of " + ",".join(MODES))
    parser.add_argument("--relaunch", action="store_true", help="relaunch a tile per wall half")
    parser.add_argument("--takeover", action="store_true", help="borrow and drive a Minecraft server")
    args = parser.parse_args(argv)
    modes = tuple(m for m in args.modes.split(",") if m)
    if any(m not in MODES for m in modes):
        parser.error("unknown mode")
    if not modes and not args.relaunch:
        parser.error("nothing to check: give --modes or --relaunch")
    return dict(family=args.family, single=args.single_family, modes=modes,
                relaunch=args.relaunch, takeover=args.takeover)


def run(argv=None):
    try:
        failed = main(**arguments(sys.argv[1:] if argv is None else argv))
        if failed:
            step("viewer path")
            raise AssertionError("a tile failed the viewer path")
    except Exception as error:
        print(json.dumps({"status": "failed", "step": STEP, "errorType": type(error).__name__,
                          "error": describe(error)}))
        raise SystemExit(1)


if __name__ == "__main__":
    run()
