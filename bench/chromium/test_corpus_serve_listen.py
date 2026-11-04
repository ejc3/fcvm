#!/usr/bin/env python3
"""corpus_serve.py's listener measurements and the two listener options.

Issue #1159: the corpus throughput run's navigation time grew with the offered
rate while the replay's own per-request time (the access log's duration_ms,
which starts when a handler thread parses the request line) stayed flat. What
sits before that point is the kernel's accept queue (socketserver listens with
a backlog of 5), the accept thread, which for the TLS listener also runs the
handshake, and the handler thread's start. --listen-log records those, and
--backlog and --tls-handshake-in-accept change them. The server's defaults are
a backlog of 1024 and the handshake in the handler thread, which the runs
behind #1159 measured as the fix; the library's 5 and the accept-thread
handshake remain available for experiments.

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

    With --tls-handshake-in-accept the handshake runs inside accept(), in the
    one thread that accepts, so that client holds every later connection until
    it gives up. By default the handshake runs in the connection's own thread
    and the later connection is answered at once.
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

    def test_in_the_accept_thread_a_stalled_hello_holds_up_the_next_connection(self):
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

    def test_the_library_queue_overflows_under_a_burst(self):
        self.assertLess(self._fill(5), 16)

    def test_the_default_backlog_holds_the_whole_burst(self):
        self.assertEqual(self._fill(None), 64)

    def test_a_larger_backlog_holds_the_whole_burst(self):
        self.assertEqual(self._fill(256), 64)

    def test_the_flags_parse_and_default_to_the_measured_fix(self):
        parser = corpus_serve.build_parser()
        self.assertIsNone(parser.parse_args([]).backlog)
        self.assertEqual(parser.parse_args(["--backlog", "5"]).backlog, 5)
        self.assertFalse(parser.parse_args([]).tls_handshake_in_accept)
        self.assertTrue(parser.parse_args(["--tls-handshake-in-accept"]).tls_handshake_in_accept)


class DeadClients(TlsFixture, unittest.TestCase):
    """Connections whose client went away without closing them.

    A bridged or routed clone's namespace and veth are deleted when it is torn
    down, so the host side of every connection it had open gets no FIN: one it
    opened and never used stays open with a handler thread reading it. The
    campaign stops the replay with SIGTERM and SIGKILLs it after 5 s, and a
    killed replay's logs are incomplete, which makes the run's DNS evidence
    unclean. Seen on the first bridged trial: "corpus_serve did not exit;
    escalating to SIGKILL", exit status 137.

    Red before the fix: stop_listeners waited on the idle handler for as long
    as its client held the connection (the join below timed out), and the
    idle connection stayed open past any idle limit.
    """

    def _stop_in_background(self, server):
        done = threading.Event()

        def stop():
            corpus_serve.stop_listeners([server])
            done.set()

        threading.Thread(target=stop, daemon=True).start()
        return done

    def test_a_connection_that_sends_nothing_does_not_hold_the_shutdown(self):
        with tempfile.TemporaryDirectory() as d:
            server, _ = self._replay(d)
            idle = socket.create_connection(("127.0.0.1", server.server_port))
            self.addCleanup(idle.close)
            time.sleep(0.3)  # a handler thread is now reading it
            done = self._stop_in_background(server)
            self.assertTrue(done.wait(5), "the shutdown waited on a client that will never send")

    def test_a_tls_hello_that_never_comes_does_not_hold_the_shutdown(self):
        with tempfile.TemporaryDirectory() as d:
            server = self._tls(d, handshake_in_handler=True)
            idle = socket.create_connection(("127.0.0.1", server.server_port))
            self.addCleanup(idle.close)
            time.sleep(0.3)
            done = self._stop_in_background(server)
            self.assertTrue(done.wait(5), "the shutdown waited on a handshake that will never start")

    def test_a_response_in_flight_still_completes_after_the_reads_end(self):
        with tempfile.TemporaryDirectory() as d:
            server, _ = self._replay(d)
            conn = socket.create_connection(("127.0.0.1", server.server_port), timeout=5)
            self.addCleanup(conn.close)
            conn.sendall(b"GET /a.txt HTTP/1.0\r\nHost: example.test\r\n\r\n")
            data = b""
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
            server.end_reads()
            self.assertTrue(data.startswith(b"HTTP/1.0 200"), data[:40])
            self.assertTrue(data.endswith(self.BODY))

    def test_an_idle_connection_is_closed_after_the_idle_limit(self):
        class Quick(corpus_serve.Handler):
            timeout = 0.5

        with tempfile.TemporaryDirectory() as d:
            server, _ = self._replay(d, handler=Quick)
            idle = socket.create_connection(("127.0.0.1", server.server_port), timeout=5)
            self.addCleanup(idle.close)
            t0 = time.monotonic()
            self.assertEqual(idle.recv(1), b"", "the server never closed the idle connection")
            self.assertLess(time.monotonic() - t0, 4)

    def test_a_connection_whose_thread_has_not_started_is_still_released(self):
        """Red before the fix: a connection was registered for end_reads by its
        own handler thread, so one accepted just before shutdown whose thread
        had not yet run was missed, and the join waited for its 30 s idle limit."""
        from pathlib import Path

        exact, noquery, redirects = corpus_serve.load_indexes(Path(self._corpus(self._tmp())))

        class H(corpus_serve.Handler):
            pass

        H.exact, H.noquery, H.redirects = exact, noquery, redirects
        H.misses, H.access_log = [], None
        started = threading.Event()

        class Late(corpus_serve.ReplayServer):
            def process_request_thread(self, request, client_address):
                started.wait(10)
                super().process_request_thread(request, client_address)

        server = Late(("127.0.0.1", 0), H)
        self.addCleanup(server.server_close)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        idle = socket.create_connection(("127.0.0.1", server.server_port))
        self.addCleanup(idle.close)
        time.sleep(0.3)  # accepted; its handler thread is held before it runs
        done = threading.Event()

        def stop():
            server.shutdown()
            server.end_reads()
            started.set()
            server.server_close()
            done.set()

        threading.Thread(target=stop, daemon=True).start()
        self.assertTrue(done.wait(5), "a connection accepted before shutdown held the join")

    def _tmp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        return tmp.name

    def test_the_shipped_idle_limit_outlasts_the_stall_gate(self):
        self.assertGreaterEqual(corpus_serve.Handler.timeout, 15.0)


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
