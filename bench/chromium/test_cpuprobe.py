#!/usr/bin/env python3
"""cpuprobe must measure the window it watched, and must not lose processes.

This probe exists to answer "is 2 vCPU enough", and both defects below produce a
plausible number rather than an error. A probe that reports a wrong figure
confidently is worse than one that reports nothing, because the wrong figure is
what ends up in the report.

Run: python3 -m unittest test_cpuprobe -v
"""

import os
import subprocess
import sys
import threading
import time
import unittest

import cpuprobe

# A child that spins for a fixed wall-clock span. `comm` is set with the
# interpreter's own name, so the scan test matches on a prefix the way the real
# probe matches `firecracker-def` (the kernel truncates comm to 15 bytes).
BURNER = (
    "import time,sys\n"
    "end = time.monotonic() + float(sys.argv[1])\n"
    "while time.monotonic() < end: pass\n"
)


def burn(seconds: float) -> subprocess.Popen:
    return subprocess.Popen([sys.executable, "-c", BURNER, str(seconds)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


class PrimingWindow(unittest.TestCase):
    """track() must charge only the time it was actually attached."""

    def test_runtime_before_the_probe_attached_is_not_counted(self):
        """The first collect() has an empty `last`, so every delta is measured
        from ZERO and equals that thread's runtime since process start.
        Accumulating those charges the probe for work it never watched: for a
        VMM the probe attaches to after boot, that is the entire boot.

        The per-sample series was always right -- only the totals were wrong --
        so no shape in a run reveals this. It takes a process with a known
        head start.

        Let the child burn for HEAD seconds before attaching, then watch it for
        roughly TAIL. total_run_ms must reflect TAIL, not HEAD + TAIL. Without
        the fix this reports about 1500 ms for a 500 ms window.
        """
        head, tail = 1.0, 0.5
        child = burn(head + tail)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        time.sleep(head)  # accumulate a head start the probe must NOT claim

        started = time.monotonic()
        record = cpuprobe.track(child.pid, 0.02, cpuprobe.schedstats_enabled())
        watched_ms = (time.monotonic() - started) * 1000

        self.assertTrue(record, "the probe recorded nothing at all")
        # A single-threaded spinner cannot exceed one core, so its runtime over
        # the watched window is bounded by the window itself. The head start is
        # 2x the window, so counting it would blow this bound outright.
        self.assertLess(
            record["total_run_ms"], watched_ms * 1.5,
            f"total_run_ms={record['total_run_ms']:.0f} over a "
            f"{watched_ms:.0f}ms window: the probe is charging for the "
            f"{head * 1000:.0f}ms the process ran BEFORE it attached",
        )
        # And it must not have gone the other way and measured nothing.
        self.assertGreater(record["total_run_ms"], watched_ms * 0.3,
                           "the probe attached but measured almost no runtime")

    def test_mean_cores_stays_within_what_one_thread_can_do(self):
        """The same defect stated as the figure a reader actually quotes.

        mean_cores is total_run_ms over the watched span. Counting the head
        start pushes it above 1.0 for a single-threaded spinner, which is
        arithmetically impossible and is exactly the tell that caught the
        earlier non-monotonic-sum bug (mean 0.01 beside a 1.05 peak).
        """
        child = burn(1.5)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        time.sleep(1.0)
        record = cpuprobe.track(child.pid, 0.02, cpuprobe.schedstats_enabled())
        self.assertTrue(record)
        self.assertLessEqual(
            record["mean_cores"], 1.35,
            f"mean_cores={record['mean_cores']:.2f} for ONE spinning thread; "
            "a single thread cannot exceed 1.0 cores, so the surplus is "
            "pre-attach runtime",
        )


class ConcurrentFollowing(unittest.TestCase):
    """The scan loop must not lose a process because another one is running."""

    def test_a_second_process_is_not_missed_while_the_first_is_tracked(self):
        """track() runs until its process exits. Calling it inline froze the
        scan loop for that whole lifetime, so any process that started AND
        finished inside another's track was never added to `checked` and
        vanished from the record silently -- concurrent clones reported as one.

        Two overlapping children, the second entirely inside the first's
        lifetime. Both must appear.
        """
        long_child = burn(1.2)
        self.addCleanup(long_child.wait)
        self.addCleanup(long_child.kill)

        out = os.path.join(os.environ.get("TMPDIR", "/tmp"),
                           f"cpuprobe-scan-{os.getpid()}.jsonl")
        self.addCleanup(lambda: os.path.exists(out) and os.unlink(out))

        # The probe watches by comm prefix; both children are this interpreter.
        watch = os.path.basename(sys.executable)[:15]
        argv = ["cpuprobe.py", "--watch", watch, "--out", out,
                "--interval-ms", "20", "--scan-ms", "20", "--duration-s", "1.5"]

        short_child = []

        def start_second():
            time.sleep(0.3)  # well inside the long child's lifetime
            short_child.append(burn(0.3))

        starter = threading.Thread(target=start_second, daemon=True)
        starter.start()

        saved = sys.argv
        sys.argv = argv
        try:
            cpuprobe.main()
        finally:
            sys.argv = saved
            starter.join(timeout=5)
            for child in short_child:
                child.kill()
                child.wait()

        with open(out) as handle:
            seen = {int(line.split('"pid":')[1].split(",")[0])
                    for line in handle if line.strip()}
        self.assertIn(long_child.pid, seen, "the long-lived process was missed")
        self.assertIn(short_child[0].pid, seen,
                      "a process that started and finished while another was "
                      "being tracked never reached the record")


class EmptySummary(unittest.TestCase):
    def test_a_run_with_no_usable_figure_reports_instead_of_crashing(self):
        """percentile() returns None for an empty list, `:.2f` raises TypeError
        on it, and max([]) raises ValueError. A process that exits before its
        second sample leaves cores_max None, so a run of only such processes
        crashed on the way out -- discarding the summary and, worse, hiding the
        real finding: the probe attached too late to measure anything.
        """
        out = os.path.join(os.environ.get("TMPDIR", "/tmp"),
                           f"cpuprobe-none-{os.getpid()}.jsonl")
        self.addCleanup(lambda: os.path.exists(out) and os.unlink(out))
        saved = sys.argv
        sys.argv = ["cpuprobe.py", "--watch", "no-such-process-anywhere",
                    "--out", out, "--duration-s", "0.2"]
        try:
            self.assertEqual(cpuprobe.main(), 1,
                             "a run that observed nothing must report failure")
        finally:
            sys.argv = saved


if __name__ == "__main__":
    unittest.main()
