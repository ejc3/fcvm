#!/usr/bin/env python3
"""Tests for race.py: the identity checks that refuse a mislabelled sample, and the probe's timing of an answer.

    python3 -m unittest bench/boot-race/test_race.py
"""

import asyncio
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

    def test_cold_lambda_is_not_refused_here(self):
        # its evidence (Init Duration, new process, new kernel) is reported on the page instead
        self.assertIsNone(race.check_identity("lambda-cold", A, A, {}))


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
