#!/usr/bin/env python3
"""The EC2 tile's nginx site (tile/nginx-renderer.conf), run by a real nginx: it passes the
routes something outside the instance uses to the renderer and answers 404 to every other
path without reaching it, and holds each source to its share of requests in progress. Needs nginx
and openssl on PATH (or nginx at /usr/sbin/nginx)."""
import http.server
from pathlib import Path
import re
import shutil
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time
import unittest

SITE = Path(__file__).resolve().parent / "tile" / "nginx-renderer.conf"
# The paths the page, the coordinator and the viewer-path smoke use (fleet/web/core/net.ts,
# src/fleet/coord/ec2_aws.rs, fleet/coord/stage-smoke.py), with the method each uses.
PASSED = [("GET", "/frames?h=180"), ("GET", "/frames?h=270&q=lo"), ("GET", "/health"), ("HEAD", "/health"),
          ("POST", "/scene"), ("POST", "/budget")]
REFUSED = [("GET", "/still?h=2160"), ("HEAD", "/still"), ("GET", "/raw?t=1"), ("GET", "/mirror"), ("POST", "/action"),
           ("GET", "/"), ("GET", "/frames/x"), ("GET", "/framesx"), ("GET", "/healthz"), ("GET", "/health/")]


def nginx_binary():
    return shutil.which("nginx") or ("/usr/sbin/nginx" if Path("/usr/sbin/nginx").exists() else None)


class Upstream(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


def upstream(path, seen, headers):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def handle_one(self):
            seen.append((self.command, self.path))
            headers.append({k.lower(): v for k, v in self.headers.items()})
            if self.headers.get("Upgrade", "").lower() == "websocket":
                # A frame socket: switch protocols and stream until nginx closes its end.
                self.wfile.write(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                                 b"Connection: Upgrade\r\n\r\n")
                self.wfile.flush()
                while self.rfile.read(1):
                    pass
                self.close_connection = True
                return
            body = b"renderer"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        do_GET = do_HEAD = do_POST = handle_one

        def address_string(self):
            return "upstream"

        def log_message(self, *args):
            pass

    return Upstream(path, Handler)


class TileNginxTests(unittest.TestCase):
    def setUp(self):
        binary = nginx_binary()
        self.assertIsNotNone(binary, "nginx is not installed: this test runs the site in a real nginx")
        self.tmp = Path(tempfile.mkdtemp(prefix="tile-nginx-"))
        subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                        "-nodes", "-subj", "/CN=tile.test", "-days", "1", "-keyout", str(self.tmp / "key.pem"),
                        "-out", str(self.tmp / "fullchain.pem")], check=True, capture_output=True)
        self.seen, self.headers = [], []
        self.up = upstream(str(self.tmp / "up.sock"), self.seen, self.headers)
        threading.Thread(target=self.up.serve_forever, daemon=True).start()
        site = SITE.read_text()
        # The site as shipped, moved onto Unix sockets in this directory: each replacement must
        # find its text, so a changed site cannot slip past the test unexamined.
        for old, new in (("/etc/fcvm-fleet/tls/", f"{self.tmp}/"),
                         ("listen 443 ssl default_server;", f"listen unix:{self.tmp}/front.sock ssl;"),
                         ("listen [::]:443 ssl default_server;\n", ""),
                         ("http://127.0.0.1:8080", f"http://unix:{self.tmp}/up.sock:")):
            self.assertIn(old, site)
            site = site.replace(old, new)
        # The source of a connection on the front socket is the X-Test-Source header's address
        # (test-only, set_real_ip_from), so the test can connect from any IPv4 address or /64.
        temp = "set_real_ip_from unix:;\nreal_ip_header X-Test-Source;\n" + "".join(f"{k}_temp_path {self.tmp}/{k};\n" for k in ("client_body", "proxy", "fastcgi", "uwsgi", "scgi"))
        (self.tmp / "nginx.conf").write_text(
            f"pid {self.tmp}/nginx.pid;\nerror_log {self.tmp}/error.log;\ndaemon off;\nevents {{}}\n"
            f"http {{\naccess_log off;\n{temp}{site}\n}}\n")
        self.nginx = subprocess.Popen([binary, "-p", str(self.tmp), "-c", str(self.tmp / "nginx.conf"),
                                       "-e", str(self.tmp / "error.log")],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while not (self.tmp / "front.sock").exists():
            if self.nginx.poll() is not None or time.monotonic() > deadline:
                self.fail("nginx did not start: " + (self.tmp / "error.log").read_text(errors="replace"))
            time.sleep(0.02)
        self.tls = ssl.create_default_context(cafile=str(self.tmp / "fullchain.pem"))
        self.tls.check_hostname = False

    def tearDown(self):
        self.nginx.terminate()
        self.nginx.wait(10)
        self.up.shutdown()
        self.up.server_close()
        shutil.rmtree(self.tmp)

    def ask(self, method, target, extra="Connection: close"):
        raw = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        raw.settimeout(10)
        raw.connect(str(self.tmp / "front.sock"))
        with self.tls.wrap_socket(raw, server_hostname="tile.test") as s:
            body = b"{}" if method == "POST" else b""
            s.sendall(f"{method} {target} HTTP/1.1\r\nHost: tile.test\r\nContent-Type: application/json\r\n"
                      f"Content-Length: {len(body)}\r\n{extra}\r\n\r\n".encode() + body)
            # The status line is enough: a keep-alive connection (the upgrade's) stays open.
            reply = b""
            while b"\r\n" not in reply and (chunk := s.recv(65536)):
                reply += chunk
        return int(reply.split(b" ", 2)[1])

    def open_frames(self, source):
        """Open a frame socket from `source` and keep it: the status (101 when the renderer took
        it) and the socket, or None and no socket when nginx closed it without a response."""
        raw = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        raw.settimeout(10)
        raw.connect(str(self.tmp / "front.sock"))
        s = self.tls.wrap_socket(raw, server_hostname="tile.test")
        s.sendall(f"GET /frames?h=180 HTTP/1.1\r\nHost: tile.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                  f"Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
                  f"X-Test-Source: {source}\r\n\r\n".encode())
        reply = b""
        try:
            while b"\r\n" not in reply and (chunk := s.recv(65536)):
                reply += chunk
        except (ssl.SSLEOFError, ConnectionResetError, BrokenPipeError):
            pass
        if not reply:
            s.close()
            return None, None
        self.held.append(s)
        return int(reply.split(b" ", 2)[1]), s

    def test_one_source_holds_at_most_the_limit_and_others_still_connect(self):
        found = re.search(r"limit_conn fleet_source (\d+);", SITE.read_text())
        self.assertIsNotNone(found, "the site sets no per-source limit")
        limit = int(found.group(1))
        self.held = []
        try:
            # An IPv4 address, and an IPv6 /64 whose connections come from different addresses in it.
            for name, source, same, other in (
                    ("ipv4", lambda i: "192.0.2.1", "192.0.2.1", "192.0.2.2"),
                    ("ipv6", lambda i: f"2001:db8:1:2::{i + 1:x}", "2001:db8:1:2:ffff::1", "2001:db8:1:3::1")):
                with self.subTest(family=name):
                    for i in range(limit):
                        self.assertEqual(self.open_frames(source(i))[0], 101, f"connection {i + 1} of {limit}")
                    self.assertEqual(self.open_frames(same), (None, None),
                                     f"connection {limit + 1} from one source was not refused")
                    self.assertEqual(self.open_frames(other)[0], 101, "another source was refused")
            # A closed socket gives its place back: the source connects again once nginx has seen
            # the close (it ends the request when it reads the end of the tunnel).
            self.held.pop(0).close()
            deadline = time.monotonic() + 5
            while (status := self.open_frames("192.0.2.1")[0]) is None and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertEqual(status, 101, "a closed socket did not give its place back")
        finally:
            for s in self.held:
                s.close()

    def test_the_routes_outside_clients_use_reach_the_renderer(self):
        for method, target in PASSED:
            with self.subTest(method=method, target=target):
                before = len(self.seen)
                self.assertEqual(self.ask(method, target), 200)
                self.assertEqual(self.seen[before:], [(method, target)])

    def test_a_frame_socket_upgrade_reaches_the_renderer(self):
        self.ask("GET", "/frames?h=180", "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                 "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13")
        self.assertEqual(self.seen, [("GET", "/frames?h=180")])
        self.assertEqual((self.headers[0].get("upgrade"), self.headers[0].get("connection")), ("websocket", "upgrade"))
        self.assertEqual(self.headers[0].get("host"), "tile.test")

    def test_every_other_path_is_404_without_reaching_the_renderer(self):
        for method, target in REFUSED:
            with self.subTest(method=method, target=target):
                self.assertEqual(self.ask(method, target), 404)
        self.assertEqual(self.seen, [], "nginx passed a refused path to the renderer")


if __name__ == "__main__":
    unittest.main()
