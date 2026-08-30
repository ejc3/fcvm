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
"""

import argparse
import base64
import json
import sys
import time
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


def capture(host: str, url: str, dest: Path) -> dict:
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

    bodies_dir = dest / "bodies"
    bodies_dir.mkdir(parents=True, exist_ok=True)
    index, n, fetched_bytes = {}, 0, 0
    for rid in finished:
        u = meta.get(rid, {}).get("url", "")
        if not u.startswith("http") or u in index:
            continue
        try:
            body = cdp.cmd("Network.getResponseBody", {"requestId": rid},
                           deadline=min(deadline, time.monotonic() + 15))
        except Exception:  # noqa: BLE001 - body evicted or non-cacheable; recorded as miss
            continue
        raw = body.get("body", "")
        data = base64.b64decode(raw) if body.get("base64Encoded") else raw.encode()
        fname = f"{n:04d}"
        (bodies_dir / fname).write_bytes(data)
        index[u] = {"file": f"bodies/{fname}",
                    "status": meta[rid].get("status", 200),
                    "mime": meta[rid].get("mime", ""),
                    "bytes": len(data)}
        n += 1
        fetched_bytes += len(data)

    dom = cdp.cmd("Runtime.evaluate", {
        "expression": "document.documentElement.innerText.slice(0, 200000)",
        "returnByValue": True,
    }, deadline=deadline).get("result", {}).get("value", "") or ""
    shot = cdp.cmd("Page.captureScreenshot", {"format": "png"},
                   deadline=min(deadline, time.monotonic() + 45))
    (dest / "live.txt").write_text(dom)
    (dest / "live.png").write_bytes(base64.b64decode(shot.get("data", "")))
    (dest / "index.json").write_text(json.dumps(
        {"url": url, "captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
         "redirects": redirects, "resources": index}, indent=1))
    try:
        ws.sock.close()
    except Exception:  # noqa: BLE001
        pass
    return {"resources": len(index), "bytes": fetched_bytes}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(HERE / "corpus-live"))
    ap.add_argument("--cdp", default="127.0.0.1:9222")
    ap.add_argument("--sites", default="")
    args = ap.parse_args()
    picks = {s.strip() for s in args.sites.split(",") if s.strip()}
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
            r = capture(args.cdp, url, tmp)
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
