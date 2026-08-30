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

import os
import sys
import tempfile
import unittest

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

    def test_one_unreadable_node_does_not_lose_its_siblings(self):
        """Refusing to read one node must not silently zero the whole subtree."""
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"))
            write_cgroup(os.path.join(parent, "good"), [4011])
            bad = os.path.join(parent, "bad")
            os.makedirs(bad)
            with open(os.path.join(bad, "cgroup.procs"), "w") as handle:
                handle.write("not-a-pid\n")
            self.assertEqual(sorted(report.cgroup_procs(parent)), [4011])

    def test_a_partly_unparseable_node_contributes_nothing_rather_than_a_prefix(self):
        """All-or-nothing per node.

        A total summed over a partial process set is indistinguishable from a
        complete one, so a node that cannot be read whole contributes nothing
        and its siblings still do.
        """
        with tempfile.TemporaryDirectory() as tmp:
            parent = write_cgroup(os.path.join(tmp, "parent"))
            write_cgroup(os.path.join(parent, "good"), [4011])
            torn = os.path.join(parent, "torn")
            os.makedirs(torn)
            with open(os.path.join(torn, "cgroup.procs"), "w") as handle:
                handle.write("4031\nnot-a-pid\n")
            self.assertEqual(sorted(report.cgroup_procs(parent)), [4011])

    def test_a_path_that_is_not_a_cgroup_reports_no_processes(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(report.cgroup_procs(os.path.join(tmp, "absent")), [])
            self.assertEqual(report.cgroup_procs(write_cgroup(os.path.join(tmp, "empty"))), [])


if __name__ == "__main__":
    unittest.main()
