#!/usr/bin/env python3
"""Deterministic tests for the fault benchmark's measurement-integrity rules.

No VM, no root, no clock beyond mtimes we set ourselves. Every test here guards a
rule about what the harness is allowed to REPORT, which is the part a wrong answer
survives: a corrupt number reads exactly like a real one.
"""
import json
import os
import subprocess
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


class KvmAttribution(unittest.TestCase):
    """A host-wide ftrace dump contains other VMs' faults too."""

    # (comm, pid, cpu, ts, ipa) as read_kvm_trace returns them.
    EVENTS = [
        ("fc_vcpu 0", 1001, 0, 1.0, 0x40000000),
        ("fc_vcpu 1", 1002, 1, 1.1, 0x40001000),
        ("fc_vcpu 0", 2001, 2, 1.2, 0x40002000),  # a different firecracker
    ]

    def test_only_this_request_s_threads_are_counted(self):
        """The tracepoint fires in vCPU thread context, so the filter is a TID set."""
        kept, discarded = faultanalyze.kvm_events_for_request(self.EVENTS, [1001, 1002])
        self.assertEqual([e[1] for e in kept], [1001, 1002])
        self.assertEqual(discarded, 1, "the foreign VM's fault must be counted as discarded")

    def test_the_process_id_is_not_a_thread_id(self):
        """Filtering on fc_pid, which is never a vCPU tid, would silently keep nothing."""
        kept, discarded = faultanalyze.kvm_events_for_request(self.EVENTS, [1000])
        self.assertEqual(kept, [])
        self.assertEqual(discarded, 3)

    def test_a_run_without_recorded_tids_attributes_nothing(self):
        kept, discarded = faultanalyze.kvm_events_for_request(self.EVENTS, None)
        self.assertEqual(kept, [], "with no tid set there is no basis to attribute")
        self.assertEqual(discarded, 3)


class FtraceOverflow(unittest.TestCase):
    """A dropped event truncates the fault set on whichever CPU dropped it."""

    STATS = (
        "== /sys/kernel/tracing/instances/faultbench/per_cpu/cpu0/stats\n"
        "entries: 10\noverrun: 0\ndropped events: 0\n"
        "== /sys/kernel/tracing/instances/faultbench/per_cpu/cpu3/stats\n"
        "entries: 10\noverrun: 42\ndropped events: 7\n"
    )

    def test_a_drop_on_any_cpu_is_counted(self):
        self.assertEqual(faultbench.ftrace_lost_events(self.STATS), 49,
                         "cpu0 alone reports zero while cpu3 overran")

    def test_a_clean_trace_reports_no_loss(self):
        clean = self.STATS.replace("overrun: 42", "overrun: 0").replace("dropped events: 7",
                                                                        "dropped events: 0")
        self.assertEqual(faultbench.ftrace_lost_events(clean), 0)


class Schedule(unittest.TestCase):
    """Request order, so host drift is not attributed to whichever cell ran last."""

    CELLS = ["file-4k", "uffd-4k-copy"]
    PAGES = ["a.html", "b.html"]

    def build(self, seed):
        return faultbench.build_schedule(self.CELLS, self.PAGES, reps=3, warmup=1, seed=seed)

    def test_the_same_seed_gives_the_same_order(self):
        self.assertEqual(self.build(7), self.build(7))

    def test_a_different_seed_gives_a_different_order(self):
        self.assertNotEqual(self.build(7), self.build(8))

    def test_every_request_is_scheduled_exactly_once(self):
        schedule = self.build(7)
        self.assertEqual(len(schedule), len(set(schedule)))
        self.assertEqual(len(schedule), 2 * 2 * (3 + 1))
        for cell in self.CELLS:
            measured = [r for r in schedule if r[0] == cell and not r[3]]
            self.assertEqual(len(measured), 6, f"{cell} must keep all reps x pages")

    def test_warmups_run_first(self):
        schedule = self.build(7)
        first_measured = next(i for i, r in enumerate(schedule) if not r[3])
        self.assertTrue(all(r[3] for r in schedule[:first_measured]))
        self.assertTrue(all(not r[3] for r in schedule[first_measured:]),
                        "a warmup after a measured rep would leave a cell measured cold")

    def test_measured_reps_are_not_grouped_by_cell(self):
        """The defect: with cells walked in order, the last cell absorbs all late drift."""
        measured = [r[0] for r in self.build(7) if not r[3]]
        blocked = sorted(measured, key=measured.index)
        self.assertNotEqual(measured, blocked, "the measured order must not be cell-blocked")


class ServeIdentity(unittest.TestCase):
    """A serve is identified by tag AND uffd mode."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        real = faultbench.STATE_DIR
        faultbench.STATE_DIR = Path(self.tmp.name)
        self.addCleanup(setattr, faultbench, "STATE_DIR", real)

    def write_serve(self, name, tag, umode, pid):
        (Path(self.tmp.name) / f"{name}.json").write_text(json.dumps({
            "pid": pid,
            "config": {"process_type": "serve", "snapshot_name": tag, "uffd_mode": umode},
        }))

    def test_the_other_mode_s_serve_is_not_this_one(self):
        self.write_serve("a", "cb-golden-rootless", "minor", 4242)
        self.assertIsNone(faultbench.serve_pid_for("cb-golden-rootless", "copy"),
                          "a minor-mode serve must not answer for the copy-mode cell")
        self.assertEqual(faultbench.serve_pid_for("cb-golden-rootless", "minor"), 4242)

    def test_both_modes_can_be_told_apart(self):
        self.write_serve("a", "cb-golden-rootless", "minor", 4242)
        self.write_serve("b", "cb-golden-rootless", "copy", 4343)
        self.assertEqual(faultbench.serve_pid_for("cb-golden-rootless", "minor"), 4242)
        self.assertEqual(faultbench.serve_pid_for("cb-golden-rootless", "copy"), 4343)

    def test_a_serve_without_a_recorded_mode_reads_as_copy(self):
        """`snapshot serve` defaults to the copy backend when no mode is given."""
        (Path(self.tmp.name) / "c.json").write_text(json.dumps({
            "pid": 4444,
            "config": {"process_type": "serve", "snapshot_name": "t"},
        }))
        self.assertEqual(faultbench.serve_pid_for("t", "copy"), 4444)
        self.assertIsNone(faultbench.serve_pid_for("t", "minor"))


class _FakeSubprocess:
    """faultbench's view of the subprocess module, with Popen recorded.

    run() stays real (main shells out for the host IPv4 address); Popen is
    what starts hostserver.py, and a test must never leak a real one.
    """

    def __init__(self, record):
        self.record = record
        self.run = subprocess.run
        self.STDOUT = subprocess.STDOUT
        self.PIPE = subprocess.PIPE
        self.DEVNULL = subprocess.DEVNULL

    def Popen(self, *args, **kwargs):
        self.record.append(args[0] if args else kwargs.get("args"))

        class Dummy:
            def poll(self):
                return None

            def terminate(self):
                pass

            def kill(self):
                pass

            def wait(self, timeout=None):
                return 0

        return Dummy()


class QuietGate(unittest.TestCase):
    """The start-load check and its SETTLE_WAIT_SECS knob."""

    def patch(self, obj, name, value):
        real = getattr(obj, name)
        setattr(obj, name, value)
        self.addCleanup(setattr, obj, name, real)

    def test_the_start_load_check_settles_within_the_settle_window(self):
        """`make bench-chromium-fault` runs build and setup right before this
        gate, so a fail-fast cold chain refuses on its own prerequisite wake
        and a retry repeats the prerequisites. SETTLE_WAIT_SECS bounds a wait
        for the load to fall, same knob as reqbench.sh and hostcdp.sh."""
        samples = iter([(9.9, 0.0, 0.0), (9.5, 0.0, 0.0), (0.2, 0.0, 0.0)])
        self.patch(os, "getloadavg", lambda: next(samples))
        self.patch(faultbench, "firecracker_pids", lambda: set())
        naps = []
        self.patch(faultbench.time, "sleep", naps.append)
        os.environ["SETTLE_WAIT_SECS"] = "30"
        self.addCleanup(os.environ.pop, "SETTLE_WAIT_SECS", None)
        info = faultbench.host_precheck()
        self.assertEqual(info["loadavg_1m"], 0.2)
        self.assertEqual(len(naps), 2, "one nap per busy re-sample")

    def test_a_busy_refusal_spawns_nothing_and_keeps_the_out_dir_fresh(self):
        """A refused run must be retryable with the same --out.

        A hostserver started before host_precheck sits outside the teardown
        try/finally, so a refusal leaks it; require_fresh_out_dir and the
        mkdirs running earlier still means the retry is refused for the
        directory the refused run itself dirtied.
        """
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "run"
            spawned = []
            self.patch(faultbench, "subprocess", _FakeSubprocess(spawned))
            self.patch(os, "getloadavg", lambda: (9.9, 0.0, 0.0))
            self.patch(faultbench, "firecracker_pids", lambda: set())
            self.patch(sys, "argv", ["faultbench.py", "--out", str(out)])
            with self.assertRaises(SystemExit) as caught:
                faultbench.main()
            self.assertIn("load average", str(caught.exception))
            self.assertEqual(spawned, [], "a refused run spawned a child")
            # Raises SystemExit("not empty") if the refusal dirtied it.
            faultbench.require_fresh_out_dir(out)


if __name__ == "__main__":
    unittest.main()
