#!/usr/bin/env python3
"""Host-aware replay server for the deep-captured corpus.

Serves the ORIGINAL urls: the replay Chromium runs with
--host-resolver-rules="MAP * 127.0.0.1", so every host resolves here; this
server picks the response by (Host header, path?query) from the capture
indexes and replays status + mime + body byte-identically. HTTP on --port and
HTTPS on --tls-port with a self-signed cert (the bench Chromium bakes in
--ignore-certificate-errors).

Resolution order for a request URL u:
  1. exact match on the full url (scheme-insensitive)
  2. match ignoring the query string (analytics beacons carry per-view ids)
  3. 404, counted in /––misses (read by the checker for the report)
"""

import argparse
import http.server
import json
import socket
import ssl
import struct
import subprocess
import sys
import threading
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_indexes(root: Path):
    exact, noquery, redirects = {}, {}, {}
    for idx in sorted(root.glob("*/index.json")):
        data = json.loads(idx.read_text())
        site = idx.parent
        for url, meta in data.get("resources", {}).items():
            u = urllib.parse.urlparse(url)
            key = (u.netloc, u.path, u.query)
            entry = (site / meta["file"], meta.get("status", 200), meta.get("mime", ""))
            exact.setdefault(key, entry)
            noquery.setdefault((u.netloc, u.path), entry)
        for url, r in data.get("redirects", {}).items():
            u = urllib.parse.urlparse(url)
            redirects.setdefault((u.netloc, u.path, u.query), r)
            redirects.setdefault((u.netloc, u.path), r)
    return exact, noquery, redirects


class Handler(http.server.BaseHTTPRequestHandler):
    exact = {}
    noquery = {}
    redirects = {}
    misses = []
    lock = threading.Lock()

    def _serve(self):
        host = (self.headers.get("Host") or "").split(":")[0]
        u = urllib.parse.urlparse(self.path)
        entry = self.exact.get((host, u.path, u.query)) or \
            self.noquery.get((host, u.path))
        if self.path == "/––misses":
            body = json.dumps(self.misses).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if entry is None:
            r = self.redirects.get((host, u.path, u.query)) or \
                self.redirects.get((host, u.path))
            if r:
                self.send_response(r.get("status", 302))
                self.send_header("Location", r.get("location", "/"))
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            with self.lock:
                self.misses.append(f"{host}{self.path}"[:200])
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        path, status, mime = entry
        data = path.read_bytes()
        self.send_response(status if 200 <= (status or 200) < 400 else 200)
        if mime:
            # CDP's mimeType drops the charset parameter, and captured TEXT
            # bodies are re-encoded utf-8 (getResponseBody returns decoded
            # text). Without an explicit charset Chromium assumes cp1252 and
            # every accented site renders mojibake (elmundo, 2026-08-13).
            if ("charset" not in mime
                    and (mime.startswith("text/")
                         or mime in ("application/javascript",
                                     "application/json",
                                     "image/svg+xml"))):
                mime += "; charset=utf-8"
            self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        # Cross-origin subresources (fonts, module scripts, manifests) are CORS
        # requests; the real CDNs send ACAO and dropping it turned every such
        # fetch into net::ERR_FAILED on replay (Guardian, 2026-08-13). Echo the
        # Origin rather than "*": credentialed fetches (CMP/geo APIs) reject
        # the wildcard, and those gate consent overlays the pixel check sees.
        origin = self.headers.get("Origin")
        self.send_header("Access-Control-Allow-Origin", origin or "*")
        if origin:
            self.send_header("Access-Control-Allow-Credentials", "true")
            self.send_header("Vary", "Origin")
        self.end_headers()
        self.wfile.write(data)

    def do_OPTIONS(self):
        # CORS preflight for credentialed fetches (CMP/geo chains): without
        # this the base handler answers 501 and the chain dies before any
        # ACAO header is ever consulted.
        origin = self.headers.get("Origin") or "*"
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", origin)
        self.send_header("Access-Control-Allow-Credentials", "true")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header(
            "Access-Control-Allow-Headers",
            self.headers.get("Access-Control-Request-Headers") or "*",
        )
        self.end_headers()

    do_GET = _serve
    do_POST = _serve  # beacons POST; replay their captured (or 404) response

    def log_message(self, *a):  # quiet
        pass


def selfsigned(tmpdir: Path):
    crt, key = tmpdir / "replay.crt", tmpdir / "replay.key"
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
         "-keyout", str(key), "-out", str(crt), "-days", "2",
         "-subj", "/CN=corpus-replay",
         "-addext", "subjectAltName=DNS:*,DNS:localhost,IP:127.0.0.1"],
        check=True, capture_output=True)
    return crt, key


def dns_responder(addr: str, port: int):
    """Answer every A query with 127.0.0.1 (AAAA answered empty, forcing v4).

    Browser-agnostic host mapping for the replay arm: the replay container
    mounts a resolv.conf pointing here, so ANY engine (Chromium today, WebKit
    later) resolves every corpus host to this box with no engine flags — a
    space-containing --host-resolver-rules value cannot survive the container
    env word-split, which is how the flag approach died (2026-08-13).
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((addr, port))
    while True:
        try:
            data, peer = sock.recvfrom(512)
            if len(data) < 12:
                continue
            txid = data[:2]
            flags = b"\x81\x80"
            qd = data[12:]
            # parse qname end
            i = 0
            while i < len(qd) and qd[i] != 0:
                i += qd[i] + 1
            qend = i + 1 + 4  # name + qtype/qclass
            question = qd[:qend]
            qtype = struct.unpack(">H", qd[i + 1:i + 3])[0]
            if qtype == 1:  # A
                answer = (b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 5, 4)
                          + socket.inet_aton("127.0.0.1"))
                resp = txid + flags + struct.pack(">HHHH", 1, 1, 0, 0) + question + answer
            else:  # empty NOERROR (esp. AAAA -> fall back to A)
                resp = txid + flags + struct.pack(">HHHH", 1, 0, 0, 0) + question
            sock.sendto(resp, peer)
        except Exception:  # noqa: BLE001 - a malformed query must not kill the server
            continue


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=str(HERE / "corpus-live"))
    ap.add_argument("--port", type=int, default=80)
    ap.add_argument("--tls-port", type=int, default=443)
    ap.add_argument("--dns-addr", default="127.0.0.2")
    ap.add_argument("--dns-port", type=int, default=53)
    args = ap.parse_args()

    Handler.exact, Handler.noquery, Handler.redirects = load_indexes(Path(args.root))
    print(f"loaded {len(Handler.exact)} urls", flush=True)

    threading.Thread(target=dns_responder, args=(args.dns_addr, args.dns_port),
                     daemon=True).start()
    print(f"wildcard DNS on {args.dns_addr}:{args.dns_port} -> 127.0.0.1", flush=True)

    plain = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    threading.Thread(target=plain.serve_forever, daemon=True).start()

    tmp = Path("/tmp/corpus-replay-cert")
    tmp.mkdir(exist_ok=True)
    crt, key = selfsigned(tmp)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(crt), str(key))
    tls = http.server.ThreadingHTTPServer(("127.0.0.1", args.tls_port), Handler)
    tls.socket = ctx.wrap_socket(tls.socket, server_side=True)
    print(f"replaying on http://127.0.0.1:{args.port} and https://127.0.0.1:{args.tls_port}", flush=True)
    tls.serve_forever()


if __name__ == "__main__":
    sys.exit(main())
