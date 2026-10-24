#!/usr/bin/env python3
"""Offline checks for publishing and the live IPv4/name-regression probe."""
import importlib.util
import os
import socket
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


smoke, publish = load("smoke", "stage-smoke.py"), load("publish", "publish.py")


class ProbeTests(unittest.TestCase):
    def tile(self):
        return dict(id="fcvm:1", half="fcvm", slot="slot-1", generation=1, phase="running",
                    frame_url="wss://old.fcvm.dev/frames", frame_url_v4="wss://old-v4.fcvm.dev:20200/frames")

    def test_a_running_tile_without_ipv4_never_passes(self):
        tile = self.tile()
        tile["frame_url_v4"] = None
        self.assertIsNone(smoke.ready({"tiles": [tile]}, "fcvm"))

    def test_a_tile_must_have_both_new_names_and_a_new_generation(self):
        old = self.tile()
        for changed in ({"generation": 2}, {"generation": 2, "frame_url": "wss://new.fcvm.dev/frames"},
                        {"frame_url": "wss://new.fcvm.dev/frames", "frame_url_v4": "wss://new-v4.fcvm.dev/frames"}):
            with self.subTest(changed=changed), self.assertRaises(AssertionError):
                smoke.fresh(old, {**old, **changed})
        smoke.fresh(old, {**old, "generation": 2, "frame_url": "wss://new.fcvm.dev/frames",
                          "frame_url_v4": "wss://new-v4.fcvm.dev/frames"})

    def test_waits_for_the_same_slot_at_a_later_generation(self):
        old = self.tile()
        other = {**old, "slot": "slot-2", "generation": 2}
        self.assertIsNone(smoke.ready({"tiles": [old, other]}, "fcvm", slot="slot-1", after=1))

    def test_renderer_urls_cannot_send_credentials_or_dial_arbitrary_hosts(self):
        for url in ("ws://x.fcvm.dev/frames", "wss://elsewhere.example/frames", "wss://user@x.fcvm.dev/frames",
                    "wss://x.fcvm.dev:22/frames", "wss://x.fcvm.dev/other", "wss://x.fcvm.dev/frames?token=x"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                smoke.frame_host(url)

    def test_ipv4_probe_rejects_loopback_and_private_addresses(self):
        for address in ("127.0.0.1", "10.0.0.1", "169.254.169.254"):
            with patch.object(socket, "getaddrinfo", return_value=[(None, None, None, None, (address, 443))]) as resolve:
                with patch.object(socket, "create_connection") as connect, self.assertRaises(RuntimeError):
                    smoke.ipv4("tile.example", 443)
                connect.assert_not_called()
                self.assertEqual(resolve.call_args.args[2], socket.AF_INET)

    def test_tcp_or_websocket_connection_without_frames_does_not_pass(self):
        class Empty:
            def __init__(self, *args): self.closed = False
            def message(self, deadline): raise TimeoutError("no frames")
            def close(self): self.closed = True
        with patch.object(smoke, "Socket", Empty), self.assertRaises(TimeoutError):
            smoke.prove_frame(self.tile())

    def test_a_renderer_receives_no_access_credentials(self):
        calls = []
        class Renderer:
            def __init__(self, *args, **kwargs): calls.append((args, kwargs))
            def message(self, deadline): return 2, b"synthetic frame"
            def close(self): pass
        with patch.object(smoke, "Socket", Renderer):
            smoke.prove_frame(self.tile())
        self.assertEqual(calls, [((self.tile()["frame_url_v4"],), {})])


class PublishTests(unittest.TestCase):
    def test_malformed_commit_or_repository_never_runs_a_command(self):
        env = {"GITHUB_SHA": "main", "COORDINATOR_REPO": "registry.example/image", "COORDINATOR_RELEASE": "fcvm-coordinator-release"}
        with patch.dict(os.environ, env), patch.object(publish, "command") as command:
            with self.assertRaises(ValueError): publish.main()
            command.assert_not_called()
        env["GITHUB_SHA"] = "a" * 40
        with patch.dict(os.environ, env), patch.object(publish, "command") as command:
            with self.assertRaises(ValueError): publish.main()
            command.assert_not_called()


if __name__ == "__main__":
    unittest.main(buffer=True)
