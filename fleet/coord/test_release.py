#!/usr/bin/env python3
"""Offline checks for publishing and the live IPv4/name-regression probe."""
import subprocess
import textwrap
import importlib.util
import contextlib
import fnmatch
import io
import copy
import errno
import ipaddress
import json
import types
import re
import os
import socket
import ssl
import struct
import base64
import hashlib
import tempfile
import threading
import time
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
prod = load("prod_release", "prod_release.py")


def ssl_context_raising(error):
    """A TLS context whose handshake raises `error`."""
    class Context:
        def wrap_socket(self, raw, server_hostname=None):
            raise error
    return Context()


class ProbeTests(unittest.TestCase):
    def tile(self):
        return dict(id="fcvm:1", half="fcvm", slot="slot-1", generation=1, phase="running",
                    frame_url="wss://old.fcvm.dev/frames", frame_url_v4="wss://old-v4.fcvm.dev:20200/frames")

    def test_an_unreachable_endpoint_says_whether_tcp_or_tls_failed(self):
        # Run 45032617135: a relaunched EC2 tile's IPv4 frame socket failed after about 20 s with
        # "no reachable public endpoint in this family", which said nothing of whether TCP was
        # refused, TCP timed out or TLS failed. The probe opener (`opener`) re-raises its last
        # error; the relaunch checks' `public` must too.
        host = "x-v4.tiles.fcvm.dev"
        found = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("1.0.0.1", 443))]
        with patch.object(smoke.socket, "getaddrinfo", return_value=found):
            for error in (TimeoutError("timed out"), ConnectionRefusedError(errno.ECONNREFUSED, "refused")):
                with self.subTest(error=type(error).__name__), \
                        patch.object(smoke.socket, "create_connection", side_effect=error), \
                        self.assertRaises(type(error)) as caught:
                    smoke.public(host, 443, "v4")
                self.assertIn(smoke.fixed(caught.exception)[1], ("timed out", "ECONNREFUSED"))
            # TCP answered and the TLS handshake did not finish.
            near, far = socket.socketpair()
            self.addCleanup(far.close)
            context = ssl_context_raising(socket.timeout("_ssl.c: The handshake operation timed out"))
            with patch.object(smoke.socket, "create_connection", return_value=near), \
                    patch.object(smoke.ssl, "create_default_context", return_value=context), \
                    self.assertRaises(RuntimeError) as caught:
                smoke.public(host, 443, "v4")
            self.assertEqual(smoke.describe(caught.exception), "the TLS handshake did not finish in time")
            self.assertEqual(near.fileno(), -1, "the TCP socket is closed")

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

    def test_an_ec2_relaunch_waits_out_the_cooldown_from_the_tiles_last_launch(self):
        # The coordinator refuses an EC2 relaunch within EC2_RELAUNCH_COOLDOWN_MS of the
        # tile's last launch (`last_relaunch.clicked_at`, which a wake also sets).
        cooldown = smoke.ec2_relaunch_cooldown_seconds()
        self.assertEqual(cooldown, 90)
        tile = {**self.tile(), "half": "ec2", "last_relaunch": {"clicked_at": 1_000_000}}
        self.assertAlmostEqual(smoke.relaunch_wait(tile, 1_000_000 + 30_000, cooldown),
                               60 + smoke.CLOCK_SKEW_SECONDS)
        self.assertEqual(smoke.relaunch_wait(tile, 1_000_000 + 200_000, cooldown), 0)
        # fcvm tiles have no launch cooldown.
        self.assertEqual(smoke.relaunch_wait({**tile, "half": "fcvm"}, 1_000_000, cooldown), 0)
        # An EC2 tile with no launch on record waits the whole cooldown.
        self.assertEqual(smoke.relaunch_wait({**tile, "last_relaunch": None}, 1_000_000, cooldown),
                         cooldown + smoke.CLOCK_SKEW_SECONDS)

    def test_a_relaunch_refusal_names_its_reason_in_fixed_text(self):
        self.assertEqual(smoke.refusal_category("ec2-3 launched 5 s ago; it can be relaunched in 85 s"),
                         "the tile's launch cooldown")
        self.assertEqual(smoke.refusal_category("one relaunch per 30 s per client"),
                         "the per-viewer relaunch interval")
        self.assertEqual(smoke.refusal_category("wss://secret.example/x?token=abc"), "an unlisted reason")
        self.assertEqual(smoke.refusal_category(None), "an unlisted reason")
        # The failure line carries the categorized message, and nothing else it was not given.
        for why in {w for _, w in smoke.REFUSALS} | {"an unlisted reason"}:
            message = f"staging refused the relaunch: {why}"
            self.assertEqual(smoke.describe(AssertionError(message)), message)
        self.assertIsNone(smoke.describe(AssertionError("staging refused the relaunch: wss://x.example/?t=1")))
        # The relaunch step raises with the template the allowlist is built from.
        import ast
        tree = ast.parse((HERE / "stage-smoke.py").read_text())
        templates = [ast.unparse(n.exc.args[0]) for n in ast.walk(tree)
                     if isinstance(n, ast.Raise) and isinstance(n.exc, ast.Call) and n.exc.args
                     and isinstance(n.exc.args[0], ast.JoinedStr) and "refused the relaunch" in ast.unparse(n.exc.args[0])]
        self.assertEqual(templates, ["f'staging refused the relaunch: {why}'"])

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
            def message(self, deadline): raise smoke.WebSocketClosed("WebSocket closed")
            def close(self): pass
        class Malformed(Closes):
            def message(self, deadline): raise RuntimeError("masked server frame")
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
        with patch.object(smoke, "Socket", Malformed), self.assertRaises(RuntimeError):
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
                self.queue.insert(0, admission or {"type": "admission", "capacity": None})
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
                patch.object(smoke, "prove_resolves", return_value=(0.05, 1)), \
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
            smoke.run(["--relaunch"])
        return json.loads(out.getvalue().strip().splitlines()[-1])

    def test_a_closed_live_socket_names_its_step_and_message(self):
        def closed():
            raise RuntimeError("WebSocket closed")
        r = self.failed(closed)
        self.assertEqual(r, {"status": "failed", "step": "/live: wait for an fcvm tile to run",
                             "errorType": "RuntimeError", "error": "WebSocket closed"})

    def test_an_os_error_prints_its_errno_name_and_foreign_text_prints_nothing(self):
        def reset():
            raise ConnectionResetError(errno.ECONNRESET, "Connection reset by peer from 192.0.2.7")
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
                # An f-string may interpolate only these names: a probe's role, path, status code
                # and reason, never a URL or a header value.
                def literal(arg):
                    if isinstance(arg, ast.JoinedStr):
                        return all(isinstance(v, ast.Constant) or
                                   (isinstance(v.value, ast.Name) and v.value.id in {"role", "path", "code", "why"})
                                   for v in arg.values)
                    return isinstance(arg, ast.Constant) and isinstance(arg.value, str)
                self.assertTrue(len(args) == 1 and literal(args[0]),
                                f"line {node.lineno}: a raise whose message is not a string literal")


class ReconnectTests(unittest.TestCase):
    def run_probe(self, at, *, failures=1, wire="eof", advance=0, open_cost=0):
        clock, connections, intents, frames, seats, deadlines = [100.0], [], [], [], [], []
        closed, attempts, applied = [0], [], set()
        def tile(half, generation=1):
            name = f"{half}-{generation}"
            return {"id": half + ":1", "half": half, "slot": half + "-1", "generation": generation,
                    "phase": "running", "frame_url": f"wss://{name}.fcvm.dev/frames",
                    "frame_url_v4": f"wss://{name}-v4.fcvm.dev/frames"}
        state = {"tiles": [tile("fcvm"), tile("ec2")]}
        if at == "cold":
            state["tiles"][0]["phase"] = "starting"
        real_socket = smoke.Socket
        class Transport:
            def __init__(self):
                self.pending = {"eof": b"", "close": b"\x88\x02\x03\xe8",
                                "protocol": b"\x82\x81"}[wire]
            def recv(self, count):
                result, self.pending = self.pending[:count], self.pending[count:]
                return result
            def settimeout(self, timeout): pass
        def fail_connection(deadline):
            connection = real_socket.__new__(real_socket)
            connection.s = Transport()
            return connection.message(deadline)
        class Viewer:
            def __init__(self, *args):
                self.number, self.closed = len(connections), False
                connections.append(self)
                if self.number:
                    clock[0] += open_cost
                admission = {"type": "admission", "seat": "synthetic-seat", "locked": False,
                             "capacity": 500, "direct": {"value": f"token-{self.number}"}}
                snapshot = {"type": "snapshot", "state": copy.deepcopy(state)}
                # After a reconnect the snapshot deliberately precedes the fresh token.
                self.queue = [admission, snapshot] if self.number == 0 else [snapshot, admission]
                if at == "cold" and closed[0] < failures:
                    self.queue.append(None)
            def send(self, message):
                if message["type"] == "seat":
                    self.first = message
                    seats.append(message["ticket"])
                    return
                if not hasattr(self, "first"):
                    raise AssertionError("an intent preceded the seat message")
                attempts.append(message["id"])
                if at == "send_eof" and self.number == 0:
                    # The connection died while the probe was not reading: the intent never
                    # leaves (run 44764745380).
                    raise ssl.SSLEOFError(8, "EOF occurred in violation of protocol")
                intents.append(message)
                if message["id"] in applied and at == "receipt_always":
                    self.queue.append(None)
                    return
                if message["id"] in applied:
                    # A repeated id gets its first receipt and changes nothing.
                    self.queue.append({"type": "receipt", "id": message["id"], "outcome": "committed"})
                    self.queue.append({"type": "snapshot", "state": copy.deepcopy(state)})
                    return
                applied.add(message["id"])
                half = message["args"]["tile"].split(":")[0]
                state["tiles"] = [tile(t["half"], 2 if t["half"] == half else t["generation"])
                                  for t in state["tiles"]]
                if (at == "receipt" and self.number == 0) or at == "receipt_always":
                    self.queue.append(None)
                    return
                self.queue.append({"type": "receipt", "id": message["id"], "outcome": "committed"})
                self.queue.append(None if at == "ec2_fresh" and half == "ec2"
                                  else {"type": "snapshot", "state": copy.deepcopy(state)})
            def message(self, deadline):
                deadlines.append(deadline)
                clock[0] += 0.1
                message = self.queue.pop(0)
                if message is None:
                    closed[0] += 1
                    clock[0] += advance
                    if closed[0] >= failures:
                        state["tiles"][0]["phase"] = "running"
                    return fail_connection(deadline)
                return 1, json.dumps(message).encode()
            def close(self): self.closed = True
        def get(url, headers=None):
            return ((403 if url.endswith("/agent") else 200), {}, b"") if headers else (
                302, {"Location": "https://login.cloudflareaccess.com/login"}, b"")
        def prove(tile, admit=None):
            if at == "renderer":
                fail_connection(clock[0] + 40)
            frames.append((tile["half"], tile["generation"], admit))
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        output, error = io.StringIO(), None
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Viewer), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_frame", prove), patch.object(smoke, "prove_refused"), \
                patch.object(smoke, "prove_resolves", return_value=(0.05, 1)), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), \
                patch.object(smoke.time, "sleep", lambda delay: clock.__setitem__(0, clock[0] + delay)), \
                contextlib.redirect_stdout(output):
            try:
                smoke.main()
            except Exception as caught:
                error = caught
        return dict(error=error, connections=connections, intents=intents, frames=frames,
                    seats=seats, deadlines=deadlines, attempts=attempts,
                    # Without the commit attribution lines (SmokeCommitTests covers them).
                    output="".join(line for line in output.getvalue().splitlines(keepends=True)
                                   if '"tested_commit"' not in line and "not this run's" not in line))

    def test_cold_readiness_eof_reuses_the_seat_and_waits_for_a_new_token(self):
        result = self.run_probe("cold")
        self.assertIsNone(result["error"])
        self.assertEqual(result["seats"], [None, "synthetic-seat"])
        self.assertEqual([token for _, _, token in result["frames"]], ["token-1"] * 4)
        self.assertEqual([i["args"]["tile"] for i in result["intents"]], ["fcvm:1", "ec2:1"])
        self.assertTrue(all(c.closed for c in result["connections"]))

    def test_ec2_postreceipt_close_resumes_freshness_wait_without_replaying_intents(self):
        result = self.run_probe("ec2_fresh", wire="close")
        self.assertIsNone(result["error"])
        self.assertEqual(result["seats"], [None, "synthetic-seat"])
        self.assertEqual(result["frames"], [("fcvm", 1, "token-0"), ("fcvm", 2, "token-0"),
                                            ("ec2", 1, "token-0"), ("ec2", 2, "token-1")])
        self.assertEqual([i["args"]["tile"] for i in result["intents"]], ["fcvm:1", "ec2:1"])
        self.assertEqual(len({i["id"] for i in result["intents"]}), 2)
        self.assertTrue(all(c.closed for c in result["connections"]))

    def test_readiness_closures_exhaust_three_total_reconnects(self):
        result = self.run_probe("cold", failures=4)
        self.assertIsInstance(result["error"], RuntimeError)
        self.assertEqual(len(result["connections"]), 4)
        self.assertEqual(result["intents"], [])
        self.assertEqual([json.loads(line) for line in result["output"].splitlines()],
                         [{"reconnect": 1}, {"reconnect": 2}, {"reconnect": 3}])
        self.assertTrue(all(c.closed for c in result["connections"]))

    def test_reconnects_keep_the_original_deadline(self):
        result = self.run_probe("cold", failures=4, advance=300)
        self.assertIsInstance(result["error"], TimeoutError)
        self.assertEqual(len(result["connections"]), 3)
        self.assertEqual(set(result["deadlines"]), {820.0})

    def test_malformed_coordinator_frames_are_not_retried(self):
        result = self.run_probe("cold", wire="protocol")
        self.assertIsInstance(result["error"], RuntimeError)
        self.assertEqual(str(result["error"]), "masked server frame")
        self.assertEqual(len(result["connections"]), 1)

    def test_a_close_before_the_receipt_reconnects_and_sends_the_same_intent_id(self):
        # fleet/PROTOCOL.md: a page sends an intent again after it reconnects, and the coordinator
        # answers an id it has seen with that id's first receipt and changes nothing.
        result = self.run_probe("receipt")
        self.assertIsNone(result["error"])
        self.assertEqual(len(result["connections"]), 2)
        self.assertEqual(result["seats"], [None, "synthetic-seat"])
        fcvm = [i["id"] for i in result["intents"] if i["args"]["tile"] == "fcvm:1"]
        self.assertEqual(len(fcvm), 2)
        self.assertEqual(len(set(fcvm)), 1, "the replay reuses the intent's id")
        self.assertEqual(json.loads(result["output"].splitlines()[0]), {"reconnect": 1})
        self.assertTrue(all(c.closed for c in result["connections"]))

    def test_an_intent_sent_on_a_dead_connection_is_sent_again_on_a_new_one(self):
        # Run 44764745380: the probe sent the EC2 relaunch onto a /live connection that had
        # closed while it was not reading, and stopped with SSLEOFError.
        result = self.run_probe("send_eof")
        self.assertIsNone(result["error"])
        self.assertEqual(len(result["connections"]), 2)
        self.assertEqual(result["attempts"][0], result["attempts"][1], "the same intent id, sent again")
        self.assertEqual([i["args"]["tile"] for i in result["intents"]], ["fcvm:1", "ec2:1"])
        self.assertEqual(json.loads(result["output"].splitlines()[0]), {"reconnect": 1})

    def test_no_intent_is_sent_once_its_wait_has_run_out(self):
        # Greptile on #2003: opening the replacement connection can use the rest of the wait,
        # and the intent must not go out after it.
        result = self.run_probe("receipt", open_cost=1000)
        self.assertIsInstance(result["error"], TimeoutError)
        self.assertEqual(len(result["connections"]), 2)
        self.assertEqual(len(result["attempts"]), 1, "the intent was sent after its wait ran out")

    def test_intent_replays_share_the_three_reconnects(self):
        result = self.run_probe("receipt_always")
        self.assertIsInstance(result["error"], smoke.WebSocketClosed)
        self.assertEqual(len(result["connections"]), 4)
        self.assertEqual(len(set(result["attempts"])), 1)

    def test_a_renderer_close_is_not_a_coordinator_reconnect(self):
        result = self.run_probe("renderer")
        self.assertIsInstance(result["error"], RuntimeError)
        self.assertEqual(len(result["connections"]), 1)
        self.assertEqual(result["intents"], [])


class FakeViewerServer:
    """A tile on loopback as a viewer meets it: TCP, then TLS with a throwaway CA, the
    WebSocket upgrade, the client's first message, then what `behave` sends."""

    def __init__(self, test, family, behave, tls=True):
        import ssl as _ssl
        import threading
        self.behave, self.first = behave, []
        af, host = (socket.AF_INET, "127.0.0.1") if family == "v4" else (socket.AF_INET6, "::1")
        self.listener = socket.socket(af, socket.SOCK_STREAM)
        self.listener.bind((host, 0))
        self.listener.listen(8)
        self.port, self.host = self.listener.getsockname()[1], host
        test.addCleanup(self.listener.close)
        self.context = None
        if tls:
            self.context = _ssl.SSLContext(_ssl.PROTOCOL_TLS_SERVER)
            self.context.load_cert_chain(CERTS["cert"], CERTS["key"])
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                raw, _ = self.listener.accept()
            except OSError:
                return
            import threading
            threading.Thread(target=self.one, args=(raw,), daemon=True).start()

    def one(self, raw):
        try:
            if self.context is None:  # accepts TCP and never answers the TLS handshake
                raw.recv(4096)
                time.sleep(60)
                return
            s = self.context.wrap_socket(raw, server_side=True)
            head = b""
            while b"\r\n\r\n" not in head:
                head += s.recv(1)
            key = [l.split(b":", 1)[1].strip() for l in head.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key")][0]
            accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
            s.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                      b"Sec-WebSocket-Accept: " + accept + b"\r\n\r\n")
            self.first.append(read_client_message(s))
            self.behave(s, self)
            time.sleep(5)
        except OSError:
            pass


def read_client_message(s):
    s.recv(1)
    b = s.recv(1)[0]
    n = b & 127
    if n == 126:
        n = struct.unpack(">H", s.recv(2))[0]
    mask = s.recv(4)
    data = b""
    while len(data) < n:
        data += s.recv(n - len(data))
    return json.loads(bytes(c ^ mask[i % 4] for i, c in enumerate(data)))


def send_server(s, payload, opcode=1):
    if isinstance(payload, dict):
        payload = json.dumps(payload).encode()
    n = len(payload)
    size = bytes([n]) if n < 126 else (b"\x7e" + struct.pack(">H", n) if n < 65536 else b"\x7f" + struct.pack(">Q", n))
    s.sendall(bytes([0x80 | opcode]) + size + payload)


CERTS = {}


def make_certs(directory):
    """A throwaway CA and a leaf for *.tiles.fcvm.dev, as the host-image acceptance makes."""
    import subprocess
    d = Path(directory)
    run = lambda *a: subprocess.run(a, check=True, capture_output=True)
    # A CA without keyUsage is refused under VERIFY_X509_STRICT ("CA cert does not include key
    # usage extension"), which Python 3.13 and later set in create_default_context(), so on
    # GitHub's ubuntu-latest (Python 3.14) every TLS probe here failed at "connect".
    run("openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", str(d / "ca.key"), "-out", str(d / "ca.pem"), "-days", "1", "-subj", "/CN=viewer-path test CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    run("openssl", "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", str(d / "leaf.key"), "-out", str(d / "leaf.csr"), "-subj", "/CN=tiles.fcvm.dev")
    (d / "ext").write_text("subjectAltName=DNS:*.tiles.fcvm.dev\nbasicConstraints=CA:FALSE\n")
    run("openssl", "x509", "-req", "-in", str(d / "leaf.csr"), "-CA", str(d / "ca.pem"), "-CAkey", str(d / "ca.key"),
        "-CAcreateserial", "-out", str(d / "leaf.pem"), "-days", "1", "-extfile", str(d / "ext"))
    # A second CA that signed nothing here: a viewer trusting only it must refuse the tile. The
    # leaf itself is no such case: create_default_context() sets VERIFY_X509_PARTIAL_CHAIN on
    # Python 3.13 and later, which trusts a leaf named in the CA file directly.
    run("openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", str(d / "other-ca.key"), "-out", str(d / "other-ca.pem"), "-days", "1", "-subj", "/CN=another CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    CERTS.update(ca=str(d / "ca.pem"), cert=str(d / "leaf.pem"), key=str(d / "leaf.key"),
                 other_ca=str(d / "other-ca.pem"))


def h264(source):
    """Access units of a 64x48 H.264 stream of an ffmpeg lavfi source, split at their delimiters."""
    import subprocess
    out = subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", source + (":" if "=" in source else "=") + "size=64x48:rate=30", "-frames:v", "6",
                          "-c:v", "libx264", "-bf", "0", "-g", "30", "-bsf:v", "h264_metadata=aud=insert",
                          "-f", "h264", "pipe:1"], check=True, capture_output=True).stdout
    aud = b"\x00\x00\x00\x01\x09"
    return [aud + u for u in out.split(aud) if u]


def fcvf(unit, keyframe, n):
    return b"FCVF\x01" + bytes([1 if keyframe else 0]) + struct.pack("<HHdI", 64, 48, n / 30, n) + unit


def stream_of(source):
    units = h264(source)
    def behave(s, server):
        send_server(s, {"codec": "avc1.64000A", "width": 64, "height": 48})
        for i, u in enumerate(units):
            send_server(s, fcvf(u, i == 0, i), opcode=2)
    return behave


class ViewerPathTests(unittest.TestCase):
    """The viewer path against tiles on loopback: what each broken state of 2026-11 looked like
    to a viewer must fail, and a tile that streams a picture must pass."""

    @classmethod
    def setUpClass(cls):
        import tempfile
        cls.dir = tempfile.TemporaryDirectory()
        make_certs(cls.dir.name)

    @classmethod
    def tearDownClass(cls):
        cls.dir.cleanup()

    def test_the_test_ca_passes_strict_verification(self):
        # What create_default_context() does on Python 3.13 and later, whichever Python runs this.
        import ssl
        import threading
        server_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server_ctx.load_cert_chain(CERTS["cert"], CERTS["key"])
        client_ctx = ssl.create_default_context(cafile=CERTS["ca"])
        client_ctx.verify_flags |= ssl.VERIFY_X509_STRICT
        a, b = socket.socketpair()
        def serve():
            with contextlib.suppress(ssl.SSLError, OSError):
                server_ctx.wrap_socket(b, server_side=True).close()
        t = threading.Thread(target=serve)
        t.start()
        try:
            client_ctx.wrap_socket(a, server_hostname="win-1-abcdefgh-v4.tiles.fcvm.dev").close()
        finally:
            a.close()
            t.join(10)
            b.close()

    def tile(self, mode, server=None, family="v4", **extra):
        port = server.port if server else 20208
        host = "win-1-abcdefgh" + ("-v4" if family == "v4" else "") + ".tiles.fcvm.dev"
        path = "/view" if mode == "minecraft" else "/frames"
        url = f"wss://{host}:{port}{path}" if port != 443 else f"wss://{host}{path}"
        key = "frame_url_v4" if family == "v4" else "frame_url"
        return {"id": "t:1", "slot": "mc-1" if mode == "minecraft" else "win-1", "phase": "running",
                key: url, **extra}

    def probe(self, mode, tile, family="v4", server=None, admit="v1.9.v-1.mac", ports=None, half="fcvm", **kw):
        addr = ipaddress.ip_address(server.host) if server else ipaddress.ip_address("192.0.2.9")
        declared = ports if ports is not None else {server.port} if server else None
        patches = [patch.object(smoke, "lookup", lambda host, fam, r: [addr]),
                   patch.dict(os.environ, {"SMOKE_CA_FILE": CERTS["ca"]})]
        if declared is not None:
            patches.append(patch.object(smoke, "declared_ports", lambda m, f, h="fcvm": declared))
        with contextlib.ExitStack() as stack:
            for p in patches:
                stack.enter_context(p)
            stack.enter_context(patch.object(smoke, "PROBE_SECONDS", 3))
            return smoke.probe_tile(mode, half, tile, family, admit, "v-1", ["192.0.2.53"], **kw)

    def test_a_streaming_tile_decodes_to_a_picture_over_each_family(self):
        for family in ("v4", "v6"):
            with self.subTest(family=family):
                server = FakeViewerServer(self, family, stream_of("testsrc"))
                p = self.probe("windows", self.tile("windows", server, family), family, server)
                self.assertTrue(p.ok, p.line())
                self.assertGreaterEqual(p.detail["lumaSpread"], smoke.FLAT_SPREAD)
                self.assertEqual(server.first, [{"admit": "v1.9.v-1.mac"}], "the token is the first message")

    def test_a_tile_with_no_url_in_the_family_fails(self):
        # Minecraft before #1833: view sockets on IPv6 names only.
        tile = {"id": "mc:1", "slot": "mc-1", "phase": "running",
                "frame_url": "wss://mc-1-abcdefgh.tiles.fcvm.dev/view", "frame_url_v4": None}
        p = self.probe("minecraft", tile, "v4")
        self.assertFalse(p.ok)
        self.assertEqual((p.step, p.error[1]), ("published URL",
                         "the coordinator publishes no URL for this tile in this family"))

    def test_a_port_outside_the_declaration_fails_before_any_connection(self):
        with patch.object(smoke, "Socket", side_effect=AssertionError("connected")):
            for mode, port in (("windows", 20205), ("wall", 20208), ("minecraft", 20216), ("phones", 20300)):
                with self.subTest(mode=mode, port=port):
                    tile = {"id": "x:1", "slot": "x-1", "phase": "running",
                            "frame_url_v4": f"wss://x-1-abcdefgh-v4.tiles.fcvm.dev:{port}" + ("/view" if mode == "minecraft" else "/frames")}
                    p = smoke.probe_tile(mode, "fcvm", tile, "v4", None, "v-1", [])
                    self.assertEqual((p.ok, p.error[1]), (False, "a port outside fleet/published-ports.json for its mode and family"))
            tile = {"id": "x:1", "frame_url": "wss://x-1-abcdefgh.tiles.fcvm.dev:20208/frames"}
            self.assertFalse(smoke.probe_tile("windows", "fcvm", tile, "v6", None, "v-1", []).ok, "IPv6 is on 443 only")

    def test_the_declared_ports_are_the_agents(self):
        ports = json.loads((HERE.parent / "published-ports.json").read_text())
        base = ports["gpu_host_ipv4"]["base"]
        self.assertEqual(smoke.declared_ports("windows", "v4"), {443} | set(range(base + 8, base + 12)))
        # Every mode's IPv4 path is on 443 through the host's front.
        for mode in ("wall", "phones", "windows", "minecraft", "aggregate"):
            self.assertIn(443, smoke.declared_ports(mode, "v4"), mode)
        self.assertEqual(smoke.declared_ports("phones", "v4"), smoke.declared_ports("wall", "v4"))
        self.assertEqual(smoke.declared_ports("wall", "v4", "ec2"), {443})
        self.assertEqual(smoke.declared_ports("minecraft", "v6"), {443})

    def test_a_socket_whose_tls_handshake_never_answers_fails(self):
        # Windows over IPv6: the TCP connection opened and the TLS handshake never answered.
        server = FakeViewerServer(self, "v6", None, tls=False)
        with patch.object(smoke, "CONNECT_SECONDS", 1):
            p = self.probe("windows", self.tile("windows", server, "v6"), "v6", server)
        self.assertFalse(p.ok)
        self.assertEqual(p.step, "connect")
        self.assertEqual(p.error[1], "timed out")
        self.assertEqual(p.attributed()["verdict"], "inconclusive")

    def test_an_open_socket_that_sends_no_frame_fails(self):
        server = FakeViewerServer(self, "v4", lambda s, srv: None)
        p = self.probe("windows", self.tile("windows", server), "v4", server)
        self.assertFalse(p.ok)
        self.assertEqual(p.step, "stream header")
        self.assertEqual(p.attributed()["verdict"], "inconclusive")

    def test_an_application_error_without_an_identified_stream_is_inconclusive(self):
        for message in ("Unauthorized", "Too Many Requests", "Service Unavailable"):
            with self.subTest(message=message):
                def deny(connection, server, denied=message):
                    send_server(connection, {"error": denied})

                server = FakeViewerServer(self, "v4", deny)
                p = self.probe("wall", self.tile("wall", server), "v4", server)
                self.assertFalse(p.ok)
                self.assertEqual((p.step, p.error),
                                 ("stream header", ("AssertionError", "the stream's header names no H.264 codec")))
                self.assertEqual(p.attributed()["verdict"], "inconclusive")

    def test_a_flat_picture_is_a_blank_tile_but_does_not_blame_the_image(self):
        server = FakeViewerServer(self, "v4", stream_of("color=c=black"))
        p = self.probe("wall", self.tile("wall", server), "v4", server)
        self.assertFalse(p.ok)
        self.assertEqual(p.error[1], "every picture the tile sent is flat: a blank tile")
        self.assertEqual(p.seen, {"flatPictures": 1, "lumaSpreadMax": 0, "lumaMean": [0]})
        self.assertEqual(p.attributed()["verdict"], "inconclusive")

    def test_a_tile_that_streams_only_flat_pictures_is_a_blank_tile(self):
        # A renderer that keeps streaming one colour, a keyframe every 0.3 s, until the probe's
        # time is up: every picture is read and none is a picture of anything.
        units = h264("color=c=0x404040")
        def blank(s, server):
            send_server(s, {"codec": "avc1.64000A", "width": 64, "height": 48})
            n = 0
            with contextlib.suppress(OSError):
                for _ in range(20):
                    for i, u in enumerate(units):
                        send_server(s, fcvf(u, i == 0, n), opcode=2)
                        n += 1
                    time.sleep(0.3)
        server = FakeViewerServer(self, "v4", blank)
        p = self.probe("wall", self.tile("wall", server), "v4", server)
        self.assertFalse(p.ok)
        self.assertEqual(p.error[1], "every picture the tile sent is flat: a blank tile")
        self.assertGreater(p.seen["flatPictures"], 3, p.seen)
        self.assertEqual(p.seen["lumaSpreadMax"], 0)

    def test_a_flat_stretch_of_the_scene_passes_once_a_later_picture_is_not_flat(self):
        # #1953: the staging smoke failed tiles whose first picture was the scene itself in one
        # tone (inside the cloud layer, the ground at night, open sky in a corner). The next
        # keyframe, at most 2 s later, is read before the tile is called blank.
        flat, scene = h264("color=c=0x404040"), h264("testsrc")
        def behave(s, server):
            send_server(s, {"codec": "avc1.64000A", "width": 64, "height": 48})
            for n, (i, u) in enumerate([(i, u) for i, u in enumerate(flat)] + [(i, u) for i, u in enumerate(scene)]):
                send_server(s, fcvf(u, i == 0, n), opcode=2)
        server = FakeViewerServer(self, "v4", behave)
        p = self.probe("wall", self.tile("wall", server), "v4", server)
        self.assertTrue(p.ok, p.line())
        self.assertEqual(p.detail["flatPictures"], 1)
        self.assertGreaterEqual(p.detail["lumaSpread"], smoke.FLAT_SPREAD)

    def test_protocol_invalid_data_blames_only_the_identified_direct_stream(self):
        def malformed(s, server):
            send_server(s, {"codec": "avc1.64000A", "width": 64, "height": 48})
            send_server(s, b"not a valid frame", opcode=2)

        host = {"host": "i-0example0host00a"}
        instance = {"half": "ec2", "vm": "us-east-1/i-1234567890abcdef0"}
        for mode, half, attribution, verdict in (
            ("wall", "fcvm", host, "image_failure"),
            ("wall", "ec2", instance, "image_failure"),
            ("wall", "fcvm", {}, "inconclusive"),
            ("wall", "fcvm", instance, "inconclusive"),
            ("aggregate", "fcvm", host, "inconclusive"),
        ):
            with self.subTest(mode=mode, half=half, attribution=attribution):
                server = FakeViewerServer(self, "v4", malformed)
                p = self.probe(mode, self.tile(mode, server, **attribution), "v4", server, half=half)
                self.assertFalse(p.ok)
                self.assertEqual(p.error[1], "a frame message without FCVF framing")
                self.assertEqual(p.attributed()["verdict"], verdict)

    def test_a_certificate_the_viewer_does_not_trust_fails(self):
        server = FakeViewerServer(self, "v4", stream_of("testsrc"))
        tile = self.tile("windows", server)
        with patch.dict(os.environ, {"SMOKE_CA_FILE": CERTS["other_ca"]}):
            addr = ipaddress.ip_address(server.host)
            with patch.object(smoke, "lookup", lambda h, f, r: [addr]), \
                    patch.object(smoke, "declared_ports", lambda m, f, h="fcvm": {server.port}):
                p = smoke.probe_tile("windows", "fcvm", tile, "v4", None, "v-1", [])
        self.assertFalse(p.ok)
        self.assertEqual(p.step, "connect")

    def minecraft(self, messages):
        def behave(s, server):
            for m in messages:
                send_server(s, m)
            while True:
                m = read_client_message(s)
                if m.get("type") == "drive":
                    server.first.append(m)
                    send_server(s, {"type": "drive_ack", "seq": m["seq"], "ok": m["seq"] >= 2})
        return behave

    def test_a_minecraft_tile_must_send_its_hello_then_a_map(self):
        good = [{"type": "hello", "tile": "mc-1", "generation": 1, "bots": 2},
                {"type": "map", "seq": 1, "region": {"x0": 0, "z0": 0, "w": 2, "h": 2},
                 "cells": base64.b64encode(bytes([3, 64] * 4)).decode()}]
        server = FakeViewerServer(self, "v4", self.minecraft(good))
        p = self.probe("minecraft", self.tile("minecraft", server), "v4", server, takeover=True)
        self.assertTrue(p.ok, p.line())
        self.assertEqual(p.detail["mapColumns"], 4)
        self.assertIn("driveAckMs", p.detail, "the take-over waited for an acknowledged drive")
        unread = {**good[1], "cells": base64.b64encode(bytes(8)).decode()}
        malformed = {**good[1], "cells": base64.b64encode(bytes([3])).decode()}
        for messages, step, verdict in (
            ([{**good[0], "tile": "mc-2"}], "view hello", "inconclusive"),
            (good[:1], "view map", "inconclusive"),
            ([good[0], unread], "view map", "inconclusive"),
            ([good[0], malformed], "view map", "image_failure"),
        ):
            with self.subTest(step=step):
                server = FakeViewerServer(self, "v4", self.minecraft(messages))
                tile = self.tile("minecraft", server, host="i-0example0host00a")
                p = self.probe("minecraft", tile, "v4", server)
                self.assertEqual((p.ok, p.step), (False, step))
                self.assertEqual(p.attributed()["verdict"], verdict)

    def test_without_admission_a_view_socket_names_the_viewer(self):
        good = [{"type": "hello", "tile": "mc-1"}, {"type": "map", "region": {"w": 1, "h": 1},
                "cells": base64.b64encode(bytes([3, 64])).decode()}]
        server = FakeViewerServer(self, "v4", self.minecraft(good))
        self.assertTrue(self.probe("minecraft", self.tile("minecraft", server), "v4", server, admit=None).ok)
        self.assertEqual(server.first[0], {"viewer": "v-1"})

    def test_a_probe_line_carries_no_url_host_or_token(self):
        server = FakeViewerServer(self, "v4", lambda s, srv: None)
        tile = self.tile("windows", server)
        p = self.probe("windows", tile, "v4", server)
        text = json.dumps(p.line())
        for secret in ("tiles.fcvm.dev", "127.0.0.1", "v1.9.v-1.mac", str(server.port)):
            self.assertNotIn(secret, text)

    def test_every_tile_the_state_names_is_expected_and_a_missing_mode_fails(self):
        state = {"tiles": [{"id": "fcvm:1", "half": "fcvm", "phase": "running"},
                           {"id": "fcvm:2", "half": "fcvm", "phase": "waiting_capacity"},
                           {"id": "fcvm:3", "half": "fcvm", "phase": "borrowed", "borrowed": {"viewer": "v-9"}}],
                 "windows": {"tiles": [{"id": "win:1", "phase": "running"}]},
                 # A lent server keeps its URLs: everyone watches it, so it is checked.
                 "minecraft": {"tiles": [{"id": "mc:1", "phase": "borrowed", "borrowed": {"viewer": "v-9"},
                                          "frame_url": "wss://mc-1-abcdefgh.tiles.fcvm.dev/view"}]},
                 "aggregates": [{"id": "agg:h", "half": "fcvm", "frame_url": None, "frame_url_v4": None}]}
        jobs, failed = smoke.sweep_jobs(state, ("wall", "windows", "minecraft", "phones"), "v4", None, "v-1", [])
        self.assertEqual([j[2]["id"] for j in jobs], ["fcvm:1", "win:1", "mc:1"])
        self.assertEqual([(f.mode, f.tile, f.error[1]) for f in failed],
                         [("wall", "fcvm:2", "the tile is not running"),
                          ("phones", None, "the coordinator reports no tiles of this mode")])
        self.assertFalse(smoke.settled(state, ("wall",)))
        self.assertTrue(smoke.settled({**state, "tiles": state["tiles"][:1]}, ("wall", "windows", "minecraft")))
        self.assertFalse(smoke.settled(state, ("potato",)))

    def test_the_viewer_must_have_its_family_and_with_single_family_only_it(self):
        with patch.object(smoke, "reaches", lambda f: f == "v4"):
            smoke.prove_viewer("v4", single=True)
            with self.assertRaises(RuntimeError):
                smoke.prove_viewer("v6", single=False)
        with patch.object(smoke, "reaches", lambda f: True), self.assertRaises(AssertionError):
            smoke.prove_viewer("v4", single=True)
        with patch.object(smoke, "reaches", lambda f: True):
            smoke.prove_viewer("v6", single=False)

    def test_no_decoder_blocks(self):
        with patch.object(smoke.shutil, "which", return_value=None), self.assertRaises(RuntimeError):
            smoke.decode([b"\x00"], 1, 1)

    def test_a_failed_tile_fails_the_run_with_its_step(self):
        out = io.StringIO()
        with patch.object(smoke, "main", return_value=2), contextlib.redirect_stdout(out), \
                self.assertRaises(SystemExit):
            smoke.run(["--modes", "windows"])
        self.assertEqual(json.loads(out.getvalue().splitlines()[-1]),
                         {"status": "failed", "step": "viewer path", "errorType": "AssertionError",
                          "error": "a tile failed the viewer path"})


class ViewerPathFlowTests(unittest.TestCase):
    """main() with modes: it waits for every mode to settle, probes every tile, borrows a
    Minecraft server and gives it back, and needs a potato catch it saw."""

    def run_flow(self, claim="committed", catch=True, drop_claim=False):
        clock, sent, probed, opened = [100.0], [], [], []
        tile = lambda i, **k: {"id": f"t:{i}", "slot": f"s-{i}", "half": "fcvm", "phase": "running",
                               "frame_url": "wss://x.tiles.fcvm.dev/frames", "frame_url_v4": None, **k}
        state = {"tiles": [tile(1)], "phones": {"tiles": [tile(2)]}, "windows": {"tiles": [tile(3, phase="restoring")]},
                 "minecraft": {"tiles": [tile(4)]}, "potato": {"capacity": 4, "round": {"seq": 7}}}
        class Live:
            def __init__(self, *a, **k):
                opened.append(self)
                # Each connection is its own viewer with its own direct token.
                self.viewer = "v-1" if len(opened) == 1 else f"v-{len(opened)}"
                token = "tok" if len(opened) == 1 else f"tok-{len(opened)}"
                self.queue = [{"type": "hello", "viewer": self.viewer},
                              {"type": "admission", "capacity": 500, "direct": {"value": token}},
                              {"type": "snapshot", "state": copy.deepcopy(state)}]
            def send(self, m):
                if m.get("type") != "intent":
                    return
                if drop_claim and m["name"] == "claim_minecraft" and len(opened) == 1:
                    # The connection closed while the probe was not reading: the claim never
                    # leaves, and the probe claims again on a new connection.
                    raise ssl.SSLEOFError(8, "EOF occurred in violation of protocol")
                sent.append(m["name"])
                self.queue.append({"type": "receipt", "id": m["id"],
                                   "outcome": claim if m["name"] == "claim_minecraft" else "committed"})
                if m["name"] == "claim_minecraft" and claim == "committed":
                    state["minecraft"]["tiles"][0]["borrowed"] = {"viewer": self.viewer}
                    self.queue.append({"type": "snapshot", "state": copy.deepcopy(state)})
            def message(self, deadline):
                clock[0] += 1
                if not self.queue:
                    if catch:
                        state["potato"]["round"]["seq"] += 1
                    state["windows"]["tiles"][0]["phase"] = "running"
                    return 1, json.dumps({"type": "snapshot", "state": copy.deepcopy(state)}).encode()
                return 1, json.dumps(self.queue.pop(0)).encode()
            def close(self): pass
        def probe(mode, half, t, family, admit, viewer, resolvers, takeover=None):
            probed.append((mode, t["id"], admit, bool(takeover)))
            p = smoke.Probe(mode, t["id"], family)
            p.ok = True
            return p
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        def get(url, headers=None):
            return ((403 if url.endswith("/agent") else 200), {}, b"") if headers else (
                302, {"Location": "https://login.cloudflareaccess.com/login"}, b"")
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Live), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_viewer", lambda f, s: None), patch.object(smoke, "probe_tile", probe), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            failed = smoke.main(family="v6", modes=smoke.MODES, relaunch=False, takeover=True)
        return failed, sent, probed, [json.loads(l) for l in out.getvalue().splitlines()]

    def test_a_take_over_claimed_on_a_new_connection_is_that_connections(self):
        # Greptile on #2003: a claim sent again on a new connection belongs to that connection's
        # viewer, so the take-over must look for that viewer and stream with that token.
        failed, sent, probed, lines = self.run_flow(drop_claim=True)
        self.assertEqual(failed, 0, lines)
        self.assertEqual(sent, ["claim_minecraft", "release_minecraft"])
        self.assertEqual([(m, i, a) for m, i, a, t in probed if t], [("minecraft", "t:4", "tok-2")])

    def test_every_mode_is_probed_after_it_settles_and_the_borrow_is_returned(self):
        failed, sent, probed, lines = self.run_flow()
        self.assertEqual(failed, 0, lines)
        self.assertEqual(sorted(i for _, i, _, t in probed if not t), ["t:1", "t:2", "t:3", "t:4"])
        self.assertEqual([(m, i, a) for m, i, a, t in probed if t], [("minecraft", "t:4", "tok")])
        self.assertEqual(sent, ["claim_minecraft", "release_minecraft"])
        self.assertTrue(any(l.get("mode") == "potato" and l.get("ok") for l in lines))

    def test_a_refused_take_over_and_no_catch_fail(self):
        failed, sent, _, lines = self.run_flow(claim="refused", catch=False)
        self.assertEqual(failed, 2)
        self.assertEqual(sent, ["claim_minecraft"], "nothing was lent, so nothing is given back")
        errors = {l.get("error") for l in lines if l.get("ok") is False}
        self.assertEqual(errors, {"the coordinator refused the take-over", "no verified catch while the smoke watched"})


class PotatoEvidenceTests(unittest.TestCase):
    """A potato check that sees no catch names what the game did while the smoke watched: the
    rounds, the drops with their reason and stage, the player phases and how many snapshots
    arrived. The staging run of 2026-11-13 (43456580894) printed only "no verified catch" in
    both families, and the coordinator's log is not readable where the smoke runs, so nothing
    said whether a round was running, dropping or absent."""

    def test_a_failed_watch_carries_the_rounds_drops_and_player_phases_it_saw(self):
        drop = {"round": 41, "size": 64, "reason": "not_listening", "player": 3, "seq": 0,
                "stage": "waking", "stage_ms": 812.5, "at": 1_000_060_000,
                "detail": "player 3 was waking when player 2 threw (to [2001:db8::1003]:7070)"}
        players = [{"player": 0, "phase": "awake"}, {"player": 1, "phase": "asleep"},
                   {"player": 2, "phase": "asleep"}]
        snapshots = [
            {"capacity": 96, "round": {"id": 41, "size": 64, "seq": 0, "started_at": 1_000_000_000,
                                       "last_catch_at": 1_000_050_000}, "players": players},
            {"capacity": 96, "round": None, "next_round_at": 1_000_065_000, "last_drop": drop,
             "players": players},
            {"capacity": 96, "round": {"id": 42, "size": 96, "seq": None, "started_at": 1_000_065_000},
             "last_drop": drop, "players": players + [{"player": 3, "phase": "booting"}]},
        ]
        clock, current = [0.0], [{"potato": snapshots[0]}]

        def until(predicate, by=None):
            for potato in snapshots + [snapshots[-1]] * 3:
                clock[0] += 40
                current[0] = {"potato": potato}
                if predicate(current[0]) or (by is not None and clock[0] > by):
                    return current[0]
            raise AssertionError("the watch did not end")

        out = io.StringIO()
        with patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            failed = smoke.viewer_path("v4", ("potato",), False, [], until, None,
                                       lambda: (current[0], {}, "v-1"))
        self.assertEqual(failed, 1)
        line = next(json.loads(l) for l in out.getvalue().splitlines() if '"potato"' in l)
        self.assertEqual(line["error"], "no verified catch while the smoke watched")
        seen = line["seen"]
        self.assertEqual(seen["rounds"], [{"id": 41, "size": 64, "seq": 0}, {"id": 42, "size": 96, "seq": None}])
        self.assertEqual(seen["drops"], [{"round": 41, "size": 64, "reason": "not_listening", "player": 3,
                                          "seq": 0, "stage": "waking", "stageMs": 812.5}])
        self.assertEqual(seen["capacity"], 96)
        self.assertEqual(seen["phases"], {"asleep": 2, "awake": 1, "booting": 1})
        self.assertGreaterEqual(seen["snapshots"], 3)
        self.assertGreaterEqual(seen["watchedSeconds"], 90)
        self.assertNotIn("2001:db8", json.dumps(line), "a drop's free text can name an address")


class SilentLiveTests(unittest.TestCase):
    """A /live that goes silent after readiness ends a bounded wait at its own limit."""

    def test_a_silent_live_ends_the_potato_watch_in_its_own_failure_line(self):
        clock = [100.0]
        state = {"potato": {"capacity": 4, "round": {"seq": 7}}}
        class Live:
            def __init__(self, *a, **k):
                self.queue = [{"type": "hello", "viewer": "v-1"},
                              {"type": "admission", "capacity": 500, "direct": {"value": "tok"}},
                              {"type": "snapshot", "state": copy.deepcopy(state)}]
            def send(self, m): pass
            def message(self, deadline):
                if self.queue:
                    clock[0] += 1
                    return 1, json.dumps(self.queue.pop(0)).encode()
                # Silent: like Socket.exact, nothing arrives until the caller's deadline.
                clock[0] = max(clock[0], deadline)
                raise TimeoutError("WebSocket response timeout")
            def close(self): pass
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        def get(url, headers=None):
            return ((403 if url.endswith("/agent") else 200), {}, b"") if headers else (
                302, {"Location": "https://login.cloudflareaccess.com/login"}, b"")
        out, start = io.StringIO(), clock[0]
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Live), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_viewer", lambda f, s: None), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            failed = smoke.main(family="v6", modes=("potato",), relaunch=False)
        self.assertEqual(failed, 1)
        lines = [json.loads(l) for l in out.getvalue().splitlines()]
        potato = next(l for l in lines if l.get("mode") == "potato")
        self.assertEqual(potato["error"], "no verified catch while the smoke watched")
        self.assertLess(clock[0] - start, 200, "the watch ran to the run's global deadline")


class PartialFrameTests(unittest.TestCase):
    def test_a_deadline_inside_a_frame_leaves_the_next_read_at_a_frame_header(self):
        chunks = [b"\x81\x02{", None, b"}"]
        class Transport:
            def recv(self, count):
                if not chunks:
                    return b""
                if chunks[0] is None:
                    chunks.pop(0)
                    raise smoke.socket.timeout()
                chunk, chunks[0] = chunks[0][:count], chunks[0][count:]
                if not chunks[0]:
                    chunks.pop(0)
                return chunk
            def settimeout(self, timeout): pass
        clock = [0.0]
        connection = smoke.Socket.__new__(smoke.Socket)
        connection.s = Transport()
        def tick(): clock[0] += 1; return clock[0]
        with patch.object(smoke.time, "monotonic", tick):
            with self.assertRaises(TimeoutError):
                connection.message(4.0)
            self.assertEqual(connection.message(100.0), (1, b"{}"))


class AccessCheckMessageTests(unittest.TestCase):
    ENV = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
           "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}

    def failure(self, get):
        with patch.dict(os.environ, self.ENV), patch.object(smoke, "get", get), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(AssertionError) as caught:
                smoke.main(family="v4", modes=(), relaunch=False)
        return str(caught.exception)

    def test_an_unauthenticated_failure_names_the_endpoint_and_code(self):
        message = self.failure(lambda url, headers=None: (200, {}, b""))
        self.assertIn("api /healthz", message)
        self.assertIn("200", message)
        self.assertIn("no Location", message)
        self.assertNotIn("example.test", message)

    def test_a_redirect_to_another_host_is_named_without_echoing_it(self):
        message = self.failure(lambda url, headers=None: (302, {"Location": "https://elsewhere.test/x"}, b""))
        self.assertIn("api /healthz", message)
        self.assertIn("302", message)
        self.assertIn("not an Access host", message)
        self.assertNotIn("elsewhere", message)

    def test_an_authenticated_failure_names_the_endpoint_and_code(self):
        def get(url, headers=None):
            if headers:
                return (503 if url.endswith("/") and "stage" in url else 200), {}, b""
            return 302, {"Location": "https://team.cloudflareaccess.com/login"}, b""
        message = self.failure(get)
        self.assertIn("origin /", message)
        self.assertIn("503", message)
        self.assertNotIn("example.test", message)


class SilentReleaseTests(unittest.TestCase):
    def test_a_silent_live_after_a_claim_still_prints_the_claim_failure_probe(self):
        clock, sent = [100.0], []
        tile = {"id": "t:4", "slot": "s-4", "half": "fcvm", "phase": "running",
                "frame_url": "wss://x.tiles.fcvm.dev/frames", "frame_url_v4": None}
        state = {"minecraft": {"tiles": [tile]}}
        class Live:
            def __init__(self, *a, **k):
                self.queue = [{"type": "hello", "viewer": "v-1"},
                              {"type": "admission", "capacity": 500, "direct": {"value": "tok"}},
                              {"type": "snapshot", "state": copy.deepcopy(state)}]
            def send(self, m):
                if m.get("type") == "intent":
                    sent.append(m["name"])
                    if m["name"] == "claim_minecraft":
                        self.queue.append({"type": "receipt", "id": m["id"], "outcome": "committed"})
            def message(self, deadline):
                if self.queue:
                    clock[0] += 1
                    return 1, json.dumps(self.queue.pop(0)).encode()
                clock[0] = max(clock[0], deadline)
                raise TimeoutError("WebSocket response timeout")
            def close(self): pass
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        def get(url, headers=None):
            return ((403 if url.endswith("/agent") else 200), {}, b"") if headers else (
                302, {"Location": "https://login.cloudflareaccess.com/login"}, b"")
        def probe(mode, half, t, family, admit, viewer, resolvers, takeover=None):
            p = smoke.Probe(mode, t["id"], family)
            p.ok = True
            return p
        out, start = io.StringIO(), clock[0]
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Live), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_viewer", lambda f, s: None), patch.object(smoke, "probe_tile", probe), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            failed = smoke.main(family="v6", modes=("minecraft",), relaunch=False, takeover=True)
        self.assertGreaterEqual(failed, 1)
        self.assertEqual(sent, ["claim_minecraft", "release_minecraft"])
        errors = {l.get("error") for l in map(json.loads, out.getvalue().splitlines()) if l.get("ok") is False}
        self.assertIn("the borrowed server never showed as this viewer's", errors)
        self.assertLess(clock[0] - start, 200, "the release ran to the run's global deadline")


class AttributionTests(unittest.TestCase):
    """The release gate (fleet/ami/release_gate.py) decides only on probes whose tiles the
    coordinator said ran on the release's hosts and tile instances, so each family's summary
    records, per probe, digests of the host and tile instance ids /live reported for the tile.
    The ids themselves never reach a printed line: this repository's logs are public."""

    STATE = {"tiles": [{"id": "fcvm:1", "half": "fcvm", "phase": "running", "host": "i-0example0host00a"},
                       {"id": "fcvm:2", "half": "fcvm", "phase": "failed", "host": "i-0example0host00a"},
                       {"id": "ec2:1", "half": "ec2", "phase": "running", "vm": "us-east-1/i-1234567890abcdef0"}],
             "windows": {"tiles": [{"id": "win:1", "phase": "running", "host": "i-0example0host00b"}]},
             "aggregates": [{"id": "agg:ec2", "half": "ec2", "host": None, "tiles": ["ec2:1"],
                             "frame_url": "wss://agg-ec2-abcdefgh.tiles.fcvm.dev/frames"}]}

    def test_an_ec2_tile_is_attributed_to_its_bare_instance_id(self):
        # The coordinator reports an EC2 tile's `vm` as region/instance (src/fleet/coord/ec2_aws.rs),
        # and the release gate lists bare instance ids: on 2026-11-15 every EC2 tile probe of a
        # staging gate run fell outside the release's tiles because of the region prefix.
        tile = {"id": "ec2:1", "half": "ec2", "phase": "running", "vm": "us-east-1/i-0123456789abcdef0"}
        self.assertEqual(smoke.tile_attribution(tile), ([], ["i-0123456789abcdef0"]))
        jobs, _ = smoke.sweep_jobs({"tiles": [tile], "aggregates": [
            {"id": "agg:ec2", "half": "ec2", "host": None, "tiles": ["ec2:1"],
             "frame_url": "wss://agg-ec2-abcdefgh.tiles.fcvm.dev/frames"}]}, ("wall",), "v4", None, "v-1", [])
        self.assertEqual([smoke.tile_attribution(j[2]) for j in jobs],
                         [([], ["i-0123456789abcdef0"]), ([], ["i-0123456789abcdef0"])])
        # Anything else is not an instance the gate could list: no attribution, so no verdict.
        for vm in ("i-0123456789abcdef0", "us-east-1/fake-1", "us-east-1/i-0123", "fleet-slot-1", ""):
            self.assertEqual(smoke.tile_attribution(dict(tile, vm=vm)), ([], []), vm)
        # The format is the coordinator's: every Launched instance it builds is region/id.
        source = (HERE.parent.parent / "src/fleet/coord/ec2_aws.rs").read_text()
        built = re.findall(r"instance: (format!\([^)]*\))", source)
        self.assertTrue(built, "ec2_aws.rs builds no Launched instance")
        self.assertTrue(all(f.startswith('format!("{region}/') for f in built), built)

    def test_the_agents_line_is_the_coordinators_registration_record_by_digest(self):
        # The release gate's no-agent verdict reads the coordinator's record of every host whose
        # agent registered since its start or resume (/live fleet.agents, fleet-core
        # Registrations), from a fleet-core vector's real view.
        view = json.loads((HERE.parent / "core/tests/vectors/04_retile_join_leave.json").read_text())["final_view"]
        record = view["fleet"]["agents"]
        names = [h["name"] for h in record["hosts"]]
        self.assertTrue(names)
        line = smoke.agents_line(view, 1_794_780_000_000)
        self.assertEqual(line, {"since": record["since"], "registered": sorted(smoke.attribution_digest(n) for n in names),
                                "observed_at": 1_794_780_000_000})
        for name in names:
            self.assertNotIn(name, json.dumps(line))
        # A host whose agent has gone is still in the record: it registered.
        self.assertTrue(set(names) >= {h["name"] for h in view["fleet"]["hosts"]})
        # A snapshot without the record, or with a malformed one, gives no line.
        for state in ({}, {"fleet": {"hosts": []}}, {"fleet": {"agents": {"since": "x", "hosts": []}}},
                      {"fleet": {"agents": {"since": 1}}}):
            self.assertIsNone(smoke.agents_line(state, 5))
        self.assertTrue(smoke.arguments(["--agents"])["agents"])

    def test_the_digest_is_the_release_gates(self):
        gate = load("release_gate", "../ami/release_gate.py")
        for instance in ("i-0example0host00a", ""):
            self.assertEqual(smoke.attribution_digest(instance), gate.attribution_digest(instance))
        self.assertEqual(smoke.attribution_digest("i-0example0host00a"),
                         hashlib.sha256(b"fcvm-release-gate\0i-0example0host00a").hexdigest())

    def test_each_probe_carries_the_host_or_tile_instance_live_reported(self):
        jobs, failed = smoke.sweep_jobs(self.STATE, ("wall", "windows"), "v4", None, "v-1", [])
        by_tile = {j[2]["id"]: smoke.tile_attribution(j[2]) for j in jobs}
        self.assertEqual(by_tile["fcvm:1"], (["i-0example0host00a"], []))
        self.assertEqual(by_tile["ec2:1"], ([], ["i-1234567890abcdef0"]))
        self.assertEqual(by_tile["win:1"], (["i-0example0host00b"], []))
        # The EC2 half's aggregate has no host of its own: it relays its tiles' frames.
        self.assertEqual(by_tile["agg:ec2"], ([], ["i-1234567890abcdef0"]))
        # A tile the coordinator reports not running is still attributed to where it runs.
        self.assertEqual([(f.tile, f.hosts, f.instances) for f in failed], [("fcvm:2", ["i-0example0host00a"], [])])
        # A tile /live names no host for has no attribution, and the gate then decides nothing.
        self.assertEqual(smoke.tile_attribution({"id": "fcvm:9", "half": "fcvm"}), ([], []))

    def test_every_running_tile_of_a_real_view_is_attributed(self):
        # The browser views fleet-core's replayed vectors produce (tests/vectors/*.json
        # final_view, rewritten from the Rust code by FLEET_VECTORS_BLESS=1): what /live really
        # serializes, not a fixture written to match the smoke.
        vectors = sorted((HERE.parent / "core" / "tests" / "vectors").glob("*.json"))
        seen = set()
        for path in vectors:
            view = json.loads(path.read_text()).get("final_view") or {}
            modes = tuple(m for m in ("wall", "phones", "windows", "minecraft") if smoke.tiles_of(view, m))
            jobs, _ = smoke.sweep_jobs(view, modes, "v4", None, "v-1", [])
            for job in jobs:
                mode, tile = job[0], job[2]
                with self.subTest(vector=path.name, tile=tile.get("id")):
                    if tile.get("half") == "ec2":
                        # The vectors' EC2 instances are fleet-core's own names, not the driver's
                        # region/instance (test_an_ec2_tile_is_attributed_to_its_bare_instance_id
                        # pins that format): only the field is checked here.
                        self.assertTrue(isinstance(tile.get("vm"), str) and tile["vm"], "an EC2 tile names no vm")
                    elif tile.get("half") == "lambda":
                        # A Lambda tile runs in a Lambda environment (`vm`) on an instance the
                        # fleet cannot see, not on a host image the release gate judges: it names
                        # an environment and is attributed to nothing.
                        self.assertTrue(isinstance(tile.get("vm"), str) and tile["vm"], "a Lambda tile names no vm")
                        self.assertEqual(smoke.tile_attribution(tile), ([], []))
                    else:
                        hosts, instances = smoke.tile_attribution(tile)
                        self.assertTrue(hosts or instances, "a running tile /live names no host or instance for")
                    seen.add((mode, tile.get("half")))
            # No vector serves an aggregate's URLs, so the sweep skips them; their attribution
            # is the view's own fields all the same.
            for aggregate in view.get("aggregates") or []:
                with self.subTest(vector=path.name, tile=aggregate.get("id")):
                    self.assertTrue(aggregate.get("host"), "a GPU host's aggregate names no host")
                    self.assertEqual(smoke.tile_attribution(aggregate), ([aggregate["host"]], []))
                    seen.add(("aggregate", aggregate.get("half")))
        # Every kind of probe the smoke attributes from a tile appears in some vector. The potato
        # game's host is pinned by 33_potato.json's /view/potato/host checks: its final view is
        # after the host has gone.
        for kind in (("wall", "fcvm"), ("wall", "ec2"), ("wall", "lambda"), ("phones", "fcvm"),
                     ("windows", "fcvm"), ("minecraft", "fcvm"), ("aggregate", "fcvm")):
            self.assertIn(kind, seen)

    def test_the_summary_line_holds_digests_and_no_line_holds_an_id(self):
        clock, current = [0.0], [{"potato": {"capacity": 96, "host": "i-0example0host00b",
                                             "round": {"id": 1, "size": 2, "seq": 0}}}]

        def until(predicate, by=None):
            # Each wait sees one more catch: the settle wait first, then the catch watch.
            clock[0] += 1
            current[0]["potato"]["round"]["seq"] += 1
            predicate(current[0])
            return current[0]

        out = io.StringIO()
        with patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            smoke.viewer_path("v4", ("potato",), False, [], until, None, lambda: (current[0], {}, "v-1"))
        self.assertNotIn("i-0example0host00b", out.getvalue())
        records = [json.loads(line) for line in out.getvalue().splitlines() if line.strip()]
        summaries = [record for record in records if isinstance(record, dict) and "probes" in record]
        self.assertEqual(len(summaries), 1)
        summary = summaries[0]
        self.assertEqual(summary["probes"], len(summary["attribution"]))
        self.assertEqual(summary["attribution"], [{"mode": "potato", "tile": None, "ok": True,
                                                   "verdict": "pass", "hosts": [smoke.attribution_digest("i-0example0host00b")],
                                                   "instances": []}])


class HostLocalTests(unittest.TestCase):
    """The image factory's validator has a builder security group that admits no inbound port and
    lets out only 80, 443 and 53 over IPv4, so a host-image acceptance that went out through the
    host's public address, or that required a route to a public control address, refused every
    image. Under SMOKE_HOST_LOCAL it reaches the host's public IPv4 address on its private one
    and does not ask the host to prove it is a viewer."""

    LOCAL = {"SMOKE_LIVE_URL": "ws://127.0.0.1:1/live", "SMOKE_HOST_LOCAL": "1.0.0.1=10.0.4.9"}

    def test_the_public_address_is_reached_on_the_private_one(self):
        with patch.dict(os.environ, self.LOCAL), \
                patch.object(smoke, "resolve", return_value=([ipaddress.ip_address("1.0.0.1")], 1)):
            self.assertEqual(smoke.lookup("x-v4.tiles.fleet.test", "v4", ["127.0.0.1:2"]),
                             [ipaddress.ip_address("10.0.4.9")])
        with patch.object(smoke, "resolve", return_value=([ipaddress.ip_address("1.0.0.1")], 1)):
            self.assertEqual(smoke.lookup("x-v4.tiles.fcvm.dev", "v4", ["1.1.1.1"]),
                             [ipaddress.ip_address("1.0.0.1")], "staging is untouched")

    def test_it_is_refused_outside_an_acceptance_and_for_a_bad_mapping(self):
        for env in ({"SMOKE_HOST_LOCAL": "1.0.0.1=10.0.4.9"},
                    {**self.LOCAL, "SMOKE_HOST_LOCAL": "10.0.4.9=1.0.0.1"},
                    {**self.LOCAL, "SMOKE_HOST_LOCAL": "2001:db8::1=10.0.4.9"}):
            with self.subTest(env=env), patch.dict(os.environ, env), self.assertRaises(ValueError):
                smoke.host_local()

    def test_the_host_is_not_asked_to_prove_it_is_a_viewer(self):
        # Without the change this is the control connection to a public address, which the
        # builder group's egress refuses over IPv6 and the smoke then blocks on.
        class Stop(Exception):
            pass
        def live_target(target):
            raise Stop()
        with patch.dict(os.environ, self.LOCAL), \
                patch.object(smoke, "prove_viewer", side_effect=AssertionError("asked the host")), \
                patch.object(smoke, "live_target", live_target), contextlib.redirect_stdout(io.StringIO()), \
                self.assertRaises(Stop):
            smoke.main(family="v6", modes=("wall",), relaunch=False)


class SmokeHoldTests(unittest.TestCase):
    """A drill (the prod Spot reclaim) needs a viewer holding a seat, or the fleet sleeps under it.
    --hold-seconds keeps the smoke's seat after the viewer path and then checks every tile again."""

    def run_main(self, **kw):
        clock, opened, paths = [100.0], [], []
        class Live:
            def __init__(self, *a, **k):
                opened.append(self)
                self.queue = [{"type": "hello", "viewer": "v-1"}, {"type": "admission", "capacity": 5},
                              {"type": "snapshot", "state": {"tiles": []}}]
            def send(self, m): pass
            def message(self, deadline):
                clock[0] += 5
                if clock[0] >= deadline:
                    raise TimeoutError()
                return 1, json.dumps(self.queue.pop(0) if self.queue else {"type": "snapshot", "state": {"tiles": []}}).encode()
            def close(self): pass
        def viewer_path(family, modes, takeover, resolvers, until, commit, now, settle_seconds):
            until(lambda s: True)
            paths.append(clock[0])
            return 0
        env = {"STAGING_API_URL": "https://api.example.test", "STAGING_ORIGIN": "https://stage.example.test",
               "CF_ACCESS_CLIENT_ID": "synthetic-id", "CF_ACCESS_CLIENT_SECRET": "synthetic-secret"}
        def get(url, headers=None):
            return ((403 if url.endswith("/agent") else 200), {}, b"") if headers else (
                302, {"Location": "https://login.cloudflareaccess.com/login"}, b"")
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(smoke, "Socket", Live), patch.object(smoke, "get", get), \
                patch.object(smoke, "prove_viewer", lambda f, s: None), patch.object(smoke, "viewer_path", viewer_path), \
                patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(out):
            failed = smoke.main(family="v4", modes=("wall",), relaunch=False, **kw)
        return failed, paths, len(opened), out.getvalue()

    def test_a_held_seat_runs_the_viewer_path_again_after_the_hold_on_one_connection(self):
        failed, paths, connections, out = self.run_main(hold_seconds=1800)
        self.assertEqual(failed, 0)
        self.assertEqual(len(paths), 2)
        self.assertGreaterEqual(paths[1] - paths[0], 1800)
        self.assertEqual(connections, 1)
        self.assertIn('"held": 1800', out)

    def test_without_a_hold_the_viewer_path_runs_once(self):
        self.assertEqual(len(self.run_main()[1]), 1)

    def test_the_hold_argument_is_bounded_and_needs_modes(self):
        self.assertEqual(smoke.arguments(["--modes", "wall"])["hold_seconds"], 0)
        self.assertEqual(smoke.arguments(["--modes", "wall", "--hold-seconds", "1800"])["hold_seconds"], 1800)
        for argv in (["--modes", "wall", "--hold-seconds", "1801"], ["--modes", "wall", "--hold-seconds", "-1"],
                     ["--broadcast", "--hold-seconds", "60"],
                     # A mode list that normalizes to nothing would run unheld.
                     ["--modes", ",", "--broadcast", "--hold-seconds", "60"]):
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                smoke.arguments(argv)

    def test_only_a_dispatch_holds_and_its_job_gets_the_time(self):
        wf = (HERE.parent.parent / ".github/workflows/fleet-prod-smoke.yml").read_text()
        self.assertIn('HOLD_MINUTES: ${{ inputs.hold_minutes || \'0\' }}', wf)
        # 10#: "010" is ten minutes, not octal eight, and "08" is not an arithmetic error.
        self.assertIn('--hold-seconds "$((10#$HOLD_MINUTES * 60))"', wf)
        for given, seconds in (("010", "600"), ("08", "480"), ("0", "0")):
            got = subprocess.run(["bash", "-c", 'echo "$((10#$HOLD_MINUTES * 60))"'], env={"HOLD_MINUTES": given},
                                 capture_output=True, text=True, check=True).stdout.strip()
            self.assertEqual(got, seconds, given)
        self.assertIn("timeout-minutes: ${{ (inputs.hold_minutes || '0') != '0' && 90 || 45 }}", wf)
        self.assertNotIn("${{ inputs.hold_minutes }}\n", wf.split("run: |", 1)[1].split("\n\n", 1)[0])


class SmokeWorkflowTests(unittest.TestCase):
    """The deploys call fleet-viewer-smoke.yml. Its jobs read the Access token from the
    fleet-stage-deploy environment's secrets, which a called workflow gets only when its caller
    passes them: without `secrets: inherit` the first run on main sent an empty token and failed
    its Access check (2026-11-13)."""

    def test_every_caller_of_the_smoke_passes_its_secrets(self):
        import re
        callers = []
        for path in sorted((HERE.parent.parent / ".github" / "workflows").glob("*.yml")):
            text = path.read_text()
            for job in re.split(r"(?m)^  (?=[\w-]+:\n)", text.split("\njobs:\n", 1)[-1]):
                if "uses: ./.github/workflows/fleet-viewer-smoke.yml" not in job:
                    continue
                callers.append(path.name)
                self.assertRegex(job, r"(?m)^    secrets: inherit$", f"{path.name}: the smoke gets no secrets")
        self.assertEqual(callers, ["fleet-coordinator-deploy.yml", "fleet-release-gate.yml", "fleet-web-deploy.yml"],
                         "the coordinator and web deploys and the image release gate call the smoke")


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
            return smoke.resolve("127.0.0.1", "slot-1-abcdefgh.tiles.fcvm.dev", qtype, 0.5)[0]

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
            return ([], 1) if resolver == "9.9.9.9" and qtype == 1 else (["x"], 1)
        with patch.object(smoke, "resolve", resolve), self.assertRaises(AssertionError):
            smoke.prove_resolves(tile)
        self.assertEqual(len(asked), 6)
        clock = [0.0]
        def slow(resolver, name, qtype, timeout):
            clock[0] += 2.5
            return ["x"], 1
        with patch.object(smoke, "resolve", slow), patch.object(smoke.time, "monotonic", lambda: clock[0]), \
                self.assertRaises(AssertionError):
            smoke.prove_resolves(tile)
        with patch.object(smoke, "resolve", lambda *a: (["x"], 1)):
            slowest, sends = smoke.prove_resolves(tile)
            self.assertLess(slowest, 2.0)
            self.assertEqual(sends, 1)


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


class WorkerDeployMessageTests(unittest.TestCase):
    def test_every_wrangler_message_is_one_word(self):
        # fleet/web is an OpenNext project: `wrangler deploy` hands its arguments to `opennextjs-cloudflare deploy`,
        # which splits them on whitespace, so `--message "fcvm <sha>"` made the commit the entry point and failed prod's
        # first Worker release (ejc3/fcvm run 44872476086). Every message is one word.
        import re
        workflows = HERE.parent.parent / ".github" / "workflows"
        found = 0
        for path in sorted(workflows.glob("*.yml")):
            for line in path.read_text().splitlines():
                if "wrangler" not in line:
                    continue
                for value in re.findall(r'--message[ =]("[^"]*"|\S+)', line):
                    found += 1
                    self.assertNotRegex(value.strip('"'), r"\s", f"{path.name}: {line.strip()}")
        self.assertGreater(found, 0, "no wrangler --message found: the check is not looking where the workflows are")


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

    def test_a_call_the_cli_rejects_before_invoking_is_not_retried(self):
        # 252: the command's syntax was refused; 253: the configuration was. Nothing was invoked.
        for status in (252, 253):
            printed, error, sleeps = self.run_publish([status])
            self.assertIsInstance(error, publish.CommandFailed)
            self.assertEqual(error.returncode, status)
            self.assertEqual(sleeps, [])
            self.assertEqual(printed, "")

    def test_a_failed_aws_call_carries_its_error_code_and_nothing_else_of_stderr(self):
        with tempfile.TemporaryDirectory() as d:
            aws = Path(d) / "aws"
            aws.write_text("#!/bin/sh\necho 'An error occurred (AccessDeniedException) when calling the Invoke operation: "
                           "User: arn:aws:sts::111122223333:assumed-role/secret is not authorized' >&2\nexit 254\n")
            aws.chmod(0o755)
            with patch.dict(os.environ, {"PATH": d + os.pathsep + os.environ["PATH"]}):
                with self.assertRaises(publish.CommandFailed) as caught:
                    publish.command(["aws", "lambda", "invoke"])
        self.assertEqual((caught.exception.returncode, caught.exception.code), (254, "AccessDeniedException"))
        self.assertNotIn("111122223333", str(caught.exception))

    def test_the_retry_line_names_the_aws_error_code(self):
        results = [publish.CommandFailed("aws", 254, "ResourceConflictException"), {"status": "unchanged", "digest": self.DIGEST}]
        sleeps, out = [], io.StringIO()
        def command(args, body=None, env=None, allow_missing=False):
            if args[1] == "ecr":
                return json.dumps({"imageDetails": [{"imageDigest": self.DIGEST}]})
            outcome = results.pop(0)
            if isinstance(outcome, Exception):
                raise outcome
            Path(args[-1]).write_text(json.dumps(outcome))
            return json.dumps({"StatusCode": 200})
        env = {"GITHUB_SHA": "a" * 40, "COORDINATOR_REPO": "111122223333.dkr.ecr.us-east-2.amazonaws.com/fcvm-fleet/coordinator",
               "COORDINATOR_RELEASE": "fcvm-coordinator-release"}
        with patch.dict(os.environ, env), patch.object(publish, "command", command), \
                patch.object(publish.time, "sleep", sleeps.append), contextlib.redirect_stdout(out):
            publish.main()
        self.assertIn("(aws exit 254, ResourceConflictException); retrying in 60 seconds", out.getvalue())

    def test_invocations_that_never_return_a_result_still_fail(self):
        printed, error, sleeps = self.run_publish([255] * 11)
        self.assertIsInstance(error, publish.CommandFailed)
        self.assertEqual(len(sleeps), 10)


class LaneReleaseTests(unittest.TestCase):
    """The staging release with the Lambda lane: off (FLEET_LAMBDA_LANE unset or off) it is exactly the release
    without a lane; on, it asks the release function which lane image the coordinator runs, selects the lane build
    that pairs with this commit for staging (fleet/lambda/release.py), and releases with that digest. A release
    that fails puts the selection back unless the function reports the new lane image running."""

    DIGEST = "sha256:" + "c" * 64
    RUNNING = "sha256:" + "1" * 64
    SELECTED = "sha256:" + "2" * 64
    BEFORE = {"current": RUNNING, "previous": "sha256:" + "0" * 64, "next": RUNNING}
    OLDER, NEWER = "9" * 40, "e" * 40  # an ancestor and a descendant of the run's commit, "a" * 40

    def run_publish(self, results, lane=None):
        results, events = list(results), []
        def command(args, body=None, env=None, allow_missing=False):
            if args[1] == "ecr":
                return json.dumps({"imageDetails": [{"imageDigest": self.DIGEST}]})
            events.append(("invoke", json.loads(body)))
            outcome = results.pop(0)
            if isinstance(outcome, Exception):
                raise outcome
            Path(args[-1]).write_text(json.dumps(outcome))
            return json.dumps({"StatusCode": 200})
        def select_lane(stack, running, commit):
            events.append(("select", stack, running, commit))
            return self.SELECTED, dict(self.BEFORE)
        def restore_lane(stack, before):
            events.append(("restore", stack, before))
        def is_ancestor(older, newer):
            # Strict: publish.py never asks about a commit and itself (its `c != commit` guard).
            assert older != newer, "asked whether a commit is its own ancestor"
            events.append(("ancestry", older, newer))
            return (older, newer) in {("9" * 40, "a" * 40), ("a" * 40, "e" * 40), ("9" * 40, "e" * 40)}
        github_output = tempfile.NamedTemporaryFile("r", delete=False)
        self.addCleanup(os.unlink, github_output.name)
        env = {"GITHUB_SHA": "a" * 40, "COORDINATOR_REPO": "111122223333.dkr.ecr.us-east-2.amazonaws.com/fcvm-fleet/coordinator",
               "COORDINATOR_RELEASE": "fcvm-coordinator-release", "GITHUB_OUTPUT": github_output.name}
        if lane is not None:
            env["FLEET_LAMBDA_LANE"] = lane
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(publish, "command", command), \
                patch.object(publish, "select_lane", select_lane), patch.object(publish, "restore_lane", restore_lane), \
                patch.object(publish, "commit_is_ancestor", is_ancestor), \
                patch.object(publish.time, "sleep", lambda s: None), contextlib.redirect_stdout(out):
            if lane is None:
                os.environ.pop("FLEET_LAMBDA_LANE", None)
            try:
                publish.main()
                error = None
            except Exception as caught:
                error = caught
        self.printed = [json.loads(line) for line in out.getvalue().splitlines() if line.startswith("{")]
        self.github_output = Path(github_output.name).read_text()
        return events, error

    def release_payload(self):
        return {"commit": "a" * 40, "digest": self.DIGEST, "lambda_digest": self.SELECTED}

    def test_with_the_lane_off_the_release_is_exactly_todays(self):
        for lane in (None, "", "off"):
            with self.subTest(lane=lane):
                events, error = self.run_publish([{"status": "deployed", "digest": self.DIGEST}], lane)
                self.assertIsNone(error)
                self.assertEqual(events, [("invoke", {"commit": "a" * 40, "digest": self.DIGEST})])

    def test_with_the_lane_on_the_release_carries_the_selected_build_in_order(self):
        events, error = self.run_publish([
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]},
            {"status": "deployed", "digest": self.DIGEST, "lambda_digest": self.SELECTED}], "on")
        self.assertIsNone(error)
        self.assertEqual(events, [("invoke", {"action": "lane"}), ("ancestry", "a" * 40, self.OLDER),
                                  ("select", "stage", self.RUNNING, "a" * 40), ("invoke", self.release_payload())])

    def test_a_release_of_an_ancestor_of_what_staging_runs_fails_the_release_step(self):
        # Invariant 1 again, under the release step: the order step decides the skip, so a descendant found here
        # means staging moved between the two steps. Nothing is selected or released, and the step fails rather
        # than conclude success for a release it did not make (prod's release discovery reads that conclusion).
        events, error = self.run_publish([{"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.NEWER]}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertIn("staging runs a descendant", str(error))
        self.assertEqual(events, [("invoke", {"action": "lane"}), ("ancestry", "a" * 40, self.NEWER)])

    def test_staging_running_this_very_commit_is_released_normally(self):
        events, error = self.run_publish([{"status": "ok", "lambda_digest": self.RUNNING, "commits": ["a" * 40]},
                                          {"status": "unchanged", "digest": self.DIGEST, "lambda_digest": self.SELECTED}], "on")
        self.assertIsNone(error)
        self.assertEqual(events, [("invoke", {"action": "lane"}), ("select", "stage", self.RUNNING, "a" * 40),
                                  ("invoke", self.release_payload())])

    def run_order(self, results, lane="on"):
        results, sent = list(results), []
        def invoke(function, payload):
            sent.append(payload)
            return results.pop(0)
        def is_ancestor(older, newer):
            assert older != newer
            return (older, newer) in {("a" * 40, "e" * 40)}
        github_output = tempfile.NamedTemporaryFile("r", delete=False)
        self.addCleanup(os.unlink, github_output.name)
        env = {"GITHUB_SHA": "a" * 40, "COORDINATOR_RELEASE": "fcvm-coordinator-release",
               "GITHUB_OUTPUT": github_output.name}
        if lane is not None:
            env["FLEET_LAMBDA_LANE"] = lane
        out = io.StringIO()
        with patch.dict(os.environ, env), patch.object(publish, "invoke", invoke), \
                patch.object(publish, "commit_is_ancestor", is_ancestor), \
                patch.object(publish, "command", side_effect=AssertionError("the order step pushes nothing")), \
                contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as exit:
            if lane is None:
                os.environ.pop("FLEET_LAMBDA_LANE", None)
            publish.run(["--order"])
            raise SystemExit(0)
        return exit.exception.code, sent, Path(github_output.name).read_text()

    def test_the_order_step_skips_the_release_of_an_ancestor_before_any_release_call(self):
        # The workflow's release step runs only when this says release, so a skipped run's release step
        # concludes "skipped" and prod's release discovery (prod_release.released) never takes it.
        code, sent, out = self.run_order([{"status": "ok", "lambda_digest": None, "commits": [self.NEWER]}])
        self.assertEqual((code, sent, out), (0, [{"action": "lane"}], "skip=true\n"))
        code, sent, out = self.run_order([{"status": "ok", "lambda_digest": None, "commits": [self.OLDER]}])
        self.assertEqual((code, out), (0, "skip=false\n"))
        code, sent, out = self.run_order([{"status": "ok", "lambda_digest": None, "commits": ["a" * 40]}])
        self.assertEqual((code, out), (0, "skip=false\n"))
        # Lane off: nothing is asked, nothing skipped.
        for lane in (None, "off"):
            code, sent, out = self.run_order([], lane=lane)
            self.assertEqual((code, sent, out), (0, [], "skip=false\n"))
        # An answer that cannot order fails the step: the release does not run.
        code, sent, out = self.run_order([{"status": "ok", "lambda_digest": None, "commits": []}])
        self.assertEqual((code, out), (1, ""))

    def test_a_lane_answer_with_no_running_commit_releases_nothing(self):
        # A running image with no commit tag cannot be ordered: the gate fails closed.
        events, error = self.run_publish([{"status": "ok", "lambda_digest": self.RUNNING, "commits": []}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(events, [("invoke", {"action": "lane"})])

    def git(self, codes):
        calls, codes = [], iter(codes)
        def run(args, **kwargs):
            calls.append(args[:4])
            return types.SimpleNamespace(returncode=next(codes), stdout="", stderr="")
        return calls, patch.object(publish.subprocess, "run", side_effect=run)

    def test_a_failed_fetch_is_not_called_a_missing_commit(self):
        calls, p = self.git([128, 128])  # not in the checkout's main, the fetch fails
        with p, self.assertRaises(RuntimeError) as raised:
            publish.commit_is_ancestor("a" * 40, "e" * 40)
        self.assertIn("could not fetch main", str(raised.exception))
        self.assertNotIn("does not contain", str(raised.exception))

    def test_a_running_commit_newer_than_the_checkout_is_fetched_before_ordering(self):
        calls, p = self.git([128, 0, 0, 0])  # not here, fetch, on main now, an ancestor
        with p:
            self.assertTrue(publish.commit_is_ancestor("a" * 40, "e" * 40))
        self.assertEqual(calls, [["git", "merge-base", "--is-ancestor", "e" * 40], ["git", "fetch", "--quiet", "origin"],
                                 ["git", "merge-base", "--is-ancestor", "e" * 40], ["git", "merge-base", "--is-ancestor", "a" * 40]])
        calls, p = self.git([0, 0])  # on the checkout's main: no fetch
        with p:
            self.assertTrue(publish.commit_is_ancestor("a" * 40, "e" * 40))
        self.assertEqual(len(calls), 2)

    def test_a_running_commit_from_another_branch_stops_the_release(self):
        # In the checkout (fetch-depth 0 brings every branch) but not on main, even after a fetch: refused,
        # never ordered as "not an ancestor".
        calls, p = self.git([1, 0, 1])
        with p, self.assertRaisesRegex(RuntimeError, "main does not contain"):
            publish.commit_is_ancestor("a" * 40, "e" * 40)
        self.assertEqual(calls[-1], ["git", "merge-base", "--is-ancestor", "e" * 40])

    def test_a_lane_answer_without_the_running_commits_releases_nothing(self):
        events, error = self.run_publish([{"status": "ok", "lambda_digest": self.RUNNING}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(events, [("invoke", {"action": "lane"})])

    def test_a_first_lane_release_selects_with_nothing_running(self):
        events, error = self.run_publish([
            {"status": "ok", "lambda_digest": None, "commits": [self.OLDER]},
            {"status": "deployed", "digest": self.DIGEST, "lambda_digest": self.SELECTED}], "on")
        self.assertIsNone(error)
        self.assertEqual(events[2], ("select", "stage", None, "a" * 40))

    def test_a_lane_query_that_fails_releases_nothing(self):
        events, error = self.run_publish([{"status": "failed", "errorType": "ValueError", "step": "input"}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(events, [("invoke", {"action": "lane"})])

    def test_a_failed_release_puts_the_selection_back(self):
        events, error = self.run_publish([
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]},
            {"status": "failed", "errorType": "Refused", "step": "rollout", "code": "rollout-failed"},
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(events[-2:], [("invoke", {"action": "lane"}), ("restore", "stage", self.BEFORE)])

    def test_a_release_that_applied_despite_its_error_keeps_the_selection(self):
        # The call ended without a result each time, but the function had released: it runs the new lane image.
        events, error = self.run_publish(
            [{"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]}] + [publish.CommandFailed("aws", 255)] * 11
            + [{"status": "ok", "lambda_digest": self.SELECTED, "commits": ["a" * 40]}], "on")
        self.assertIsInstance(error, publish.CommandFailed)
        self.assertEqual(events[-1], ("invoke", {"action": "lane"}))
        self.assertNotIn("restore", [e[0] for e in events])

    def test_a_release_reporting_another_lane_image_puts_the_selection_back(self):
        events, error = self.run_publish([
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]},
            {"status": "deployed", "digest": self.DIGEST, "lambda_digest": self.RUNNING},
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertEqual(events[-1], ("restore", "stage", self.BEFORE))

    def test_when_nothing_says_what_runs_the_selection_is_left_and_the_release_error_raised(self):
        events, error = self.run_publish([
            {"status": "ok", "lambda_digest": self.RUNNING, "commits": [self.OLDER]},
            {"status": "failed", "step": "rollout"},
            {"status": "failed", "step": "lane"}], "on")
        self.assertIsInstance(error, RuntimeError)
        self.assertIn("release did not complete", str(error))
        self.assertNotIn("restore", [e[0] for e in events])

    def test_an_unknown_lane_setting_is_refused_before_anything_runs(self):
        events, error = self.run_publish([], "yes")
        self.assertIsInstance(error, ValueError)
        self.assertEqual(events, [])

    def test_the_lane_selection_takes_the_build_that_pairs_with_this_commit(self):
        calls = []
        lane = publish.lane_release()
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile"}), \
                patch.object(lane, "paired_build", lambda ecr, commit: calls.append(("paired", ecr.registry, commit)) or self.SELECTED), \
                patch.object(lane, "snapshot", lambda ecr, stack: calls.append(("snapshot", stack)) or dict(self.BEFORE)), \
                patch.object(lane, "select", lambda ecr, stack, new, running: calls.append((stack, new, running)) or {}):
            self.assertEqual(publish.select_lane("stage", self.RUNNING, "a" * 40), (self.SELECTED, self.BEFORE))
        self.assertEqual(calls, [("paired", "111122223333", "a" * 40), ("snapshot", "stage"),
                                 ("stage", self.SELECTED, self.RUNNING)])
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": "registry.example/other"}):
            with self.assertRaises(ValueError):
                publish.select_lane("stage", self.RUNNING, "a" * 40)

    def test_a_lane_repository_error_keeps_its_aws_code(self):
        lane = publish.lane_release()
        out = io.StringIO()
        with patch.object(publish, "main", side_effect=lane.CommandFailed("AccessDeniedException")), \
                contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            publish.run([])
        self.assertEqual(json.loads(out.getvalue()), {"status": "failed", "errorType": "CommandFailed",
                                                      "code": "AccessDeniedException"})

    def test_the_staging_deploy_passes_the_lane_setting_and_follows_the_lane_build(self):
        workflow = (HERE.parent.parent / ".github/workflows/fleet-coordinator-deploy.yml").read_text()
        step = workflow.split("- name: Publish and release the staging coordinator", 1)[1].split("\n\n", 1)[0]
        self.assertIn("FLEET_LAMBDA_LANE: ${{ vars.FLEET_LAMBDA_LANE }}", step)
        self.assertIn("LAMBDA_TILE_REPO: ${{ secrets.LAMBDA_TILE_REPO }}", step)
        # publish.py runs fleet/lambda/release.py and reads the lane workflow's paths in the credentialed job.
        paths = re.findall(r"^      - '([^']+)'", workflow.split("\n  push:\n", 1)[1].split("\n  workflow_", 1)[0], re.M)
        for path in ("fleet/lambda/release.py", ".github/workflows/fleet-lambda-image.yml"):
            self.assertIn(path, paths)
        # A finished lane publish on main starts a staging release (invariant 1 orders it in publish.py).
        triggers = workflow.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertEqual(re.findall(r"^  (\w+):", triggers, re.M), ["push", "workflow_run", "workflow_dispatch"])
        trigger = triggers.split("\n  workflow_run:\n", 1)[1].split("\n  workflow_dispatch:", 1)[0]
        self.assertIn('workflows: ["Fleet Lambda lane image"]', trigger)
        self.assertIn("types: [completed]", trigger)
        self.assertIn("branches: [main]", trigger)
        for job in ("build", "deploy"):
            condition = workflow.split(f"\n  {job}:\n", 1)[1].split("\n    runs-on:", 1)[0]
            condition = " ".join(condition.split("if: >-", 1)[1].split())
            self.assertEqual(condition,
                             "github.repository == 'ejc3/fcvm' && github.ref == 'refs/heads/main' && "
                             "(github.event_name != 'workflow_run' || (vars.FLEET_LAMBDA_LANE == 'on' && "
                             "github.event.workflow_run.conclusion == 'success' && "
                             "github.event.workflow_run.event == 'push' && "
                             "github.event.workflow_run.head_branch == 'main' && "
                             "github.event.workflow_run.head_repository.full_name == github.repository))", job)
        # Invariant 2: a lane-started run never shares the push deploys' group.
        group = workflow.split("\nconcurrency:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("group: ${{ github.event_name == 'workflow_run' && format('fleet-coordinator-stage-lane-{0}', "
                      "github.event.workflow_run.id) || 'fleet-coordinator-stage' }}", group)
        # Invariant 3: every release runs in the deploy job, one at a time in the smokes' queued group.
        deploy = workflow.split("\n  deploy:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        self.assertIn("    concurrency:\n      group: fleet-viewer-smoke\n      cancel-in-progress: false\n", deploy)
        self.assertIn("      queue: max\n", deploy)
        self.assertIn("fetch-depth: 0", deploy)
        self.assertIn("run: python3 -B fleet/coord/publish.py", deploy)
        # The order step decides before any release call; the release step runs only when it says release,
        # so a skipped run's release step concludes "skipped" (prod_release.released ignores it).
        order = deploy.split("      - name: Order the release (an ancestor of what staging runs is skipped)\n", 1)[1]
        self.assertIn("        id: order\n", order.split("\n      - name:", 1)[0])
        self.assertIn("run: python3 -B fleet/coord/publish.py --order", order.split("\n      - name:", 1)[0])
        release = deploy.split("      - name: Publish and release the staging coordinator\n", 1)[1].split("\n      - name:", 1)[0]
        self.assertIn("        if: steps.order.outputs.skip == 'false'\n", release)
        self.assertLess(deploy.index("Order the release"), deploy.index("Publish and release the staging coordinator"))
        self.assertNotIn("outputs.status", workflow)
        self.assertNotIn("id: release", workflow)
        # A successful deploy is always smoked, and the smoke knows it follows this run's release.
        smoke_job = workflow.split("\n  smoke:\n", 1)[1]
        self.assertIn("    if: github.repository == 'ejc3/fcvm' && github.ref == 'refs/heads/main'\n", smoke_job)
        # The smoke follows this run's release only when the order step did not skip it.
        self.assertIn("    outputs:\n      skip: ${{ steps.order.outputs.skip }}\n", deploy)
        self.assertIn("      released: ${{ needs.deploy.outputs.skip == 'false' }}\n", smoke_job)
        # The served image reports its commit before it is published (invariant 4's other half).
        self.assertIn('http://127.0.0.1:24120/version)" = "{\\"commit\\":\\"$GITHUB_SHA\\"}"', workflow)
        # One place for the switch, which the job conditions and the step env read: the repository variable.
        protocol = (HERE.parent.parent / "fleet/PROTOCOL.md").read_text()
        self.assertEqual(len(protocol.split("- With the repository variable `FLEET_LAMBDA_LANE` set to `on`", 1)), 2)
        self.assertNotRegex(protocol, r"FLEET_LAMBDA_LANE[^\n]*\n?[^\n]*in `fleet-stage-deploy`")


class SmokeCommitTests(unittest.TestCase):
    """Invariant 4: a smoke says which commit it tested. The coordinator reports its build's commit at
    /version; the smoke compares it with its run's commit (GITHUB_SHA)."""

    RUN = "a" * 40

    def attribute(self, answer):
        with patch.object(smoke, "get", lambda url, headers=None: answer):
            return smoke.commit_attribution("https://api.example", {"h": "v"}, self.RUN)

    def test_the_run_commit_is_the_one_tested(self):
        self.assertEqual(self.attribute((200, {}, json.dumps({"commit": self.RUN}).encode())),
                         {"tested_commit": self.RUN, "run_commit": self.RUN, "same": True})

    def test_another_commit_is_named(self):
        other = "e" * 40
        self.assertEqual(self.attribute((200, {}, json.dumps({"commit": other}).encode())),
                         {"tested_commit": other, "run_commit": self.RUN, "same": False})

    def test_a_coordinator_that_cannot_say_is_reported_as_unknown(self):
        for answer in ((404, {}, b""), (200, {}, b"not json"), (200, {}, b'{"commit": null}'),
                       (200, {}, b'{"commit": "https://example.invalid"}')):
            with self.subTest(answer=answer):
                self.assertEqual(self.attribute(answer), {"tested_commit": None, "run_commit": self.RUN, "same": False})

    def test_a_version_request_that_fails_in_transport_is_an_unknown_commit(self):
        import http.client, socket, urllib.error
        for error in (TimeoutError("timed out"), socket.timeout("timed out"), ConnectionResetError(),
                      urllib.error.URLError("unreachable"), http.client.RemoteDisconnected("closed")):
            with self.subTest(error=type(error).__name__):
                def get(url, headers=None):
                    raise error
                with patch.object(smoke, "get", get):
                    self.assertEqual(smoke.commit_attribution("https://api.example", {}, self.RUN),
                                     {"tested_commit": None, "run_commit": self.RUN, "same": False})

    def test_the_agents_mode_prints_the_agents_record_and_nothing_else(self):
        source = (HERE / "stage-smoke.py").read_text()
        body = source.split("\ndef main(", 1)[1]
        check = body.index('raise AssertionError("the CI token reached the agents-only endpoint")')
        attribution = body.index("commit_attribution(api, headers")
        guard = body.rindex("if not agents:", check, attribution)
        self.assertLess(check, guard)
        self.assertLess(guard, attribution)

    def test_the_notice_is_for_a_smoke_that_follows_its_own_release(self):
        other = {"tested_commit": "e" * 40, "run_commit": self.RUN, "same": False}
        self.assertEqual(smoke.commit_lines(other, released=True),
                         [other, {"notice": "this smoke tests the commit in tested_commit, not the one its run released"}])
        # A web deploy, the release gate, the schedule or a dispatch released no coordinator: no notice.
        self.assertEqual(smoke.commit_lines(other, released=False), [other])
        same = {"tested_commit": self.RUN, "run_commit": self.RUN, "same": True}
        self.assertEqual(smoke.commit_lines(same, released=True), [same])
        workflow = (HERE.parent.parent / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        self.assertIn("      released:\n        type: boolean\n        default: false\n", workflow)
        self.assertEqual(workflow.count("SMOKE_RELEASED: ${{ inputs.released }}"), 2)

    def test_the_smoke_prints_it_after_the_access_check(self):
        source = (HERE / "stage-smoke.py").read_text()
        body = source.split("\ndef main(", 1)[1]
        self.assertLess(body.index('step("agents-only endpoint check")'), body.index("commit_attribution(api, headers"))
        self.assertLess(body.index("commit_attribution(api, headers"), body.index('step("/live connect")'))


class ResolveRetransmitTests(unittest.TestCase):
    """resolve() retransmits one query within its deadline, as a stub resolver does."""

    def serve(self, drop):
        """A UDP DNS stub on loopback that ignores the first `drop` datagrams (all of them when
        None) and answers the rest with 192.0.2.7; returns (address, received list)."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind(("127.0.0.1", 0))
        sock.settimeout(0.2)
        received, stop = [], threading.Event()

        def loop():
            while not stop.is_set():
                try:
                    query, peer = sock.recvfrom(4096)
                except socket.timeout:
                    continue
                received.append(query)
                if drop is None or len(received) <= drop:
                    continue
                question = query[12:len(query) - 11]
                reply = query[:2] + b"\x81\x80\x00\x01\x00\x01\x00\x00\x00\x00" + question
                kind = struct.unpack(">H", question[-4:-2])[0]
                rdata = bytes([192, 0, 2, 7]) if kind == 1 else socket.inet_pton(socket.AF_INET6, "2001:db8::7")
                reply += b"\xc0\x0c" + struct.pack(">HHIH", kind, 1, 60, len(rdata)) + rdata
                sock.sendto(reply, peer)

        thread = threading.Thread(target=loop)
        thread.start()

        def close():
            stop.set()
            thread.join()
            sock.close()

        self.addCleanup(close)
        return "127.0.0.1:%d" % sock.getsockname()[1], received

    def test_a_lost_first_datagram_is_retransmitted_and_answered(self):
        resolver, received = self.serve(drop=1)
        found, sends = smoke.resolve(resolver, "tile.example", 1, 2.0)
        self.assertEqual([str(a) for a in found], ["192.0.2.7"])
        self.assertEqual(sends, 2)
        self.assertEqual(len(set(received)), 1, "a retransmit repeats the same query and id")

    def test_a_resolver_that_never_answers_fails_inside_the_deadline_and_says_which(self):
        resolver, received = self.serve(drop=None)
        start = time.monotonic()
        with self.assertRaises(TimeoutError) as caught:
            smoke.resolve(resolver, "tile.example", 28, 1.2)
        self.assertLess(time.monotonic() - start, 1.2 + 0.5)
        message = str(caught.exception)
        for part in (resolver, "AAAA", "%d datagrams" % len(received)):
            self.assertIn(part, message)
        self.assertNotIn("tile.example", message)

    def test_the_failure_names_the_tile_key_and_the_success_reports_sends(self):
        resolver, _ = self.serve(drop=None)
        tile = dict(frame_url="wss://a.fcvm.dev/frames", frame_url_v4="wss://b.fcvm.dev:20200/frames")
        out = io.StringIO()
        with patch.object(smoke, "PUBLIC_RESOLVERS", (resolver,)), patch.object(smoke, "RESOLVE_WITHIN", 0.6), \
                contextlib.redirect_stdout(out):
            with self.assertRaises(TimeoutError):
                smoke.prove_resolves(tile)
        self.assertEqual(json.loads(out.getvalue())["record"], "frame_url")
        resolver, _ = self.serve(drop=1)
        with patch.object(smoke, "PUBLIC_RESOLVERS", (resolver,)):
            slowest, sends = smoke.prove_resolves(tile)
        self.assertEqual(sends, 2)

    def test_the_failure_line_names_the_resolver_record_and_datagrams(self):
        # 2026-11-19: the scheduled smoke printed {"errorType": "TimeoutError", "error": null}.
        resolver, received = self.serve(drop=None)
        tile = dict(frame_url="wss://a.fcvm.dev/frames", frame_url_v4="wss://b.fcvm.dev:20200/frames")
        out = io.StringIO()
        with patch.object(smoke, "PUBLIC_RESOLVERS", (resolver,)), patch.object(smoke, "RESOLVE_WITHIN", 0.6), \
                contextlib.redirect_stdout(out), self.assertRaises(TimeoutError) as caught:
            smoke.prove_resolves(tile)
        self.assertIsNotNone(smoke.describe(caught.exception), "the failure line would print no error text")
        lines = [json.loads(line) for line in out.getvalue().splitlines()]
        self.assertEqual(lines, [{"resolve": "no answer", "record": "frame_url", "type": "AAAA",
                                  "resolver": resolver, "sends": len(received), "within_s": 0.6}])
        self.assertNotIn("fcvm.dev", out.getvalue() + str(caught.exception))


class SettleBoundTests(unittest.TestCase):
    """How long the viewer path waits for every mode's tiles to run. Generation 21 (2026-11-18)
    never registered an agent, so its tiles never came and each family waited the whole 600 s;
    the release gate passes a shorter bound."""

    def wait_bound(self, **kw):
        clock, bounds = [1000.0], []

        def until(predicate, by=None):
            bounds.append(by)
            return {}
        with patch.object(smoke.time, "monotonic", lambda: clock[0]), contextlib.redirect_stdout(io.StringIO()):
            smoke.viewer_path("v4", ("wall",), False, [], until, None, lambda: ({}, {}, "v-1"), **kw)
        return bounds[0] - clock[0]

    def test_the_wait_for_tiles_is_bounded_by_the_settle_seconds(self):
        self.assertEqual(self.wait_bound(), 600)
        self.assertEqual(self.wait_bound(settle_seconds=300), 300)

    def test_the_settle_seconds_argument_reaches_the_viewer_path(self):
        self.assertEqual(smoke.arguments(["--modes", "wall"])["settle_seconds"], 600)
        self.assertEqual(smoke.arguments(["--modes", "wall", "--settle-seconds", "300"])["settle_seconds"], 300)
        for bad in ("0", "-5", "601", "soon"):
            with self.subTest(bad=bad), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                smoke.arguments(["--modes", "wall", "--settle-seconds", bad])
        seen = {}
        with patch.object(smoke, "main", lambda **kw: seen.update(kw) or 0):
            smoke.run(["--modes", "wall", "--settle-seconds", "300"])
        self.assertEqual(seen["settle_seconds"], 300)


class WorkflowTests(unittest.TestCase):
    def test_changes_to_the_actual_cli_files_trigger_a_release(self):
        workflow = (HERE.parent.parent / ".github/workflows/fleet-coordinator-deploy.yml").read_text()
        import re
        patterns = re.findall(r"^      - '([^']+)'", workflow, re.M)
        for path in ("src/cli/mod.rs", "src/cli/args.rs"):
            self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns), path)

    def test_the_coordinator_release_and_the_smokes_never_overlap(self):
        # A web deploy's IPv6 smoke read /live while a coordinator deploy replaced the one
        # coordinator task, and got "WebSocket upgrade refused".
        # The job that replaces the task takes the group every smoke job holds.
        import re
        workflows = HERE.parent.parent / ".github/workflows"
        viewer = (workflows / "fleet-viewer-smoke.yml").read_text()
        # queue: max on every job in the group: with GitHub's default of one pending job, a
        # smoke that queues behind a pending release cancels the release.
        block = (r"(?m)^    concurrency:\n      group: (\S+)\n      cancel-in-progress: false\n"
                 r"(?:      #.*\n)*      queue: max$")
        groups = re.findall(block, viewer)
        self.assertEqual(groups, ["fleet-viewer-smoke"] * 2, "the IPv4 and IPv6 smoke jobs share one queued group")
        deploy = (workflows / "fleet-coordinator-deploy.yml").read_text()
        job = deploy.split("\n  deploy:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        self.assertIn("python3 -B fleet/coord/publish.py", job, "this is the job that replaces the task")
        self.assertEqual(re.findall(block, job), ["fleet-viewer-smoke"])


def ts(seconds):
    """A GitHub timestamp `seconds` after a fixed instant."""
    import datetime
    base = datetime.datetime(2026, 11, 19, tzinfo=datetime.timezone.utc)
    return (base + datetime.timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


T0 = ts(0)


def at(seconds):
    return prod.when(ts(seconds))


class FakeGitHub:
    """Workflow runs and their jobs, as the REST API lists them."""

    def __init__(self):
        self.runs = {wf: [] for wf in prod.WORKFLOWS.values()}
        self.jobs = {}
        self.next = 100

    def run(self, workflow, sha, jobs, updated=10_000, status="completed"):
        self.next += 1
        self.runs[workflow].insert(0, {"id": self.next, "head_sha": sha, "head_branch": "main", "event": "push",
            "status": status, "updated_at": ts(updated), "head_repository": {"full_name": "ejc3/fcvm"}})
        self.jobs[self.next] = jobs
        return self.next

    def api(self, path):
        if path.startswith("actions/workflows/"):
            return {"workflow_runs": self.runs[path.split("/")[2]]}
        return {"jobs": self.jobs[int(path.split("/")[2])]}


def release_job(name, step, start, end, conclusion="success", status="completed"):
    return {"name": name, "status": status, "conclusion": conclusion if status == "completed" else None,
            "steps": [{"name": step, "conclusion": conclusion if status == "completed" else None, "completed_at": ts(end)}],
            "started_at": ts(start), "completed_at": ts(end)}


def viewers(start, end, prefix="smoke / ", conclusion="success"):
    return [{"name": prefix + "IPv4-only viewer", "conclusion": conclusion, "started_at": ts(start), "completed_at": ts(start + 200)},
            {"name": prefix + "IPv6 viewer", "conclusion": conclusion, "started_at": ts(start + 210), "completed_at": ts(end)}]


class ProdDecideTests(unittest.TestCase):
    """The staging set prod takes: what staging runs, and a smoke that ran against exactly that."""

    C, W, D = "c" * 40, "e" * 40, "sha256:" + "d" * 64
    RECEIPT = {"generation": 12, "build_id": "factory:new"}

    def setUp(self):
        self.gh = FakeGitHub()
        coordinator = self.gh.run("fleet-coordinator-deploy.yml", self.C,
            [release_job("deploy", "Publish and release the staging coordinator", 900, 1000)] + viewers(1100, 1500))
        self.coordinator_run = coordinator
        self.gh.run("fleet-web-deploy.yml", self.W, [release_job("Deploy fcvm-fleet-stage", "Deploy", 1010, 1050)])
        self.candidate = {"status": "ok", "stage": {"digest": self.D, "commits": [self.C]},
                          "prod": {"digest": "sha256:" + "a" * 64, "commits": ["a" * 40]}, "release": None}
        self.status = {"status": "ok", "stage": self.RECEIPT, "verified": self.RECEIPT, "stage_verified": True,
                       "verified_at": int(at(500)), "prod": {"generation": 11, "build_id": "factory:old"},
                       "asleep": True, "reason": None}
        env = patch.dict(os.environ, {"GITHUB_REPOSITORY": "ejc3/fcvm", "GH_TOKEN": "t",
                                      "COORDINATOR_RELEASE": "fcvm-coordinator-release-prod",
                                      "IMAGE_RELEASE": "fcvm-fleet-ami-prod-release"})
        env.start()
        self.addCleanup(env.stop)
        self.calls = []
        # A second candidate read (decide re-reads it after its checks with the prod lane on) answers
        # self.second when set, so a test can tell the two reads apart.
        self.second = None
        def invoke(function, payload):
            self.calls.append(payload)
            if function == "fcvm-coordinator-release-prod":
                reads = sum(1 for c in self.calls if c == {"action": "candidate"})
                return copy.deepcopy(self.second if reads > 1 and self.second is not None else self.candidate)
            return copy.deepcopy(self.status)
        patches = [patch.object(prod, "api", self.gh.api), patch.object(prod.publish, "invoke", invoke)]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_a_smoke_after_both_releases_and_the_verification_promotes_the_set(self):
        target, prior, changes, _ = prod.decide()
        self.assertEqual(target, {"coordinator": {"commit": self.C, "digest": self.D}, "web_commit": self.W,
                                  "generation": 12, "build_id": "factory:new", "smoke_run": self.coordinator_run})
        self.assertEqual(changes, {"image": True, "coordinator": True, "web": True})
        self.assertEqual(prior["coordinator"], {"commit": "a" * 40, "digest": "sha256:" + "a" * 64})
        self.assertEqual((prior["generation"], prior["worker_version"]), (11, None))

    def skips(self, why):
        with self.assertRaisesRegex(prod.Skip, why):
            prod.decide()

    def test_a_smoke_that_started_before_the_web_release_finished_does_not_count(self):
        self.gh.run("fleet-web-deploy.yml", "f" * 40, [release_job("Deploy fcvm-fleet-stage", "Deploy", 1050, 1200)])
        self.skips("no staging smoke")

    def test_a_smoke_before_the_generation_was_verified_does_not_count_unless_it_is_the_verifying_one(self):
        self.status["verified_at"] = int(at(1600))
        self.skips("no staging smoke")
        jobs = viewers(1300, 1550, prefix="Staging smoke / ") + [
            {"name": "Verify or roll back", "conclusion": "success", "started_at": ts(1560), "completed_at": ts(1620)}]
        gate = self.gh.run("fleet-release-gate.yml", "0" * 40, jobs)
        self.assertEqual(prod.decide()[0]["smoke_run"], gate)

    def test_a_failed_viewer_is_no_smoke(self):
        self.gh.jobs[self.coordinator_run][-1]["conclusion"] = "failure"
        self.skips("no staging smoke")

    def test_the_release_that_finished_last_is_staging_even_from_an_older_run(self):
        # An older run rerun after a newer one deployed: its deploy finished last, so staging serves it.
        older = self.gh.runs["fleet-web-deploy.yml"][-1]
        self.gh.run("fleet-web-deploy.yml", "f" * 40, [release_job("Deploy fcvm-fleet-stage", "Deploy", 1020, 1040)])
        self.assertEqual(prod.released("web")[0], self.W)
        self.gh.jobs[older["id"]] = [release_job("Deploy fcvm-fleet-stage", "Deploy", 1600, 1700, status="in_progress")]
        with self.assertRaisesRegex(prod.Skip, "in progress"):
            prod.released("web")

    def test_staging_running_an_unverified_generation_promotes_nothing(self):
        self.status["stage_verified"] = False
        self.skips("not verified")

    def test_a_release_in_progress_promotes_nothing(self):
        self.gh.run("fleet-coordinator-deploy.yml", "1" * 40,
            [release_job("deploy", "Publish and release the staging coordinator", 2000, 2100, status="in_progress")])
        self.skips("in progress")

    def test_a_release_skipped_for_an_ancestor_is_not_what_staging_runs(self):
        # The descendant deploys, then an ancestor's run finishes later: its order step skipped the release, so
        # its release step concludes "skipped" and staging still runs the descendant.
        job = release_job("deploy", "Publish and release the staging coordinator", 2000, 2100)
        job["steps"] = [{"name": "Order the release (an ancestor of what staging runs is skipped)", "conclusion": "success",
                         "completed_at": ts(2050)},
                        {"name": "Publish and release the staging coordinator", "conclusion": "skipped", "completed_at": ts(2050)}]
        self.gh.run("fleet-coordinator-deploy.yml", "1" * 40, [job])
        self.assertEqual(prod.released("coordinator")[0], self.C)
        self.assertEqual(prod.decide()[0]["coordinator"]["commit"], self.C)

    def test_a_failed_newer_release_falls_back_and_staging_pointer_must_agree(self):
        self.gh.run("fleet-coordinator-deploy.yml", "1" * 40,
            [release_job("deploy", "Publish and release the staging coordinator", 2000, 2100, conclusion="failure")])
        self.assertEqual(prod.decide()[0]["coordinator"]["commit"], self.C)
        self.candidate["stage"]["commits"] = ["1" * 40]
        self.skips("not its last successful staging release")

    STAGED, RUNNING = "sha256:" + "2" * 64, "sha256:" + "1" * 64

    def lane_on(self, selection=None, stage_runs=None, prod_runs=None):
        """The prod lane on. What each stack runs comes from the candidate (its running task's lane image);
        staging's selection (keep-stage-current) is only checked against it."""
        lane = publish.lane_release()
        selected = lane.Image(selection or self.STAGED, "{}", lane.MEDIA_TYPES[0])
        ecr = types.SimpleNamespace(tagged=lambda tag: selected if tag == "keep-stage-current" else None)
        for p in (patch.dict(os.environ, {"FLEET_LAMBDA_LANE_PROD": "on"}),
                  patch.object(prod.publish, "staging_lane", lambda: (lane, ecr))):
            p.start()
            self.addCleanup(p.stop)
        self.candidate["stage"]["lambda_digest"] = self.STAGED if stage_runs is None else stage_runs
        self.candidate["prod"]["lambda_digest"] = self.RUNNING if prod_runs is None else prod_runs or None

    def record(self, **extra):
        return {"coordinator": {"commit": self.C, "digest": self.D}, "web_commit": self.W,
                "worker_version": "0" * 8 + "-0000-0000-0000-" + "0" * 12, **self.RECEIPT, "smoke_run": 1,
                **extra, "previous": None}

    def test_with_the_prod_lane_on_the_set_carries_the_lane_image_staging_runs(self):
        self.lane_on()
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(target["lambda_digest"], self.STAGED)
        self.assertNotIn("lambda_digest", prior)
        self.assertEqual(prior_lane, self.RUNNING)
        self.assertEqual(changes, {"image": True, "coordinator": True, "web": True, "lane": True})

    def test_a_record_without_a_lane_image_is_passed_back_unchanged(self):
        # The record's compare-and-swap takes prior as the record it replaces: it must be the stored entry, byte
        # for byte. (Prod runs no lane image, which is what an entry without lambda_digest names.)
        self.lane_on(prod_runs="")
        self.candidate["prod"].update({"digest": self.D, "commits": [self.C]})
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = self.record()
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(prior, prod.entry(self.candidate["release"]))
        self.assertEqual(prior_lane, "")

    def test_before_prods_first_lane_image_nothing_says_null(self):
        self.lane_on(prod_runs="")
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(target["lambda_digest"], self.STAGED)
        self.assertNotIn(None, (target.get("lambda_digest"), prior_lane))
        self.assertEqual(prior_lane, "")
        self.assertNotIn("lambda_digest", prior)

    def test_a_lane_selection_staging_does_not_run_is_not_promoted(self):
        # The tags stay moved after a cancelled deploy, a hand selection or an unknown running image: the set is
        # what staging runs, and a selection that differs from it is no set a smoke covered.
        self.lane_on(selection="sha256:" + "9" * 64)
        self.skips("not the lane image it runs")
        self.setUp()
        self.lane_on(stage_runs="")
        self.skips("staging runs no lane image")

    def test_prod_running_another_lane_image_than_its_record_names_is_outside_a_promotion(self):
        self.lane_on()
        self.candidate["prod"].update({"digest": self.D, "commits": [self.C]})
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = self.record(lambda_digest="sha256:" + "8" * 64)
        self.skips("its record names another lane image")

    def test_a_new_lane_image_alone_is_a_promotion(self):
        self.lane_on()
        self.candidate["prod"].update({"digest": self.D, "commits": [self.C]})
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = self.record(lambda_digest=self.RUNNING)
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(changes, {"image": False, "coordinator": False, "web": False, "lane": True})
        self.assertEqual(prior, prod.entry(self.candidate["release"]))
        self.setUp()
        self.lane_on(prod_runs=self.STAGED)
        self.candidate["prod"].update({"digest": self.D, "commits": [self.C]})
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = self.record(lambda_digest=self.STAGED)
        self.skips("already runs")

    def test_the_lane_image_comes_from_the_read_after_the_checks(self):
        self.lane_on(selection="sha256:" + "6" * 64, stage_runs="sha256:" + "7" * 64)
        self.second = copy.deepcopy(self.candidate)
        self.second["stage"]["lambda_digest"] = "sha256:" + "6" * 64
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(target["lambda_digest"], "sha256:" + "6" * 64)
        self.assertEqual(self.calls.count({"action": "candidate"}), 2)

    def test_staging_changing_between_the_two_reads_promotes_nothing(self):
        self.lane_on()
        self.second = copy.deepcopy(self.candidate)
        self.second["stage"]["digest"] = "sha256:" + "f" * 64
        self.skips("changed while the promotion was deciding")

    def test_a_lane_image_the_function_could_not_read_promotes_nothing(self):
        for stack in ("stage", "prod"):
            with self.subTest(stack=stack):
                self.setUp()
                self.lane_on()
                self.candidate[stack]["lane_error"] = "pointer-mismatch"
                self.skips("could not read the lane image " + stack + " runs")

    def test_a_record_without_a_lane_image_means_none(self):
        # With the lane on, an entry without lambda_digest names no lane image. Prod running one is a change no
        # promotion recorded (a lane-only promotion whose rollback also failed), not "already runs the set".
        self.lane_on(prod_runs=self.STAGED)
        self.candidate["prod"].update({"digest": self.D, "commits": [self.C]})
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = self.record()
        self.skips("its record names another lane image")

    def test_with_the_prod_lane_off_one_candidate_read_and_no_prior_lane(self):
        target, prior, changes, prior_lane = prod.decide()
        self.assertEqual(self.calls.count({"action": "candidate"}), 1)
        self.assertIsNone(prior_lane)
        out = tempfile.NamedTemporaryFile("r", delete=False)
        self.addCleanup(os.unlink, out.name)
        with patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.main(["decide"]), 0)
        self.assertNotIn("prior_lane", Path(out.name).read_text())

    def test_with_the_prod_lane_off_the_set_is_todays(self):
        target, prior, changes, prior_lane = prod.decide()
        self.assertNotIn("lambda_digest", target)
        self.assertNotIn("lambda_digest", prior)
        self.assertEqual(set(changes), {"image", "coordinator", "web"})
        self.assertIsNone(prior_lane)

    def test_prod_already_running_the_set_promotes_nothing(self):
        self.candidate["prod"] = {"digest": self.D, "commits": [self.C]}
        self.status["prod"] = dict(self.RECEIPT)
        self.candidate["release"] = {"coordinator": {"commit": self.C, "digest": self.D}, "web_commit": self.W,
            "worker_version": "0" * 8 + "-0000-0000-0000-" + "0" * 12, **self.RECEIPT, "smoke_run": 1, "previous": None}
        self.skips("already runs")

    def test_a_record_that_does_not_describe_prod_stops_the_promotion(self):
        self.candidate["release"] = {"coordinator": {"commit": self.C, "digest": self.D}, "web_commit": self.W,
            "worker_version": None, "generation": 11, "build_id": "factory:old", "smoke_run": 1, "previous": None}
        self.skips("outside a promotion")

    def test_runs_from_another_repository_or_branch_are_ignored(self):
        for run in self.gh.runs["fleet-coordinator-deploy.yml"]:
            run["head_repository"] = {"full_name": "someone/fork"}
        self.skips("no staging coordinator release")


class ProdStepTests(unittest.TestCase):
    """hold, the image steps and the exit codes: a held step changes nothing and exits 3."""

    def setUp(self):
        env = patch.dict(os.environ, {"COORDINATOR_RELEASE": "fcvm-coordinator-release-prod",
                                      "IMAGE_RELEASE": "fcvm-fleet-ami-prod-release"})
        env.start()
        self.addCleanup(env.stop)
        # prod-status answers prod's current receipt; image_promote reads it before its first call.
        self.answers, self.calls = {"prod-status": {"prod": {"generation": 11, "build_id": "factory:old"}}}, []
        def invoke(function, payload):
            self.calls.append(payload.get("action", "release"))
            answer = self.answers[payload.get("action")]
            return answer.pop(0) if isinstance(answer, list) else answer
        p = patch.object(prod.publish, "invoke", invoke)
        p.start()
        self.addCleanup(p.stop)

    def test_an_image_change_waits_for_prod_to_sleep_and_a_coordinator_change_for_no_viewer(self):
        self.answers["prod-status"] = [{"asleep": False, "reason": "awake"}, {"asleep": True}]
        clock, sleeps = iter([0, 60, 120]), []
        self.assertTrue(prod.hold(True, deadline=1000, clock=lambda: next(clock), sleep=sleeps.append))
        self.assertEqual((self.calls, sleeps), (["prod-status", "prod-status"], [60]))
        self.calls.clear()
        self.answers["quiet"] = [{"status": "held", "reason": "viewers"}] * 3
        clock = iter([0, 500, 1000, 1500])
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertFalse(prod.hold(False, deadline=1000, clock=lambda: next(clock), sleep=lambda _: None))
        self.assertEqual(set(self.calls), {"quiet"})

    def test_a_held_promotion_exits_3_and_a_refused_one_fails(self):
        receipt = '{"generation": 12, "build_id": "factory:new"}'
        for answer, code in (({"status": "held", "reason": "awake"}, prod.HELD),
                             ({"status": "refused", "reason": "x"}, 1),
                             ({"status": "promoted", "generation": 12, "build_id": "factory:new"}, 0)):
            with self.subTest(answer=answer["status"]):
                self.answers["promote-prod"] = answer
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(prod.main(["image-promote", receipt]), code)

    def test_a_promotion_waits_while_terraforms_lock_is_held(self):
        # The image release function shares Terraform's backend lock. With ejc3/aws#1014 applied it answers "busy"
        # while someone else holds it (an apply, a plan, another release); until then a held lock is still a function
        # error, which is what failed run 45149811634. Busy means nothing ran: ask again, at most 11 times.
        receipt = {"generation": 24, "build_id": "factory:new"}
        self.answers["promote-prod"] = [{"status": "busy"}] * 10 + [{"status": "promoted", **receipt}]
        sleeps = []
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.image_promote(receipt, sleep=sleeps.append), "promoted")
        self.assertEqual(sleeps, [60] * 10)
        self.assertEqual(self.calls.count("promote-prod"), 11)
        self.assertEqual(self.calls.count("prod-status"), 1)

    def test_a_promotion_still_locked_after_eleven_answers_is_held_not_failed(self):
        # Held (exit 3): the release job ends with a notice and no rollback runs for a set that never moved. Held only
        # when prod-status, which takes no lock, still names the receipt prod ran before the call.
        receipt = '{"generation": 24, "build_id": "factory:new"}'
        old = {"generation": 11, "build_id": "factory:old"}
        self.answers["prod-status"] = [{"prod": old}, {"prod": old}]
        self.answers["promote-prod"] = [{"status": "busy"}] * 11 + [{"status": "promoted", "generation": 24, "build_id": "factory:new"}]
        with contextlib.redirect_stdout(io.StringIO()), patch.object(prod.time, "sleep", lambda _: None):
            self.assertEqual(prod.main(["image-promote", receipt]), prod.HELD)
        self.assertEqual(self.calls.count("promote-prod"), 11)
        workflow = (HERE.parent.parent / ".github/workflows/fleet-prod-promote.yml").read_text()
        notice = [line for line in workflow.splitlines() if 'if [ "$rc" = 3 ]' in line and "image" in line]
        self.assertEqual(len(notice), 1)
        self.assertIn("Terraform's lock", notice[0])

    def test_a_lock_held_after_the_promotion_completed_is_that_promotion(self):
        # publish.invoke retries a call that ended without a result while the function went on: the first
        # promote-prod completed and every retry answers busy. prod-status names the receipt: the release continues.
        receipt = {"generation": 24, "build_id": "factory:new"}
        self.answers["prod-status"] = [{"prod": {"generation": 11, "build_id": "factory:old"}}, {"prod": dict(receipt)}]
        self.answers["promote-prod"] = [{"status": "busy"}] * 11
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.image_promote(receipt, sleep=lambda _: None), "unchanged")
            self.answers["prod-status"] = [{"prod": {"generation": 11, "build_id": "factory:old"}}, {"prod": dict(receipt)}]
            self.answers["promote-prod"] = [{"status": "busy"}] * 11
            with patch.object(prod.time, "sleep", lambda _: None):
                self.assertEqual(prod.main(["image-promote", json.dumps(receipt)]), 0)

    def test_a_lock_held_with_prod_on_a_third_release_is_a_failure(self):
        receipt = '{"generation": 24, "build_id": "factory:new"}'
        self.answers["prod-status"] = [{"prod": {"generation": 11, "build_id": "factory:old"}},
                                       {"prod": {"generation": 9, "build_id": "factory:other"}}]
        self.answers["promote-prod"] = [{"status": "busy"}] * 11
        with contextlib.redirect_stdout(io.StringIO()), patch.object(prod.time, "sleep", lambda _: None):
            self.assertEqual(prod.main(["image-promote", receipt]), 1)

    def test_a_lock_held_with_prods_images_unreadable_is_a_failure(self):
        # prod-status refused both times names no receipt at all: two unreadable answers are not "nothing moved".
        receipt = '{"generation": 24, "build_id": "factory:new"}'
        self.answers["prod-status"] = [{"status": "refused", "reason": "x"}] * 2
        self.answers["promote-prod"] = [{"status": "busy"}] * 11
        with contextlib.redirect_stdout(io.StringIO()), patch.object(prod.time, "sleep", lambda _: None):
            self.assertEqual(prod.main(["image-promote", receipt]), 1)

    def test_a_lock_held_with_prod_already_on_the_receipt_is_that_promotion(self):
        # A rerun after an earlier attempt completed, with the lock held: prod-status names the receipt before and
        # after, which is the completed promotion and not a held one.
        receipt = {"generation": 24, "build_id": "factory:new"}
        self.answers["prod-status"] = [{"prod": dict(receipt)}, {"prod": dict(receipt)}]
        self.answers["promote-prod"] = [{"status": "busy"}] * 11
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.image_promote(receipt, sleep=lambda _: None), "unchanged")

    def test_an_image_rollback_waits_while_terraforms_lock_is_held(self):
        new, old = {"generation": 12, "build_id": "n"}, {"generation": 11, "build_id": "o"}
        self.answers["prod-status"] = {"prod": new}
        self.answers["rollback-prod"] = [{"status": "busy"}, {"status": "rolled_back", **old}]
        sleeps = []
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.image_rollback(new, old, deadline=time.time() + 1000, sleep=sleeps.append), "rolled_back")
        self.assertEqual(sleeps, [60])

    def test_an_image_rollback_keeps_asking_through_a_long_lock_until_its_deadline(self):
        # A Terraform apply that holds the lock for 30 minutes: the rollback keeps asking every 60 s and then runs.
        new, old = {"generation": 12, "build_id": "n"}, {"generation": 11, "build_id": "o"}
        self.answers["prod-status"] = {"prod": new}
        self.answers["rollback-prod"] = [{"status": "busy"}] * 30 + [{"status": "rolled_back", **old}]
        sleeps = []
        clock = lambda: sum(sleeps)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prod.image_rollback(new, old, deadline=3600, clock=clock, sleep=sleeps.append), "rolled_back")
        self.assertEqual(sleeps, [60] * 30)

    def test_an_image_rollback_that_cannot_run_fails_by_its_deadline(self):
        # A rollback that could not run fails (never held), and the busy wait ends at the rollback's own deadline, not
        # at the job's timeout or the role's session.
        new, old = {"generation": 12, "build_id": "n"}, {"generation": 11, "build_id": "o"}
        self.answers["prod-status"] = {"prod": new}
        self.answers["rollback-prod"] = [{"status": "busy"}] * 200
        sleeps = []
        clock = lambda: sum(sleeps)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(RuntimeError, "deadline") as raised:
            prod.image_rollback(new, old, deadline=1800, clock=clock, sleep=sleeps.append)
        self.assertNotIsInstance(raised.exception, prod.publish.Held)
        self.assertEqual(sum(sleeps), 1800)

    def test_the_rollback_job_says_which_return_failed(self):
        # "prod was returned to its previous set" only when every step that ran succeeded.
        workflow = (HERE.parent.parent / ".github/workflows/fleet-prod-promote.yml").read_text()
        rollback = workflow.split("\n  rollback:\n", 1)[1]
        step = rollback[rollback.index("      - name: The promotion failed\n"):]
        script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        for outcomes, says in (({"WORKER": "success", "COORDINATOR": "skipped", "IMAGES": "success"}, "was returned to its previous set"),
                               ({"WORKER": "skipped", "COORDINATOR": "success", "IMAGES": "failure"}, "Host and tile images"),
                               # A job cancelled before a return step started reports that step skipped too.
                               ({"WORKER": "skipped", "COORDINATOR": "skipped", "IMAGES": "skipped", "CANCELLED": "true"},
                                "was cancelled")):
            with self.subTest(**outcomes):
                out = subprocess.run(["bash", "-c", script],
                                     env={"PATH": os.environ["PATH"], "CANCELLED": "false", **outcomes},
                                     capture_output=True, text=True)
                self.assertEqual(out.returncode, 1)
                self.assertIn(says, out.stdout)
                if outcomes["IMAGES"] == "failure" or outcomes.get("CANCELLED") == "true":
                    self.assertNotIn("was returned to its previous set", out.stdout)
        for name, id_ in (("Worker", "worker"), ("Coordinator", "coordinator"), ("Host and tile images", "images")):
            self.assertRegex(rollback, r"      - name: " + re.escape(name) + r"\n        id: " + id_ + r"\n")
        self.assertIn("IMAGES: ${{ steps.images.outcome }}", step)
        self.assertIn("CANCELLED: ${{ job.status == 'cancelled' }}", step)
        # Status functions such as cancelled() are valid only in `if:`; in a step's env they make the whole file invalid.
        self.assertNotRegex(workflow, r"(?m)^\s+[A-Z_]+: \$\{\{[^}]*\b(cancelled|always|success|failure)\(\)")

    def test_an_image_rollback_returns_only_to_the_previous_release(self):
        new, old = {"generation": 12, "build_id": "n"}, {"generation": 11, "build_id": "o"}
        self.answers["prod-status"] = {"prod": old}
        self.assertEqual(prod.image_rollback(new, old), "unchanged")
        self.assertNotIn("rollback-prod", self.calls)
        self.answers["prod-status"] = {"prod": {"generation": 9, "build_id": "x"}}
        with self.assertRaisesRegex(RuntimeError, "neither"):
            prod.image_rollback(new, old)
        self.answers["prod-status"] = {"prod": new}
        self.answers["rollback-prod"] = [{"status": "held"}, {"status": "rolled_back", **old}]
        sleeps = []
        self.assertEqual(prod.image_rollback(new, old, deadline=time.time() + 1000, sleep=sleeps.append), "rolled_back")
        self.assertEqual(sleeps, [60])

    def test_the_record_drops_a_previous_without_a_commit(self):
        self.answers["record"] = {"status": "recorded"}
        target = {"coordinator": {"commit": "c" * 40, "digest": "sha256:" + "d" * 64}, "web_commit": "e" * 40,
                  "generation": 12, "build_id": "n", "smoke_run": 5}
        sent = []
        with patch.object(prod.publish, "invoke", lambda f, payload: sent.append(payload) or {"status": "recorded"}):
            prod.record(target, "", {"coordinator": {"commit": None, "digest": "sha256:" + "a" * 64}})
        self.assertEqual(sent[0]["release"], {**target, "worker_version": None, "previous": None})


class ProdEnabledTests(unittest.TestCase):
    def test_off_while_the_prod_stack_is_off_with_a_message(self):
        for value, expected in (("", False), ("false", False), ("true", True)):
            out = io.StringIO()
            with patch.dict(os.environ, {"ENABLED": value}), contextlib.redirect_stdout(out):
                self.assertEqual(prod.enabled(), expected)
            self.assertEqual("the prod stack is off" in out.getvalue(), not expected)


class PublishProdTests(unittest.TestCase):
    """publish.py --stack prod: no push, the prod function only, held is exit 3, force for rollback."""

    DIGEST = "sha256:" + "c" * 64

    def run_prod(self, results, extra=(), function="fcvm-coordinator-release-prod"):
        sent = []
        def invoke(name, payload):
            sent.append((name, payload))
            return results.pop(0)
        with patch.dict(os.environ, {"COORDINATOR_RELEASE": function}), patch.object(publish, "invoke", invoke), \
                patch.object(publish, "command", side_effect=AssertionError("prod pushes nothing")), \
                patch.object(publish.time, "sleep", lambda _: None), contextlib.redirect_stdout(io.StringIO()), \
                self.assertRaises(SystemExit) as exit:
            publish.run(["--stack", "prod", "--commit", "a" * 40, "--digest", self.DIGEST, *extra])
            raise SystemExit(0)
        return exit.exception.code, sent

    def test_held_exits_3_and_busy_is_asked_again(self):
        code, sent = self.run_prod([{"status": "busy"}, {"status": "held", "reason": "viewers"}])
        self.assertEqual(code, publish.HELD)
        self.assertEqual(sent, [("fcvm-coordinator-release-prod", {"commit": "a" * 40, "digest": self.DIGEST})] * 2)

    def test_force_is_sent_only_when_asked(self):
        code, sent = self.run_prod([{"status": "deployed", "digest": self.DIGEST}], extra=["--force"])
        self.assertEqual(code, 0)
        self.assertEqual(sent[0][1]["force"], True)

    def test_the_staging_function_is_refused_for_prod(self):
        code, sent = self.run_prod([], function="fcvm-coordinator-release")
        self.assertEqual((code, sent), (1, []))

    def test_staging_takes_no_commit_or_force(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            publish.arguments(["--force"])
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            publish.arguments(["--stack", "prod", "--commit", "a" * 40])


class ProdLaneTests(unittest.TestCase):
    """publish.py --stack prod --lambda-digest D: the lane image of the set prod takes (a promotion) or returns to
    (a rollback) is selected for prod and released with the coordinator. Without it the release is today's."""

    DIGEST = "sha256:" + "c" * 64
    RUNNING, STAGED, EARLIER = "sha256:" + "1" * 64, "sha256:" + "2" * 64, "sha256:" + "0" * 64
    BEFORE = {"current": RUNNING, "previous": EARLIER, "next": RUNNING}

    def run_prod(self, results, extra=(), repository=None):
        results, events = list(results), []
        def invoke(name, payload):
            events.append(("invoke", payload))
            outcome = results.pop(0)
            if isinstance(outcome, Exception):
                raise outcome
            return outcome
        def select_prod_lane(running, digest):
            events.append(("select", running, digest))
            return digest, dict(self.BEFORE)
        def restore_lane(stack, before):
            events.append(("restore", stack, before))
        env = {"COORDINATOR_RELEASE": "fcvm-coordinator-release-prod", "LAMBDA_TILE_REPO": repository or "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile-prod"}
        with patch.dict(os.environ, env), patch.object(publish, "invoke", invoke), \
                patch.object(publish, "select_prod_lane", select_prod_lane), patch.object(publish, "restore_lane", restore_lane), \
                patch.object(publish, "command", side_effect=AssertionError("prod pushes nothing")), \
                patch.object(publish.time, "sleep", lambda _: None), contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit:
            publish.run(["--stack", "prod", "--commit", "a" * 40, "--digest", self.DIGEST, *extra])
            raise SystemExit(0)
        return exit.exception.code, events

    def lane(self, digest):
        return {"status": "ok", "lambda_digest": digest, "commits": ["9" * 40]}

    def test_without_a_lane_digest_the_release_is_todays(self):
        code, events = self.run_prod([{"status": "deployed", "digest": self.DIGEST}])
        self.assertEqual((code, events), (0, [("invoke", {"commit": "a" * 40, "digest": self.DIGEST})]))

    def test_a_promotion_carries_the_sets_lane_image(self):
        code, events = self.run_prod([self.lane(self.RUNNING),
                                      {"status": "deployed", "digest": self.DIGEST, "lambda_digest": self.STAGED}],
                                     extra=["--lambda-digest", self.STAGED])
        self.assertEqual(code, 0)
        self.assertEqual(events, [("invoke", {"action": "lane"}), ("select", self.RUNNING, self.STAGED),
                                  ("invoke", {"commit": "a" * 40, "digest": self.DIGEST, "lambda_digest": self.STAGED})])

    def test_a_rollback_carries_the_lane_image_its_record_names(self):
        code, events = self.run_prod([self.lane(self.STAGED),
                                      {"status": "deployed", "digest": self.DIGEST, "lambda_digest": self.EARLIER}],
                                     extra=["--force", "--lambda-digest", self.EARLIER])
        self.assertEqual(code, 0)
        self.assertEqual(events[1], ("select", self.STAGED, self.EARLIER))
        self.assertEqual(events[2][1], {"commit": "a" * 40, "digest": self.DIGEST, "force": True, "lambda_digest": self.EARLIER})

    def test_a_held_or_failed_prod_release_puts_the_selection_back(self):
        for result, code in (({"status": "held", "reason": "viewers"}, publish.HELD),
                             ({"status": "failed", "step": "rollout"}, 1)):
            with self.subTest(status=result["status"]):
                got, events = self.run_prod([self.lane(self.RUNNING), result, self.lane(self.RUNNING)],
                                            extra=["--lambda-digest", self.STAGED])
                self.assertEqual(got, code)
                self.assertEqual(events[-1], ("restore", "prod", self.BEFORE))

    def test_a_rollback_to_a_set_without_a_lane_image_removes_it(self):
        # The set returned to ran no lane image: the function removes FLEET_LAMBDA_IMAGE; no tag moves.
        code, events = self.run_prod([{"status": "deployed", "digest": self.DIGEST, "lambda_digest": None}],
                                     extra=["--force", "--lambda-digest", "none"])
        self.assertEqual(code, 0)
        self.assertEqual(events, [("invoke", {"commit": "a" * 40, "digest": self.DIGEST, "force": True, "lambda_digest": None})])

    def test_a_none_release_must_report_the_lane_image_removed(self):
        code, events = self.run_prod([{"status": "deployed", "digest": self.DIGEST}], extra=["--force", "--lambda-digest", "none"])
        self.assertEqual(code, 1)

    def test_a_malformed_lane_digest_is_refused_before_anything(self):
        code, events = self.run_prod([], extra=["--force", "--lambda-digest", "latest"])
        self.assertEqual((code, events), (1, []))
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            publish.arguments(["--lambda-digest", self.STAGED])  # staging takes none

    def test_a_production_release_naming_the_staging_repository_is_refused(self):
        # Before any call: a prod release that moves or names a lane image uses prod's repository only.
        for lane in (self.STAGED, "none"):
            for repository in ("111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile", "registry.example/other"):
                with self.subTest(lane=lane, repository=repository):
                    code, events = self.run_prod([], extra=["--force", "--lambda-digest", lane], repository=repository)
                    self.assertEqual((code, events), (1, []))

    def test_each_stack_takes_only_its_own_lane_repository(self):
        for stack, url, other in (("stage", "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile", "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile-prod"), ("prod", "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile-prod", "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile")):
            with self.subTest(stack=stack):
                with patch.dict(os.environ, {"LAMBDA_TILE_REPO": url}):
                    _, ecr = publish.lane_repository(stack)
                self.assertEqual((ecr.repository, ecr.registry), (publish.lane_release().REPOSITORIES[stack], "111122223333"))
                for wrong in (other, ""):
                    with patch.dict(os.environ, {"LAMBDA_TILE_REPO": wrong}), self.assertRaisesRegex(ValueError, stack):
                        publish.lane_repository(stack)

    def test_prod_reads_stagings_selection_from_stagings_repository_on_its_own_credentials(self):
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile-prod"}):
            _, source = publish.staging_lane()
        self.assertEqual((source.repository, source.registry), ("fcvm-fleet/lambda-tile", "111122223333"))
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile"}), self.assertRaises(ValueError):
            publish.staging_lane()

    def test_staging_code_paths_never_name_the_production_repository(self):
        import inspect
        for function in (publish.main, publish.order, publish.select_lane):
            source = inspect.getsource(function)
            with self.subTest(function=function.__name__):
                self.assertNotIn('"prod"', source)
                self.assertNotIn("lambda-tile-prod", source)
                self.assertNotIn("staging_lane", source)
        self.assertIn('lane_repository("stage")', inspect.getsource(publish.select_lane))
        workflows = HERE.parent.parent / ".github/workflows"
        for name in ("fleet-lambda-image.yml", "fleet-coordinator-deploy.yml"):
            text = (workflows / name).read_text()
            with self.subTest(workflow=name):
                self.assertNotIn("lambda-tile-prod", text)
                self.assertNotIn("fleet-prod-deploy", text)
                self.assertNotIn("release.py copy", text)

    def test_the_prod_selection_takes_the_named_digest(self):
        calls = []
        lane = publish.lane_release()
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": "111122223333.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile-prod"}), \
                patch.object(lane, "snapshot", lambda ecr, stack: calls.append(("snapshot", ecr.repository, stack)) or dict(self.BEFORE)), \
                patch.object(lane, "select", lambda ecr, stack, new, running: calls.append((stack, new, running)) or {}):
            self.assertEqual(publish.select_prod_lane(self.STAGED, self.EARLIER), (self.EARLIER, self.BEFORE))
        self.assertEqual(calls, [("snapshot", "fcvm-fleet/lambda-tile-prod", "prod"), ("prod", self.EARLIER, self.STAGED)])


class ProdLaneWorkflowTests(unittest.TestCase):
    """The prod workflows carry the set's lane image: decide reads staging's selection with the prod lane on, a
    promotion releases the target's, and both rollbacks return to the one the earlier set names."""

    WORKFLOWS = HERE.parent.parent / ".github/workflows"
    FLAG = '${lane:+--lambda-digest "$lane"}'

    def step(self, text, name, after=0):
        start = text.index(f"      - name: {name}\n", after)
        return text[start:text.find("\n      - ", start + 10)], start

    def test_a_promotion_decides_and_releases_the_lane_image(self):
        promote = (self.WORKFLOWS / "fleet-prod-promote.yml").read_text()
        candidate = promote.split("\n  candidate:\n", 1)[1].split("\n  hold:\n", 1)[0]
        self.assertIn("      lane: ${{ steps.decide.outputs.lane }}\n", candidate)
        self.assertIn("          sparse-checkout: |\n            fleet/coord\n            fleet/lambda\n", candidate)
        self.assertIn("FLEET_LAMBDA_LANE_PROD: ${{ vars.FLEET_LAMBDA_LANE_PROD }}", candidate)
        self.assertIn("LAMBDA_TILE_REPO: ${{ secrets.LAMBDA_TILE_REPO }}", candidate)
        release = promote.split("\n  release:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        coordinator, _ = self.step(release, "Coordinator")
        self.assertIn("(needs.candidate.outputs.coordinator == 'true' || needs.candidate.outputs.lane == 'true')", coordinator)
        self.assertIn("lane=$(jq -r '.lambda_digest // empty' <<<\"$TARGET\")", coordinator)
        self.assertIn(self.FLAG, coordinator)
        self.assertIn("      prior_lane: ${{ steps.decide.outputs.prior_lane }}\n", candidate)
        rollback = promote.split("\n  rollback:\n", 1)[1]
        coordinator, _ = self.step(rollback, "Coordinator")
        self.assertIn("needs.candidate.outputs.lane == 'true'", coordinator)
        # Leaving a set with a lane image: return to the one prod ran before (from its task), or to none.
        self.assertIn("PRIOR_LANE: ${{ needs.candidate.outputs.prior_lane }}", coordinator)
        self.assertIn('if [ -n "$(jq -r \'.lambda_digest // empty\' <<<"$TARGET")" ]; then lane="${PRIOR_LANE:-none}"; fi', coordinator)
        self.assertIn(self.FLAG, coordinator)

    def test_a_promotion_copies_the_tested_lane_image_before_anything_prod_runs_moves(self):
        # Its own step, right after the credentials and before the host and tile images move: a failed copy changes
        # nothing prod runs. The Coordinator step then releases the same digest; neither rollback copies.
        promote = (self.WORKFLOWS / "fleet-prod-promote.yml").read_text()
        release = promote.split("\n  release:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        names = re.findall(r"^      - (?:name: (.+)|uses: ([^@ ]+))", release, re.M)
        order = [n or u for n, u in names]
        self.assertLess(order.index("aws-actions/configure-aws-credentials"), order.index("Copy the lane image into prod's repository"))
        self.assertLess(order.index("Copy the lane image into prod's repository"), order.index("Host and tile images"))
        step, _ = self.step(release, "Copy the lane image into prod's repository")
        self.assertIn("LAMBDA_TILE_REPO: ${{ secrets.LAMBDA_TILE_REPO }}", step)
        script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        lane = "sha256:" + "2" * 64
        def run(target, skopeo=True):
            bin_ = tempfile.mkdtemp()
            self.addCleanup(lambda: __import__("shutil").rmtree(bin_))
            # The script sees jq and, when asked for, a skopeo: nothing else on PATH.
            os.symlink(__import__("shutil").which("jq"), Path(bin_, "jq"))
            if skopeo:
                Path(bin_, "skopeo").write_text("#!/bin/sh\n")
                os.chmod(Path(bin_, "skopeo"), 0o755)
            stub = 'python3() { echo "python3 $*"; }\nsudo() { echo "sudo $*"; }\n'
            return subprocess.run(["/bin/bash", "-c", stub + script], capture_output=True, text=True,
                                  env={"PATH": bin_, "TARGET": json.dumps(target)})
        with_lane = {"coordinator": {"commit": "a" * 40, "digest": "sha256:" + "a" * 64}, "lambda_digest": lane}
        out = run(with_lane)
        self.assertEqual((out.returncode, out.stdout.splitlines()), (0, [f"python3 -B fcvm/fleet/lambda/release.py copy --digest {lane}"]))
        out = run({"coordinator": with_lane["coordinator"]})
        self.assertEqual((out.returncode, out.stdout), (0, ""))
        # Without skopeo on the runner it is installed, and a runner that still lacks it fails before the copy.
        out = run(with_lane, skopeo=False)
        self.assertIn("sudo apt-get install -y skopeo", out.stdout)
        self.assertNotIn("release.py copy", out.stdout)
        self.assertNotEqual(out.returncode, 0)
        coordinator, _ = self.step(release, "Coordinator")
        self.assertNotIn("release.py copy", coordinator)
        self.assertIn(self.FLAG, coordinator)
        rollbacks = promote.split("\n  rollback:\n", 1)[1] + (self.WORKFLOWS / "fleet-prod-rollback.yml").read_text()
        self.assertNotIn("release.py copy", rollbacks)

    def run_coordinator_step(self, lane):
        """The manual rollback's Coordinator step, run by bash with python3 replaced by a stub that prints its
        arguments one per line."""
        rollback = (self.WORKFLOWS / "fleet-prod-rollback.yml").read_text()
        coordinator, _ = self.step(rollback, "Coordinator")
        script = textwrap.dedent(coordinator.split("        run: |\n", 1)[1])
        previous = json.dumps({"coordinator": {"commit": "a" * 40, "digest": "sha256:" + "a" * 64}})
        env = {"PATH": os.environ["PATH"], "PREVIOUS": previous, "LANE": lane}
        return subprocess.run(["bash", "-c", 'python3() { printf "%s\\n" "$@"; }\n' + script], env=env,
                              capture_output=True, text=True, check=True).stdout.splitlines()

    def test_a_manual_rollback_passes_the_lane_the_current_step_chose(self):
        today = ["-B", "fleet/coord/publish.py", "--stack", "prod", "--force",
                 "--commit", "a" * 40, "--digest", "sha256:" + "a" * 64]
        self.assertEqual(self.run_coordinator_step(""), today)
        self.assertEqual(self.run_coordinator_step("none"), today + ["--lambda-digest", "none"])
        lane = "sha256:" + "2" * 64
        self.assertEqual(self.run_coordinator_step(lane), today + ["--lambda-digest", lane])
        rollback = (self.WORKFLOWS / "fleet-prod-rollback.yml").read_text()
        coordinator, _ = self.step(rollback, "Coordinator")
        self.assertIn("LANE: ${{ steps.current.outputs.lane }}", coordinator)
        self.assertIn("LAMBDA_TILE_REPO: ${{ secrets.LAMBDA_TILE_REPO }}", coordinator)
        for step in ("Worker", "Coordinator", "Host and tile images"):
            body, _ = self.step(rollback, step)
            self.assertIn("steps.current.outcome == 'success'", body)


class ManualRollbackLaneTests(unittest.TestCase):
    """prod_release.py current, the manual rollback's first step, chooses its lane flag from both recorded
    entries and refuses when prod runs another lane image than its record names."""

    S, R = "sha256:" + "5" * 64, "sha256:" + "2" * 64

    C1, C0 = "sha256:" + "a" * 64, "sha256:" + "c" * 64

    def entry(self, generation, lane=None, coordinator=None):
        return {"coordinator": {"commit": "a" * 40, "digest": coordinator or self.C1}, "web_commit": None,
                "worker_version": None, "generation": generation, "build_id": "b", "smoke_run": None,
                **({"lambda_digest": lane} if lane else {})}

    def current(self, release, runs=None, lane_error=None, switch=None, coordinator=None):
        """Run `current` against a candidate answer whose prod runs `runs` (None: the function names nothing, as
        before ejc3/aws#1002; "": no lane image). Returns the exit code, the outputs and what was printed."""
        out = tempfile.NamedTemporaryFile("r", delete=False)
        self.addCleanup(os.unlink, out.name)
        prod_side = {"digest": coordinator or self.C1, "commits": ["a" * 40]}
        if runs is not None:
            prod_side["lambda_digest"] = runs or None
        if lane_error:
            prod_side.update({"lambda_digest": None, "lane_error": lane_error})
        answer = {"status": "ok", "stage": {"digest": "sha256:" + "b" * 64, "commits": []}, "prod": prod_side,
                  "release": release}
        env = {"COORDINATOR_RELEASE": "fcvm-coordinator-release-prod", "GITHUB_OUTPUT": out.name}
        printed = io.StringIO()
        with patch.dict(os.environ, env), patch.object(prod.publish, "invoke", lambda f, p: copy.deepcopy(answer)), \
                contextlib.redirect_stdout(printed):
            os.environ.pop("FLEET_LAMBDA_LANE_PROD", None)
            if switch:
                os.environ["FLEET_LAMBDA_LANE_PROD"] = switch
            code = prod.main(["current"])
        outputs = dict(line.split("=", 1) for line in Path(out.name).read_text().splitlines())
        return code, outputs, printed.getvalue()

    def test_with_no_lane_image_anywhere_it_is_todays(self):
        release = {**self.entry(1), "previous": self.entry(0)}
        for runs in (None, ""):
            for switch in (None, "on"):
                with self.subTest(runs=runs, switch=switch):
                    code, outputs, _ = self.current(release, runs, switch=switch)
                    self.assertEqual(code, 0)
                    self.assertEqual(json.loads(outputs["previous"]), self.entry(0))
                    self.assertEqual(json.loads(outputs["current"]), self.entry(1))
                    self.assertEqual(outputs["lane"], "")

    def test_the_lane_flag_is_chosen_from_both_entries(self):
        # PREVIOUS names a lane image: pass it. Else CURRENT names one: none. Else no flag.
        cases = [(self.S, self.R, self.R), (self.S, None, "none"), (None, self.S, self.S), (self.S, self.S, self.S)]
        for left, back, flag in cases:
            with self.subTest(left=left, back=back):
                release = {**self.entry(1, left), "previous": self.entry(0, back)}
                code, outputs, _ = self.current(release, left or "")
                self.assertEqual(code, 0)
                self.assertEqual(outputs["lane"], flag)

    def test_a_rollback_of_a_rollback_returns_to_the_set_it_left(self):
        # The record job writes the restored set with the rolled-back one as its previous (record(PREVIOUS, ...,
        # CURRENT)); rolling back again must return to the lane image the first rollback left.
        for back in (self.R, None):
            with self.subTest(back=back):
                release = {**self.entry(1, self.S), "previous": self.entry(0, back)}
                code, outputs, _ = self.current(release, self.S)
                self.assertEqual((code, outputs["lane"]), (0, back or "none"))
                written = {}
                def record(function, payload):
                    written.update(payload["release"])
                    return {"status": "recorded"}
                with patch.object(prod.publish, "invoke", record), contextlib.redirect_stdout(io.StringIO()), \
                        patch.dict(os.environ, {"COORDINATOR_RELEASE": "fcvm-coordinator-release-prod"}):
                    prod.record(json.loads(outputs["previous"]), None, json.loads(outputs["current"]))
                code, outputs, _ = self.current(written, back or "")
                self.assertEqual((code, outputs["lane"]), (0, self.S))
                self.assertEqual(json.loads(outputs["previous"]), self.entry(1, self.S))

    def test_prod_running_another_lane_image_than_its_record_names_refuses(self):
        # What prod runs, not only the record: a rollback whose record step failed, or a switch turned off while
        # prod ran a lane image, leaves the record describing something else.
        cases = [(self.S, self.R), (self.S, "")]
        for recorded, runs in cases:
            with self.subTest(recorded=recorded, runs=runs):
                release = {**self.entry(1, recorded), "previous": self.entry(0, coordinator=self.C0)}
                code, outputs, printed = self.current(release, runs)
                self.assertEqual(code, 1)
                self.assertNotIn("previous", outputs)
                self.assertIn("prod runs another lane image than its release record names", printed)

    def test_with_no_lane_image_recorded_the_lane_read_is_not_consulted(self):
        # Main's emergency path: a flagless release leaves FLEET_LAMBDA_IMAGE alone, so a lane read that failed (a
        # deployment not COMPLETED, a pointer mismatch, an ECS error) or a lane image nobody recorded stops nothing.
        release = {**self.entry(1), "previous": self.entry(0)}
        for kwargs in ({"lane_error": "not-idle"}, {"lane_error": "unreadable"}, {"runs": self.S}):
            with self.subTest(**kwargs):
                code, outputs, _ = self.current(release, **kwargs)
                self.assertEqual((code, outputs["lane"]), (0, ""))

    def test_a_resumed_rollback_of_a_lane_only_set_is_accepted(self):
        # Both entries name one coordinator image (a promotion that changed only the lane image), so the
        # coordinator digest cannot tell the sets apart: prod running E0's lane image is the resumed state.
        for back in (self.R, None):
            with self.subTest(back=back):
                release = {**self.entry(1, self.S), "previous": self.entry(0, back)}
                code, outputs, _ = self.current(release, back or "")
                self.assertEqual((code, outputs["lane"]), (0, back or "none"))

    def test_a_lane_image_prod_cannot_read_refuses_when_an_entry_names_one(self):
        for left, back in ((self.S, None), (None, self.S), (self.S, self.R)):
            with self.subTest(left=left, back=back):
                release = {**self.entry(1, left), "previous": self.entry(0, back)}
                code, outputs, printed = self.current(release, lane_error="not-idle")
                self.assertEqual(code, 1)
                self.assertNotIn("previous", outputs)
                self.assertIn("could not read the lane image prod runs (not-idle)", printed)

    def test_a_rollback_whose_later_step_failed_is_resumed(self):
        # The Coordinator step applied the previous set, then a later step failed, so the record job did not run:
        # prod runs E0's coordinator and lane image while the record still says E1. A rerun must go on.
        for back in (self.R, None):
            with self.subTest(back=back):
                release = {**self.entry(1, self.S), "previous": self.entry(0, back, coordinator=self.C0)}
                code, outputs, _ = self.current(release, back or "", coordinator=self.C0)
                self.assertEqual((code, outputs["lane"]), (0, back or "none"))
        # Only E0's coordinator with E0's lane image is that state.
        release = {**self.entry(1, self.S), "previous": self.entry(0, self.R, coordinator=self.C0)}
        for runs, coordinator in ((self.R, self.C1), ("", self.C0)):
            with self.subTest(runs=runs, coordinator=coordinator):
                code, _, printed = self.current(release, runs, coordinator=coordinator)
                self.assertEqual(code, 1)
                self.assertIn("prod runs another lane image than its release record names", printed)


class FakeLive:
    """A coordinator /live socket that answers from a script."""

    def __init__(self, hello, admission, power, receipt, wakes=False):
        self.out = [{"type": "hello", **hello}, {"type": "admission", **admission},
                    {"type": "snapshot", "state": {"fleet": {"power": {"state": power}}}}]
        self.receipt, self.wakes, self.sent = receipt, wakes, []

    def __call__(self, url, headers=None, opener=None):
        self.url, self.headers = url, headers
        return self

    def send(self, payload, opcode=1):
        self.sent.append(payload)
        if payload.get("type") == "intent":
            self.out.append({"type": "receipt", "id": payload["id"], **self.receipt})
            if self.wakes:
                self.out.append({"type": "snapshot", "state": {"fleet": {"power": {"state": "waking"}}}})

    def message(self, deadline):
        if not self.out:
            raise TimeoutError("no message")
        return 1, json.dumps(self.out.pop(0)).encode()

    def close(self):
        pass


class UnverifiedProbeTests(unittest.TestCase):
    ENV = {"PROD_PUBLIC_URL": "https://fleet-api.example", "PROD_ORIGIN": "https://site.example"}
    VERIFY = {"verify": True, "seat": None, "direct": None, "locked": True}
    REFUSED = {"outcome": "refused", "reason": "verify first", "version": 0}

    def probe(self, live):
        with patch.dict(os.environ, self.ENV), patch.object(smoke, "Socket", live), contextlib.redirect_stdout(io.StringIO()) as out:
            smoke.unverified_probe("v4", watch_seconds=0.2)
        return out.getvalue()

    def test_an_unverified_visitor_is_asked_to_verify_refused_and_wakes_nothing(self):
        live = FakeLive({"turnstile": "site-key"}, self.VERIFY, "asleep", self.REFUSED)
        line = json.loads(self.probe(live).splitlines()[-1])
        self.assertEqual(line, {"unverified": True, "family": "v4", "seated": False, "wakeRefused": True, "power": "asleep"})
        self.assertEqual(live.url, "wss://fleet-api.example/live")
        self.assertEqual(live.headers, {"Origin": "https://site.example"})
        self.assertNotIn("CF-Access-Client-Id", json.dumps(live.headers))

    def test_each_way_an_unverified_visitor_gets_in_fails(self):
        cases = {"no turnstile asked": FakeLive({"turnstile": None}, self.VERIFY, "asleep", self.REFUSED),
                 "seated": FakeLive({"turnstile": "k"}, {**self.VERIFY, "seat": "ticket"}, "asleep", self.REFUSED),
                 "direct token": FakeLive({"turnstile": "k"}, {**self.VERIFY, "direct": {"value": "v"}}, "asleep", self.REFUSED),
                 "wake taken": FakeLive({"turnstile": "k"}, self.VERIFY, "asleep", {"outcome": "committed", "version": 9}),
                 "fleet woke": FakeLive({"turnstile": "k"}, self.VERIFY, "asleep", self.REFUSED, wakes=True)}
        for name, live in cases.items():
            with self.subTest(case=name), self.assertRaises(AssertionError):
                self.probe(live)


class BroadcastProbeTests(unittest.TestCase):
    def test_the_playlist_a_stream_and_a_segment_load_over_the_family(self):
        bodies = {"https://play.example/m.m3u8?token=t": (200, b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv/720.m3u8?x=1\n"),
                  "https://play.example/v/720.m3u8?x=1": (200, b"#EXTM3U\n#EXTINF:2,\ns1.ts\n#EXTINF:2,\ns2.ts\n"),
                  "https://play.example/v/s2.ts": (200, b"\x47" * 188)}
        seen = []
        def get(url, family):
            seen.append((url, family))
            return bodies[url]
        with patch.object(smoke, "https_get", get):
            smoke.prove_broadcast("https://play.example/m.m3u8?token=t", "v6")
        self.assertEqual([u for u, _ in seen], list(bodies))
        self.assertEqual({f for _, f in seen}, {"v6"})
        self.assertEqual(sum(u.endswith("token=t") for u, _ in seen), 1, "the single-use token is fetched once")

    def test_a_playlist_with_no_segment_or_a_failed_segment_fails(self):
        for bodies in ({"p": (200, b"#EXTM3U\n#EXT-X-STREAM-INF:B=1\nv.m3u8\n"), "v.m3u8": (200, b"#EXTM3U\n")},
                       {"p": (200, b"#EXTM3U\n#EXTINF:2,\ns.ts\n"), "s.ts": (403, b"<html>forbidden</html>")},
                       {"p": (200, b"<html>")}):
            with self.subTest(bodies=list(bodies)), \
                    patch.object(smoke, "https_get", lambda url, family: bodies[url.rsplit("/", 1)[-1]]), \
                    self.assertRaises(AssertionError):
                smoke.prove_broadcast("https://play.example/p", "v4")


class ProdWorkflowTests(unittest.TestCase):
    """The prod workflows: triggers, what may run where, and the security rules (CLAUDE.md)."""

    WORKFLOWS = HERE.parent.parent / ".github" / "workflows"

    def text(self, name):
        return (self.WORKFLOWS / name).read_text()

    def jobs(self, name):
        body = self.text(name).split("\njobs:\n", 1)[1]
        return dict((m.group(1), m.group(2)) for m in re.finditer(r"(?ms)^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)", body))

    @staticmethod
    def run_blocks(body):
        """Every `run:` value of a job: the line, or a block scalar's indented lines."""
        lines, blocks = body.split("\n"), []
        for i, line in enumerate(lines):
            m = re.match(r"( +)(- )?run: ?(.*)$", line)
            # A step's run (job outputs and `defaults: run:` sit at six spaces).
            if not m or (len(m.group(1)) < 8 and not m.group(2)):
                continue
            m = re.match(r"( +(?:- )?)run: ?(.*)$", line)
            if m.group(2).strip() not in ("|", ">", "|-", ">-"):
                blocks.append(m.group(2))
                continue
            indent, block = len(m.group(1)), []
            for follow in lines[i + 1:]:
                if follow.strip() and len(follow) - len(follow.lstrip()) <= indent:
                    break
                block.append(follow)
            blocks.append("\n".join(block))
        return blocks

    def test_promotion_follows_both_staging_deploys_by_name_and_the_gates_verdict(self):
        promote = self.text("fleet-prod-promote.yml")
        names = re.search(r"workflows: \[(.*)\]", promote).group(1)
        triggers = [n.strip().strip('"') for n in names.split(",")]
        actual = [re.search(r"(?m)^name: (.+)$", self.text(f)).group(1)
                  for f in ("fleet-coordinator-deploy.yml", "fleet-web-deploy.yml")]
        self.assertEqual(triggers, actual)
        self.assertIn("  workflow_dispatch:", promote)
        # The gate is scheduled, so it dispatches the promotion when it verifies, and only while prod exists.
        gate = self.jobs("fleet-release-gate.yml")["promote"]
        self.assertIn("needs.decide.outputs.verdict == 'verified'", gate)
        self.assertIn("vars.FLEET_PROD_ENABLED == 'true'", gate)
        self.assertIn("gh workflow run fleet-prod-promote.yml --ref main", gate)

    def test_a_workflow_run_is_taken_only_from_this_repositorys_main(self):
        gate = self.jobs("fleet-prod-promote.yml")["enabled"]
        for clause in ("github.event.workflow_run.head_repository.full_name == github.repository",
                       "github.event.workflow_run.head_branch == 'main'",
                       "github.event.workflow_run.conclusion == 'success'",
                       "github.event.workflow_run.event != 'pull_request'"):
            self.assertIn(clause, gate)
        self.assertIn("vars.FLEET_PROD_ENABLED", gate)
        self.assertNotIn("environment:", gate, "the off check runs without the prod environment")

    def test_no_event_value_reaches_a_shell_and_every_credential_is_in_the_prod_environment(self):
        for name in ("fleet-prod-promote.yml", "fleet-prod-rollback.yml", "fleet-prod-smoke.yml"):
            for job, body in self.jobs(name).items():
                with self.subTest(workflow=name, job=job):
                    for run in self.run_blocks(body):
                        self.assertNotIn("${{ github.event", run)
                        self.assertNotRegex(run, r"\$\{\{ *(needs|steps|inputs|vars|secrets)\.")
                    if "secrets." in body:
                        self.assertIn("environment: fleet-prod-deploy", body)
                    self.assertNotIn("fleet-stage-deploy", body)
                    self.assertNotIn("cloudflare-deploy", body)

    def test_the_run_block_reader_sees_single_lines_and_blocks(self):
        body = "    steps:\n      - run: one ${{ x }}\n      - name: n\n        run: |\n          two\n          three\n        env:\n          A: b\n"
        self.assertEqual(self.run_blocks(body), ["one ${{ x }}", "          two\n          three"])
        self.assertGreater(sum(len(self.run_blocks(b)) for b in self.jobs("fleet-prod-promote.yml").values()), 8)

    def test_the_prod_role_never_pushes_and_the_staging_release_is_never_called(self):
        for name in ("fleet-prod-promote.yml", "fleet-prod-rollback.yml"):
            text = self.text(name)
            self.assertNotIn("docker push", text)
            self.assertNotRegex(text, r"publish\.py(?! --stack prod)")
            self.assertNotIn("RELEASE_GATE", text)

    def test_the_rollback_job_runs_on_a_failed_release_smoke_or_record_and_force_is_only_in_rollbacks(self):
        jobs = self.jobs("fleet-prod-promote.yml")
        self.assertIn("needs.release.result == 'failure'", jobs["rollback"])
        self.assertIn("needs.smoke.result != 'success'", jobs["rollback"])
        # A record that was not written would leave prod on a set its record does not describe.
        self.assertIn("needs.record.result != 'success'", jobs["rollback"])
        self.assertIn("needs: [candidate, release, smoke, record]", jobs["rollback"])
        self.assertIn("--force", jobs["rollback"])
        for job, body in jobs.items():
            if job != "rollback":
                self.assertNotIn("--force", body, job)
        self.assertIn("needs.smoke.result == 'success'", jobs["record"])

    def test_every_component_rollback_is_attempted_whatever_failed_before_it(self):
        steps = {"fleet-prod-promote.yml": self.jobs("fleet-prod-promote.yml")["rollback"],
                 "fleet-prod-rollback.yml": self.jobs("fleet-prod-rollback.yml")["rollback"]}
        for name, body in steps.items():
            for step in ("Worker", "Coordinator", "Host and tile images"):
                with self.subTest(workflow=name, step=step):
                    block = body.split(f"      - name: {step}\n", 1)[1].split("\n      - ", 1)[0]
                    self.assertRegex(block, r"(?m)^        if: \$\{\{ !cancelled\(\)", "a failed rollback step skips this one")
        final = steps["fleet-prod-promote.yml"].split("      - name: The promotion failed\n", 1)[1]
        self.assertIn("if: ${{ always() }}", final)

    def test_the_prod_smoke_is_both_families_with_the_unverified_probe_and_its_callers_pass_secrets(self):
        jobs = self.jobs("fleet-prod-smoke.yml")
        for job, family in (("ipv4", "v4"), ("ipv6", "v6")):
            self.assertIn(f"--target prod --unverified --family {family}", jobs[job])
            self.assertIn(f"--target prod --family {family}", jobs[job])
            self.assertIn("--broadcast", jobs[job])
            self.assertIn("vars.FLEET_PROD_ENABLED", jobs["ipv4"])
        self.assertIn("--single-family", jobs["ipv4"])
        for name in ("fleet-prod-promote.yml", "fleet-prod-rollback.yml"):
            callers = [b for b in self.jobs(name).values() if "fleet-prod-smoke.yml" in b]
            self.assertTrue(callers, name)
            for body in callers:
                self.assertRegex(body, r"(?m)^    secrets: inherit$")

    def test_the_test_files_the_coordinator_build_compiles_trigger_its_deploy(self):
        makefile = (HERE.parent.parent / "Makefile").read_text()
        target = makefile.split("\ntest-fleet-coordinator: cargo-target-link", 1)[1].split("\n\n", 1)[0]
        tests = re.findall(r"--test (\w+)", target)
        self.assertTrue(tests)
        patterns = re.findall(r"^      - '([^']+)'", self.text("fleet-coordinator-deploy.yml"), re.M)
        for test in tests:
            self.assertTrue(any(fnmatch.fnmatchcase(f"tests/{test}.rs", p) for p in patterns), test)


if __name__ == "__main__":
    unittest.main(buffer=True)
