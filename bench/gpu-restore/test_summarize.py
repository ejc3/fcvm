import csv
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SUMMARIZE = Path(__file__).with_name("summarize.py")
HEADER = ["run", "sbdf", "wall_ms", "exit", "ack_ms", "gpu_ms", "backend", "load1"]


def summarize(rows):
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "runs.csv"
        buf = io.StringIO()
        w = csv.writer(buf)
        w.writerow(HEADER)
        w.writerows(rows)
        path.write_text(buf.getvalue())
        return subprocess.run(
            [sys.executable, "-I", str(SUMMARIZE), str(path)],
            capture_output=True,
            text=True,
        )


class Summarize(unittest.TestCase):
    def test_all_rows_with_ack_are_summarized(self):
        r = summarize(
            [
                [1, "g0", 1000, 0, 400, 50, "firecracker", "0.10"],
                [2, "g0", 1200, 0, 500, 60, "firecracker", "0.20"],
            ]
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("| wall_ms | 2 |", r.stdout)

    def test_exit_zero_row_without_ack_is_refused(self):
        # A cold boot exits 0 with no restored record. Its 40 s wall time must not
        # land in the cache-hit wall_ms distribution.
        r = summarize(
            [
                [1, "g0", 1000, 0, 400, 50, "firecracker", "0.10"],
                [2, "g0", 40000, 0, "", "", "", "0.20"],
            ]
        )
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("run 2", r.stderr)
        self.assertNotIn("| wall_ms |", r.stdout)

    def test_failed_rows_are_still_excluded(self):
        r = summarize(
            [
                [1, "g0", 1000, 0, 400, 50, "firecracker", "0.10"],
                [2, "g0", 90000, 1, "", "", "", "0.20"],
            ]
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("| wall_ms | 1 |", r.stdout)


if __name__ == "__main__":
    unittest.main()
