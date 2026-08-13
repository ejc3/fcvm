#!/usr/bin/env python3
"""Render each corpus URL against the replay server and compare with the
reference captured in the SAME live session (corpus-live/<key>/live.{txt,png}).

The replay Chromium maps all hosts to 127.0.0.1 (CHROMIUM_EXTRA_FLAGS in the
bench container), so this renders the ORIGINAL urls with zero rewriting;
corpus_serve.py replays the captured bodies. Reference and replay share one
live session, so content drift between runs cannot masquerade as divergence.
"""

import argparse
import json
import sys
import time
from pathlib import Path

# The visual check is part of the verdict; without Pillow it would silently
# pass (fail-open), so its absence is a startup error, not a degraded mode.
import PIL  # noqa: F401  (hard requirement)

sys.path.insert(0, str(Path(__file__).resolve().parent))
from corpus_check import dom_similarity, har_summary, pixel_diff, render  # noqa: E402
from corpus_capture import URLS  # noqa: E402

HERE = Path(__file__).resolve().parent


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--live-dir", default=str(HERE / "corpus-live"))
    ap.add_argument("--out", required=True)
    ap.add_argument("--cdp", default="127.0.0.1:9222")
    ap.add_argument("--sites", default="")
    args = ap.parse_args()
    live_root = Path(args.live_dir)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    picks = {s.strip() for s in args.sites.split(",") if s.strip()}

    rows = []
    for key, url in URLS.items():
        if picks and key not in picks:
            continue
        ref = live_root / key
        if not (ref / "index.json").is_file():
            rows.append({"key": key, "verdict": "NO-CAPTURE"})
            continue
        print(f"── {key}", flush=True)
        try:
            rep = render(args.cdp, url, out / key / "replay")
        except Exception as e:  # noqa: BLE001
            rows.append({"key": key, "verdict": "RENDER-FAIL", "error": str(e)[:200]})
            continue
        ref_dom = (ref / "live.txt").read_text()
        ref_png = (ref / "live.png").read_bytes()
        h = har_summary(rep["har"])
        sim = dom_similarity(ref_dom, rep["dom"])
        px = pixel_diff(ref_png, rep["png"])
        captured = json.loads((ref / "index.json").read_text())["resources"]
        n_captured = len(captured)
        # Faithfulness, not blanket success: a replayed 404 whose CAPTURE was a
        # 404 is fidelity (the live favicon 404'd too), while a 200 where the
        # capture had 404 — or any network-level failure — is divergence.
        import urllib.parse
        cap_status = {}
        for cu, meta in captured.items():
            pu = urllib.parse.urlparse(cu)
            cap_status[(pu.netloc, pu.path, pu.query)] = meta.get("status", 200)
            cap_status.setdefault((pu.netloc, pu.path), meta.get("status", 200))
        faithful = unfaithful = 0
        for e in rep["har"]:
            pu = urllib.parse.urlparse(e.get("url", ""))
            want = cap_status.get((pu.netloc, pu.path, pu.query),
                                  cap_status.get((pu.netloc, pu.path)))
            if e.get("failed"):
                unfaithful += 1
            elif want is not None and e.get("status") == want:
                faithful += 1
            elif want is None and (e.get("status") or 0) < 400:
                faithful += 1  # request live never made but replay satisfied
            else:
                unfaithful += 1
        h["faithful"] = faithful
        h["unfaithful"] = unfaithful
        row = {"key": key, "url": url,
               "replay_requests": h, "captured_resources": n_captured,
               "dom_similarity": round(sim, 3), "pixels": px}
        # Fail closed on every axis: an unavailable pixel diff blocks; failed
        # or missing replay requests block even when text/pixels look right —
        # a replay that exercises a different request set is a different
        # working set, and measurements from it would be about the wrong
        # thing. Analytics-tier failures are tolerated to a bounded fraction.
        visual_ok = px.get("available") and px.get("changed_frac", 1) < 0.10
        coverage_ok = h["faithful"] >= 0.85 * max(1, n_captured)
        failures_ok = h["unfaithful"] <= max(3, 0.08 * max(1, n_captured))
        row["verdict"] = "EQUIVALENT" if (
            sim >= 0.90 and visual_ok and coverage_ok and failures_ok
        ) else "DIVERGENT"
        rows.append(row)
        print(f"   {row['verdict']}  dom={sim:.2f} px={px.get('changed_frac','n/a')} "
              f"faithful={h['faithful']} unfaithful={h['unfaithful']} captured={n_captured}", flush=True)
        time.sleep(0.5)

    (out / "report.json").write_text(json.dumps(rows, indent=1))
    eq = sum(1 for r in rows if r.get("verdict") == "EQUIVALENT")
    print(f"\n{eq}/{len(rows)} equivalent; metrics in {out}/report.json")
    return 0 if rows and eq == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
