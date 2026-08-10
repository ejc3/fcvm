#!/usr/bin/env python3
"""Per-child teardown timing: WHICH process is the straggler after one SIGKILL?

The A/B measured `reap_wall` for the fast arm at ~410 ms while the summed
reclaim CPU across all children was ~30 ms. A 380 ms gap that burns no CPU is
not memory reclaim -- something is WAITING. Aggregate numbers cannot say what,
so this probe times each child's disappearance INDEPENDENTLY, with its own
pidfd, from the single kill instant.

Run: python3 teardown_probe.py --serve-pid N [--n 5]
"""

import argparse
import ctypes
import json
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from reqbench import (  # noqa: E402
    children_of, clone_cdp_endpoint, find_state, pidfd_open, proc_comm, wait_port,
)

libc = ctypes.CDLL("libc.so.6", use_errno=True)


def poll_gone(pid_fds, deadline):
    """Return {name: ms_until_gone}. Polls each pidfd separately so one slow
    child cannot hide behind another -- the exact failure of an all_gone flag."""
    import select

    t0 = time.monotonic()
    out = {}
    pending = dict(pid_fds)
    poller = select.poll()
    for name, fd in pending.items():
        poller.register(fd, select.POLLIN)
    while pending and time.monotonic() < deadline:
        for fd, _ev in poller.poll(20):
            for name, f in list(pending.items()):
                if f == fd:
                    out[name] = (time.monotonic() - t0) * 1000
                    poller.unregister(fd)
                    del pending[name]
    for name in pending:
        out[name] = None
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serve-pid", type=int, required=True)
    ap.add_argument("--n", type=int, default=5)
    ap.add_argument("--cdp-port", type=int, default=9223)
    ap.add_argument("--fcvm", default=os.path.join(HERE, "..", "..", "target", "release", "fcvm"))
    ap.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    a.fcvm = os.path.abspath(a.fcvm)
    state_dir = os.path.join(a.data_root, "state")

    rows = []
    for i in range(a.n):
        name = f"tdp-{os.getpid()}-{i}"
        log = f"/tmp/{name}.log"
        cmd = [a.fcvm, "snapshot", "run", "--pid", str(a.serve_pid), "--name", name,
               "--no-dirty-tracking", "--no-swap"]
        with open(log, "wb") as lf:
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL,
                                    env=dict(os.environ, RUST_LOG="fcvm=debug"))
        deadline = time.monotonic() + 120
        sp, state = find_state(state_dir, proc.pid, deadline)
        if state is None:
            print("no state", file=sys.stderr)
            continue
        wait_port(clone_cdp_endpoint(state, a.cdp_port), deadline)
        time.sleep(0.3)  # settle: measure steady-state teardown, not mid-restore

        kids = children_of(proc.pid)
        named = {}
        for p in kids:
            named[f"{proc_comm(p)}:{p}"] = p
        fds = {n: pidfd_open(p) for n, p in named.items()}
        fds = {n: f for n, f in fds.items() if f is not None}
        t_kill = time.monotonic()
        os.kill(proc.pid, signal.SIGKILL)
        gone = poll_gone(fds, t_kill + 30)
        for f in fds.values():
            os.close(f)
        row = {"rep": i, "gone_ms": gone}
        rows.append(row)
        print(f"rep {i}: " + "  ".join(
            f"{n.split(':')[0]}={('%.1f' % v) if v is not None else 'TIMEOUT'}ms"
            for n, v in sorted(gone.items(), key=lambda kv: (kv[1] is None, kv[1]))))
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        if sp and os.path.exists(sp):
            os.remove(sp)

    print("\n=== medians per child ===")
    agg = {}
    for r in rows:
        for n, v in r["gone_ms"].items():
            agg.setdefault(n.split(":")[0], []).append(v)
    import statistics
    for n, vs in sorted(agg.items()):
        vs2 = [v for v in vs if v is not None]
        if vs2:
            print(f"  {n:22s} median {statistics.median(vs2):8.1f} ms   "
                  f"min {min(vs2):.1f}  max {max(vs2):.1f}  n={len(vs2)}")
    if a.out:
        with open(a.out, "w") as f:
            json.dump(rows, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
