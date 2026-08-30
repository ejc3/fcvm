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
             mock.patch.object(report, "pss_kb_of_pid", return_value=64), \
             mock.patch.object(report, "cgroup_stat", return_value={
                 "anon": 1, "file": 2, "kernel": 3, "sock": 4,
             }):
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
        with mock.patch("builtins.open", side_effect=PermissionError("denied")):
            with self.assertRaises(report.CgroupReadError):
                report.pss_kb_of_pid(4001)

    def test_malformed_smaps_refuses_the_process_set(self):
        with mock.patch("builtins.open", mock.mock_open(read_data="Pss: nope kB\n")):
            with self.assertRaises(report.CgroupReadError):
                report.pss_kb_of_pid(4001)

    def test_malformed_memory_current_refuses_the_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            with open(os.path.join(tmp, "memory.current"), "w") as handle:
                handle.write("not-bytes\n")
            with self.assertRaises(report.CgroupReadError):
                report.cgroup_bytes(tmp)


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
            state_dir=None,
            name_prefix=None,
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
             mock.patch.object(report, "pss_kb_of_pid", return_value=64), \
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


if __name__ == "__main__":
    unittest.main()
