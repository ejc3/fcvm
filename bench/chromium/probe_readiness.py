#!/usr/bin/env python3
"""Where does the cdp path actually WAIT for the guest?

A negative `port_wait` stage in the profile said the clone's forwarded CDP port answers a
TCP connect at or before the moment fcvm logs "VM resume completed". Two explanations fit,
and they have opposite consequences:

  (a) log-read skew  -- the mark is stamped when the harness DRAINS the line, not when fcvm
      wrote it, so port_wait differences a lagged mark against a live one.
  (b) eager accept   -- pasta owns the host-side listener and completes the TCP handshake
      itself, before the guest is able to serve anything. Then a successful connect is NOT
      a readiness signal at all, and the real wait has moved somewhere else.

This separates them by racing three probes on ONE clone from the instant its state file
appears: first successful TCP connect, first successful HTTP /json/list, and (for scale)
the moment fcvm reports resume. If TCP succeeds far earlier than HTTP, (b) holds and the
readiness wait lives inside the first HTTP round trip.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
FCVM = Path(os.environ.get("FCVM", HERE.parent.parent / "target" / "release" / "fcvm"))
STATE_DIR = Path("/mnt/fcvm-btrfs/state")


def main() -> int:
    tag, port = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 9223
    name = f"probe-ready-{os.getpid()}"
    log = Path(f"/tmp/{name}.log")
    t0 = time.monotonic()
    with open(log, "wb") as lf:
        proc = subprocess.Popen(
            [str(FCVM), "snapshot", "run", "--snapshot", tag, "--name", name,
             "--no-dirty-tracking", "--no-swap"],
            stdout=lf, stderr=lf, stdin=subprocess.DEVNULL,
            env=dict(os.environ, RUST_LOG="fcvm=debug"))
    fcvm_pid = None
    try:
        st = None
        while time.monotonic() - t0 < 90:
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
            print("no state file")
            return 1
        t_state = time.monotonic() - t0
        fcvm_pid = st.get("pid")
        ip = st["config"]["network"]["loopback_ip"]

        t_tcp = None
        while time.monotonic() - t0 < 90:
            try:
                c = socket.create_connection((ip, port), 0.25)
                c.close()
                t_tcp = time.monotonic() - t0
                break
            except OSError:
                time.sleep(0.001)

        t_http = None
        attempts = 0
        while time.monotonic() - t0 < 90:
            attempts += 1
            try:
                with urllib.request.urlopen(f"http://{ip}:{port}/json/list", timeout=5) as r:
                    json.load(r)
                t_http = time.monotonic() - t0
                break
            except Exception:  # noqa: BLE001 - any failure means "not ready yet"
                time.sleep(0.001)

        txt = log.read_text(errors="replace")
        resumed = "VM resume completed" in txt

        def ms(v):
            return "   never" if v is None else f"{v * 1000:7.1f}"

        print(f"state file appeared   : {ms(t_state)} ms after spawn")
        print(f"first TCP connect ok  : {ms(t_tcp)} ms after spawn")
        print(f"first /json/list ok   : {ms(t_http)} ms after spawn ({attempts} attempt(s))")
        if t_http is not None and t_tcp is not None:
            print(f"HTTP - TCP            : {(t_http - t_tcp) * 1000:7.1f} ms  <-- the real wait")
        print(f"fcvm logged resume    : {resumed}")
        # A TCP connect that succeeds while HTTP never does is the signature of connecting
        # to a RECYCLED loopback IP still held by a previous clone's pasta. Worth naming,
        # because it looks identical to "the guest is broken".
        return 0 if t_http is not None else 2
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


if __name__ == "__main__":
    sys.exit(main())
