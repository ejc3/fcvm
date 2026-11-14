#!/usr/bin/env python3
"""The memory harness's accounting primitives, pinned.

Every property here is a way for a measurement to come back WRONG rather than
absent. That is the whole hazard of this file: report.py's `sample` subcommand
is the only thing in the memory comparison that reads memory, and none of its
failures raise. A basis that silently reports zero looks exactly like a side
that used no memory, and the comparison it feeds is fcvm against host
containers, so a zero on one side alone moves the published ratio.

Run: python3 -m unittest test_report_memory -v
"""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import report  # noqa: E402
import analyze  # noqa: E402


def write_cgroup(path, pids=()):
    """One cgroup v2 node: a directory holding a cgroup.procs file."""
    os.makedirs(path, exist_ok=True)
    with open(os.path.join(path, "cgroup.procs"), "w") as handle:
        handle.write("".join(f"{pid}\n" for pid in pids))
    return path


class CgroupProcs(unittest.TestCase):
    """`cgroup_procs` must name every process the cgroup's memory.current counts.

    memory.current is charged over the whole SUBTREE. Reading cgroup.procs from
    the single node the caller names is therefore not the same process set, and
    the two are only equal for a cgroup with no children. Rootless podman does
    have children: it nests the container's processes one level below the cgroup
    `podman inspect --format {{.State.CgroupPath}}` reports, so the single-node
    read returned [] for every container while memory.current still counted
    them. report.py's sample then published pool_pss_kb = 0 with
    pool_containers = N, which is a live count with an empty process set, and
    the harness's own instance-count check (corpus_mem.run_cell) passed because
    pool_containers comes from `podman ps`, not from this function.

    The fcvm side of the same comparison does not have that hole:
    measure_cgroup_set skips a leaf whose procs are empty, so the same bug there
    lowers `clones` and the instance-count check refuses the cell. The bias was
    therefore one-sided, in fcvm's favour, in the harness whose entire job is
    that comparison.
    """

    def test_pids_below_the_named_node_are_returned(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"))
            write_cgroup(os.path.join(parent, "child"), [4011, 4012])
            self.assertEqual(sorted(report.cgroup_procs(parent)), [4011, 4012])

    def test_pids_at_every_depth_are_returned(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"), [4001])
            child = write_cgroup(os.path.join(parent, "child"), [4011, 4012])
            write_cgroup(os.path.join(child, "grandchild"), [4021])
            self.assertEqual(sorted(report.cgroup_procs(parent)),
                             [4001, 4011, 4012, 4021])

    def test_the_named_nodes_own_pids_are_still_returned(self):
        """A walk that visited only the children would drop the root's own."""
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"), [4001, 4002])
            write_cgroup(os.path.join(parent, "child"))
            self.assertEqual(sorted(report.cgroup_procs(parent)), [4001, 4002])

    def test_one_unreadable_node_refuses_the_whole_process_set(self):
        """A subtotal is not the process set charged by memory.current."""
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"))
            write_cgroup(os.path.join(parent, "good"), [4011])
            bad = os.path.join(parent, "bad")
            os.makedirs(bad)
            with open(os.path.join(bad, "cgroup.procs"), "w") as handle:
                handle.write("not-a-pid\n")
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_procs(parent)

    def test_a_partly_unparseable_node_refuses_the_whole_process_set(self):
        """Dropping one node still produces an incomplete subtree total."""
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"))
            write_cgroup(os.path.join(parent, "good"), [4011])
            torn = os.path.join(parent, "torn")
            os.makedirs(torn)
            with open(os.path.join(torn, "cgroup.procs"), "w") as handle:
                handle.write("4031\nnot-a-pid\n")
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_procs(parent)

    def test_a_path_that_is_not_a_cgroup_reports_no_processes(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(report.cgroup_procs(os.path.join(tmp, "absent")), [])
            self.assertEqual(report.cgroup_procs(write_cgroup(os.path.join(tmp, "empty"))), [])


class CgroupBytes(unittest.TestCase):
    """A missing memory.current is no memory measurement."""

    def test_unreadable_memory_current_refuses_the_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_bytes(tmp)


class StableProcessSet(unittest.TestCase):
    """The PSS process set must not change while memory.current is sampled."""

    def test_a_disappeared_cgroup_cannot_be_measured_as_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(report.CgroupReadError):
                report.measure_complete_cgroup(os.path.join(tmp, "gone"))

    def test_an_empty_owned_cgroup_cannot_be_measured_as_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            empty = write_cgroup(os.path.join(tmp, "owned"))
            with self.assertRaises(report.CgroupReadError):
                report.measure_complete_cgroup(empty)

    def test_process_arrival_refuses_the_sample(self):
        with mock.patch.object(report, "cgroup_procs", side_effect=([4001], [4001, 4002])), \
             mock.patch.object(report, "cgroup_bytes", return_value=4096), \
             mock.patch.object(report, "smaps_rollup_kb", return_value={"Pss": 64, "Private_Clean": 0, "Private_Dirty": 0, "Shared_Clean": 0, "Shared_Dirty": 0}), \
             mock.patch.object(report, "process_comm", return_value="fcvm"), \
             mock.patch.object(report, "cgroup_stat", return_value=dict.fromkeys(
                 report.CGROUP_STAT_FIELDS, 1)):
            with self.assertRaises(report.CgroupReadError):
                report.measure_complete_cgroup("/owned")

    def test_duplicate_pid_from_a_move_refuses_the_sample(self):
        with mock.patch("os.path.isdir", return_value=True), \
             mock.patch("os.walk", return_value=[
                 ("/owned", [], ["cgroup.procs"]),
                 ("/owned/child", [], ["cgroup.procs"]),
             ]), \
             mock.patch("builtins.open", mock.mock_open(read_data="4001\n")):
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_procs("/owned")


class ProcessPss(unittest.TestCase):
    """One unreadable process cannot become a plausible nonzero subtotal."""

    def test_unreadable_smaps_refuses_the_process_set(self):
        # The sudo retry is stubbed to refuse too: unstubbed, it read the real
        # /proc/4001/smaps_rollup whenever pid 4001 existed on the test host
        # (it did after a reboot), and the test went red for no code change.
        refused = subprocess.CompletedProcess(["sudo"], 1, "", "sudo: a password is required")
        with mock.patch("builtins.open", side_effect=PermissionError("denied")), \
             mock.patch.object(report.subprocess, "run", return_value=refused):
            with self.assertRaises(report.CgroupReadError):
                report.smaps_rollup_kb(4001)

    def test_malformed_smaps_refuses_the_process_set(self):
        # Every other field is valid, so only the malformed Pss can refuse.
        rollup = ("Pss: nope kB\nPrivate_Clean: 1 kB\nPrivate_Dirty: 2 kB\n"
                  "Shared_Clean: 3 kB\nShared_Dirty: 4 kB\n")
        with mock.patch("builtins.open", mock.mock_open(read_data=rollup)):
            with self.assertRaises(report.CgroupReadError):
                report.smaps_rollup_kb(4001)

    def test_malformed_memory_current_refuses_the_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            with open(os.path.join(tmp, "memory.current"), "w") as handle:
                handle.write("not-bytes\n")
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_bytes(tmp)


class SustainedSampleSchema(unittest.TestCase):
    """Current cgroup samples and archived process samples remain readable."""

    def test_current_cgroup_sample_supplies_clone_count_and_pss(self):
        sample = {"clones": 4, "clone_pss_kb": 8192}
        self.assertEqual(report.sustained_clone_count(sample), 4)
        self.assertEqual(report.sustained_clone_pss_kb(sample), 8192)

    def test_archived_process_sample_supplies_clone_count_and_pss(self):
        sample = {"fc_procs": 3, "pss_kb": 6144}
        self.assertEqual(report.sustained_clone_count(sample), 3)
        self.assertEqual(report.sustained_clone_pss_kb(sample), 6144)


class RemovedLegacyDensityBasis(unittest.TestCase):
    @staticmethod
    def analyse(include_legacy):
        with tempfile.TemporaryDirectory() as tmp:
            sample_dir = os.path.join(tmp, "samples")
            os.mkdir(sample_dir)
            path = os.path.join(sample_dir, "density.jsonl")
            with open(path, "w") as handle:
                for n in (1, 2, 4):
                    handle.write(json.dumps({
                        "cell": "uffd-4k", "rep": 1, "n": n,
                        "phase": "pre", "mem_available_kb": 1_000_000,
                    }) + "\n")
                    steady = {
                        "cell": "uffd-4k", "rep": 1, "n": n,
                        "phase": "steady", "clones": n,
                        "clone_cgroup_kb": n * 1000,
                        "clone_pss_kb": n * 800,
                        "mem_available_kb": 1_000_000 - n * 1200,
                    }
                    if include_legacy:
                        steady["fc_only_pss_kb"] = n * 700
                    handle.write(json.dumps(steady) + "\n")
            result = {}
            analyze.analyse_density(tmp, result)
            return result["density"]["uffd-4k"]["fits"]

    def test_current_samples_do_not_fabricate_a_zero_legacy_fit(self):
        self.assertNotIn(
            "fc_only_pss_REFUTED_BASIS", self.analyse(include_legacy=False))

    def test_archived_samples_retain_the_recorded_legacy_fit(self):
        self.assertIn(
            "fc_only_pss_REFUTED_BASIS", self.analyse(include_legacy=True))


class PodmanCgroupIdentity(unittest.TestCase):
    """A container sample belongs only to the cgroup podman identifies.

    An inspect failure and an empty CgroupPath used to become
    ``/sys/fs/cgroup``. The recursive process walk then measured the whole host
    while ``pool_containers`` still named the requested pool, so every
    downstream nonzero/count gate accepted it.
    """

    @staticmethod
    def args():
        return SimpleNamespace(
            cgroup_root=None,
            cgroup_prefix=None,
            podman_prefix="owned-",
            extra=None,
        )

    def drive(self, inspect_result, cgroup_root="/sys/fs/cgroup"):
        def run(cmd, **_kwargs):
            if cmd[:2] == ["podman", "ps"]:
                return subprocess.CompletedProcess(cmd, 0, "owned-one\n", "")
            if cmd[:2] == ["podman", "inspect"]:
                return inspect_result
            raise AssertionError(f"unexpected command: {cmd}")

        output = io.StringIO()
        with mock.patch.object(subprocess, "run", run), \
             mock.patch.object(report, "CGROUP_ROOT", cgroup_root, create=True), \
             mock.patch.object(report, "read_meminfo", return_value={}), \
             mock.patch.object(report, "cgroup_procs", return_value=[4001]), \
             mock.patch.object(report, "smaps_rollup_kb", return_value={"Pss": 64, "Private_Clean": 0, "Private_Dirty": 0, "Shared_Clean": 0, "Shared_Dirty": 0}), \
             mock.patch.object(report, "process_comm", return_value="fcvm"), \
             mock.patch.object(report, "cgroup_bytes", return_value=4096), \
             contextlib.redirect_stdout(output):
            report.cmd_sample(self.args())
        return json.loads(output.getvalue())

    def test_failed_inspect_refuses_the_sample_instead_of_measuring_the_host(self):
        failed = subprocess.CompletedProcess(
            ["podman", "inspect"], 125, "", "container disappeared")
        with self.assertRaises(SystemExit) as caught:
            self.drive(failed)
        self.assertNotIn(caught.exception.code, (0, None))

    def test_empty_cgroup_path_refuses_the_sample_instead_of_measuring_the_host(self):
        empty = subprocess.CompletedProcess(["podman", "inspect"], 0, "\n", "")
        with self.assertRaises(SystemExit) as caught:
            self.drive(empty)
        self.assertNotIn(caught.exception.code, (0, None))

    def test_existing_non_root_cgroup_is_measured(self):
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "owned.slice"))
            valid = subprocess.CompletedProcess(
                ["podman", "inspect"], 0, "/owned.slice\n", "")
            rec = self.drive(valid, cgroup_root=tmp)
        self.assertEqual(rec["pool_containers"], 1)
        self.assertEqual(rec["pool_procs"], 1)
        self.assertEqual(rec["pool_pss_kb"], 64)


class CloneMemoryBreakdown(unittest.TestCase):
    """A clone sample says what its file pages are and how much of PSS is private.

    The 4 vCPU corpus memory phase charged each added clone about 104 MiB of
    `file` that its PSS did not show, and its samples held only `anon` and
    `file`, so nothing in the record could say whether those pages were the
    clone's disk page cache or memfd pages its faults had filled (`shmem`).
    """

    STAT = {
        "anon": 300 << 20, "file": 120 << 20, "kernel": 9 << 20, "sock": 0,
        "shmem": 100 << 20, "file_mapped": 3 << 20, "file_dirty": 1 << 20,
        "active_file": 40 << 20, "inactive_file": 80 << 20,
        "active_anon": 200 << 20, "inactive_anon": 100 << 20,
    }

    def sample(self, stat):
        with tempfile.TemporaryDirectory() as root:
            leaf = write_cgroup(os.path.join(root, "req-c-0"), [os.getpid()])
            with open(os.path.join(leaf, "memory.current"), "w") as handle:
                handle.write(f"{430 << 20}\n")
            with open(os.path.join(leaf, "memory.stat"), "w") as handle:
                handle.write("".join(f"{k} {v}\n" for k, v in stat.items()))
            args = SimpleNamespace(cgroup_root=root, cgroup_prefix="req-c-",
                                   podman_prefix=None, extra=None)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                report.cmd_sample(args)
        return json.loads(output.getvalue())

    def test_a_clone_sample_records_the_stat_breakdown_and_the_private_shared_split(self):
        rec = self.sample(self.STAT)
        for key in ("shmem", "file_mapped", "file_dirty", "active_file",
                    "inactive_file", "active_anon", "inactive_anon"):
            self.assertEqual(rec[f"clone_{key}_kb"], self.STAT[key] // 1024, key)
        with open(f"/proc/{os.getpid()}/comm") as handle:
            comm = handle.read().strip()
        mine = rec["clone_smaps_by_comm"][comm]
        self.assertEqual(mine["procs"], 1)
        self.assertGreater(mine["private_dirty_kb"], 0)
        self.assertEqual(
            rec["clone_private_kb"], mine["private_clean_kb"] + mine["private_dirty_kb"])
        self.assertEqual(
            rec["clone_shared_kb"], mine["shared_clean_kb"] + mine["shared_dirty_kb"])

    def test_a_process_name_that_is_not_utf8_is_still_measured(self):
        """A process may name itself with any bytes (PR_SET_NAME). Decoding
        /proc/<pid>/comm strictly raised UnicodeDecodeError and aborted a
        sample whose process set was otherwise complete."""
        child = subprocess.Popen([
            sys.executable, "-c",
            "import ctypes, sys, time\n"
            "ctypes.CDLL(None).prctl(15, b'example-\\xff', 0, 0, 0)\n"
            "print('named', flush=True); time.sleep(30)"],
            stdout=subprocess.PIPE, text=True)
        try:
            self.assertEqual(child.stdout.readline().strip(), "named")
            self.assertEqual(report.process_comm(child.pid), "example-\\xff")
        finally:
            child.kill()
            child.wait()

    def test_a_stat_without_the_breakdown_refuses_the_sample(self):
        stat = dict(self.STAT)
        del stat["shmem"]
        with self.assertRaises(SystemExit) as caught, \
                contextlib.redirect_stderr(io.StringIO()):
            self.sample(stat)
        self.assertEqual(caught.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
