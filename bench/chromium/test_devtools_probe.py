#!/usr/bin/env python3
"""The guest-loopback DevTools probe sends a request Chromium answers.

Chromium's DevTools HTTP server drops an HTTP/1.0 request without replying
("Cannot handle request with protocol: HTTP/1.0" in the browser's log), so both
forensics probes, reqbench's FailureProbe section `cdp_from_guest_loopback` and
corpus_mem's per-clone forensics, came back empty from a guest whose Chromium
was serving: an empty result that read as "DevTools is not answering". Seen in
every probe of a prewired run whose clone logs show Chromium up.

The fake below applies that rule (no reply to HTTP/1.0) and answers HTTP/1.1.
Red before DEVTOOLS_VERSION_REQUEST: the reqbench command printed nothing.

Run: python3 -m unittest test_devtools_probe -v
"""

import os
import re
import socket
import subprocess
import sys
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqbench  # noqa: E402


class DevtoolsLikeServer:
    """Answer /json/version on HTTP/1.1 and close HTTP/1.0 without a reply."""

    def __init__(self):
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(10)
        self.port = self.listener.getsockname()[1]
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self):
        try:
            conn, _ = self.listener.accept()
        except OSError:
            return
        with conn:
            conn.settimeout(5)
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                data += chunk
            if data.split(b"\r\n", 1)[0].endswith(b"HTTP/1.1"):
                body = b'{"Browser":"Chrome/1.0"}'
                conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n%s"
                             % (len(body), body))

    def close(self):
        self.listener.close()
        self.thread.join(timeout=10)


class GuestDevtoolsProbe(unittest.TestCase):
    def test_reqbench_probe_section_gets_an_answer(self):
        server = DevtoolsLikeServer()
        self.addCleanup(server.close)
        probe = object.__new__(reqbench.FailureProbe)
        probe.cdp_port = server.port
        sections = dict(probe.guest_active_sections())
        out = subprocess.run(["sh", "-c", sections["cdp_from_guest_loopback"]],
                             capture_output=True, text=True, timeout=10)
        self.assertIn("200 OK", out.stdout)
        self.assertIn("Chrome/1.0", out.stdout)

    def test_corpus_mem_probe_uses_the_same_request(self):
        source = open(os.path.join(HERE, "corpus_mem.py")).read()
        self.assertFalse("HTTP/1.0" in source, "corpus_mem.py still sends an HTTP/1.0 probe")
        self.assertRegex(source, re.escape("printf '{DEVTOOLS_VERSION_REQUEST.format(port=port)}'"))


class ChromiumProcessSection(unittest.TestCase):
    """The chromium_processes section lists Chromium, not the probe itself.

    The guest sections run as one `sh -c` whose args hold every section's
    name, and a whole-line /[c]hrom/ matched "chromium_processes" in them: a
    repro probe of a timed-out clone listed only the probe's own sh lines.
    Red before matching the comm column: the batch's sh line is listed."""

    def test_only_processes_named_chrom_are_listed(self):
        command = dict(reqbench.PROBE_GUEST_PASSIVE_SECTIONS)["chromium_processes"]
        ps_output = (
            "    PID    PPID STAT ELAPSED COMMAND         COMMAND\n"
            "    101       1 S        30 chromium        /usr/lib/chromium/chromium --headless=new\n"
            "   1256    1180 S         0 sh              sh -c printf '===fcvm-probe-section %s\\n' chromium_processes\n"
        )
        script = command.replace("ps -eo pid,ppid,stat,etimes,comm,args", "cat")
        out = subprocess.run(["sh", "-c", script], input=ps_output, capture_output=True,
                             text=True, timeout=10)
        lines = out.stdout.splitlines()
        self.assertEqual(len(lines), 2, out.stdout)
        self.assertIn("chromium --headless=new", lines[1])

if __name__ == "__main__":
    unittest.main()
