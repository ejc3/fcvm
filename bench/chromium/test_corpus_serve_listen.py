#!/usr/bin/env python3
"""corpus_serve.py's listener measurements and the two listener options.

Issue #1159: the corpus throughput run's navigation time grew with the offered
rate while the replay's own per-request time (the access log's duration_ms,
which starts when a handler thread parses the request line) stayed flat. What
sits before that point is the kernel's accept queue (socketserver listens with
a backlog of 5), the accept thread, which for the TLS listener also runs the
handshake, and the handler thread's start. --listen-log records those, and
--backlog and --tls-handshake-in-handler change them.

Run: python3 -m unittest test_corpus_serve_listen -v
"""

import errno
import os
import socket
import ssl
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import corpus_serve  # noqa: E402
from test_corpus_serve_logs import ReplayFixture, read_jsonl  # noqa: E402


class TlsFixture(ReplayFixture):
    """A TLS ReplayServer over the one-file corpus, started on an ephemeral port."""

    def _tls(self, d, handshake_in_handler=False, log=None):
        from pathlib import Path

        exact, noquery, redirects = corpus_serve.load_indexes(Path(self._corpus(d)))

        class H(corpus_serve.Handler):
            pass

        H.exact, H.noquery, H.redirects = exact, noquery, redirects
        H.misses = []
        H.access_log = log
        server = corpus_serve.ReplayServer(("127.0.0.1", 0), H, name="tls",
                                           handshake_in_handler=handshake_in_handler)
        crt, key = corpus_serve.selfsigned(Path(d))
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(str(crt), str(key))
        server.socket = ctx.wrap_socket(server.socket, server_side=True,
                                        do_handshake_on_connect=not handshake_in_handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server

    def _https_get(self, port, timeout):
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as raw:
            with ctx.wrap_socket(raw, server_hostname="example.test") as tls:
                tls.sendall(b"GET /a.txt HTTP/1.0\r\nHost: example.test\r\n\r\n")
                data = b""
                while True:
                    chunk = tls.recv(4096)
                    if not chunk:
                        return data
                    data += chunk


class TlsHandshakePlacement(TlsFixture, unittest.TestCase):
    """A client that connects and never sends its hello.

    By default the handshake runs inside accept(), in the one thread that
    accepts, so that client holds every later connection until it gives up.
    With --tls-handshake-in-handler the handshake runs in the connection's own
    thread and the later connection is answered at once.
    """

    def _stall_then_get(self, handshake_in_handler):
        with tempfile.TemporaryDirectory() as d:
            server = self._tls(d, handshake_in_handler=handshake_in_handler)
            port = server.server_port
            stalled = socket.create_connection(("127.0.0.1", port))
            self.addCleanup(stalled.close)
            time.sleep(0.2)  # the accept thread has taken the stalled connection
            t0 = time.monotonic()
            try:
                body = self._https_get(port, timeout=1.5)
            except (socket.timeout, TimeoutError, ssl.SSLError):
                return None, time.monotonic() - t0
            return body, time.monotonic() - t0

    def test_by_default_a_stalled_hello_holds_up_the_next_connection(self):
        body, _ = self._stall_then_get(handshake_in_handler=False)
        self.assertIsNone(body, "the second connection was answered while the "
                                "accept thread was inside the first one's handshake")

    def test_in_the_handler_a_stalled_hello_holds_up_nobody(self):
        body, elapsed = self._stall_then_get(handshake_in_handler=True)
        self.assertIsNotNone(body, "the second connection was not answered "
                                   "while another client's hello was outstanding")
        self.assertIn(b"hello from the corpus", body)
        self.assertLess(elapsed, 1.0)

    def test_a_hello_that_never_comes_ends_the_handler_after_the_timeout(self):
        with tempfile.TemporaryDirectory() as d:
            server = self._tls(d, handshake_in_handler=True)
            server.HANDSHAKE_TIMEOUT_S = 0.3
            stalled = socket.create_connection(("127.0.0.1", server.server_port))
            self.addCleanup(stalled.close)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                row = server.stats.drain(1.0)
                if row and row["handshake_errors"]:
                    break
                time.sleep(0.05)
            else:
                self.fail("the handler never gave up on a client that sent no hello")
            self.assertEqual(row["handshakes"], 0)
            self.assertGreaterEqual(row["handshake_ms_max"], 250)


class Backlog(unittest.TestCase):
    """--backlog is the listen() queue length; socketserver's default is 5."""

    def _fill(self, backlog):
        """How many of 64 non-blocking connects complete against a listener
        nobody accepts from."""
        server = corpus_serve.ReplayServer(("127.0.0.1", 0), corpus_serve.Handler,
                                           backlog=backlog)
        self.addCleanup(server.server_close)
        clients = []
        for _ in range(64):
            c = socket.socket()
            c.setblocking(False)
            err = c.connect_ex(("127.0.0.1", server.server_port))
            self.assertIn(err, (0, errno.EINPROGRESS))
            clients.append(c)
            self.addCleanup(c.close)
        time.sleep(0.4)  # well inside the first SYN retransmit at 1 s
        done = 0
        for c in clients:
            if c.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR) == 0:
                try:
                    c.getpeername()
                    done += 1
                except OSError:
                    pass
        return done

    def test_the_default_queue_overflows_under_a_burst(self):
        self.assertLess(self._fill(None), 16)

    def test_a_larger_backlog_holds_the_whole_burst(self):
        self.assertEqual(self._fill(256), 64)

    def test_the_flag_parses_and_defaults_to_the_library_value(self):
        parser = corpus_serve.build_parser()
        self.assertIsNone(parser.parse_args([]).backlog)
        self.assertEqual(parser.parse_args(["--backlog", "1024"]).backlog, 1024)
        self.assertFalse(parser.parse_args([]).tls_handshake_in_handler)
        self.assertTrue(parser.parse_args(["--tls-handshake-in-handler"]).tls_handshake_in_handler)


class ListenLog(TlsFixture, unittest.TestCase):
    """--listen-log rows: what happened before the handler parsed the request."""

    def _rows(self, handshake_in_handler):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "corpus-listen.log")
            log = corpus_serve.JsonlLog(path)
            self.addCleanup(log.close)
            server = self._tls(d, handshake_in_handler=handshake_in_handler)
            sampler = corpus_serve.ListenSampler([server], log)
            for _ in range(5):
                self.assertIn(b"hello from the corpus", self._https_get(server.server_port, 5))
            corpus_serve.stop_listeners([server])
            sampler.sample()
            return read_jsonl(path)

    def test_accept_placement_charges_the_handshake_to_the_accept_thread(self):
        rows = self._rows(handshake_in_handler=False)
        tls = next(r for r in rows if r["listener"] == "tls")
        self.assertEqual(tls["accepts"], 5)
        self.assertEqual(tls["requests"], 5)
        self.assertEqual(tls["handshakes"], 0)
        self.assertGreater(tls["accept_ms_sum"], 0)
        self.assertEqual(tls["thread_starts"], 5)

    def test_handler_placement_charges_the_handshake_to_the_handler(self):
        rows = self._rows(handshake_in_handler=True)
        tls = next(r for r in rows if r["listener"] == "tls")
        self.assertEqual(tls["handshakes"], 5)
        self.assertGreater(tls["handshake_ms_sum"], 0)
        self.assertEqual(tls["accepts"], 5)
        self.assertLess(tls["accept_ms_sum"], tls["handshake_ms_sum"],
                        "the accept thread still ran the handshakes")

    def test_every_sample_writes_a_kernel_row(self):
        rows = self._rows(handshake_in_handler=False)
        kernel = [r for r in rows if r["listener"] == "kernel"]
        self.assertTrue(kernel)
        self.assertIn("accept_queue", kernel[0])


class KernelCounters(unittest.TestCase):
    NETSTAT = (
        "TcpExt: SyncookiesSent ListenOverflows ListenDrops TCPTimeouts\n"
        "TcpExt: 0 17 23 4\n"
        "IpExt: InNoRoutes InTruncatedPkts\n"
        "IpExt: 0 0\n"
    )

    def test_listen_counters_are_read_by_name(self):
        with tempfile.NamedTemporaryFile("w", suffix=".netstat") as f:
            f.write(self.NETSTAT)
            f.flush()
            self.assertEqual(corpus_serve.read_listen_counters(f.name), (17, 23))

    def test_a_missing_file_or_name_is_none_not_zero(self):
        self.assertIsNone(corpus_serve.read_listen_counters("/nonexistent/netstat"))
        with tempfile.NamedTemporaryFile("w") as f:
            f.write("TcpExt: SyncookiesSent\nTcpExt: 0\n")
            f.flush()
            self.assertIsNone(corpus_serve.read_listen_counters(f.name))

    def test_accept_queues_reads_recv_q_and_send_q_of_the_named_ports(self):
        with tempfile.TemporaryDirectory() as d:
            ss = os.path.join(d, "ss")
            with open(ss, "w") as f:
                f.write("#!/bin/sh\n"
                        "echo 'LISTEN 3 5 127.0.0.1:443 0.0.0.0:*'\n"
                        "echo 'LISTEN 0 128 0.0.0.0:22 0.0.0.0:*'\n"
                        "echo 'LISTEN 0 5 127.0.0.1:80 0.0.0.0:*'\n")
            os.chmod(ss, 0o755)
            self.assertEqual(corpus_serve.accept_queues({80, 443}, ss=ss),
                             {443: [3, 5], 80: [0, 5]})
            self.assertIsNone(corpus_serve.accept_queues({80}, ss=os.path.join(d, "absent")))


if __name__ == "__main__":
    unittest.main()
