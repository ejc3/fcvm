#!/usr/bin/env python3
"""Offline checks for publishing and the live IPv4/name-regression probe."""
import importlib.util
import fnmatch
import copy
import json
import os
import socket
import base64
import hashlib
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
        self.assertEqual(calls, [((self.tile()["frame_url_v4"] + "?h=180",), {})])

    def test_renderer_handshake_requests_a_valid_frame_height(self):
        class Transport:
            closed = False
            request = b""
            pending = b""
            def sendall(self, request):
                self.request = request
                lines = request.decode().split("\r\n")
                headers = dict(line.split(": ", 1) for line in lines[1:] if ": " in line)
                if lines[0] != "GET /frames?h=180 HTTP/1.1":
                    self.pending = b"HTTP/1.1 400 Bad Request\r\n\r\n"
                    return
                accept = base64.b64encode(hashlib.sha1((headers["Sec-WebSocket-Key"] +
                    "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
                self.pending = (b"HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: " +
                                accept + b"\r\n\r\n\x82\x01x")
            def recv(self, count):
                result, self.pending = self.pending[:count], self.pending[count:]
                return result
            def settimeout(self, timeout): pass
            def close(self): self.closed = True
        transport, error = Transport(), None
        with patch.object(smoke, "ipv4", return_value=transport):
            try:
                smoke.prove_frame(self.tile())
            except RuntimeError as caught:
                error = caught
        self.assertEqual(transport.request.split(b"\r\n", 1)[0], b"GET /frames?h=180 HTTP/1.1")
        self.assertIsNone(error)
        self.assertTrue(transport.closed)

    def test_a_fast_fcvm_restore_does_not_trip_the_shared_relaunch_limit(self):
        clock = [100.0]
        sent = []
        def tile(half, generation):
            label = half + ("-old" if generation == 1 else "-new")
            return {"id": half + ":1", "slot": half + "-1", "half": half, "phase": "running",
                    "generation": generation, "frame_url": f"wss://{label}.fcvm.dev/frames",
                    "frame_url_v4": f"wss://{label}-v4.fcvm.dev/frames"}
        class Viewer:
            def __init__(self, *args):
                self.state = {"tiles": [tile("fcvm", 1), tile("ec2", 1)]}
                self.queue = [{"type": "snapshot", "state": copy.deepcopy(self.state)}]
                self.last = None
            def send(self, message):
                sent.append(clock[0])
                allowed = self.last is None or clock[0] - self.last >= 2.0
                self.queue.append({"type": "receipt", "id": message["id"],
                                   "outcome": "committed" if allowed else "refused"})
                if allowed:
                    self.last = clock[0]
                    half = message["args"]["tile"].split(":")[0]
                    self.state["tiles"] = [tile(t["half"], 2 if t["half"] == half else t["generation"])
                                           for t in self.state["tiles"]]
                    self.queue.append({"type": "snapshot", "state": copy.deepcopy(self.state)})
            def message(self, deadline):
                message = self.queue.pop(0)
                clock[0] += 0.4 if message["type"] == "snapshot" else 0.1
                return 1, json.dumps(message).encode()
            def close(self): pass
        def get(url, headers=None):
            if not headers:
                return 302, {"Location": "https://login.cloudflareaccess.com/login"}, b""
            return (403 if url.endswith("/agent") else 200), {}, b""
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Viewer), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_frame"), patch.object(smoke.time, "monotonic", lambda: clock[0]), \
                patch.object(smoke.time, "sleep", lambda delay: clock.__setitem__(0, clock[0] + delay)):
            smoke.main()
        self.assertEqual(len(sent), 2)
        self.assertGreaterEqual(sent[1] - sent[0], 2.0)


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


class WorkflowTests(unittest.TestCase):
    def test_changes_to_the_actual_cli_files_trigger_a_release(self):
        workflow = (HERE.parent.parent / ".github/workflows/fleet-coordinator-deploy.yml").read_text()
        import re
        patterns = re.findall(r"^      - '([^']+)'", workflow, re.M)
        for path in ("src/cli/mod.rs", "src/cli/args.rs"):
            self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns), path)


if __name__ == "__main__":
    unittest.main(buffer=True)
