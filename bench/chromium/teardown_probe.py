#!/usr/bin/env python3
"""Per-child teardown timing: WHICH process is the straggler after one SIGKILL?

Aggregate numbers cannot say which child is slow, so this probe times each
child's disappearance INDEPENDENTLY, with its own pidfd, from the single kill
instant.

WHAT THIS PROBE HAS AND HAS NOT ESTABLISHED. The run that motivated it reported
`reap_wall ~410 ms` against a summed reclaim CPU of ~30 ms and concluded "a gap
that burns no CPU is not memory reclaim -- something is WAITING", then attributed
the wait to pasta at 704 ms. Both figures are WITHDRAWN (see REVIEW.md): the CPU
side came from a sampler whose control window was a busy-spin and whose per-child
values were quantized to a 10 ms jiffy, and the pasta side was measured on a tree
that did not yet arm `PR_SET_PDEATHSIG` on pasta (`46dbb789`). The per-child WALL
times this file produces are unaffected by the CPU defects -- they come from
`poll_gone`, which is pure pidfd timing -- but they must be RE-MEASURED on the
current tree before any of them is quoted.

Run: python3 teardown_probe.py --serve-pid N [--n 5] [--state-timeout 120]
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
    DirWatch, children_of, clone_cdp_endpoint, find_state, pidfd_open, proc_comm,
    reap_disk, scan_state, wait_port,
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
    ap.add_argument("--state-timeout", type=float, default=120.0,
                    help="how long to wait for the clone's state file to appear")
    a = ap.parse_args()
    a.fcvm = os.path.abspath(a.fcvm)
    state_dir = os.path.join(a.data_root, "state")

    rows = []
    for i in range(a.n):
        name = f"tdp-{os.getpid()}-{i}"
        log = f"/tmp/{name}.log"
        cmd = [a.fcvm, "snapshot", "run", "--pid", str(a.serve_pid), "--name", name,
               "--no-dirty-tracking", "--no-swap"]
        watch = DirWatch(state_dir)  # registered BEFORE the spawn
        with open(log, "wb") as lf:
            proc = subprocess.Popen(cmd, stdout=lf, stderr=lf, stdin=subprocess.DEVNULL,
                                    env=dict(os.environ, RUST_LOG="fcvm=debug"))
        sp = None
        data_dir = None
        row = {"rep": i, "gone_ms": {}}
        # ONE reap path for every rep, including the failures. The `no state`
        # branch used to `continue` — never signalling `proc`, never waiting on
        # it, and dropping the reference on the next iteration. CPython's
        # Popen.__del__ only queues a later *reap*; it never kills. So a scan that
        # gave up while fcvm was mid-restore (the state file is written with
        # `pid: null` well before the post-resume pid write, so this is a real
        # window, not a hypothetical) left the fcvm, its firecracker, pasta, the
        # holder, the netns, the 127.x loopback IP, the vm-disks dir AND a state
        # file that fcvm's own sweeper will never remove — `cleanup_stale_state`
        # bails on a null pid. Nothing was recorded either: no row, no error field.
        try:
            deadline = time.monotonic() + a.state_timeout
            sp, state = find_state(state_dir, proc.pid, deadline, watch, name)
            if state is None:
                row["error"] = f"no state file within {a.state_timeout}s"
                rows.append(row)
                print(f"rep {i}: no state -> killing the clone and skipping", file=sys.stderr)
                continue
            data_dir = os.path.join(a.data_root, "vm-disks", state.get("vm_id", ""))
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
            row["gone_ms"] = gone
            rows.append(row)
            print(f"rep {i}: " + "  ".join(
                f"{n.split(':')[0]}={('%.1f' % v) if v is not None else 'TIMEOUT'}ms"
                for n, v in sorted(gone.items(), key=lambda kv: (kv[1] is None, kv[1]))))
        finally:
            watch.close()
            # SIGKILL cannot be caught, so `cleanup_vm` never runs: WE own the reap
            # of BOTH artifacts. The state file alone is not enough — nothing in
            # fcvm ever removes `vm-disks/<vm_id>`, which holds a reflink of the
            # golden rootfs and therefore pins the golden snapshot's extents on
            # btrfs even after `snapshots delete`.
            if proc.poll() is None:
                try:
                    os.kill(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    print(f"rep {i}: fcvm {proc.pid} survived SIGKILL", file=sys.stderr)
            if sp is None:
                # The pid-keyed scan is exactly what failed; the NAME is written
                # before the first save, so it is the only key that can recover
                # vm_id (and thus the data dir) on this path.
                sp, state = scan_state(state_dir, proc.pid, name)
                if state is not None:
                    data_dir = os.path.join(a.data_root, "vm-disks", state.get("vm_id", ""))
            reaped = reap_disk(row, sp, data_dir)
            if reaped:
                row["disk_reaped"] = reaped
            for err in row.get("disk_errors", []):
                print(f"rep {i}: disk reap failed: {err}", file=sys.stderr)

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
