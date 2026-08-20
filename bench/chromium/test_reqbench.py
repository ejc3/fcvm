#!/usr/bin/env python3
"""Regression tests for the request-bench harness. VM-free, deterministic, ~2 s.

    python3 -m unittest discover -s bench/chromium -p 'test_*.py' -v

Every test here was watched FAIL against the code as it stood before the matching
fix; the failure each one produced is quoted in its docstring. A test never seen
red is not evidence, so the ones that could not be made to fail (they need a real
microVM) are not in this file — they are in tests/test_signal_cleanup.rs.

There is no pytest in this repo, so this is stdlib `unittest` only.
"""

import argparse
import ctypes
import fcntl
import hashlib
import io
import json
import os
import random
import re
import signal
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
import urllib.request
from contextlib import contextmanager, redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqanalyze  # noqa: E402
import reqbench  # noqa: E402


def self_cpu_ms() -> float:
    s = reqbench.proc_stat_fields(os.getpid())
    return (s[1] + s[2]) * 1000.0 / reqbench.CLK_TCK


def spawn_pdeathsig_parent(child_argv):
    """A parent whose child dies with it — fcvm's shape, without a VM.

    `PR_SET_PDEATHSIG` is 1. The child inherits nothing else special, so killing
    the parent exercises exactly the kernel-enforced reap the fast arm relies on.
    """
    code = (
        "import ctypes,signal,subprocess,sys;"
        "libc=ctypes.CDLL('libc.so.6');"
        "p=subprocess.Popen(sys.argv[1:],preexec_fn=lambda: libc.prctl(1, signal.SIGKILL));"
        "p.wait()"
    )
    return subprocess.Popen([sys.executable, "-c", code, *child_argv])


def spawn_mixed_parent(linger_bin, fast_bin):
    """Fork order [linger (no pdeathsig), fast (pdeathsig)].

    The linger child outlives the parent's SIGKILL; the fast one dies with it in
    under a millisecond. That is the fcvm shape the ordering defect needs — a
    child that is ALIVE at kill time and gone almost immediately after — which a
    child that simply exits early does not reproduce.
    """
    code = (
        "import ctypes,signal,subprocess,sys,time;"
        "libc=ctypes.CDLL('libc.so.6');"
        "subprocess.Popen([sys.argv[1],'300']);"
        "subprocess.Popen([sys.argv[2],'300'],"
        " preexec_fn=lambda: libc.prctl(1, signal.SIGKILL));"
        "time.sleep(3600)"
    )
    return subprocess.Popen([sys.executable, "-c", code, linger_bin, fast_bin])


def wait_for_execed_children(pid, want, timeout):
    """Wait until `pid` has `want` children that have finished exec'ing.

    Waiting for the fork alone is not enough. `fork()` copies the parent's
    `comm`, and `execve` is what replaces it, so between the two a child of a
    python parent reads back as "python3". A loop that stops at
    `len(children_of(pid)) >= want` therefore races the exec, and any assertion
    on the names is a coin flip weighted by how loaded the box is. On this
    64-core host the exec effectively always won; on a shared GitHub-hosted
    runner it did not, and the precondition failed as

        AssertionError: Lists differ: ['lingersleep', 'python3'] != ['lingersleep', 'fastexit']

    Returns (pids, comms) whatever happens — the caller still asserts, so a
    child that never execs fails the test rather than hanging it.
    """
    parent_comm = reqbench.proc_comm(pid)
    deadline = time.monotonic() + timeout
    kids, comms = [], []
    while True:
        kids = reqbench.children_of(pid)
        comms = [reqbench.proc_comm(k) for k in kids]
        settled = len(kids) >= want and all(
            c is not None and c != parent_comm for c in comms
        )
        if settled or time.monotonic() >= deadline:
            return kids, comms
        time.sleep(0.005)


def spawn_slow_exec_parent(child_bin, preexec_delay_s):
    """A parent whose child sits in `preexec_fn` for a known interval.

    `preexec_fn` runs in the child AFTER fork and BEFORE exec, which makes the
    otherwise sub-millisecond pre-exec window wide enough to observe on purpose.
    That is what lets the race above be reproduced deterministically instead of
    waited for.
    """
    code = (
        "import subprocess,sys,time;"
        "subprocess.Popen([sys.argv[1],'300'],"
        " preexec_fn=lambda: time.sleep(float(sys.argv[2])));"
        "time.sleep(3600)"
    )
    return subprocess.Popen(
        [sys.executable, "-c", code, child_bin, str(preexec_delay_s)]
    )


def spawn_pdeathsig_parent_ignoring_sigterm(child_argv):
    """A parent that IGNORES SIGTERM but whose child dies with it.

    `teardown_normal` sends SIGTERM first, so this shape forces the timed_out
    path — and the child still dies sub-millisecond once the SIGKILL lands. That
    combination is what separates "the teardown timed out" from "the teardown
    leaked": exactly the distinction the all_gone verdict has to get right.
    """
    code = (
        "import ctypes,signal,subprocess,sys,time;"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
        "libc=ctypes.CDLL('libc.so.6');"
        "subprocess.Popen(sys.argv[1:],preexec_fn=lambda: libc.prctl(1, signal.SIGKILL));"
        "time.sleep(3600)"
    )
    return subprocess.Popen([sys.executable, "-c", code, *child_argv])


def wait_for_child(pid, timeout=5.0):
    """Block until `pid` has forked at least one child. Returns the child list."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        kids = reqbench.children_of(pid)
        if kids:
            return kids
        time.sleep(0.005)
    raise AssertionError(f"pid {pid} never forked a child")


def kill_tree(p):
    """SIGKILL a parent and anything still under it. Tests must not leak either."""
    for pid in reqbench.children_of(p.pid):
        try:
            os.kill(pid, 9)
        except (ProcessLookupError, PermissionError):
            pass
    try:
        os.kill(p.pid, 9)
    except ProcessLookupError:
        pass
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


PR_SET_CHILD_SUBREAPER = 36
PR_GET_CHILD_SUBREAPER = 37


@contextmanager
def child_subreaper():
    """Adopt orphaned grandchildren for the duration, so this process reaps them.

    Whether a killed orphan's `/proc/<pid>` entry disappears is a property of
    whoever inherits it, not of the kill. Under a PID 1 that reaps (systemd on a
    normal host) it vanishes; under a PID 1 that does not (a container, or
    `unshare --pid --fork`) the corpse stays in state `Z` forever and a test
    that waits for the entry to go away waits out its whole deadline and then
    fails, while the process it was asking about has in fact been dead the whole
    time. Becoming the subreaper makes the orphan OURS, so the test can reap it
    itself and read how it died instead of inferring death from an absence.
    """
    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    previous = ctypes.c_int(0)
    restore = libc.prctl(PR_GET_CHILD_SUBREAPER, ctypes.byref(previous),
                         0, 0, 0) == 0
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
    try:
        yield
    finally:
        libc.prctl(PR_SET_CHILD_SUBREAPER,
                   previous.value if restore else 0, 0, 0, 0)


def reap_orphan(pid, note, timeout=5.0):
    """Reap an adopted orphan and return its raw wait status.

    Returns None when the pid is not ours to wait for, which is the case on a
    host whose PID 1 got there first; the caller then has to settle for the
    weaker evidence. Raises `note` if it is ours and still running at the
    deadline, because that is the defect the caller is testing for.
    """
    deadline = time.monotonic() + timeout
    while True:
        try:
            reaped, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return None
        if reaped == pid:
            return status
        if time.monotonic() >= deadline:
            raise AssertionError(
                f"{note}: pid {pid} is still running {timeout:.0f}s later, "
                f"state {reqbench.proc_stat_fields(pid)}"
            )
        time.sleep(0.01)


@contextmanager
def pending_harness_signal(signum):
    """Leave the harness in the state a real INT/TERM leaves it in, then clear.

    `record_harness_interrupt` is the harness's own handler, so this is the same
    module state a delivered signal produces; `signum=0` arms nothing and only
    guarantees the flag is cleared afterwards for whoever injects it later. The
    flag is global, so an uncleared one would make every later test in the
    process behave as though the run were shutting down.
    """
    reqbench._pending_harness_signal = 0
    if signum:
        reqbench.record_harness_interrupt(signum, None)
    try:
        yield
    finally:
        reqbench._pending_harness_signal = 0


def assert_sigkilled(test, pid, status, note):
    """Assert `pid` was KILLED, from its exit status where we have one.

    The status is the direct evidence and says which signal did it. Only when
    the orphan was never ours does this fall back to the procfs entry being
    gone, which is a claim about the reaper as much as about the kill.
    """
    if status is None:
        test.assertIsNone(reqbench.proc_stat_fields(pid), note)
        return
    test.assertTrue(
        os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL,
        f"{note}: it ended on its own with wait status {status:#x}",
    )


def write_graceful_clone_stub(path, state_path, data_dir, name, term_path):
    """Write a VM-free fcvm shape that performs exact cleanup on SIGTERM."""
    quoted_state = shlex.quote(state_path)
    quoted_data = shlex.quote(data_dir)
    quoted_term = shlex.quote(term_path)
    script = f"""#!/bin/bash
python3 -c 'import ctypes,signal,time; ctypes.CDLL("libc.so.6").prctl(1, signal.SIGKILL); time.sleep(300)' &
child=$!
cleanup() {{
    kill -TERM "$child" 2>/dev/null
    wait "$child" 2>/dev/null
    rm -f {quoted_state} {quoted_state}.lock
    rmdir {quoted_data}
    printf 'SIGTERM\\n' > {quoted_term}
    exit 0
}}
trap cleanup TERM INT
mkdir -p {quoted_data}
read -r proc_stat < /proc/$$/stat
proc_stat=${{proc_stat##*) }}
read -ra proc_fields <<< "$proc_stat"
start=${{proc_fields[19]}}
cat > {quoted_state}.tmp <<EOF
{{"vm_id":"{os.path.basename(state_path)[:-5]}","name":"{name}","pid":$$,"pid_start_time":$start,"lifecycle_ready":true,"config":{{"network":{{"loopback_ip":"127.0.0.1"}}}}}}
EOF
mv {quoted_state}.tmp {quoted_state}
: > {quoted_state}.lock
wait "$child"
"""
    with open(path, "w") as output:
        output.write(script)
    os.chmod(path, 0o755)


class CloneSpawnSignalMask(unittest.TestCase):
    """Every clone must be able to receive the graceful teardown signal.

    RED BEFORE THE FIX: all three request arms blocked SIGINT and SIGTERM before
    Popen.  The mask survives exec, so normal teardown always spent its whole
    timeout and killed fcvm without letting fcvm remove state or disk artifacts.
    """

    def test_shared_spawn_unblocks_child_and_restores_calling_thread_mask(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "mask.log")
            code = (
                "import json,signal,time;"
                "print(json.dumps(sorted(int(s) for s in "
                "signal.pthread_sigmask(signal.SIG_BLOCK,set()))),flush=True);"
                "time.sleep(300)"
            )
            old_mask = signal.pthread_sigmask(
                signal.SIG_BLOCK, reqbench.TERMINATION_SIGNALS
            )
            proc = None
            try:
                proc = reqbench.spawn_clone_process(
                    [sys.executable, "-c", code], log, dict(os.environ)
                )
                current = signal.pthread_sigmask(signal.SIG_BLOCK, set())
                self.assertTrue(reqbench.TERMINATION_SIGNALS.issubset(current))
                deadline = time.monotonic() + 2
                blocked = None
                while time.monotonic() < deadline:
                    try:
                        with open(log) as source:
                            line = source.readline()
                        if line:
                            blocked = set(json.loads(line))
                            break
                    except FileNotFoundError:
                        pass
                    time.sleep(0.005)
                self.assertIsNotNone(blocked, "spawned child never reported its mask")
                self.assertTrue(
                    blocked.isdisjoint(int(item) for item in reqbench.TERMINATION_SIGNALS),
                    blocked,
                )
                proc.terminate()
                self.assertEqual(proc.wait(timeout=2), -signal.SIGTERM)
            finally:
                signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
                if proc is not None and proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=2)

    def test_all_request_arms_use_the_shared_spawn_boundary(self):
        for request in (
            reqbench.run_cdp_request,
            reqbench.run_noop_request,
            reqbench.run_exec_request,
        ):
            with self.subTest(request=request.__name__):
                self.assertIn("spawn_clone_process", request.__code__.co_names)
                self.assertNotIn("Popen", request.__code__.co_names)

    def test_normal_request_receives_sigterm_and_cleans_exact_artifacts(self):
        import socket

        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            vm_id = "vm-22222222222222222222222222222222"
            state_path = os.path.join(state_dir, f"{vm_id}.json")
            data_dir = os.path.join(d, "vm-disks", vm_id)
            term_path = os.path.join(d, "term-received")
            stub = os.path.join(d, "fcvm-stub")
            write_graceful_clone_stub(
                stub,
                state_path,
                data_dir,
                "rb-test-run-0-noop",
                term_path,
            )

            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            listener.listen(8)
            args = argparse.Namespace(
                fcvm=stub,
                out_dir=d,
                snapshot_tag="",
                serve_pid=1,
                rust_log="off",
                timeout=5.0,
                teardown_timeout=2.0,
                cdp_port=listener.getsockname()[1],
                state_dir=state_dir,
                data_root=d,
                run_id="test-run",
            )
            saved_pending = reqbench._pending_harness_signal
            reqbench._pending_harness_signal = 0
            try:
                record = reqbench.run_noop_request(args, 0)
            finally:
                reqbench._pending_harness_signal = saved_pending
                listener.close()

            self.assertTrue(record["ok"], record)
            self.assertNotIn("timed_out", record["teardown"])
            self.assertTrue(record["teardown"]["all_gone"])
            self.assertTrue(record["teardown"]["disk_cleanup_verified"])
            self.assertTrue(os.path.exists(term_path))
            self.assertFalse(os.path.lexists(state_path))
            self.assertFalse(os.path.lexists(state_path + ".lock"))
            self.assertFalse(os.path.lexists(data_dir))


class TeardownFastReapGuard(unittest.TestCase):
    """teardown_fast must NOT delete the tracking record of a VM it failed to kill.

    RED BEFORE THE FIX: `all_gone` was computed and stored, then the state file was
    removed and the data dir rmtree'd unconditionally, so a parent whose child has
    no pdeathsig produced
        all_gone: False, disk_reaped: [state.json, data/], state exists: False
    with the survivor still in /proc afterwards.
    """

    def test_failed_parent_pin_never_signals_the_numeric_pid(self):
        """A failed pidfd pin leaves no process identity safe to signal."""
        with tempfile.TemporaryDirectory() as d:
            vm_id = "vm-11111111111111111111111111111111"
            state = os.path.join(d, f"{vm_id}.json")
            data = os.path.join(d, "vm-disks", vm_id)
            with open(state, "w") as f:
                json.dump({"vm_id": vm_id}, f)
            os.makedirs(data)

            real_pidfd_open = reqbench.pidfd_open
            real_kill = reqbench.os.kill
            signalled = []

            def record_kill(pid, sig):
                signalled.append((pid, sig))
                raise AssertionError("an unpinned numeric PID was signalled")

            reqbench.pidfd_open = lambda _pid: None
            reqbench.os.kill = record_kill
            try:
                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(424242, d, state, data, 0.01)
            finally:
                reqbench.pidfd_open = real_pidfd_open
                reqbench.os.kill = real_kill

            self.assertEqual(signalled, [])
            self.assertIs(cm.exception.teardown["all_gone"], False)
            self.assertTrue(os.path.exists(state))
            self.assertTrue(os.path.isdir(data))

    def test_survivor_blocks_the_reap_and_raises(self):
        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-11111111111111111111111111111111.json")
            # Shaped exactly as production builds it — `<data_root>/vm-disks/<vm_id>`
            # — because `reap_disk` now refuses anything that is not strictly below
            # a `vm-disks` root (see ReapDiskPathGuard).
            data = os.path.join(d, "vm-disks", "vm-11111111111111111111111111111111")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-11111111111111111111111111111111", "pid": 1}, f)
            os.makedirs(os.path.join(data, "disks"))

            # A parent whose child does NOT inherit a pdeathsig: killing the parent
            # leaves the child running. This is the shape of an unarmed hop.
            p = subprocess.Popen(["bash", "-c", "sleep 300 & wait"])
            try:
                # Give bash a moment to actually fork the child, then confirm.
                deadline = time.monotonic() + 5
                while not reqbench.children_of(p.pid) and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(reqbench.children_of(p.pid), "bash never forked its child")

                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(p.pid, d, state, data, 0.3)

                self.assertIn("NOT reaped", str(cm.exception))
                self.assertTrue(
                    os.path.exists(state),
                    "state file of a VM that survived the kill must NOT be reaped",
                )
                self.assertTrue(
                    os.path.isdir(data),
                    "data dir of a VM that survived the kill must NOT be rmtree'd",
                )
            finally:
                for pid in reqbench.children_of(p.pid):
                    try:
                        os.kill(pid, 9)
                    except ProcessLookupError:
                        pass
                try:
                    os.kill(p.pid, 9)
                except ProcessLookupError:
                    pass
                p.wait(timeout=5)

    def test_clean_teardown_still_reaps_both_artifacts(self):
        """The guard must not disarm the reap on the normal path."""
        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-11111111111111111111111111111111.json")
            data = os.path.join(d, "vm-disks", "vm-11111111111111111111111111111111")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-11111111111111111111111111111111"}, f)
            with open(state + ".lock", "w") as f:
                f.write("")
            with open(state + ".lock", "w") as f:
                f.write("")
            os.makedirs(data)
            p = spawn_pdeathsig_parent(["sleep", "300"])
            wait_for_child(p.pid)
            out = reqbench.teardown_fast(p.pid, d, state, data, 5.0)
            p.wait(timeout=5)
            self.assertTrue(out["all_gone"])
            self.assertFalse(os.path.exists(state))
            self.assertFalse(os.path.exists(state + ".lock"), "the .json.lock must go too")
            self.assertFalse(os.path.isdir(data))


    def test_failed_rmtree_aborts_the_run(self):
        """A reap that could not remove the data dir must STOP the schedule.

        RED BEFORE THE FIX: `reap_disk` recorded the EPERM in `out["disk_errors"]`
        and `teardown_fast` returned normally, so main() wrote `ok: true` and ran
        the next rep against a box carrying an un-reapable multi-GB reflink of the
        golden rootfs. Observed:
            teardown_fast returned NORMALLY:
              disk_errors=['/tmp/.../data: [Errno 1] EPERM']
              disk_reaped=['/tmp/.../vm-test.json']  data dir still on disk=True
        `git grep disk_errors` found no reader anywhere in the harness.
        """
        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-11111111111111111111111111111111.json")
            data = os.path.join(d, "vm-disks", "vm-11111111111111111111111111111111")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-11111111111111111111111111111111"}, f)
            with open(state + ".lock", "w") as f:
                f.write("")
            os.makedirs(data)
            real = shutil.rmtree

            def boom(*_a, **_k):
                raise PermissionError(1, "Operation not permitted")

            p = spawn_pdeathsig_parent(["sleep", "300"])
            wait_for_child(p.pid)
            shutil.rmtree = boom
            try:
                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(p.pid, d, state, data, 5.0)
            finally:
                shutil.rmtree = real
                p.wait(timeout=5)
            self.assertIn("could not reap", str(cm.exception))
            self.assertTrue(os.path.isdir(data), "the un-reaped dir is still there")
            self.assertTrue(os.path.exists(state), "failed disk reap must retain its state")
            self.assertTrue(os.path.exists(state + ".lock"), "failed disk reap must retain its lock")
            self.assertTrue(cm.exception.teardown.get("disk_errors"))
            self.assertIn(data, cm.exception.teardown.get("disk_reap_failed", []))


class ReapDiskPathGuard(unittest.TestCase):
    """`reap_disk` must refuse a data_dir that is not strictly below vm-disks.

    RED BEFORE THE FIX: every call site computes
    `os.path.join(data_root, "vm-disks", state.get("vm_id", ""))`, and an empty
    vm_id makes that `<data_root>/vm-disks/` — a directory `os.path.isdir` accepts,
    which `shutil.rmtree` then empties. That is EVERY VM's disks on the box. Commit
    e1286e3f called this arithmetic catastrophic and guarded only the Rust fixture
    (`an_empty_vm_id_resolves_to_the_shared_disk_root_and_must_be_rejected`); the
    Python harness that does the identical computation, runs under sudo, and
    rmtree's the result was left unguarded.
    """

    def test_an_empty_vm_id_does_not_wipe_the_shared_disk_root(self):
        with tempfile.TemporaryDirectory() as root:
            disks = os.path.join(root, "vm-disks")
            vm_a = "vm-44444444444444444444444444444444"
            vm_b = "vm-55555555555555555555555555555555"
            os.makedirs(os.path.join(disks, vm_a))
            os.makedirs(os.path.join(disks, vm_b))
            out: dict = {}
            reaped = reqbench.reap_disk(out, root, "", os.path.join(disks, ""))
            self.assertTrue(os.path.isdir(os.path.join(disks, vm_a)),
                            "another VM's disks were deleted")
            self.assertTrue(os.path.isdir(os.path.join(disks, vm_b)),
                            "another VM's disks were deleted")
            self.assertEqual(reaped, [])
            self.assertTrue(any("vm-disks" in e for e in out.get("disk_errors", [])),
                            f"the refusal must be recorded: {out}")

    def test_a_real_per_vm_dir_is_still_reaped(self):
        """The guard must not disarm the ordinary reap."""
        with tempfile.TemporaryDirectory() as root:
            vm_id = "vm-44444444444444444444444444444444"
            state = os.path.join(root, f"{vm_id}.json")
            with open(state, "w") as f:
                json.dump({"vm_id": vm_id}, f)
            dd = os.path.join(root, "vm-disks", vm_id, "disks")
            os.makedirs(dd)
            out: dict = {}
            reaped = reqbench.reap_disk(
                out, root, state, os.path.join(root, "vm-disks", vm_id)
            )
            self.assertEqual(
                reaped,
                [os.path.join(root, "vm-disks", vm_id), state],
            )
            self.assertFalse(os.path.isdir(os.path.join(root, "vm-disks", vm_id)))
            self.assertNotIn("disk_errors", out)


class TeardownNormalLeakVerdict(unittest.TestCase):
    """`teardown_normal` computes `all_gone` and must ACT on it — correctly.

    Two failures in opposite directions, both live on the branch:
      * a real survivor is recorded and then ignored, so `cdp`/`noop` keep running
        the schedule next to a live Firecracker;
      * on the timed_out path the verdict is decided with a ZERO observation
        budget (`max(0.0, t0 + timeout_s - now)` is always 0 by then, and
        `wait_pidfds` returns False without polling at all), so a perfectly clean
        teardown is reported as a leak.
    """

    def test_a_survivor_stops_the_schedule(self):
        """RED BEFORE THE FIX: returned {'timed_out': True, 'all_gone': False} and
        the caller ran the next rep with the survivor still in /proc."""
        p = subprocess.Popen(["bash", "-c", "trap '' TERM; sleep 300 & wait"])
        kids = wait_for_child(p.pid)
        try:
            with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                reqbench.teardown_normal(p, p.pid, 0.4)
            self.assertTrue(cm.exception.teardown.get("survivors"))
            # ...and the survivor was SIGKILLed rather than left on the box.
            def alive(pid):
                state = reqbench.proc_stat_fields(pid)
                return state is not None and state[0] not in ("Z", "X", "x")

            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if not any(alive(k) for k in kids):
                    break
                time.sleep(0.01)
            self.assertEqual(
                [k for k in kids if alive(k)],
                [],
                "the survivor must be killed before we abort",
            )
        finally:
            kill_tree(p)

    def test_a_clean_timed_out_teardown_is_not_reported_as_a_leak(self):
        """RED BEFORE THE FIX: all_gone=False although the pdeathsig child died
        sub-millisecond — `wait_pidfds` was handed a 0.0 budget and returns False
        without ever polling. Gating on that boolean would abort healthy runs."""
        p = spawn_pdeathsig_parent_ignoring_sigterm(["sleep", "300"])
        try:
            wait_for_child(p.pid)
            out = reqbench.teardown_normal(p, p.pid, 0.4)
        finally:
            kill_tree(p)
        self.assertIs(out.get("timed_out"), True, "SIGTERM was ignored; this must time out")
        self.assertTrue(out["all_gone"],
                        f"pdeathsig child died with its parent, but all_gone={out['all_gone']}")

    def test_on_disk_leftovers_are_reaped_but_still_abort_the_run(self):
        """A clean process tree does not excuse leaked clone state.

        This models a noop clone killed after the first null-PID state save but
        before fcvm's post-resume owner save. RED BEFORE THE FIX: normal teardown
        returned ``all_gone: true`` and left both paths forever; the sweeper
        refuses null-PID states.
        """
        with tempfile.TemporaryDirectory() as d:
            vm_id = "vm-11111111111111111111111111111111"
            state = os.path.join(d, "state", f"{vm_id}.json")
            data = os.path.join(d, "vm-disks", vm_id)
            os.makedirs(os.path.dirname(state))
            os.makedirs(data)
            with open(state, "w") as f:
                json.dump({"vm_id": vm_id, "pid": None}, f)
            with open(state + ".lock", "w") as f:
                f.write("")
            p = spawn_pdeathsig_parent(["sleep", "300"])
            wait_for_child(p.pid)
            with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                reqbench.teardown_normal(
                    p,
                    p.pid,
                    2.0,
                    d,
                    state,
                    data,
                    verify_disk_cleanup=True,
                )
            self.assertFalse(cm.exception.teardown["disk_cleanup_verified"])
            self.assertFalse(os.path.exists(state))
            self.assertFalse(os.path.exists(state + ".lock"))
            self.assertFalse(os.path.exists(data))
            self.assertIn("left on-disk state", str(cm.exception))


class TeardownAttributionFailure(unittest.TestCase):
    def _artifacts(self, root):
        vm_id = "vm-11111111111111111111111111111111"
        state = os.path.join(root, f"{vm_id}.json")
        data = os.path.join(root, "vm-disks", vm_id)
        with open(state, "w") as f:
            json.dump({"vm_id": vm_id}, f)
        os.makedirs(data)
        return state, data

    def _invoke(self, mode, proc, root, state, data):
        if mode == "fast":
            return reqbench.teardown_fast(proc.pid, root, state, data, 1.0)
        return reqbench.teardown_normal(
            proc, proc.pid, 1.0, root, state, data
        )

    def test_capture_error_kills_exact_owner_and_retains_disk(self):
        real_capture = reqbench.freeze_and_capture_children
        try:
            for mode in ("fast", "normal"):
                with self.subTest(mode=mode), tempfile.TemporaryDirectory() as d:
                    state, data = self._artifacts(d)
                    proc = spawn_pdeathsig_parent(["sleep", "300"])
                    wait_for_child(proc.pid)

                    def fail_capture(_pid):
                        raise RuntimeError("injected procfs attribution failure")

                    reqbench.freeze_and_capture_children = fail_capture
                    try:
                        with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                            self._invoke(mode, proc, d, state, data)
                        proc.wait(timeout=5)
                    finally:
                        kill_tree(proc)
                    self.assertIs(
                        cm.exception.teardown["child_attribution_established"], False
                    )
                    self.assertTrue(os.path.exists(state))
                    self.assertTrue(os.path.isdir(data))
        finally:
            reqbench.freeze_and_capture_children = real_capture

    def test_empty_child_set_is_never_a_vacuous_success(self):
        for mode in ("fast", "normal"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as d:
                state, data = self._artifacts(d)
                proc = subprocess.Popen(["sleep", "300"])
                try:
                    with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                        self._invoke(mode, proc, d, state, data)
                    proc.wait(timeout=5)
                finally:
                    kill_tree(proc)
                self.assertIn("no completely pinned child set", str(cm.exception))
                self.assertIs(
                    cm.exception.teardown["child_attribution_established"], False
                )
                self.assertTrue(os.path.exists(state))
                self.assertTrue(os.path.isdir(data))


class CdpFailureIsLabelledOnTheRecord(unittest.TestCase):
    """A cdpdrive `ok: false` that does not RAISE must still label the record.

    RED BEFORE THE FIX: `run_cdp_request` set `rec["ok"] = bool(result["ok"])` and
    nothing else, so the error/stage/failure_class stayed buried under
    `rec["render"]`. `reqanalyze`'s only failure breakdown reads the TOP level
    (`r.get("error", f"rc={r.get('rc')}")`), so a WsClosed transport drop printed
    as `FAILURE x1: rc=None` — the exact separation REVIEW.md's availability gate
    depends on, erased.
    """

    def test_a_non_raising_failure_is_stamped_at_the_top_level(self):
        import cdpdrive

        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            stub = os.path.join(d, "fcvm-stub")
            child_ready = os.path.join(d, "child-ready")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"python3 -c 'import ctypes,signal,time; ctypes.CDLL(\"libc.so.6\").prctl(1, signal.SIGKILL); open(\"{child_ready}\", \"w\").close(); time.sleep(600)' &\n"
                    f"while [ ! -e {child_ready} ]; do sleep 0.01; done\n"
                    "read -r proc_stat < /proc/$$/stat; proc_stat=${proc_stat##*) }; "
                    "read -ra proc_fields <<< \"$proc_stat\"; start=${proc_fields[19]}\n"
                    f"cat > {state_dir}/vm-22222222222222222222222222222222.json <<EOF\n"
                    '{"vm_id": "vm-22222222222222222222222222222222", "name": "rb-test-run-0-fast", "pid": $$, '
                    '"pid_start_time": $start, "lifecycle_ready": true, "config": {"network": {"loopback_ip": "127.0.0.1"}}}\n'
                    "EOF\n"
                    f": > {state_dir}/vm-22222222222222222222222222222222.json.lock\n"
                    "wait\n"
                )
            os.chmod(stub, 0o755)
            # A listener so wait_port succeeds without a VM.
            import socket
            srv = socket.socket()
            srv.bind(("127.0.0.1", 0))
            srv.listen(8)
            port = srv.getsockname()[1]

            real_drive = cdpdrive.drive
            cdpdrive.drive = lambda _a: {
                "ok": False, "error": "WsClosed: connection closed mid-frame",
                "failure_class": "transport", "stage": "navigate", "stages": {},
            }
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=10.0, teardown_timeout=5.0, cdp_port=port,
                state_dir=state_dir, data_root=d, ws_url="", run_id="test-run",
            )
            try:
                rec = reqbench.run_cdp_request(args, 0, fast=True)
            finally:
                cdpdrive.drive = real_drive
                srv.close()
            self.assertIs(rec["ok"], False)
            self.assertIn("WsClosed", rec.get("error", ""))
            self.assertEqual(rec.get("failure_class"), "transport")
            self.assertEqual(rec.get("failure_stage"), "navigate")

    def test_cdp_response_waits_for_lifecycle_ready_before_fast_teardown(self):
        """A serving port is not yet permission to tear down clone setup."""
        import cdpdrive
        import socket

        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            vm_id = "vm-22222222222222222222222222222222"
            data_dir = os.path.join(d, "vm-disks", vm_id)
            state_path = os.path.join(state_dir, f"{vm_id}.json")
            os.makedirs(state_dir)
            os.makedirs(data_dir)
            child_ready = os.path.join(d, "child-ready")
            owner_wait_entered = os.path.join(d, "owner-wait-entered")
            owner_wait_release = os.path.join(d, "owner-wait-release")
            stub = os.path.join(d, "fcvm-stub")

            server = socket.socket()
            server.bind(("127.0.0.1", 0))
            server.listen(8)
            port = server.getsockname()[1]
            name = "rb-test-run-0-fast"
            initial = json.dumps({
                "vm_id": vm_id,
                "name": name,
                "pid": "PID",
                "pid_start_time": "START",
                "lifecycle_ready": False,
                "config": {"network": {"loopback_ip": "127.0.0.1"}},
            })
            ready = json.dumps({
                "vm_id": vm_id,
                "name": name,
                "pid": "PID",
                "pid_start_time": "START",
                "lifecycle_ready": True,
                "config": {"network": {"loopback_ip": "127.0.0.1"}},
            })
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"python3 -c 'import ctypes,signal,time; ctypes.CDLL(\"libc.so.6\").prctl(1, signal.SIGKILL); open(\"{child_ready}\", \"w\").close(); time.sleep(600)' &\n"
                    f"while [ ! -e {child_ready} ]; do sleep 0.01; done\n"
                    "read -r proc_stat < /proc/$$/stat; proc_stat=${proc_stat##*) }; "
                    "read -ra proc_fields <<< \"$proc_stat\"; start=${proc_fields[19]}\n"
                    f"printf '%s\\n' '{initial}' | sed -e \"s/\\\"PID\\\"/$$/\" -e \"s/\\\"START\\\"/$start/\" > {state_path}\n"
                    f": > {state_path}.lock\n"
                    f"( while [ ! -e {owner_wait_entered} ] && [ ! -e {owner_wait_release} ]; do sleep 0.01; done; "
                    f"if [ -e {owner_wait_entered} ]; then sleep 0.25; "
                    f"printf '%s\\n' '{ready}' | sed -e \"s/\\\"PID\\\"/$$/\" -e \"s/\\\"START\\\"/$start/\" > {state_path}.tmp; "
                    f"mv {state_path}.tmp {state_path}; fi ) &\n"
                    "wait\n"
                )
            os.chmod(stub, 0o755)

            real_drive = cdpdrive.drive
            real_wait_state_owned = reqbench.wait_state_owned
            cdpdrive.drive = lambda _args: {"ok": True, "stages": {}}
            owner_wait_calls = []

            def mark_owner_wait(*call_args, **call_kwargs):
                owner_wait_calls.append(True)
                open(owner_wait_entered, "w").close()
                return real_wait_state_owned(*call_args, **call_kwargs)

            reqbench.wait_state_owned = mark_owner_wait
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=5.0, teardown_timeout=5.0, cdp_port=port,
                state_dir=state_dir, data_root=d, ws_url="", run_id="test-run",
            )
            try:
                rec = reqbench.run_cdp_request(args, 0, fast=True)
            finally:
                open(owner_wait_release, "w").close()
                cdpdrive.drive = real_drive
                reqbench.wait_state_owned = real_wait_state_owned
                server.close()

            self.assertTrue(rec["ok"])
            self.assertEqual(owner_wait_calls, [True])
            self.assertTrue(os.path.exists(owner_wait_entered))
            self.assertGreaterEqual(rec["state_owner_wait_ms"], 150.0, rec)
            self.assertTrue(rec["teardown"]["all_gone"])
            self.assertFalse(os.path.lexists(state_path))
            self.assertFalse(os.path.lexists(data_dir))


class ExecArmTimeoutDoesNotReapALiveClone(unittest.TestCase):
    """The exec arm's timeout path must not delete a live microVM's only record.

    RED BEFORE THE FIX: `run_exec_request` SIGKILLed fcvm, waited only on fcvm
    ITSELF, then unconditionally `reap_disk`'d the state file AND rmtree'd
    `vm-disks/<vm_id>`. `children_of()` was never called — and it cannot be called
    after the kill, because `/proc/<fcvm>/task/*/children` is gone by then, so the
    survivors are unrecoverable. Observed:
        timed_out=True
        reaped=['.../state/vm-red.json', '.../vm-disks/vm-red']
        orphan child still alive=True   rootfs still on disk=False
    i.e. the rootfs was deleted underneath a running child. This is the same rule
    `teardown_fast` enforces at its `if not all_gone:` guard, bypassed.
    """

    def test_a_surviving_child_blocks_the_reap_and_raises(self):
        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            disks = os.path.join(d, "vm-disks", "vm-22222222222222222222222222222222")
            os.makedirs(state_dir)
            os.makedirs(disks)
            marker = os.path.join(disks, "rootfs.raw")
            with open(marker, "w") as f:
                f.write("golden reflink")
            stub = os.path.join(d, "fcvm-stub")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    "sleep 300 &\n"     # UNARMED child: survives parent SIGKILL
                    "read -r proc_stat < /proc/$$/stat; proc_stat=${proc_stat##*) }; "
                    "read -ra proc_fields <<< \"$proc_stat\"; start=${proc_fields[19]}\n"
                    f"cat > {state_dir}/vm-22222222222222222222222222222222.json <<EOF\n"
                    '{"vm_id": "vm-22222222222222222222222222222222", "name": "rb-test-run-0-exec", "pid": $$, "pid_start_time": $start, "lifecycle_ready": true}\n'
                    "EOF\n"
                    f": > {state_dir}/vm-22222222222222222222222222222222.json.lock\n"
                    "wait\n"
                )
            os.chmod(stub, 0o755)
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=0.6, teardown_timeout=0.4,
                state_dir=state_dir, data_root=d, run_id="test-run",
            )
            try:
                returned = reqbench.run_exec_request(args, 0)
            except reqbench.SurvivedTeardown as error:
                cm = error
            else:
                self.fail(f"live-child exec teardown returned instead of aborting: {returned}")
            rec = cm.record
            try:
                self.assertTrue(rec.get("survivors"), f"no survivor list in {rec}")
                self.assertIs(rec.get("disk_reap_skipped"), True)
                self.assertTrue(os.path.exists(marker),
                                "rootfs of a live child must NOT be reaped")
                self.assertTrue(os.path.exists(os.path.join(state_dir, "vm-22222222222222222222222222222222.json")),
                                "the state file is the only record that child is ours")
            finally:
                for pid in rec.get("survivors", {}):
                    try:
                        os.kill(int(pid), 9)
                    except (ProcessLookupError, PermissionError, ValueError):
                        pass


class TeardownFastCpuAccounting(unittest.TestCase):
    """The CPU-accounting windows must not be dominated by the harness's own spin.

    RED BEFORE THE FIX: the control window was `while time.monotonic()-t0 < 0.05:
    pass`, so `control_busy_cores` came back at 3.0-3.4 on this box against ~2.0
    for the same ambient load measured over a sleep — one full core of the
    harness's own `pass` loop, then multiplied by the whole reclaim window.
    """

    def setUp(self):
        # machine_counter_tracks_this_process memoizes into a module global,
        # because whether /proc/stat encloses this process is a property of the
        # host and re-measuring it burns a core for ~320 ms per call. That is
        # right in production and wrong across tests: the frozen-counter test
        # mocks the counter, caches False, and every later test then reads that
        # cached answer instead of asking. Observed as
        #   test_the_probe_reports_this_host_tracks_its_own_processes
        #   AssertionError: False is not true
        # on a host that tracks perfectly well. Reset per test so each one
        # measures what it claims to.
        reqbench._MACHINE_COUNTER_TRACKS = None

    def test_control_window_does_not_measure_our_own_spin(self):
        from unittest import mock

        p = spawn_pdeathsig_parent(["sleep", "300"])
        wait_for_child(p.pid)
        before = self_cpu_ms()
        calls = []
        real_machine = reqbench.machine_cpu_ms
        real_self = reqbench.self_cpu_ms

        def machine_counter():
            calls.append("machine")
            return real_machine()

        def self_counter():
            calls.append("self")
            return real_self()

        with (
            mock.patch.object(reqbench, "machine_cpu_ms", side_effect=machine_counter),
            mock.patch.object(reqbench, "self_cpu_ms", side_effect=self_counter),
        ):
            out = reqbench.teardown_fast(p.pid, "", "", "", 5.0)
        spent = self_cpu_ms() - before
        p.wait(timeout=5)
        self.assertEqual(
            calls,
            ["machine", "self", "self", "machine"] * 2,
            "each host-wide counter window must strictly enclose its harness window",
        )
        # 50 ms of control window: a spin would put >=45 ms of user time in it.
        # The harness counter advances in 10 ms jiffies here, so compare to the
        # measured wall window rather than demanding a sub-tick absolute 5 ms.
        self.assertLess(
            out["control_harness_cpu_ms"],
            0.5 * out["control_wall_ms"],
            f"control window burned {out['control_harness_cpu_ms']:.1f} ms of OUR cpu "
            f"(total call {spent:.1f} ms) — it is spinning, so control_busy_cores "
            f"({out['control_busy_cores']:.2f}) is ambient + the harness",
        )
        self.assertGreaterEqual(out["control_busy_cores"], 0.0)
        self.assertLessEqual(
            out["control_busy_cores_lo"], out["control_busy_cores"]
        )
        self.assertLessEqual(
            out["control_busy_cores"], out["control_busy_cores_hi"]
        )
        self.assertEqual(
            out["machine_cpu_source"],
            "/proc/stat:busy(user,nice,system,irq,softirq,steal)",
        )
        self.assertEqual(out["harness_cpu_source"], "/proc/self/stat:utime+stime")

    def test_cpu_residual_clamps_only_within_declared_resolution(self):
        uncertainty = reqbench.CPU_RESIDUAL_UNCERTAINTY_MS
        within = reqbench.bounded_cpu_residual(0.0, uncertainty / 2)
        self.assertLess(within["raw_ms"], 0.0)
        self.assertEqual(within["point_ms"], 0.0)
        self.assertEqual(within["lo_ms"], 0.0)
        self.assertGreater(within["hi_ms"], 0.0)
        self.assertIs(within["clamped"], True)

        normal = reqbench.bounded_cpu_residual(30.0, 10.0)
        self.assertEqual(normal["raw_ms"], 20.0)
        self.assertEqual(normal["point_ms"], 20.0)
        self.assertLessEqual(normal["lo_ms"], normal["point_ms"])
        self.assertLessEqual(normal["point_ms"], normal["hi_ms"])
        self.assertIs(normal["clamped"], False)

    def test_cpu_residual_rejects_an_impossible_negative_delta(self):
        """A machine counter that MOVED, but by less than the harness it encloses.

        The machine figure must be non-zero. A zero is a different condition
        entirely (the counter does not track this process at all) and is
        asserted separately below; using zero here made this test pass for a
        reason it did not intend.

        `tracks` states the environment: a host whose /proc/stat DOES enclose
        its processes. On such a host a shortfall this large is a real
        accounting bug and must still be raised as one.
        """
        with self.assertRaisesRegex(RuntimeError, "smaller than enclosed"):
            reqbench.bounded_cpu_residual(
                10.0, reqbench.CPU_RESIDUAL_UNCERTAINTY_MS + 100.0,
                tracks=lambda: True,
            )

    def test_a_dead_machine_counter_is_named_not_reported_as_a_violation(self):
        """machine=0 while the harness burned CPU means the counter is unusable.

        Observed on GitHub-hosted runners: machine=0.000000ms against
        harness=150.000000ms. That is not the measurement disagreeing with
        itself, it is /proc/stat not tracking this process, and reporting it as
        an enclosure violation sent a reader hunting an accounting bug that did
        not exist while the bench suite passed on every real bench host.
        """
        with self.assertRaises(reqbench.MachineCpuCounterUnusable) as caught:
            reqbench.bounded_cpu_residual(
                0.0, reqbench.CPU_RESIDUAL_UNCERTAINTY_MS + 10.0,
                tracks=lambda: False,
            )
        self.assertIn("does not track this process", str(caught.exception))

        # And it must NOT fire when the harness used nothing worth enclosing:
        # a genuinely idle window legitimately reads zero on both counters.
        quiet = reqbench.bounded_cpu_residual(0.0, 0.0)
        self.assertEqual(quiet["raw_ms"], 0.0)

    def test_one_tick_of_movement_is_not_tracking_either(self):
        """RED BEFORE THE FIX: the guard asked `machine_ms == 0.0`.

        That classified the GitHub-hosted runner correctly the first time
        (machine=0.000000ms harness=150.000000ms) and wrongly the next, when the
        same host reported

            machine=10.000000ms harness=160.000000ms raw=-150.000000ms

        One 10 ms jiffy is the counter's resolution, not evidence that it
        tracks us, so the `== 0.0` test let the identical environment through as
        an enclosure violation and failed the bench suite again.

        The classifier is now the probe, not the magnitude of the shortfall, so
        the SAME numbers land on either side depending only on what the host can
        actually do.
        """
        observed = (10.0, 160.0)  # verbatim from the failing CI job
        with self.assertRaises(reqbench.MachineCpuCounterUnusable):
            reqbench.bounded_cpu_residual(*observed, tracks=lambda: False)
        with self.assertRaisesRegex(RuntimeError, "smaller than enclosed"):
            reqbench.bounded_cpu_residual(*observed, tracks=lambda: True)

    def test_a_cpu_measurement_failure_does_not_abort_the_teardown(self):
        """RED BEFORE THE FIX: teardown reported "NOT reaped" for a reaped VM.

        bounded_cpu_residual runs AFTER the kill, the wait and t_gone, so by the
        time it can fail the process set is already terminal. Letting it
        propagate made teardown_fast raise SurvivedTeardown with

            state  and data  NOT reaped: host CPU delta is smaller than
            enclosed harness CPU delta: machine=30.000000ms harness=160.000000ms

        which is a statement about the MEASUREMENT dressed up as a statement
        about the PROCESSES. On GitHub-hosted runners it failed the bench suite
        for a teardown that had worked, and it set disk_reap_skipped on a VM
        whose disk was safe to reap.

        Fail-closed belongs on PUBLICATION: the figure is withheld and the error
        recorded, so a reader gets a KeyError rather than a plausible number.
        """
        from unittest import mock

        p = spawn_pdeathsig_parent(["sleep", "300"])
        wait_for_child(p.pid)
        kids = reqbench.children_of(p.pid)
        self.assertEqual(len(kids), 1, "parent never forked its child")

        boom = RuntimeError("host CPU delta is smaller than enclosed harness CPU delta")
        with mock.patch.object(reqbench, "bounded_cpu_residual", side_effect=boom):
            out = reqbench.teardown_fast(p.pid, "", "", "", 5.0)

        self.assertTrue(out["all_gone"], "the process set must still be reaped")
        self.assertNotIn("disk_reap_skipped", out,
                         "a measurement failure must not skip the disk reap")
        self.assertIn("per_child_cpu", out,
                      "per-child CPU is measured before the residual and must survive it")
        self.assertIn("RuntimeError", out["cpu_residual_error"] or "",
                      "the measurement error must be recorded, not swallowed")
        for absent in ("machine_cpu_ms_net", "control_busy_cores",
                       "cpu_residual_uncertainty_ms"):
            self.assertNotIn(absent, out,
                             f"{absent} must be ABSENT, not zeroed, when unmeasurable")
        for pid in kids:
            self.assertFalse(reqbench.proc_stat_fields(pid),
                             f"child {pid} survived a teardown that reported success")

    def test_the_probe_reports_this_host_tracks_its_own_processes(self):
        """The probe must say yes HERE, or it would excuse every real violation.

        A probe that answered "not tracking" on a normal Linux host would turn
        the enclosure check into a no-op everywhere — the fail-open shape this
        repo keeps finding. This is a bench host; /proc/stat encloses it.
        """
        self.assertTrue(
            reqbench.machine_counter_tracks_this_process(),
            "/proc/stat did not reflect a deliberate CPU burn by this process; "
            "if that is true of this host the enclosure check cannot work here",
        )

    def test_the_probe_reports_a_frozen_counter_as_untracked(self):
        """And it must say no when the machine counter does not move."""
        from unittest import mock

        with mock.patch.object(reqbench, "machine_cpu_ms", return_value=100.0):
            self.assertFalse(
                reqbench.machine_counter_tracks_this_process(),
                "a machine counter frozen across a deliberate burn is not tracking",
            )

    def test_ambient_load_alone_does_not_look_like_tracking(self):
        """The case a single burn window cannot distinguish.

        A counter that EXCLUDES this process but advances steadily because the
        box is busy satisfies `machine_delta >= spent - tolerance` on its own.
        The probe then declares the host healthy, and bounded_cpu_residual
        raises RuntimeError -- "your accounting is wrong" -- where it should
        raise MachineCpuCounterUnusable -- "this host cannot be measured". The
        operator is sent to debug the wrong thing, and nothing in the record
        says so.

        Here the stub counter grows purely with WALL time at 4 cores' worth of
        ambient load and never reflects our burn. Paired windows cancel it: the
        idle and burn windows are the same length, so ambient contributes
        equally to both and the difference is ~0.

        RED WITHOUT THE PAIRED DESIGN: the single-window version compared
        4 x window against our ~1 x window of burn and answered True.
        """
        from unittest import mock

        start = time.monotonic()

        def ambient_only():
            # 4 cores of unrelated work, entirely independent of what we burn.
            return (time.monotonic() - start) * 1000.0 * 4

        with mock.patch.object(reqbench, "machine_cpu_ms", side_effect=ambient_only):
            self.assertFalse(
                reqbench.machine_counter_tracks_this_process(),
                "a counter that only reflects ambient load was read as tracking "
                "this process; on a busy host that turns the enclosure check "
                "into a no-op",
            )

    def test_choppy_ambient_does_not_condemn_the_host(self):
        """Fluctuating load must read as "cannot tell", not as "not tracking".

        Pairing cancels STEADY ambient but amplifies FLUCTUATING ambient: a
        load that starts or stops between the idle and burn windows lands in
        the difference at full weight. Observed live -- two test suites running
        concurrently made the probe condemn this host (median excess dragged
        below tolerance), failing the positive-control test above once per
        ~six runs.

        The stub alternates ambient between 0 and 8 cores per WINDOW, the
        worst case: every idle window quiet, every burn window loud, then the
        reverse -- excesses scatter far beyond tolerance. A probe that trusts
        its median here condemns the host; one that notices the pairs disagree
        must fail toward "tracks" (keeping the strict violation path live) and
        must NOT memoize, because choppiness describes the moment, not the
        host.

        RED WITHOUT THE FIX: the median-only version returns False.
        """
        from unittest import mock

        # Four calls bracket each pair: idle start, idle end, burn start, burn
        # end. The deltas between consecutive calls are therefore
        # [idle-window, gap, burn-window, gap] per pair. Ambient of 8 cores
        # lands in pair 0's IDLE window (excess strongly negative), pair 1's
        # BURN window (strongly positive), nowhere in pair 2 (zero), and so on
        # -- the pairs DISAGREE, which is what distinguishes chop from a
        # counter that genuinely excludes this process (whose excesses agree
        # near zero; that case is the frozen/ambient tests above and must
        # still be condemned). The first stub of this test alternated ambient
        # IDENTICALLY in every pair, so all five excesses were equal, the
        # spread was zero, and the probe rightly condemned it -- a stub of
        # consistent chop is a stub of a broken counter.
        w = 8.0 * 320.0  # 8 cores for one ~320 ms window, in ms of CPU
        deltas = iter([0.0,  # first read
                       w, 0.0, 0.0, 0.0,    # pair 0: loud idle  -> excess -w
                       0.0, 0.0, w, 0.0,    # pair 1: loud burn  -> excess +w
                       0.0, 0.0, 0.0, 0.0,  # pair 2: quiet      -> excess 0
                       w, 0.0, 0.0, 0.0,    # pair 3: loud idle  -> excess -w
                       0.0, 0.0, w, 0.0])   # pair 4: loud burn  -> excess +w
        state = {"clock": 0.0}

        def choppy():
            state["clock"] += next(deltas, 0.0)
            return state["clock"]

        with mock.patch.object(reqbench, "machine_cpu_ms", side_effect=choppy):
            self.assertTrue(
                reqbench.machine_counter_tracks_this_process(),
                "a choppy ambient made the probe condemn the host; 'the pairs "
                "disagree' must read as 'cannot tell', not as 'not tracking'",
            )
        self.assertIsNone(
            reqbench._MACHINE_COUNTER_TRACKS,
            "a cannot-tell verdict was memoized; the next campaign inherits a "
            "guess about a moment, not a fact about the host",
        )

    def test_reclaim_sampler_does_not_burn_a_core(self):
        """The RECLAIM window must not spin either — the control window is only half.

        RED BEFORE THE FIX: `sample_all_until_gone` was a `while live and ...` loop
        with no sleep, no yield and no backoff, INSIDE the window whose CPU it
        reports. Measured on this box against a child that outlives its parent by
        ~0.5 s:
            reap_wall_ms=541.3  harness_cpu_ms=540.0   (100% of a core)
            machine_cpu_ms=630.0  machine_cpu_ms_excess=-17.9
        A NEGATIVE reclaim excess is physically impossible and is direct evidence
        the subtraction is broken: the subtrahend is a ~540 ms quantity quantized
        to one jiffy, and the signal it is meant to expose is "< 20 ms".
        """
        p = subprocess.Popen(["bash", "-c", "sleep 0.6 & wait"])
        wait_for_child(p.pid)
        out = reqbench.teardown_fast(p.pid, "", "", "", 5.0)
        p.wait(timeout=5)
        self.assertGreater(out["reap_wall_ms"], 300, "the child must outlive the kill")
        self.assertIn("sample_period_s", out, "the residual bias must be quantifiable")
        self.assertLess(
            out["harness_cpu_ms"],
            0.2 * out["reap_wall_ms"],
            f"the reclaim sampler burned {out['harness_cpu_ms']:.1f} ms cpu over "
            f"{out['reap_wall_ms']:.1f} ms wall "
            f"({100 * out['harness_cpu_ms'] / out['reap_wall_ms']:.0f}% of a core) "
            f"INSIDE the window whose CPU it reports",
        )

    def test_reclaim_cpu_is_reported_as_an_interval_not_a_bare_zero(self):
        """A sub-tick reclaim is `0.0 +/- one tick`, never a hard 0.0.

        RED BEFORE THE FIX: per_child_cpu carried only
        {'cpu_before_ms': 0.0, 'cpu_final_ms': 0.0, 'reclaim_cpu_ms': 0.0,
         'complete': False} — zero CPU quoted with zero uncertainty on data whose
        resolution is 1000/CLK_TCK ms (10 ms here). AGENTS.md defect 6.
        """
        p = spawn_pdeathsig_parent(["sleep", "300"])
        try:
            deadline = time.monotonic() + 10
            while not reqbench.children_of(p.pid) and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(reqbench.children_of(p.pid), "child never appeared")
            out = reqbench.teardown_fast(p.pid, "", "", "", 10.0)
        finally:
            kill_tree(p)
        self.assertTrue(out["all_gone"], "the pdeathsig child should have died with its parent")
        self.assertEqual(out["tick_ms"], 1000.0 / reqbench.CLK_TCK)
        self.assertTrue(out["per_child_cpu"], "no children were tracked")
        for name, c in out["per_child_cpu"].items():
            self.assertIn("below_resolution", c, f"{name} has no resolution flag")
            self.assertIn("reclaim_cpu_ms_hi", c, f"{name} has no upper bound")
            if c["reclaim_cpu_ms"] == 0.0:
                self.assertTrue(c["below_resolution"])
                self.assertEqual(c["reclaim_cpu_ms_hi"], 2 * out["tick_ms"])

    def test_the_child_wait_settles_on_exec_not_on_fork(self):
        """The pre-exec window is real, and waiting for the fork does not close it.

        RED BEFORE THE FIX: `test_every_tracked_child_gets_a_cpu_sample` waited
        for `len(children_of(pid)) >= 2` and then read `proc_comm`. Between fork
        and execve a child still carries its parent's comm, so that read can
        return "python3" for a child that will become "lingersleep". It did, on
        a GitHub-hosted runner, on 2026-08-16.

        This makes the window deterministic with a 0.5 s `preexec_fn` instead of
        hoping to catch a sub-millisecond one, and asserts both halves: the
        fork-only wait observes the parent's comm, and the exec-aware wait does
        not.
        """
        with tempfile.TemporaryDirectory() as d:
            child = os.path.join(d, "slowexec")
            shutil.copy("/bin/sleep", child)
            p = spawn_slow_exec_parent(child, 0.5)
            try:
                # The fork-only wait: exactly what the old precondition did.
                deadline = time.monotonic() + 10
                while (
                    len(reqbench.children_of(p.pid)) < 1
                    and time.monotonic() < deadline
                ):
                    time.sleep(0.005)
                kids = reqbench.children_of(p.pid)
                self.assertEqual(len(kids), 1, "parent never forked its child")
                self.assertEqual(
                    reqbench.proc_comm(kids[0]),
                    reqbench.proc_comm(p.pid),
                    "the pre-exec window did not reproduce, so this test proves "
                    "nothing about the wait — the child already carried its own "
                    "comm the moment the fork became visible",
                )
                # The exec-aware wait, on the same live process.
                kids, comms = wait_for_execed_children(p.pid, 1, 10)
                self.assertEqual(
                    comms, ["slowexec"],
                    "wait_for_execed_children returned before execve replaced comm",
                )
            finally:
                p.kill()
                p.wait(timeout=5)
                for k in kids:
                    try:
                        os.kill(k, signal.SIGKILL)
                    except (ProcessLookupError, PermissionError):
                        pass

    def test_every_tracked_child_gets_a_cpu_sample(self):
        """A fast child must not be skipped because a slow one was sampled first.

        RED BEFORE THE FIX: `{name: sample_until_gone(pid, deadline) for ...}` ran
        SEQUENTIALLY, so a later child was not looked at until every earlier one
        had gone. `/proc/<pid>/task/<tid>/children` is in FORK order (verified),
        so putting the long-lived child first and a short-lived one second makes
        the defect deterministic: by the time the second child's turn came it had
        been reaped, `proc_stat_fields` returned None on the very first read, and
        its `reclaim_cpu_ms` was recorded as null. Observed:
            AssertionError: ['fastexit'] != []
            these tracked children were never sampled at all: ['fastexit']
        """
        with tempfile.TemporaryDirectory() as d:
            # Distinct comms, so the two children cannot collide in `tracked`.
            linger = os.path.join(d, "lingersleep")
            fast = os.path.join(d, "fastexit")
            shutil.copy("/bin/sleep", linger)
            shutil.copy("/bin/sleep", fast)
            p = spawn_mixed_parent(linger, fast)
            try:
                kids, comms = wait_for_execed_children(p.pid, 2, 10)
                self.assertEqual(len(kids), 2, "parent never forked both children")
                # Precondition: the child that dies FIRST must be SECOND in fork
                # order, or this test is not exercising the ordering defect at all.
                self.assertEqual(
                    comms, ["lingersleep", "fastexit"],
                    "fork order is not [linger, fast]; the ordering defect is not exercised",
                )
                # `lingersleep` has no pdeathsig, so it survives and teardown_fast
                # (correctly) refuses to reap and raises. The partial record it
                # carries is what this test inspects.
                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(p.pid, "", "", "", 1.0)
                out = cm.exception.teardown
                # Name the failure. teardown_fast raises SurvivedTeardown from
                # three points BEFORE it samples CPU (attribution unprovable,
                # owner set unpinnable, measure_fast_reap failed), and on those
                # paths the record has no per_child_cpu at all. Reading it blind
                # turned a diagnosable environment failure into a bare KeyError
                # in CI, which said nothing about which path fired.
                # This used to branch on a MachineCpuCounterUnusable cause and
                # return early. That branch can no longer fire: the residual
                # errors are now caught inside measure_fast_reap and converted
                # to cpu_residual_error, so neither MachineCpuCounterUnusable
                # nor a plain enclosure RuntimeError escapes as a cause, and
                # per_child_cpu is produced on every host. A branch that cannot
                # execute is the shape AGENTS.md names, so it is gone rather
                # than left looking like it covers something.
                self.assertIn(
                    "per_child_cpu",
                    out,
                    "teardown failed BEFORE the CPU sampling this test inspects. "
                    f"reason={cm.exception.args[0] if cm.exception.args else '?'} "
                    f"attribution={out.get('child_attribution_established')} "
                    f"measurement_error={out.get('measurement_error')} "
                    f"keys={sorted(out)}",
                )
                missing = [n for n, c in out["per_child_cpu"].items()
                           if c["cpu_final_ms"] is None]
                self.assertEqual(
                    missing,
                    [],
                    f"these tracked children were never sampled at all: {missing} "
                    f"(all: {out['per_child_cpu']})",
                )
            finally:
                kill_tree(p)


class FindStateIsEventDriven(unittest.TestCase):
    """find_state must not burn a core while the measured clone is restoring.

    RED BEFORE THE FIX: `while time.monotonic() < deadline:` with an os.listdir +
    json.load of every file and no sleep. Measured 400 ms of harness CPU for a
    400 ms wait with 8 unrelated state files present — 100% of one core, at the
    exact instant a 2-vCPU clone is restoring on the same box.
    """

    def test_waiting_for_the_state_file_costs_no_cpu(self):
        with tempfile.TemporaryDirectory() as d:
            for i in range(8):  # unrelated files, as on a real box
                with open(os.path.join(d, f"vm-other{i}.json"), "w") as f:
                    json.dump({"vm_id": f"o{i}", "pid": 900000 + i}, f)

            target = os.path.join(d, "vm-mine.json")

            def writer():
                time.sleep(0.4)
                tmp = target + ".tmp"
                with open(tmp, "w") as f:
                    json.dump({
                        "vm_id": "vm-mine",
                        "pid": 123456,
                        "pid_start_time": 789,
                        "name": "mine",
                    }, f)
                os.rename(tmp, target)

            watch = reqbench.DirWatch(d)  # registered BEFORE the writer starts
            th = threading.Thread(target=writer)
            th.start()
            t0 = time.monotonic()
            c0 = self_cpu_ms()
            path, st = reqbench.find_state(
                d,
                123456,
                time.monotonic() + 10,
                watch,
                fcvm_start_time=789,
            )
            wall_ms = (time.monotonic() - t0) * 1000
            cpu_ms = self_cpu_ms() - c0
            th.join()
            watch.close()

            self.assertIsNotNone(st, "did not find the state file")
            self.assertEqual(path, target)
            self.assertGreater(wall_ms, 300, "the file landed at +400 ms; sanity check")
            self.assertLess(
                cpu_ms,
                0.1 * wall_ms,
                f"burned {cpu_ms:.0f} ms cpu over {wall_ms:.0f} ms wall "
                f"({100 * cpu_ms / wall_ms:.0f}% of a core) — this is a spin, not a wait",
            )

    def test_state_is_findable_by_name_when_pid_is_null(self):
        """fcvm writes the state file with `pid: null` until POST-RESUME.

        RED BEFORE THE FIX: `scan_state`/`find_state` matched only on pid, so the
        entire window between the first save and the post-resume pid write was
        invisible — and a file left behind with a null pid is never removed by
        fcvm's own sweeper either (`cleanup_stale_state` bails on a null pid).
        """
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "vm-x.json")
            with open(p, "w") as f:
                json.dump({"vm_id": "vm-x", "pid": None, "name": "rb-1-0-fast"}, f)
            path, st = reqbench.scan_state(d, 4242, "rb-1-0-fast")
            self.assertEqual(path, p)
            self.assertEqual(st["vm_id"], "vm-x")
            # ...and a pid-only scan still finds nothing, which is the bug's shape.
            self.assertEqual(reqbench.scan_state(d, 4242)[0], None)

    def test_pre_spawn_null_state_is_never_adopted_or_reaped(self):
        """A refused clone cannot inherit debris with the same requested name."""
        with tempfile.TemporaryDirectory() as d:
            vm_id = "vm-66666666666666666666666666666666"
            state_path = os.path.join(d, f"{vm_id}.json")
            data_dir = os.path.join(d, "vm-disks", vm_id)
            name = "rb-reused-run-0-noop"
            os.makedirs(data_dir)
            with open(state_path, "w") as f:
                json.dump({"vm_id": vm_id, "pid": None, "name": name}, f)
            with open(state_path + ".lock", "w") as f:
                f.write("")

            before_spawn = reqbench.state_path_baseline(d)
            path, state = reqbench.scan_state(
                d,
                4242,
                name,
                123456,
                before_spawn,
            )
            self.assertIsNone(path)
            self.assertIsNone(state)

            out = {}
            self.assertEqual(
                reqbench.reap_disk(
                    out,
                    d,
                    state_path,
                    data_dir,
                    (4242, 123456),
                ),
                [],
            )
            self.assertTrue(os.path.exists(state_path))
            self.assertTrue(os.path.isdir(data_dir))
            self.assertIn("exact state identity", " ".join(out["disk_errors"]))

            # A genuinely new null-PID path published after the baseline remains
            # discoverable for diagnostics; the old path never becomes eligible.
            new_vm_id = "vm-77777777777777777777777777777777"
            new_path = os.path.join(d, f"{new_vm_id}.json")
            with open(new_path, "w") as f:
                json.dump({"vm_id": new_vm_id, "pid": None, "name": name}, f)
            self.assertEqual(
                reqbench.scan_state(
                    d, 4242, name, 123456, before_spawn
                )[0],
                new_path,
            )

    def test_refused_clone_retains_pre_spawn_same_name_state(self):
        """The recovery path must not feed stale paths into teardown."""
        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            vm_id = "vm-99999999999999999999999999999999"
            name = "rb-reused-run-0-noop"
            state_path = os.path.join(state_dir, f"{vm_id}.json")
            data_dir = os.path.join(d, "vm-disks", vm_id)
            os.makedirs(state_dir)
            os.makedirs(data_dir)
            with open(state_path, "w") as f:
                json.dump({"vm_id": vm_id, "pid": None, "name": name}, f)
            with open(state_path + ".lock", "w") as f:
                f.write("")
            stub = os.path.join(d, "refused-fcvm")
            with open(stub, "w") as f:
                f.write("#!/bin/bash\nexit 42\n")
            os.chmod(stub, 0o755)
            args = argparse.Namespace(
                fcvm=stub,
                out_dir=d,
                snapshot_tag="",
                serve_pid=1,
                rust_log="off",
                timeout=2.0,
                teardown_timeout=0.1,
                cdp_port=9222,
                state_dir=state_dir,
                data_root=d,
                run_id="reused-run",
            )

            with self.assertRaises(reqbench.SurvivedTeardown) as raised:
                reqbench.run_noop_request(args, 0)
            record = raised.exception.record
            self.assertFalse(record["ok"])
            self.assertIsNot(
                record["teardown"].get("disk_cleanup_verified"), True
            )
            self.assertNotIn("recovered_state_by_name", record)
            self.assertTrue(os.path.exists(state_path))
            self.assertTrue(os.path.isdir(data_dir))

    def test_reused_pid_with_wrong_start_time_never_owns_or_reaps_state(self):
        """The same numeric PID is not the process identity."""
        with tempfile.TemporaryDirectory() as d:
            p = subprocess.Popen(["sleep", "300"])
            try:
                start_time = reqbench.proc_stat_fields(p.pid)[3]
                vm_id = "vm-88888888888888888888888888888888"
                name = "rb-reused-pid-0-fast"
                state_path = os.path.join(d, f"{vm_id}.json")
                data_dir = os.path.join(d, "vm-disks", vm_id)
                os.makedirs(data_dir)
                with open(state_path, "w") as f:
                    json.dump(
                        {
                            "vm_id": vm_id,
                            "name": name,
                            "pid": p.pid,
                            "pid_start_time": start_time + 1,
                            "lifecycle_ready": True,
                        },
                        f,
                    )
                with open(state_path + ".lock", "w") as f:
                    f.write("")

                self.assertIsNone(
                    reqbench.scan_state(
                        d,
                        p.pid,
                        name,
                        start_time,
                        frozenset(),
                        allow_unowned=False,
                    )[0]
                )
                watch = reqbench.DirWatch(d)
                try:
                    with self.assertRaisesRegex(TimeoutError, "never recorded"):
                        reqbench.wait_state_owned(
                            state_path,
                            p.pid,
                            time.monotonic(),
                            watch,
                            p,
                            start_time,
                            name,
                        )
                finally:
                    watch.close()

                out = {}
                self.assertEqual(
                    reqbench.reap_disk(
                        out, d, state_path, data_dir, (p.pid, start_time)
                    ),
                    [],
                )
                self.assertTrue(os.path.exists(state_path))
                self.assertTrue(os.path.isdir(data_dir))
                self.assertIn("expected exact owner", " ".join(out["disk_errors"]))
            finally:
                p.kill()
                p.wait(timeout=5)

    def test_waits_for_post_resume_owner_without_polling_or_early_teardown(self):
        """Port readiness can precede the state file's owner-PID update."""
        with tempfile.TemporaryDirectory() as d:
            state_path = os.path.join(d, "vm-x.json")
            p = subprocess.Popen(["sleep", "300"])
            start_time = reqbench.proc_stat_fields(p.pid)[3]
            with open(state_path, "w") as f:
                json.dump({"vm_id": "vm-x", "pid": None, "name": "rb-x"}, f)
            watch = reqbench.DirWatch(d)

            def claim_state():
                time.sleep(0.2)
                tmp = state_path + ".tmp"
                with open(tmp, "w") as f:
                    json.dump({
                        "vm_id": "vm-x", "pid": p.pid, "name": "rb-x",
                        "pid_start_time": start_time,
                        "lifecycle_ready": True,
                    }, f)
                os.rename(tmp, state_path)

            th = threading.Thread(target=claim_state)
            th.start()
            t0 = time.monotonic()
            try:
                state = reqbench.wait_state_owned(
                    state_path,
                    p.pid,
                    time.monotonic() + 5,
                    watch,
                    p,
                    start_time,
                    "rb-x",
                )
            finally:
                watch.close()
                th.join()
                p.kill()
                p.wait(timeout=5)
            self.assertEqual(state["pid"], p.pid)
            self.assertGreater(time.monotonic() - t0, 0.15)

    def test_process_exit_interrupts_owner_wait_immediately(self):
        with tempfile.TemporaryDirectory() as d:
            state_path = os.path.join(d, "vm-x.json")
            with open(state_path, "w") as f:
                json.dump({"vm_id": "vm-x", "pid": None, "name": "rb-x"}, f)
            watch = reqbench.DirWatch(d)
            p = subprocess.Popen(["true"])
            start_time = reqbench.proc_stat_fields(p.pid)[3]
            p.wait(timeout=5)
            t0 = time.monotonic()
            try:
                with self.assertRaisesRegex(RuntimeError, "before claiming state"):
                    reqbench.wait_state_owned(
                        state_path,
                        p.pid,
                        time.monotonic() + 30,
                        watch,
                        p,
                        start_time,
                        "rb-x",
                    )
            finally:
                watch.close()
            self.assertLess(time.monotonic() - t0, 0.5)


class WaitPortAttributesEarlyCloneExit(unittest.TestCase):
    def test_exited_clone_fails_immediately_with_log_tail(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "clone.log")
            with open(log, "wb") as f:
                p = subprocess.Popen(
                    [sys.executable, "-c", "print('clone admission refused')"],
                    stdout=f,
                    stderr=f,
                )
                p.wait(timeout=5)
            t0 = time.monotonic()
            with self.assertRaises(RuntimeError) as cm:
                reqbench.wait_port(
                    "127.0.0.1:9", time.monotonic() + 30, p, log
                )
            self.assertLess(time.monotonic() - t0, 0.5)
            self.assertIn("exited with status 0", str(cm.exception))
            self.assertIn("clone admission refused", str(cm.exception))

    def test_listener_that_opens_after_deadline_is_not_accepted(self):
        import socket

        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "clone.log")
            with open(log, "w") as f:
                f.write("clone still starting\n")
            proc = subprocess.Popen(["sleep", "5"])
            server = socket.socket()
            server.bind(("127.0.0.1", 0))
            endpoint = f"127.0.0.1:{server.getsockname()[1]}"

            def listen_late():
                time.sleep(0.15)
                server.listen(1)

            thread = threading.Thread(target=listen_late)
            thread.start()
            started = time.monotonic()
            try:
                with self.assertRaises(TimeoutError):
                    reqbench.wait_port(
                        endpoint, time.monotonic() + 0.05, proc, log
                    )
            finally:
                thread.join()
                server.close()
                proc.kill()
                proc.wait(timeout=5)
            self.assertLess(time.monotonic() - started, 0.4)


class WsUrlPrewiring(unittest.TestCase):
    """A prewired --ws-url must be re-hosted onto THIS clone's endpoint.

    RED BEFORE THE FIX: reqbench passed `ws_url=args.ws_url` verbatim into the
    per-clone Namespace, so a single URL named one fixed 127.x.y.z:port for the
    whole run while every clone had its own address.
    """

    def test_netloc_is_rebuilt_from_this_clones_endpoint(self):
        got = reqbench.clone_ws_url(
            "ws://127.0.0.99:9223/devtools/page/DEADBEEF", "127.0.0.19:9223"
        )
        self.assertEqual(got, "ws://127.0.0.19:9223/devtools/page/DEADBEEF")

    def test_path_carrying_the_target_id_is_preserved(self):
        got = reqbench.clone_ws_url("ws://x:1/devtools/page/ABC123", "127.0.0.5:9223")
        self.assertTrue(got.endswith("/devtools/page/ABC123"))


class ExecArmTimeout(unittest.TestCase):
    """A slow exec rep must be recorded as a failure, not abort the whole run.

    RED BEFORE THE FIX: `rc = proc.wait(timeout=...)` was bare, so
    subprocess.TimeoutExpired propagated out of run_exec_request AND out of main,
    orphaning the spawned fcvm (no pdeathsig from Python, harness is not a
    subreaper) with its whole VM tree into the next run.
    """

    def test_timeout_is_recorded_and_the_child_is_killed(self):
        with tempfile.TemporaryDirectory() as d:
            stub = os.path.join(d, "fcvm-stub")
            child_ready = os.path.join(d, "exec-child-ready")
            state = os.path.join(d, "vm-33333333333333333333333333333333.json")
            data = os.path.join(d, "vm-disks", "vm-33333333333333333333333333333333")
            os.makedirs(data)
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"python3 -c 'import ctypes,signal,time; ctypes.CDLL(\"libc.so.6\").prctl(1, signal.SIGKILL); open(\"{child_ready}\", \"w\").close(); time.sleep(600)' &\n"
                    f"while [ ! -e {child_ready} ]; do sleep 0.01; done\n"
                    "read -r proc_stat < /proc/$$/stat; proc_stat=${proc_stat##*) }; "
                    "read -ra proc_fields <<< \"$proc_stat\"; start=${proc_fields[19]}\n"
                    f"cat > {state} <<EOF\n"
                    '{"vm_id": "vm-33333333333333333333333333333333", "name": "rb-test-run-0-exec", "pid": $$, "pid_start_time": $start, "lifecycle_ready": true}\n'
                    "EOF\n"
                    f": > {state}.lock\n"
                    "wait\n"
                )
            os.chmod(stub, 0o755)
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=0.3, teardown_timeout=0.2,
                state_dir=d, data_root=d, run_id="test-run",
            )
            before = set(reqbench.children_of(os.getpid()))
            rec = reqbench.run_exec_request(args, 0)  # must NOT raise
            self.assertIs(rec["ok"], False)
            self.assertIs(rec["timed_out"], True)
            leaked = [
                pid for pid in reqbench.children_of(os.getpid())
                if pid not in before and reqbench.proc_stat_fields(pid)
                and reqbench.proc_stat_fields(pid)[0] not in ("Z", "X", "x")
            ]
            self.assertEqual(leaked, [], f"stub survived the timeout: {leaked}")


class _JsonListServer:
    """A stdlib HTTP server that speaks just enough `/json/list` to drive cdpdrive.

    `targets(elapsed_s)` decides what the list contains, so a test can make the
    page target appear LATE (readiness) or never (exhaustion). Every inbound Host
    header is recorded, which is how the `--host-header` claim gets checked
    instead of argued about.
    """

    def __init__(self, targets):
        import http.server

        self.hosts = []
        self.hits = 0
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802 - stdlib naming
                outer.hosts.append(self.headers.get("Host"))
                outer.hits += 1
                body = json.dumps(targets(time.monotonic() - outer.t0)).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_a):
                pass

        self.t0 = time.monotonic()
        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.endpoint = "127.0.0.1:%d" % self.srv.server_address[1]
        self.th = threading.Thread(target=self.srv.serve_forever, daemon=True)
        self.th.start()

    def __enter__(self):
        return self

    def __exit__(self, *_a):
        self.srv.shutdown()
        self.srv.server_close()
        self.th.join(timeout=5)


PAGE_TARGET = {"type": "page", "id": "ABC123", "url": "http://x/",
               "webSocketDebuggerUrl": "ws://127.0.0.1:1/devtools/page/ABC123"}


class CdpDriveHostHeader(unittest.TestCase):
    """The docstring has promised `--host-header` since the file was written.

    RED BEFORE THE FIX: argparse did not have it, so the invocation the docstring
    documents exited 2:
        $ python3 cdpdrive.py 127.0.0.1:9222 http://x/ --host-header example.com
        cdpdrive.py: error: unrecognized arguments: --host-header example.com
    This is a REPEAT of the defect the previous round fixed for `--print-target`
    in this same docstring, and did not audit the rest of the paragraph for.
    """

    def test_the_flag_is_accepted_and_actually_sets_the_header(self):
        with _JsonListServer(lambda _t: [PAGE_TARGET]) as s:
            r = subprocess.run(
                [sys.executable, os.path.join(HERE, "cdpdrive.py"), s.endpoint,
                 "http://x/", "--print-target", "--host-header", "evil.example"],
                capture_output=True, text=True, timeout=30,
            )
            self.assertNotEqual(r.returncode, 2, f"argparse rejected the flag: {r.stderr}")
            self.assertEqual(r.stdout.strip(), "ABC123")
            self.assertIn("evil.example", s.hosts)

    def test_without_the_flag_urllib_sends_the_ip_literal(self):
        with _JsonListServer(lambda _t: [PAGE_TARGET]) as s:
            r = subprocess.run(
                [sys.executable, os.path.join(HERE, "cdpdrive.py"), s.endpoint,
                 "http://x/", "--print-target"],
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(s.hosts, [s.endpoint])

    def test_reqbench_namespace_satisfies_cdpdrive(self):
        """Pin the producer/consumer coupling a new flag can silently break.

        `run_cdp_request` hand-builds an explicit, CLOSED field list. A `drive()`
        that reads `args.host_header` directly raises AttributeError — which is
        NOT in drive()'s except tuple, so it escapes drive(), is swallowed by
        run_cdp_request's `except Exception`, and every cdp/cdp-fast rep fails.
        This is the regression test the `--print-target` round should have left.
        """
        import cdpdrive

        ns = argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", format="jpeg", quality=80,
            timeout=0.35, idle_wait_ms=0.0, out_prefix="", ws_url="",
            connect_retries=2, nav_timing=True, print_target=False,
            render_module=os.path.join(HERE, "render.py"),
        )
        out = cdpdrive.drive(ns)  # must NOT raise AttributeError
        self.assertIs(out["ok"], False)
        self.assertEqual(out["stage"], "resolve", f"never reached resolve: {out}")


class CdpDriveResolveThrottling(unittest.TestCase):
    """Target resolution must be bounded by the DEADLINE, not by burning retries.

    RED BEFORE THE FIX: the loop had no sleep anywhere in it — the `timeout` on
    line 104 is urlopen's SOCKET timeout, not an inter-attempt delay. Measured on
    this box against a closed port with retries=200 and a 30 s deadline:
        attempts=200  elapsed=40.7 ms  rate=4919 req/s
        retry budget consumed in 0.041s of a 30.0s deadline -> 0.14% of the window
    A clone whose DevTools endpoint needs 100 ms more to present a page target is
    recorded as a hard CDP failure — and `reqanalyze` sets `pub = n_bad == 0`, so
    one spurious exhaustion censors the whole arm.
    """

    def test_it_sleeps_between_attempts_and_spends_the_deadline(self):
        import cdpdrive

        attempts = [0]
        real = urllib.request.urlopen

        def counting(*_a, **_k):
            attempts[0] += 1
            raise ConnectionRefusedError(111, "Connection refused")

        urllib.request.urlopen = counting
        t0 = time.monotonic()
        try:
            with self.assertRaises(ConnectionError):
                cdpdrive.resolve_target("127.0.0.1:1", time.monotonic() + 1.0, 200)
        finally:
            urllib.request.urlopen = real
        elapsed = time.monotonic() - t0
        self.assertLessEqual(attempts[0], 25,
                             f"{attempts[0]} attempts in {elapsed:.3f}s — still a burst")
        self.assertGreaterEqual(elapsed, 0.9,
                                f"gave up after {elapsed:.3f}s of a 1.0s deadline")

    def test_a_late_page_target_is_resolved_not_failed(self):
        import cdpdrive

        with _JsonListServer(lambda t: [PAGE_TARGET] if t > 0.3 else []) as s:
            got = cdpdrive.resolve_target(s.endpoint, time.monotonic() + 5.0, 200)
        self.assertEqual(got["id"], "ABC123")

    def test_readiness_exhaustion_is_not_classified_as_transport(self):
        """`resolve_target` raised bare ConnectionError regardless of cause, and
        ConnectionError subclasses OSError, so `no page target among 3` — pure
        readiness — was counted as a TRANSPORT drop. That is the very count
        REVIEW.md's WsClosed diagnosis rests on."""
        import cdpdrive

        with _JsonListServer(lambda _t: []) as s:
            ns = argparse.Namespace(
                cdp_host=s.endpoint, url="http://x/", format="jpeg", quality=80,
                timeout=0.6, idle_wait_ms=0.0, out_prefix="", ws_url="",
                connect_retries=200, nav_timing=False, print_target=False,
                host_header="", render_module=os.path.join(HERE, "render.py"),
            )
            out = cdpdrive.drive(ns)
        self.assertIs(out["ok"], False)
        self.assertNotEqual(out["failure_class"], "transport",
                            "a clone that is merely not ready yet is not a transport drop")
        self.assertEqual(out["failure_class"], "readiness")
        self.assertGreater(out.get("resolve_attempts", 0), 1,
                           "the attempt count must be recorded so a retried "
                           "resolve_ms is separable from a first-try one")


class CdpDriveNavigationFailurePhases(unittest.TestCase):
    """A transport close must identify which navigation wait observed it.

    `Page.navigate` has two independent waits: the command response and the later
    `Page.loadEventFired` event. Both used to report only `stage=navigate`, making
    the preserved 108-second failures incapable of distinguishing a renderer that
    never answered the command from one that answered and then lost its lifecycle
    event or transport.
    """

    @staticmethod
    def _args():
        return argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", format="jpeg", quality=80,
            timeout=1.0, idle_wait_ms=0.0, out_prefix="", ws_url="ws://unused",
            connect_retries=1, nav_timing=False, print_target=False,
            host_header="", render_module=os.path.join(HERE, "render.py"),
        )

    @staticmethod
    def _drive(fail_at):
        import cdpdrive

        class FakeWsClosed(Exception):
            pass

        class FakeWs:
            tcp_ms = 0.1
            upgrade_ms = 0.2

            def close(self):
                pass

        class FakeCdp:
            def __init__(self, _ws):
                pass

            def cmd(self, method, _params=None, deadline=0):
                del deadline
                if method == "Page.navigate":
                    if fail_at == "command":
                        raise ConnectionResetError(104, "Connection reset by peer")
                    return {"loaderId": "loader-1"}
                return {}

            def wait_event(self, _pred, _deadline):
                raise FakeWsClosed("connection closed mid-frame")

        fake_render = types.SimpleNamespace(Cdp=FakeCdp, WsClosed=FakeWsClosed)
        real_load = cdpdrive.load_render
        real_ws = cdpdrive.TimedWs
        cdpdrive.load_render = lambda _path: fake_render
        cdpdrive.TimedWs = lambda _render, _url, _deadline: FakeWs()
        try:
            return cdpdrive.drive(CdpDriveNavigationFailurePhases._args())
        finally:
            cdpdrive.load_render = real_load
            cdpdrive.TimedWs = real_ws

    def test_reset_waiting_for_page_navigate_response_is_identified(self):
        out = self._drive("command")
        self.assertFalse(out["ok"])
        self.assertEqual(out["stage"], "navigate-command-response")
        self.assertEqual(out["failure_operation"], "Page.navigate response")
        self.assertEqual(out["transport_signal"], "tcp-rst")

    def test_peer_eof_waiting_for_load_event_is_identified(self):
        out = self._drive("lifecycle")
        self.assertFalse(out["ok"])
        self.assertEqual(out["stage"], "navigate-load-event")
        self.assertEqual(out["failure_operation"], "Page.loadEventFired wait")
        self.assertEqual(out["transport_signal"], "tcp-eof")


class SnapshotGenerationIdentity(unittest.TestCase):
    """A reusable tag is never the identity of benchmark evidence."""

    SNAPSHOT = "reused-tag"
    VM_ID = "vm-11111111111111111111111111111111"
    IMAGE_CACHE_KEY = "a" * 64

    @classmethod
    def _write_generation(cls, data_root, generation_id):
        snapshot_dir = os.path.join(data_root, "snapshots", cls.SNAPSHOT)
        os.makedirs(snapshot_dir, exist_ok=True)
        config = {
            "generation_id": generation_id,
            "created_at": "2026-08-09T00:00:00Z",
            "vm_id": cls.VM_ID,
            "metadata": {
                "image": "localhost/chromium-bench-req",
                "image_disk_path": (
                    f"/image-cache/{cls.IMAGE_CACHE_KEY}.storage-v2.img"
                ),
                "vcpu": 2,
                "memory_mib": 1024,
                "network_mode": "rootless",
                "port_mappings": [{
                    "host_ip": None,
                    "host_port": 9222,
                    "guest_port": 9222,
                    "proto": "tcp",
                }],
            },
        }
        config_json = (
            json.dumps(config, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
        config_sha256 = hashlib.sha256(config_json).hexdigest()
        config_path = os.path.join(snapshot_dir, "config.json")
        with open(config_path, "wb") as target:
            target.write(config_json)
        provenance = {
            "snapshot_generation_id": generation_id,
            "snapshot_config_sha256": config_sha256,
            "snapshot_created_at": config["created_at"],
            "snapshot_vm_id": config["vm_id"],
            "image": config["metadata"]["image"],
            "image_id": "sha256:" + "b" * 64,
            "image_digest": "sha256:" + cls.IMAGE_CACHE_KEY,
            "image_cache_key": cls.IMAGE_CACHE_KEY,
            "creator_fcvm_sha256": "c" * 64,
            "creator_runtime_bundle_sha256": "d" * 64,
            "source_revision": "e" * 40,
        }
        with open(
            os.path.join(snapshot_dir, "reqbench-provenance.json"), "w"
        ) as target:
            json.dump(provenance, target)
        return config_path, config_sha256

    def test_exact_generation_and_config_digest_load(self):
        with tempfile.TemporaryDirectory() as data_root:
            generation_id = "11111111-1111-4111-8111-111111111111"
            _path, config_sha256 = self._write_generation(
                data_root, generation_id,
            )
            generation = reqbench.snapshot_generation(data_root, self.SNAPSHOT)
            self.assertEqual(generation["generation_id"], generation_id)
            self.assertEqual(generation["config_sha256"], config_sha256)

    def test_recreated_tag_rejects_the_previous_generation_provenance(self):
        with tempfile.TemporaryDirectory() as data_root:
            config_path, _digest = self._write_generation(
                data_root, "11111111-1111-4111-8111-111111111111",
            )
            with open(config_path) as source:
                replacement = json.load(source)
            replacement["generation_id"] = (
                "22222222-2222-4222-8222-222222222222"
            )
            with open(config_path, "w") as target:
                json.dump(replacement, target, sort_keys=True)
                target.write("\n")
            with self.assertRaisesRegex(RuntimeError, "snapshot_generation_id"):
                reqbench.snapshot_generation(data_root, self.SNAPSHOT)

    def test_in_place_config_change_rejects_the_previous_digest(self):
        with tempfile.TemporaryDirectory() as data_root:
            config_path, _digest = self._write_generation(
                data_root, "11111111-1111-4111-8111-111111111111",
            )
            with open(config_path, "ab") as target:
                target.write(b" ")
            with self.assertRaisesRegex(RuntimeError, "snapshot_config_sha256"):
                reqbench.snapshot_generation(data_root, self.SNAPSHOT)

    def test_main_holds_the_generation_lease_through_the_full_schedule(self):
        """An exclusive tag replacement waits until reqbench.main returns."""
        with tempfile.TemporaryDirectory() as data_root:
            self._write_generation(
                data_root, "11111111-1111-4111-8111-111111111111",
            )
            runtime_bundle = os.path.join(data_root, "runtime")
            os.makedirs(runtime_bundle)
            manifest_path = os.path.join(runtime_bundle, "MANIFEST.sha256")
            with open(manifest_path, "w") as target:
                target.write("sealed runtime fixture\n")
            fcvm = os.path.join(runtime_bundle, "fcvm")
            with open(fcvm, "w") as target:
                target.write("#!/bin/sh\nexit 0\n")
            os.chmod(fcvm, 0o755)

            lock_path = os.path.join(
                data_root, "snapshots", f"{self.SNAPSHOT}.lock",
            )
            begin_contender = threading.Event()
            first_attempt_finished = threading.Event()
            retry_after_main_return = threading.Event()
            retry_finished = threading.Event()
            main_returned = threading.Event()
            first_attempt = []
            retry_attempt = []
            contender_errors = []

            def try_exclusive(contender_lock):
                try:
                    fcntl.flock(
                        contender_lock,
                        fcntl.LOCK_EX | fcntl.LOCK_NB,
                    )
                except BlockingIOError:
                    return "blocked"
                fcntl.flock(contender_lock, fcntl.LOCK_UN)
                return "acquired"

            def contend_for_generation():
                try:
                    if not begin_contender.wait(timeout=5):
                        raise AssertionError("request never released contender")
                    with open(lock_path, "a+") as contender_lock:
                        first_attempt.append(try_exclusive(contender_lock))
                        first_attempt_finished.set()
                        if not retry_after_main_return.wait(timeout=5):
                            raise AssertionError(
                                "caller never released post-main retry",
                            )
                        retry_attempt.append(try_exclusive(contender_lock))
                except BaseException as error:  # keep thread failures observable
                    contender_errors.append(repr(error))
                finally:
                    first_attempt_finished.set()
                    retry_finished.set()

            contender = threading.Thread(
                target=contend_for_generation,
                name="snapshot-generation-exclusive-contender",
            )
            contender.start()

            request_observations = []

            def record(arm, rep):
                return {
                    "arm": arm,
                    "rep": rep,
                    "ok": True,
                    "blocking_ms": 1.0,
                    "wall_ms": 1.0,
                    "teardown": {},
                }

            def run_exec(_args, rep):
                begin_contender.set()
                completed = first_attempt_finished.wait(timeout=5)
                request_observations.append({
                    "attempt_completed": completed,
                    "result": first_attempt[0] if first_attempt else None,
                })
                return record("exec", rep)

            def run_noop(_args, rep):
                return record("noop", rep)

            # `probe` is recorded, not ignored: this is the only test that
            # drives the real `main`, so it is the only place the failure
            # probe's arrival at a CDP arm can be observed end to end rather
            # than asserted about the source.
            cdp_probes = []

            def run_cdp(_args, rep, fast, probe=None):
                cdp_probes.append(probe)
                return record("cdp-fast" if fast else "cdp", rep)

            saved = {
                "HERE": reqbench.HERE,
                "run_exec_request": reqbench.run_exec_request,
                "run_noop_request": reqbench.run_noop_request,
                "run_cdp_request": reqbench.run_cdp_request,
                "sha256_file": reqbench.sha256_file,
                "harness_sha256": reqbench.harness_sha256,
                "command_text": reqbench.command_text,
                "pending_signal": reqbench._pending_harness_signal,
                "argv": sys.argv,
                "sigint": signal.getsignal(signal.SIGINT),
                "sigterm": signal.getsignal(signal.SIGTERM),
            }
            env_updates = {
                "REQBENCH_RUNTIME_BUNDLE": runtime_bundle,
                "REQBENCH_SOURCE_REVISION": "e" * 40,
                "REQBENCH_GUARD_LOADAVG1": "0.1",
                "REQBENCH_GUARD_VM_PROCESSES": "0",
                "REQBENCH_QUIET_LOADAVG1_LIMIT": "2.0",
                "REQBENCH_QUIET_GUARD": "1",
                "ALLOW_BUSY": "0",
            }
            saved_env = {key: os.environ.get(key) for key in env_updates}
            out_dir = os.path.join(data_root, "results")
            rc = None
            retry_completed = False

            exact_hashes = {
                os.path.realpath(fcvm): "c" * 64,
                os.path.realpath(manifest_path): "d" * 64,
            }

            def fixture_sha256(path):
                return exact_hashes[os.path.realpath(path)]

            try:
                os.environ.update(env_updates)
                reqbench.HERE = runtime_bundle
                reqbench.run_exec_request = run_exec
                reqbench.run_noop_request = run_noop
                reqbench.run_cdp_request = run_cdp
                reqbench.sha256_file = fixture_sha256
                reqbench.harness_sha256 = lambda: "f" * 64
                reqbench.command_text = lambda _argv: "fcvm fixture"
                reqbench._pending_harness_signal = 0
                sys.argv = [
                    "reqbench.py",
                    "--snapshot-tag", self.SNAPSHOT,
                    "--snapshot-name", self.SNAPSHOT,
                    "--url", "http://fixture/medium.html",
                    # The recorded shuffle puts exec last for this seed, so the
                    # rejected exclusive attempt occurs at the end of the real
                    # schedule rather than only at its first request.
                    "--arms", "cdp-fast,noop,exec",
                    "--reps", "1",
                    "--warmup", "0",
                    "--image", "localhost/chromium-bench-req",
                    "--image-id", "sha256:" + "b" * 64,
                    "--network-mode", "rootless",
                    "--cpu", "2",
                    "--memory-mib", "1024",
                    "--fcvm", fcvm,
                    "--data-root", data_root,
                    "--out-dir", out_dir,
                    "--run-id", "1" * 32,
                ]
                rc = reqbench.main()
                main_returned.set()
                retry_after_main_return.set()
                retry_completed = retry_finished.wait(timeout=5)
            finally:
                begin_contender.set()
                retry_after_main_return.set()
                contender.join(timeout=5)
                reqbench.HERE = saved["HERE"]
                reqbench.run_exec_request = saved["run_exec_request"]
                reqbench.run_noop_request = saved["run_noop_request"]
                reqbench.run_cdp_request = saved["run_cdp_request"]
                reqbench.sha256_file = saved["sha256_file"]
                reqbench.harness_sha256 = saved["harness_sha256"]
                reqbench.command_text = saved["command_text"]
                reqbench._pending_harness_signal = saved["pending_signal"]
                sys.argv = saved["argv"]
                signal.signal(signal.SIGINT, saved["sigint"])
                signal.signal(signal.SIGTERM, saved["sigterm"])
                for key, value in saved_env.items():
                    if value is None:
                        os.environ.pop(key, None)
                    else:
                        os.environ[key] = value

            self.assertEqual(rc, 0)
            self.assertTrue(cdp_probes, "the cdp-fast arm never ran")
            self.assertTrue(
                all(isinstance(p, reqbench.FailureProbe) for p in cdp_probes),
                f"main must hand every CDP arm a real failure probe: {cdp_probes}",
            )
            self.assertEqual(contender_errors, [])
            self.assertEqual(request_observations, [{
                "attempt_completed": True,
                "result": "blocked",
            }])
            self.assertEqual(first_attempt, ["blocked"])
            self.assertTrue(main_returned.is_set())
            self.assertTrue(retry_completed)
            self.assertEqual(retry_attempt, ["acquired"])
            self.assertFalse(contender.is_alive())
            with open(os.path.join(out_dir, "reqbench.jsonl")) as source:
                records = [json.loads(line) for line in source]
            self.assertEqual(records[0]["kind"], "meta")
            self.assertEqual(
                {record["arm"] for record in records[1:]},
                {"exec", "cdp-fast", "noop"},
            )


class AnalyzerAvailability(unittest.TestCase):
    """Failed requests must be counted per arm, and must not hide a leak.

    RED BEFORE THE FIX (both observed by running the shipped analyzer on a
    synthetic jsonl of 30 exec-ok + 27 cdp-ok + 3 cdp-failed-with-all_gone-false):
      * it printed `all_gone: 27/27 confirmed` with no warning, because the
        teardown section read the ok-FILTERED list, so `all_gone: False` on a
        FAILED record was never examined;
      * `arms.cdp` had no `attempted`/`failed`/`failure_rate` keys at all — only
        one global scalar `n_failed` over every arm.
    """

    def _synthetic(self, path):
        self._write_clean_backend(path, "file", 6, 384.0)
        with open(path) as source:
            rows = [json.loads(line) for line in source]
        failed = 0
        for row in rows:
            if row.get("arm") != "cdp" or row.get("warmup") or failed == 3:
                continue
            row.update({
                "ok": False,
                "error": "WsClosed: connection closed mid-frame",
                "failure_class": "transport",
                "failure_stage": "navigate",
                "blocking_ms": 5250.0,
                "wall_ms": 5300.0,
                "render": {
                    "ok": False,
                    "error": "WsClosed: connection closed mid-frame",
                    "failure_class": "transport",
                    "stage": "navigate",
                },
            })
            row["teardown"].update({
                "all_gone": False,
                "disk_cleanup_verified": False,
                "survivors": {"firecracker": 1234},
            })
            failed += 1
        with open(path, "w") as target:
            for row in rows:
                target.write(json.dumps(row) + "\n")

    @staticmethod
    def _write_clean_backend(
        path,
        backend,
        measured,
        blocking_ms,
        noop_values=None,
        **cell_overrides,
    ):
        if noop_values is None:
            noop_values = (50.0,) * measured
        if len(noop_values) != measured:
            raise ValueError("noop_values must contain one value per measured repetition")
        arms = ["exec", "cdp", "cdp-fast", "noop"]
        warmup = 2
        seed = 1
        run_id = "fixture-" + os.path.basename(path).replace(".", "-")
        meta = {
            "kind": "meta", "run_id": run_id, "seed": seed, "backend": backend,
            "uffd_mode": "file" if backend == "file" else "copy",
            "arms": arms, "reps": measured, "warmup": warmup,
            "url": "http://fixture/medium.html", "format": "jpeg", "quality": 80,
            "image": "localhost/chromium-bench-req",
            "image_id": "sha256:" + "d" * 64, "snapshot": f"snapshot-{backend}",
            "snapshot_generation_id": (
                "11111111-1111-4111-8111-111111111111"
                if backend == "file"
                else "22222222-2222-4222-8222-222222222222"
            ),
            "snapshot_config_sha256": ("6" if backend == "file" else "7") * 64,
            "snapshot_created_at": "2026-08-09T00:00:00Z",
            "snapshot_vm_id": "vm-" + ("e" if backend == "file" else "f") * 32,
            "fcvm_sha256": "a" * 64, "harness_sha256": "c" * 64,
            "runtime_bundle_sha256": "8" * 64,
            "source_revision": "b" * 40,
            "cdp_port": 9222, "network_mode": "rootless", "cpu": 2,
            "port_mappings": [{
                "host_ip": None, "host_port": 9222, "guest_port": 9222,
                "proto": "tcp",
            }],
            "memory_mib": 1024,
            "rust_log": "fcvm=debug", "ws_url_prewired": False,
            "allow_busy": False, "quiet_guard_passed": True,
            "quiet_guard_loadavg1": 0.1, "quiet_vm_processes": 0,
            "quiet_loadavg1_limit": 2.0,
            "host_boot_id": "00000000-0000-0000-0000-000000000001",
            "host_kernel_release": "6.18.0-fixture", "host_machine": "aarch64",
            "loadavg": ["0.1", "0.1", "0.1"], "started": 1.0,
        }
        meta.update(cell_overrides)
        schedule = []
        rng = random.Random(seed)
        for rep in range(warmup + measured):
            order = list(arms)
            rng.shuffle(order)
            schedule.extend((rep, arm, rep < warmup) for arm in order)
        with open(path, "w") as f:
            f.write(json.dumps(meta) + "\n")
            for rep, arm, is_warmup in schedule:
                value = (
                    50.0 if is_warmup and arm == "noop"
                    else noop_values[rep - warmup] if arm == "noop"
                    else blocking_ms
                )
                mode = "fast" if arm == "cdp-fast" else arm if arm == "exec" else "normal"
                record = {
                    "arm": arm,
                    "rep": rep,
                    "warmup": is_warmup,
                    "run_id": run_id,
                    "record_id": f"{run_id}:{arm}:{rep}:{int(is_warmup)}",
                    "ok": True,
                    "blocking_ms": value,
                    "wall_ms": value + 1,
                    "loadavg1": 0.1,
                    "teardown": {
                        "mode": mode,
                        "all_gone": True,
                        "disk_cleanup_verified": True,
                        "child_attribution_established": True,
                        "teardown_total_ms": 1.0,
                    },
                }
                if arm.startswith("cdp"):
                    record["state_to_port_ms"] = 0.1
                    record["spawn_to_port_ms"] = 1.0
                    record["endpoint"] = "127.0.0.2:9222"
                    record["render"] = {
                        "ok": True,
                        "url": meta["url"],
                        "format": meta["format"],
                        "cdp_host": "127.0.0.2:9222",
                        "idle_timeout": 0,
                        "target_prewired": False,
                        "stages": {
                            "resolve_ms": 0.1, "tcp_ms": 0.08,
                            "upgrade_ms": 0.1, "enable_ms": 0.1,
                            "connect_total_ms": 0.4, "navigate_ms": 1.0,
                            "idle_ms": 0.0, "screenshot_ms": 1.0,
                            "decode_ms": 0.1, "nav_timing_ms": 0.1,
                            "total_ms": value,
                        },
                        "image_bytes": 1024,
                        "image_sha256": "9" * 64,
                        "width": 800,
                        "height": 600,
                        "quality": 80,
                        "nav": {
                            "dns_ms": 0.0, "connect_ms": 0.0,
                            "tls_ms": 0.0, "ttfb_ms": 0.1,
                            "resp_ms": 0.1, "load_ms": 1.0,
                        },
                    }
                if arm == "cdp-fast":
                    machine_resolution = 10.0
                    harness_resolution = 10.0
                    uncertainty = (
                        6 * machine_resolution + 2 * harness_resolution
                    )
                    machine_raw = 10.0 - 1.0
                    machine_net = max(0.0, machine_raw)
                    machine_lo = max(0.0, machine_raw - uncertainty)
                    machine_hi = max(0.0, machine_raw + uncertainty)
                    control_raw = 5.0 - 1.0
                    control_net = max(0.0, control_raw)
                    control_lo = max(0.0, control_raw - uncertainty)
                    control_hi = max(0.0, control_raw + uncertainty)
                    control_wall = 50.0
                    control_rate = control_net / control_wall
                    control_rate_lo = control_lo / control_wall
                    control_rate_hi = control_hi / control_wall
                    machine_window = 10.0
                    record["teardown"].update({
                        "accounting_version": "post-terminal-ambient-v2",
                        "reap_wall_ms": 1.0,
                        "machine_cpu_ms": 10.0,
                        "harness_cpu_ms": 1.0,
                        "machine_cpu_window_ms": machine_window,
                        "machine_cpu_ms_raw": machine_raw,
                        "machine_cpu_ms_net": machine_net,
                        "machine_cpu_ms_net_lo": machine_lo,
                        "machine_cpu_ms_net_hi": machine_hi,
                        "machine_cpu_ms_subtraction_clamped": False,
                        "machine_cpu_ms_excess": (
                            machine_net - control_rate * machine_window
                        ),
                        "machine_cpu_ms_excess_lo": (
                            machine_lo - control_rate_hi * machine_window
                        ),
                        "machine_cpu_ms_excess_hi": (
                            machine_hi - control_rate_lo * machine_window
                        ),
                        "control_machine_cpu_ms": 5.0,
                        "control_harness_cpu_ms": 1.0,
                        "control_wall_ms": control_wall,
                        "control_target_ms": 50.0,
                        "control_cpu_ms_raw": control_raw,
                        "control_cpu_ms_net": control_net,
                        "control_cpu_ms_net_lo": control_lo,
                        "control_cpu_ms_net_hi": control_hi,
                        "control_cpu_ms_subtraction_clamped": False,
                        "control_busy_cores": control_rate,
                        "control_busy_cores_lo": control_rate_lo,
                        "control_busy_cores_hi": control_rate_hi,
                        "cpu_residual_uncertainty_ms": uncertainty,
                        "machine_cpu_source": (
                            "/proc/stat:busy(user,nice,system,irq,softirq,steal)"
                        ),
                        "machine_cpu_resolution_ms": machine_resolution,
                        "harness_cpu_source": "/proc/self/stat:utime+stime",
                        "harness_cpu_resolution_ms": harness_resolution,
                        "sample_period_s": 0.0002,
                        "tick_ms": 10.0,
                        "per_child_cpu": {
                            "fcvm": {"reclaim_cpu_ms": 0.0, "complete": True}
                        },
                    })
                elif arm == "exec":
                    record.update({
                        "rc": 0,
                        "timed_out": False,
                        "render_total_ms": max(0.0, value - 1.0),
                    })
                elif arm == "noop":
                    record["spawn_to_port_ms"] = value
                f.write(json.dumps(record) + "\n")

    @staticmethod
    def _fast_median_ci(xs, *_args, **_kwargs):
        values = sorted(float(x) for x in xs if x is not None)
        if not values:
            return None, None, None, 0
        return reqanalyze.statistics.median(values), values[0], values[-1], len(values)

    @staticmethod
    def _fast_shift(a, b, *_args, **_kwargs):
        if not a or not b:
            return None, None, None
        delta = reqanalyze.statistics.median(b) - reqanalyze.statistics.median(a)
        return delta, delta, delta

    def _run_gate_fixture(self, argv):
        # The production bootstrap remains covered elsewhere. These tests target
        # input partitioning and gate truth, so avoid tens of millions of random
        # resamples over deliberately constant 200-record fixtures.
        from unittest import mock
        with (
            mock.patch.object(reqanalyze, "median_ci", self._fast_median_ci),
            mock.patch.object(reqanalyze, "hodges_lehmann_shift", self._fast_shift),
        ):
            return reqanalyze.main_with(argv)

    @staticmethod
    def _metadata_errors(path_or_paths):
        paths = (
            [path_or_paths]
            if isinstance(path_or_paths, (str, os.PathLike))
            else list(path_or_paths)
        )
        return [
            error
            for dataset in reqanalyze.load(paths)
            for error in dataset["metadata_errors"]
        ]

    @staticmethod
    def _mutate_record(path, arm, mutation, *, warmup=False):
        with open(path) as source:
            rows = [json.loads(line) for line in source]
        record = next(
            row for row in rows
            if row.get("arm") == arm and row.get("warmup") is warmup
        )
        mutation(record)
        with open(path, "w") as target:
            for row in rows:
                target.write(json.dumps(row) + "\n")
        return record

    def test_strict_json_rejects_duplicate_keys_constants_and_non_objects(self):
        cases = {
            "duplicate-key": ('{"kind":"meta","kind":"meta"}\n', "duplicate JSON key"),
            "nan": ("NaN\n", "non-standard JSON numeric constant NaN"),
            "infinity": ("Infinity\n", "non-standard JSON numeric constant Infinity"),
            "negative-infinity": (
                "-Infinity\n", "non-standard JSON numeric constant -Infinity"
            ),
            "array": ("[]\n", "JSONL value must be an object"),
            "null": ("null\n", "JSONL value must be an object"),
            "string": ('"not an object"\n', "JSONL value must be an object"),
        }
        with tempfile.TemporaryDirectory() as d:
            for name, (contents, expected) in cases.items():
                with self.subTest(name=name):
                    src = os.path.join(d, f"{name}.jsonl")
                    with open(src, "w") as target:
                        target.write(contents)
                    errors = self._metadata_errors(src)
                    self.assertTrue(
                        any(expected in error for error in errors),
                        errors,
                    )

    def test_empty_jsonl_is_an_explicit_gate_error(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "empty.jsonl")
            with open(src, "w") as target:
                target.write("\n  \n")
            errors = self._metadata_errors(src)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("empty JSONL has no metadata or records", errors[0])

    def test_duplicate_and_symlink_inputs_cannot_double_the_sample(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            alias = os.path.join(d, "alias.jsonl")
            self._write_clean_backend(src, "file", 6, 384.0)
            os.symlink(src, alias)
            for paths in ([src, src], [src, alias]):
                with self.subTest(paths=paths):
                    datasets = reqanalyze.load(paths)
                    errors = [
                        error
                        for dataset in datasets
                        for error in dataset["metadata_errors"]
                    ]
                    self.assertTrue(
                        any("duplicate input" in error for error in errors),
                        errors,
                    )
                    valid_records = sum(
                        len(dataset["records"])
                        for dataset in datasets
                        if not dataset["metadata_errors"]
                    )
                    self.assertEqual(valid_records, 32)

    def test_analysis_output_cannot_alias_an_input(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            self._write_clean_backend(src, "file", 6, 384.0)
            before = reqbench.sha256_file(src)
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                rc = self._run_gate_fixture(["--json-out", src, src])
            self.assertEqual(rc, 5)
            self.assertEqual(reqbench.sha256_file(src), before)
            self.assertIn("aliases protected artifact", stdout.getvalue())

    def test_analysis_output_cannot_alias_the_analyzer_source(self):
        from unittest import mock

        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            analyzer_copy = os.path.join(d, "reqanalyze-copy.py")
            self._write_clean_backend(src, "file", 6, 384.0)
            shutil.copyfile(reqanalyze.__file__, analyzer_copy)
            before = reqbench.sha256_file(analyzer_copy)
            stdout = io.StringIO()
            with (
                mock.patch.object(reqanalyze, "__file__", analyzer_copy),
                mock.patch.dict(
                    os.environ,
                    {reqanalyze.ANALYZER_SOURCE_PATH_ENV: analyzer_copy},
                ),
                redirect_stdout(stdout),
            ):
                rc = self._run_gate_fixture(
                    ["--json-out", analyzer_copy, src]
                )
            self.assertEqual(rc, 5)
            self.assertEqual(reqbench.sha256_file(analyzer_copy), before)
            self.assertIn("aliases protected artifact", stdout.getvalue())

    def test_inherited_sealed_fd_must_be_the_executing_analyzer(self):
        import fcntl
        import hashlib

        decoy = b"print('this is not reqanalyze')\n"
        fd = os.memfd_create("reqanalyze-decoy", flags=os.MFD_ALLOW_SEALING)
        try:
            os.write(fd, decoy)
            fcntl.fcntl(
                fd,
                fcntl.F_ADD_SEALS,
                fcntl.F_SEAL_SEAL
                | fcntl.F_SEAL_SHRINK
                | fcntl.F_SEAL_GROW
                | fcntl.F_SEAL_WRITE,
            )
            env = dict(os.environ)
            env[reqanalyze.SEALED_ANALYZER_FD_ENV] = str(fd)
            env["REQANALYZE_EXPECTED_SHA256"] = hashlib.sha256(decoy).hexdigest()
            result = subprocess.run(
                [sys.executable, reqanalyze.__file__],
                env=env,
                pass_fds=(fd,),
                capture_output=True,
                text=True,
                timeout=30,
            )
        finally:
            os.close(fd)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "sealed analyzer descriptor does not identify the executing source",
            result.stderr,
        )

    def test_failed_warmup_still_gates_availability_and_teardown(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "analysis.json")
            self._write_clean_backend(src, "file", 6, 384.0)

            def fail_warmup(record):
                record.update({
                    "ok": False,
                    "error": "warmup transport failure",
                    "failure_class": "transport",
                    "render": {
                        "ok": False,
                        "error": "warmup transport failure",
                        "failure_class": "transport",
                        "stage": "navigate",
                    },
                })
                record["teardown"].update({
                    "all_gone": False,
                    "disk_cleanup_verified": False,
                    "survivors": {"firecracker": 1234},
                })

            failed = self._mutate_record(
                src, "cdp", fail_warmup, warmup=True
            )
            self.assertIs(failed["warmup"], True)
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as result_file:
                result = json.load(result_file)
            cdp = result["arms"]["cdp"]
            self.assertEqual(cdp["attempted"], 8)
            self.assertEqual(cdp["failed"], 1)
            self.assertEqual(cdp["all_gone_confirmed"], [7, 8])
            self.assertEqual(cdp["disk_cleanup_confirmed"], [7, 8])
            self.assertIs(result["gate"]["availability"]["passed"], False)
            self.assertIs(result["gate"]["teardown"]["passed"], False)
            self.assertEqual(rc, 5)

    def test_wide_drift_ci_crossing_zero_fails_equivalence(self):
        from unittest import mock

        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "analysis.json")
            self._write_clean_backend(src, "file", 6, 384.0)
            with (
                mock.patch.object(reqanalyze, "median_ci", self._fast_median_ci),
                mock.patch.object(
                    reqanalyze,
                    "hodges_lehmann_shift",
                    return_value=(0.0, -25.0, 25.0),
                ),
                redirect_stdout(io.StringIO()),
            ):
                rc = reqanalyze.main_with(["--json-out", dst, src])
            with open(dst) as result_file:
                result = json.load(result_file)
            drift = result["gate"]["baseline_drift"]
            self.assertEqual(drift["ci"], [-25.0, 25.0])
            self.assertIs(drift["significant"], False)
            self.assertIs(drift["passed"], False)
            self.assertIn("baseline drift", " ".join(result["gate"]["reasons"]))
            self.assertEqual(rc, 5)

    def test_arm_cross_field_contradictions_are_rejected(self):
        cases = (
            (
                "wall-before-response", "noop",
                lambda record: record.__setitem__(
                    "wall_ms", record["blocking_ms"] - 1
                ),
                "wall_ms < blocking_ms",
            ),
            (
                "exec-timeout", "exec",
                lambda record: record.__setitem__("timed_out", True),
                "successful exec did not exit cleanly",
            ),
            (
                "cdp-host", "cdp",
                lambda record: record["render"].__setitem__(
                    "cdp_host", "127.0.0.3:9222"
                ),
                "CDP render cdp_host",
            ),
            (
                "teardown-mode", "cdp-fast",
                lambda record: record["teardown"].__setitem__("mode", "normal"),
                "expected 'fast'",
            ),
        )
        with tempfile.TemporaryDirectory() as d:
            for name, arm, mutation, expected in cases:
                with self.subTest(name=name):
                    src = os.path.join(d, f"{name}.jsonl")
                    self._write_clean_backend(src, "file", 6, 384.0)
                    self.assertEqual(self._metadata_errors(src), [])
                    self._mutate_record(src, arm, mutation)
                    errors = self._metadata_errors(src)
                    self.assertTrue(
                        any(expected in error for error in errors),
                        errors,
                    )

    def test_invalid_optional_teardown_numerics_are_rejected(self):
        cases = (
            (
                "boolean-signal", "cdp",
                lambda record: record["teardown"].__setitem__("signal_ms", True),
                "invalid signal_ms=True",
            ),
            (
                "string-excess", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "machine_cpu_ms_excess", "1.0"
                ),
                "invalid machine_cpu_ms_excess='1.0'",
            ),
            (
                "negative-child-cpu", "cdp-fast",
                lambda record: record["teardown"]["per_child_cpu"]["fcvm"].__setitem__(
                    "cpu_before_ms", -1.0
                ),
                "invalid cpu_before_ms=-1.0",
            ),
            (
                "negative-control-rate", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "control_busy_cores", -0.1
                ),
                "invalid control_busy_cores=-0.1",
            ),
            (
                "missing-control-raw", "cdp-fast",
                lambda record: record["teardown"].pop("control_cpu_ms_raw"),
                "no finite control_cpu_ms_raw",
            ),
            (
                "contradictory-control-net", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "control_cpu_ms_net", 5.0
                ),
                "control_cpu_ms_net=5.0 does not match derived",
            ),
            (
                "contradictory-control-bound", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "control_busy_cores_hi", 0.01
                ),
                "control_busy_cores_hi=0.01 does not match derived",
            ),
            (
                "contradictory-clamp", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "machine_cpu_ms_subtraction_clamped", True
                ),
                "clamp classification contradicts",
            ),
            (
                "stale-accounting-version", "cdp-fast",
                lambda record: record["teardown"].__setitem__(
                    "accounting_version", "post-terminal-ambient-v1"
                ),
                "stale accounting semantics",
            ),
        )
        with tempfile.TemporaryDirectory() as d:
            for name, arm, mutation, expected in cases:
                with self.subTest(name=name):
                    src = os.path.join(d, f"{name}.jsonl")
                    self._write_clean_backend(src, "file", 6, 384.0)
                    self._mutate_record(src, arm, mutation)
                    errors = self._metadata_errors(src)
                    self.assertTrue(
                        any(expected in error for error in errors),
                        errors,
                    )

    def test_in_resolution_negative_control_is_transparent_and_valid(self):
        def clamp_control(record):
            teardown = record["teardown"]
            uncertainty = teardown["cpu_residual_uncertainty_ms"]
            teardown["control_machine_cpu_ms"] = 0.0
            teardown["control_harness_cpu_ms"] = 10.0
            teardown["control_cpu_ms_raw"] = -10.0
            teardown["control_cpu_ms_net"] = 0.0
            teardown["control_cpu_ms_net_lo"] = 0.0
            teardown["control_cpu_ms_net_hi"] = uncertainty - 10.0
            teardown["control_cpu_ms_subtraction_clamped"] = True
            teardown["control_busy_cores"] = 0.0
            teardown["control_busy_cores_lo"] = 0.0
            teardown["control_busy_cores_hi"] = (
                (uncertainty - 10.0) / teardown["control_wall_ms"]
            )
            machine_window = teardown["machine_cpu_window_ms"]
            teardown["machine_cpu_ms_excess"] = teardown["machine_cpu_ms_net"]
            teardown["machine_cpu_ms_excess_lo"] = (
                teardown["machine_cpu_ms_net_lo"]
                - teardown["control_busy_cores_hi"] * machine_window
            )
            teardown["machine_cpu_ms_excess_hi"] = (
                teardown["machine_cpu_ms_net_hi"]
            )

        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "clamped-control.jsonl")
            self._write_clean_backend(src, "file", 6, 384.0)
            self._mutate_record(src, "cdp-fast", clamp_control)
            self.assertEqual(self._metadata_errors(src), [])

    def test_accounting_resolution_and_window_are_bound_to_raw_evidence(self):
        def recompute(teardown):
            uncertainty = 6 * teardown["machine_cpu_resolution_ms"] + 2 * (
                teardown["harness_cpu_resolution_ms"]
            )
            teardown["cpu_residual_uncertainty_ms"] = uncertainty
            for prefix, machine_field, harness_field in (
                ("machine_cpu_ms", "machine_cpu_ms", "harness_cpu_ms"),
                (
                    "control_cpu_ms",
                    "control_machine_cpu_ms",
                    "control_harness_cpu_ms",
                ),
            ):
                raw = teardown[machine_field] - teardown[harness_field]
                teardown[f"{prefix}_raw"] = raw
                teardown[f"{prefix}_net"] = max(0.0, raw)
                teardown[f"{prefix}_net_lo"] = max(0.0, raw - uncertainty)
                teardown[f"{prefix}_net_hi"] = max(0.0, raw + uncertainty)
                teardown[f"{prefix}_subtraction_clamped"] = raw < 0.0
            control_wall = teardown["control_wall_ms"]
            teardown["control_busy_cores"] = (
                teardown["control_cpu_ms_net"] / control_wall
            )
            teardown["control_busy_cores_lo"] = (
                teardown["control_cpu_ms_net_lo"] / control_wall
            )
            teardown["control_busy_cores_hi"] = (
                teardown["control_cpu_ms_net_hi"] / control_wall
            )
            machine_window = teardown["machine_cpu_window_ms"]
            teardown["machine_cpu_ms_excess"] = (
                teardown["machine_cpu_ms_net"]
                - teardown["control_busy_cores"] * machine_window
            )
            teardown["machine_cpu_ms_excess_lo"] = (
                teardown["machine_cpu_ms_net_lo"]
                - teardown["control_busy_cores_hi"] * machine_window
            )
            teardown["machine_cpu_ms_excess_hi"] = (
                teardown["machine_cpu_ms_net_hi"]
                - teardown["control_busy_cores_lo"] * machine_window
            )

        def forge_narrow_resolution(record):
            teardown = record["teardown"]
            teardown["machine_cpu_resolution_ms"] = 0.001
            teardown["harness_cpu_resolution_ms"] = 0.001
            recompute(teardown)

        def forge_short_machine_window(record):
            teardown = record["teardown"]
            teardown["machine_cpu_window_ms"] = teardown["reap_wall_ms"] / 2
            recompute(teardown)

        def forge_short_control_window(record):
            teardown = record["teardown"]
            teardown["control_wall_ms"] = teardown["control_target_ms"] / 2
            recompute(teardown)

        cases = (
            (
                "forged-resolution",
                forge_narrow_resolution,
                "machine_cpu_resolution_ms=0.001 does not match derived 10.0",
            ),
            (
                "short-machine-window",
                forge_short_machine_window,
                "machine CPU window does not enclose reap_wall_ms",
            ),
            (
                "short-control-window",
                forge_short_control_window,
                "control window ended before its declared target",
            ),
        )
        with tempfile.TemporaryDirectory() as d:
            for name, mutation, expected in cases:
                with self.subTest(name=name):
                    src = os.path.join(d, f"{name}.jsonl")
                    self._write_clean_backend(src, "file", 6, 384.0)
                    self._mutate_record(src, "cdp-fast", mutation)
                    errors = self._metadata_errors(src)
                    self.assertTrue(
                        any(expected in error for error in errors), errors
                    )

    def test_busy_host_evidence_and_allow_busy_override_gate_publication(self):
        cases = (
            ("start-load", {"loadavg": ["2.1", "0.1", "0.1"]}, "host loadavg1"),
            ("guard-load", {"quiet_guard_loadavg1": 2.1}, "guard-time loadavg1"),
            ("allow-busy", {"allow_busy": True}, "used ALLOW_BUSY"),
            (
                "bad-limit-type",
                {"quiet_loadavg1_limit": "2.0"},
                "no valid quiet_loadavg1_limit",
            ),
        )
        with tempfile.TemporaryDirectory() as d:
            for name, overrides, expected in cases:
                with self.subTest(name=name):
                    src = os.path.join(d, f"{name}.jsonl")
                    self._write_clean_backend(
                        src, "file", 6, 384.0, **overrides
                    )
                    errors = self._metadata_errors(src)
                    self.assertTrue(
                        any(expected in error for error in errors),
                        errors,
                    )

    def test_port_host_ip_must_be_canonical(self):
        with tempfile.TemporaryDirectory() as d:
            canonical = os.path.join(d, "canonical.jsonl")
            noncanonical = os.path.join(d, "noncanonical.jsonl")
            self._write_clean_backend(
                canonical,
                "file",
                6,
                384.0,
                port_mappings=[{
                    "host_ip": "127.0.0.1",
                    "host_port": 9222,
                    "guest_port": 9222,
                    "proto": "tcp",
                }],
            )
            self.assertEqual(self._metadata_errors(canonical), [])
            cell = reqanalyze.load([canonical])[0]["cell"]
            self.assertEqual(cell["port_mappings"][0]["host_ip"], "127.0.0.1")

            self._write_clean_backend(
                noncanonical,
                "file",
                6,
                384.0,
                port_mappings=[{
                    "host_ip": "2001:0db8::1",
                    "host_port": 9222,
                    "guest_port": 9222,
                    "proto": "tcp",
                }],
            )
            errors = self._metadata_errors(noncanonical)
            self.assertTrue(
                any("no valid port_mappings" in error for error in errors),
                errors,
            )

    def test_failed_records_are_counted_per_arm_and_gate_publication(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with(["--json-out", dst, src])
            with open(dst) as result_file:
                out = json.load(result_file)
            self.assertEqual(out["arms"]["cdp"]["attempted"], 8)
            self.assertEqual(out["arms"]["cdp"]["failed"], 3)
            self.assertEqual(out["arms"]["exec"]["failed"], 0)
            self.assertIn("failure_rate_ci", out["arms"]["cdp"])
            self.assertIs(out["arms"]["cdp"]["publishable"], False)
            self.assertIs(out["arms"]["exec"]["publishable"], True)
            self.assertIn("DO NOT PUBLISH", buf.getvalue())

    def test_a_leak_on_a_FAILED_request_is_still_reported(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with(["--json-out", dst, src])
            with open(dst) as result_file:
                out = json.load(result_file)
            self.assertEqual(
                out["arms"]["cdp"]["all_gone_confirmed"], [5, 8],
                "the 3 failed reps' teardowns must be examined, not filtered out",
            )
            self.assertIn("NOT CONFIRMED GONE", buf.getvalue())

    def test_the_stage_header_does_not_claim_forward_localhost(self):
        """AGENTS.md: `--forward-localhost` is GUEST->HOST and cannot carry this path.

        RED BEFORE THE FIX: the CDP stage table printed under
            CDP ARMS: per-request stage decomposition (host -> clone over forward-localhost)
        The harness never passes `--forward-localhost` — reqbench.sh publishes
        port 9222 and fc-agent DNATs it from guest eth0 to guest loopback.
        a55d25d4 claimed to have fixed the direction
        "in the right direction" everywhere; `git show` proves it never touched
        this line, which was then the only place in bench/chromium still asserting
        the wrong one.
        """
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with([src, "--no-gate"])
            text = buf.getvalue()
            self.assertNotIn("forward-localhost", text)
            self.assertIn("publish", text)
            self.assertIn("guest loopback", text)

    def test_a_null_all_gone_is_not_counted_as_confirmed(self):
        """`ag.count(False)` treats null and absent as CONFIRMED GONE.

        RED BEFORE THE FIX, reproduced by running the branch analyzer on 27
        `all_gone: true` + 2 `all_gone: null` + 1 with the key absent:
            all_gone: 27/30 confirmed
        No warning, no per-rep dump, and `--json-out` said `[27, 30]` — the
        numerator and denominator disagreeing by three while the report reads
        clean. a55d25d4 EDITED these exact lines and reinforced the False-only
        test rather than fixing it.
        """
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 6, 384.0)
            with open(src) as source:
                rows = [json.loads(line) for line in source]
            targets = [
                row for row in rows
                if row.get("arm") == "cdp" and row.get("warmup") is False
            ][:3]
            targets[0]["teardown"]["all_gone"] = None
            targets[1]["teardown"]["all_gone"] = None
            targets[2]["teardown"].pop("all_gone")
            with open(src, "w") as target:
                for row in rows:
                    target.write(json.dumps(row) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = self._run_gate_fixture(["--json-out", dst, src, "--no-gate"])
            text = buf.getvalue()
            self.assertIn("** 3 NOT CONFIRMED GONE **", text)
            for row in targets:
                rep = row["rep"]
                self.assertIn(f"rep {rep}", text, f"rep {rep} must appear in the dump")
            with open(dst) as f:
                out = json.load(f)
            self.assertEqual(out["arms"]["cdp"]["all_gone_confirmed"], [5, 8])
            self.assertEqual(out["arms"]["cdp"]["all_gone_no_evidence"], 3)
            self.assertEqual(rc, 0, "--no-gate was passed")

    def test_a_transport_drop_is_named_in_the_failure_breakdown(self):
        """A record whose diagnostic is only NESTED must still be named.

        RED BEFORE THE FIX, run against the shipped analyzer on the shape
        `run_cdp_request` actually emitted (error under `render`):
            FAILURE x3: rc=None
        The WsClosed message, the stage and the failure_class never reached the
        operator, and the existing test passed only because its fixture put
        `error` at the TOP level — a shape the producer did not write.
        """
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                self._run_gate_fixture(["--json-out", dst, src, "--no-gate"])
            text = buf.getvalue()
            self.assertIn("WsClosed", text)
            self.assertNotIn("rc=None", text)
            with open(dst) as f:
                out = json.load(f)
            self.assertEqual(out["arms"]["cdp"]["failure_classes"], {"transport": 3})

    def test_failure_class_is_consumed_not_just_written(self):
        """RED BEFORE THE FIX: `git grep failure_class` over the whole branch
        returned exactly ONE hit — the line in cdpdrive.py that writes it. The
        comment above it said downstream could gate on it instead of
        substring-matching the message; nothing downstream read it."""
        with open(os.path.join(HERE, "reqanalyze.py")) as f:
            self.assertIn("failure_class", f.read(),
                          "the analyzer must CONSUME the classification, not just "
                          "let cdpdrive write it into the void")

    def test_an_unpublishable_arm_makes_the_analyzer_exit_nonzero(self):
        """RED BEFORE THE FIX: measured EXIT CODE 0 at 10% cdp censoring with
        ** DO NOT PUBLISH THIS ARM'S LATENCY ** on stdout. A harness that exits 0
        on a run it has itself marked DO NOT PUBLISH will have those numbers
        quoted by someone who only checked the exit code."""
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            self._write_clean_backend(src, "file", 200, 384.0)
            with open(src) as source:
                rows = [json.loads(line) for line in source]
            for row in rows:
                if row.get("arm") == "cdp" and row.get("warmup") is False:
                    row["ok"] = False
                    row["error"] = "WsClosed: connection closed mid-frame"
                    row["failure_class"] = "transport"
                    row["render"] = {
                        "ok": False,
                        "error": row["error"],
                        "failure_class": "transport",
                        "stage": "navigate",
                    }
                    break
            with open(src, "w") as target:
                for row in rows:
                    target.write(json.dumps(row) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = self._run_gate_fixture([src])
            self.assertIn("DO NOT PUBLISH", buf.getvalue())
            self.assertNotEqual(rc, 0, "the run must gate, not merely narrate")

    def test_199_measured_cdp_attempts_fail_the_backend_gate(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 199, 384.0)
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as f:
                out = json.load(f)
            sample = out["gate"]["cdp_sample_size"]
            self.assertEqual(sample["measured_non_warmup_attempts_per_arm"]["cdp"], 199)
            self.assertIs(sample["passed"], False)
            self.assertIs(out["publishable"], False)
            self.assertIn("199/200", " ".join(out["gate"]["reasons"]))
            self.assertEqual(rc, 5, buf.getvalue())

    def test_200_measured_cdp_attempts_pass_the_backend_gate(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 200, 384.0)
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as f:
                out = json.load(f)
            self.assertEqual(
                out["gate"]["cdp_sample_size"]
                ["measured_non_warmup_attempts_per_arm"]["cdp"],
                200,
            )
            self.assertIs(out["publishable"], True, out["gate"])
            self.assertIs(out["gate"]["passed"], True)
            self.assertEqual(rc, 0, buf.getvalue())

    def test_two_backend_inputs_are_analyzed_without_pooling(self):
        with tempfile.TemporaryDirectory() as d:
            file_src = os.path.join(d, "file.jsonl")
            uffd_src = os.path.join(d, "uffd.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(file_src, "file", 200, 100.0)
            self._write_clean_backend(uffd_src, "uffd", 200, 900.0)
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(
                    ["--json-out", dst, file_src, uffd_src]
                )
            with open(dst) as f:
                out = json.load(f)
            self.assertEqual(set(out["backends"]), {"file", "uffd"})
            self.assertEqual(out["backends"]["file"]["sources"], [file_src])
            self.assertEqual(out["backends"]["uffd"]["sources"], [uffd_src])
            for backend in ("file", "uffd"):
                count = out["backends"][backend]["gate"]["cdp_sample_size"][
                    "measured_non_warmup_attempts_per_arm"
                ]["cdp"]
                self.assertEqual(count, 200, f"{backend} was pooled with the other backend")
            self.assertEqual(out["backends"]["file"]["arms"]["cdp"]
                             ["blocking_ms"]["median"], 100.0)
            self.assertEqual(out["backends"]["uffd"]["arms"]["cdp"]
                             ["blocking_ms"]["median"], 900.0)
            self.assertIs(out["publishable"], True)
            self.assertEqual(rc, 0)

    def test_detected_drift_fails_even_when_no_gate_overrides_exit_status(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(
                src, "uffd", 200, 384.0,
                noop_values=(10.0,) * 100 + (100.0,) * 100,
            )
            with redirect_stdout(io.StringIO()):
                gated_rc = self._run_gate_fixture(["--json-out", dst, src])
            self.assertEqual(gated_rc, 5)

            with redirect_stdout(io.StringIO()):
                override_rc = self._run_gate_fixture(
                    ["--json-out", dst, "--no-gate", src]
                )
            with open(dst) as f:
                out = json.load(f)
            self.assertIs(out["gate"]["baseline_drift"]["significant"], True)
            self.assertIs(out["gate"]["baseline_drift"]["passed"], False)
            self.assertIn("baseline drift", " ".join(out["gate"]["reasons"]))
            self.assertIs(out["publishable"], False)
            self.assertIs(out["gate"]["passed"], False)
            self.assertIs(out["gate"]["exit_code_overridden"], True)
            self.assertEqual(override_rc, 0)

    def test_missing_noop_control_fails_publication(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 200, 384.0)
            with open(src) as f:
                rows = [json.loads(line) for line in f]
            with open(src, "w") as f:
                for row in rows:
                    if row.get("arm") != "noop":
                        f.write(json.dumps(row) + "\n")
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as f:
                out = json.load(f)
            self.assertIs(out["gate"]["baseline_drift"]["evaluated"], False)
            self.assertIs(out["gate"]["baseline_drift"]["passed"], False)
            self.assertIn("0/6", " ".join(out["gate"]["reasons"]))
            self.assertIs(out["publishable"], False)
            self.assertEqual(rc, 5)

    def test_recreated_tag_with_same_legacy_identity_never_pools(self):
        """The exact generation and config digest, not tag/time/VM, define a cell."""
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            first = os.path.join(d, "first.jsonl")
            second = os.path.join(d, "second.jsonl")
            self._write_clean_backend(
                first, "file", 100, 100.0,
                snapshot="same-reused-tag",
                snapshot_created_at="2026-08-09T00:00:00Z",
                snapshot_vm_id="vm-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                snapshot_generation_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                snapshot_config_sha256="1" * 64,
            )
            self._write_clean_backend(
                second, "file", 100, 900.0,
                snapshot="same-reused-tag",
                snapshot_created_at="2026-08-09T00:00:00Z",
                snapshot_vm_id="vm-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                snapshot_generation_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                snapshot_config_sha256="2" * 64,
            )
            with open(src, "wb") as out, open(first, "rb") as a, open(second, "rb") as b:
                out.write(a.read())
                out.write(b.read())
            dst = os.path.join(d, "r.json")
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as f:
                result = json.load(f)
            self.assertEqual(len(result["backends"]), 2)
            counts = sorted(
                cell["gate"]["cdp_sample_size"]
                ["measured_non_warmup_attempts_per_arm"]["cdp"]
                for cell in result["backends"].values()
            )
            self.assertEqual(counts, [100, 100])
            self.assertTrue(all(key.startswith("file:") for key in result["backends"]))
            self.assertIs(result["publishable"], False)
            self.assertEqual(rc, 5)

    def test_same_backend_files_with_different_quality_never_pool(self):
        with tempfile.TemporaryDirectory() as d:
            a = os.path.join(d, "a.jsonl")
            b = os.path.join(d, "b.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(a, "uffd", 100, 100.0, quality=75)
            self._write_clean_backend(b, "uffd", 100, 900.0, quality=90)
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(["--json-out", dst, a, b])
            with open(dst) as f:
                result = json.load(f)
            self.assertEqual(len(result["backends"]), 2)
            self.assertEqual(
                {cell["cell"]["quality"] for cell in result["backends"].values()},
                {75, 90},
            )
            self.assertIs(result["publishable"], False)
            self.assertEqual(rc, 5)

    def test_metric_provenance_names_record_and_governing_meta_lines(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 200, 384.0)
            with redirect_stdout(io.StringIO()):
                rc = self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as f:
                result = json.load(f)
            refs = result["arms"]["cdp"]["blocking_ms"]["provenance"]
            self.assertEqual(refs[0]["path"], src)
            self.assertEqual(refs[0]["meta_line"], 1)
            with open(src) as source:
                expected_lines = [
                    line_no
                    for line_no, line in enumerate(source, 1)
                    if line_no > 1
                    and (record := json.loads(line)).get("arm") == "cdp"
                    and record.get("warmup") is False
                ]
            self.assertEqual(
                refs[0]["record_lines"],
                reqanalyze._line_ranges(expected_lines),
            )
            self.assertEqual(refs[0]["n"], 200)
            self.assertEqual(refs[0]["sha256"], reqbench.sha256_file(src))
            self.assertEqual(rc, 0)

    def test_record_before_metadata_is_rejected_with_its_line(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            with open(src, "w") as f:
                f.write(json.dumps({"arm": "cdp", "rep": 0, "ok": True}) + "\n")
            datasets = reqanalyze.load([src])
            self.assertEqual(len(datasets), 1)
            self.assertIn(f"{src}:1", datasets[0]["metadata_errors"][0])
            self.assertEqual(datasets[0]["records"][0]["_source"]["meta_line"], None)

    def test_per_child_cpu_is_reported_by_name_not_pooled(self):
        """Pooling across children medians a straggler away.

        RED BEFORE THE FIX: reqanalyze appended every child's value to one list and
        discarded the name, so {firecracker 110 ms, holder 0 ms, pasta 0 ms}
        published a median of 0 — the opposite of the finding.
        """
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._write_clean_backend(src, "file", 6, 372.0)
            with open(src) as source:
                rows = [json.loads(line) for line in source]
            for record in rows:
                if record.get("arm") != "cdp-fast":
                    continue
                record["teardown"]["tick_ms"] = 10.0
                record["teardown"]["per_child_cpu"] = {
                    "fcvm": {
                        "reclaim_cpu_ms": 0.0, "complete": True,
                        "below_resolution": True, "reclaim_cpu_ms_hi": 20.0,
                    },
                    "firecracker": {
                        "reclaim_cpu_ms": 110.0, "complete": True,
                        "below_resolution": False, "reclaim_cpu_ms_hi": 130.0,
                    },
                    "sleep": {
                        "reclaim_cpu_ms": 0.0, "complete": True,
                        "below_resolution": True, "reclaim_cpu_ms_hi": 20.0,
                    },
                    "pasta": {
                        "reclaim_cpu_ms": 0.0, "complete": True,
                        "below_resolution": True, "reclaim_cpu_ms_hi": 20.0,
                    },
                }
            with open(src, "w") as target:
                for row in rows:
                    target.write(json.dumps(row) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                self._run_gate_fixture(["--json-out", dst, src])
            with open(dst) as result_file:
                out = json.load(result_file)
            per_child = out["arms"]["cdp-fast"]["reclaim_cpu_ms_by_child"]
            self.assertIn("firecracker", per_child)
            self.assertIn("pasta", per_child)
            self.assertEqual(per_child["firecracker"]["median"], 110.0)
            text = buf.getvalue()
            self.assertIn("firecracker", text)
            self.assertNotIn(
                "0.00 [0.00, 0.00] ms", text,
                "a sub-tick child must print as a bound, never as an exact zero",
            )
            self.assertIn("below /proc tick resolution", text)


class TeardownProbeGuards(unittest.TestCase):
    """The probe imported `reap_disk` but not the RULE that governs it.

    `reqbench.py`'s `teardown_fast` refuses to reap the state file and data dir of
    a VM whose Firecracker is still running, SIGKILLs the survivors and aborts.
    The probe computed the identical per-child evidence (`poll_gone` returns None
    for a child still alive at 30 s), printed it as TIMEOUT, discarded it, and
    reaped anyway — then returned 0.
    """

    def test_a_survivor_blocks_the_reap_and_is_killed(self):
        """RED BEFORE THE FIX: the `finally` inspected only fcvm (`if proc.poll()
        is None:` — already -9 by then, so the branch never ran) and called
        `reap_disk` unconditionally at the end. Both artifacts were deleted, the
        survivor stayed in /proc, and main() returned 0."""
        import teardown_probe

        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-11111111111111111111111111111111.json")
            data = os.path.join(d, "vm-disks", "vm-11111111111111111111111111111111")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-11111111111111111111111111111111"}, f)
            os.makedirs(data)
            p = subprocess.Popen(["bash", "-c", "sleep 300 & wait"])
            kids = wait_for_child(p.pid)
            try:
                named = {f"{reqbench.proc_comm(k)}:{k}": k for k in kids}
                row = {"rep": 0, "gone_ms": {n: None for n in named}}
                leaked = teardown_probe.reap_rep(row, named, state, data)
                self.assertTrue(leaked)
                self.assertTrue(os.path.exists(state),
                                "the state file is the only record this VM is ours")
                self.assertTrue(os.path.isdir(data),
                                "the data dir holds a reflink of the golden rootfs")
                self.assertTrue(row["disk_reap_skipped"])
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if all(reqbench.proc_stat_fields(k) is None
                           or reqbench.proc_stat_fields(k)[0] in ("Z", "X", "x")
                           for k in kids):
                        break
                    time.sleep(0.01)
                alive = [k for k in kids
                         if reqbench.proc_stat_fields(k) is not None
                         and reqbench.proc_stat_fields(k)[0] not in ("Z", "X", "x")]
                self.assertEqual(alive, [], "a survivor must be SIGKILLed, not left")
            finally:
                kill_tree(p)

    def test_a_clean_rep_is_still_reaped(self):
        import teardown_probe

        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-11111111111111111111111111111111.json")
            data = os.path.join(d, "vm-disks", "vm-11111111111111111111111111111111")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-11111111111111111111111111111111"}, f)
            os.makedirs(data)
            row = {"rep": 0, "gone_ms": {"firecracker:1": 12.0}}
            leaked = teardown_probe.reap_rep(row, {"firecracker:1": 1}, state, data)
            self.assertFalse(leaked)
            self.assertFalse(os.path.exists(state))
            self.assertFalse(os.path.isdir(data))

    def test_summarize_reports_the_censoring_rate_not_only_the_survivors(self):
        """RED BEFORE THE FIX: `n=` was `len(vs2)` — the observed exits only — and
        the timeout count and the denominator were never printed. Worse, there was
        no `else`, so a child that TIMED OUT in EVERY rep printed NOTHING AT ALL:
        the straggler the probe exists to find is the one that vanished from its
        own summary."""
        import teardown_probe

        rows = [
            {"rep": 0, "gone_ms": {"firecracker:1": 12.0, "pasta:2": None}},
            {"rep": 1, "gone_ms": {"firecracker:3": 14.0, "pasta:4": None}},
            {"rep": 2, "gone_ms": {}, "error": "no state file within 120.0s"},
        ]
        buf = io.StringIO()
        with redirect_stdout(buf):
            censored = teardown_probe.summarize(rows)
        text = buf.getvalue()
        self.assertIn("n=2/2", text)                 # firecracker: both observed
        self.assertIn("censored=2", text)            # pasta: both timed out
        self.assertIn("pasta", text, "an all-timeout child must still get a row")
        self.assertIn("NO EXIT OBSERVED", text)
        self.assertIn("reps attempted=3", text)
        self.assertIn("errored=1", text)
        self.assertNotEqual(censored, 0, "a TIMEOUT is a leak, not a missing sample")

    def test_an_exception_mid_rep_does_not_lose_every_other_row(self):
        """RED BEFORE THE FIX: `wait_port` raising TimeoutError (or
        `clone_cdp_endpoint` raising RuntimeError) left main() through a bare
        try/finally, so `json.dump(rows, f)` never ran and reps 0..i-1 were lost.
        reqbench.py's main() deliberately does the opposite."""
        import teardown_probe

        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            os.makedirs(os.path.join(d, "vm-disks", "vm-22222222222222222222222222222222"))
            stub = os.path.join(d, "fcvm-stub")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"cat > {state_dir}/vm-$$.json <<EOF\n"
                    '{"vm_id": "vm-22222222222222222222222222222222", "pid": $$, '
                    '"config": {"network": {"loopback_ip": "127.0.0.1"}}}\n'
                    "EOF\n"
                    "exec sleep 600\n"
                )
            os.chmod(stub, 0o755)
            out_json = os.path.join(d, "probe.json")
            real = teardown_probe.wait_port

            def boom(*_a, **_k):
                raise TimeoutError("CDP port never answered")

            teardown_probe.wait_port = boom
            argv = sys.argv
            sys.argv = ["teardown_probe.py", "--serve-pid", "1", "--n", "2",
                        "--fcvm", stub, "--data-root", d, "--out", out_json,
                        "--state-timeout", "5"]
            try:
                buf = io.StringIO()
                with redirect_stdout(buf):
                    teardown_probe.main()
            finally:
                teardown_probe.wait_port = real
                sys.argv = argv
            self.assertTrue(os.path.exists(out_json), "no artifact was written at all")
            with open(out_json) as f:
                rows = json.load(f)
            self.assertEqual(len(rows), 2, f"reps were lost: {rows}")
            self.assertTrue(all("error" in r for r in rows), rows)


class BackendIsExplicit(unittest.TestCase):
    """A run must name exactly one memory backend.

    RED BEFORE THE FIX: with neither flag, `clone_backend_args` returned
    `["--pid", "0"]` and the meta record still said `"backend": "uffd"`; with
    both, the tag silently won while the metadata was decided by the same
    expression. Two different backends with different per-request costs, mixed
    without saying so — AGENTS.md defect 1.
    """

    def _main(self, extra):
        argv = sys.argv
        sys.argv = ["reqbench.py", "--url", "http://x/", "--out-dir", "/tmp"] + extra
        try:
            with self.assertRaises(SystemExit) as cm:
                with redirect_stdout(io.StringIO()):
                    reqbench.main()
            return cm.exception.code
        finally:
            sys.argv = argv

    def test_neither_backend_is_rejected(self):
        self.assertEqual(self._main([]), 2)

    def test_both_backends_are_rejected(self):
        self.assertEqual(self._main(["--serve-pid", "7", "--snapshot-tag", "t"]), 2)


class ReqbenchShell(unittest.TestCase):
    """`reqbench.sh` drives the run; three defects live in it.

    Driven with stubs on PATH so no VM, no sudo and no podman are involved.
    """

    SH = os.path.join(HERE, "reqbench.sh")
    RUN_ID = "0" * 32

    def _env(self, d, **extra):
        binx = os.path.join(d, "bin")
        os.makedirs(binx, exist_ok=True)
        fcvm = os.path.join(d, "fcvm")
        fc_agent = os.path.join(d, "fc-agent")
        self._write(fcvm, "#!/bin/bash\nexit 0\n")
        self._write(fc_agent, "#!/bin/bash\nexit 0\n")
        env = dict(os.environ)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            RESULTS=os.path.join(d, "results"),
            STATE_DIR=os.path.join(d, "state"),
            ALLOW_BUSY="1",
            RUNID=self.RUN_ID,
            FCVM=fcvm,
            FC_AGENT=fc_agent,
        )
        env.update(extra)
        os.makedirs(env["STATE_DIR"], exist_ok=True)
        return env, binx

    def _write(self, path, body):
        with open(path, "w") as f:
            f.write(body)
        os.chmod(path, 0o755)

    def _read_if_exists(self, path, default=""):
        if not os.path.exists(path):
            return default
        with open(path) as f:
            return f.read()

    def _write_fake_fcvm(self, d):
        fcvm = os.path.join(d, "fcvm")
        self._write(fcvm, "#!/bin/bash\nexit 0\n")
        self._write(os.path.join(d, "fc-agent"), "#!/bin/bash\nexit 0\n")
        return fcvm

    def test_concurrent_identical_runtime_install_is_atomic(self):
        """Two stagers publish one verified bundle and leave no temp tree.

        The cp shim is a barrier at the last copied input. Both invocations have
        therefore completed their private staging directories before either can
        acquire the install lock; this exercises the existing-bundle side of the
        race deterministically rather than relying on scheduler timing.
        """
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d)
            barrier = os.path.join(d, "copy-barrier")
            os.makedirs(barrier)
            self._write(os.path.join(binx, "cp"), r'''#!/bin/bash
destination="${!#}"
case "$destination" in
  */.stage.*/fcvm)
    touch "$COPY_BARRIER/$$"
    deadline=$((SECONDS + 10))
    while [ "$(find "$COPY_BARRIER" -maxdepth 1 -type f | wc -l)" -lt 2 ]; do
      [ "$SECONDS" -lt "$deadline" ] || exit 90
      sleep 0.01
    done
    ;;
esac
exec /usr/bin/cp "$@"
''')
            env.update(FCVM=self._write_fake_fcvm(d), COPY_BARRIER=barrier)
            first_env = dict(env, TAG="stage-race-a")
            second_env = dict(env, TAG="stage-race-b")
            first = subprocess.Popen(
                [self.SH, "not-a-command"], env=first_env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            second = subprocess.Popen(
                [self.SH, "not-a-command"], env=second_env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            try:
                first_out, first_err = first.communicate(timeout=30)
                second_out, second_err = second.communicate(timeout=30)
            finally:
                for process in (first, second):
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=5)

            self.assertEqual(first.returncode, 2, first_out + first_err)
            self.assertEqual(second.returncode, 2, second_out + second_err)
            runtime = os.path.join(env["RESULTS"], "runtime")
            stage_dirs = [
                os.path.join(root, name)
                for root, dirs, _files in os.walk(runtime)
                for name in dirs
                if name.startswith(".stage.")
            ]
            self.assertEqual(stage_dirs, [], f"staging directories leaked: {stage_dirs}")
            bundles = [
                entry.path for entry in os.scandir(runtime)
                if entry.is_dir(follow_symlinks=False)
            ]
            self.assertEqual(len(bundles), 1, f"runtime entries: {os.listdir(runtime)}")
            verified = subprocess.run(
                ["sha256sum", "--check", "--status", "MANIFEST.sha256"],
                cwd=bundles[0], capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(verified.returncode, 0, verified.stderr)
            with open(os.path.join(bundles[0], "MANIFEST.sha256")) as f:
                manifest_names = [line.split()[-1] for line in f]
            self.assertIn("fc-agent", manifest_names)
            self.assertTrue(os.access(os.path.join(bundles[0], "fc-agent"), os.X_OK))

    def test_generated_run_id_survives_staged_reexec(self):
        """The outer shell generates the ID once and explicitly passes it in."""
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d)
            env.pop("RUNID")
            capture = os.path.join(d, "staged-env.txt")
            self._write(os.path.join(binx, "bash"), f'''#!/bin/bash
printf '%s\n%s\n' "$RUNID" "$REQBENCH_RUNTIME_BUNDLE" > {capture}
exec /bin/bash "$@"
''')
            env["FCVM"] = self._write_fake_fcvm(d)
            result = subprocess.run(
                [self.SH, "not-a-command"], env=env,
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 2, result.stderr)
            run_id, bundle = self._read_if_exists(capture).splitlines()
            self.assertEqual(len(run_id), 32, run_id)
            self.assertTrue(all(c in "0123456789abcdef" for c in run_id), run_id)
            self.assertTrue(os.path.samefile(bundle, os.path.join(
                env["RESULTS"], "runtime", os.path.basename(bundle),
            )))

    def test_golden_delegates_the_cold_build_to_prepare(self):
        """golden's cold-boot guarantee now lives inside `podman prepare`.

        The previous flow exported FCVM_NO_SNAPSHOT=1 around a hand-rolled
        `podman run`, and a sudo env_reset once silently dropped the assignment,
        turning "cold boot" into a restore from a stale cached snapshot. prepare
        forces the cold build internally (src/commands/podman/mod.rs sets
        no_snapshot before any cache lookup), so the knob must be GONE from the
        invocation: its reappearance would mean someone reintroduced the
        env-sensitive dance this rework deleted. Run under an env_reset sudo to
        hold the original failure conditions in place.
        """
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d, REQBENCH_STAGED="1")
            seen = os.path.join(d, "seen.txt")
            # Record the knob at the sudo boundary, BEFORE env -i wipes it: the
            # fake fcvm's own recording can only ever read <unset> behind the
            # reset, so it would keep this test green even if the harness
            # regressed to `FCVM_NO_SNAPSHOT=1 $SUDO ...`.
            self._write(os.path.join(binx, "sudo"),
                        f'#!/bin/bash\n'
                        f'echo "sudo-saw FCVM_NO_SNAPSHOT=${{FCVM_NO_SNAPSHOT:-<unset>}}" >> {seen}\n'
                        f'exec env -i PATH="$PATH" HOME="$HOME" "$@"\n')
            self._write(os.path.join(binx, "podman"), '''#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{"Digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]'
    exit 0
fi
exit 1
''')
            fcvm = os.path.join(d, "fcvm")
            self._write(fcvm, f"""#!/bin/bash
if [ "$1 $2" = "podman prepare" ]; then
    echo "argv=$* FCVM_NO_SNAPSHOT=${{FCVM_NO_SNAPSHOT:-<unset>}}" >> {seen}
    exit 1  # stop before the provenance step; the invocation is the evidence
fi
exit 1
""")
            subprocess.run([self.SH, "golden"], env=dict(env, SUDO="sudo", FCVM=fcvm),
                           capture_output=True, text=True, timeout=120)
            got = self._read_if_exists(seen, "<no invocation>")
            self.assertNotEqual(
                got, "<no invocation>",
                "golden never invoked `fcvm podman prepare`, so this test observed nothing")
            self.assertIn("--tag cb-req-golden", got)
            self.assertIn("--force", got)
            self.assertIn("FCVM_NO_SNAPSHOT=<unset>", got,
                          f"the retired cold-boot env knob is back: {got}")
            self.assertIn("sudo-saw FCVM_NO_SNAPSHOT=<unset>", got,
                          f"the harness handed the retired knob to sudo: {got}")

    def test_golden_fails_when_prepare_fails(self):
        """A failed prepare is attributed immediately, not after a poll timeout."""
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d, REQBENCH_STAGED="1")
            fcvm = os.path.join(d, "fcvm")
            self._write(fcvm, "#!/bin/bash\nexit 42\n")
            self._write(os.path.join(binx, "podman"), '''#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{"Digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]'
    exit 0
fi
exit 1
''')
            result = subprocess.run(
                [self.SH, "golden"], env=dict(env, FCVM=fcvm),
                capture_output=True, text=True, timeout=10,
            )
            self.assertNotEqual(result.returncode, 0, result.stderr)
            self.assertIn("golden: PREPARE FAILED", result.stderr)

    def _golden_image_identity_fixture(self, d, snapshot_cache_key):
        env, binx = self._env(d, DATA_ROOT=d)
        argv = os.path.join(d, "fcvm-argv.log")
        digest = "a" * 64
        image_id = "b" * 64
        fcvm = os.path.join(d, "fcvm")
        # The prepare stub installs what the real one installs: the snapshot
        # generation directory with its config.json, under the tag's lock.
        self._write(fcvm, f'''#!/bin/bash
echo "$*" >> {argv}
case "$1 $2" in
  "podman prepare")
      mkdir -p "$DATA_ROOT/snapshots/$TAG"
      : > "$DATA_ROOT/snapshots/$TAG.lock"
      cat > "$DATA_ROOT/snapshots/$TAG/config.json" <<'EOF'
{{"generation_id":"12345678-1234-4234-8234-123456789abc","created_at":"2026-08-09T00:00:00Z","vm_id":"vm-11111111111111111111111111111111","metadata":{{"image":"localhost/chromium-bench-req","image_disk_path":"/image-cache/{snapshot_cache_key}.storage-v2.img"}}}}
EOF
      # Real prepare reports the generation it installed on stdout; the harness
      # binds its provenance record to exactly that generation.
      digest=$(sha256sum "$DATA_ROOT/snapshots/$TAG/config.json" | cut -d" " -f1)
      printf '{{"status":"prepared","generation_id":"%s","config_digest":"%s"}}\n' \
          "${{GENERATION_OVERRIDE:-12345678-1234-4234-8234-123456789abc}}" "$digest"
      ;;
esac
''')
        self._write(os.path.join(d, "fc-agent"), "#!/bin/bash\nexit 0\n")
        self._write(os.path.join(binx, "podman"), f'''#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{{"Digest":"sha256:{digest}","Id":"{image_id}"}}]'
    exit 0
fi
exit 1
''')
        result = subprocess.run(
            [self.SH, "golden"], env=dict(env, FCVM=fcvm),
            capture_output=True, text=True, timeout=60,
        )
        return result, self._read_if_exists(argv), digest, image_id

    def test_golden_launches_local_tag_and_commits_exact_content_identity(self):
        """fcvm needs the local tag to attach its content-addressed image disk."""
        with tempfile.TemporaryDirectory() as d:
            result, argv, digest, image_id = self._golden_image_identity_fixture(
                d, "a" * 64,
            )
            self.assertEqual(result.returncode, 0, result.stderr[-1600:])
            prepare_line = next(
                line for line in argv.splitlines() if line.startswith("podman prepare ")
            )
            self.assertTrue(
                prepare_line.endswith(" localhost/chromium-bench-req"), prepare_line,
            )
            self.assertIn("--tag cb-req-golden", prepare_line)
            self.assertIn("--force", prepare_line)
            self.assertNotIn(image_id, prepare_line)
            with open(os.path.join(
                d, "snapshots", "cb-req-golden", "reqbench-provenance.json",
            )) as source:
                provenance = json.load(source)
            self.assertEqual(provenance["image"], "localhost/chromium-bench-req")
            self.assertEqual(provenance["image_id"], "sha256:" + image_id)
            self.assertEqual(provenance["image_digest"], "sha256:" + digest)
            self.assertEqual(provenance["image_cache_key"], digest)
            self.assertEqual(
                provenance["snapshot_generation_id"],
                "12345678-1234-4234-8234-123456789abc",
            )
            config_path = os.path.join(
                d, "snapshots", "cb-req-golden", "config.json",
            )
            with open(config_path, "rb") as source:
                self.assertEqual(
                    provenance["snapshot_config_sha256"],
                    hashlib.sha256(source.read()).hexdigest(),
                )

    def test_golden_rejects_a_generation_installed_by_someone_else(self):
        """Provenance must name the generation prepare reported, not the tag's.

        Any other fcvm command can replace the tag between prepare exiting and
        the provenance write. A replacement carrying the same image passes every
        content check, so without this binding the record would stamp another
        process's snapshot with this run's creator hashes and source revision.
        """
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d, DATA_ROOT=d)
            fcvm = os.path.join(d, "fcvm")
            digest = "a" * 64
            image_id = "b" * 64
            self._write(os.path.join(binx, "podman"), f'''#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{{"Digest":"sha256:{digest}","Id":"{image_id}"}}]'
    exit 0
fi
exit 1
''')
            self._write(fcvm, '''#!/bin/bash
case "$1 $2" in
  "podman prepare")
      mkdir -p "$DATA_ROOT/snapshots/$TAG"
      : > "$DATA_ROOT/snapshots/$TAG.lock"
      cat > "$DATA_ROOT/snapshots/$TAG/config.json" <<'EOF'
{"generation_id":"12345678-1234-4234-8234-123456789abc","created_at":"2026-08-09T00:00:00Z","vm_id":"vm-11111111111111111111111111111111","metadata":{"image":"localhost/chromium-bench-req","image_disk_path":"/image-cache/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.storage-v2.img"}}
EOF
      digest=$(sha256sum "$DATA_ROOT/snapshots/$TAG/config.json" | cut -d" " -f1)
      # prepare installed one generation; the tag now holds a different one.
      printf '{"status":"prepared","generation_id":"%s","config_digest":"%s"}\n' \
          "99999999-9999-4999-8999-999999999999" "$digest"
      ;;
esac
''')
            self._write(os.path.join(d, "fc-agent"), "#!/bin/bash\nexit 0\n")
            result = subprocess.run(
                [self.SH, "golden"], env=dict(env, FCVM=fcvm),
                capture_output=True, text=True, timeout=60,
            )
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("is not the one prepare installed", result.stderr)

    def test_a_non_debug_log_level_is_refused_before_anything_runs(self):
        """Measuring at info drops the records every phase reads, silently.

        The lookalike values are the reviewer-caught false accepts: a substring
        test lets `notfcvm=debug` (a different target) and `fcvm=debugging` (an
        invalid level tracing ignores) through a gate whose whole job is
        refusing configurations that produce no fcvm debug records.
        """
        for bad in ("fcvm=info", "notfcvm=debug", "fcvm=debugging"):
            with self.subTest(fcvm_log=bad), tempfile.TemporaryDirectory() as d:
                env, _ = self._env(d, FCVM_LOG=bad)
                marker = os.path.join(d, "fcvm-was-invoked")
                fcvm = os.path.join(d, "fcvm")
                self._write(fcvm, f"#!/bin/bash\ntouch {marker}\n")
                result = subprocess.run(
                    [self.SH, "golden"], env=dict(env, FCVM=fcvm),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("must select fcvm=debug", result.stderr)
                self.assertFalse(
                    os.path.exists(marker),
                    "the harness ran fcvm before refusing the log level",
                )

    def test_log_directives_that_enable_fcvm_debug_are_accepted(self):
        """The exact-directive gate must not refuse values that DO work."""
        for good in ("fcvm=debug", "fcvm=trace", "warn,fcvm=debug", "fcvm=debug,hyper=warn",
                     "warn, fcvm=debug"):
            with self.subTest(fcvm_log=good), tempfile.TemporaryDirectory() as d:
                env, binx = self._env(d, FCVM_LOG=good, REQBENCH_STAGED="1")
                self._write(os.path.join(binx, "podman"), """#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{"Digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]'
    exit 0
fi
exit 1
""")
                marker = os.path.join(d, "fcvm-was-invoked")
                fcvm = os.path.join(d, "fcvm")
                # An invoked fcvm is the proof the gate was passed; exiting
                # non-zero right after keeps the run short.
                self._write(fcvm, f"#!/bin/bash\ntouch {marker}\nexit 42\n")
                result = subprocess.run(
                    [self.SH, "golden"], env=dict(env, FCVM=fcvm),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertNotIn("must select fcvm=debug", result.stderr)
                self.assertTrue(
                    os.path.exists(marker),
                    f"the gate refused a working directive: {result.stderr}",
                )

    def test_golden_rejects_a_tag_repointed_before_fcvm_resolved_it(self):
        """The snapshot disk key must match the harness's atomic image inspect."""
        with tempfile.TemporaryDirectory() as d:
            result, _argv, digest, _image_id = self._golden_image_identity_fixture(
                d, "c" * 64,
            )
            self.assertNotEqual(result.returncode, 0, result.stderr)
            self.assertIn("the image tag changed during golden creation", result.stderr)
            self.assertIn(digest, result.stderr)

    def _run_stub(self, d, backend, analyzer_rc=0, sudo_env_reset=False):
        env, binx = self._env(d, BACKEND=backend, REPS="1", WARMUP="0")
        provenance_dir = os.path.join(d, "snapshots", "cb-req-golden")
        os.makedirs(provenance_dir, exist_ok=True)
        with open(os.path.join(provenance_dir, "reqbench-provenance.json"), "w") as f:
            json.dump({"image_id": "sha256:" + "1" * 64}, f)
        # Every real installed snapshot carries config.json; without it the
        # hugepage guard reads state "unknown" and (correctly) refuses
        # BACKEND=file fail-closed rather than risking a mislabeled record.
        with open(os.path.join(provenance_dir, "config.json"), "w") as f:
            json.dump({"metadata": {"hugepages": False}}, f)
        argv = os.path.join(d, "argv.log")
        pyargv = os.path.join(d, "pyargv.log")
        driver_env = os.path.join(d, "driver-env.log")
        fcvm = os.path.join(d, "fcvm")
        self._write(fcvm, f"""#!/bin/bash
echo "$@" >> {argv}
if [ "$1 $2" = "snapshot serve" ]; then
    echo "Serve PID: $$"; echo "Waiting for VMs"; exec sleep 30
fi
""")
        self._write(os.path.join(binx, "python3"),
                    f'''#!/bin/bash
echo "$@" >> {pyargv}
case "$1" in
  *reqbench.py)
      printf '%s\n%s\n' "${{REQBENCH_RUNTIME_BUNDLE:-<unset>}}" \
          "${{REQBENCH_SOURCE_REVISION:-<unset>}}" > {driver_env}
      ;;
  *reqanalyze.py) exit {analyzer_rc} ;;
esac
exit 0
''')
        self._write(os.path.join(binx, "podman"),
                    '#!/bin/bash\necho sha256:test-image\n')
        sudo = ""
        if sudo_env_reset:
            sudo = "sudo"
            self._write(os.path.join(binx, "sudo"),
                        '#!/bin/bash\nexec env -i PATH="$PATH" HOME="$HOME" "$@"\n')
        r = subprocess.run([self.SH, "run"], env=dict(env, FCVM=fcvm, SUDO=sudo),
                           capture_output=True, text=True, timeout=180)
        return (r,
                self._read_if_exists(argv),
                self._read_if_exists(pyargv))

    def test_run_provenance_survives_sudo_env_reset(self):
        with tempfile.TemporaryDirectory() as d:
            r, _argv, pyargv = self._run_stub(d, "file", sudo_env_reset=True)
            self.assertEqual(r.returncode, 0, f"{pyargv}\n{r.stderr[-1200:]}")
            runtime_bundle, source_revision = self._read_if_exists(
                os.path.join(d, "driver-env.log")
            ).splitlines()
            self.assertNotEqual(runtime_bundle, "<unset>")
            self.assertTrue(os.path.samefile(runtime_bundle, os.path.join(
                d, "results", "runtime", os.path.basename(runtime_bundle),
            )))
            expected_revision = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=HERE, check=True,
                capture_output=True, text=True,
            ).stdout.strip()
            self.assertEqual(source_revision, expected_revision)

    def test_live_non_child_is_retained_after_term_and_kill(self):
        """A failed exact-owner stop is an error, never a successful untrack."""
        with tempfile.TemporaryDirectory() as d:
            sleeper = subprocess.Popen(["sleep", "300"])
            kill_log = os.path.join(d, "kills.log")
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
mock_sudo() {{
    if [ "${{1:-}}" = kill ]; then
        printf '%s\n' "$*" >> "$KILL_LOG"
        return 0
    fi
    "$@"
}}
SUDO=mock_sudo
sleep() {{ SECONDS=$((SECONDS + 20)); }}
track "$TARGET_PID"
set +e
stop_tracked "$TARGET_PID" 0
rc=$?
set -e
tracked=no
tracked_entry "$TARGET_PID" >/dev/null && tracked=yes
live=no
process_matches "$(tracked_entry "$TARGET_PID")" && live=yes
printf 'rc=%s tracked=%s live=%s\n' "$rc" "$tracked" "$live"
[ "$rc" -eq 2 ] && [ "$tracked" = yes ] && [ "$live" = yes ]
'''
            try:
                env, _binx = self._env(
                    d, TARGET_PID=str(sleeper.pid), KILL_LOG=kill_log,
                )
                result = subprocess.run(
                    ["bash", "-c", script], env=env,
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), "rc=2 tracked=yes live=yes")
                signals = self._read_if_exists(kill_log).splitlines()
                self.assertEqual(signals, [
                    f"kill -TERM {sleeper.pid}",
                    f"kill -KILL {sleeper.pid}",
                ])
            finally:
                sleeper.kill()
                sleeper.wait(timeout=5)

    def test_empty_cleanup_has_no_synthetic_process(self):
        """An empty tracked-process array must stay empty under set -u."""
        with tempfile.TemporaryDirectory() as d:
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
CLEANUP_PIDS=()
set +e
cleanup
rc=$?
set -e
printf 'rc=%s count=%s\n' "$rc" "${{#CLEANUP_PIDS[@]}}"
'''
            env, _binx = self._env(d)
            result = subprocess.run(
                ["bash", "-c", script], env=env,
                capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "rc=0 count=0")
            self.assertNotIn("survived SIGKILL", result.stderr)

    def test_track_replaces_a_reused_pid_identity_before_stopping_it(self):
        """A stale starttime must not make the new process invisible to cleanup."""
        with tempfile.TemporaryDirectory() as d:
            identity = os.path.join(d, "identity")
            kill_log = os.path.join(d, "kills.log")
            with open(identity, "w") as f:
                f.write("111\n")
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
process_identity() {{ cat "$IDENTITY_FILE"; }}
mock_sudo() {{
    printf '%s\n' "$*" >> "$KILL_LOG"
    printf '333\n' > "$IDENTITY_FILE"
}}
SUDO=mock_sudo
track 424242
printf '222\n' > "$IDENTITY_FILE"
track 424242
before=$(tracked_entry 424242)
count=${{#CLEANUP_PIDS[@]}}
stop_tracked 424242 0
if tracked_entry 424242 >/dev/null; then after=present; else after=gone; fi
printf 'before=%s count=%s after=%s\n' "$before" "$count" "$after"
'''
            env, _binx = self._env(
                d, IDENTITY_FILE=identity, KILL_LOG=kill_log,
            )
            result = subprocess.run(
                ["bash", "-c", script],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.strip(), "before=424242:222 count=1 after=gone"
            )
            self.assertEqual(
                self._read_if_exists(kill_log).splitlines(),
                ["kill -TERM 424242"],
            )

    def test_backend_file_runs_without_a_uffd_serve(self):
        """RED BEFORE THE FIX: `cmd_run` hardcoded `snapshot serve` + `--serve-pid`
        and `grep -n snapshot-tag reqbench.sh` had no hit, so the FILE-backed arm
        that `reqbench.py` fully supports (`--snapshot-tag` -> `--snapshot <name>`)
        had NO PATH THROUGH THIS DRIVER. REVIEW.md's re-run gate is ">=200 CDP
        requests PER BACKEND", and that re-run was not runnable."""
        with tempfile.TemporaryDirectory() as d:
            r, argv, pyargv = self._run_stub(d, "file")
            self.assertNotIn("snapshot serve", argv,
                             f"a UFFD serve was started for a FILE-backed run:\n{argv}")
            self.assertIn("--snapshot-tag", pyargv, f"{pyargv}\n{r.stderr[-800:]}")
            self.assertNotIn("--serve-pid", pyargv, pyargv)

    def test_backend_uffd_is_still_the_default_and_serves(self):
        with tempfile.TemporaryDirectory() as d:
            r, argv, pyargv = self._run_stub(d, "uffd")
            self.assertIn("snapshot serve", argv, f"{argv}\n{r.stderr[-800:]}")
            self.assertIn("--serve-pid", pyargv, pyargv)
            self.assertNotIn("--snapshot-tag", pyargv, pyargv)

    def test_analyzer_rejection_fails_the_driver(self):
        with tempfile.TemporaryDirectory() as d:
            r, _argv, pyargv = self._run_stub(d, "file", analyzer_rc=5)
            self.assertEqual(r.returncode, 5, f"stdout={r.stdout}\nstderr={r.stderr}")
            self.assertIn("reqbench.py", pyargv, pyargv)
            self.assertIn("reqanalyze.py", pyargv, pyargv)
            self.assertIn("gated run exit 5", r.stderr)

    def test_vm_process_count_uses_comm_prefixes_and_ignores_zombies(self):
        with tempfile.TemporaryDirectory() as d:
            fixture = os.path.join(d, "ps.txt")
            with open(fixture, "w") as f:
                f.write(
                    "S fcvm\n"
                    "S firecracker-def\n"
                    "S cloud-hypervis\n"
                    "Z fcvm\n"
                    "S codex\n"
                    "S tmux: server\n"
                )
            env, binx = self._env(d, PS_FIXTURE=fixture)
            self._write(os.path.join(binx, "ps"), '#!/bin/bash\ncat "$PS_FIXTURE"\n')
            r = subprocess.run(
                ["bash", "-c", f"source {self.SH}; vm_process_count"],
                env=env, capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "3", r.stdout)

    def test_busy_guard_returns_three_before_starting_measurements(self):
        with tempfile.TemporaryDirectory() as d:
            fixture = os.path.join(d, "ps.txt")
            loadavg = os.path.join(d, "loadavg")
            marker = os.path.join(d, "python-called")
            with open(fixture, "w") as f:
                f.write("S fcvm\n")
            with open(loadavg, "w") as f:
                f.write("0.01 0.01 0.01 1/1 1\n")
            env, binx = self._env(
                d, ALLOW_BUSY="0", PS_FIXTURE=fixture, LOADAVG_FILE=loadavg,
            )
            self._write(os.path.join(binx, "ps"), '#!/bin/bash\ncat "$PS_FIXTURE"\n')
            self._write(os.path.join(binx, "python3"),
                        f'#!/bin/bash\ntouch {marker}\n')
            r = subprocess.run(
                [self.SH, "run"], env=env, capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(r.returncode, 3, f"stdout={r.stdout}\nstderr={r.stderr}")
            self.assertFalse(os.path.exists(marker), "the measurement driver ran after refusal")
            self.assertIn("FATAL: no measurements were taken", r.stderr)

    def test_quiet_guard_treats_integer_and_decimal_limit_equally(self):
        with tempfile.TemporaryDirectory() as d:
            fixture = os.path.join(d, "ps.txt")
            loadavg = os.path.join(d, "loadavg")
            open(fixture, "w").close()
            env, binx = self._env(
                d, ALLOW_BUSY="0", PS_FIXTURE=fixture, LOADAVG_FILE=loadavg,
            )
            self._write(os.path.join(binx, "ps"), '#!/bin/bash\ncat "$PS_FIXTURE"\n')
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
for load in 2 2.0; do
    printf '%s 0 0 1/1 1\n' "$load" > "$LOADAVG_FILE"
    if guard_quiet; then
        printf '%s=quiet\n' "$load"
    else
        printf '%s=busy:%s\n' "$load" "$?"
        exit 1
    fi
done
printf '2.1 0 0 1/1 1\n' > "$LOADAVG_FILE"
if guard_quiet; then
    printf '2.1=quiet\n'
else
    printf '2.1=busy:%s\n' "$?"
fi
for load in 2.0001 3; do
    printf '%s 0 0 1/1 1\n' "$load" > "$LOADAVG_FILE"
    if guard_quiet; then
        printf '%s=quiet\n' "$load"
    else
        printf '%s=busy:%s\n' "$load" "$?"
    fi
done
'''
            result = subprocess.run(
                ["bash", "-c", script], env=env,
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines(), [
                "2=quiet", "2.0=quiet", "2.1=busy:3",
                "2.0001=busy:3", "3=busy:3",
            ])

    def _settle_env(self, d, **extra):
        """A busy loadavg fixture plus an empty ps fixture, for guard tests."""
        fixture = os.path.join(d, "ps.txt")
        loadavg = os.path.join(d, "loadavg")
        open(fixture, "w").close()
        with open(loadavg, "w") as f:
            f.write("9.99 0 0 1/1 1\n")
        env, binx = self._env(
            d, ALLOW_BUSY="0", PS_FIXTURE=fixture, LOADAVG_FILE=loadavg,
            **extra,
        )
        self._write(os.path.join(binx, "ps"), '#!/bin/bash\ncat "$PS_FIXTURE"\n')
        return env

    def test_quiet_guard_settles_within_the_opt_in_window(self):
        """SETTLE_WAIT_SECS turns one busy sample into a bounded re-sample loop.

        The one-shot chain (cmd_all) reaches the run gate seconds after its own
        build, golden and verify phases, so the 1-minute load average it reads
        still carries that prerequisite work. Without the window a cold
        `make bench-chromium-request-all` refuses because of its own wake, and
        a retry repeats the phony prerequisites.
        """
        with tempfile.TemporaryDirectory() as d:
            env = self._settle_env(d, SETTLE_WAIT_SECS="30")
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
( sleep 2; printf '0.10 0 0 1/1 1\n' > "$LOADAVG_FILE" ) &
helper=$!
if guard_quiet; then
    echo settled
else
    echo "refused:$?"
fi
wait "$helper"
'''
            result = subprocess.run(
                ["bash", "-c", script], env=env,
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.strip(), "settled", result.stderr[-800:],
            )

    def test_quiet_guard_still_refuses_when_the_settle_window_elapses(self):
        """A box that never goes quiet is refused, with the wait named."""
        with tempfile.TemporaryDirectory() as d:
            env = self._settle_env(d, SETTLE_WAIT_SECS="1")
            script = f'''
source {self.SH!r}
trap - EXIT INT TERM
if guard_quiet; then
    echo quiet
else
    echo "refused:$?"
fi
'''
            result = subprocess.run(
                ["bash", "-c", script], env=env,
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "refused:3")
            self.assertIn("still busy after 1s settle wait", result.stderr)

    def test_all_gives_its_own_chain_a_settle_default(self):
        """cmd_all runs the gate right after its own prerequisite phases."""
        for preset, expected in ((None, "120"), ("7", "7")):
            with self.subTest(preset=preset), tempfile.TemporaryDirectory() as d:
                extra = {"REQBENCH_STAGED": "1", "DATA_ROOT": d}
                if preset is not None:
                    extra["SETTLE_WAIT_SECS"] = preset
                env, binx = self._env(d, **extra)
                capture = os.path.join(d, "settle.txt")
                # cmd_build's podman invocation is the first child the chain
                # runs; failing there keeps the test to the dispatch line.
                self._write(os.path.join(binx, "podman"), f'''#!/bin/bash
echo "settle=${{SETTLE_WAIT_SECS:-<unset>}}" >> {capture}
exit 1
''')
                result = subprocess.run(
                    [self.SH, "all"], env=env,
                    capture_output=True, text=True, timeout=60,
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(
                    f"settle={expected}",
                    self._read_if_exists(capture, "<no podman invocation>"),
                )

    def test_target_id_waits_for_readiness_and_skips_devtools_pages(self):
        """RED BEFORE THE FIX: `target_id` was a SINGLE-SHOT urlopen with
        `2>/dev/null || true`, and `start_clone` returns as soon as the state file
        carries a pid — it never waits for the CDP port. Clone 1 is warm because
        HOP A/B/C ran against it; clone 2 is queried the instant it registers, so
        connection-refused yields an empty id and the documented stability gate
        fails on a RACE. It also took the first `type == "page"`, while the driver
        that consumes the id skips `devtools://` pages — so the two could compare
        different targets."""
        with tempfile.TemporaryDirectory() as d:
            with _JsonListServer(lambda t: ([] if t < 0.6 else [
                {"type": "page", "id": "DEVTOOLS", "url": "devtools://devtools/x.html"},
                PAGE_TARGET,
            ])) as s:
                script = (
                    f'source {self.SH}\n'
                    f'target_id "{s.endpoint}"\n'
                )
                env, _ = self._env(d)
                r = subprocess.run(["bash", "-c", script], env=env,
                                   capture_output=True, text=True, timeout=120)
            self.assertEqual(r.stdout.strip(), "ABC123",
                             f"stdout={r.stdout!r} stderr={r.stderr[-800:]}")


class HostCdpQuietGate(unittest.TestCase):
    """hostcdp.sh shares reqbench's SETTLE_WAIT_SECS quiet-gate knob.

    The Makefile runs the host baseline right after `build`, so its tighter
    1.0 gate is even easier for the chain to trip on its own wake.
    """

    SH = os.path.join(HERE, "hostcdp.sh")

    def test_the_gate_settles_with_the_same_knob(self):
        with tempfile.TemporaryDirectory() as d:
            binx = os.path.join(d, "bin")
            os.makedirs(binx)
            loadavg = os.path.join(d, "loadavg")
            with open(loadavg, "w") as f:
                f.write("9.99 0 0 1/1 1\n")
            stubs = {
                # No firecrackers; the load fixture is the only busy signal.
                "pgrep": "#!/bin/bash\necho 0\nexit 1\n",
                # Failing the container start right after the gate keeps the
                # test to the gate itself; exit 7 is distinguishable from the
                # gate's refusal exit 3.
                "podman": "#!/bin/bash\nexit 7\n",
            }
            for name, body in stubs.items():
                path = os.path.join(binx, name)
                with open(path, "w") as f:
                    f.write(body)
                os.chmod(path, 0o755)
            env = dict(os.environ)
            env.update(
                PATH=binx + os.pathsep + env["PATH"],
                LOADAVG_FILE=loadavg,
                SETTLE_WAIT_SECS="30",
                ALLOW_BUSY="0",
                RESULTS=os.path.join(d, "results"),
            )
            helper = subprocess.Popen(
                ["bash", "-c",
                 f'sleep 2; printf "0.10 0 0 1/1 1\\n" > {loadavg}'],
            )
            try:
                result = subprocess.run(
                    ["bash", self.SH], env=env,
                    capture_output=True, text=True, timeout=60,
                )
            finally:
                helper.wait(timeout=10)
            self.assertIn("settling", result.stderr)
            self.assertIn("9.99", result.stderr)
            self.assertNotIn("REFUSING", result.stderr)
            self.assertNotEqual(result.returncode, 3, result.stderr)


class SustainedRateUncertainty(unittest.TestCase):
    """The throughput HEADLINE must carry uncertainty, like everything else here.

    RED BEFORE THE FIX: `analyse_throughput`'s sustained block emitted
    `achieved_rps` as a bare `len(done)/dur` quotient — no `rate_ci` key existed
    anywhere in corrected.json — and the only `boot_ci` call in that block was
    spent on latency, while the BURST path immediately above it does bootstrap its
    rate (it can: bursts are replicated 5x). `analyze.py` also had no binomial
    function at all (`grep`: zero hits), so "462/462 completed" was quoted bare
    sixteen lines after the paragraph that forbids exactly that.
    """

    def test_sustained_reports_a_rate_interval_and_a_binomial_bound(self):
        import analyze

        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "samples"))
            t0 = 1000.0
            dur = 60.0
            with open(os.path.join(d, "samples", "bursts.jsonl"), "w") as f:
                f.write(json.dumps({"phase": "sustained-meta", "cell": "file-4k",
                                    "rate": 8, "launched": 100, "skipped": 0,
                                    "t0": t0, "t1": t0 + dur}) + "\n")
            recs = [{"phase": "sust-r8", "arm": "file-4k", "ok": True,
                     "total_ms": 600.0, "t0_ts": t0 + i * (dur / 99)}
                    for i in range(98)]
            out: dict = {}
            analyze.analyse_throughput(d, recs, out)
            v = out["sustained"]["file-4k/target=8rps"]
            self.assertIsNotNone(v["rate_ci"], "the rate must carry an interval")
            self.assertEqual(len(v["rate_ci"]), 2)
            self.assertIn("sub-window", v["rate_ci_basis"])
            lo, hi = v["incomplete_rate_ci"]
            self.assertGreater(hi, 0.0, "2/100 incomplete is not a 0% failure rate")
            exp = reqanalyze.clopper_pearson(2, 100)
            self.assertAlmostEqual(lo, exp[0], places=9)
            self.assertAlmostEqual(hi, exp[1], places=9)


class DocLint(unittest.TestCase):
    """The docs ARE the deliverable, so their numbers get the same gate as the code.

    AGENTS.md's own Deliverables rule 3: "Every figure traceable to a raw record —
    cite the json file (and the cell)." Nothing enforced that, which is how two
    rounds of review walked past a CP bound the repo's own function contradicts
    and a refuted percentage still published as a finding.
    """

    def _read(self, name):
        with open(os.path.join(HERE, name)) as f:
            return f.read()

    def _corrected(self):
        with open(os.path.join(HERE, "results", "20260808-corrected", "corrected.json")) as f:
            return json.load(f)

    def test_every_binomial_bound_matches_reqanalyze_clopper_pearson(self):
        """RED BEFORE THE FIX: REVIEW.md L32 said "At 0/200 the CP upper bound is
        1.5%". `reqanalyze.clopper_pearson(0, 200)` gives [0.000%, 1.828%]; 1.5% is
        the ONE-sided bound (1 - 0.05**(1/200) = 1.487%) while L48 declares the
        convention as "Clopper-Pearson, 95%, two-sided". Six of the file's seven
        bounds reproduce exactly — which is itself the proof the convention is
        two-sided. reqanalyze.py's own docstring carried a second one: "0/426 is
        [0, 0.70%]" against 0.862% computed by the function 80 lines below it.
        """
        import re

        bad = []
        for name in ("REVIEW.md", "reqanalyze.py"):
            flat = re.sub(r"\s+", " ", self._read(name))
            claims = [
                (int(m.group(1)), int(m.group(2)), m.group(3), m.group(4), m.group(0))
                for m in re.finditer(
                    r"(\d+)/(\d+)\b[^\[\]]{0,80}?\[\s*([\d.]+)\s*%?\s*,\s*([\d.]+)\s*%\s*\]",
                    flat)
            ]
            claims += [
                (int(m.group(1)), int(m.group(2)), None, m.group(3), m.group(0))
                for m in re.finditer(
                    r"(\d+)/(\d+)\s+the CP upper bound is\s+([\d.]+)\s*%", flat)
            ]
            self.assertTrue(claims, f"{name}: the lint matched nothing — it is vacuous")
            for k, n, q_lo, q_hi, raw in claims:
                lo, hi = reqanalyze.clopper_pearson(k, n)
                for quoted, computed in ((q_lo, 100 * lo), (q_hi, 100 * hi)):
                    if quoted is None:
                        continue
                    dec = len(quoted.split(".")[1]) if "." in quoted else 0
                    tol = 0.5 * 10 ** -dec + 1e-9
                    if abs(float(quoted) - computed) > tol:
                        bad.append(f"{name}: {raw!r} -> {k}/{n} is "
                                   f"[{100 * lo:.3f}%, {100 * hi:.3f}%], "
                                   f"quoted {quoted}%")
        self.assertEqual(bad, [], "\n".join(bad))

    def test_agents_md_jpeg_figures_match_the_record_run(self):
        """RED BEFORE THE FIX: AGENTS.md L95 published "JPEG q80 measured −40%
        screenshot, −21% whole request in-VM" while REVIEW.md marks exactly that
        claim **REFUTED AS STATED** and corrected.json says screenshot_ms pct
        −28.81 and artifact_ms pct −8.34. Both numbers on that line are
        contradicted by the record run, and the AGENTS.md REFUTED list — whose own
        preamble says it "now points at [REVIEW.md] rather than contradicting it"
        — omitted the claim entirely.
        """
        import re

        text = self._read("AGENTS.md")
        m = re.search(r"JPEG q80 measured\s+(.+?)\n-", text, re.S)
        self.assertIsNotNone(m, "the JPEG bullet moved; update this lint")
        # Only the CLAIM, not the marked history. A refuted number quoted behind
        # "used to read" IS the refutation and must stay; an unmarked one is the
        # defect. That distinction is the whole point of the AGENTS.md/REVIEW.md
        # reconciliation.
        seg = m.group(1).split("used to read")[0]
        self.assertIn("corrected.json", m.group(1), "the figures must cite their record")
        pcts = {round(float(x), 1) for x in re.findall(r"[-−]([\d.]+)%", seg)}
        sf = self._corrected()["screenshot_format"]
        want = {round(abs(sf["screenshot_ms"]["pct"]), 1),
                round(abs(sf["artifact_ms"]["pct"]), 1)}
        self.assertFalse(pcts & {40.0, 21.0},
                         f"refuted figures still published: {pcts & {40.0, 21.0}}")
        self.assertTrue(want <= pcts,
                        f"AGENTS.md says {sorted(pcts)}, corrected.json says {sorted(want)}")
        self.assertIn("REFUTED AS STATED", text,
                      "the REFUTED list must carry this claim, per its own preamble")

    def test_every_path_cited_in_a_doc_table_is_committed(self):
        """RED BEFORE THE FIX: the CDP-handshake table cited
        `scratchpad/cb/*.jsonl`, `scratchpad/cb/vmlogs/clone-*.log` and
        `results/20260808-corrected/requests/*.log`. `git ls-tree` shows zero
        `scratchpad` entries — the directory is not in the tree at all — and
        `results/` is gitignored except for the nine committed files, which do not
        include a `requests/` directory. AGENTS.md Deliverables rule 3 requires
        every figure to be traceable to a raw record.
        """
        import fnmatch
        import re

        root = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=HERE,
                              capture_output=True, text=True, check=True).stdout.strip()
        tracked = subprocess.run(["git", "ls-files"], cwd=root, capture_output=True,
                                 text=True, check=True).stdout.split()
        here_rel = os.path.relpath(HERE, root)
        bad = []
        for name in ("AGENTS.md", "REVIEW.md"):
            for line in self._read(name).splitlines():
                if not line.lstrip().startswith("|"):
                    continue
                for cited in re.findall(r"`([^`]*/[^`]*)`", line):
                    cited = cited.strip()
                    if not re.search(r"\.\w+$", cited):
                        continue
                    # Cites are written relative to the repo root OR to this
                    # directory; both are legitimate, neither is "uncommitted".
                    pats = (cited, f"{here_rel}/{cited}")
                    if not any(fnmatch.fnmatch(t, p) for t in tracked for p in pats):
                        bad.append(f"{name}: table cites {cited!r}, not in git ls-files")
        self.assertEqual(bad, [], "\n".join(bad))

    def test_the_readme_healthcheck_verification_actually_fails(self):
        """RED BEFORE THE FIX: the README's verification step was
        `podman inspect ... --format '{{json .HealthCheck}}'` followed by the prose
        "This must print the Test array, not `null`" — exit 0 whichever it printed.
        "Must" is an instruction to a human, inside a block designed to be pasted.
        `reqbench.sh` already has the hard gate for its own path, but bench.sh —
        the harness `make bench-chromium` runs — has no podman build, no
        `--format docker` and no healthcheck check at all, so on that route this
        non-failing line is the ENTIRE verification that the golden snapshot will
        freeze a WARM browser rather than a cold one.
        """
        import re

        for name in ("README.md", "AGENTS.md"):
            text = self._read(name)
            cmds = [c for c in re.split(r"\n(?!\s)", text.replace("\\\n", " "))
                    if "HealthCheck" in c and "podman" in c]
            self.assertTrue(cmds, f"{name}: no healthcheck verification found")
            with tempfile.TemporaryDirectory() as d:
                binx = os.path.join(d, "bin")
                os.makedirs(binx)
                with open(os.path.join(binx, "podman"), "w") as f:
                    f.write("#!/bin/bash\necho null\n")   # the OCI-drop outcome
                os.chmod(os.path.join(binx, "podman"), 0o755)
                env = dict(os.environ, PATH=binx + os.pathsep + os.environ["PATH"])
                for c in cmds:
                    snippet = "\n".join(
                        ln for ln in c.splitlines()
                        if ln.strip() and not ln.lstrip().startswith(("#", "`", ">"))
                    )
                    if not snippet.strip():
                        continue
                    r = subprocess.run(["bash", "-c", snippet], env=env,
                                       capture_output=True, text=True, timeout=60)
                    self.assertNotEqual(
                        r.returncode, 0,
                        f"{name}: the verification exits 0 on a MISSING healthcheck "
                        f"(fcvm treats a missing healthcheck as a PASS, so the golden "
                        f"snapshot would fire on a cold browser).\n{snippet}")


def probe_stub_source(clone_pid_file, exec_log, sleep_pid_file, exec_mode="canned"):
    """An fcvm stub that is BOTH a clone and an exec client.

    The clone half is the usual shape (a pdeathsig child, a state file, wait).
    The exec half is what makes the failure probe testable without a microVM: it
    answers `fcvm exec --pid N --vm -- sh -c <script>` with a canned framed
    reply, and records whether the clone was still ALIVE when the exec arrived.
    That liveness flag is the ordering proof: teardown's first act kills the
    clone, so an exec that finds it alive provably ran before teardown.

    `exec_mode="hang"` sleeps instead, with the sleep's pid recorded, so a test
    can check that the bound killed the process GROUP and not just the stub.
    """
    if exec_mode == "hang":
        exec_body = f"""
    sleep 120 &
    printf '%s\\n' "$!" > {shlex.quote(sleep_pid_file)}
    wait
    exit 0
"""
    else:
        exec_body = """
    for section in guest_date ip_neigh listening_sockets; do
        printf '===fcvm-probe-section %s\\n' "$section"
        printf 'stub output for %s\\n' "$section"
        printf '===fcvm-probe-rc %s 0\\n' "$section"
    done
    exit 0
"""
    return f"""#!/bin/bash
if [ "$1" = "exec" ]; then
    clone_pid=$(cat {shlex.quote(clone_pid_file)} 2>/dev/null || echo 0)
    if kill -0 "$clone_pid" 2>/dev/null; then alive=yes; else alive=no; fi
    printf 'exec clone_alive=%s\\n' "$alive" >> {shlex.quote(exec_log)}
{exec_body}
fi
python3 -c 'import ctypes,signal,time; ctypes.CDLL("libc.so.6").prctl(1, signal.SIGKILL); time.sleep(600)' &
printf '%s\\n' "$$" > {shlex.quote(clone_pid_file)}
"""


class ProbeBatchFraming(unittest.TestCase):
    """The framing has to survive a real shell and a cut-off batch.

    A batch is one `fcvm exec`, so every section's status rides back inside one
    stdout stream. Two things must hold: a section that failed reports ITS exit
    status rather than the batch's, and a batch the timeout cut in half reports
    the unterminated section as unknown rather than as success.
    """

    SECTIONS = (
        ("healthy", "echo hello"),
        ("failing", "exit 7"),
        ("absent", "fcvm-probe-no-such-binary-xyzzy"),
        ("piped", "printf 'a\\nb\\nc\\n' | tail -2"),
    )

    def test_each_section_carries_its_own_status_through_a_real_shell(self):
        script = reqbench.probe_batch_script(self.SECTIONS)
        result = subprocess.run(["sh", "-c", script], capture_output=True,
                                text=True, timeout=30)
        parsed = reqbench.parse_probe_batch(result.stdout)
        self.assertEqual(sorted(parsed), ["absent", "failing", "healthy", "piped"])
        self.assertEqual(parsed["healthy"]["rc"], 0)
        self.assertEqual(parsed["healthy"]["output"], "hello")
        self.assertEqual(parsed["failing"]["rc"], 7)
        self.assertEqual(parsed["absent"]["rc"], 127)
        self.assertIn("not found", parsed["absent"]["output"])
        self.assertEqual(parsed["piped"]["rc"], 0)
        self.assertEqual(parsed["piped"]["output"], "b\nc")

    def test_a_truncated_batch_reports_unknown_status_not_success(self):
        script = reqbench.probe_batch_script(self.SECTIONS)
        result = subprocess.run(["sh", "-c", script], capture_output=True,
                                text=True, timeout=30)
        cut = result.stdout.split(f"{reqbench.PROBE_RC_MARK} piped")[0]
        parsed = reqbench.parse_probe_batch(cut)
        self.assertEqual(parsed["healthy"]["rc"], 0)
        self.assertIsNone(
            parsed["piped"]["rc"],
            "a section whose status line never arrived must not read as rc 0",
        )
        self.assertEqual(parsed["piped"]["output"], "b\nc")


class ProbeLogMarkers(unittest.TestCase):
    """The dump quotes the clone's own restore narration instead of citing it."""

    def test_matching_lines_are_kept_head_and_tail_with_the_gap_named(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "clone.log")
            with open(path, "w") as f:
                f.write("boring line\n")
                for i in range(500):
                    f.write(f"[fc-agent] restore line {i}\n")
                    f.write(f"unrelated chatter {i}\n")
                f.write("guest MAC resolved ping_replied=false\n")
            out = reqbench.probe_log_markers(path, max_lines=10)
        self.assertEqual(out["matched"], 501)
        self.assertTrue(out["truncated"])
        self.assertIn("[fc-agent] restore line 0", out["lines"])
        self.assertIn("guest MAC resolved ping_replied=false", out["lines"])
        self.assertTrue(any("lines omitted" in line for line in out["lines"]))
        self.assertNotIn("boring line", out["lines"])

    def test_a_missing_log_is_recorded_rather_than_raised(self):
        out = reqbench.probe_log_markers("/nonexistent/clone.log")
        self.assertIn("error", out)
        self.assertEqual(out["matched"], 0)


class FailureProbeCapture(unittest.TestCase):
    """Guest-side and host-side evidence for a CDP failure, before teardown.

    RED BEFORE THE FIX (`test_a_failed_cdp_request_leaves_a_dump...`): the CDP
    arms tore the clone down the instant the request resolved, so the only record
    of a failure was its one-line `error` string. The 808-clone run's three
    failures had a live, interrogable guest, since vsock exec kept working the
    whole time, and every one of them was deleted before anything asked it a
    question.
    """

    def _args(self, d, stub, port, state_dir):
        return argparse.Namespace(
            fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
            snapshot_tag="", serve_pid=1, rust_log="off",
            timeout=10.0, teardown_timeout=5.0, cdp_port=port,
            state_dir=state_dir, data_root=d, ws_url="", run_id="0" * 32,
        )

    def _stub_clone(self, d, state_dir, name, port, exec_mode="canned"):
        """Write the stub plus the state file it will publish. Returns paths."""
        vm_id = "vm-22222222222222222222222222222222"
        clone_pid_file = os.path.join(d, "clone.pid")
        exec_log = os.path.join(d, "exec.log")
        sleep_pid_file = os.path.join(d, "sleep.pid")
        stub = os.path.join(d, "fcvm-stub")
        state_path = os.path.join(state_dir, f"{vm_id}.json")
        body = probe_stub_source(clone_pid_file, exec_log, sleep_pid_file, exec_mode)
        state = json.dumps({
            "vm_id": vm_id, "name": name, "pid": "PID", "pid_start_time": "START",
            "lifecycle_ready": True,
            "config": {"network": {"loopback_ip": "127.0.0.1"}},
        })
        body += (
            "read -r proc_stat < /proc/$$/stat; proc_stat=${proc_stat##*) }; "
            "read -ra proc_fields <<< \"$proc_stat\"; start=${proc_fields[19]}\n"
            f"printf '%s\\n' {shlex.quote(state)} "
            f"| sed -e \"s/\\\"PID\\\"/$$/\" -e \"s/\\\"START\\\"/$start/\" > {state_path}\n"
            f": > {state_path}.lock\n"
            "wait\n"
        )
        with open(stub, "w") as f:
            f.write(body)
        os.chmod(stub, 0o755)
        return stub, state_path, exec_log, sleep_pid_file

    def _drive(self, args, probe, fast=True):
        """Run one CDP rep and return its record however the rep ended.

        `teardown_fast` needs `/proc/<pid>/task/<tid>/children`, which a kernel
        built without `CONFIG_PROC_CHILDREN` does not have; there it raises
        `SurvivedTeardown` carrying the same record. The probe runs BEFORE
        teardown either way, which is the property under test, so both endings
        are accepted and neither is silently treated as a pass.
        """
        try:
            return reqbench.run_cdp_request(args, 0, fast=fast, probe=probe)
        except reqbench.SurvivedTeardown as error:
            return error.record

    def test_a_failed_cdp_request_leaves_a_dump_taken_while_the_clone_was_alive(self):
        import cdpdrive
        import socket

        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            server = socket.socket()
            server.bind(("127.0.0.1", 0))
            server.listen(8)
            port = server.getsockname()[1]
            name = f"rb-{'0' * 32}-0-fast"
            stub, state_path, exec_log, _ = self._stub_clone(d, state_dir, name, port)
            real_drive = cdpdrive.drive
            cdpdrive.drive = lambda _a: {
                "ok": False, "error": "TimeoutError: timed out",
                "failure_class": "transport", "stage": "connect", "stages": {},
            }
            probe = reqbench.FailureProbe(
                fcvm=stub, data_root=d, out_dir=d, run_id="0" * 32, cdp_port=port,
                command_timeout_s=10.0, budget_s=30.0,
            )
            clone_pid_file = os.path.join(d, "clone.pid")
            try:
                rec = self._drive(self._args(d, stub, port, state_dir), probe)
            finally:
                cdpdrive.drive = real_drive
                server.close()
                # The stub clone outlives a teardown that refused to prove its
                # child set. Tests must not leak either (AGENTS.md).
                try:
                    with open(clone_pid_file) as f:
                        os.kill(int(f.read().strip()), signal.SIGKILL)
                except (OSError, ValueError, ProcessLookupError):
                    pass

            self.assertIs(rec["ok"], False)
            self.assertIn("probe", rec, f"no probe stamp on a failed record: {rec}")
            self.assertEqual(rec["probe"]["role"], "failure")
            dump_path = rec["probe"]["path"]
            self.assertTrue(os.path.exists(dump_path), dump_path)
            with open(dump_path) as f:
                dump = json.load(f)

            # The dump names its own request, so the artifact stands alone.
            self.assertEqual(dump["name"], name)
            self.assertEqual(dump["run_id"], "0" * 32)
            self.assertEqual(dump["failure_stage"], "connect")
            self.assertIn("TimeoutError", dump["request_error"])

            # Guest side, over the vsock exec path that keeps working.
            self.assertEqual(
                sorted(dump["guest"]["passive"]["sections"]),
                ["guest_date", "ip_neigh", "listening_sockets"],
            )
            self.assertIn("--vm", dump["guest"]["passive"]["argv"])
            self.assertIn("active_mutating", dump["guest"])
            self.assertIs(dump["guest"]["active_mutating"]["mutates_guest_state"], True)

            # Host side for the same clone.
            self.assertEqual(dump["host"]["clone_state"]["vm_id"],
                             "vm-22222222222222222222222222222222")
            self.assertIs(dump["host"]["cdp_connect_now"]["connected"], True)
            self.assertIn("log_markers", dump["host"])
            self.assertIn("pasta", dump["host"])

            # Ordering: the clone was still alive when the probe reached it, and
            # teardown's first act is to kill it.
            with open(exec_log) as f:
                execs = f.read().split()
            self.assertTrue(execs, "the probe never ran an exec against the clone")
            self.assertNotIn(
                "clone_alive=no", execs,
                "the probe must run BEFORE teardown, while the guest still exists",
            )

    def test_a_success_after_the_control_writes_no_dump(self):
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222,
            )
            probe.control_captured = True
            rec = {"arm": "cdp", "rep": 3, "ok": True}
            probe.observe(rec, name="rb-x-3-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            self.assertNotIn("probe", rec)
            self.assertEqual(
                [n for n in os.listdir(d) if n.endswith(".probe.json")], [],
                "a healthy request past the control must write nothing",
            )

    def test_the_healthy_control_is_taken_once_and_only_from_a_healthy_clone(self):
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222, budget_s=5.0,
            )
            probe.begin_request(True)
            first = {"arm": "cdp", "rep": 0, "ok": True}
            probe.observe(first, name="rb-x-0-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            second = {"arm": "cdp", "rep": 1, "ok": True}
            probe.observe(second, name="rb-x-1-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            self.assertEqual(first["probe"]["role"], "control")
            self.assertNotIn("probe", second)
            self.assertNotIn(
                "probe_perturbed_timings", first,
                "a warmup rep is discarded at analysis, so it is not a perturbation",
            )
            dumps = [n for n in os.listdir(d) if n.endswith(".probe.json")]
            self.assertEqual(dumps, ["rb-x-0-fast.probe.json"],
                             "the second healthy rep must write nothing")

            # A later failure has to be able to find the control it is read
            # against; the control itself has nothing earlier to point at.
            self.assertEqual(first["probe"]["control_path"], "")
            failed = {"arm": "cdp", "rep": 2, "ok": False}
            probe.observe(failed, name="rb-x-2-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            self.assertEqual(failed["probe"]["control_path"],
                             first["probe"]["path"])

    def _healthy(self, probe, rep):
        rec = {"arm": "cdp", "rep": rep, "ok": True}
        probe.observe(rec, name=f"rb-x-{rep}-fast", fcvm_pid=os.getpid(),
                      state_path="", log_path="", endpoint="")
        return rec

    def test_a_control_that_fails_to_write_retries_on_the_next_healthy_clone(self):
        """RED BEFORE THE FIX: `control_captured` was set in a block that also
        ran on the exception path, so one failed write retired the control for
        the whole run and every later failure dump had nothing to be read
        against:

            AssertionError: True is not false : a dump that was never written
            is not the control
        """
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222, budget_s=5.0,
            )
            probe.begin_request(True)
            real_write = probe.write
            written = []

            def fail_the_first_write(name, dump):
                written.append(name)
                if len(written) == 1:
                    raise OSError(28, "No space left on device")
                return real_write(name, dump)

            probe.write = fail_the_first_write
            first = self._healthy(probe, 0)
            self.assertIn("No space left on device", first["probe"]["probe_error"])
            self.assertEqual(first["probe"]["path"], "")
            self.assertFalse(probe.control_captured,
                             "a dump that was never written is not the control")

            second = self._healthy(probe, 1)
            self.assertEqual(second["probe"]["role"], "control")
            self.assertTrue(second["probe"]["path"].endswith(
                "rb-x-1-fast.probe.json"))
            self.assertTrue(probe.control_captured)

            # The point of retrying: a later failure has something to read the
            # dump against.
            failed = {"arm": "cdp", "rep": 2, "ok": False}
            probe.observe(failed, name="rb-x-2-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            self.assertEqual(failed["probe"]["control_path"],
                             second["probe"]["path"])

            # And the retry does not become a second control.
            third = self._healthy(probe, 3)
            self.assertNotIn("probe", third)

    def test_a_control_that_never_writes_stops_taxing_healthy_reps(self):
        """Retrying forever is the other way to get this wrong: each attempt
        costs up to the full budget and perturbs the measured rep it lands on,
        so a systematically broken probe would tax every healthy request in the
        run."""
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222, budget_s=5.0,
            )
            probe.begin_request(True)

            def never_writes(name, dump):
                raise OSError(28, "No space left on device")

            probe.write = never_writes
            attempted = [rep for rep in range(probe.CONTROL_ATTEMPTS + 2)
                         if "probe" in self._healthy(probe, rep)]
            self.assertEqual(attempted, list(range(probe.CONTROL_ATTEMPTS)),
                             "the control retry is not bounded")
            self.assertEqual(probe.control_attempts, probe.CONTROL_ATTEMPTS)
            self.assertFalse(probe.control_captured)

    def test_a_control_taken_from_a_measured_rep_is_stamped_as_perturbing(self):
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222, budget_s=5.0,
            )
            probe.begin_request(False)
            rec = {"arm": "cdp", "rep": 4, "ok": True}
            with io.StringIO() as buf:
                stderr, sys.stderr = sys.stderr, buf
                try:
                    probe.observe(rec, name="rb-x-4-fast", fcvm_pid=os.getpid(),
                                  state_path="", log_path="", endpoint="")
                finally:
                    sys.stderr = stderr
                warning = buf.getvalue()
            self.assertIs(rec["probe_perturbed_timings"], True)
            self.assertIn("probe_perturbed_timings", warning)

    def test_a_probe_that_raises_is_recorded_and_the_request_survives(self):
        """RED BEFORE THE FIX: `observe` called `capture` bare, so anything the
        probe got wrong took down a request that had ALREADY produced its answer
        and, through `main`'s `fatal` path, the rest of the schedule."""
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222,
            )

            def explode(**_kwargs):
                raise RuntimeError("probe blew up")

            probe.capture = explode
            rec = {"arm": "cdp", "rep": 2, "ok": False, "error": "original failure"}
            probe.observe(rec, name="rb-x-2-fast", fcvm_pid=os.getpid(),
                          state_path="", log_path="", endpoint="")
            self.assertIn("probe blew up", rec["probe"]["probe_error"])
            self.assertEqual(rec["error"], "original failure",
                             "the probe must not overwrite the real failure")

    def test_a_hung_guest_exec_is_cut_off_at_the_bound_with_its_group(self):
        """A wedged guest must cost the bound, not the request's whole budget.

        `fcvm exec`'s own connect ladder spans ~54 s, and the exec holds a
        command running inside the guest, so killing only the direct child would
        leave the real work behind. The bound therefore kills the process GROUP,
        and this asserts the grandchild was KILLED rather than asserting its
        procfs entry went away. Those are different claims, and the second one
        is about the reaper rather than about the kill. This test adopts the
        orphan and reads its exit status, so it says what it means.
        """
        with tempfile.TemporaryDirectory() as d, child_subreaper():
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            stub, state_path, _, sleep_pid_file = self._stub_clone(
                d, state_dir, "rb-x-0-fast", 9222, exec_mode="hang"
            )
            probe = reqbench.FailureProbe(
                fcvm=stub, data_root=d, out_dir=d, run_id="0" * 32, cdp_port=9222,
                command_timeout_s=1.0, budget_s=30.0,
            )
            started = time.monotonic()
            result = probe.exec_batch(os.getpid(), (("noop", "true"),), 1.0)
            elapsed = time.monotonic() - started
            self.assertIs(result["timed_out"], True)
            self.assertLess(elapsed, 15.0, f"the bound did not hold: {elapsed:.1f}s")
            self.assertEqual(result["sections"], {},
                             "a cut-off batch has no completed sections")
            with open(sleep_pid_file) as f:
                grandchild = int(f.read().strip())
            note = ("the timeout killed the exec wrapper but left its "
                    "guest-side work")
            assert_sigkilled(self, grandchild, reap_orphan(grandchild, note), note)

    def test_a_zombie_group_member_is_not_counted_as_a_survivor(self):
        """The kill's verification has to tell a corpse from a survivor.

        `killpg(pgid, 0)` cannot: an unreaped zombie keeps the whole group
        present, so on a host whose PID 1 does not reap it would report the
        group alive forever. `live_group_members` reads each member's state, and
        this checks it against a group holding one of each.
        """
        code = (
            "import subprocess,sys,time;"
            "corpse=subprocess.Popen(['true']);"          # never waited on
            "live=subprocess.Popen(['sleep','300']);"
            "open(sys.argv[1],'w').write(f'{corpse.pid} {live.pid}');"
            "time.sleep(300)"
        )
        with tempfile.TemporaryDirectory() as d:
            pid_file = os.path.join(d, "members.pid")
            leader = subprocess.Popen([sys.executable, "-c", code, pid_file],
                                      start_new_session=True)
            try:
                deadline = time.monotonic() + 10
                corpse = live = None
                while time.monotonic() < deadline:
                    try:
                        with open(pid_file) as handle:
                            corpse, live = (int(x) for x in
                                            handle.read().split())
                    except (OSError, ValueError):
                        time.sleep(0.02)
                        continue
                    state = reqbench.proc_stat_fields(corpse)
                    if state and state[0] == "Z":
                        break
                    time.sleep(0.02)
                self.assertIsNotNone(corpse, "the group never came up")
                # Without this the assertion below passes for the wrong reason.
                self.assertEqual(reqbench.proc_stat_fields(corpse)[0], "Z",
                                 "the corpse was reaped, so it proves nothing")
                members = reqbench.live_group_members(leader.pid)
                self.assertIn(leader.pid, members)
                self.assertIn(live, members)
                self.assertNotIn(corpse, members,
                                 "a zombie is dead, so it is not a survivor")
            finally:
                outcome = reqbench.kill_process_group(leader.pid)
                leader.wait(timeout=5)
            self.assertEqual(outcome["survivors"], [],
                             "the group outlived its own kill")

    def test_the_bound_kills_the_group_when_the_leader_is_already_gone(self):
        """RED BEFORE THE FIX: the group was named by asking the leader for it.

        `os.getpgid(proc.pid)` raises ESRCH once the leader is gone, and the
        `proc.kill()` fallback then re-signals that same dead leader while the
        work it spawned keeps running:

            AssertionError: the group kill missed the descendant the vanished
            leader left behind: pid 4193142 is still running 5s later, state
            ('S', 0, 0, 254146417)

        The window is real and this reproduces it rather than racing for it. The
        wrapper exits immediately, its child keeps the stdout pipe open, so
        `communicate()` still blocks for the whole timeout. A SIGCHLD reaper
        makes the leader's pid VANISH inside that window instead of lingering as
        a zombie, which is what turns `getpgid` into ESRCH; any harness that
        reaps its own children supplies one.
        """
        with tempfile.TemporaryDirectory() as d, child_subreaper():
            survivor_pid_file = os.path.join(d, "survivor.pid")
            wrapper = os.path.join(d, "vanishing-leader")
            with open(wrapper, "w") as f:
                f.write(
                    "#!/bin/sh\n"
                    "sleep 300 &\n"
                    f"echo $! > {shlex.quote(survivor_pid_file)}\n"
                    "exit 0\n"
                )
            os.chmod(wrapper, 0o755)
            statuses = {}

            def reap_any_child(_signum, _frame):
                while True:
                    try:
                        pid, status = os.waitpid(-1, os.WNOHANG)
                    except ChildProcessError:
                        return
                    if pid == 0:
                        return
                    statuses[pid] = status

            previous = signal.signal(signal.SIGCHLD, reap_any_child)
            try:
                record = reqbench.run_probe_command([wrapper], 1.0)
            finally:
                signal.signal(signal.SIGCHLD, previous)

            self.assertIs(record["timed_out"], True)
            self.assertTrue(
                statuses,
                "the leader was never reaped, so the ESRCH window never opened",
            )
            with open(survivor_pid_file) as f:
                survivor = int(f.read().strip())
            note = ("the group kill missed the descendant the vanished leader "
                    "left behind")
            status = statuses.get(survivor)
            if status is None:
                status = reap_orphan(survivor, note)
            assert_sigkilled(self, survivor, status, note)
            self.assertEqual(record["group_kill"]["survivors"], [],
                             "the kill was not verified against the group")

    def test_a_pending_termination_signal_skips_the_probe_entirely(self):
        """A probe that delays a shutdown can leak the clone it came to explain.

        RED BEFORE THE FIX: the handler only RECORDS INT/TERM, so a signal that
        landed after the request's last interrupt poll left the probe free to
        spend its whole budget before teardown started, which is where a job
        runner escalates to SIGKILL:

            AssertionError: 2.017 not less than 0.5 : the probe ran with a
            termination signal pending

        The signal is injected through the harness's own handler, so this is the
        state a real INT/TERM leaves behind, not a stand-in for it.
        """
        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            stub, _, _, _ = self._stub_clone(d, state_dir, "rb-x-0-fast", 9222,
                                             exec_mode="hang")
            probe = reqbench.FailureProbe(
                fcvm=stub, data_root=d, out_dir=d, run_id="0" * 32, cdp_port=9222,
                command_timeout_s=1.0, budget_s=60.0,
            )
            rec = {"arm": "cdp", "rep": 0, "ok": False, "error": "TimeoutError"}
            with pending_harness_signal(signal.SIGTERM):
                started = time.monotonic()
                probe.observe(rec, name="rb-x-0-fast", fcvm_pid=os.getpid(),
                              state_path="", log_path="", endpoint="")
                elapsed = time.monotonic() - started
            self.assertLess(elapsed, 0.5,
                            "the probe ran with a termination signal pending")
            self.assertEqual(rec["probe"]["skipped"],
                             f"termination signal {int(signal.SIGTERM)} pending")
            self.assertEqual(rec["probe"]["path"], "")
            self.assertEqual(
                [n for n in os.listdir(d) if n.endswith(".probe.json")], [],
                "a skipped probe must not leave a half-written dump",
            )

    def test_a_signal_arriving_mid_capture_stops_the_remaining_steps(self):
        """RED BEFORE THE FIX: nothing inside `capture` looked at the pending
        signal, so a capture already under way kept going step by step:

            AssertionError: 'active_mutating' unexpectedly found in
            dict_keys(['passive', 'active_mutating'])
             : the steps after the signal must not run

        The signal is injected from inside the first probe command, which is
        where a capture spends nearly all of its time and therefore where a real
        one is most likely to land.
        """
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222, command_timeout_s=1.0,
                budget_s=60.0,
            )
            real_run = reqbench.run_probe_command
            fired = []

            def signal_during_the_first_command(*args, **kwargs):
                if not fired:
                    fired.append(True)
                    reqbench.record_harness_interrupt(signal.SIGINT, None)
                return real_run(*args, **kwargs)

            reqbench.run_probe_command = signal_during_the_first_command
            try:
                with pending_harness_signal(0):
                    dump = probe.capture(
                        role="failure", rec={"arm": "cdp", "rep": 0, "ok": False},
                        name="rb-x-0-fast", fcvm_pid=os.getpid(), state_path="",
                        log_path="", endpoint="127.0.0.1:9",
                    )
            finally:
                reqbench.run_probe_command = real_run
            self.assertTrue(fired, "no probe command ran, so nothing was injected")
            self.assertIn("passive", dump["guest"],
                          "the step that was already running must be kept")
            self.assertNotIn("active_mutating", dump["guest"],
                             "the steps after the signal must not run")
            self.assertEqual(dump["interrupted_by_signal"], int(signal.SIGINT))
            self.assertTrue(
                [e for e in dump["errors"] if "termination signal" in e],
                f"the skipped steps must name the signal: {dump['errors']}",
            )

    def test_the_capture_stops_at_its_budget_and_says_so(self):
        with tempfile.TemporaryDirectory() as d:
            probe = reqbench.FailureProbe(
                fcvm="/nonexistent/fcvm", data_root=d, out_dir=d,
                run_id="0" * 32, cdp_port=9222,
                command_timeout_s=5.0, budget_s=0.0,
            )
            dump = probe.capture(
                role="failure", rec={"arm": "cdp", "rep": 0, "ok": False},
                name="rb-x-0-fast", fcvm_pid=os.getpid(), state_path="",
                log_path="", endpoint="",
            )
            self.assertIs(dump["budget_exhausted"], True)
            self.assertNotIn("passive", dump["guest"])
            self.assertNotIn("active_mutating", dump["guest"])
            self.assertTrue(
                [e for e in dump["errors"] if "budget" in e],
                f"the skipped steps must be named: {dump['errors']}",
            )
            # Even a fully skipped capture is a usable artifact.
            self.assertIn("fcvm_process", dump["host"])
            self.assertIn("log_markers", dump["host"])

    def test_a_step_given_a_shortened_timeout_says_so_on_its_own_record(self):
        """`timed_out` from a nearly-spent budget is not a wedged guest.

        Without this label the two are the same boolean, and telling them apart
        is the entire job of the dump.
        """
        with tempfile.TemporaryDirectory() as d:
            state_dir = os.path.join(d, "state")
            os.makedirs(state_dir)
            stub, _, _, _ = self._stub_clone(d, state_dir, "rb-x-0-fast", 9222)
            probe = reqbench.FailureProbe(
                fcvm=stub, data_root=d, out_dir=d, run_id="0" * 32, cdp_port=9222,
                command_timeout_s=30.0, budget_s=5.0,
            )
            dump = probe.capture(
                role="failure", rec={"arm": "cdp", "rep": 0, "ok": False},
                name="rb-x-0-fast", fcvm_pid=os.getpid(), state_path="",
                log_path="", endpoint="",
            )
            passive = dump["guest"]["passive"]
            self.assertIs(passive["budget_limited"], True)
            self.assertLess(passive["timeout_s"], 30.0)
            self.assertIs(passive["timed_out"], False)
            self.assertEqual(sorted(passive["sections"]),
                             ["guest_date", "ip_neigh", "listening_sockets"])


class FailureProbeWiring(unittest.TestCase):
    """The probe has to REACH the CDP arms; a gap here is invisible until the
    next unexplained failure has already been torn down."""

    def _recorders(self):
        calls = []

        def cdp(args, rep, fast, probe=None):
            calls.append(("cdp-fast" if fast else "cdp", probe))
            return {"arm": "cdp", "rep": rep}

        def other(name):
            def run(args, rep):
                calls.append((name, None))
                return {"arm": name, "rep": rep}
            return run

        return calls, cdp, other

    def test_both_cdp_arms_get_the_probe_and_the_warmup_flag(self):
        calls, cdp, other = self._recorders()
        seen = []

        class Recorder(reqbench.FailureProbe):
            def __init__(self):
                super().__init__(fcvm="", data_root="", out_dir="",
                                 run_id="", cdp_port=0)

            def begin_request(self, is_warmup):
                seen.append(is_warmup)
                super().begin_request(is_warmup)

        probe = Recorder()
        saved = (reqbench.run_cdp_request, reqbench.run_exec_request,
                 reqbench.run_noop_request)
        reqbench.run_cdp_request = cdp
        reqbench.run_exec_request = other("exec")
        reqbench.run_noop_request = other("noop")
        try:
            for arm, warm in (("cdp", True), ("cdp-fast", False),
                              ("exec", False), ("noop", True)):
                reqbench.dispatch_request(argparse.Namespace(), 0, arm, warm, probe)
        finally:
            (reqbench.run_cdp_request, reqbench.run_exec_request,
             reqbench.run_noop_request) = saved
        self.assertEqual([name for name, _ in calls],
                         ["cdp", "cdp-fast", "exec", "noop"])
        self.assertIs(calls[0][1], probe)
        self.assertIs(calls[1][1], probe)
        self.assertEqual(seen, [True, False, False, True],
                         "every arm must set the warmup flag, not just the CDP ones")

    def test_the_dispatcher_is_the_only_request_call_site_in_the_schedule(self):
        """`main` must not reach an arm around the dispatcher.

        That `main` hands a REAL probe to a CDP arm is proved behaviourally, by
        `SnapshotGenerationIdentity.test_main_holds_the_generation_lease_through_the_full_schedule`,
        which drives the actual `main` and asserts on the object the arm
        received. What that cannot see is a second call site added later that
        bypasses the dispatcher, since it would simply not be exercised by that
        run's schedule, so this reads the loop body.
        """
        with open(os.path.join(HERE, "reqbench.py")) as f:
            body = f.read().split("for rep, arm, is_warmup in schedule:", 1)
        self.assertEqual(len(body), 2, "the schedule loop moved; update this lint")
        for arm_call in ("run_cdp_request(", "run_exec_request(", "run_noop_request("):
            self.assertNotIn(
                arm_call, body[1],
                f"the schedule loop calls {arm_call} directly instead of "
                "dispatch_request, so the probe is not carried",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)


class NoConfusableIdentifiers(unittest.TestCase):
    """No bench source may contain a Cyrillic homoglyph.

    `reqstages.py` had a variable whose name was Latin `g` followed by CYRILLIC SMALL
    LETTER O (U+043E) rather than Latin `o`. It runs, and it is indistinguishable on
    screen from `go`, so anyone who types the name they can plainly see gets a NameError
    on a variable that is right there. The same trap exists for U+0441/c, U+0435/e,
    U+0430/a, U+0440/p, U+0445/x.

    This file states those characters by CODEPOINT, never literally: an earlier version
    spelled them out and the guard flagged its own docstring, which is funny once and
    useless afterwards.

    A reviewer caught the first one. This catches the next.
    """

    def test_no_cyrillic_homoglyphs_in_bench_sources(self):
        import pathlib

        here = pathlib.Path(__file__).parent
        offenders = []
        for path in sorted(here.glob("*.py")) + sorted(here.glob("*.sh")):
            text = path.read_text(encoding="utf-8", errors="replace")
            for lineno, line in enumerate(text.split("\n"), 1):
                bad = [ch for ch in line if 0x0400 <= ord(ch) <= 0x04FF]
                if bad:
                    offenders.append(
                        f"{path.name}:{lineno} contains {[hex(ord(c)) for c in bad]} "
                        f"in: {line.strip()[:70]}"
                    )
        self.assertEqual(
            offenders,
            [],
            "Cyrillic characters found in bench sources. If one sits inside an identifier "
            "it is a homoglyph trap: the name reads as ASCII and is not.\n"
            + "\n".join(offenders),
        )


class TimedWsUpgradeDiagnostics(unittest.TestCase):
    """A WebSocket-upgrade failure must still report the socket it failed on.

    `ws = TimedWs(...)` binds `ws` only when the constructor RETURNS, so a
    failure during the HTTP upgrade left the name unbound and cdpdrive's
    handler emitted a transport failure with no `socket_local`, `socket_peer`
    or `socket_so_error` — the three fields that say WHICH connection died.
    Red before the fix: `AssertionError: socket diagnostics missing: []`.
    """

    def _serve_once(self, respond: bytes):
        import socket as _socket
        import threading as _threading

        listener = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
        listener.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        port = listener.getsockname()[1]

        def run():
            conn, _ = listener.accept()
            try:
                conn.recv(4096)
                conn.sendall(respond)
            except OSError:
                pass
            finally:
                conn.close()
                listener.close()

        thread = _threading.Thread(target=run, daemon=True)
        thread.start()
        return port, thread

    def test_upgrade_rejection_carries_socket_diagnostics(self):
        import importlib.util
        import pathlib
        import socket as _socket

        here = pathlib.Path(__file__).parent
        spec = importlib.util.spec_from_file_location("cdpdrive", here / "cdpdrive.py")
        cdpdrive = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cdpdrive)
        render = importlib.import_module("render") if False else None
        spec_r = importlib.util.spec_from_file_location("render", here / "render.py")
        render = importlib.util.module_from_spec(spec_r)
        spec_r.loader.exec_module(render)

        # A server that completes TCP and then REJECTS the upgrade: the failure
        # lands after self.sock exists, which is the case that lost diagnostics.
        port, thread = self._serve_once(b"HTTP/1.1 403 Forbidden\r\n\r\n")
        with self.assertRaises(ConnectionError) as caught:
            cdpdrive.TimedWs(render, f"ws://127.0.0.1:{port}/devtools/page/x", time.monotonic() + 5)
        thread.join(timeout=5)

        diagnostics = getattr(caught.exception, "fcvm_socket_diagnostics", {})
        self.assertEqual(
            sorted(diagnostics),
            ["socket_local", "socket_peer", "socket_so_error"],
            f"socket diagnostics missing: {sorted(diagnostics)}. An upgrade failure "
            "must still say which connection died; the values have to be read while "
            "the socket is open, since a closed one raises EBADF and drive() "
            "swallows that.",
        )
        self.assertEqual(diagnostics["socket_local"][0], "127.0.0.1")
        self.assertEqual(diagnostics["socket_peer"][1], port)
        self.assertIsInstance(diagnostics["socket_so_error"], int)


class HarnessIdentity(unittest.TestCase):
    """harness_sha256 must cover every staged script that defines a sample.

    The staged runtime bundle (reqbench.sh's `for source in ...` list) is what
    actually runs; a script in the bundle but not in the hash means two runs
    with different driver code can carry the same harness identity. That was
    real: wddrive.py defined every webkit sample while harness_sha256 omitted
    it.
    """

    def test_harness_hash_covers_every_staged_request_script(self):
        sh = open(os.path.join(HERE, "reqbench.sh")).read()
        m = re.search(r"for source in ([^;]+); do", sh)
        self.assertIsNotNone(m, "staged source list not found in reqbench.sh")
        staged = set(m.group(1).split())
        # reqanalyze.py is staged (the analysis step runs from the bundle) but
        # defines no request sample.
        self.assertEqual(set(reqbench.HARNESS_SOURCES), staged - {"reqanalyze.py"})


class MakefileBenchGraph(unittest.TestCase):
    """The Chromium bench make targets must encode their real dependencies.

    Watched red 2026-08-13 against the PHASE?=run single-target Makefile:
    every assertion failed with "bench-chromium-request-golden not found in
    make database" (and the PHASE target still present). That day's golden had
    failed at RUNTIME instead — "Custom firecracker not found ... Run: fcvm
    setup --kernel-profile default" — because the only make entry point
    depended on `build` alone and nothing expressed the asset dependency.
    """

    REPO = os.path.dirname(os.path.dirname(HERE))

    @classmethod
    def setUpClass(cls):
        out = subprocess.run(
            ["make", "-C", cls.REPO, "-pq", "help"],
            capture_output=True, text=True, timeout=120)
        # -q exits 0/1 for up-to-date/rebuild-needed; 2 means make itself
        # FAILED to parse — and it still prints a partial database, so
        # trusting stdout alone lets a broken Makefile pass every structural
        # assertion below (codex P2, 2026-08-14).
        cls.make_rc = out.returncode
        cls.make_stderr = out.stderr[-2000:]
        cls.rules = {}
        cls.recipes = {}
        cls.phony = set()
        cur = None
        for line in out.stdout.splitlines():
            if line.startswith("\t") and cur:
                cls.recipes.setdefault(cur, []).append(line)
                continue
            if line.startswith(".PHONY:"):
                cls.phony.update(line.partition(":")[2].split())
                continue
            if line.startswith("#"):
                # -p interleaves "# recipe to execute (from ...)" comments
                # between a rule and its recipe: keep cur so the recipe
                # lines that follow still attach to their target.
                continue
            # Rule lines sit at column 0 as "target: prereqs". Skip recipes,
            # special targets, and target-specific variable lines
            # ("bench-quick: BENCH_ARGS := ..."), which contain "=".
            if not line or line[0] in "\t." or "=" in line or ":" not in line:
                cur = None
                continue
            tgt, _, prereqs = line.partition(":")
            cur = tgt.strip()
            cls.rules.setdefault(cur, set()).update(prereqs.split())

    def prereqs(self, target):
        self.assertIn(target, self.rules,
                      f"{target} not found in make database")
        return self.rules[target]

    def closure(self, target):
        seen, stack = set(), [target]
        while stack:
            for p in self.rules.get(stack.pop(), ()):
                if p not in seen:
                    seen.add(p)
                    stack.append(p)
        return seen

    def test_make_database_is_from_a_parsable_makefile(self):
        self.assertIn(self.make_rc, (0, 1),
                      f"make -pq exited {self.make_rc} (fatal parse error) — "
                      f"the database below it is partial and every other "
                      f"assertion in this class is vacuous. stderr: "
                      f"{self.make_stderr}")

    def test_golden_depends_on_binary_and_assets(self):
        p = self.prereqs("bench-chromium-request-golden")
        self.assertIn("bench-chromium-request-build", p)
        self.assertIn("setup-default", p)

    def test_image_build_depends_on_fcvm_build(self):
        self.assertIn("build", self.prereqs("bench-chromium-request-build"))

    def test_measured_phases_never_rebuild(self):
        # verify/run stage the CURRENT binary into the runtime bundle, and the
        # run refuses a golden whose provenance records a different bundle
        # hash. A `build` prerequisite here could swap the binary between
        # golden and run: the seal would fail closed, but the chain would be
        # self-breaking. The binary under test comes from the golden-time
        # build, so these targets must not rebuild anything — TRANSITIVELY:
        # a direct-only check passes `run: bench-chromium-request-build`,
        # which rebuilds through its own deps (codex P2, 2026-08-14).
        for t in ("bench-chromium-request-run", "bench-chromium-request-verify"):
            c = self.closure(t)
            for forbidden in ("build", "setup-default", "cargo-target-link"):
                self.assertNotIn(forbidden, c,
                                 f"{t} transitively reaches {forbidden}")

    def test_bench_targets_are_phony(self):
        # A stray file named like a target silently suppresses its recipe;
        # make then reports "up to date" and nothing runs.
        for t in ("bench-chromium-request-build", "bench-chromium-request-golden",
               "bench-chromium-request-verify", "bench-chromium-request-run",
               "bench-chromium-request-all", "bench-chromium-hostcdp",
               "bench-chromium-fault"):
            self.assertIn(t, self.phony, f"{t} missing from .PHONY")

    def test_webkit_run_forwards_the_measurement_knobs(self):
        # Without the forwarding, `make bench-webkit-request-run REPS=202`
        # silently ran reqbench.sh's default rep count and wrote to a
        # RUNID-derived directory while analyze-chromium-request read the
        # empty $(RESULTS).
        recipe = "\n".join(self.recipes.get("bench-webkit-request-run", []))
        for knob in ("BACKEND", "REPS", "WARMUP", "RESULTS"):
            self.assertIn(f'{knob}="$({knob})"', recipe,
                          f"bench-webkit-request-run does not forward {knob}")

    def test_webkit_build_routes_through_the_healthcheck_assert(self):
        # A raw `podman build` here skips cmd_build's HEALTHCHECK assertion,
        # so an image that lost its healthcheck (OCI format drop) snapshots a
        # cold browser and the golden gate cannot notice.
        recipe = "\n".join(self.recipes.get("bench-webkit-request-build", []))
        self.assertIn("reqbench.sh build", recipe)
        self.assertNotIn("podman build", recipe)

    def test_fault_target_provisions_its_hugepage_pool(self):
        # faultbench selects uffd-huge-minor whenever the huge golden exists,
        # and bench.sh restores the pool (commonly to zero) after creating
        # that golden — so without provisioning here the new entry point
        # fails immediately after the workflow that creates its own
        # prerequisite (codex P1, 2026-08-14).
        recipe = "\n".join(self.recipes.get("bench-chromium-fault", []))
        # Three reads of the knob: current-value cat, the grow write, and
        # the POST-WRITE reread. Linux accepts the write even when
        # fragmentation delivers fewer pages, so a recipe that never
        # rereads reports a pool it does not have (codex round 2).
        self.assertGreaterEqual(recipe.count("nr_hugepages"), 3,
                                "fault recipe must reread the pool "
                                "after growing it")
        self.assertIn("ERROR", recipe,
                      "short delivery must fail the target")
        # Growth is gated on the huge golden existing: a file-4k-only
        # run must not reserve gigabytes it will never touch.
        self.assertIn("cb-golden-huge", recipe,
                      "pool growth must be gated on the huge golden")
        # And the grow must hold the cross-harness pool lock (codex P1,
        # PR #815): the pool is host-global and reqbench/faultbench/bench.sh
        # all write it.
        self.assertIn("hugepage-pool.lock", recipe,
                      "fault recipe must serialize on the shared pool lock")
        # The lock file must be creatable by unprivileged callers even when
        # the data root is root-owned (fresh boxes): the recipe pre-creates
        # it with sudo before flock (CodeRabbit round 2, PR #815).
        self.assertRegex(recipe, r"sudo[^\n]*touch[^\n]*hugepage-pool\.lock",
                         "fault recipe must sudo-pre-create the pool lock")

    def test_run_help_documents_tag(self):
        # Following the documented huge flow without TAG= on the run line
        # measures the DEFAULT 4K tag while the huge golden sits unused
        # (codex P2, 2026-08-14).
        help_lines = [ln for ln in self.recipes.get("help", [])
                      if "bench-chromium-request-run" in ln]
        self.assertTrue(help_lines, "run target missing from help")
        self.assertIn("TAG=", help_lines[0])

    def test_full_chain_and_companion_benches(self):
        p = self.prereqs("bench-chromium-request-all")
        self.assertIn("build", p)
        self.assertIn("setup-default", p)
        self.assertIn("bench-chromium-request-build",
                      self.prereqs("bench-chromium-hostcdp"))
        fp = self.prereqs("bench-chromium-fault")
        self.assertIn("build", fp)
        self.assertIn("setup-default", fp)

    def test_phase_indirection_is_gone(self):
        # NO LEGACY: the PHASE?=run single target could not express
        # inter-phase dependencies and is exactly how a golden ran on a box
        # with no firecracker asset. Its survival (including as a stray
        # .PHONY entry) means the clean break did not happen.
        self.assertNotIn("bench-chromium-request", self.rules)
        self.assertNotIn("bench-chromium-request", self.phony)


class HugepageGuards(unittest.TestCase):
    """reqbench.sh must fail closed around hugepage goldens.

    Watched red 2026-08-14 before the fix: `ensure_hugepage_pool` and
    `hugepage_snapshot_state` did not exist (bash: command not found), and
    `cmd_run BACKEND=file` against a hugepage-snapshot fixture sailed past
    the missing check into guard_quiet. Codex P1s: (a) a hugepage golden
    restored with BACKEND=file silently starts a UFFD server while the
    record says backend=file — the analyzer then gates MISLABELED data;
    (b) the pool grow was golden-only and fixed at 2048 pages, ignoring
    MEM and later phases (a 2050 MiB guest needs 4100 pages; a rebooted
    box re-runs verify/run with an empty pool).
    """

    SH = os.path.join(HERE, "reqbench.sh")

    def _bash(self, snippet, env_extra=None, hugepages="true"):
        d = tempfile.mkdtemp(prefix="hugeguard-")

        def _cleanup(path=d):
            shutil.rmtree(path)
            assert not os.path.exists(path), f"cleanup left {path}"

        self.addCleanup(_cleanup)
        snapdir = os.path.join(d, "data", "snapshots", "tag-under-test")
        os.makedirs(snapdir)
        with open(os.path.join(snapdir, "config.json"), "w") as f:
            f.write('{"metadata": {"hugepages": %s, "memory_mib": 4096}}'
                    % hugepages)
        pool = os.path.join(d, "nr_hugepages")
        with open(pool, "w") as f:
            f.write("100\n")
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        with open(os.path.join(binx, "sudo"), "w") as f:
            f.write('#!/bin/bash\nexec "$@"\n')
        os.chmod(os.path.join(binx, "sudo"), 0o755)
        env = dict(os.environ)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            TAG="tag-under-test",
            STATE_DIR=os.path.join(d, "data", "state"),
            HUGEPAGE_POOL_FILE=pool,
            MEM="1024",
            RESULTS=os.path.join(d, "results"),
        )
        env.update(env_extra or {})
        r = subprocess.run(
            ["bash", "-c", f'source "{self.SH}" && {snippet}'],
            capture_output=True, text=True, env=env, timeout=60)
        return r, pool

    def test_pool_grows_to_mem_derived_need(self):
        r, pool = self._bash("ensure_hugepage_pool")
        self.assertEqual(r.returncode, 0, r.stderr)
        # 1024 MiB / 2 MiB per page = 512 per VM; x4 for backing + prepare
        # VM + two teardown-overlapping clones.
        self.assertEqual(open(pool).read().strip(), "2048")

    def test_pool_need_scales_with_mem(self):
        r, pool = self._bash("ensure_hugepage_pool", {"MEM": "2050"})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(open(pool).read().strip(), "4100")

    def test_pool_grow_failure_is_fatal(self):
        # sudo "succeeds" but the write never lands (the kernel could not
        # deliver contiguous pages): the phase must stop, not measure a
        # hugepage cell on a starved pool.
        r, _ = self._bash('sudo() { :; }; ensure_hugepage_pool')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("pool only", r.stdout + r.stderr,
                      "must fail via the reread check, not incidentally")

    def test_snapshot_state_reads_metadata(self):
        for js, want in (("true", "huge"), ("false", "normal")):
            r, _ = self._bash("hugepage_snapshot_state", hugepages=js)
            self.assertEqual(r.stdout.strip(), want, r.stderr)
        r, _ = self._bash('TAG=missing-tag hugepage_snapshot_state')
        self.assertEqual(r.stdout.strip(), "unknown")

    def test_cmd_run_refuses_file_backend_on_hugepage_snapshot(self):
        r, _ = self._bash("BACKEND=file cmd_run")
        self.assertEqual(r.returncode, 2, f"stdout={r.stdout} stderr={r.stderr}")
        self.assertIn("BACKEND=uffd", r.stdout + r.stderr)


class HugepageGuardsRound2(unittest.TestCase):
    """Second codex round on the hugepage guards.

    Watched red 2026-08-14 before the fix: `snapshot_memory_mib` did not
    exist; `ensure_hugepage_pool` accepted odd MEM (2051 -> grew the pool,
    exit 0) and sized from ambient MEM rather than the snapshot's recorded
    memory_mib; `cmd_verify` treated unknown hugepage state as non-huge and
    sailed on toward serve (fail-open when jq or the config is missing).
    """

    SH = HugepageGuards.SH
    _bash = HugepageGuards._bash

    def test_pool_sizes_from_snapshot_memory(self):
        # The fixture snapshot records memory_mib=4096: verify/run must size
        # from THAT, not from the caller's ambient MEM (default 1024) — an
        # 8 GiB golden verified after a reboot would otherwise provision a
        # quarter of what the first clone needs.
        r, pool = self._bash(
            'ensure_hugepage_pool "$(snapshot_memory_mib)"')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(open(pool).read().strip(), "8192")

    def test_pool_rejects_odd_mem(self):
        r, pool = self._bash( "ensure_hugepage_pool",
                                   {"MEM": "2051"})
        self.assertNotEqual(r.returncode, 0,
                            "odd MEM must fail before touching the pool "
                            "(fcvm rejects it AFTER we would have reserved "
                            "gigabytes)")
        self.assertEqual(open(pool).read().strip(), "100",
                         "pool must be untouched on rejection")

    def test_verify_fails_closed_on_unknown_state(self):
        # jq missing / config unreadable => unknown. Proceeding as if
        # non-huge is exactly the fail-open shape this repo bans.
        r, _ = self._bash( "TAG=missing-tag cmd_verify")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("hugepage state", r.stdout + r.stderr)


class SnapshotLockHeldAcrossRun(unittest.TestCase):
    """Codex P1 (PR #815): the backend classification must happen under the
    snapshot generation lock and the lock must stay held through the driver
    handoff — otherwise another fcvm command can replace $TAG between the
    hugepage_snapshot_state read and the run, and a same-shaped hugepage
    generation slips past the BACKEND=file refusal (mislabeled data).

    Watched red 2026-08-14: the exclusive-flock probe inside the stub driver
    SUCCEEDED (no lock held during the run) and the pool-lease probe likewise.
    """

    SH = HugepageGuards.SH
    _bash = HugepageGuards._bash

    def test_fcvm_data_dir_is_aligned_with_data_root(self):
        # fcvm resolves its snapshot paths (and therefore the generation
        # lock file) from FCVM_DATA_DIR; reqbench derives DATA_ROOT
        # independently. Without exporting the alignment, the two processes
        # can lock DIFFERENT files and the generation lock is theater
        # (CodeRabbit round 2, PR #815).
        r, _ = self._bash('echo "FCVM_DATA_DIR=[$FCVM_DATA_DIR]"')
        self.assertIn("FCVM_DATA_DIR=[", r.stdout)
        self.assertNotIn("FCVM_DATA_DIR=[]", r.stdout,
                         "reqbench.sh must export FCVM_DATA_DIR=$DATA_ROOT")

    def test_generation_lock_held_shared_through_driver(self):
        # The stub driver tries to take the generation lock EXCLUSIVE; if
        # cmd_run holds it SHARED across the handoff, that must fail.
        r, _ = self._bash(
            'mkdir -p "$RESULTS/logs"; '
            'probe() { flock -x -n "$DATA_ROOT/snapshots/$TAG.lock" true '
            '  && echo LOCK-FREE || echo LOCK-HELD; }; '
            'export -f probe; '
            'REQBENCH_DRIVER_HOOK="probe" cmd_run',
            {"BACKEND": "uffd", "UFFD_MODE": "minor"})
        self.assertIn("LOCK-HELD", r.stdout + r.stderr)

    def test_pool_lease_held_shared_through_phase(self):
        r, _ = self._bash(
            'mkdir -p "$RESULTS/logs"; '
            'probe() { flock -x -n "$DATA_ROOT/hugepage-pool.lock" true '
            '  && echo POOL-FREE || echo POOL-HELD; }; '
            'export -f probe; '
            'REQBENCH_DRIVER_HOOK="probe" cmd_run',
            {"BACKEND": "uffd", "UFFD_MODE": "minor"})
        self.assertIn("POOL-HELD", r.stdout + r.stderr)

    def test_pool_grow_respects_exclusive_holder(self):
        # While another process holds the pool lock exclusive, a grow must
        # not proceed concurrently: bounded wait, then fail closed.
        r, pool = self._bash(
            'touch "$DATA_ROOT/hugepage-pool.lock"; '
            'exec 9<>"$DATA_ROOT/hugepage-pool.lock"; flock -x 9; '
            'HUGEPAGE_POOL_LOCK_WAIT=1 ensure_hugepage_pool')
        self.assertNotEqual(r.returncode, 0,
                            "grow must not race a concurrent pool owner")
        self.assertEqual(open(pool).read().strip(), "100",
                         "pool must be untouched while another owner holds it")


class ExecArmIsOptional(unittest.TestCase):
    """The retired exec arm must not be REQUIRED for publication.

    Red/green evidence is recorded per test (the first driver version was
    vacuous — it exited at backend selection before the arm check; codex
    caught it on #816 and the rewrite below drives the real main). The measured
    justification for making exec optional, from run
    reqbench-20260814-022254-uffd: 95% of noop reps that FOLLOW an exec rep
    land in a +17 ms slow mode (59/62), vs 15% after cdp-fast — the in-guest
    Python driver faults a large, run-varying page set that pollutes the
    shared prefetch working set and destabilizes the noop drift canary. No
    published claim rests on exec (it was retired for its ~230 ms in-guest
    driver startup), so requiring it only forces the arm that corrupts the
    baseline into every publication run. Keeping it ALLOWED preserves the
    continuity arm for anyone who asks for it.
    """

    def test_driver_accepts_publication_arms_without_exec(self):
        """Drive the real main() to completion with --arms noop,cdp-fast.

        The earlier version of this test exited at the backend-selection
        check (no --serve-pid/--snapshot-tag) and never reached the arm
        validation it claimed to cover, so restoring the old exec
        requirement left it green (found by codex review on #816). This
        version reaches the full schedule. Watched red 2026-08-14 with the
        exec-requirement hunk reverse-applied: SystemExit(2) from p.error
        "publication runs require exec, noop, and at least one CDP arm" —
        the exact check under test — then green again with it restored.
        """
        with tempfile.TemporaryDirectory() as data_root:
            SnapshotGenerationIdentity._write_generation(
                data_root, "22222222-2222-4222-8222-222222222222",
            )
            snapshot = SnapshotGenerationIdentity.SNAPSHOT
            runtime_bundle = os.path.join(data_root, "runtime")
            os.makedirs(runtime_bundle)
            manifest_path = os.path.join(runtime_bundle, "MANIFEST.sha256")
            with open(manifest_path, "w") as target:
                target.write("sealed runtime fixture\n")
            fcvm = os.path.join(runtime_bundle, "fcvm")
            with open(fcvm, "w") as target:
                target.write("#!/bin/sh\nexit 0\n")
            os.chmod(fcvm, 0o755)

            def record(arm, rep):
                return {
                    "arm": arm,
                    "rep": rep,
                    "ok": True,
                    "blocking_ms": 1.0,
                    "wall_ms": 1.0,
                    "teardown": {},
                }

            def run_exec(_args, rep):
                raise AssertionError(
                    "exec arm ran despite --arms noop,cdp-fast",
                )

            def run_noop(_args, rep):
                return record("noop", rep)

            def run_cdp(_args, rep, fast, probe=None):
                return record("cdp-fast" if fast else "cdp", rep)

            saved = {
                "HERE": reqbench.HERE,
                "run_exec_request": reqbench.run_exec_request,
                "run_noop_request": reqbench.run_noop_request,
                "run_cdp_request": reqbench.run_cdp_request,
                "sha256_file": reqbench.sha256_file,
                "harness_sha256": reqbench.harness_sha256,
                "command_text": reqbench.command_text,
                "pending_signal": reqbench._pending_harness_signal,
                "argv": sys.argv,
                "sigint": signal.getsignal(signal.SIGINT),
                "sigterm": signal.getsignal(signal.SIGTERM),
            }
            env_updates = {
                "REQBENCH_RUNTIME_BUNDLE": runtime_bundle,
                "REQBENCH_SOURCE_REVISION": "e" * 40,
                "REQBENCH_GUARD_LOADAVG1": "0.1",
                "REQBENCH_GUARD_VM_PROCESSES": "0",
                "REQBENCH_QUIET_LOADAVG1_LIMIT": "2.0",
                "REQBENCH_QUIET_GUARD": "1",
                "ALLOW_BUSY": "0",
            }
            saved_env = {key: os.environ.get(key) for key in env_updates}
            out_dir = os.path.join(data_root, "results")
            exact_hashes = {
                os.path.realpath(fcvm): "c" * 64,
                os.path.realpath(manifest_path): "d" * 64,
            }
            rc = None
            try:
                os.environ.update(env_updates)
                reqbench.HERE = runtime_bundle
                reqbench.run_exec_request = run_exec
                reqbench.run_noop_request = run_noop
                reqbench.run_cdp_request = run_cdp
                reqbench.sha256_file = (
                    lambda path: exact_hashes[os.path.realpath(path)]
                )
                reqbench.harness_sha256 = lambda: "f" * 64
                reqbench.command_text = lambda _argv: "fcvm fixture"
                reqbench._pending_harness_signal = 0
                sys.argv = [
                    "reqbench.py",
                    "--snapshot-tag", snapshot,
                    "--snapshot-name", snapshot,
                    "--url", "http://fixture/medium.html",
                    "--arms", "noop,cdp-fast",
                    "--reps", "1",
                    "--warmup", "0",
                    "--image", "localhost/chromium-bench-req",
                    "--image-id", "sha256:" + "b" * 64,
                    "--network-mode", "rootless",
                    "--cpu", "2",
                    "--memory-mib", "1024",
                    "--fcvm", fcvm,
                    "--data-root", data_root,
                    "--out-dir", out_dir,
                    "--run-id", "2" * 32,
                ]
                rc = reqbench.main()
            finally:
                reqbench.HERE = saved["HERE"]
                reqbench.run_exec_request = saved["run_exec_request"]
                reqbench.run_noop_request = saved["run_noop_request"]
                reqbench.run_cdp_request = saved["run_cdp_request"]
                reqbench.sha256_file = saved["sha256_file"]
                reqbench.harness_sha256 = saved["harness_sha256"]
                reqbench.command_text = saved["command_text"]
                reqbench._pending_harness_signal = saved["pending_signal"]
                sys.argv = saved["argv"]
                signal.signal(signal.SIGINT, saved["sigint"])
                signal.signal(signal.SIGTERM, saved["sigterm"])
                for key, value in saved_env.items():
                    if value is None:
                        os.environ.pop(key, None)
                    else:
                        os.environ[key] = value

            self.assertEqual(rc, 0)
            with open(os.path.join(out_dir, "reqbench.jsonl")) as source:
                records = [json.loads(line) for line in source]
            self.assertEqual(records[0]["kind"], "meta")
            self.assertEqual(
                {row["arm"] for row in records[1:]},
                {"cdp-fast", "noop"},
            )

    def test_analyzer_accepts_schedule_without_exec(self):
        """Drive the analyzer's REAL arm rule, not its source text.

        The previous version probed a helper that did not exist and always
        fell back to scanning reqanalyze.py for one exact string — a rewritten
        exec requirement with different wording would have passed (CodeRabbit
        finding on #816). _validate_arms is the rule _validate_schedule now
        calls; an exec-less publication schedule must produce no errors, and
        the negative control proves this test exercises the rule at all.
        """
        import reqanalyze

        errors = []
        reqanalyze._validate_arms(["noop", "cdp"], "run", errors)
        self.assertEqual(
            errors, [],
            "an exec-less noop+cdp schedule must validate cleanly",
        )

        # Negative control: the rule still rejects a schedule missing its
        # REQUIRED arms — proving the helper under test is the live rule,
        # not a stub.
        control = []
        reqanalyze._validate_arms(["cdp"], "run", control)
        self.assertTrue(
            control, "a noop-less schedule must be rejected by the same rule"
        )

    def test_the_analyzer_accepts_an_html_publication_schedule(self):
        """Both sides of the harness must agree on what a CDP arm is.

        The producer's publication_arms_ok learned that html is CDP-class,
        but this validator still spelled the rule as {"cdp","cdp-fast"} —
        so `--arms noop,html` would run its full 200+ reps and then be
        rejected here as an invalid schedule, paying for a campaign that
        could never publish (codex finding on #836).
        """
        import reqanalyze

        errors = []
        reqanalyze._validate_arms(["noop", "html"], "run", errors)
        self.assertEqual(
            errors, [],
            "noop+html is a publication schedule: html pays the CDP handshake",
        )

        # The producer and the analyzer must not drift apart again.
        import reqbench

        self.assertTrue(reqbench.publication_arms_ok(["noop", "html"]))
        for arm in sorted(reqbench.CDP_CLASS_ARMS):
            with self.subTest(arm=arm):
                side = []
                reqanalyze._validate_arms(["noop", arm], "run", side)
                self.assertEqual(
                    side, [],
                    f"producer calls {arm} CDP-class; the analyzer must agree",
                )


class CorpusMixUrls(unittest.TestCase):
    """Multi-URL runs for the corpus arm (Cloudflare 14-URL mix).

    Watched red 2026-08-14: --url with a comma list was passed through as one
    junk URL (no cycling, no per-record url, no warmup floor), and the
    analyzer's schedule validation rejected any record whose render.url
    differed from the single meta.url.
    """

    def _parse(self, extra):
        argv = sys.argv
        sys.argv = ["reqbench.py", "--out-dir", "/tmp"] + extra
        import io as _io
        from contextlib import redirect_stderr
        err = _io.StringIO()
        try:
            with self.assertRaises(SystemExit) as cm:
                with redirect_stderr(err), redirect_stdout(io.StringIO()):
                    reqbench.main()
            return cm.exception.code, err.getvalue()
        finally:
            sys.argv = argv

    def test_urls_helper_cycles_deterministically(self):
        urls = reqbench.parse_urls("http://a/,http://b/,http://c/")
        self.assertEqual(urls, ["http://a/", "http://b/", "http://c/"])
        self.assertEqual([reqbench.url_for_rep(urls, r) for r in range(5)],
                         ["http://a/", "http://b/", "http://c/",
                          "http://a/", "http://b/"])

    def test_mix_requires_warmup_of_two_cycles(self):
        # A mix trains the prefetch working set during its first cycle; the
        # baseline is not stationary until every URL has run. Fail closed
        # when warmup cannot cover two full cycles.
        rc, err = self._parse(["--url", "http://a/,http://b/,http://c/",
                               "--arms", "noop,cdp", "--warmup", "2",
                               "--serve-pid", "7"])
        self.assertEqual(rc, 2)
        self.assertIn("warmup", err.lower())

    def test_analyzer_accepts_declared_url_set(self):
        import reqanalyze
        src = open(os.path.join(HERE, "reqanalyze.py")).read()
        body = src.split("def _validate_schedule")[1]
        self.assertIn('meta.get("urls")', body,
                      "schedule validation must accept a declared URL set, "
                      "not only a single meta.url")


class PortProbeResolution(unittest.TestCase):
    """wait_port must measure readiness on a fine grid.

    Watched red 2026-08-14: with the 1ms x1.5 backoff capped at 20 ms, probe
    attempts land ~17-20 ms apart past the ramp, so a port that opens at
    45 ms was reported as ~57 ms — readiness figures sat on the probe grid
    rather than on true readiness, and a small real teardown-adjacency
    effect near an attempt boundary read as a clean bimodal restore floor
    until the fine-grid gated run (reqbench-20260814-035757) showed
    readiness is unimodal.
    """

    def test_readiness_is_resolved_within_3ms(self):
        import socket as _socket
        import threading

        srv = _socket.socket()
        srv.bind(("127.0.0.1", 0))
        port = srv.getsockname()[1]
        ready_at_s = 0.045

        def open_late():
            time.sleep(ready_at_s)
            srv.listen(1)

        t = threading.Thread(target=open_late)
        t.start()
        try:
            measured = reqbench.wait_port(
                f"127.0.0.1:{port}", time.monotonic() + 10.0)
        finally:
            t.join()
            srv.close()
        self.assertGreaterEqual(measured, ready_at_s * 1000 - 1)
        self.assertLessEqual(
            measured, ready_at_s * 1000 + 3.5,
            f"wait_port reported {measured:.1f} ms for a port ready at "
            f"{ready_at_s * 1000:.0f} ms: the probe grid is too coarse")


class GuestDnsKnob(unittest.TestCase):
    """GUEST_DNS must reach the golden's podman prepare as --dns.

    Watched red 2026-08-14: the knob did not exist and the prepare argv
    carried no --dns.
    """

    SH = HugepageGuards.SH

    def test_guest_dns_reaches_prepare_argv(self):
        src = open(self.SH).read()
        self.assertIn('GUEST_DNS="${GUEST_DNS:-}"', src)
        self.assertIn('--dns "$GUEST_DNS"', src)


class PublicationArmSets(unittest.TestCase):
    """html is a CDP-class arm on BOTH sides of the harness.

    The analyzer classified html as CDP-class (is_cdp_class) while the
    producer still spelled the requirement inline as {"cdp","cdp-fast"}, so a
    legitimate `--arms noop,html` publication run was refused with "requires
    at least one CDP arm". Found by running exactly that during the corpus
    campaign, 2026-08-15.
    """

    def test_noop_plus_html_is_a_publication_set(self):
        self.assertTrue(reqbench.publication_arms_ok(["noop", "html"]))

    def test_the_other_cdp_class_arms_still_qualify(self):
        self.assertTrue(reqbench.publication_arms_ok(["noop", "cdp"]))
        self.assertTrue(reqbench.publication_arms_ok(["noop", "cdp-fast"]))
        self.assertTrue(
            reqbench.publication_arms_ok(["noop", "cdp", "cdp-fast", "html"])
        )

    def test_a_rendering_arm_is_required(self):
        """noop alone measures no render, so it cannot back a published cell."""
        self.assertFalse(reqbench.publication_arms_ok(["noop"]))
        self.assertFalse(reqbench.publication_arms_ok(["noop", "exec"]))

    def test_the_drift_canary_is_required(self):
        """Without noop there is no baseline to judge drift against."""
        self.assertFalse(reqbench.publication_arms_ok(["cdp"]))
        self.assertFalse(reqbench.publication_arms_ok(["html", "cdp-fast"]))


class CorpusServeAnswerIp(unittest.TestCase):
    """--answer-ip must fail at startup, not by silently dropping A queries.

    inet_aton runs inside the DNS responder's broad exception handler, so a
    malformed address left every server running while every A query vanished
    without a trace.
    """

    def test_malformed_answer_ip_exits_before_serving(self):
        proc = subprocess.run(
            [
                sys.executable,
                os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpus_serve.py"),
                "--answer-ip",
                "not-an-ip",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("answer-ip", proc.stderr)
        self.assertNotIn(
            "wildcard DNS", proc.stdout,
            "the DNS responder must never start under an unusable answer address",
        )


class HtmlArmAndPrewire(unittest.TestCase):
    """The html op and target-prewiring, validated through the FULL analyzer.

    Both features change what a valid record looks like, so the tests build a
    complete clean dataset, transform it, and hold the analyzer to zero
    metadata errors — then break one field and demand the specific error, so
    the validation under test is proven live rather than skipped.
    """

    @staticmethod
    def _load_errors(path):
        return [
            error
            for dataset in reqanalyze.load([path])
            for error in dataset["metadata_errors"]
        ]

    @staticmethod
    def _rows(path):
        with open(path) as source:
            return [json.loads(line) for line in source]

    @staticmethod
    def _write(path, rows):
        with open(path, "w") as target:
            target.write("\n".join(json.dumps(row) for row in rows) + "\n")

    def _dataset_with_html_arm(self, path, reps=6):
        """Clean dataset whose exec arm is rewritten as an html arm."""
        AnalyzerAvailability._write_clean_backend(path, "file", reps, 384.0)
        rows = self._rows(path)
        cdp_by_key = {
            (row["rep"], row["warmup"]): row
            for row in rows
            if row.get("arm") == "cdp"
        }
        for row in rows:
            if row.get("kind") == "meta":
                row["arms"] = ["html" if a == "exec" else a for a in row["arms"]]
                continue
            if row.get("arm") != "exec":
                continue
            template = cdp_by_key[(row["rep"], row["warmup"])]
            row["arm"] = "html"
            row["record_id"] = row["record_id"].replace(":exec:", ":html:")
            row["teardown"] = dict(template["teardown"])
            row["endpoint"] = template["endpoint"]
            render = json.loads(json.dumps(template["render"]))
            stages = render["stages"]
            for gone in ("screenshot_ms", "decode_ms"):
                stages.pop(gone, None)
            stages["extract_ms"] = 1.0
            for gone in ("image_bytes", "image_sha256", "width", "height"):
                render.pop(gone, None)
            render["html_bytes"] = 2048
            render["html_sha256"] = "a" * 64
            row["render"] = render
            # The html record carries every per-record metric a cdp record
            # does; identity fields stay the (renamed) exec record's own.
            identity = {"arm", "rep", "warmup", "record_id", "name", "run_id",
                        "render", "teardown", "endpoint"}
            for metric, value in template.items():
                if metric not in identity and metric not in row:
                    row[metric] = value
        self._write(path, rows)

    def test_short_html_arm_fails_the_sample_size_gate(self):
        """A mixed run must not publish while its html arm is short.

        expected_cdp_arms matched only names starting with "cdp", so a run
        whose cdp arms reached 200 passed publication with the html arm at
        any count — an html latency published from a sample the gate never
        examined.
        """
        from unittest import mock

        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._dataset_with_html_arm(src, reps=200)
            rows = self._rows(src)
            victim = next(
                r for r in rows
                if r.get("arm") == "html" and r.get("warmup") is False
            )
            rows.remove(victim)
            self._write(src, rows)
            buf = io.StringIO()
            with (
                mock.patch.object(
                    reqanalyze, "median_ci", AnalyzerAvailability._fast_median_ci
                ),
                mock.patch.object(
                    reqanalyze,
                    "hodges_lehmann_shift",
                    AnalyzerAvailability._fast_shift,
                ),
                redirect_stdout(buf),
            ):
                rc = reqanalyze.main_with(["--json-out", dst, src])
            with open(dst) as f:
                out = json.load(f)
            sample = out["gate"]["cdp_sample_size"]
            self.assertEqual(
                sample["measured_non_warmup_attempts_per_arm"].get("html"), 199
            )
            self.assertIs(sample["passed"], False)
            self.assertIs(out["publishable"], False)
            self.assertEqual(rc, 5, buf.getvalue())

    def test_html_arm_gets_stage_decomposition(self):
        """analysis.json must carry the html arm's per-stage metrics.

        The stage loop keyed on startswith("cdp") dropped every connection
        and extraction stage for html even though the arm pays the same
        per-request CDP handshake; only aggregate blocking and wall time
        survived into analysis.json.
        """
        from unittest import mock

        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._dataset_with_html_arm(src)
            buf = io.StringIO()
            with (
                mock.patch.object(
                    reqanalyze, "median_ci", AnalyzerAvailability._fast_median_ci
                ),
                mock.patch.object(
                    reqanalyze,
                    "hodges_lehmann_shift",
                    AnalyzerAvailability._fast_shift,
                ),
                redirect_stdout(buf),
            ):
                reqanalyze.main_with(["--json-out", dst, src, "--no-gate"])
            with open(dst) as f:
                out = json.load(f)
            html_arm = out["arms"]["html"]
            for metric in ("connect_total_ms", "navigate_ms", "extract_ms", "total_ms"):
                self.assertIn(metric, html_arm, f"html arm must publish {metric}")
            self.assertNotIn(
                "screenshot_ms", html_arm,
                "html renders no screenshot; a summary here means the arm was "
                "pooled with the wrong stage list",
            )

    def test_html_arm_dataset_validates_cleanly(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "html.jsonl")
            self._dataset_with_html_arm(path)
            self.assertEqual(self._load_errors(path), [])

    def test_html_record_without_payload_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "html.jsonl")
            self._dataset_with_html_arm(path)
            rows = self._rows(path)
            victim = next(
                r for r in rows
                if r.get("arm") == "html" and r.get("warmup") is False
            )
            del victim["render"]["html_bytes"]
            self._write(path, rows)
            errors = self._load_errors(path)
            self.assertTrue(
                any("html_bytes" in e for e in errors),
                f"dropping html_bytes must be caught, got: {errors[:3]}",
            )

    def _dataset_with_prewire(self, path):
        """Clean dataset in prewire mode: discovery on one warmup, pinned after."""
        AnalyzerAvailability._write_clean_backend(path, "file", 6, 384.0)
        rows = self._rows(path)
        first_warmup_seen = False
        for row in rows:
            if row.get("kind") == "meta":
                row["ws_url_prewired"] = True
                continue
            render = row.get("render")
            if not isinstance(render, dict):
                continue
            if row.get("warmup") and not first_warmup_seen:
                first_warmup_seen = True
                render["target_prewired"] = False  # the discovery rep
            else:
                render["target_prewired"] = True
                render["stages"]["resolve_ms"] = 0.0
        self._write(path, rows)

    def test_prewire_discovery_warmup_is_tolerated(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "prewire.jsonl")
            self._dataset_with_prewire(path)
            self.assertEqual(self._load_errors(path), [])

    def test_prewire_mismatch_on_measured_rep_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "prewire.jsonl")
            self._dataset_with_prewire(path)
            rows = self._rows(path)
            victim = next(
                r for r in rows
                if isinstance(r.get("render"), dict) and r.get("warmup") is False
            )
            victim["render"]["target_prewired"] = False
            self._write(path, rows)
            errors = self._load_errors(path)
            self.assertTrue(
                any("prewire" in e for e in errors),
                f"an unprewired MEASURED rep must be caught, got: {errors[:3]}",
            )


class PerRequestPrivateDirty(unittest.TestCase):
    """The delta a request costs, sampled where it can still be sampled.

    A clone starts as a view of the shared snapshot, so Private_Dirty is what it
    PRIVATISED: pages written, not read. It is read alive, immediately before
    the kill, because smaps_rollup dies with the address space and cannot be
    recovered from a zombie the way CPU can.

    Reported on the same record as the latency, so a memory/latency frontier is
    one measurement rather than a join across two harnesses on two goldens.
    """

    def test_a_live_process_reports_private_dirty(self) -> None:
        got = reqbench.proc_private_dirty_kb(os.getpid())
        self.assertIsNotNone(
            got.get("private_dirty_kb"),
            f"could not sample this process: {got.get('unavailable')}",
        )
        self.assertGreater(got["private_dirty_kb"], 0, got)

    def test_an_unreadable_process_reports_a_reason_not_a_zero(self) -> None:
        """A zero with no uncertainty is a claim. An unreadable file does not
        support one, and reporting 0 KiB would silently understate every clone
        whose sample failed."""
        got = reqbench.proc_private_dirty_kb(999_999_999)
        self.assertIsNone(got.get("private_dirty_kb"), got)
        self.assertTrue(got.get("unavailable"), "no reason given for the missing sample")

    def test_a_partial_sample_totals_none_not_a_smaller_number(self) -> None:
        """Calls the production rule, not a copy of it.

        The previous version of this test inlined the expression and asserted on
        its own arithmetic, so deleting every out[...] assignment in reqbench
        left the suite green. It proved that Python sums the way Python sums.

        The case that matters: firecracker exits between the pin and the read.
        The survivors total a few MiB against its few hundred, and a number is
        what a reader takes at face value.
        """
        self.assertIsNone(
            reqbench.private_dirty_total_kb(
                {"firecracker": {"private_dirty_kb": None, "unavailable": "exited"},
                 "pasta": {"private_dirty_kb": 2048}}
            ),
            "a partial sum was published as an ordinary number",
        )
        self.assertIsNone(reqbench.private_dirty_total_kb({}), "an empty set has no total")
        self.assertEqual(
            reqbench.private_dirty_total_kb(
                {"firecracker": {"private_dirty_kb": 300_000}, "pasta": {"private_dirty_kb": 2048}}
            ),
            302_048,
            "a complete sample must still total",
        )
