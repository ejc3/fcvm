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


def reap_rep(row: dict, named: dict, sp, data_dir) -> bool:
    """Reap this rep's on-disk state — but ONLY if every child is confirmed gone.

    Returns True if the rep LEAKED (a child outlived the kill), in which case
    nothing is reaped and the survivors are SIGKILLed.

    This is `reqbench.py`'s rule, which the probe imported `reap_disk` without:
    "reaping the state file and rmtree'ing the data dir of a VM whose Firecracker
    is still RUNNING deletes the only record of a live microVM and pulls its
    rootfs out from under it." `poll_gone` already returns None for a child still
    alive at the deadline; the probe printed that as TIMEOUT and then reaped
    anyway, never signalled the survivor, and still exited 0.
    """
    stuck = [n for n, v in (row.get("gone_ms") or {}).items() if v is None]
    if stuck:
        row["disk_reap_skipped"] = True
        row["survivors"] = stuck
        print(f"rep {row.get('rep')}: NOT reaping — still alive: {stuck}", file=sys.stderr)
        for n in stuck:
            pid = named.get(n)
            if pid:
                try:
                    os.kill(pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
        return True
    reaped = reap_disk(row, sp, data_dir)
    if reaped:
        row["disk_reaped"] = reaped
    for err in row.get("disk_errors", []):
        print(f"rep {row.get('rep')}: disk reap failed: {err}", file=sys.stderr)
    return False


def summarize(rows: list) -> int:
    """Per-child medians WITH their denominator. Returns the censored count.

    `n=` used to be `len(vs2)` — the observed exits only — so the denominator and
    the timeout count were never printed, and there was no `else` branch at all:
    a child that TIMED OUT in every rep printed NOTHING, i.e. the straggler this
    probe exists to find is precisely the one that disappeared from its own
    summary. Reps that took the no-state branch contribute an empty `gone_ms` and
    were likewise absent from the denominator.
    """
    import statistics

    agg: dict = {}
    for r in rows:
        for n, v in (r.get("gone_ms") or {}).items():
            agg.setdefault(n.split(":")[0], []).append(v)
    print("\n=== medians per child ===")
    censored_total = 0
    for n, vs in sorted(agg.items()):
        vs2 = [v for v in vs if v is not None]
        censored = len(vs) - len(vs2)
        censored_total += censored
        if vs2:
            print(f"  {n:22s} median {statistics.median(vs2):8.1f} ms   "
                  f"min {min(vs2):.1f}  max {max(vs2):.1f}  "
                  f"n={len(vs2)}/{len(vs)} censored={censored}")
        else:
            print(f"  {n:22s} NO EXIT OBSERVED  n=0/{len(vs)} censored={censored}")
    errored = sum(1 for r in rows if r.get("error"))
    leaked = sum(1 for r in rows if r.get("disk_reap_skipped"))
    print(f"  reps attempted={len(rows)} recorded={len(rows)} errored={errored} "
          f"reaps_skipped={leaked} censored_children={censored_total}")
    if censored_total:
        print("  ** CENSORED EXITS ARE LEAKS, NOT MISSING SAMPLES — do not quote "
              "these medians **")
    return censored_total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serve-pid", type=int, required=True)
    ap.add_argument("--n", type=int, default=5)
    ap.add_argument("--cdp-port", type=int, default=9222)
    ap.add_argument("--fcvm", default=os.path.join(HERE, "..", "..", "target", "release", "fcvm"))
    ap.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    ap.add_argument("--out", default="")
    ap.add_argument("--state-timeout", type=float, default=120.0,
                    help="how long to wait for the clone's state file to appear")
    a = ap.parse_args()
    a.fcvm = os.path.abspath(a.fcvm)
    state_dir = os.path.join(a.data_root, "state")

    rows: list = []
    leaked = False
    # EVERY rep records something, and the summary + artifact are written whatever
    # happens. `wait_port` raises TimeoutError when the port never answers and
    # `clone_cdp_endpoint` raises RuntimeError ("no usable host-side IP"); neither
    # was caught, so one bad rep left main() through a bare try/finally, the
    # summary never printed, `json.dump(rows, f)` never ran, and reps 0..i-1 were
    # lost with no trace. reqbench.py's main() deliberately does the opposite.
    try:
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
            named: dict = {}
            row = {"rep": i, "gone_ms": {}}
            # ONE reap path for every rep, including the failures. The `no state`
            # branch used to `continue` — never signalling `proc`, never waiting on
            # it, and dropping the reference on the next iteration. CPython's
            # Popen.__del__ only queues a later *reap*; it never kills. So a scan
            # that gave up while fcvm was mid-restore (the state file is written
            # with `pid: null` well before the post-resume pid write, so this is a
            # real window, not a hypothetical) left the fcvm, its firecracker,
            # pasta, the holder, the netns, the 127.x loopback IP, the vm-disks dir
            # AND a state file that fcvm's own sweeper will never remove —
            # `cleanup_stale_state` bails on a null pid.
            try:
                deadline = time.monotonic() + a.state_timeout
                sp, state = find_state(state_dir, proc.pid, deadline, watch, name)
                if state is None:
                    row["error"] = f"no state file within {a.state_timeout}s"
                    print(f"rep {i}: no state -> killing the clone and skipping",
                          file=sys.stderr)
                    continue
                data_dir = os.path.join(a.data_root, "vm-disks", state.get("vm_id", ""))
                wait_port(clone_cdp_endpoint(state, a.cdp_port), deadline)
                time.sleep(0.3)  # settle: measure steady-state teardown, not mid-restore

                for p in children_of(proc.pid):
                    named[f"{proc_comm(p)}:{p}"] = p
                fds = {n: pidfd_open(p) for n, p in named.items()}
                fds = {n: f for n, f in fds.items() if f is not None}
                t_kill = time.monotonic()
                os.kill(proc.pid, signal.SIGKILL)
                gone = poll_gone(fds, t_kill + 30)
                for f in fds.values():
                    os.close(f)
                row["gone_ms"] = gone
                print(f"rep {i}: " + "  ".join(
                    f"{n.split(':')[0]}={('%.1f' % v) if v is not None else 'TIMEOUT'}ms"
                    for n, v in sorted(gone.items(), key=lambda kv: (kv[1] is None, kv[1]))))
            except Exception as e:  # noqa: BLE001 - record, never lose the run
                row["error"] = f"{type(e).__name__}: {e}"
                print(f"rep {i}: {row['error']}", file=sys.stderr)
            finally:
                watch.close()
                # SIGKILL cannot be caught, so `cleanup_vm` never runs: WE own the
                # reap of BOTH artifacts. The state file alone is not enough —
                # nothing in fcvm ever removes `vm-disks/<vm_id>`, which holds a
                # reflink of the golden rootfs and therefore pins the golden
                # snapshot's extents on btrfs even after `snapshots delete`.
                if proc.poll() is None:
                    # We never reached the MEASURED kill (no state file, or an
                    # exception), so capture the children BEFORE signalling — after
                    # fcvm dies, /proc/<fcvm>/task/*/children is gone and any
                    # survivor is unrecoverable. Without this the failure paths
                    # reaped with no evidence at all.
                    if not named:
                        for p in children_of(proc.pid):
                            named[f"{proc_comm(p)}:{p}"] = p
                    late = {n: pidfd_open(p) for n, p in named.items()}
                    late = {n: f for n, f in late.items() if f is not None}
                    try:
                        os.kill(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        print(f"rep {i}: fcvm {proc.pid} survived SIGKILL", file=sys.stderr)
                    if late:
                        row["gone_ms"] = {**row["gone_ms"],
                                          **poll_gone(late, time.monotonic() + 30)}
                        for f in late.values():
                            os.close(f)
                if sp is None:
                    # The pid-keyed scan is exactly what failed; the NAME is written
                    # before the first save, so it is the only key that can recover
                    # vm_id (and thus the data dir) on this path.
                    sp, state = scan_state(state_dir, proc.pid, name)
                    if state is not None:
                        data_dir = os.path.join(a.data_root, "vm-disks",
                                                state.get("vm_id", ""))
                leaked |= reap_rep(row, named, sp, data_dir)
                rows.append(row)
    finally:
        censored = summarize(rows)
        if a.out:
            with open(a.out, "w") as f:
                json.dump(rows, f, indent=2)
    # 4 matches reqbench.py's exit code for the same condition: the box now
    # carries a microVM this harness cannot see.
    return 4 if (leaked or censored) else 0


if __name__ == "__main__":
    sys.exit(main())
