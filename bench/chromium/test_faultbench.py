#!/usr/bin/env python3
"""Deterministic tests for the fault benchmark's measurement-integrity rules.

No VM, no root, no clock beyond mtimes we set ourselves. Every test here guards a
rule about what the harness is allowed to REPORT, which is the part a wrong answer
survives: a corrupt number reads exactly like a real one.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import faultanalyze  # noqa: E402
import faultbench  # noqa: E402


def trace_file(directory, name, mtime):
    path = Path(directory) / name
    path.write_bytes(b"")
    os.utime(path, (mtime, mtime))
    return path


class TraceAttribution(unittest.TestCase):
    """Which UFFD trace belongs to which request."""

    def test_one_trace_in_the_window_is_the_request_s_trace(self):
        with tempfile.TemporaryDirectory() as d:
            mine = trace_file(d, "1000-vm-0.faults", 150.0)
            trace_file(d, "1000-vm-1.faults", 400.0)  # a later request's
            tf, ambiguous = faultanalyze.match_trace([mine] + list(Path(d).glob("*vm-1*")),
                                                     100.0, 160.0)
            self.assertEqual(tf, mine)
            self.assertEqual(ambiguous, [])

    def test_a_trace_written_during_teardown_still_counts(self):
        """The trace lands when the handler exits, which trails t1 by the teardown."""
        with tempfile.TemporaryDirectory() as d:
            late = trace_file(d, "1000-vm-0.faults", 163.0)
            tf, ambiguous = faultanalyze.match_trace([late], 100.0, 160.0)
            self.assertEqual(tf, late, "a trace inside the grace period is still this run's")
            self.assertEqual(ambiguous, [])
            self.assertLessEqual(163.0, 160.0 + faultanalyze.TRACE_GRACE_S)

    def test_two_traces_in_the_window_attribute_to_neither(self):
        """The reviewer's case: the next request starts 1s later, so the grace overlaps.

        Taking the newest would give this request the NEXT one's faults.
        """
        with tempfile.TemporaryDirectory() as d:
            mine = trace_file(d, "1000-vm-0.faults", 155.0)
            theirs = trace_file(d, "1000-vm-1.faults", 163.0)
            tf, ambiguous = faultanalyze.match_trace([mine, theirs], 100.0, 160.0)
            self.assertIsNone(tf, "an ambiguous window must not be attributed to either")
            self.assertEqual(ambiguous, ["1000-vm-0.faults", "1000-vm-1.faults"],
                             "the record must name what could not be told apart")

    def test_no_trace_in_the_window_is_not_an_ambiguity(self):
        with tempfile.TemporaryDirectory() as d:
            other = trace_file(d, "1000-vm-9.faults", 900.0)
            tf, ambiguous = faultanalyze.match_trace([other], 100.0, 160.0)
            self.assertIsNone(tf)
            self.assertEqual(ambiguous, [], "absence is not ambiguity")


class SerialIsolation(unittest.TestCase):
    """A request may only be measured with no clone left over from the previous one."""

    def setUp(self):
        self.real = faultbench.wait_clones_gone
        self.addCleanup(setattr, faultbench, "wait_clones_gone", self.real)

    def test_a_cleanup_timeout_stops_the_run(self):
        faultbench.wait_clones_gone = lambda prefix, timeout: False
        with self.assertRaises(SystemExit) as caught:
            faultbench.require_clones_gone("fb-abc", 120, "uffd 4k rep3")
        message = str(caught.exception)
        self.assertIn("fb-abc", message)
        self.assertIn("uffd 4k rep3", message, "the message must name where it stopped")
        self.assertIn("surviving clone", message)

    def test_a_clean_teardown_continues(self):
        faultbench.wait_clones_gone = lambda prefix, timeout: True
        faultbench.require_clones_gone("fb-abc", 120, "uffd 4k rep3")


class OutputDirectory(unittest.TestCase):
    """A run's raw record has to be its own."""

    def test_an_existing_run_directory_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "run"
            (out / "requests").mkdir(parents=True)
            (out / "requests.jsonl").write_text('{"rep": 0}\n')
            with self.assertRaises(SystemExit) as caught:
                faultbench.require_fresh_out_dir(out)
            self.assertIn("not empty", str(caught.exception))

    def test_an_absent_or_empty_directory_is_accepted(self):
        with tempfile.TemporaryDirectory() as d:
            faultbench.require_fresh_out_dir(Path(d) / "fresh")
            empty = Path(d) / "empty"
            empty.mkdir()
            faultbench.require_fresh_out_dir(empty)


class Stability(unittest.TestCase):
    """The cross-run working-set numbers."""

    def test_all_empty_sets_do_not_abort_the_analysis(self):
        """A trace with no faults is a real outcome, not a reason to lose the run."""
        result = faultanalyze.stability([set(), set()])
        self.assertEqual(result["core_size"], 0)
        self.assertIsNone(result["core_frac_of_mean_set"],
                          "a fraction of an empty mean set is undefined, not zero")

    def test_non_empty_sets_still_report_the_fraction(self):
        result = faultanalyze.stability([{1, 2, 3, 4}, {3, 4, 5, 6}])
        self.assertEqual(result["core_size"], 2)
        self.assertAlmostEqual(result["core_frac_of_mean_set"], 0.5)


if __name__ == "__main__":
    unittest.main()
