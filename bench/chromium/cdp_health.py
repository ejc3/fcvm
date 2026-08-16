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


# Where the resident loop publishes its verdict, and how stale a verdict may be
# before the reader must refuse it.
STATE_FILE = os.environ.get("BENCH_HEALTH_STATE", "/run/bench-health")
LOOP_INTERVAL = float(os.environ.get("BENCH_HEALTH_INTERVAL", "1"))


def monotonic_seconds() -> float:
    """Seconds from /proc/uptime, so a clock step cannot age a verdict.

    NOT time.time(). fc-agent steps CLOCK_REALTIME on every restore
    (set_system_clock), which would make every clone's freshly written verdict
    look hours old to a wall-clock reader and fail the gate on every clone.
    /proc/uptime is CLOCK_MONOTONIC and is what the bash reader uses too, so
    both sides measure the same thing.
    """
    with open("/proc/uptime", "r", encoding="ascii") as handle:
        return float(handle.read().split()[0])


def publish(verdict: str, detail: str) -> None:
    """Write the verdict atomically, so a reader never sees a half-written line."""
    tmp = f"{STATE_FILE}.tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        handle.write(f"{verdict} {monotonic_seconds():.3f} {detail}\n")
    os.replace(tmp, STATE_FILE)


def loop() -> int:
    """Run the check forever, publishing each verdict.

    Why resident: as a HEALTHCHECK command this cost a fresh CPython per second
    in EVERY clone, forever. Measured in this image: 9.1ms of interpreter
    startup, 43.6ms for the whole check even when it fails fast. Paid once at
    the golden instead, the interpreter's pages are dirtied before the snapshot
    and are therefore SHARED by every clone rather than privately re-dirtied.
    Each iteration then writes one small file on tmpfs.
    """
    while True:
        started = monotonic_seconds()
        try:
            code = main()
            publish("healthy" if code == 0 else "unhealthy", f"exit={code}")
        except Exception as error:  # a crash here must not look healthy
            publish("unhealthy", f"loop error: {type(error).__name__}: {error}")
        elapsed = monotonic_seconds() - started
        time.sleep(max(0.0, LOOP_INTERVAL - elapsed))


if __name__ == "__main__":
    if "--loop" in sys.argv:
        sys.exit(loop())
    sys.exit(main())
