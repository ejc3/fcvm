#!/usr/bin/env python3
"""cdpdrive --net-trace: per-request Network.* rows for one navigation.

The measured arms send exactly the CDP messages they send today. The trace is
a diagnostic that adds `Network.enable` and a post-load drain, so it must be
opt-in, and the opt-out path must be provably identical on the wire. Both
sides are pinned here against a fake CDP endpoint that speaks real RFC 6455,
so render.py's event stash and the socket-timeout drain are the code under
test rather than a paraphrase of them. The argparse.Namespace shape follows
CdpDriveNavigationFailurePhases in test_reqbench.py.

Watched red 2026-08-28 against cdpdrive.py at 4d172153; the failure text is
quoted on each test.

Run: python3 -m unittest test_net_trace -v
"""

import argparse
import base64
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cdpdrive  # noqa: E402

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 16 + b"\xff\xd9"


class FakeCdpServer:
    """One-connection WebSocket server answering CDP commands from a script.

    `script(method, params)` returns `(result, events)`. The result goes back
    under the command's id; each event is then sent as a CDP notification, and
    a float in the list is a pause in seconds, which is how "ended after the
    load event" is staged. Every command method is recorded in `methods`, in
    the order it arrived, so a test can hold the wire sequence to an exact list.
    """

    def __init__(self, script):
        self.script = script
        self.methods = []
        self.messages = []  # (method, params) in wire order
        self._buf = b""
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.port = self.listener.getsockname()[1]
        self.ws_url = f"ws://127.0.0.1:{self.port}/devtools/page/TARGET"
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def close(self):
        if self.thread.is_alive():
            # A test that failed before connecting leaves the thread in accept();
            # a throwaway connection releases it instead of waiting out the timeout.
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
            except OSError:
                pass
        self.thread.join(timeout=10)
        self.listener.close()

    def _run(self):
        self.listener.settimeout(10)
        conn, _ = self.listener.accept()
        conn.settimeout(10)
        try:
            self._upgrade(conn)
            while True:
                text = self._recv_text(conn)
                if text is None:
                    return
                request = json.loads(text)
                self.methods.append(request["method"])
                self.messages.append((request["method"], request.get("params", {})))
                result, events = self.script(request["method"], request.get("params", {}))
                self._send_text(conn, json.dumps({"id": request["id"], "result": result}))
                for event in events:
                    if isinstance(event, float):
                        time.sleep(event)
                        continue
                    self._send_text(conn, json.dumps(event))
        except OSError:
            return
        finally:
            conn.close()

    def _exact(self, conn, n):
        while len(self._buf) < n:
            chunk = conn.recv(65536)
            if not chunk:
                return None
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _upgrade(self, conn):
        while b"\r\n\r\n" not in self._buf:
            chunk = conn.recv(4096)
            if not chunk:
                raise OSError("client closed during upgrade")
            self._buf += chunk
        head, _, self._buf = self._buf.partition(b"\r\n\r\n")
        key = ""
        for line in head.decode().split("\r\n"):
            name, _, value = line.partition(":")
            if name.strip().lower() == "sec-websocket-key":
                key = value.strip()
        accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
        conn.sendall(
            b"HTTP/1.1 101 Switching Protocols\r\n"
            b"Upgrade: websocket\r\nConnection: Upgrade\r\n"
            b"Sec-WebSocket-Accept: " + accept.encode() + b"\r\n\r\n"
        )

    def _recv_text(self, conn):
        while True:
            header = self._exact(conn, 2)
            if header is None:
                return None
            b1, b2 = header
            opcode, masked, n = b1 & 0x0F, b2 & 0x80, b2 & 0x7F
            if n == 126:
                (n,) = struct.unpack("!H", self._exact(conn, 2))
            elif n == 127:
                (n,) = struct.unpack("!Q", self._exact(conn, 8))
            mask = self._exact(conn, 4) if masked else b""
            payload = self._exact(conn, n)
            if payload is None:
                return None
            if masked:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if opcode == 0x8:
                return None
            if opcode == 0x1:
                return payload.decode()

    @staticmethod
    def _send_text(conn, text):
        payload = text.encode()
        n = len(payload)
        if n < 126:
            header = struct.pack("!BB", 0x81, n)
        elif n < 65536:
            header = struct.pack("!BBH", 0x81, 126, n)
        else:
            header = struct.pack("!BBQ", 0x81, 127, n)
        conn.sendall(header + payload)


def request_will_be_sent(rid, url, ts):
    return {"method": "Network.requestWillBeSent",
            "params": {"requestId": rid, "loaderId": "L1", "timestamp": ts,
                       "request": {"url": url, "method": "GET"}}}


def response_received(rid, ts, ip, port, protocol, status, dns=(-1, -1)):
    return {"method": "Network.responseReceived",
            "params": {"requestId": rid, "loaderId": "L1", "timestamp": ts,
                       "response": {"url": "", "status": status, "protocol": protocol,
                                    "remoteIPAddress": ip, "remotePort": port,
                                    "timing": {"requestTime": ts, "dnsStart": dns[0],
                                               "dnsEnd": dns[1]}}}}


def loading_finished(rid, ts):
    return {"method": "Network.loadingFinished",
            "params": {"requestId": rid, "timestamp": ts, "encodedDataLength": 1}}


def loading_failed(rid, ts, error, canceled=False):
    return {"method": "Network.loadingFailed",
            "params": {"requestId": rid, "timestamp": ts, "type": "Image",
                       "errorText": error, "canceled": canceled}}


def load_event_fired(ts):
    return {"method": "Page.loadEventFired", "params": {"timestamp": ts}}


# Five requests around one load event at t=1000.100 on the browser clock:
# r1, r2 finish before it; r3 fails before it; r4 is open at the load event
# and finishes 350 ms later, after the pause; r5 starts after the load event
# and never ends.
NAV_EVENTS = [
    request_will_be_sent("r1", "http://site.test/", 1000.000),
    request_will_be_sent("r2", "http://cdn.test/a.js", 1000.010),
    request_will_be_sent("r3", "http://tracker.test/pixel", 1000.020),
    request_will_be_sent("r4", "http://slow.test/font.woff", 1000.030),
    response_received("r1", 1000.050, "10.0.2.2", 80, "http/1.1", 200, dns=(1.0, 3.5)),
    loading_finished("r1", 1000.060),
    response_received("r2", 1000.070, "10.0.2.2", 80, "http/1.1", 200),
    loading_finished("r2", 1000.080),
    loading_failed("r3", 1000.090, "net::ERR_NAME_NOT_RESOLVED"),
    load_event_fired(1000.100),
    0.05,
    response_received("r4", 1000.400, "93.184.216.34", 443, "h2", 200),
    loading_finished("r4", 1000.450),
    request_will_be_sent("r5", "http://late.test/beacon", 1000.500),
]


NAV_ENTRY = {
    "domainLookupStart": 1.0, "domainLookupEnd": 2.0, "connectStart": 2.0,
    "connectEnd": 3.0, "requestStart": 3.0, "responseStart": 4.0,
    "responseEnd": 5.0, "loadEventEnd": 6.0,
}


def scripted(nav_events):
    def script(method, _params):
        if method == "Page.navigate":
            return {"frameId": "F", "loaderId": "L1"}, list(nav_events)
        if method == "Page.captureScreenshot":
            return {"data": base64.b64encode(JPEG).decode()}, []
        if method == "Runtime.evaluate":
            return {"result": {"type": "string", "value": json.dumps(NAV_ENTRY)}}, []
        return {}, []
    return script


def args_for(server, **overrides):
    """reqbench.py's closed cdpdrive Namespace, pointed at the fake endpoint.

    nav_timing=True, idle_wait_ms=0 and host_header="" are what the measured
    producer sets (reqbench.py, run_cdp_request), so the wire sequence pinned
    below is the measured arm's.
    """
    ns = argparse.Namespace(
        cdp_host="127.0.0.1:1", url="http://site.test/", format="jpeg", quality=80,
        timeout=5.0, idle_wait_ms=0.0, out_prefix="", ws_url=server.ws_url,
        connect_retries=1, nav_timing=True, print_target=False,
        host_header="", render_module=os.path.join(HERE, "render.py"),
    )
    for key, value in overrides.items():
        setattr(ns, key, value)
    return ns


class NetTraceRecorded(unittest.TestCase):
    """With --net-trace the file and the record carry the summary.

    Red: `KeyError: 'net_trace'` (the flag was ignored and no file was written).
    """

    def _drive(self, nav_events=NAV_EVENTS, drain_ms=500.0, **overrides):
        server = FakeCdpServer(scripted(nav_events))
        self.addCleanup(server.close)
        d = tempfile.mkdtemp(prefix="net-trace-")
        self.addCleanup(shutil.rmtree, d)
        path = os.path.join(d, "trace.json")
        out = cdpdrive.drive(args_for(server, net_trace=path, net_trace_drain_ms=drain_ms,
                                      **overrides))
        return out, path, server

    def test_summary_counts_pending_at_load_remote_ips_and_errors(self):
        out, path, server = self._drive()
        self.assertTrue(out["ok"], out)
        summary = out["net_trace"]
        self.assertEqual(summary["n_requests"], 5)
        self.assertEqual(summary["n_finished_before_load"], 2)
        self.assertEqual(summary["n_failed"], 1)
        self.assertEqual(summary["n_pending_at_load"], 1,
                         "r4 was the only request open at the load event; r5 "
                         "started after it and must not count")
        self.assertEqual(summary["remote_ips"], {"10.0.2.2": 2, "93.184.216.34": 1})
        self.assertEqual(summary["errors"], {"net::ERR_NAME_NOT_RESOLVED": 1})
        self.assertEqual(summary["slowest_10"][0],
                         {"url": "http://slow.test/font.woff", "end_ms": 450.0,
                          "duration_ms": 420.0})
        self.assertEqual(summary["pending_at_load"],
                         [{"url": "http://slow.test/font.woff", "start_ms": 30.0}])
        self.assertEqual(summary["load_event_ms"], 100.0)
        self.assertEqual(server.methods,
                         ["Page.enable", "Network.enable", "Page.navigate",
                          "Page.captureScreenshot", "Runtime.evaluate"])

    def test_file_rows_carry_per_request_fields_and_match_the_record(self):
        out, path, _ = self._drive()
        with open(path) as handle:
            trace = json.load(handle)
        self.assertEqual(sorted(trace), ["requests", "summary"])
        self.assertEqual(trace["summary"], out["net_trace"])
        rows = {row["url"]: row for row in trace["requests"]}
        self.assertEqual(len(rows), 5)
        r1 = rows["http://site.test/"]
        self.assertEqual((r1["start_ms"], r1["end_ms"]), (0.0, 60.0))
        self.assertEqual((r1["remote_ip"], r1["remote_port"], r1["protocol"], r1["status"]),
                         ("10.0.2.2", 80, "http/1.1", 200))
        self.assertTrue(r1["finished_before_load"])
        self.assertFalse(r1["failed"])
        self.assertEqual(r1["error_text"], "")
        self.assertEqual(r1["dns_ms"], 2.5)
        r3 = rows["http://tracker.test/pixel"]
        self.assertTrue(r3["failed"])
        self.assertFalse(r3["finished_before_load"])
        self.assertEqual(r3["error_text"], "net::ERR_NAME_NOT_RESOLVED")
        r4 = rows["http://slow.test/font.woff"]
        self.assertFalse(r4["finished_before_load"])
        self.assertTrue(r4["pending_at_load"])
        self.assertEqual(r4["end_ms"], 450.0)
        r5 = rows["http://late.test/beacon"]
        self.assertIsNone(r5["end_ms"])
        self.assertFalse(r5["pending_at_load"])

    def test_slowest_ranks_by_duration_and_names_the_rows_open_at_load(self):
        """slowest_10 was sorted by end_ms, completion relative to the first
        request, so a request that started late and finished fast outranked
        an earlier, longer one, and a request still open at the load event,
        the first thing a stall investigation reads, was not in the summary
        at all. Rank by duration, carry it per entry, and list the open rows.

        Red: `{'url': 'http://fast.test/late', 'end_ms': 350.0}` first, then
        `KeyError: 'pending_at_load'`.
        """
        events = [
            request_will_be_sent("r1", "http://slow.test/long", 1000.000),
            request_will_be_sent("r2", "http://hang.test/open", 1000.010),
            request_will_be_sent("r3", "http://fast.test/late", 1000.250),
            response_received("r1", 1000.290, "10.0.2.2", 80, "http/1.1", 200),
            loading_finished("r1", 1000.300),
            response_received("r3", 1000.340, "10.0.2.2", 80, "http/1.1", 200),
            loading_finished("r3", 1000.350),
            load_event_fired(1000.400),
        ]
        out, _, _ = self._drive(events, drain_ms=100.0)
        self.assertTrue(out["ok"], out)
        summary = out["net_trace"]
        self.assertEqual(summary["slowest_10"], [
            {"url": "http://slow.test/long", "end_ms": 300.0, "duration_ms": 300.0},
            {"url": "http://fast.test/late", "end_ms": 350.0, "duration_ms": 100.0},
        ])
        self.assertEqual(summary["pending_at_load"],
                         [{"url": "http://hang.test/open", "start_ms": 10.0}])
        self.assertEqual(summary["n_pending_at_load"], 1)

    def test_pending_at_load_lists_at_most_ten_rows_in_start_order(self):
        """Twelve requests open at the load event: the list names the ten
        that started first, and the count still says twelve.

        Red: `KeyError: 'pending_at_load'`.
        """
        events = [request_will_be_sent(f"r{i}", f"http://open.test/{i}", 1000.0 + i / 1000)
                  for i in range(12)]
        events.append(request_will_be_sent("done", "http://done.test/", 1000.020))
        events.append(loading_finished("done", 1000.030))
        trace = cdpdrive.reduce_net_trace(events, 1000.100, 100.0)
        summary = trace["summary"]
        self.assertEqual(summary["n_pending_at_load"], 12)
        self.assertEqual(summary["pending_at_load"],
                         [{"url": f"http://open.test/{i}", "start_ms": float(i)}
                          for i in range(10)])
        self.assertEqual(summary["slowest_10"],
                         [{"url": "http://done.test/", "end_ms": 30.0, "duration_ms": 10.0}])

    def test_the_drain_is_bounded_by_net_trace_drain_ms(self):
        out, _, _ = self._drive(drain_ms=200.0)
        self.assertTrue(out["ok"], out)
        drain = out["stages"]["net_trace_drain_ms"]
        self.assertGreaterEqual(drain, 200.0)
        self.assertLess(drain, 1500.0, f"drain ran {drain} ms against a 200 ms bound")

    def test_a_trace_is_still_written_when_the_load_event_never_fires(self):
        """The stall is the case the trace exists for, so it must survive it."""
        stalled = [
            request_will_be_sent("r1", "http://site.test/", 1000.000),
            request_will_be_sent("r2", "http://slow.test/font.woff", 1000.010),
            response_received("r1", 1000.050, "10.0.2.2", 80, "http/1.1", 200),
            loading_finished("r1", 1000.060),
        ]
        out, path, _ = self._drive(stalled, timeout=1.0)
        self.assertFalse(out["ok"])
        self.assertEqual(out["stage"], "navigate-load-event")
        self.assertTrue(os.path.exists(path), "no trace for the failure it exists to explain")
        summary = out["net_trace"]
        self.assertIsNone(summary["load_event_ms"])
        self.assertEqual(summary["n_requests"], 2)
        self.assertEqual(summary["n_finished_before_load"], 0)
        self.assertEqual(summary["n_pending_at_load"], 1)


class NetTraceWrite(unittest.TestCase):
    """The trace file is whole or absent, and a trace that was asked for and
    not written fails the CLI.

    Red: an empty trace.json left behind by an interrupted write
    (`open(path, "w")` truncates before json.dump runs), and exit 0 from a
    CLI whose --net-trace PATH does not exist afterwards.
    """

    def test_an_interrupted_write_leaves_no_partial_file(self):
        from unittest import mock
        server = FakeCdpServer(scripted(NAV_EVENTS))
        self.addCleanup(server.close)
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "trace.json")
            with mock.patch("json.dump", side_effect=OSError(28, "No space left on device")):
                out = cdpdrive.drive(args_for(server, net_trace=path, net_trace_drain_ms=100.0))
            self.assertTrue(out["ok"], out)
            self.assertIn("net_trace_error", out)
            self.assertIn("No space left", out["net_trace_error"])
            self.assertFalse(os.path.exists(path), "a partial trace file was left behind")
            self.assertEqual(os.listdir(d), [], "a temp file was left behind")

    def test_a_trace_that_could_not_be_written_fails_the_cli(self):
        server = FakeCdpServer(scripted(NAV_EVENTS))
        self.addCleanup(server.close)
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "no-such-dir", "trace.json")
            result = subprocess.run(
                [sys.executable, os.path.join(HERE, "cdpdrive.py"),
                 "127.0.0.1:1", "http://site.test/", "--ws-url", server.ws_url,
                 "--format", "jpeg", "--timeout", "5", "--connect-retries", "1",
                 "--net-trace", path, "--net-trace-drain-ms", "100"],
                capture_output=True, text=True, timeout=60)
            self.assertNotEqual(result.returncode, 0,
                                "exit 0 with no trace file\n" + result.stdout + result.stderr)
            record = json.loads(result.stdout.strip().splitlines()[-1])
            self.assertTrue(record["ok"], record)
            self.assertIn("net_trace_error", record)
            self.assertFalse(os.path.exists(path))


class NetTraceOff(unittest.TestCase):
    """Without the flag the wire is the measured arm's, message for message.

    The fake records (method, params) in wire order, and the trace-on run is
    held to exactly one extra message: Network.enable before Page.navigate.
    Verified red by fault injection (an unconditional Network.enable in
    cdpdrive.drive fails both trace-off tests and the one-extra-message
    assertion).
    """

    TODAY = ["Page.enable", "Page.navigate", "Page.captureScreenshot", "Runtime.evaluate"]

    def _drive(self, **overrides):
        server = FakeCdpServer(scripted(NAV_EVENTS))
        self.addCleanup(server.close)
        out = cdpdrive.drive(args_for(server, **overrides))
        server.close()
        return out, server

    def _assert_measured_wire(self, out, server):
        self.assertTrue(out["ok"], out)
        self.assertEqual([m for m, _ in server.messages], self.TODAY)
        self.assertEqual(server.messages[0], ("Page.enable", {}))
        self.assertEqual(server.messages[1], ("Page.navigate", {"url": "http://site.test/"}))
        self.assertEqual(server.messages[3][1].get("returnByValue"), True)
        self.assertNotIn("net_trace", out)
        self.assertNotIn("net_trace_error", out)
        self.assertNotIn("net_trace_drain_ms", out["stages"])
        self.assertIn("nav_timing_ms", out["stages"])

    def test_no_attribute_sends_no_network_enable(self):
        """reqbench.py builds a closed Namespace without net_trace."""
        out, server = self._drive()
        self._assert_measured_wire(out, server)

    def test_net_trace_none_sends_no_network_enable(self):
        """The CLI default is None, and None means off."""
        out, server = self._drive(net_trace=None, net_trace_drain_ms=5000.0)
        self._assert_measured_wire(out, server)

    def test_the_trace_adds_exactly_one_message(self):
        off_out, off_server = self._drive()
        with tempfile.TemporaryDirectory() as d:
            on_server = FakeCdpServer(scripted(NAV_EVENTS))
            self.addCleanup(on_server.close)
            on_out = cdpdrive.drive(args_for(
                on_server, net_trace=os.path.join(d, "trace.json"), net_trace_drain_ms=100.0))
            on_server.close()
        self.assertTrue(off_out["ok"] and on_out["ok"], (off_out, on_out))
        on = list(on_server.messages)
        self.assertEqual(on[1], ("Network.enable", {}))
        del on[1]
        self.assertEqual(on, off_server.messages,
                         "the trace changed more than the one Network.enable")


if __name__ == "__main__":
    unittest.main()
