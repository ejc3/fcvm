#!/usr/bin/env python3
"""Deep corpus capture: record every response body a LIVE render fetches.

Drives the bench container's Chromium (CDP, Network domain) at each corpus
URL, then pulls Network.getResponseBody for every finished request and stores
body + status + mime keyed by full URL in corpus-live/<key>/:

    index.json    url -> {file, status, mime, base64}
    bodies/NNNN   raw bytes
    live.txt      post-load DOM innerText (equivalence reference)
    live.png      viewport screenshot     (equivalence reference)

No URL rewriting anywhere: the replay server (corpus_serve.py) resolves the
ORIGINAL urls by Host+path, and the replay Chromium maps all hosts to
127.0.0.1. What the live render fetched is exactly what the local render can
fetch — including JS-fetched chunks, root-relative references inside scripts,
and third-party beacons — which is the fidelity wget-style mirroring cannot
reach (2026-08-13: 11/14 sites divergent that way).

The corpus is committed to a public repository, and some responses echo the
client's address back (ad geo lookups return {"client_ip": ...}). Before any
of it is written, the capture replaces this host's own public IPv4 address,
learned at capture time, with an RFC 5737 documentation address in every
stored body, URL, redirect and the DOM text. It refuses to capture when it
cannot learn that address. live.png is stored as rendered: a page that draws
the address as pixels is not scrubbed, so look at the screenshots before
committing a recapture. test_corpus_addresses.py checks the committed corpus.
"""

import argparse
import base64
import ipaddress
import json
import re
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cdpdrive
import report

HERE = Path(__file__).resolve().parent
RENDER = cdpdrive.load_render(str(HERE / "render.py"))

# The corpus, defined once in report.py: the comparator block there claims a
# shared workload only for a run that rendered exactly these pages, so the
# capture and the claim read the same list.
URLS = report.KITESURF_CORPUS

# RFC 5737 addresses of 9 to 14 characters, one per length, so a scrubbed
# address keeps its length and a body keeps its size.
DOCUMENTATION_ADDRESSES = ("192.0.2.1", "192.0.2.10", "192.0.2.100",
                           "198.51.100.1", "198.51.100.10", "198.51.100.100")

IMDS = "http://169.254.169.254/latest"
ECHO_URL = "https://checkip.amazonaws.com/"


def documentation_address(address: str) -> str:
    for doc in DOCUMENTATION_ADDRESSES:
        if len(doc) == len(address):
            return doc
    return DOCUMENTATION_ADDRESSES[0]


def scrub_addresses(data: bytes, addresses) -> bytes:
    """Replace each address, dotted and dashed (EC2 host names), with a documentation address."""
    for address in addresses:
        doc = documentation_address(address)
        for sep in (".", "-"):
            found = sep.join(address.split(".")).encode()
            pattern = re.compile(rb"(?<![0-9])" + re.escape(found) + rb"(?![0-9])")
            data = pattern.sub(sep.join(doc.split(".")).encode(), data)
    return data


def _scrub_text(text: str, addresses) -> str:
    return scrub_addresses(text.encode(), addresses).decode()


def _imds_public_ipv4() -> str:
    direct = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    token = direct.open(urllib.request.Request(
        f"{IMDS}/api/token", method="PUT",
        headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"}), timeout=2).read().decode()
    return direct.open(urllib.request.Request(
        f"{IMDS}/meta-data/public-ipv4",
        headers={"X-aws-ec2-metadata-token": token}), timeout=2).read().decode()


def _echoed_public_ipv4() -> str:
    with urllib.request.urlopen(ECHO_URL, timeout=5) as r:
        return r.read().decode()


def own_public_addresses(explicit, probes=(_imds_public_ipv4, _echoed_public_ipv4)) -> list:
    """This host's public IPv4 addresses: the explicit ones plus every probe's answer.

    A probe that fails or answers with anything but a public IPv4 address adds
    nothing. Raises when the result is empty, because a capture that cannot
    name the address cannot remove it.
    """
    found = []
    for address in explicit:
        if not ipaddress.IPv4Address(address).is_global:
            raise ValueError(f"--own-address {address!r} is not a public IPv4 address")
        found.append(address)
    for probe in probes:
        try:
            address = str(ipaddress.IPv4Address(probe().strip()))
        except Exception:  # noqa: BLE001 - an absent answer is handled below
            continue
        if ipaddress.IPv4Address(address).is_global and address not in found:
            found.append(address)
    if not found:
        raise RuntimeError("cannot learn this host's public IPv4 address, so cannot "
                           "scrub it from the capture; pass --own-address")
    return found


def write_capture(dest: Path, url: str, resources, redirects: dict, dom: str,
                  png: bytes, own_addresses) -> dict:
    """Store one site's capture under dest with own_addresses scrubbed.

    resources is a list of (url, body bytes, status, mime) in fetch order.
    """
    bodies_dir = dest / "bodies"
    bodies_dir.mkdir(parents=True, exist_ok=True)
    index, fetched_bytes = {}, 0
    for n, (u, data, status, mime) in enumerate(resources):
        data = scrub_addresses(data, own_addresses)
        fname = f"{n:04d}"
        (bodies_dir / fname).write_bytes(data)
        index[_scrub_text(u, own_addresses)] = {
            "file": f"bodies/{fname}", "status": status, "mime": mime, "bytes": len(data)}
        fetched_bytes += len(data)
    redirects = {
        _scrub_text(src, own_addresses): {
            "status": hop["status"],
            "location": _scrub_text(hop["location"], own_addresses)}
        for src, hop in redirects.items()}
    (dest / "live.txt").write_text(_scrub_text(dom, own_addresses))
    (dest / "live.png").write_bytes(png)
    (dest / "index.json").write_text(json.dumps(
        {"url": url, "captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
         "redirects": redirects, "resources": index}, indent=1))
    return {"resources": len(index), "bytes": fetched_bytes}


def capture(host: str, url: str, dest: Path, own_addresses) -> dict:
    deadline = time.monotonic() + 90.0
    target = cdpdrive.resolve_target(host, deadline, 40)
    ws = cdpdrive.TimedWs(RENDER, target["webSocketDebuggerUrl"], deadline)
    cdp = RENDER.Cdp(ws)
    # Big buffers: news pages ship multi-MB documents and bundles.
    cdp.cmd("Network.enable", {"maxTotalBufferSize": 300_000_000,
                               "maxResourceBufferSize": 100_000_000}, deadline=deadline)
    cdp.cmd("Page.enable", deadline=deadline)
    nav = cdp.cmd("Page.navigate", {"url": url}, deadline=deadline)
    if "errorText" in nav:
        raise RuntimeError(f"navigation failed: {nav['errorText']}")
    cdp.wait_event(lambda ev: ev["method"] == "Page.loadEventFired", deadline)
    try:  # drain post-load fetches into the stash
        cdp.wait_event(lambda ev: False, min(deadline, time.monotonic() + 6.0))
    except TimeoutError:
        pass

    finished, meta, redirects = [], {}, {}
    for ev in cdp.events:
        m, p = ev["method"], ev.get("params", {})
        rid = p.get("requestId")
        if m == "Network.requestWillBeSent":
            # A redirect surfaces as a new requestWillBeSent carrying the
            # previous hop's response; record the hop so replay can serve it
            # (the redirect itself has no body to capture).
            rr = p.get("redirectResponse")
            if rr:
                redirects[rr.get("url", "")] = {
                    "status": rr.get("status", 302),
                    "location": p.get("request", {}).get("url", ""),
                }
            meta.setdefault(rid, {})["url"] = p.get("request", {}).get("url", "")
        elif m == "Network.responseReceived":
            r = p.get("response", {})
            meta.setdefault(rid, {}).update(status=r.get("status"),
                                            mime=r.get("mimeType", ""))
        elif m == "Network.loadingFinished":
            finished.append(rid)

    resources, seen = [], set()
    for rid in finished:
        u = meta.get(rid, {}).get("url", "")
        if not u.startswith("http") or u in seen:
            continue
        try:
            body = cdp.cmd("Network.getResponseBody", {"requestId": rid},
                           deadline=min(deadline, time.monotonic() + 15))
        except Exception:  # noqa: BLE001 - body evicted or non-cacheable; recorded as miss
            continue
        raw = body.get("body", "")
        data = base64.b64decode(raw) if body.get("base64Encoded") else raw.encode()
        seen.add(u)
        resources.append((u, data, meta[rid].get("status", 200), meta[rid].get("mime", "")))

    dom = cdp.cmd("Runtime.evaluate", {
        "expression": "document.documentElement.innerText.slice(0, 200000)",
        "returnByValue": True,
    }, deadline=deadline).get("result", {}).get("value", "") or ""
    shot = cdp.cmd("Page.captureScreenshot", {"format": "png"},
                   deadline=min(deadline, time.monotonic() + 45))
    result = write_capture(dest, url, resources, redirects, dom,
                           base64.b64decode(shot.get("data", "")), own_addresses)
    try:
        ws.sock.close()
    except Exception:  # noqa: BLE001
        pass
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(HERE / "corpus-live"))
    ap.add_argument("--cdp", default="127.0.0.1:9222")
    ap.add_argument("--sites", default="")
    ap.add_argument("--own-address", action="append", default=[],
                    help="this host's public IPv4 address, in addition to the ones "
                         "read from EC2 instance metadata and " + ECHO_URL)
    args = ap.parse_args()
    picks = {s.strip() for s in args.sites.split(",") if s.strip()}
    try:
        own = own_public_addresses(args.own_address)
    except (RuntimeError, ValueError) as e:
        print(f"BLOCKED: {e}", flush=True)
        return 2
    print(f"scrubbing {len(own)} own public address(es) from every stored file", flush=True)
    out = Path(args.out)
    failed = []
    for key, url in URLS.items():
        if picks and key not in picks:
            continue
        # Capture into a temp dir and swap on success: a failed capture must
        # not leave a half-written mixture that a later replay check reads as
        # a same-session reference. On failure the previous capture (with its
        # own accurate captured_utc) stays in place and the run exits nonzero
        # naming the site.
        import shutil, uuid
        tmp = out / f".{key}.tmp-{uuid.uuid4().hex[:8]}"
        tmp.mkdir(parents=True, exist_ok=True)
        print(f"── {key}", flush=True)
        try:
            r = capture(args.cdp, url, tmp, own)
            dest = out / key
            if dest.exists():
                shutil.rmtree(dest)
            tmp.rename(dest)
            print(f"   captured {r['resources']} resources, {r['bytes']} bytes", flush=True)
        except Exception as e:  # noqa: BLE001
            shutil.rmtree(tmp, ignore_errors=True)
            failed.append(key)
            print(f"   CAPTURE-FAIL: {e}", flush=True)
        time.sleep(0.5)
    if failed:
        print(f"FAILED captures: {','.join(failed)}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
