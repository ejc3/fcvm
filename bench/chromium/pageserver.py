#!/usr/bin/env python3
"""Tiny threaded static server for the bench fixture pages (stdlib only).

Endpoints:
  /<fixture files>  static files from --root, served with Cache-Control:
                    no-store so every render is a real fetch (a warm clone
                    would otherwise serve repeats from Chromium's HTTP cache
                    and the in-guest vs host-served arms would diverge)
  /ready            200 once --ready-file exists (touched by entry.sh after
                    warmup), 503 before — usable as an fcvm --health-check
                    target so the auto startup snapshot captures a WARM
                    browser, not merely a started one

Threaded because medium.html loads CSS + JS + 4 images: parallel connections
must not serialize on the server.
"""

import argparse
import os
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, help="directory of fixture pages")
    parser.add_argument("--addr", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--ready-file", default="/run/bench-ready")
    args = parser.parse_args()

    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=args.root, **kw)

        def do_GET(self):
            if self.path == "/ready":
                ready = os.path.exists(args.ready_file)
                body = b"ready\n" if ready else b"warming\n"
                self.send_response(200 if ready else 503)
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

    server = ThreadingHTTPServer((args.addr, args.port), Handler)
    print(f"pageserver listening on {args.addr}:{args.port} root={args.root}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
