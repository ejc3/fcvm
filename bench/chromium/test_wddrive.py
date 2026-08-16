#!/usr/bin/env python3
"""navigate() must not report a render that has not happened yet.

The WebKit arm exists because WebDriver's own navigate command is unreliable
(the lost-completion defect documented in navigate's docstring), so navigate()
establishes readiness itself. That makes navigate() the arm's only definition of
"the page is up", and a false ready there does not fail -- it silently moves
layout time out of navigate_ms and into screenshot_ms, or freezes a golden whose
browser never rendered the fixture.

Both guards against that were added in response to measurements, and a
measurement is not a test: a number can be explained away, and it cannot be
re-run against a later regression. These drive the two guards through a real
HTTP server speaking the same protocol WebKit's driver does, so each one fails
if its guard is removed.

Run: python3 -m unittest test_wddrive -v
"""

import http.server
import json
import threading
import unittest

import wddrive


class FakeDriver(http.server.BaseHTTPRequestHandler):
    """A WebDriver endpoint whose script results are scripted by the test."""

    def log_message(self, *_args):
        pass  # keep the test output clean

    def _reply(self, value):
        body = json.dumps({"value": value}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        script = self.server.plan
        if self.path.endswith("/url"):
            script["navigated"] = True
            self._reply(None)
        else:  # execute/sync
            self._reply(script["results"].pop(0) if script["results"] else None)

    def do_GET(self):
        self._reply(self.server.plan["landed"])


class NavigateReadiness(unittest.TestCase):
    def drive(self, results, landed="http://page/x"):
        """Serve one navigate() against a scripted sequence of script results."""
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeDriver)
        # results[0] answers the sentinel plant; the rest answer the poll.
        server.plan = {"results": list(results), "landed": landed,
                       "navigated": False}
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        host = f"127.0.0.1:{server.server_port}"
        try:
            wddrive.navigate(host, "sess", "http://page/x", timeout=10.0,
                             poll_s=0.001)
        finally:
            self.remaining = len(server.plan["results"])
            self.navigated = server.plan["navigated"]

    def test_a_stale_complete_from_the_previous_document_is_not_ready(self):
        """readyState is "complete" from the OLD document when navigate lands.

        This is the false-ready that was caught in the field as navigate_ms=3.4
        beside screenshot_ms=1120 -- the 985 ms of layout had not started, let
        alone finished. The only thing separating the old document from the new
        one is the sentinel: a real document swap wipes the global, so
        `swapped` false means this "complete" belongs to the page we are
        navigating AWAY from.

        Remove the `swapped` term from navigate()'s readiness condition and this
        returns on the first poll with two results unconsumed.
        """
        self.drive([
            1,                    # sentinel planted on the old document
            ["complete", False],  # old document, still "complete" -- NOT ready
            ["loading", True],    # new document has begun
            ["complete", True],   # new document finished: ready
        ])
        self.assertEqual(self.remaining, 0,
                         "navigate() returned before the document swapped, "
                         "which is the false-ready the sentinel exists to catch")

    def test_an_error_page_is_a_failed_load_and_not_a_render(self):
        """WebKit's network-error page is a real document that reaches complete.

        It wipes the sentinel and satisfies every readiness term, so without
        comparing the landed URL a dead pageserver yields a successful render
        over a screenshot of "Unable to load page". Chromium's twin is guarded
        by render.py raising on nav["errorText"]; WebDriver classic exposes no
        such field, which is why this compares document.URL instead.
        """
        with self.assertRaises(wddrive.WdError) as caught:
            self.drive([1, ["complete", True]], landed="about:blank")
        self.assertIn("landed elsewhere", str(caught.exception))

    def test_a_fresh_session_navigates_without_a_previous_document(self):
        """No previous document means no stale "complete" to guard against.

        The sentinel plant is allowed to fail ONLY here, and navigate() must
        still proceed -- a fresh session is the first thing the warm gate does.
        """
        self.drive([
            wddrive.WdError("no such window (POST /session/sess/execute/sync)"),
            ["complete", True],
        ])
        self.assertTrue(self.navigated)

    def test_any_other_sentinel_failure_propagates(self):
        """Anything but the fresh-session case must NOT fall back silently.

        Treating a transient error as "no sentinel available" re-enables the
        stale-complete path for a session that does have a previous document,
        which is the same false-ready measured above wearing a different hat.
        """
        with self.assertRaises(wddrive.WdError):
            self.drive([wddrive.WdError("unknown error: driver went away")])
        self.assertFalse(self.navigated,
                         "a failed sentinel must stop before navigating")


def _install_error_replies():
    """Let a scripted result be an exception the fake driver raises as HTTP 500.

    Keeps the plan a flat list: a WdError in the sequence stands for "this round
    trip fails", which is how the two sentinel-failure cases above are written.
    """
    original = FakeDriver._reply

    def reply(self, value):
        if isinstance(value, wddrive.WdError):
            body = json.dumps({"value": {"error": "no such window",
                                         "message": str(value)}}).encode()
            if "unknown error" in str(value):
                body = json.dumps({"value": {"error": "unknown error",
                                             "message": str(value)}}).encode()
            self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        original(self, value)

    FakeDriver._reply = reply


_install_error_replies()


if __name__ == "__main__":
    unittest.main()
