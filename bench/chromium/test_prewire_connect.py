#!/usr/bin/env python3
"""A prewired CDP connect waits for the clone's port the way resolve does.

With `--ws-url` the harness skips /json/list, so its WebSocket connect is the
first contact with a clone whose guest may not answer yet. pasta accepts on the
host and resets that connection until the restored guest is reachable. The
connect was tried once, so every prewired cdp rep of a measured run failed:
606 of 606 at the connect stage with ConnectionResetError, 58 ms after spawn,
while the noop arms of the same run passed 230 of 230.

The fake endpoint below resets its first connections (SO_LINGER 0 then close,
an RST like pasta's) and then speaks CDP. Red against cdpdrive.py before
connect_prewired: `ConnectionResetError: [Errno 104] Connection reset by peer`
in render["error"], ok False.

Run: python3 -m unittest test_prewire_connect -v
"""

import os
import socket
import struct
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import cdpdrive  # noqa: E402
from test_net_trace import NAV_EVENTS, FakeCdpServer, args_for, scripted  # noqa: E402


class ResettingCdpServer(FakeCdpServer):
    """Reset the first `resets` connections, then serve one CDP session.

    With `resets=None` every connection is reset, for the exhausted budget.
    """

    def __init__(self, script, resets):
        self.resets = resets
        self.reset_count = 0
        self.closing = False
        super().__init__(script)
        self.listener.listen(64)

    def _run(self):
        self.listener.settimeout(10)
        while self.resets is None or self.reset_count < self.resets:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            if self.closing:
                conn.close()
                return
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            conn.close()
            self.reset_count += 1
        super()._run()

    def close(self):
        self.closing = self.resets is None
        super().close()


class PrewiredConnectWaitsForThePort(unittest.TestCase):
    def test_resets_before_the_guest_answers_are_retried(self):
        server = ResettingCdpServer(scripted(NAV_EVENTS), resets=3)
        self.addCleanup(server.close)
        out = cdpdrive.drive(args_for(server, connect_retries=2000, resolve_poll_ms=5.0))
        self.assertTrue(out["ok"], out.get("error"))
        self.assertTrue(out["target_prewired"])
        self.assertEqual(server.reset_count, 3)
        self.assertEqual(out["connect_attempts"], 4)
        outcomes = [p["outcome"] for p in out["connect_polls"]]
        self.assertEqual(outcomes[-1], "open")
        self.assertEqual(len(outcomes), 4)
        self.assertTrue(all(o.startswith("os-") for o in outcomes[:3]), outcomes)
        # Three refused attempts and the sleeps between them: at least 10 ms.
        self.assertGreaterEqual(out["stages"]["connect_wait_ms"], 10.0)
        self.assertEqual(out["stages"]["resolve_ms"], 0.0)

    def test_a_port_that_never_answers_fails_within_the_attempt_budget(self):
        server = ResettingCdpServer(scripted(NAV_EVENTS), resets=None)
        self.addCleanup(server.close)
        out = cdpdrive.drive(args_for(server, connect_retries=5, resolve_poll_ms=5.0))
        self.assertFalse(out["ok"])
        self.assertEqual(out["stage"], "connect")
        self.assertEqual(out["failure_class"], "transport")
        self.assertEqual(out["connect_attempts"], 5)
        self.assertEqual(len(out["connect_polls"]), 5)


if __name__ == "__main__":
    unittest.main()
