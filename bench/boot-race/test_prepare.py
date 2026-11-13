#!/usr/bin/env python3
"""Tests for prepare.py cleanup and race.py's task reads, against fake AWS clients.

    python3 -m unittest bench/boot-race/test_prepare.py
"""

import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import prepare  # noqa: E402
import race  # noqa: E402


class Waiter:
    def wait(self, **kw):
        pass


class FakeAws:
    """One object for the ec2, ecs and ecr clients: a campaign that died with a task running and an image made."""

    def __init__(self):
        self.calls = []

    def client(self, name):
        return self

    def get_waiter(self, name):
        return Waiter()

    def describe_tasks(self, cluster, tasks):
        return {"tasks": [{"taskArn": a, "lastStatus": "RUNNING" if a.endswith("live") else "STOPPED"}
                          for a in tasks]}

    def describe_instances(self, Filters):
        return {"Reservations": []}

    def describe_images(self, **kw):
        if "repositoryName" in kw:
            return {"imageDetails": []}
        return {"Images": [{"ImageId": "ami-0123456789abcdef0", "Name": "boot-race-workload-x"}]}

    def describe_snapshots(self, OwnerIds, Filters):
        return {"Snapshots": [{"SnapshotId": "snap-0123456789abcdef0"}]}

    def deregister_image(self, ImageId):
        self.calls.append(("deregister", ImageId))

    def delete_snapshot(self, SnapshotId):
        self.calls.append(("delete_snapshot", SnapshotId))


class Cleanup(unittest.TestCase):
    def test_cleanup_stops_live_tasks_and_finds_tagged_images_without_the_config(self):
        stopped = []

        class Lane:
            def __init__(self, cfg):
                pass

            def stop(self, arn):
                stopped.append(arn)

        aws = FakeAws()
        with tempfile.TemporaryDirectory() as d, mock.patch.object(race, "Ecs", Lane):
            with open(os.path.join(d, "tasks.txt"), "w") as f:
                f.write("task/a-live\ntask/b-done\ntask/a-live\n")
            prepare.cleanup({"results": d}, aws)  # no "ami": the AMI step died before recording it
        self.assertEqual(stopped, ["task/a-live"])
        self.assertIn(("deregister", "ami-0123456789abcdef0"), aws.calls)
        self.assertIn(("delete_snapshot", "snap-0123456789abcdef0"), aws.calls)


class TaskReads(unittest.TestCase):
    def test_a_task_described_as_missing_just_after_runtask_is_waited_for(self):
        answers = [{"tasks": [], "failures": [{"reason": "MISSING"}]},
                   {"tasks": [{"lastStatus": "STOPPED"}]}]

        class Client:
            def describe_tasks(self, cluster, tasks):
                return answers.pop(0)

        lane = race.Ecs.__new__(race.Ecs)
        lane.ecs = Client()
        self.assertIsNone(lane.describe("arn"))
        lane.ecs = Client()
        answers[:] = [{"tasks": []}, {"tasks": [{"lastStatus": "RUNNING"}]}]
        with mock.patch.object(race.time, "sleep"):
            self.assertEqual(lane.describe_settled("arn")["lastStatus"], "RUNNING")


if __name__ == "__main__":
    unittest.main()
