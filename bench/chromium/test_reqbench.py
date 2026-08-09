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
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from contextlib import redirect_stdout

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


class TeardownFastReapGuard(unittest.TestCase):
    """teardown_fast must NOT delete the tracking record of a VM it failed to kill.

    RED BEFORE THE FIX: `all_gone` was computed and stored, then the state file was
    removed and the data dir rmtree'd unconditionally, so a parent whose child has
    no pdeathsig produced
        all_gone: False, disk_reaped: [state.json, data/], state exists: False
    with the survivor still in /proc afterwards.
    """

    def test_survivor_blocks_the_reap_and_raises(self):
        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "vm-test.json")
            # Shaped exactly as production builds it — `<data_root>/vm-disks/<vm_id>`
            # — because `reap_disk` now refuses anything that is not strictly below
            # a `vm-disks` root (see ReapDiskPathGuard).
            data = os.path.join(d, "vm-disks", "vm-test")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-test", "pid": 1}, f)
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
                    reqbench.teardown_fast(p.pid, state, data, 0.3)

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
            state = os.path.join(d, "vm-test.json")
            data = os.path.join(d, "vm-disks", "vm-test")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-test"}, f)
            with open(state + ".lock", "w") as f:
                f.write("")
            os.makedirs(data)
            p = subprocess.Popen(["sleep", "300"])
            out = reqbench.teardown_fast(p.pid, state, data, 5.0)
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
            state = os.path.join(d, "vm-test.json")
            data = os.path.join(d, "vm-disks", "vm-test")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-test"}, f)
            os.makedirs(data)
            real = shutil.rmtree

            def boom(*_a, **_k):
                raise PermissionError(1, "Operation not permitted")

            p = subprocess.Popen(["sleep", "300"])
            shutil.rmtree = boom
            try:
                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(p.pid, state, data, 5.0)
            finally:
                shutil.rmtree = real
                p.wait(timeout=5)
            self.assertIn("could not reap", str(cm.exception))
            self.assertTrue(os.path.isdir(data), "the un-reaped dir is still there")
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
            os.makedirs(os.path.join(disks, "vm-a"))
            os.makedirs(os.path.join(disks, "vm-b"))
            out: dict = {}
            reaped = reqbench.reap_disk(out, "", os.path.join(disks, ""))
            self.assertTrue(os.path.isdir(os.path.join(disks, "vm-a")),
                            "another VM's disks were deleted")
            self.assertTrue(os.path.isdir(os.path.join(disks, "vm-b")),
                            "another VM's disks were deleted")
            self.assertEqual(reaped, [])
            self.assertTrue(any("vm-disks" in e for e in out.get("disk_errors", [])),
                            f"the refusal must be recorded: {out}")

    def test_a_real_per_vm_dir_is_still_reaped(self):
        """The guard must not disarm the ordinary reap."""
        with tempfile.TemporaryDirectory() as root:
            dd = os.path.join(root, "vm-disks", "vm-a", "disks")
            os.makedirs(dd)
            out: dict = {}
            reaped = reqbench.reap_disk(out, "", os.path.join(root, "vm-disks", "vm-a"))
            self.assertEqual(reaped, [os.path.join(root, "vm-disks", "vm-a")])
            self.assertFalse(os.path.isdir(os.path.join(root, "vm-disks", "vm-a")))
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
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if all(reqbench.proc_stat_fields(k) is None
                       or reqbench.proc_stat_fields(k)[0] in ("Z", "X", "x") for k in kids):
                    break
                time.sleep(0.01)
            alive = [k for k in kids if reqbench.proc_stat_fields(k) is not None
                     and reqbench.proc_stat_fields(k)[0] not in ("Z", "X", "x")]
            self.assertEqual(alive, [], "the survivor must be killed before we abort")
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
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"cat > {state_dir}/vm-red.json <<EOF\n"
                    '{"vm_id": "vm-red", "pid": $$, '
                    '"config": {"network": {"loopback_ip": "127.0.0.1"}}}\n'
                    "EOF\n"
                    "exec sleep 600\n"
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
                state_dir=state_dir, data_root=d, ws_url="",
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
            disks = os.path.join(d, "vm-disks", "vm-red")
            os.makedirs(state_dir)
            os.makedirs(disks)
            marker = os.path.join(disks, "rootfs.raw")
            with open(marker, "w") as f:
                f.write("golden reflink")
            stub = os.path.join(d, "fcvm-stub")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"cat > {state_dir}/vm-red.json <<EOF\n"
                    '{"vm_id": "vm-red", "pid": $$}\n'
                    "EOF\n"
                    "sleep 300 &\n"     # UNARMED child: no pdeathsig, survives the kill
                    "wait\n"
                )
            os.chmod(stub, 0o755)
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=0.6, teardown_timeout=0.4,
                state_dir=state_dir, data_root=d,
            )
            with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                reqbench.run_exec_request(args, 0)
            rec = cm.exception.record
            try:
                self.assertTrue(rec.get("survivors"), f"no survivor list in {rec}")
                self.assertIs(rec.get("disk_reap_skipped"), True)
                self.assertTrue(os.path.exists(marker),
                                "rootfs of a live child must NOT be reaped")
                self.assertTrue(os.path.exists(os.path.join(state_dir, "vm-red.json")),
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

    def test_control_window_does_not_measure_our_own_spin(self):
        p = subprocess.Popen(["sleep", "300"])
        before = self_cpu_ms()
        out = reqbench.teardown_fast(p.pid, "", "", 5.0)
        spent = self_cpu_ms() - before
        p.wait(timeout=5)
        # 50 ms of control window: a spin would put >=45 ms of user time in it.
        # The whole call (control + reclaim sampling) must stay well under that.
        self.assertLess(
            out["control_harness_cpu_ms"],
            5.0,
            f"control window burned {out['control_harness_cpu_ms']:.1f} ms of OUR cpu "
            f"(total call {spent:.1f} ms) — it is spinning, so control_busy_cores "
            f"({out['control_busy_cores']:.2f}) is ambient + the harness",
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
        out = reqbench.teardown_fast(p.pid, "", "", 5.0)
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
            out = reqbench.teardown_fast(p.pid, "", "", 10.0)
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
                deadline = time.monotonic() + 10
                while len(reqbench.children_of(p.pid)) < 2 and time.monotonic() < deadline:
                    time.sleep(0.005)
                kids = reqbench.children_of(p.pid)
                self.assertEqual(len(kids), 2, "parent never forked both children")
                # Precondition: the child that dies FIRST must be SECOND in fork
                # order, or this test is not exercising the ordering defect at all.
                self.assertEqual(
                    [reqbench.proc_comm(k) for k in kids], ["lingersleep", "fastexit"],
                    "fork order is not [linger, fast]; the ordering defect is not exercised",
                )
                # `lingersleep` has no pdeathsig, so it survives and teardown_fast
                # (correctly) refuses to reap and raises. The partial record it
                # carries is what this test inspects.
                with self.assertRaises(reqbench.SurvivedTeardown) as cm:
                    reqbench.teardown_fast(p.pid, "", "", 1.0)
                out = cm.exception.teardown
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
                    json.dump({"vm_id": "vm-mine", "pid": 123456, "name": "mine"}, f)
                os.rename(tmp, target)

            watch = reqbench.DirWatch(d)  # registered BEFORE the writer starts
            th = threading.Thread(target=writer)
            th.start()
            t0 = time.monotonic()
            c0 = self_cpu_ms()
            path, st = reqbench.find_state(d, 123456, time.monotonic() + 10, watch)
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
            with open(stub, "w") as f:
                f.write("#!/bin/bash\nexec sleep 600\n")
            os.chmod(stub, 0o755)
            args = argparse.Namespace(
                fcvm=stub, out_dir=d, url="http://x/", format="jpeg", quality=80,
                snapshot_tag="", serve_pid=1, rust_log="off",
                timeout=0.3, teardown_timeout=0.2,
                state_dir=d, data_root=d,
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
        with open(path, "w") as f:
            f.write(json.dumps({
                "kind": "meta", "seed": 1, "backend": "file",
                "arms": ["exec", "cdp"],
            }) + "\n")
            for i in range(30):
                f.write(json.dumps({
                    "arm": "exec", "rep": i, "ok": True,
                    "blocking_ms": 565.0, "wall_ms": 565.0,
                }) + "\n")
            for i in range(27):
                f.write(json.dumps({
                    "arm": "cdp", "rep": i, "ok": True,
                    "blocking_ms": 384.0, "wall_ms": 455.0,
                    "teardown": {"mode": "fast", "all_gone": True,
                                 "reap_wall_ms": 63.0, "teardown_total_ms": 70.0},
                }) + "\n")
            # THE PRODUCER'S SHAPE, not a convenient one: run_cdp_request nests
            # the whole cdpdrive result under `render` and lifts the label to the
            # top level. The old fixture wrote `error` at the top level ONLY, a
            # schema nothing emitted, so the analyzer test was green against a
            # record the harness never produces.
            for i in range(27, 30):
                f.write(json.dumps({
                    "arm": "cdp", "rep": i, "ok": False,
                    "error": "WsClosed: connection closed mid-frame",
                    "failure_class": "transport", "failure_stage": "navigate",
                    "blocking_ms": 5250.0, "wall_ms": 5300.0,
                    "render": {"ok": False,
                               "error": "WsClosed: connection closed mid-frame",
                               "failure_class": "transport", "stage": "navigate"},
                    "teardown": {"mode": "fast", "all_gone": False,
                                 "reap_wall_ms": 60000.0, "teardown_total_ms": 60000.0},
                }) + "\n")

    @staticmethod
    def _write_clean_backend(path, backend, measured, blocking_ms, noop_values=()):
        arms = ["cdp"] + (["noop"] if noop_values else [])
        with open(path, "w") as f:
            f.write(json.dumps({
                "kind": "meta", "seed": 1, "backend": backend,
                "arms": arms, "reps": measured, "warmup": 2,
            }) + "\n")
            # Warmups are real records but must not satisfy the publication n.
            for i in range(2):
                f.write(json.dumps({
                    "arm": "cdp", "rep": i, "warmup": True, "ok": True,
                    "blocking_ms": blocking_ms, "wall_ms": blocking_ms + 1,
                }) + "\n")
            for i in range(measured):
                f.write(json.dumps({
                    "arm": "cdp", "rep": i, "warmup": False, "ok": True,
                    "blocking_ms": blocking_ms, "wall_ms": blocking_ms + 1,
                }) + "\n")
            for i, value in enumerate(noop_values):
                f.write(json.dumps({
                    "arm": "noop", "rep": i, "warmup": False, "ok": True,
                    "blocking_ms": value, "wall_ms": value + 1,
                }) + "\n")

    @staticmethod
    def _fast_median_ci(xs, *_args, **_kwargs):
        values = sorted(float(x) for x in xs if x is not None)
        if not values:
            return None, None, None, 0
        return reqanalyze.statistics.median(values), values[0], values[-1], len(values)

    def _run_gate_fixture(self, argv):
        # The production bootstrap remains covered elsewhere. These tests target
        # input partitioning and gate truth, so avoid tens of millions of random
        # resamples over deliberately constant 200-record fixtures.
        from unittest import mock
        with mock.patch.object(reqanalyze, "median_ci", self._fast_median_ci):
            return reqanalyze.main_with(argv)

    def test_failed_records_are_counted_per_arm_and_gate_publication(self):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with(["--json-out", dst, src])
            out = json.load(open(dst))
            self.assertEqual(out["arms"]["cdp"]["attempted"], 30)
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
            out = json.load(open(dst))
            self.assertEqual(
                out["arms"]["cdp"]["all_gone_confirmed"], [27, 30],
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
            with open(src, "w") as f:
                f.write(json.dumps({"kind": "meta", "seed": 1}) + "\n")
                for i in range(27):
                    f.write(json.dumps({
                        "arm": "cdp", "rep": i, "ok": True,
                        "blocking_ms": 384.0, "wall_ms": 455.0,
                        "teardown": {"mode": "fast", "all_gone": True,
                                     "reap_wall_ms": 63.0, "teardown_total_ms": 70.0},
                    }) + "\n")
                for i in (27, 28):
                    f.write(json.dumps({
                        "arm": "cdp", "rep": i, "ok": True,
                        "blocking_ms": 384.0, "wall_ms": 455.0,
                        "teardown": {"mode": "fast", "all_gone": None,
                                     "reap_wall_ms": 63.0, "teardown_total_ms": 70.0},
                    }) + "\n")
                f.write(json.dumps({
                    "arm": "cdp", "rep": 29, "ok": True,
                    "blocking_ms": 384.0, "wall_ms": 455.0,
                    "teardown": {"mode": "fast",
                                 "reap_wall_ms": 63.0, "teardown_total_ms": 70.0},
                }) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = reqanalyze.main_with(["--json-out", dst, src, "--no-gate"])
            text = buf.getvalue()
            self.assertIn("** 3 NOT CONFIRMED GONE **", text)
            for rep in (27, 28, 29):
                self.assertIn(f"rep {rep}", text, f"rep {rep} must appear in the dump")
            with open(dst) as f:
                out = json.load(f)
            self.assertEqual(out["arms"]["cdp"]["all_gone_confirmed"], [27, 30])
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
            with open(src, "w") as f:
                f.write(json.dumps({"kind": "meta", "seed": 1}) + "\n")
                for i in range(3):
                    f.write(json.dumps({
                        "arm": "cdp", "rep": i, "ok": False,
                        "blocking_ms": 5250.0, "wall_ms": 5300.0,
                        "render": {"ok": False,
                                   "error": "WsClosed: connection closed mid-frame",
                                   "failure_class": "transport", "stage": "navigate"},
                    }) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with(["--json-out", dst, src, "--no-gate"])
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
            self._synthetic(src)
            buf = io.StringIO()
            with redirect_stdout(buf):
                rc = reqanalyze.main_with([src])
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
            self.assertIs(out["publishable"], True)
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
                noop_values=(10.0, 10.0, 10.0, 100.0, 100.0, 100.0),
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

    def test_per_child_cpu_is_reported_by_name_not_pooled(self):
        """Pooling across children medians a straggler away.

        RED BEFORE THE FIX: reqanalyze appended every child's value to one list and
        discarded the name, so {firecracker 110 ms, holder 0 ms, pasta 0 ms}
        published a median of 0 — the opposite of the finding.
        """
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "r.jsonl")
            dst = os.path.join(d, "r.json")
            with open(src, "w") as f:
                f.write(json.dumps({"kind": "meta", "seed": 1}) + "\n")
                for i in range(5):
                    f.write(json.dumps({
                        "arm": "cdp-fast", "rep": i, "ok": True,
                        "blocking_ms": 372.0, "wall_ms": 1065.0,
                        "teardown": {
                            "mode": "fast", "all_gone": True, "tick_ms": 10.0,
                            "reap_wall_ms": 634.0, "teardown_total_ms": 640.0,
                            "per_child_cpu": {
                                "firecracker": {"reclaim_cpu_ms": 110.0, "complete": True,
                                                "below_resolution": False,
                                                "reclaim_cpu_ms_hi": 130.0},
                                "sleep": {"reclaim_cpu_ms": 0.0, "complete": True,
                                          "below_resolution": True,
                                          "reclaim_cpu_ms_hi": 20.0},
                                "pasta": {"reclaim_cpu_ms": 0.0, "complete": True,
                                          "below_resolution": True,
                                          "reclaim_cpu_ms_hi": 20.0},
                            },
                        },
                    }) + "\n")
            buf = io.StringIO()
            with redirect_stdout(buf):
                reqanalyze.main_with(["--json-out", dst, src])
            out = json.load(open(dst))
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
            state = os.path.join(d, "vm-test.json")
            data = os.path.join(d, "vm-disks", "vm-test")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-test"}, f)
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
            state = os.path.join(d, "vm-test.json")
            data = os.path.join(d, "vm-disks", "vm-test")
            with open(state, "w") as f:
                json.dump({"vm_id": "vm-test"}, f)
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
            os.makedirs(os.path.join(d, "vm-disks", "vm-red"))
            stub = os.path.join(d, "fcvm-stub")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    f"cat > {state_dir}/vm-$$.json <<EOF\n"
                    '{"vm_id": "vm-red", "pid": $$, '
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

    def _env(self, d, **extra):
        binx = os.path.join(d, "bin")
        os.makedirs(binx, exist_ok=True)
        env = dict(os.environ)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            RESULTS=os.path.join(d, "results"),
            STATE_DIR=os.path.join(d, "state"),
            ALLOW_BUSY="1",
            RUNID="test",
        )
        env.update(extra)
        os.makedirs(env["STATE_DIR"], exist_ok=True)
        return env, binx

    def _write(self, path, body):
        with open(path, "w") as f:
            f.write(body)
        os.chmod(path, 0o755)

    def test_fcvm_no_snapshot_survives_sudo(self):
        """RED BEFORE THE FIX: `FCVM_NO_SNAPSHOT=1 $SUDO env RUST_LOG=... fcvm ...`
        binds the assignment to SUDO, whose default env_reset drops it. RUST_LOG
        survives only because it rides the `env` AFTER sudo. The sibling harness
        already does it right (bench.sh: `sudo env FCVM_NO_SNAPSHOT=1 RUST_LOG=...`),
        so this is a local divergence, not house style. With the variable dropped,
        `src/commands/podman/mod.rs` takes the snapshot-cache path and the phase
        documented as `golden: cold boot` would RESTORE from a stale cached
        snapshot and then snapshot THAT as $TAG — contaminating every arm.
        Observed with an env_reset-emulating sudo stub: FCVM_NO_SNAPSHOT=<unset>.
        """
        with tempfile.TemporaryDirectory() as d:
            env, binx = self._env(d)
            seen = os.path.join(d, "seen.txt")
            self._write(os.path.join(binx, "sudo"),
                        '#!/bin/bash\nexec env -i PATH="$PATH" HOME="$HOME" "$@"\n')
            fcvm = os.path.join(d, "fcvm")
            self._write(fcvm, f"""#!/bin/bash
case "$1 $2" in
  "podman run")
      echo "FCVM_NO_SNAPSHOT=${{FCVM_NO_SNAPSHOT:-<unset>}}" >> {seen}
      echo CHROMIUM_BENCH_READY; sleep 30 ;;
  "snapshot create") echo created ;;
  "snapshots delete") ;;
  "ls --json")
      if [ "$3" = "--pid" ]; then
          echo '[{{"pid": 4242, "health_status": "healthy"}}]'
      else
          echo '[{{"name": "cb-req-g-test", "pid": 4242}}]'
      fi ;;
esac
""")
            r = subprocess.run([self.SH, "golden"], env=dict(env, SUDO="sudo", FCVM=fcvm),
                               capture_output=True, text=True, timeout=120)
            got = open(seen).read() if os.path.exists(seen) else "<no invocation>"
            self.assertIn("FCVM_NO_SNAPSHOT=1", got,
                          f"the assignment did not survive sudo: {got}\n{r.stderr[-800:]}")

    def _run_stub(self, d, backend, analyzer_rc=0):
        env, binx = self._env(d, BACKEND=backend, REPS="1", WARMUP="0")
        argv = os.path.join(d, "argv.log")
        pyargv = os.path.join(d, "pyargv.log")
        fcvm = os.path.join(d, "fcvm")
        self._write(fcvm, f"""#!/bin/bash
echo "$@" >> {argv}
if [ "$1 $2" = "snapshot serve" ]; then
    echo "Serve PID: $$"; echo "Waiting for VMs"; sleep 30
fi
""")
        self._write(os.path.join(binx, "python3"),
                    f'''#!/bin/bash
echo "$@" >> {pyargv}
case "$1" in
  *reqanalyze.py) exit {analyzer_rc} ;;
esac
exit 0
''')
        self._write(os.path.join(binx, "podman"),
                    '#!/bin/bash\necho sha256:test-image\n')
        r = subprocess.run([self.SH, "run"], env=dict(env, FCVM=fcvm),
                           capture_output=True, text=True, timeout=180)
        return (r,
                open(argv).read() if os.path.exists(argv) else "",
                open(pyargv).read() if os.path.exists(pyargv) else "")

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
