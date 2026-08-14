#!/usr/bin/env python3
"""W3C WebDriver classic driver for the WebKit bench — stdlib only.

The WebKit twin of render.py/cdpdrive.py. WebKitGTK has no CDP endpoint, so
navigate + screenshot go over WebDriver's HTTP protocol (one POST per command,
JSON bodies, no WebSocket). Used in three places:

  * entry-webkit.sh (in guest, --create): create the WARM session before the
    golden snapshot, warm-render a fixture, persist the session id. WebDriver
    classic has no session-discovery API, so a session created after restore
    would launch a COLD browser — the id must be minted at the warm point and
    inherited by every clone.
  * wd_health.py (in guest): GET /session/<id>/url as the liveness probe.
  * the host bench arms (default mode): reuse the inherited session id for
    navigate + screenshot against a restored clone.

Timing fields mirror render.py where the protocols correspond:
  connect_ms   first HTTP round trip on this connection (session status read)
  navigate_ms  POST /session/<id>/url (returns after the classic load event)
  screenshot_ms POST returns base64 PNG (WebKitGTK is PNG-only; no quality knob)
  total_ms     whole request from process start
"""

import argparse
import base64
import json
import sys
import time
import urllib.error
import urllib.request

MINIBROWSER = "/usr/local/bin/MiniBrowser"


def mono():
    return time.clock_gettime(time.CLOCK_MONOTONIC)


class WdError(RuntimeError):
    pass


def wd(host, method, path, body=None, timeout=30.0):
    """One WebDriver HTTP round trip; raises WdError for protocol AND transport
    failures (socket timeout, refused, reset), tagged with the command so a
    failure names its stage instead of crashing the entry script's warm gate."""
    url = f"http://{host}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = json.load(resp)
    except urllib.error.HTTPError as error:
        try:
            detail = json.load(error)["value"]
            raise WdError(f"{detail.get('error')}: {detail.get('message')} "
                          f"({method} {path})") from error
        except (ValueError, KeyError):
            raise WdError(f"HTTP {error.code} on {method} {path}") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise WdError(
            f"transport failure on {method} {path} after {timeout}s: "
            f"{type(error).__name__}: {error}"
        ) from error
    return payload["value"]


def create_session(host, timeout=120.0):
    """Mint the warm session: MiniBrowser under --automation, insecure certs OK.

    The generous timeout is for the WARM POINT only (cold MiniBrowser launch
    builds the fontconfig cache on first run); request-path arms never create
    sessions — they inherit the snapshot's."""
    caps = {
        "capabilities": {
            "alwaysMatch": {
                "browserName": "MiniBrowser",
                "acceptInsecureCerts": True,
                "webkitgtk:browserOptions": {
                    "binary": MINIBROWSER,
                    "args": ["--automation"],
                },
            }
        }
    }
    value = wd(host, "POST", "/session", caps, timeout=timeout)
    return value["sessionId"]


def navigate(host, session, url, timeout=120.0):
    wd(host, "POST", f"/session/{session}/url", {"url": url}, timeout=timeout)


def screenshot(host, session, timeout=60.0):
    b64 = wd(host, "GET", f"/session/{session}/screenshot", timeout=timeout)
    return base64.b64decode(b64)


def current_url(host, session, timeout=10.0):
    return wd(host, "GET", f"/session/{session}/url", timeout=timeout)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("url")
    p.add_argument("--host", default="127.0.0.1:9515")
    p.add_argument("--session-file",
                   help="read the warm session id from this file")
    p.add_argument("--session-id", help="explicit session id (overrides file)")
    p.add_argument("--create", action="store_true",
                   help="create a NEW session (warm point only) and write its id "
                        "to --session-file")
    p.add_argument("--out-prefix", required=True)
    p.add_argument("--then-blank", action="store_true",
                   help="after the screenshot, navigate to about:blank and "
                        "verify the session reports it (warm-point quiescence)")
    args = p.parse_args()

    t0 = mono()
    try:
        if args.create:
            if not args.session_file:
                p.error("--create requires --session-file")
            session = create_session(args.host)
            with open(args.session_file, "w") as target:
                target.write(session + "\n")
        else:
            session = args.session_id
            if not session:
                if not args.session_file:
                    p.error("give --session-id or --session-file")
                with open(args.session_file) as source:
                    session = source.read().strip()

        t_connect = mono()
        current_url(args.host, session)
        connect_ms = (mono() - t_connect) * 1000

        t_nav = mono()
        navigate(args.host, session, args.url)
        navigate_ms = (mono() - t_nav) * 1000

        t_shot = mono()
        png = screenshot(args.host, session)
        screenshot_ms = (mono() - t_shot) * 1000
        if not png.startswith(b"\x89PNG"):
            raise WdError(f"screenshot is not a PNG ({png[:8]!r})")
        with open(f"{args.out_prefix}.png", "wb") as target:
            target.write(png)

        if args.then_blank:
            navigate(args.host, session, "about:blank")
            landed = current_url(args.host, session)
            if landed != "about:blank":
                raise WdError(f"blank transition landed on {landed!r}")
    except WdError as error:
        total_ms = (mono() - t0) * 1000
        print(f"RENDER_FAIL url={args.url} error={error} total_ms={total_ms:.1f}")
        return 1

    total_ms = (mono() - t0) * 1000
    print(
        f"RENDER_OK url={args.url} connect_ms={connect_ms:.1f} "
        f"navigate_ms={navigate_ms:.1f} screenshot_ms={screenshot_ms:.1f} "
        f"total_ms={total_ms:.1f} shot_fmt=png png_bytes={len(png)} "
        f"png={args.out_prefix}.png session={session}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
