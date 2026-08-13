#!/usr/bin/env python3
"""Unit tests for the restore-matrix harness's decision logic.

Every test here exists because a review finding on #805 described broken
behaviour, and this repo closes a defect claim with a test observed RED against
the unfixed tree — not with a fix. Each test names the finding it pins. RED was
observed by restoring bench/restore-matrix.py to 9701c922 (the pre-review
version) and watching the named tests fail; the in-VM lifecycle properties
(pinned kill window, straggler scan, survivor contract) are exercised by the
kill-mid-restore cell itself, which is run in vivo, not simulated here.

stdlib unittest only — there is no pytest in this repo.

Run: python3 -m unittest discover -s bench -p 'test_restore_matrix.py' -v
"""

import importlib.util
import sys
import types
import unittest
from pathlib import Path
from unittest import mock


def load_matrix():
    spec = importlib.util.spec_from_file_location(
        "restore_matrix", Path(__file__).resolve().parent / "restore-matrix.py"
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules["restore_matrix"] = module
    spec.loader.exec_module(module)
    return module


rm = load_matrix()


def make_harness(tmp_out, data_root="/nonexistent-data-root"):
    args = types.SimpleNamespace(
        fcvm="/nonexistent/fcvm", out=str(tmp_out), data_root=data_root
    )
    return rm.Harness(args)


class CellSelectors(unittest.TestCase):
    """CR :322 + codex :322 — the documented selectors selected zero cells and
    the run reported `0/0 cells clean` with exit 0: a command that evaluated
    nothing reported success."""

    def test_unfiltered_matrix_has_all_cells(self):
        self.assertEqual(len(rm.build_cells("")), 11)

    def test_dimension_values_select_cohorts(self):
        self.assertTrue(all(c.backend == "uffd" for c in rm.build_cells("uffd")))
        self.assertTrue(all(c.network == "bridged" for c in rm.build_cells("bridged")))

    def test_c1_does_not_substring_match_c16(self):
        self.assertEqual({c.concurrency for c in rm.build_cells("c1")}, {1})

    def test_unknown_selector_is_rejected(self):
        # `backend` is the DOCUMENTED example that used to select nothing.
        with self.assertRaises(SystemExit):
            rm.build_cells("backend")

    def test_empty_selection_is_rejected(self):
        # Every token valid, intersection empty: bridged cells are all
        # lifecycle=ordinary, so bridged ∪ nothing-with-kill... construct a
        # genuinely empty pick: volumes cells are rootless-only, so
        # `bridged` AND `vol` share no cell — but selectors are a UNION, so
        # force emptiness with a token set whose union is empty is impossible
        # once tokens are valid; the empty case is the unknown-token one above
        # plus an explicitly empty string list.
        with self.assertRaises(SystemExit):
            rm.build_cells(",")


class Percentiles(unittest.TestCase):
    """CR :340 — floor-based rank made p95 of three samples the MEDIAN."""

    def test_p95_of_three_samples_is_the_maximum(self):
        rows = [
            {
                "cell": "x",
                "ok": True,
                "failures": [],
                "phases": [{"total_ms": v, "tcp_verified": True}],
                "ready_ms": [v],
            }
            for v in (100.0, 200.0, 300.0)
        ]
        summary = rm.summarise(rows)
        self.assertIn("guest_p50=  200.0", summary)
        # The old formula reported 200.0 (the median) here.
        self.assertIn("spawn_ack_p95=  300.0", summary)

    def test_summary_reports_spawn_to_ack_not_guest_total(self):
        # codex :333 — the ack_* labels were guest-only totals, hiding
        # host-side snapshot load + VMM startup.
        rows = [
            {
                "cell": "x",
                "ok": True,
                "failures": [],
                "phases": [{"total_ms": 133.0, "tcp_verified": True}],
                "ready_ms": [200.0],
            }
        ]
        summary = rm.summarise(rows)
        self.assertIn("spawn_ack_p50=  200.0", summary)
        self.assertIn("guest_p50=  133.0", summary)


class RunIsolation(unittest.TestCase):
    """codex :150 — two overlapping runs truncated each other's serve log and
    matrix.jsonl; one run could read the OTHER run's serve PID."""

    def test_two_harnesses_write_to_distinct_directories(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            a = make_harness(tmp)
            b = make_harness(tmp)
            self.assertNotEqual(a.out, b.out)
            self.assertTrue(str(a.out).startswith(tmp))
            # The serve log for one tag is therefore per-run, not shared.
            self.assertNotEqual(a.out / "serve-t.log", b.out / "serve-t.log")


class GoldenReuse(unittest.TestCase):
    """codex :141 — every cell and rep cold-prepared the snapshot: 33 prepares
    for 4 distinct tags."""

    def test_make_golden_prepares_each_tag_once(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            harness = make_harness(tmp)
            calls = []

            def fake_run(argv, timeout=0, check=False, env=None):
                calls.append(argv)
                return types.SimpleNamespace(returncode=0, stdout="", stderr="")

            with mock.patch.object(rm, "run", fake_run):
                harness.make_golden("tag-a", "rootless", False)
                harness.make_golden("tag-a", "rootless", False)
                harness.make_golden("tag-b", "rootless", False)
            self.assertEqual(
                len(calls), 2, "one prepare per distinct tag per run, not per call"
            )


class CellTeardown(unittest.TestCase):
    """CR :306 + codex :388 — a raise out of restore_clone/wait_ready lost the
    already-launched clones: no teardown ran for them."""

    def test_every_launched_clone_is_torn_down_when_a_later_spawn_raises(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            harness = make_harness(tmp)
            cell = rm.Cell("uffd", "rootless", 3, False, "ordinary")
            launched, torn_down = [], []

            def fake_restore(cell_, tag, serve_pid, index, rep):
                if index == 2:
                    raise RuntimeError("spawn 3 exploded")
                clone = {"name": f"c{index}", "proc": None, "log": None, "started": 0}
                launched.append(clone["name"])
                return clone

            def fake_teardown(clone, failures, expect_clean=True):
                torn_down.append(clone["name"])

            with mock.patch.object(harness, "restore_clone", fake_restore), mock.patch.object(
                harness, "teardown", fake_teardown
            ):
                with self.assertRaises(RuntimeError):
                    harness.run_cell(cell, "tag", 0, 1)

            self.assertEqual(
                sorted(torn_down),
                sorted(launched),
                "clones launched before the raise must still be torn down",
            )


class ServeTimeoutReap(unittest.TestCase):
    """CR :164 — the readiness-timeout path raised while the serve process kept
    running: the caller never learns the handle, so the leak was permanent."""

    def test_timeout_path_terminates_the_serve_process(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            harness = make_harness(tmp)

            class FakeProc:
                def __init__(self):
                    self.terminated = False
                    self.killed = False

                def poll(self):
                    return None  # still running, never ready

                def terminate(self):
                    self.terminated = True

                def kill(self):
                    self.killed = True

                def wait(self, timeout=None):
                    return 0

            proc = FakeProc()
            clock = {"now": 0.0}

            def fake_monotonic():
                clock["now"] += 30.0  # expire the 90s readiness deadline fast
                return clock["now"]

            with mock.patch.object(rm.subprocess, "Popen", return_value=proc), mock.patch.object(
                rm.time, "monotonic", fake_monotonic
            ), mock.patch.object(rm.time, "sleep", lambda s: None):
                with self.assertRaises(RuntimeError):
                    harness.start_serve("tag-x")

            self.assertTrue(
                proc.terminated or proc.killed,
                "the timeout path must reap the serve process it started",
            )


if __name__ == "__main__":
    unittest.main()
