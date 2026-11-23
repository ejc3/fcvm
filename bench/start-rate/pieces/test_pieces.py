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

    def test_rtnl_mutex_is_traced_as_a_mutex(self):
        # rtnl_lock() is inlined into its callers inside rtnetlink.c, so a probe on that symbol
        # misses every RTM_NEWLINK, RTM_SETLINK and RTM_DELLINK hold.
        bt = (HERE / "kernel.bt").read_text()
        self.assertIn('kprobe:mutex_unlock /arg0 == kaddr("rtnl_mutex")', bt)
        self.assertNotIn("kprobe:rtnl_lock", bt)
        self.assertNotIn("kprobe:rtnl_unlock", bt)

    def test_the_tracer_outlives_the_namespace_cleanup(self):
        # The kernel destroys a cell's namespaces after the driver has exited. A tracer stopped
        # at the driver's exit misses most of that hold.
        script = (HERE / "attribution.sh").read_text()
        self.assertLess(script.index('quiet "the end of'), script.index('sudo kill -INT "$bt"'))


    def test_a_cell_from_another_mode_is_not_done(self):
        with tempfile.TemporaryDirectory() as root:
            (pathlib.Path(root) / "c").mkdir()
            (pathlib.Path(root) / "c" / "summary.json").write_text(json.dumps(summary()))
            self.assertEqual(self.sh(f"cell_done {root}/c").returncode, 0)
            self.assertNotEqual(self.sh(f"COLD=1 cell_done {root}/c").returncode, 0)
            self.assertNotEqual(self.sh(f"UFFD_SOCKET=/u cell_done {root}/c").returncode, 0)


class RtnlSummaryTest(unittest.TestCase):
    DUMP = """
@rtnl_hold_us_total: 40000

@rtnl_hold_us_by_work[cleanup_net+0]: 30000
@rtnl_holds_by_work[cleanup_net+0]: 3
@rtnl_hold_us_by_work[linkwatch_event+0]: 1000
@rtnl_holds_by_work[linkwatch_event+0]: 5
@rtnl_hold_us_by_comm[pasta-4707649d2]: 4000
@rtnl_hold_us_by_comm[fcvm-netns]: 5000
@rtnl_holds[pasta-4707649d2]: 20
@rtnl_holds[fcvm-netns]: 110
@rtnl_hold_us_by_site[fcvm-netns, 
    mutex_unlock+0
    rtnl_newlink+1036
]: 5000
@rtnl_oncpu[
    wireless_nlevent_flush+40
    wext_netdev_notifier_call+12
    notifier_call_chain+120
]: 3998
@rtnl_oncpu[
    _raw_read_lock+20
    do_one_broadcast+80
    netlink_broadcast_filtered+300
]: 1999
@rtnl_oncpu[
    __dev_open+100
    netif_change_flags+60
]: 5997
@rcu_barrier_us_under_rtnl: 26000
@rcu_barriers_under_rtnl: 10
@rtnl_wait_us_total: 2000000
@rcu_exp[kworker/u769:3]: 40
@rcu_exp[kworker/1:2]: 10
@rcu_exp[fcvm-netns]: 20
"""

    def test_holds_are_per_op_and_split_by_holder(self):
        import rtnl
        s = rtnl.summarize(self.DUMP, 10)
        self.assertEqual(s["hold_ms_per_op"], 4.0)
        self.assertEqual(s["rcu_barrier_ms_per_op"], 2.6)
        self.assertEqual(s["hold_ms_per_op_without_rcu_barrier"], 1.4)
        self.assertEqual(s["rcu_barriers_per_op"], 1.0)
        self.assertEqual(s["wait_ms_per_op"], 200.0)
        self.assertEqual(s["by_holder"]["worker cleanup_net"], {"hold_ms_per_op": 3.0, "holds_per_op": 0.3})
        self.assertEqual(s["by_holder"]["pasta"], {"hold_ms_per_op": 0.4, "holds_per_op": 2.0})
        self.assertEqual(s["by_holder"]["fcvm-netns"], {"hold_ms_per_op": 0.5, "holds_per_op": 11.0})
        self.assertEqual(s["expedited_grace_periods_per_op"], {"worker": 5.0, "fcvm-netns": 2.0})
        self.assertEqual(s["on_cpu_ms_per_op"], {"all": 600.0, "wireless_nlevent_flush": 200.0,
                                                 "do_one_broadcast": 100.0, "rcu_barrier": 0.0})

    def test_a_dump_from_a_tracer_that_did_not_run_is_refused(self):
        import rtnl
        with self.assertRaises(SystemExit):
            rtnl.summarize("Attaching 30 probes...\n", 10)


if __name__ == "__main__":
    unittest.main()
