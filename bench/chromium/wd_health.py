#!/usr/bin/env python3
"""Container HEALTHCHECK: prove the WARM WebKit session can serve a request.

The WebKit twin of cdp_health.py, with one protocol-forced difference: classic
WebDriver's GET /status reports "ready" whether or not any browser is running —
it describes the DRIVER's readiness to mint sessions, not the health of ours.
The probe that actually covers the request path is POST execute/sync on the
warm session entry-webkit.sh created: it fails once the session or MiniBrowser
dies, and succeeds only if the exact session every clone will inherit can still
answer.

Two conditions, both required (same golden-snapshot contract as the Chromium
gate): the warm marker exists (entry-webkit.sh touches it only after a proven
navigate + screenshot + blank transition), and the warm session answers a real
WebDriver round trip.

Deliberately does NOT navigate or screenshot: the health probe runs every
second, and a mutation would thrash the quiescent about:blank state the
snapshot is meant to freeze.
"""

import json
import os
import sys

import health_loop
import urllib.error
import urllib.request

WD_HOST = os.environ.get("BENCH_WD_HEALTH_HOST", "127.0.0.1:9515")
READY_FILE = os.environ.get("BENCH_READY_FILE", "/run/bench-ready")
SESSION_FILE = os.environ.get("BENCH_SESSION_FILE", "/run/bench-session-id")
TIMEOUT = float(os.environ.get("BENCH_WD_HEALTH_TIMEOUT", "3"))


def main_with_reason() -> tuple[int, str]:
    if not os.path.exists(READY_FILE):
        return 1, f"warm marker {READY_FILE} absent"
    try:
        with open(SESSION_FILE) as source:
            session = source.read().strip()
    except OSError as error:
        return 1, f"session file: {error}"
    if not session:
        return 1, f"session file {SESSION_FILE} is empty"

    # execute/sync, NOT GET /url. Automation.getBrowsingContext -- which backs
    # GET /url -- builds its reply from page.pageLoadState().activeURL(), pure
    # UI-process state with no IPC to the web process. A wedged or dead web
    # content process still answers it 200, so using it as the liveness probe
    # would let the golden snapshot fire on a browser that cannot render: the
    # green-by-absence class AGENTS.md names. Evaluating a script proves the web
    # process executes. `return 1` mutates nothing, so the warm point stays the
    # quiescent about:blank the golden requires.
    url = f"http://{WD_HOST}/session/{session}/execute/sync"
    body = json.dumps({"script": "return 1", "args": []}).encode()
    try:
        req = urllib.request.Request(url, data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            value = json.load(resp)["value"]
        if value != 1:
            return 1, f"web process returned {value!r}, expected 1"
    except Exception as error:  # noqa: BLE001 - any transport/protocol failure = unhealthy
        return 1, f"warm-session probe failed: {type(error).__name__}: {error}"

    return 0, f"session={session}"


if __name__ == "__main__":
    # Resident by default, exactly as cdp_health.py is: the per-second
    # `python3 wd_health.py` HEALTHCHECK this file originally carried spends a
    # fresh interpreter per clone per second forever, dirtying ~9 MB of private
    # pages INSIDE the clone -- against the per-clone memory figure this bench
    # exists to measure. health_state.sh is the cheap reader, and reqbench.sh
    # asserts the image's HEALTHCHECK names it.
    if "--loop" in sys.argv:
        sys.exit(health_loop.loop(main_with_reason, "wd_health"))
    code, reason = main_with_reason()
    print(("healthy " if code == 0 else "unhealthy ") + reason,
          file=sys.stdout if code == 0 else sys.stderr)
    sys.exit(code)
