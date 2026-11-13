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
