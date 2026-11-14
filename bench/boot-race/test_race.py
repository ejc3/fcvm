#!/usr/bin/env python3
"""Tests for race.py: the identity checks that refuse a mislabelled sample, and the probe's timing of an answer.

    python3 -m unittest bench/boot-race/test_race.py
"""

import asyncio
import json
import os
import sys
import time
import unittest
from unittest import mock
from unittest.mock import Mock, patch

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import race  # noqa: E402

A = {"proc_id": "a", "boot_id": "x"}
B = {"proc_id": "b", "boot_id": "y"}


class Identity(unittest.TestCase):
    def test_hibernate_must_resume_the_same_process(self):
        self.assertIsNone(race.check_identity("ec2-hibernate", A, A, {}))
        self.assertIn("booted", race.check_identity("ec2-hibernate", A, B, {}))
        self.assertIn("no identity", race.check_identity("ec2-hibernate", None, A, {}))

    def test_warm_lambda_must_be_the_primed_process(self):
        self.assertIsNone(race.check_identity("lambda-warm", A, A, {}))
        self.assertIn("cold start", race.check_identity("lambda-warm", A, B, {}))
        self.assertIsNotNone(race.check_identity("lambda-warm", None, A, {}))

    def test_starting_lanes_must_answer_from_a_new_process(self):
        for lane in ("ec2-launch", "ec2-start", "ecs-fargate", "ecs-ec2"):
            self.assertIsNone(race.check_identity(lane, A, B, {}), lane)
            self.assertIsNone(race.check_identity(lane, None, B, {}), lane)
            self.assertIsNotNone(race.check_identity(lane, A, A, {}), lane)

    def test_fcvm_must_answer_from_the_snapshot_process(self):
        cfg = {"fcvm_proc_id": "a"}
        self.assertIsNone(race.check_identity("fcvm-file", A, A, cfg))
        self.assertIsNotNone(race.check_identity("fcvm-uffd", A, B, cfg))

    def test_fcvm_without_the_snapshot_process_id_is_refused(self):
        self.assertIn("fcvm_proc_id", race.check_identity("fcvm-file", A, A, {}))

    def test_cold_lambda_is_not_refused_here(self):
        # its evidence (Init Duration, new process, new kernel) is reported on the page instead
        self.assertIsNone(race.check_identity("lambda-cold", A, A, {}))


class EcsStop(unittest.TestCase):
    arn = "arn:aws:ecs:us-east-2:123456789012:task/boot-race/0123456789abcdef0123456789abcdef"

    class ClientError(Exception):
        def __init__(self, code):
            self.response = {"Error": {"Code": code}}

    def setUp(self):
        with patch.object(race, "session", return_value=Mock()):
            self.lane = race.Ecs({})
        self.lane.invoke = Mock()

    def test_stop_uses_ecs_without_the_run_capable_launcher(self):
        self.lane.stop(self.arn)
        self.lane.ecs.stop_task.assert_called_once_with(
            cluster="boot-race", task=self.arn, reason="boot-race sample taken")
        self.lane.invoke.assert_not_called()

    def test_an_iam_denial_does_not_retry_through_the_launcher(self):
        self.lane.ecs.stop_task.side_effect = self.ClientError("AccessDeniedException")
        with self.assertRaises(self.ClientError):
            self.lane.stop(self.arn)
        self.lane.invoke.assert_not_called()


class FakeSession:
    """The AWS answers setup_rows reads, shaped like the real ones, with real-looking ids in them."""

    def client(self, name):
        return self

    def describe_launch_template_versions(self, LaunchTemplateName, Versions):
        return {"LaunchTemplateVersions": [{"LaunchTemplateData": {
            "ImageId": "ami-0123456789abcdef0", "InstanceType": "c8g.medium",
            "BlockDeviceMappings": [{"Ebs": {"VolumeType": "gp3", "VolumeSize": 8}}]}}]}

    def describe_images(self, ImageIds):
        return {"Images": [{"Name": "al2023-ami-2023.12.20261102.0-kernel-6.1-arm64"}]}

    def describe_task_definition(self, taskDefinition):
        return {"taskDefinition": {"cpu": "1024", "memory": "2048", "networkMode": "awsvpc",
                                   "runtimePlatform": {"cpuArchitecture": "ARM64"}}}

    def get_function_configuration(self, FunctionName):
        return {"Runtime": "provided.al2023", "Architectures": ["arm64"], "MemorySize": 2048,
                "Environment": {"Variables": {"BOOT_RACE_EXIT_AFTER_RESPONSE": "1"}}}


class Setup(unittest.TestCase):
    def test_setup_rows_pass_both_public_checks(self):
        import render

        rows = race.setup_rows({"fcvm_bin": "/bin/echo"}, FakeSession())
        text = json.dumps({"region": race.REGION, "rows": rows})
        render.check_public(text)
        render.check_public(text, render.FORBIDDEN_IN_RECORDS)
        self.assertIn("c8g.medium", text)


class Lock(unittest.TestCase):
    def test_a_second_run_on_the_same_results_refuses_before_touching_anything(self):
        import fcntl
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            cfg = {"results": d}
            held = open(os.path.join(d, "campaign.lock"), "w")  # another driver's lock
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                with self.assertRaises(SystemExit):
                    asyncio.run(race.run(cfg, 1, 0, ["lambda-warm"]))
                self.assertFalse(os.path.exists(os.path.join(d, "samples.jsonl")))
            finally:
                held.close()


class Resume(unittest.TestCase):
    def test_a_resumed_round_does_not_take_a_recorded_lane_again(self):
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "samples.jsonl")
            with open(path, "w") as f:
                f.write(json.dumps({"round": 0, "lane": "lambda-warm", "ok": False, "error": "x"}) + "\n")
            asyncio.run(race.run({"results": d}, 1, 0, ["lambda-warm"]))
            with open(path) as f:
                self.assertEqual(len(f.read().splitlines()), 1)


class Hibernate(unittest.TestCase):
    def test_a_resumed_instance_settles_before_its_hibernate_stop(self):
        events = []

        class Client:
            def describe_instances(self, InstanceIds):
                state = "running" if any(e[0] == "start" for e in events) else "stopped"
                return {"Reservations": [{"Instances": [{"PrivateIpAddress": "x", "State": {"Name": state}}]}]}

            def start_instances(self, InstanceIds):
                events.append(("start",))

            def stop_instances(self, InstanceIds, Hibernate):
                events.append(("stop", Hibernate))

        async def answered(*a, **kw):
            return 0, {"proc_id": "a", "boot_id": "x"}, 1

        async def slept(s):
            events.append(("sleep", s))

        for hibernate, settle in ((True, 30), (False, 1)):
            events.clear()
            lane = race.Ec2.__new__(race.Ec2)
            lane.ec2 = Client()
            with mock.patch.object(race, "probe", answered), mock.patch.object(race.asyncio, "sleep", slept):
                asyncio.run(lane.start_sample("i-1", hibernate))
            self.assertEqual(events, [("start",), ("sleep", settle), ("stop", hibernate)])


class FailedStart(unittest.TestCase):
    def test_a_refused_start_fails_the_sample_without_waiting_or_stopping(self):
        events = []

        class Refused(Exception):
            pass

        class Client:
            def describe_instances(self, InstanceIds):
                return {"Reservations": [{"Instances": [{"PrivateIpAddress": "x", "State": {"Name": "stopped"}}]}]}

            def start_instances(self, InstanceIds):
                raise Refused("InsufficientInstanceCapacity")

            def stop_instances(self, InstanceIds, Hibernate):
                events.append(("stop", Hibernate))

        async def slept(s):
            events.append(("sleep", s))

        lane = race.Ec2.__new__(race.Ec2)
        lane.ec2 = Client()
        real_wait = lane.wait_state

        def wait_state(iid, want, timeout=900):
            events.append(("wait", want))
            if want != "stopped":
                raise TimeoutError(f"instance did not reach {want}")
            return real_wait(iid, want, timeout)

        lane.wait_state = wait_state
        with mock.patch.object(race.asyncio, "sleep", slept):
            with self.assertRaises(Refused):
                asyncio.run(lane.start_sample("i-1", True))
        self.assertEqual(events, [("wait", "stopped")])


class ColdLambda(unittest.TestCase):
    """Every lambda-cold sample is a cold start: a new execution environment before t0, an Init Duration after."""

    def lane(self, init):
        events = []

        class Lam:
            def get_function_configuration(self, FunctionName):
                return {"Environment": {"Variables": {"BOOT_RACE_EXIT_AFTER_RESPONSE": "1"}}}

            def update_function_configuration(self, FunctionName, Environment):
                events.append(("update", FunctionName, sorted(Environment["Variables"])))

            def get_waiter(self, name):
                class W:
                    def wait(self, **kw):
                        events.append(("waited", name))
                return W()

        class Logs:
            def filter_log_events(self, **kw):
                tail = f"\tInit Duration: {init} ms" if init is not None else ""
                return {"events": [{"message": f"REPORT RequestId: r1\tDuration: 2.00 ms{tail}\n"}]}

        lane = race.Lambda.__new__(race.Lambda)
        lane.cfg = {"function_urls": {"cold": "https://cold.example/", "warm": "https://warm.example/"}}
        lane.lam, lane.logs = Lam(), Logs()

        def call(url):
            events.append(("call", url))
            return {"t_start_unix_ms": 1000, "ms": 150.0, "request_id": "r1", "answer": {"proc_id": "a", "boot_id": "x"}}

        lane.call = call
        return lane, events

    def test_each_cold_sample_retires_the_environment_before_the_timed_call(self):
        lane, events = self.lane(12.27)
        s = asyncio.run(lane.sample("lambda-cold"))
        self.assertEqual(events[0], ("update", "boot-race-cold", ["BOOT_RACE_EXIT_AFTER_RESPONSE", "BOOT_RACE_GENERATION"]))
        self.assertEqual(events[1], ("waited", "function_updated_v2"))
        self.assertEqual(events[2], ("call", "https://cold.example/"))
        self.assertEqual(s["init_duration_ms"], 12.27)
        self.assertIsNone(race.check_cold("lambda-cold", s))

    def test_a_cold_sample_without_init_duration_is_refused(self):
        lane, events = self.lane(None)
        s = asyncio.run(lane.sample("lambda-cold"))
        self.assertIsNone(s["init_duration_ms"])
        self.assertIn("not a cold start", race.check_cold("lambda-cold", s))

    def test_the_warm_lane_does_not_touch_the_configuration(self):
        lane, events = self.lane(None)
        asyncio.run(lane.sample("lambda-warm"))
        self.assertEqual(events, [("call", "https://warm.example/")])


class LaunchTiming(unittest.TestCase):
    def test_t0_is_the_launchers_moment_before_runtask_not_the_invoke(self):
        # invoked at wall 1000.0 ms (monotonic 0); RunTask asked at 1040 and answered at 1090;
        # the invoke returned 120 ms after it was sent; the workload answered 5000 ms after the invoke
        launch = race.Launch((1000.0, 0, {"before_unix_ms": 1040.0, "after_unix_ms": 1090.0}))
        t = launch.timing(5_000_000_000, 120_000_000)
        self.assertEqual(t["ms"], 4960.0)
        self.assertEqual(t["invoke_to_runtask_ms"], 40.0)
        self.assertEqual(t["runtask_ms"], 50.0)
        self.assertEqual(t["runtask_to_return_ms"], 30.0)
        self.assertEqual(t["t_start_unix_ms"], 1040)


class Probe(unittest.TestCase):
    def test_answer_is_timed_at_its_last_byte_not_at_connection_close(self):
        async def main():
            async def handler(reader, writer):
                await reader.readuntil(b"\r\n\r\n")
                body = b'{"ok":true,"proc_id":"a","boot_id":"x"}'
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
                await writer.drain()
                await asyncio.sleep(2)  # hold the connection open well past the answer
                writer.close()

            server = await asyncio.start_server(handler, "127.0.0.1", 0)
            port = server.sockets[0].getsockname()[1]
            t0 = time.monotonic_ns()
            t1, doc, attempts = await race.probe("127.0.0.1", port, 0.01, 5, attempt_timeout=3)
            server.close()
            return (t1 - t0) / 1e9, doc

        elapsed, doc = asyncio.run(main())
        self.assertEqual(doc["proc_id"], "a")
        self.assertLess(elapsed, 1.0)

    def test_probe_fails_when_the_watched_child_exits(self):
        class Dead:
            returncode = 3

            def poll(self):
                return 3

        async def main():
            await race.probe("127.0.0.1", 9, 0.01, 5, child=Dead())

        with self.assertRaisesRegex(RuntimeError, "exited with 3"):
            asyncio.run(main())


if __name__ == "__main__":
    unittest.main()
