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
            data = os.path.join(d, "data")
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
            data = os.path.join(d, "data")
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
            f.write(json.dumps({"kind": "meta", "seed": 1, "arms": ["exec", "cdp"]}) + "\n")
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
            for i in range(27, 30):
                f.write(json.dumps({
                    "arm": "cdp", "rep": i, "ok": False,
                    "error": "WsClosed: connection closed mid-frame",
                    "blocking_ms": 5250.0, "wall_ms": 5300.0,
                    "teardown": {"mode": "fast", "all_gone": False,
                                 "reap_wall_ms": 60000.0, "teardown_total_ms": 60000.0},
                }) + "\n")

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
