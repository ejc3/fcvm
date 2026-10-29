#!/usr/bin/env python3
"""Offline checks for publishing and the live IPv4/name-regression probe."""
import importlib.util
import contextlib
import fnmatch
import io
import copy
import json
import os
import socket
import struct
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

    def test_with_admission_the_token_is_the_first_message_never_the_url(self):
        calls, sent = [], []
        class Renderer:
            def __init__(self, *args, **kwargs): calls.append((args, kwargs))
            def send(self, payload, opcode=1): sent.append(payload)
            def message(self, deadline): return 2, b"synthetic frame"
            def close(self): pass
        with patch.object(smoke, "Socket", Renderer):
            smoke.prove_frame(self.tile(), "v1.9.v-1.mac")
        self.assertEqual(calls, [((self.tile()["frame_url_v4"] + "?h=180",), {})])
        self.assertEqual(sent, [{"admit": "v1.9.v-1.mac"}])

    def test_a_socket_without_a_token_must_be_closed_without_frames(self):
        class Streams:
            def __init__(self, *args): pass
            def message(self, deadline): return 2, b"frame"
            def close(self): pass
        class Closes:
            def __init__(self, *args): pass
            def message(self, deadline): raise RuntimeError("WebSocket closed")
            def close(self): pass
        class Open:
            def __init__(self, *args): pass
            def message(self, deadline): raise TimeoutError("WebSocket response timeout")
            def close(self): pass
        with patch.object(smoke, "Socket", Streams), self.assertRaises(AssertionError):
            smoke.prove_refused(self.tile())
        with patch.object(smoke, "Socket", Open), self.assertRaises(AssertionError):
            smoke.prove_refused(self.tile())
        with patch.object(smoke, "Socket", Closes):
            smoke.prove_refused(self.tile())

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

    def test_http_checks_identify_the_probe_and_preserve_access_headers(self):
        requests = []
        class Reply:
            headers = {}
            def __init__(self, status): self.status = status
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self, count): return b""
        class Gateway:
            def open(self, request, timeout):
                requests.append(request)
                if request.get_header("User-agent") != "fcvm-stage-smoke/1":
                    return Reply(403)
                return Reply(200 if request.get_header("Cf-access-client-secret") else 302)
        headers = {"CF-Access-Client-Id": "synthetic-id", "CF-Access-Client-Secret": "synthetic-secret"}
        with patch.object(smoke.urllib.request, "build_opener", return_value=Gateway()):
            public, _, _ = smoke.get("https://stage.example.test/")
            authenticated, _, _ = smoke.get("https://stage.example.test/", headers)
        self.assertEqual((public, authenticated), (302, 200))
        self.assertIsNone(requests[0].get_header("Cf-access-client-secret"))
        self.assertEqual(requests[1].get_header("Cf-access-client-id"), "synthetic-id")
        self.assertEqual(requests[1].get_header("Cf-access-client-secret"), "synthetic-secret")

    def test_a_fast_fcvm_restore_does_not_trip_the_shared_relaunch_limit(self):
        self.run_smoke(admission=None)

    def test_with_admission_frames_are_probed_with_the_token_and_refused_without(self):
        frames, refused = self.run_smoke(admission={"type": "admission", "capacity": 500,
                                                    "direct": {"value": "v1.9.v-1.mac", "expires_at": 9}})
        self.assertEqual([a for _, a in frames], ["v1.9.v-1.mac"] * 4)
        self.assertEqual(len(refused), 2)

    def run_smoke(self, admission):
        clock = [100.0]
        sent, seats, frames, refused = [], [], [], []
        def tile(half, generation):
            label = half + ("-old" if generation == 1 else "-new")
            return {"id": half + ":1", "slot": half + "-1", "half": half, "phase": "running",
                    "generation": generation, "frame_url": f"wss://{label}.fcvm.dev/frames",
                    "frame_url_v4": f"wss://{label}-v4.fcvm.dev/frames"}
        class Viewer:
            def __init__(self, *args):
                self.state = {"tiles": [tile("fcvm", 1), tile("ec2", 1)]}
                self.queue = [{"type": "snapshot", "state": copy.deepcopy(self.state)}]
                if admission is not None:
                    self.queue.insert(0, admission)
                self.last = None
            def send(self, message):
                if message.get("type") == "seat":
                    seats.append(len(sent))
                    return
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
                patch.object(smoke, "prove_frame", lambda t, a=None: frames.append((t, a))), \
                patch.object(smoke, "prove_refused", lambda t: refused.append(t)), \
                patch.object(smoke, "prove_resolves", return_value=0.05), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), \
                patch.object(smoke.time, "sleep", lambda delay: clock.__setitem__(0, clock[0] + delay)):
            smoke.main()
        self.assertEqual(len(sent), 2)
        self.assertGreaterEqual(sent[1] - sent[0], 2.0)
        self.assertEqual(seats, [0], "the seat message is the first on /live")
        if admission is None:
            self.assertEqual([a for _, a in frames], [None] * 4)
            self.assertEqual(refused, [])
        return frames, refused


class FailureTests(unittest.TestCase):
    """A failed smoke prints the step it was on and fixed error text (issue 1437: a run
    printed only `RuntimeError`, which every WebSocket failure of either socket raises)."""

    def failed(self, raise_in_message):
        class Viewer:
            def __init__(self, *args): pass
            def send(self, message): pass
            def message(self, deadline): raise_in_message()
            def close(self): pass
        def get(url, headers=None):
            if not headers:
                return 302, {"Location": "https://login.cloudflareaccess.com/login"}, b""
            return (403 if url.endswith("/agent") else 200), {}, b""
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Viewer), patch.object(smoke, "get", get), \
                contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            smoke.run()
        return json.loads(out.getvalue().strip().splitlines()[-1])

    def test_a_closed_live_socket_names_its_step_and_message(self):
        def closed():
            raise RuntimeError("WebSocket closed")
        r = self.failed(closed)
        self.assertEqual(r, {"status": "failed", "step": "/live: wait for an fcvm tile to run",
                             "errorType": "RuntimeError", "error": "WebSocket closed"})

    def test_an_os_error_prints_its_errno_name_and_foreign_text_prints_nothing(self):
        def reset():
            raise ConnectionResetError(104, "Connection reset by peer from 192.0.2.7")
        self.assertEqual(self.failed(reset)["error"], "ECONNRESET")
        def foreign():
            raise RuntimeError("wss://secret-host.example/frames?token=abc")
        r = self.failed(foreign)
        self.assertIsNone(r["error"])
        self.assertEqual(r["errorType"], "RuntimeError")

    def test_every_raise_in_the_smoke_carries_a_literal_message(self):
        import ast
        tree = ast.parse((HERE / "stage-smoke.py").read_text())
        for node in ast.walk(tree):
            if (isinstance(node, ast.Raise) and isinstance(node.exc, ast.Call)
                    and getattr(node.exc.func, "id", None) != "SystemExit"):
                args = node.exc.args
                self.assertTrue(len(args) == 1 and isinstance(args[0], ast.Constant)
                                and isinstance(args[0].value, str),
                                f"line {node.lineno}: a raise whose message is not a string literal")


class ResolveTests(unittest.TestCase):
    """The public-resolver check against a resolver on loopback that answers as it is told."""

    def resolver(self, answer):
        import threading
        server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        server.bind(("127.0.0.1", 0))
        self.addCleanup(server.close)
        def serve():
            try:
                query, peer = server.recvfrom(4096)
            except OSError:
                return
            for reply in answer(query):
                server.sendto(reply, peer)
        threading.Thread(target=serve, daemon=True).start()
        return server.getsockname()[1]

    @staticmethod
    def reply(query, rcode=0, tc=False, records=()):
        """The reply to query: its question, then (type, rdata) answers owned by a pointer to it."""
        question = query[12:len(query) - 11]
        flags = 0x8180 | rcode | (0x0200 if tc else 0)
        out = query[:2] + struct.pack(">HHHHH", flags, 1, len(records), 0, 0) + question
        for kind, rdata in records:
            out += b"\xc0\x0c" + struct.pack(">HHIH", kind, 1, 60, len(rdata)) + rdata
        return out

    def ask(self, answer, qtype=28):
        port = self.resolver(answer)
        with patch.object(smoke, "DNS_PORT", port):
            return smoke.resolve("127.0.0.1", "slot-1-abcdefgh.tiles.fcvm.dev", qtype, 0.5)

    def test_an_answer_is_read(self):
        v6 = socket.inet_pton(socket.AF_INET6, "2001:db8::1")
        got = self.ask(lambda q: [self.reply(q, records=[(5, b"\x00"), (28, v6)])])
        self.assertEqual([str(a) for a in got], ["2001:db8::1"])

    def test_a_reply_to_another_query_is_ignored(self):
        v4 = socket.inet_aton("192.0.2.1")
        got = self.ask(lambda q: [b"\x00\x00" + self.reply(q)[2:], self.reply(q, records=[(1, v4)])], qtype=1)
        self.assertEqual([str(a) for a in got], ["192.0.2.1"])

    def test_nxdomain_servfail_and_truncation_fail(self):
        for answer in (lambda q: [self.reply(q, rcode=3)], lambda q: [self.reply(q, rcode=2)],
                       lambda q: [self.reply(q, tc=True)]):
            with self.subTest(), self.assertRaises(AssertionError):
                self.ask(answer)

    def test_no_answer_in_time_fails(self):
        with self.assertRaises(TimeoutError):
            self.ask(lambda q: [])

    def test_a_cut_reply_or_a_bad_label_raises(self):
        _, q = smoke.dns_query("x.fcvm.dev", 1)
        cut = self.reply(q, records=[(1, b"\x01\x02\x03\x04")])[:-2]
        with self.assertRaises(ValueError):
            smoke.addresses(cut, q[:2], q, 1)
        bad = self.reply(q)[:6] + b"\x00\x01" + self.reply(q)[8:] + b"\x40" + b"a" * 64
        with self.assertRaises(ValueError):
            smoke.addresses(bad, q[:2], q, 1)

    def test_a_fresh_name_must_answer_everywhere_within_the_bound(self):
        tile = {"frame_url": "wss://slot-1-abcdefgh.tiles.fcvm.dev/frames",
                "frame_url_v4": "wss://slot-1-abcdefgh-v4.tiles.fcvm.dev:20200/frames"}
        asked = []
        def resolve(resolver, name, qtype, timeout):
            asked.append((resolver, name, qtype))
            return [] if resolver == "9.9.9.9" and qtype == 1 else ["x"]
        with patch.object(smoke, "resolve", resolve), self.assertRaises(AssertionError):
            smoke.prove_resolves(tile)
        self.assertEqual(len(asked), 6)
        clock = [0.0]
        def slow(resolver, name, qtype, timeout):
            clock[0] += 2.5
            return ["x"]
        with patch.object(smoke, "resolve", slow), patch.object(smoke.time, "monotonic", lambda: clock[0]), \
                self.assertRaises(AssertionError):
            smoke.prove_resolves(tile)
        with patch.object(smoke, "resolve", lambda *a: ["x"]):
            self.assertLess(smoke.prove_resolves(tile), 2.0)


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


class ReleaseInvocationTests(unittest.TestCase):
    """The Lambda invocation: what a failed release prints, and a call that ends without a result."""

    DIGEST = "sha256:" + "c" * 64

    def run_publish(self, results):
        """Run main() with each `aws lambda invoke` taking the next entry: a result dict, or an exit status."""
        results, sleeps = list(results), []
        def command(args, body=None, env=None, allow_missing=False):
            if args[1] == "ecr":
                return json.dumps({"imageDetails": [{"imageDigest": self.DIGEST}]})
            outcome = results.pop(0)
            if isinstance(outcome, int):
                raise publish.CommandFailed(args[0], outcome)
            Path(args[-1]).write_text(json.dumps(outcome))
            return json.dumps({"StatusCode": 200})
        env = {"GITHUB_SHA": "a" * 40, "COORDINATOR_REPO": "111122223333.dkr.ecr.us-east-2.amazonaws.com/fcvm-fleet/coordinator",
               "COORDINATOR_RELEASE": "fcvm-coordinator-release"}
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(publish, "command", command), \
                patch.object(publish.time, "sleep", sleeps.append), contextlib.redirect_stdout(out):
            try:
                publish.main()
                error = None
            except Exception as caught:
                error = caught
        self.assertEqual(results, [], "not every scripted invocation ran")
        return out.getvalue(), error, sleeps

    def test_a_failed_release_prints_its_step_and_code_and_nothing_else(self):
        printed, error, _ = self.run_publish([{
            "status": "failed", "errorType": "ClientError", "step": "unlock",
            "code": "ConditionalCheckFailedException", "message": "secret detail", "taskDefinition": "secret"}])
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual([json.loads(line) for line in printed.splitlines()],
                         [{"release": {"step": "unlock", "code": "ConditionalCheckFailedException"}}])

    def test_a_field_that_is_not_a_plain_token_is_not_printed(self):
        printed, error, _ = self.run_publish([{"status": "failed", "step": "rollout", "code": "has spaces: secret"}])
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual([json.loads(line) for line in printed.splitlines()], [{"release": {"step": "rollout"}}])

    def test_an_invocation_that_ends_without_a_result_is_retried_until_the_release_reports(self):
        printed, error, sleeps = self.run_publish(
            [255, 254, {"status": "busy"}, {"status": "unchanged", "digest": self.DIGEST}])
        self.assertIsNone(error)
        self.assertEqual(sleeps, [60, 60, 60])
        self.assertEqual(json.loads(printed.splitlines()[-1])["status"], "unchanged")

    def test_invocations_that_never_return_a_result_still_fail(self):
        printed, error, sleeps = self.run_publish([255] * 11)
        self.assertIsInstance(error, publish.CommandFailed)
        self.assertEqual(len(sleeps), 10)


class WorkflowTests(unittest.TestCase):
    def test_changes_to_the_actual_cli_files_trigger_a_release(self):
        workflow = (HERE.parent.parent / ".github/workflows/fleet-coordinator-deploy.yml").read_text()
        import re
        patterns = re.findall(r"^      - '([^']+)'", workflow, re.M)
        for path in ("src/cli/mod.rs", "src/cli/args.rs"):
            self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns), path)


if __name__ == "__main__":
    unittest.main(buffer=True)
