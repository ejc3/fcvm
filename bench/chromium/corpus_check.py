#!/usr/bin/env python3
"""Live-vs-local equivalence check for the mirrored corpus.

For each mirrored site, render BOTH the live URL and the local mirror in the
same bare-metal bench container (host network, Chromium's own CDP), capturing:
  - a HAR-lite request log (CDP Network events: url, status, mime, bytes)
  - the DOM text (post-load innerText)
  - a viewport screenshot (PNG, fixed window)

Then compare, per site: resource counts and bytes by type, failed/missing
local requests, DOM-text similarity, and screenshot pixel-diff ratio. Live
pages move (headlines rotate between mirror time and check time), so the
verdict thresholds are structural-lenient and every metric is recorded — the
report is the evidence, the verdict is a summary.

Run (container already warm, hostserver NOT running — this script serves):
  python3 corpus_check.py --out results/corpus-check-$(date +%m%d-%H%M%S)

stdlib + PIL (PIL for pixel diff; degrades to dimension check without it).
"""

import argparse
import base64
import difflib
import io
import json
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cdpdrive  # resolve_target, TimedWs

HERE = Path(__file__).resolve().parent


# ── CDP session on cdpdrive/render.py's real primitives ─────────────────────
RENDER = cdpdrive.load_render(str(HERE / "render.py"))


def render(host: str, url: str, out_prefix: Path) -> dict:
    deadline = time.monotonic() + 75.0
    target = cdpdrive.resolve_target(host, deadline, 40)
    ws = cdpdrive.TimedWs(RENDER, target["webSocketDebuggerUrl"], deadline)
    cdp = RENDER.Cdp(ws)
    cdp.cmd("Network.enable", deadline=deadline)
    cdp.cmd("Page.enable", deadline=deadline)
    nav = cdp.cmd("Page.navigate", {"url": url}, deadline=deadline)
    if "errorText" in nav:
        raise RuntimeError(f"navigation failed: {nav['errorText']}")
    cdp.wait_event(lambda ev: ev["method"] == "Page.loadEventFired", deadline)
    # Drain post-load network activity into the stash: wait ~4s for an event
    # that never matches; every real event lands in cdp.events.
    try:
        cdp.wait_event(lambda ev: False, min(deadline, time.monotonic() + 4.0))
    except TimeoutError:
        pass

    reqs, resps, sizes, fails = {}, {}, {}, {}
    for ev in cdp.events:
        m, p = ev["method"], ev.get("params", {})
        rid = p.get("requestId")
        if m == "Network.requestWillBeSent":
            reqs[rid] = p.get("request", {}).get("url", "")
        elif m == "Network.responseReceived":
            r = p.get("response", {})
            resps[rid] = {"status": r.get("status"), "mime": r.get("mimeType", "")}
        elif m == "Network.loadingFinished":
            sizes[rid] = p.get("encodedDataLength", 0)
        elif m == "Network.loadingFailed":
            fails[rid] = p.get("errorText", "failed")
    har = []
    for rid, u in reqs.items():
        e = {"url": u}
        e.update(resps.get(rid, {}))
        e["bytes"] = sizes.get(rid, 0)
        if rid in fails:
            e["failed"] = fails[rid]
        har.append(e)

    dom = cdp.cmd("Runtime.evaluate", {
        "expression": "document.documentElement.innerText.slice(0, 200000)",
        "returnByValue": True,
    }, deadline=deadline).get("result", {}).get("value", "") or ""

    shot = cdp.cmd("Page.captureScreenshot", {"format": "png"}, deadline=deadline)
    png = base64.b64decode(shot.get("data", "")) if shot.get("data") else b""

    out_prefix.parent.mkdir(parents=True, exist_ok=True)
    (out_prefix.with_suffix(".har.json")).write_text(json.dumps(har, indent=1))
    (out_prefix.with_suffix(".txt")).write_text(dom)
    (out_prefix.with_suffix(".png")).write_bytes(png)
    try:
        ws.sock.close()
    except Exception:  # noqa: BLE001
        pass
    return {"har": har, "dom": dom, "png": png}


# ── comparison metrics ──────────────────────────────────────────────────────
def har_summary(har):
    ok = [e for e in har if not e.get("failed") and (e.get("status") or 0) < 400]
    bad = [e for e in har if e.get("failed") or (e.get("status") or 0) >= 400]
    by_type = {}
    for e in ok:
        t = (e.get("mime") or "?").split("/")[0]
        by_type[t] = by_type.get(t, 0) + 1
    return {"ok": len(ok), "bad": len(bad), "bytes": sum(e["bytes"] for e in ok),
            "by_type": by_type, "bad_urls": [e["url"][:120] for e in bad][:10]}


def dom_similarity(a: str, b: str) -> float:
    na = re.sub(r"\s+", " ", a).strip()[:100000]
    nb = re.sub(r"\s+", " ", b).strip()[:100000]
    if not na and not nb:
        return 1.0
    return difflib.SequenceMatcher(None, na, nb).quick_ratio()


def pixel_diff(a: bytes, b: bytes):
    try:
        from PIL import Image, ImageChops
    except ImportError:
        return {"available": False}
    try:
        ia, ib = Image.open(io.BytesIO(a)).convert("RGB"), Image.open(io.BytesIO(b)).convert("RGB")
    except Exception as e:  # noqa: BLE001
        return {"available": False, "error": str(e)}
    if ia.size != ib.size:
        ib = ib.resize(ia.size)
    diff = ImageChops.difference(ia, ib)
    hist = diff.convert("L").histogram()
    changed = sum(hist[16:])  # pixels differing by >16/255 luminance
    total = ia.size[0] * ia.size[1]
    return {"available": True, "size": ia.size, "changed_frac": round(changed / total, 4)}


# ── entry-file resolution for a mirrored site ───────────────────────────────
def local_entry(corpus_dir: Path, key: str, url: str):
    import urllib.parse
    u = urllib.parse.urlparse(url)
    base = corpus_dir / key
    cands = []
    path = u.path.strip("/")
    host_dir = base / u.netloc
    if path:
        cands += [host_dir / path / "index.html", host_dir / (path + ".html"), host_dir / path]
    cands += [host_dir / "index.html"]
    for c in cands:
        if c.is_file():
            return c.relative_to(corpus_dir)
    htmls = sorted(base.rglob("*.html"), key=lambda p: -p.stat().st_size)
    return htmls[0].relative_to(corpus_dir) if htmls else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus-dir", default=str(HERE / "corpus"))
    ap.add_argument("--out", required=True)
    ap.add_argument("--cdp", default="127.0.0.1:9222")
    # NOT 8000: the bench container on host network binds :8000 with its own
    # in-guest pageserver, and losing that bind race serves the wrong site
    # (smoke run 0813: local arm got 404s from the container's server).
    ap.add_argument("--http-port", type=int, default=8899)
    ap.add_argument("--sites", default="", help="comma list of keys; default all")
    args = ap.parse_args()

    corpus = Path(args.corpus_dir)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    manifest = [json.loads(l) for l in (corpus / "MANIFEST.jsonl").read_text().splitlines()]
    picks = {s.strip() for s in args.sites.split(",") if s.strip()}

    srv = subprocess.Popen(
        [sys.executable, str(HERE / "hostserver.py"), "--root", str(corpus),
         "--port", str(args.http_port)],
        stdout=open(out / "hostserver.log", "w"), stderr=subprocess.STDOUT)
    time.sleep(1.0)

    rows = []
    try:
        for m in manifest:
            key, url = m["key"], m["url"]
            if picks and key not in picks:
                continue
            entry = local_entry(corpus, key, url)
            if entry is None:
                rows.append({"key": key, "verdict": "NO-ENTRY", "url": url})
                continue
            local_url = f"http://127.0.0.1:{args.http_port}/{entry.as_posix()}"
            print(f"── {key}", flush=True)
            try:
                live = render(args.cdp, url, out / key / "live")
                time.sleep(0.5)
                loc = render(args.cdp, local_url, out / key / "local")
            except Exception as e:  # noqa: BLE001
                rows.append({"key": key, "verdict": "RENDER-FAIL", "error": str(e)[:200]})
                continue
            hl, hc = har_summary(live["har"]), har_summary(loc["har"])
            sim = dom_similarity(live["dom"], loc["dom"])
            px = pixel_diff(live["png"], loc["png"])
            row = {
                "key": key, "url": url, "local_url": local_url,
                "live_requests": hl, "local_requests": hc,
                "dom_similarity": round(sim, 3), "pixels": px,
            }
            # Structural-lenient verdict: local must serve most of its own
            # resources successfully and look/read like the live page did.
            ok_ratio = hc["ok"] / max(1, hl["ok"])
            visual_ok = (not px.get("available")) or px.get("changed_frac", 1) < 0.35
            row["verdict"] = "EQUIVALENT" if (
                hc["bad"] <= max(2, hl["bad"]) and ok_ratio >= 0.5
                and sim >= 0.55 and visual_ok
            ) else "DIVERGENT"
            rows.append(row)
            print(f"   {row['verdict']}  dom={sim:.2f} px={px.get('changed_frac','n/a')} "
                  f"local_ok={hc['ok']}/{hl['ok']} local_bad={hc['bad']}", flush=True)
    finally:
        srv.terminate()

    (out / "report.json").write_text(json.dumps(rows, indent=1))
    eq = sum(1 for r in rows if r.get("verdict") == "EQUIVALENT")
    print(f"\n{eq}/{len(rows)} sites equivalent; full metrics in {out}/report.json")
    return 0 if eq == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
