#!/usr/bin/env python3
"""Is `cdp_resolve` removable by pre-wiring the WebSocket URL? Measured, not argued.

The CDP target id is identical on every clone restored from one golden, so the
`/json/list` lookup CAN be hoisted out of the request: build
`ws://<clone_ip>:<port>/devtools/page/<baked-in-id>` directly and skip the HTTP round trip
entirely (`cdpdrive --ws-url`).

The question is whether that SAVES the stage or merely MOVES it. `probe_readiness.py`
showed the first HTTP round trip is where the guest-readiness wait gets charged, because
pasta completes the TCP handshake ~60 ms before the guest can serve. If readiness is the
dominant term, deleting the lookup just relocates the wait into the WebSocket upgrade and
the total does not move.

Two arms, interleaved from a seeded shuffle, one clone per request, file-backed only (the
question is about the request path, not the memory backend):

  lookup    resolve the target via /json/list, then connect   (today's path)
  prewired  skip /json/list, connect straight to the baked id

Reported on "time from spawn to image in hand", which is the only number a caller feels.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cdpdrive  # noqa: E402

FCVM = Path(os.environ.get("FCVM", HERE.parent.parent / "target" / "release" / "fcvm"))
STATE_DIR = Path("/mnt/fcvm-btrfs/state")


def one(tag, url, port, target_id, prewired, idx, fmt, qual):
    name = f"prewire-{'pre' if prewired else 'lkp'}-{os.getpid()}-{idx}"
    log = Path(f"/tmp/{name}.log")
    rec = {"arm": "prewired" if prewired else "lookup", "rep": idx, "name": name}
    t0 = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(
            [str(FCVM), "snapshot", "run", "--snapshot", tag, "--name", name,
             "--no-dirty-tracking", "--no-swap"],
            stdout=lf, stderr=lf, stdin=subprocess.DEVNULL,
            env=dict(os.environ, RUST_LOG="fcvm=debug"))
    fcvm_pid = None
    try:
        deadline = t0 + 120
        st = None
        while time.monotonic() < deadline:
            for p in STATE_DIR.glob("*.json"):
                try:
                    s = json.loads(p.read_text())
                except (OSError, json.JSONDecodeError):
                    continue
                if s.get("name") == name:
                    st = s
                    break
            if st:
                break
            time.sleep(0.002)
        if not st:
            raise TimeoutError("no state file")
        fcvm_pid = st.get("pid")
        ip = st["config"]["network"]["loopback_ip"]
        ep = f"{ip}:{port}"
        while time.monotonic() < deadline:
            try:
                c = socket.create_connection((ip, port), 0.25)
                c.close()
                break
            except OSError:
                time.sleep(0.001)
        ws = f"ws://{ep}/devtools/page/{target_id}" if prewired else ""
        res = cdpdrive.drive(argparse.Namespace(
            cdp_host=ep, url=url, format=fmt, quality=qual,
            timeout=max(2.0, deadline - time.monotonic()), idle_wait_ms=0.0,
            out_prefix="", ws_url=ws, connect_retries=200, nav_timing=False,
            render_module=str(HERE / "render.py")))
        rec["stages"] = res.get("stages")
        rec["ok"] = bool(res.get("ok")) and (res.get("image_bytes") or 0) > 1000
        rec["image_bytes"] = res.get("image_bytes")
        rec["error"] = res.get("error")
        # The caller's number: spawn -> image decoded. Teardown is deliberately excluded,
        # it is identical on both arms and would only add variance.
        rec["blocking_ms"] = (time.monotonic() - t0) * 1000
    except Exception as e:  # noqa: BLE001
        rec["ok"] = False
        rec["error"] = f"{type(e).__name__}: {e}"
        rec["blocking_ms"] = (time.monotonic() - t0) * 1000
    finally:
        if fcvm_pid:
            try:
                os.kill(fcvm_pid, 15)
            except ProcessLookupError:
                pass
        try:
            proc.wait(timeout=90)
        except subprocess.TimeoutExpired:
            proc.kill()
    return rec


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", required=True)
    ap.add_argument("--url", required=True)
    ap.add_argument("--target-id", required=True)
    ap.add_argument("--port", type=int, default=9223)
    ap.add_argument("--reps", type=int, default=10)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--fmt", default="jpeg")
    ap.add_argument("--qual", type=int, default=80)
    ap.add_argument("--seed", default="prewire")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    recs = []
    for i in range(1, args.warmup + args.reps + 1):
        block = [True, False]
        random.Random(f"{args.seed}:{i}").shuffle(block)
        for pre in block:
            r = one(args.tag, args.url, args.port, args.target_id, pre, i, args.fmt, args.qual)
            r["warmup"] = i <= args.warmup
            recs.append(r)
            print(f"[prewire] r{i} {r['arm']:8s} ok={r['ok']} "
                  f"blocking={r['blocking_ms']:.0f}ms "
                  f"resolve={(r.get('stages') or {}).get('resolve_ms', 0):.1f} "
                  f"upgrade={(r.get('stages') or {}).get('upgrade_ms', 0):.1f}"
                  + ("" if r["ok"] else f" err={r.get('error')}"), flush=True)
            with open(out / "prewire.jsonl", "a") as f:
                f.write(json.dumps(r) + "\n")
            time.sleep(0.5)

    def med(arm, key, sub=None):
        vs = []
        for r in recs:
            if r["warmup"] or not r["ok"] or r["arm"] != arm:
                continue
            v = (r.get("stages") or {}).get(sub) if sub else r.get(key)
            if v is not None:
                vs.append(v)
        return statistics.median(vs) if vs else None

    summary = {}
    for arm in ("lookup", "prewired"):
        summary[arm] = {
            "n": sum(1 for r in recs if r["arm"] == arm and r["ok"] and not r["warmup"]),
            "blocking_ms": med(arm, "blocking_ms"),
            "resolve_ms": med(arm, None, "resolve_ms"),
            "upgrade_ms": med(arm, None, "upgrade_ms"),
            "connect_total_ms": med(arm, None, "connect_total_ms"),
            "navigate_ms": med(arm, None, "navigate_ms"),
        }
    lk, pw = summary["lookup"], summary["prewired"]
    summary["delta_blocking_ms"] = (pw["blocking_ms"] - lk["blocking_ms"]) \
        if (pw["blocking_ms"] and lk["blocking_ms"]) else None
    summary["resolve_removed_ms"] = lk["resolve_ms"]
    # If the saving is much smaller than the lookup it removed, the wait MOVED.
    summary["verdict"] = None
    if summary["delta_blocking_ms"] is not None and lk["resolve_ms"]:
        frac = -summary["delta_blocking_ms"] / lk["resolve_ms"]
        summary["fraction_of_resolve_actually_saved"] = frac
        summary["verdict"] = ("saving is real" if frac > 0.7 else
                              "wait MOVED, not removed" if frac < 0.3 else "partial")
    json.dump({"summary": summary, "records": recs}, open(out / "prewire.json", "w"), indent=1)
    print(json.dumps(summary, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
