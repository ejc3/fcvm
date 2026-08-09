#!/usr/bin/env python3
"""Container HEALTHCHECK: prove Chromium can serve a screenshot request.

OFF THE REQUEST PATH. This runs from the image's `HEALTHCHECK` directive, inside
the container, on podman's schedule. The per-request path is host -> CDP
WebSocket -> Chromium, with nothing of ours in it.

fcvm's health gate is what triggers the golden snapshot (`src/health.rs`: with no
`--health-check` URL, `Healthy` = container running AND podman's HEALTHCHECK
reports healthy). So whatever this script accepts is exactly what gets frozen
into the snapshot. Two conditions, both required:

  1. The warm marker exists. entry.sh touches it only after it has driven a full
     navigate + screenshot through CDP, so the renderer, JIT, raster and encode
     paths are hot. Without this the gate would fire on a cold browser and every
     "warm clone" number would be a first-paint number.
  2. A REAL CDP round trip succeeds and finds a page target. Not "the port is
     open" and not "entry.sh said so" — an actual HTTP request to Chromium's
     DevTools endpoint whose parsed response contains a target we could attach
     to. Proven, not inferred.

Exit 0 = healthy. Any other exit = not healthy (podman's contract).

Deliberately does NOT open the WebSocket. Attaching would leave a session that
the snapshot then captures mid-handshake, and /json/list already proves the
DevTools endpoint is live, is answering, and has a page. The host's first
WebSocket upgrade after restore is the thing we want to MEASURE, not something to
have half-done in the snapshot.
"""

import json
import os
import sys
import urllib.request

# Chromium's own DevTools port. There is no relay any more: fcvm DNATs every
# published port to the guest's 127.0.0.1, so the host reaches Chromium directly
# (fc-agent/src/network.rs::publish_to_loopback). This probe is the container
# HEALTHCHECK, and the HEALTHCHECK is what triggers fcvm's golden snapshot — if
# it points at a dead port, `reqbench.sh golden` waits out its full 300s and
# never snapshots.
CDP_HOST = os.environ.get("BENCH_CDP_HEALTH_HOST", "127.0.0.1:9222")
READY_FILE = os.environ.get("BENCH_READY_FILE", "/run/bench-ready")
TIMEOUT = float(os.environ.get("BENCH_CDP_HEALTH_TIMEOUT", "3"))


def main() -> int:
    if not os.path.exists(READY_FILE):
        print(f"unhealthy: warm marker {READY_FILE} absent", file=sys.stderr)
        return 1
    try:
        with urllib.request.urlopen(f"http://{CDP_HOST}/json/list", timeout=TIMEOUT) as r:
            targets = json.load(r)
    except Exception as e:  # urllib raises a wide family; any of them = not healthy
        print(f"unhealthy: CDP /json/list failed: {type(e).__name__}: {e}", file=sys.stderr)
        return 1

    pages = [
        t
        for t in targets
        if t.get("type") == "page" and not str(t.get("url", "")).startswith("devtools://")
    ]
    if not pages:
        print(f"unhealthy: no page target among {len(targets)} target(s)", file=sys.stderr)
        return 1

    # Print the target so `podman inspect` health logs record WHICH target was
    # resolved. If this id is identical across clones, the host can skip its own
    # /json/list lookup per request (one fewer HTTP round trip) — the benchmark
    # checks that claim rather than assuming it.
    print(f"healthy pages={len(pages)} id={pages[0].get('id')} ws={pages[0].get('webSocketDebuggerUrl')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
