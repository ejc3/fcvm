#!/usr/bin/env python3
"""Prove the host->clone CDP chain on a RESTORED CLONE, hop by hop, on BOTH backends.

A golden VM answering CDP proves nothing about a clone: the clone is a different process,
in a different netns, with port mappings rehydrated from snapshot metadata rather than set
up by `podman run`. This script therefore restores a real clone and walks the chain in the
order the failures actually happen, so a break names its hop instead of looking like
"networking is broken":

  1. state file appears and carries a host-side IP           (fcvm rehydrated port_mappings)
  2. TCP connect to <loopback_ip>:9223 succeeds              (--publish + pasta)
  3. HTTP GET /json/list returns a page target               (socat relay -> loopback 9222)
  4. RFC 6455 upgrade returns 101 and a valid accept key     (the hop that RESETs when the
                                                              relay is missing)
  5. Page.navigate + Page.captureScreenshot returns bytes    (end to end, real pixels)

Trap this exists to disprove, from AGENTS.md: `--remote-debugging-address=0.0.0.0` is
IGNORED by this Chromium build, so a host connect can SUCCEED and then be RESET, which
reads exactly like a Host-header rejection and is not one. Step 3 failing while step 2
passes is that signature.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdpdrive  # noqa: E402

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
FCVM = Path(os.environ.get("FCVM", REPO / "target" / "release" / "fcvm"))
STATE_DIR = Path("/mnt/fcvm-btrfs/state")


def state_by_name(name: str, deadline: float):
    while time.monotonic() < deadline:
        for p in STATE_DIR.glob("*.json"):
            try:
                st = json.loads(p.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            if st.get("name") == name:
                return st
        time.sleep(0.01)
    return None


def endpoint_of(state: dict, port: int) -> str:
    net = (state.get("config") or {}).get("network") or {}
    for key in ("loopback_ip", "host_ip", "guest_ip"):
        if net.get(key):
            return f"{net[key]}:{port}"
    raise RuntimeError(f"no host-side IP in clone network config: {sorted(net)}")


def verify_one(tag: str, serve_pid, url: str, port: int, idx: int) -> dict:
    backend = "uffd" if serve_pid else "file"
    name = f"cdpverify-{backend}-{os.getpid()}-{idx}"
    src = ["--pid", str(serve_pid)] if serve_pid else ["--snapshot", tag]
    log = Path(f"/tmp/{name}.log")
    out: dict = {"backend": backend, "name": name, "log": str(log)}

    t0 = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(
            [str(FCVM), "snapshot", "run", *src, "--name", name,
             "--no-dirty-tracking", "--no-swap"],
            stdout=lf, stderr=lf, stdin=subprocess.DEVNULL,
            env=dict(os.environ, RUST_LOG="fcvm=debug"))
    fcvm_pid = None
    try:
        deadline = t0 + 90
        st = state_by_name(name, deadline)
        if st is None:
            raise TimeoutError("clone state file never appeared")
        fcvm_pid = st.get("pid")
        out["hop1_state"] = "ok"
        out["fcvm_pid"] = fcvm_pid
        ep = endpoint_of(st, port)
        out["endpoint"] = ep

        host, sport = ep.rsplit(":", 1)
        t = time.monotonic()
        while True:
            try:
                s = socket.create_connection((host, int(sport)), 0.25)
                s.close()
                break
            except OSError as e:
                if time.monotonic() > deadline:
                    raise TimeoutError(f"hop2 TCP connect to {ep} never succeeded: {e}") from e
                time.sleep(0.002)
        out["hop2_tcp_ms"] = (time.monotonic() - t) * 1000

        # hop 3/4/5 are exactly what the profile harness will do per request.
        args = argparse.Namespace(
            cdp_host=ep, url=url, format="jpeg", quality=80,
            timeout=max(2.0, deadline - time.monotonic()), idle_wait_ms=0.0,
            out_prefix="", ws_url="", connect_retries=200, nav_timing=True,
            render_module=str(HERE / "render.py"))
        r = cdpdrive.drive(args)
        out["drive"] = r
        stg = r.get("stages", {})
        out["hop3_resolve_ms"] = stg.get("resolve_ms")
        out["hop4_upgrade_ms"] = stg.get("upgrade_ms")
        out["hop5_screenshot_ms"] = stg.get("screenshot_ms")
        out["image_bytes"] = r.get("image_bytes")
        out["dimensions"] = [r.get("width"), r.get("height")]
        # Bytes, and real ones: a JPEG whose SOF frame parses to the configured window
        # WIDTH and a plausible height. A zero-length or non-image response would
        # otherwise read as success.
        #
        # Height is checked as a range, not as 800: `--window-size=1280,800` sets the
        # window, but Page.captureScreenshot captures the LAYOUT VIEWPORT, which is
        # shorter than the window. Measured 657 px, identically on all six clones and
        # both backends. Asserting 800 here failed 6/6 renders that had in fact returned
        # 61,765 bytes of correct JPEG — the assertion was wrong, not the chain.
        out["ok"] = bool(r.get("ok")) and (r.get("image_bytes") or 0) > 1000 \
            and r.get("width") == 1280 and 400 < (r.get("height") or 0) <= 800
        if not out["ok"]:
            out["why"] = r.get("error") or f"bad image: {r.get('image_bytes')}B " \
                                           f"{r.get('width')}x{r.get('height')}"
    except Exception as e:  # noqa: BLE001 - the point is to report the failing hop
        out["ok"] = False
        out["why"] = f"{type(e).__name__}: {e}"
    finally:
        if fcvm_pid:
            try:
                os.kill(fcvm_pid, 15)
            except ProcessLookupError:
                pass
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
    out["wall_ms"] = (time.monotonic() - t0) * 1000
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", required=True)
    ap.add_argument("--url", required=True)
    ap.add_argument("--port", type=int, default=9223)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--json-out", default="")
    args = ap.parse_args()

    results = []
    # FILE backend first: no serve process, so a failure here is purely the CDP chain.
    for i in range(args.reps):
        r = verify_one(args.tag, None, args.url, args.port, i)
        print(json.dumps(r.get("drive", {}).get("stages", {}), separators=(",", ":"))
              if r["ok"] else f"FAIL: {r.get('why')}", flush=True)
        print(f"  file  rep{i}: ok={r['ok']} ep={r.get('endpoint')} "
              f"img={r.get('image_bytes')}B {r.get('dimensions')} "
              f"tcp={r.get('hop2_tcp_ms', 0):.1f}ms", flush=True)
        results.append(r)

    # UFFD backend: same chain, but the clone's RAM is served by a separate process.
    serve = subprocess.Popen([str(FCVM), "snapshot", "serve", args.tag],
                             stdout=open(f"/tmp/cdpverify-serve-{os.getpid()}.log", "wb"),
                             stderr=subprocess.STDOUT,
                             env=dict(os.environ, RUST_LOG="fcvm=debug"))
    try:
        dead = time.time() + 60
        while time.time() < dead:
            found = None
            for p in STATE_DIR.glob("*.json"):
                try:
                    st = json.loads(p.read_text())
                except (OSError, json.JSONDecodeError):
                    continue
                cfg = st.get("config") or {}
                if cfg.get("process_type") == "serve" and st.get("pid") == serve.pid:
                    found = st
                    break
            if found:
                break
            time.sleep(0.2)
        else:
            print("serve never registered", file=sys.stderr)
            return 1
        for i in range(args.reps):
            r = verify_one(args.tag, serve.pid, args.url, args.port, i)
            print(f"  uffd  rep{i}: ok={r['ok']} ep={r.get('endpoint')} "
                  f"img={r.get('image_bytes')}B {r.get('dimensions')} "
                  f"tcp={r.get('hop2_tcp_ms', 0):.1f}ms"
                  + ("" if r["ok"] else f"  why={r.get('why')}"), flush=True)
            results.append(r)
    finally:
        serve.terminate()
        try:
            serve.wait(timeout=60)
        except subprocess.TimeoutExpired:
            serve.kill()

    if args.json_out:
        Path(args.json_out).write_text(json.dumps(results, indent=1))
    nok = sum(1 for r in results if r["ok"])
    print(f"\nCHAIN: {nok}/{len(results)} verified "
          f"({sum(1 for r in results if r['ok'] and r['backend'] == 'file')} file, "
          f"{sum(1 for r in results if r['ok'] and r['backend'] == 'uffd')} uffd)")
    return 0 if nok == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
