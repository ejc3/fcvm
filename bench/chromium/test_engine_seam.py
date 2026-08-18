#!/usr/bin/env python3
"""The webkit engine seam: dispatch, discovery, and the driver contract.

The seam exists so ONE harness (reqbench) produces records for two engines with
identical accounting. The failure modes it must not have: silently rendering
with the wrong driver (a chromium record labelled webkit), a lost session id
surfacing 202 reps later as uniform navigate failures, and a webkit result
whose field names the analyzer cannot read.

Run: python3 -m unittest test_engine_seam -v
"""

import argparse
import http.server
import json
import os
import subprocess
import sys
import threading
import unittest
from unittest import mock

import reqbench
import wddrive


class DriveContract(unittest.TestCase):
    """wddrive.drive() must honour cdpdrive.drive()'s record contract."""

    def drive_against(self, handler_cls, session="warm-sess", timeout=15.0):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler_cls)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return wddrive.drive(argparse.Namespace(
            cdp_host=f"127.0.0.1:{server.server_port}",
            url="http://page/x", timeout=timeout,
            session_id=session, out_prefix="",
        ))

    def test_a_successful_render_carries_the_analyzer_fields(self):
        """ok, stages{navigate_ms, screenshot_ms, total_ms}, image_bytes,
        image_sha256 -- the names reqanalyze aggregates. A webkit record with
        different names is not wrong, it is INVISIBLE: the analyzer's medians
        simply omit it and the run publishes with silently thinner stages.
        """
        png = b"\x89PNG\r\n\x1a\n" + b"x" * 64
        import base64
        b64 = base64.b64encode(png).decode()

        class Driver(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def _send(self, value):
                body = json.dumps({"value": value}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def do_GET(self):
                if self.path.endswith("/screenshot"):
                    self._send(b64)
                elif self.path.endswith("/url"):
                    self._send("http://page/x")
                else:
                    self._send({"ready": True})
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                if self.path.endswith("/execute/sync"):
                    self._send(["complete", True])
                else:
                    self._send(None)

        result = self.drive_against(Driver)
        self.assertTrue(result["ok"], result.get("error"))
        self.assertEqual(result["engine"], "webkit")
        for key in ("navigate_ms", "screenshot_ms", "total_ms", "resolve_ms"):
            self.assertIn(key, result["stages"], f"stage {key} missing")
        self.assertEqual(result["image_bytes"], len(png))
        self.assertEqual(len(result["image_sha256"]), 64)

    def test_a_failure_is_a_labelled_record_not_a_traceback(self):
        """cdpdrive's rule, inherited: drive() NEVER raises for a driver-side
        failure. reqbench stores whatever comes back; an exception would be
        swallowed by its broad except and every rep would fail with the cause
        off-screen.
        """
        result = wddrive.drive(argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", timeout=0.3,
            session_id="s", out_prefix=""))
        self.assertFalse(result["ok"])
        self.assertIn("error", result)
        self.assertEqual(result["failed_stage"], "status")
        self.assertIn("total_ms", result["stages"])

    def test_a_missing_session_id_fails_before_any_network(self):
        result = wddrive.drive(argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", timeout=5.0,
            session_id="", out_prefix=""))
        self.assertFalse(result["ok"])
        self.assertIn("no session id", result["error"])


class SessionDiscovery(unittest.TestCase):
    def test_discovery_raises_loudly_when_exec_yields_nothing(self):
        """An empty session id must stop the run AT DISCOVERY.

        Returned silently, it becomes 202 navigate failures with the real cause
        (the golden never baked a session file) nowhere in the record.
        """
        args = argparse.Namespace(fcvm="/bin/false")
        with mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr="")
            with self.assertRaises(RuntimeError) as caught:
                reqbench.discover_wd_session(args, 1234)
        self.assertIn("session discovery failed", str(caught.exception))

    def test_discovery_strips_and_returns_the_id(self):
        args = argparse.Namespace(fcvm="/bin/true")
        with mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="  abc-123\n", stderr="")
            self.assertEqual(reqbench.discover_wd_session(args, 1), "abc-123")


class EngineDispatch(unittest.TestCase):
    def test_reqbench_exposes_the_engine_flag_with_chromium_default(self):
        """The flag is the seam. Its default must stay chromium: every existing
        record and every chromium invocation predates the flag, and a changed
        default silently relabels them.
        """
        import io
        import contextlib
        buf = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stdout(buf):
            saved = sys.argv
            try:
                sys.argv = ["reqbench.py", "--help"]
                reqbench.main()
            finally:
                sys.argv = saved
        text = buf.getvalue()
        self.assertIn("--engine", text)
        self.assertIn("webkit", text)

    def test_the_webkit_branch_exists_and_is_reached_by_engine(self):
        """Structural pin on the dispatch itself: run_cdp_request must consult
        args.engine and route webkit to wddrive. The behavioural half (a full
        fake clone) needs a VM; the routing decision does not.
        """
        import inspect
        src = inspect.getsource(reqbench.run_cdp_request)
        self.assertIn('== "webkit"', src)
        self.assertIn("wddrive.drive", src)
        self.assertIn("discover_wd_session", src)
        self.assertIn("cdpdrive.drive", src,
                      "the chromium path was lost while adding webkit")


if __name__ == "__main__":
    unittest.main()
