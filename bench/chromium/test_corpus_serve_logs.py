#!/usr/bin/env python3
"""corpus_serve.py's replay logs: one JSON line per DNS query and per HTTP request.

The campaign reads $RESULTS/corpus-dns.log and $RESULTS/corpus-access.log after
a run to show which resolver the guest used and which hosts it fetched. Each
line has to carry the fields that reader keys on, and it has to be on disk
once the handler that answered has finished, so every test below reads the log
before it is closed: a line held in a stdio buffer would not be there. The
access line follows the response, so the HTTP tests wait for the handler
through the shipped shutdown sequence (stop_listeners, which joins it) rather
than read the moment the client has the body.

Watched red 2026-08-28 against corpus_serve.py at 4d172153; the failure text is
quoted on each test.

Run: python3 -m unittest test_corpus_serve_logs -v
"""

import errno
import http.client
import io
import json
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import corpus_serve  # noqa: E402

QTYPE_A = 1
QTYPE_AAAA = 28


def dns_query(name: str, qtype: int, txid: int = 0x1234) -> bytes:
    """A standard recursion-desired query, one question, built by hand."""
    packet = struct.pack(">HHHHHH", txid, 0x0100, 1, 0, 0, 0)
    for label in name.split("."):
        packet += bytes([len(label)]) + label.encode()
    return packet + b"\x00" + struct.pack(">HH", qtype, 1)


def read_jsonl(path: str) -> list:
    with open(path) as handle:
        return [json.loads(line) for line in handle if line.strip()]


class DnsLog(unittest.TestCase):
    """--dns-log: {ts, peer, qname, qtype, answer} per query, flushed per line.

    Red: `AttributeError: module 'corpus_serve' has no attribute 'bind_dns'`.
    """

    def _responder(self, log_path=None, answer_ip="10.0.2.2"):
        sock = corpus_serve.bind_dns("127.0.0.1", 0)
        port = sock.getsockname()[1]
        log = corpus_serve.JsonlLog(log_path) if log_path else None
        thread = threading.Thread(
            target=corpus_serve.serve_dns, args=(sock, answer_ip, log), daemon=True,
        )
        thread.start()
        self._dns = (sock, thread)

        def stop():
            # close() does not wake a recvfrom already blocked in the kernel
            # (see test_closing_the_socket_ends_the_responder_thread); one
            # datagram after the close releases it, and the thread is then
            # joined so no test leaves a blocked responder behind.
            sock.close()
            wake = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                wake.sendto(dns_query("wake.test", QTYPE_A), ("127.0.0.1", port))
            finally:
                wake.close()
            thread.join(timeout=5)
            if log is not None:
                log.close()
            self.assertFalse(thread.is_alive(), "serve_dns outlived its test")

        self.addCleanup(stop)
        return port

    def _rows(self, log_path: str) -> list:
        """The log once the responder has ended.

        The line follows the answer, so a client that already holds the reply
        can be ahead of the line, and a read at that moment sees a log that
        is short by it. The shipped stop_dns ends the responder and joins it;
        the log stays open, so a line that is there was flushed by the
        responder, not by close().
        """
        corpus_serve.stop_dns(*self._dns)
        return read_jsonl(log_path)

    def _ask(self, port: int, name: str, qtype: int):
        client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.addCleanup(client.close)
        client.settimeout(5)
        client.bind(("127.0.0.1", 0))  # an unbound client reports 0.0.0.0 as its name
        client.sendto(dns_query(name, qtype), ("127.0.0.1", port))
        reply, _ = client.recvfrom(512)
        return reply, "%s:%d" % client.getsockname()

    def test_an_a_query_is_logged_with_the_answer_it_got(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-dns.log")
            port = self._responder(log_path)
            reply, me = self._ask(port, "blog.cloudflare.com", QTYPE_A)
            self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2",
                             "the responder no longer answers A queries")
            rows = self._rows(log_path)
            self.assertEqual(len(rows), 1, rows)
            row = rows[0]
            self.assertEqual(sorted(row), ["answer", "peer", "qname", "qtype", "ts"])
            self.assertEqual(row["qname"], "blog.cloudflare.com")
            self.assertEqual(row["qtype"], QTYPE_A)
            self.assertEqual(row["answer"], "10.0.2.2")
            self.assertEqual(row["peer"], me)
            self.assertIsInstance(row["ts"], float)

    def test_an_aaaa_query_is_logged_with_an_empty_answer(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-dns.log")
            port = self._responder(log_path)
            self._ask(port, "fonts.gstatic.com", QTYPE_AAAA)
            (row,) = self._rows(log_path)
            self.assertEqual(row["qname"], "fonts.gstatic.com")
            self.assertEqual(row["qtype"], QTYPE_AAAA)
            self.assertEqual(row["answer"], "")

    def test_every_query_is_its_own_line(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-dns.log")
            port = self._responder(log_path)
            for name in ("a.test", "b.test", "c.test"):
                self._ask(port, name, QTYPE_A)
            self.assertEqual([r["qname"] for r in self._rows(log_path)],
                             ["a.test", "b.test", "c.test"])

    def test_without_a_log_the_responder_still_answers_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            port = self._responder(None)
            reply, _ = self._ask(port, "blog.cloudflare.com", QTYPE_A)
            self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2")
            self.assertEqual(os.listdir(d), [])

    def test_a_log_that_cannot_be_written_ends_the_responder(self):
        """A query answered but not logged is a hole in the evidence the
        campaign hashes. The broad handler swallowed the write error and the
        resolver kept answering, unlogged, for the rest of the run. It now
        closes its socket and ends: the guest stops resolving, the :53 owner
        sampler sees no owner, and the run is refused on both counts.

        Red: the second query was answered (thread alive, socket open).
        """
        class BrokenLog:
            def write(self, _row):
                raise OSError(28, "No space left on device")

            def close(self):
                pass

        sock = corpus_serve.bind_dns("127.0.0.1", 0)
        port = sock.getsockname()[1]
        seen = []
        saved_hook = threading.excepthook
        threading.excepthook = lambda args: seen.append(args.exc_value)
        self.addCleanup(setattr, threading, "excepthook", saved_hook)
        thread = threading.Thread(target=corpus_serve.serve_dns,
                                  args=(sock, "10.0.2.2", BrokenLog()), daemon=True)
        thread.start()
        reply, _ = self._ask(port, "blog.cloudflare.com", QTYPE_A)
        self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2",
                         "the query before the failed write must still be answered")
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive(), "serve_dns kept serving after a failed log write")
        self.assertEqual(sock.fileno(), -1, "the responder left its socket open")
        self.assertTrue(seen and isinstance(seen[0], OSError), seen)
        client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.addCleanup(client.close)
        client.settimeout(1)
        client.sendto(dns_query("second.test", QTYPE_A), ("127.0.0.1", port))
        with self.assertRaises(OSError):
            client.recvfrom(512)

    def test_closing_the_socket_ends_the_responder_thread(self):
        """A closed socket must end serve_dns, not spin it on EBADF forever.

        The responder's broad exception handler turned a closed socket into a
        busy loop at 100% of one core for the rest of the process. Every test
        above closes its socket in cleanup, so without this the suite itself
        would leave spinning threads behind.

        close() from this thread does not wake a recvfrom already blocked in
        the kernel; the in-flight call keeps the socket alive until a datagram
        arrives. One datagram after the close is what releases it, and what
        the loop does next is the thing under test.
        """
        sock = corpus_serve.bind_dns("127.0.0.1", 0)
        port = sock.getsockname()[1]
        thread = threading.Thread(target=corpus_serve.serve_dns,
                                  args=(sock, "10.0.2.2", None), daemon=True)
        thread.start()
        reply, _ = self._ask(port, "blog.cloudflare.com", QTYPE_A)
        self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2")
        sock.close()
        wake = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.addCleanup(wake.close)
        wake.sendto(dns_query("wake.test", QTYPE_A), ("127.0.0.1", port))
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive(), "serve_dns kept running on a closed socket")


class AccessLog(unittest.TestCase):
    """--access-log: {ts, peer, method, host, path, status, bytes, duration_ms}.

    Red: `AttributeError: module 'corpus_serve' has no attribute 'JsonlLog'`.
    """

    BODY = b"hello from the corpus\n"

    def _corpus(self, d: str):
        site = os.path.join(d, "corpus", "example")
        os.makedirs(site, exist_ok=True)
        with open(os.path.join(site, "a.txt"), "wb") as handle:
            handle.write(self.BODY)
        with open(os.path.join(site, "index.json"), "w") as handle:
            json.dump({"resources": {
                "https://example.test/a.txt": {"file": "a.txt", "status": 200,
                                               "mime": "text/plain"},
            }}, handle)
        return os.path.join(d, "corpus")

    def _replay(self, d: str, log=None, peers=None, start=True, handler=None):
        """The shipped server class over a one-file corpus on an ephemeral port.

        Returns (server, thread). With start=False the thread is None and the
        caller runs serve_forever itself, as serve_http does. `handler` is the
        handler class to serve with (default corpus_serve.Handler).
        """
        from pathlib import Path

        exact, noquery, redirects = corpus_serve.load_indexes(Path(self._corpus(d)))

        # A subclass, so the handler's class-level tables and log stay private
        # to this test instead of mutating corpus_serve.Handler for the suite.
        class H(handler or corpus_serve.Handler):
            pass

        H.exact, H.noquery, H.redirects = exact, noquery, redirects
        H.misses = []
        H.access_log = log
        server = corpus_serve.ReplayServer(("127.0.0.1", 0), H, peers)
        self.addCleanup(server.server_close)
        thread = None
        if start:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            self.addCleanup(server.shutdown)
        return server, thread

    def _server(self, d: str, log_path=None):
        log = corpus_serve.JsonlLog(log_path) if log_path else None
        if log is not None:
            self.addCleanup(log.close)
        return self._replay(d, log)[0]

    def _rows(self, server, log_path: str) -> list:
        """The log once every handler `server` dequeued has finished.

        The access line is written after the response, so a client that
        already holds the whole body can be ahead of the line, and a read at
        that moment sees a log that is short by it. The shipped shutdown
        sequence waits for the handlers; the log stays open, so a line that
        is there was flushed by the handler, not by close().
        """
        corpus_serve.stop_listeners([server])
        return read_jsonl(log_path)

    def _get(self, port: int, path: str, host: str = "example.test"):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        self.addCleanup(conn.close)
        conn.request("GET", path, headers={"Host": host})
        me = "%s:%d" % conn.sock.getsockname()  # gone once the HTTP/1.0 body is read
        response = conn.getresponse()
        return response.status, response.read(), me

    def test_a_hit_is_logged_with_host_path_status_and_body_bytes(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            server = self._server(d, log_path)
            status, body, me = self._get(server.server_port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            (row,) = self._rows(server, log_path)
            self.assertEqual(sorted(row), ["bytes", "duration_ms", "host", "method",
                                           "path", "peer", "status", "ts"])
            self.assertEqual(row["method"], "GET")
            self.assertEqual(row["host"], "example.test")
            self.assertEqual(row["path"], "/a.txt")
            self.assertEqual(row["status"], 200)
            self.assertEqual(row["bytes"], len(self.BODY))
            self.assertEqual(row["peer"], me)
            self.assertIsInstance(row["ts"], float)
            self.assertGreaterEqual(row["duration_ms"], 0.0)

    def test_a_miss_is_logged_as_a_404_with_zero_bytes(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            server = self._server(d, log_path)
            status, _, _ = self._get(server.server_port, "/absent.js?v=1", host="cdn.test")
            self.assertEqual(status, 404)
            (row,) = self._rows(server, log_path)
            self.assertEqual(row["host"], "cdn.test")
            self.assertEqual(row["path"], "/absent.js?v=1")
            self.assertEqual(row["status"], 404)
            self.assertEqual(row["bytes"], 0)

    def test_the_host_header_port_is_kept_out_of_the_lookup_but_in_the_log(self):
        """The lookup strips :port (the guest sends Host: example.test:443 to
        the TLS listener); the log records the header as sent."""
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            server = self._server(d, log_path)
            status, body, _ = self._get(server.server_port, "/a.txt", host="example.test:443")
            self.assertEqual((status, body), (200, self.BODY))
            (row,) = self._rows(server, log_path)
            self.assertEqual(row["host"], "example.test:443")

    def test_without_a_log_serving_is_unchanged_and_nothing_is_written(self):
        with tempfile.TemporaryDirectory() as d:
            server = self._server(d, None)
            status, body, _ = self._get(server.server_port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            corpus_serve.stop_listeners([server])
            self.assertEqual(sorted(os.listdir(d)), ["corpus"])


    class BreaksAfter:
        """A JsonlLog that writes its first `n` rows and fails every one
        after, as a disk that fills part way through a run would."""

        def __init__(self, path: str, n: int):
            self.inner = corpus_serve.JsonlLog(path)
            self.n = n
            self.calls = 0
            self._lock = threading.Lock()

        def write(self, row: dict) -> None:
            with self._lock:
                self.calls += 1
                if self.calls > self.n:
                    raise OSError(errno.ENOSPC, "No space left on device")
            self.inner.write(row)

        def close(self) -> None:
            self.inner.close()

    @staticmethod
    def _quiet(server):
        # The handler re-raises after fail_closed, so the server's
        # handle_error prints the traceback: wanted in corpus_serve.log,
        # noise here.
        server.handle_error = lambda _request, _address: None

    def test_a_log_that_cannot_be_written_stops_the_server(self):
        """A request answered but not logged is a hole in the evidence the
        campaign hashes, the same as an unlogged DNS query. The write raised
        inside one ThreadingHTTPServer handler thread, that thread died, and
        the server kept answering, unlogged, for the rest of the run: the
        URL brackets passed and write_dns_evidence hashed a truncated log.

        The line is written after the response, so the request whose line
        fails is still answered; what must not happen is a third one.

        Red: `serve_forever kept running after a failed access-log write`.
        """
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            log = self.BreaksAfter(log_path, 1)
            self.addCleanup(log.close)
            server, thread = self._replay(d, log)
            self._quiet(server)
            port = server.server_port
            self.assertEqual(self._get(port, "/a.txt")[0], 200)
            self.assertEqual(self._get(port, "/a.txt")[0], 200,
                             "the line is written after the response, so the "
                             "request whose line fails is still answered")
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive(),
                             "serve_forever kept running after a failed access-log write")
            self.assertIsInstance(server.log_failure, OSError)
            self.assertEqual(server.log_failure.errno, errno.ENOSPC)
            self.assertEqual(server.failed_log, "access")
            self.assertEqual(len(self._rows(server, log_path)), 1,
                             "the line that failed to write is in the log")
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            self.addCleanup(conn.close)
            with self.assertRaises(OSError,
                                   msg="a third request was answered after the failed write"):
                conn.request("GET", "/a.txt", headers={"Host": "example.test"})
                conn.getresponse()

    def test_a_failed_write_on_one_listener_ends_both_and_serve_http_returns_1(self):
        """The guest fetches through HTTP and HTTPS alike, so a log missing
        one listener's lines is as short as one missing both. main() blocks
        in serve_http: a failure on the HTTP listener, served from a helper
        thread, must also end the HTTPS serve_forever the main thread sits
        in, and serve_http must return 1 with the reason on `err`.

        Both listeners are plain HTTP here; TLS is not what is under test.

        Red: `AttributeError: module 'corpus_serve' has no attribute
        'serve_http'`.
        """
        with tempfile.TemporaryDirectory() as d:
            log = self.BreaksAfter(os.path.join(d, "corpus-access.log"), 1)
            self.addCleanup(log.close)
            peers = []
            plain, _ = self._replay(d, log, peers, start=False)
            tls, _ = self._replay(d, log, peers, start=False)
            self._quiet(plain)
            self._quiet(tls)
            err = io.StringIO()
            returned = []
            loop = threading.Thread(
                target=lambda: returned.append(corpus_serve.serve_http(plain, tls, err)),
                daemon=True)
            loop.start()
            # A loop still running after the assertions below is a failed
            # test; stop it so it does not spin on closed sockets afterwards.
            self.addCleanup(lambda: (plain.shutdown(), tls.shutdown())
                            if loop.is_alive() else None)
            self.assertEqual(self._get(tls.server_port, "/a.txt")[0], 200)
            self.assertEqual(self._get(plain.server_port, "/a.txt")[0], 200)
            loop.join(timeout=5)
            self.assertFalse(loop.is_alive(), "serve_http kept serving after a "
                                              "failed access-log write on the HTTP listener")
            self.assertEqual(returned, [1])
            self.assertIsInstance(plain.log_failure, OSError)
            self.assertIs(tls.log_failure, plain.log_failure,
                          "the failure was not recorded on the other listener")
            self.assertIn("FAILED", err.getvalue())
            self.assertIn("No space left on device", err.getvalue())

    def test_a_dns_log_line_that_cannot_be_written_stops_the_http_listeners(self):
        """A DNS line that could not be written closed the responder's socket
        and raised in its own thread, and nothing else: the HTTP listeners
        kept answering. In the after-run bracket the :53 owner sampler has
        already stopped, the clone resolves what it still holds and fetches
        through the listeners that are still up, the bracket passes, and the
        truncated DNS log is hashed as clean. The failure must reach the
        server-wide state the access log's failure uses, so every listener
        stops and main() exits 1 with the reason.

        The line follows the answer, so the query whose line fails is still
        answered; what must not happen is an HTTP request after it.

        Red: the responder thread died on `TypeError: serve_dns() got an
        unexpected keyword argument 'server'` before answering, so the first
        query timed out (`TimeoutError: timed out`); with the argument
        accepted and unused: `True is not false : the HTTP listener kept
        serving after a failed DNS-log write`.
        """
        with tempfile.TemporaryDirectory() as d:
            access_log = corpus_serve.JsonlLog(os.path.join(d, "corpus-access.log"))
            self.addCleanup(access_log.close)
            server, thread = self._replay(d, access_log)
            dns_log_path = os.path.join(d, "corpus-dns.log")
            dns_log = self.BreaksAfter(dns_log_path, 1)
            self.addCleanup(dns_log.close)
            sock = corpus_serve.bind_dns("127.0.0.1", 0)
            port = sock.getsockname()[1]
            seen = []
            saved_hook = threading.excepthook
            threading.excepthook = lambda args: seen.append(args.exc_value)
            self.addCleanup(setattr, threading, "excepthook", saved_hook)
            responder = threading.Thread(target=corpus_serve.serve_dns,
                                         args=(sock, "10.0.2.2", dns_log),
                                         kwargs={"server": server}, daemon=True)
            responder.start()
            for name in ("first.test", "second.test"):
                client = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                self.addCleanup(client.close)
                client.settimeout(5)
                client.sendto(dns_query(name, QTYPE_A), ("127.0.0.1", port))
                reply, _ = client.recvfrom(512)
                self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2", f"{name} was not answered")
            responder.join(timeout=5)
            self.assertFalse(responder.is_alive(), "serve_dns kept serving after a failed log write")
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive(),
                             "the HTTP listener kept serving after a failed DNS-log write")
            self.assertIsInstance(server.log_failure, OSError)
            self.assertEqual(server.log_failure.errno, errno.ENOSPC)
            self.assertEqual(server.failed_log, "dns")
            self.assertEqual([r["qname"] for r in read_jsonl(dns_log_path)], ["first.test"])
            self.assertTrue(seen and isinstance(seen[0], OSError),
                            f"the failure no longer reaches corpus_serve.log through the thread: {seen}")
            conn = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=1)
            self.addCleanup(conn.close)
            with self.assertRaises(OSError, msg="a request was answered after the failed DNS-log write"):
                conn.request("GET", "/a.txt", headers={"Host": "example.test"})
                conn.getresponse()

    def test_a_signal_stop_waits_for_the_handlers_in_flight_before_closing_the_log(self):
        """The access line follows the response, so a client can hold the
        whole body while the line is still unwritten. The campaign sends
        SIGTERM the moment the after-run verify returns and hashes the log
        once the process is gone, so a process that exited on the signal
        without waiting for that handler left a truncated log that was
        hashed as clean. The stop must end accepting, wait for every handler
        already dequeued, then close the logs, and serve_http returns 0.

        The handler holds its thread after answering until the test releases
        it, so the stop lands with the response read and the line to come,
        and a stop that returns before the release did not wait.

        Red: `AttributeError: module 'corpus_serve' has no attribute
        'install_signal_handlers'`; with a handler that ended the loops and
        closed the log without the wait: `False is not true : serve_http
        returned while a handler was still running`.
        """
        release = threading.Event()
        self.addCleanup(release.set)

        class Held(corpus_serve.Handler):
            def do_GET(self):
                self._serve()
                release.wait(timeout=10)

        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            log = corpus_serve.JsonlLog(log_path)
            peers = []
            plain, _ = self._replay(d, log, peers, start=False, handler=Held)
            tls, _ = self._replay(d, log, peers, start=False, handler=Held)
            returned = []
            loop = threading.Thread(
                target=lambda: returned.append(corpus_serve.serve_http(plain, tls, logs=[log])),
                daemon=True)
            loop.start()
            self.addCleanup(lambda: (plain.shutdown(), tls.shutdown())
                            if loop.is_alive() else None)
            # The real handler, installed and then put back: unittest runs
            # this on the main thread, where signal.signal is allowed.
            saved = {sig: signal.getsignal(sig) for sig in (signal.SIGTERM, signal.SIGINT)}
            for sig, previous in saved.items():
                self.addCleanup(signal.signal, sig, previous)
            on_signal = corpus_serve.install_signal_handlers(tls)
            for sig in saved:
                self.assertIs(signal.getsignal(sig), on_signal, f"{sig!r} is not handled")
            status, body, _ = self._get(plain.server_port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            on_signal(signal.SIGTERM, None)
            # serve_forever polls its shutdown flag every 0.5 s, so ending
            # both loops takes up to 1 s; a stop that waits then blocks on
            # the held handler and cannot return before the release.
            loop.join(timeout=2)
            self.assertTrue(loop.is_alive(), "serve_http returned while a handler was still running")
            release.set()
            loop.join(timeout=5)
            self.assertFalse(loop.is_alive(), "serve_http did not return once the handler had finished")
            self.assertEqual(returned, [0])
            rows = read_jsonl(log_path)
            self.assertEqual(len(rows), 1,
                             "the access line of the request in flight at the stop is not in the log")
            self.assertEqual((rows[0]["status"], rows[0]["bytes"]), (200, len(self.BODY)))
            with self.assertRaises(ValueError, msg="the log is still open after the stop"):
                log.write({"late": True})
            conn = http.client.HTTPConnection("127.0.0.1", plain.server_port, timeout=1)
            self.addCleanup(conn.close)
            with self.assertRaises(OSError, msg="a request was answered after the stop"):
                conn.request("GET", "/a.txt", headers={"Host": "example.test"})
                conn.getresponse()

    def test_sigterm_to_the_process_leaves_the_last_access_line_on_disk(self):
        """main() end to end, the way stop_corpus_serve ends it: a real
        SIGTERM to the process while the handler of an already answered
        request is still running. The process must exit 0 with that
        request's line in the log.

        The wrapper makes the shipped handler hold its thread after
        answering until a flag file appears, and announces the HTTP port
        serve_http was given, which main() does not print when asked for
        port 0.

        Red: `0 != 1 : the access line of the request in flight at SIGTERM
        is not in the log (exit -15 before the handler finished)`, the
        signal's default action; with a handler that ended the loops and
        closed the log without the wait, the same with exit 0.
        """
        with tempfile.TemporaryDirectory() as d:
            root = self._corpus(d)
            log_path = os.path.join(d, "corpus-access.log")
            flag = os.path.join(d, "release")
            wrapper = textwrap.dedent(f"""
                import os, sys, time
                sys.path.insert(0, {HERE!r})
                import corpus_serve

                def held(self):
                    corpus_serve.Handler._serve(self)
                    deadline = time.monotonic() + 10
                    while not os.path.exists({flag!r}) and time.monotonic() < deadline:
                        time.sleep(0.01)

                corpus_serve.Handler.do_GET = held
                serve_http = corpus_serve.serve_http

                def announce(plain, tls, *args, **kwargs):
                    print("PORT", plain.server_port, flush=True)
                    return serve_http(plain, tls, *args, **kwargs)

                corpus_serve.serve_http = announce
                sys.exit(corpus_serve.main())
            """)
            proc = subprocess.Popen(
                [sys.executable, "-c", wrapper, "--root", root, "--port", "0",
                 "--tls-port", "0", "--dns-addr", "127.0.0.1", "--dns-port", "0",
                 "--access-log", log_path, "--dns-log", os.path.join(d, "corpus-dns.log")],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            watchdog = threading.Timer(30, proc.kill)
            watchdog.start()
            self.addCleanup(watchdog.cancel)
            self.addCleanup(proc.stdout.close)
            self.addCleanup(proc.kill)
            port = None
            head = []
            for line in proc.stdout:
                head.append(line)
                found = re.match(r"PORT (\d+)$", line.strip())
                if found:
                    port = int(found.group(1))
                    break
            self.assertIsNotNone(port, "corpus_serve did not start:\n" + "".join(head))
            status, body, _ = self._get(port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            proc.send_signal(signal.SIGTERM)
            # serve_forever polls its shutdown flag every 0.5 s, so ending
            # both loops takes up to 1 s; a process that waits then blocks
            # on the held handler and cannot exit before the flag appears.
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            early_exit = proc.returncode
            open(flag, "w").close()
            rest = proc.stdout.read()
            proc.wait(timeout=10)
            output = "".join(head) + rest
            rows = read_jsonl(log_path)
            self.assertEqual(len(rows), 1, "the access line of the request in flight at SIGTERM "
                             f"is not in the log (exit {early_exit} before the handler finished):\n{output}")
            self.assertEqual((rows[0]["status"], rows[0]["bytes"]), (200, len(self.BODY)))
            self.assertIsNone(early_exit,
                              f"corpus_serve exited {early_exit} while a handler was still running")
            self.assertEqual(proc.returncode, 0,
                             f"corpus_serve exited {proc.returncode} on SIGTERM:\n{output}")


class Flags(unittest.TestCase):
    """Both logs are optional command-line flags that default to off.

    Red: `AttributeError: module 'corpus_serve' has no attribute 'build_parser'`.
    """

    def test_both_flags_parse_and_default_to_none(self):
        parser = corpus_serve.build_parser()
        off = parser.parse_args([])
        self.assertIsNone(off.dns_log)
        self.assertIsNone(off.access_log)
        on = parser.parse_args(["--dns-log", "/r/corpus-dns.log",
                                "--access-log", "/r/corpus-access.log"])
        self.assertEqual(on.dns_log, "/r/corpus-dns.log")
        self.assertEqual(on.access_log, "/r/corpus-access.log")


if __name__ == "__main__":
    unittest.main()
