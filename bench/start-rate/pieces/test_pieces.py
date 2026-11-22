"""Tests of the matrix and table tooling: python3 -m unittest discover -s bench/start-rate/pieces"""

import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import table  # noqa: E402


def summary(failed=0, torn_failed=0, perf_errors=None, rate=100.0):
    return {
        "timed": {"failed": failed, "ops_per_s": rate},
        "teardown": {"failed": torn_failed},
        "perf_errors": perf_errors or [],
    }


class TableTest(unittest.TestCase):
    def test_a_cell_with_a_failure_does_not_reach_the_table(self):
        self.assertFalse(table.failed(summary()))
        self.assertTrue(table.failed(summary(failed=1)))
        self.assertTrue(table.failed(summary(torn_failed=2)))
        self.assertTrue(table.failed(summary(perf_errors=["perf exited"])))
        with tempfile.TemporaryDirectory() as root:
            for name, s in [("disk-c1-n100-a0", summary()), ("disk-c32-n400-a0", summary(failed=3))]:
                (pathlib.Path(root) / name).mkdir()
                (pathlib.Path(root) / name / "summary.json").write_text(json.dumps(s))
            cells = table.load([root])
            self.assertEqual(sorted(cells["disk"]), [(1, 0)])

    def test_the_best_rate_is_the_one_with_1000_held(self):
        cells = {(32, 0): (summary(rate=340.0), []), (128, 1000): (summary(rate=70.0), []),
                 (32, 1000): (summary(rate=134.0), [])}
        self.assertEqual(table.best_rate(cells), 134.0)
        self.assertEqual(table.best_rate({(1, 0): (summary(rate=5.0), [])}), 5.0)


    def test_two_cells_at_one_concurrency_and_held_count_are_refused(self):
        with tempfile.TemporaryDirectory() as root:
            for name in ["disk-c128-n100-a1000", "disk-c128-n1000-a1000"]:
                (pathlib.Path(root) / name).mkdir()
                (pathlib.Path(root) / name / "summary.json").write_text(json.dumps(summary()))
            with self.assertRaises(SystemExit) as raised:
                table.load([root])
            self.assertIn("two cells of disk", str(raised.exception))


class ScriptsTest(unittest.TestCase):
    def test_every_script_measures_the_binary_it_was_given(self):
        # The Makefile's START_PIECES replaces the environment's: a script must pass it to make.
        for script in ["matrix.sh", "attribution.sh"]:
            text = (HERE / script).read_text()
            self.assertIn('START_PIECES="${START_PIECES:-$PWD/target/release/start-pieces}"', text,
                          script)


class MatrixTest(unittest.TestCase):
    def sh(self, script):
        return subprocess.run(["bash", "-c", f"source {HERE / 'matrix.sh'}; {script}"],
                              capture_output=True, text=True)

    def test_a_host_that_stays_busy_fails_the_wait(self):
        # LOAD_MAX=0: no host is ever below zero busy CPUs.
        r = self.sh("LOAD_MAX=0 QUIET_TRIES=1 quiet cellX")
        self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("not quiet before cellX", r.stdout)

    def test_only_a_clean_cell_counts_as_done(self):
        with tempfile.TemporaryDirectory() as root:
            for name, s in [("ok", summary()), ("bad", summary(failed=1)),
                            ("perf", summary(perf_errors=["x"]))]:
                (pathlib.Path(root) / name).mkdir()
                (pathlib.Path(root) / name / "summary.json").write_text(json.dumps(s))
            done = {n: self.sh(f"cell_done {root}/{n}").returncode == 0
                    for n in ["ok", "bad", "perf", "missing"]}
            self.assertEqual(done, {"ok": True, "bad": False, "perf": False, "missing": False})


if __name__ == "__main__":
    unittest.main()
