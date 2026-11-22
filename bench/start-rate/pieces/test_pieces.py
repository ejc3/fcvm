"""Tests of the matrix and table tooling: python3 -m unittest discover -s bench/start-rate/pieces"""

import json
import os
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
        "cold": False,
        "uffd": False,
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


    def test_a_run_without_cgroup_cpu_is_tabled_on_host_cpu(self):
        with tempfile.TemporaryDirectory() as root:
            s = summary(rate=50.0)
            s["timed"].update({"host_cpu_s_per_op_net": 0.5, "cgroup_cpu_s_per_op_net": None,
                               "latency_ms": {"p50": 1, "p99": 2},
                               "per_op": {"ctxt": 1, "ipi_total": 1}})
            s["steady_per_instance_per_s"] = {"host_busy_cpu_s": 0.0}
            (pathlib.Path(root) / "disk-c128-n1000-a1000").mkdir()
            (pathlib.Path(root) / "disk-c128-n1000-a1000" / "summary.json").write_text(json.dumps(s))
            r = subprocess.run([sys.executable, str(HERE / "table.py"), root], capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("| disk | 50 |", r.stdout)


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

    def test_a_failed_process_inspection_is_not_a_quiet_host(self):
        with tempfile.TemporaryDirectory() as bin_dir:
            for tool, body in [("ps", "exit 1"), ("pgrep", "exit 1")]:
                path = pathlib.Path(bin_dir) / tool
                path.write_text(f"#!/bin/sh\n{body}\n")
                path.chmod(0o755)
            r = self.sh(f"PATH={bin_dir}:$PATH LOAD_MAX=100000 QUIET_TRIES=1 quiet cellX")
            self.assertNotEqual(r.returncode, 0, r.stdout + r.stderr)
            self.assertIn("ps failed", r.stdout)

    def test_without_jq_the_matrix_stops_before_touching_a_cell(self):
        with tempfile.TemporaryDirectory() as root:
            bin_dir = pathlib.Path(root) / "bin"
            bin_dir.mkdir()
            for d in ["/usr/bin", "/bin"]:
                for name in os.listdir(d):
                    if name != "jq" and not os.path.lexists(bin_dir / name):
                        (bin_dir / name).symlink_to(os.path.join(d, name))
            cell = pathlib.Path(root) / "out" / "noop-c1-n1-a0"
            cell.mkdir(parents=True)
            (cell / "summary.json").write_text(json.dumps(summary()))
            r = subprocess.run(["/bin/bash", str(HERE / "matrix.sh"), str(pathlib.Path(root) / "out"), "noop"],
                               env={"PATH": str(bin_dir), "RUNGS": "1:1", "ALIVES": "0",
                                    "LOAD_MAX": "0", "QUIET_TRIES": "1"},
                               capture_output=True, text=True)
            self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
            self.assertIn("needs jq", r.stderr)
            self.assertTrue((cell / "summary.json").exists())

    def test_the_netns_probe_counts_only_new_namespace_requests(self):
        self.assertIn("kprobe:copy_net_ns /arg0 & 0x40000000/", (HERE / "kernel.bt").read_text())

    def test_a_cell_from_another_mode_is_not_done(self):
        with tempfile.TemporaryDirectory() as root:
            (pathlib.Path(root) / "c").mkdir()
            (pathlib.Path(root) / "c" / "summary.json").write_text(json.dumps(summary()))
            self.assertEqual(self.sh(f"cell_done {root}/c").returncode, 0)
            self.assertNotEqual(self.sh(f"COLD=1 cell_done {root}/c").returncode, 0)
            self.assertNotEqual(self.sh(f"UFFD_SOCKET=/u cell_done {root}/c").returncode, 0)


if __name__ == "__main__":
    unittest.main()
