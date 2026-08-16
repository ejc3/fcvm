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

import health_loop
from health_loop import monotonic_seconds, publish, state_file  # re-exported for callers/tests
import time
import urllib.request

# Chromium's own DevTools port. There is no relay any more: fcvm DNATs this
# eligible published TCP port to guest 127.0.0.1, so the host reaches Chromium
# directly (fc-agent/src/network.rs::publish_to_loopback). This probe is the
# container HEALTHCHECK, and the HEALTHCHECK is what triggers fcvm's golden
# snapshot — if it points at a dead port, `reqbench.sh golden` waits out its full
# 300s and never snapshots.
CDP_HOST = os.environ.get("BENCH_CDP_HEALTH_HOST", "127.0.0.1:9222")
READY_FILE = os.environ.get("BENCH_READY_FILE", "/run/bench-ready")
TIMEOUT = float(os.environ.get("BENCH_CDP_HEALTH_TIMEOUT", "3"))


def main_with_reason() -> tuple[int, str]:
    """The check, returning (exit code, REASON).

    The reason is the point. As a per-second HEALTHCHECK this printed its
    diagnostic to stderr and podman recorded it in the health log, which was
    exactly where an operator looked when a golden never fired. With a resident
    writer and a file-reading HEALTHCHECK, anything left on stderr is lost: the
    health log would say only "exit=1". So the reason travels in the verdict.
    """
    if not os.path.exists(READY_FILE):
        return 1, f"warm marker {READY_FILE} absent"
    try:
        with urllib.request.urlopen(f"http://{CDP_HOST}/json/list", timeout=TIMEOUT) as r:
            targets = json.load(r)
    except Exception as e:  # urllib raises a wide family; any of them = not healthy
        return 1, f"CDP /json/list failed: {type(e).__name__}: {e}"

    pages = [
        t
        for t in targets
        if t.get("type") == "page" and not str(t.get("url", "")).startswith("devtools://")
    ]
    if not pages:
        return 1, f"no page target among {len(targets)} target(s)"

    # The resolved target id travels in the verdict for the same reason the
    # failure reason does: it is the one place a reader can see WHICH target
    # answered. If the id is identical across clones, the host can skip its own
    # /json/list lookup per request; the benchmark checks that rather than
    # assuming it.
    return 0, f"pages={len(pages)} id={pages[0].get('id')}"


def main() -> int:
    """Single-shot form, still used by reqbench.sh verify (HOP A) and by hand."""
    code, reason = main_with_reason()
    print(("healthy " if code == 0 else "unhealthy: ") + reason,
          file=sys.stdout if code == 0 else sys.stderr)
    return code


# Where the resident loop publishes its verdict, and how stale a verdict may be
# before the reader must refuse it.
STATE_FILE = os.environ.get("BENCH_HEALTH_STATE", "/run/bench-health")
LOOP_INTERVAL = float(os.environ.get("BENCH_HEALTH_INTERVAL", "1"))


if __name__ == "__main__":
    if "--loop" in sys.argv:
        sys.exit(health_loop.loop(main_with_reason, "cdp_health"))
    sys.exit(main())
