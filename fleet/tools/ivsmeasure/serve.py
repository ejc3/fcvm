# The broadcast's latency, measured in a browser (docs/fleet/ARCHITECTURE.md, "Delivery: IVS
# or CloudFront"). On a box that also runs the renderer and the broadcast aggregator (one
# clock for all three):
#
#   python3 serve.py 18000 results.jsonl &
#   google-chrome --headless=new --disable-gpu --autoplay-policy=no-user-gesture-required \
#     "http://127.0.0.1:18000/player.html?url=<urlencoded playback URL>&cycles=5&secs=60"
#   python3 summarize.py results.jsonl     # once a {"done": true} line is written
#
# Serves player.html on 127.0.0.1 and appends every POST /report body to the results file.
import http.server, sys, os
DIR = os.path.dirname(os.path.abspath(__file__))
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(DIR, "results.jsonl")
class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k): super().__init__(*a, directory=DIR, **k)
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); b = self.rfile.read(n)
        with open(OUT, "ab") as f: f.write(b + b"\n")
        self.send_response(204); self.end_headers()
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
