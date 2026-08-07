#!/usr/bin/env python3
"""Host-side fixture server for the chromium bench: the "simulated external site".

Serves the SAME bench/chromium/pages/ directory that is baked into the
localhost/chromium-bench image, so the in-guest control arm and this host arm
render identical bytes — the delta between them is pure network-path cost.

Differences from the in-guest pageserver.py (which stays untouched so the
already-built image and its golden snapshots remain valid):
  * dual-stack bind ("::", IPV6_V6ONLY=0) — the IPv6 egress arms fetch
    http://[host-v6]:port/ and a 0.0.0.0 bind would refuse them
  * optional TLS (--certfile/--keyfile) so one process type serves both the
    http and https arms (run two instances); Chromium in the image is launched
    with --ignore-certificate-errors, so a self-signed cert is fine
  * /ready always returns 200 (this server has no warmup)

Same Cache-Control: no-store policy: every render is a real fetch.
"""

import argparse
import os
import socket
import ssl
import sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class DualStackServer(ThreadingHTTPServer):
    address_family = socket.AF_INET6

    def server_bind(self):
        # Accept IPv4 (as ::ffff:a.b.c.d) and IPv6 on one socket.
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, help="directory of fixture pages")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--certfile", help="enable TLS with this certificate")
    parser.add_argument("--keyfile", help="TLS private key (with --certfile)")
    args = parser.parse_args()

    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=args.root, **kw)

        def do_GET(self):
            if self.path == "/ready":
                body = b"ready\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            super().do_GET()

        def end_headers(self):
            self.send_header("Cache-Control", "no-store")
            super().end_headers()

        def log_message(self, *a):
            pass  # quiet: request logs would dominate bench log volume

    server = DualStackServer(("::", args.port), Handler)
    scheme = "http"
    if args.certfile:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(args.certfile, args.keyfile)
        server.socket = ctx.wrap_socket(server.socket, server_side=True)
        scheme = "https"
    print(f"hostserver {scheme} on [::]:{args.port} root={args.root} pid={os.getpid()}", flush=True)
    sys.stdout.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
