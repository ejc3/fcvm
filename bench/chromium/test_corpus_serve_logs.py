#!/usr/bin/env python3
"""corpus_serve.py's replay logs: one JSON line per DNS query and per HTTP request.

The campaign reads $RESULTS/corpus-dns.log and $RESULTS/corpus-access.log after
a run to show which resolver the guest used and which hosts it fetched. Each
line has to carry the fields that reader keys on, and it has to be on disk
before the run's next step reads it, so every test below reads the log while
the server that wrote it is still running: a line held in a stdio buffer would
not be there.

Watched red 2026-08-28 against corpus_serve.py at 4d172153; the failure text is
quoted on each test.

Run: python3 -m unittest test_corpus_serve_logs -v
"""

import http.client
import json
import os
import socket
import struct
import sys
import tempfile
import threading
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
        self.addCleanup(sock.close)
        log = corpus_serve.JsonlLog(log_path) if log_path else None
        if log is not None:
            self.addCleanup(log.close)
        thread = threading.Thread(
            target=corpus_serve.serve_dns, args=(sock, answer_ip, log), daemon=True,
        )
        thread.start()
        return sock.getsockname()[1]

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
            rows = read_jsonl(log_path)
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
            (row,) = read_jsonl(log_path)
            self.assertEqual(row["qname"], "fonts.gstatic.com")
            self.assertEqual(row["qtype"], QTYPE_AAAA)
            self.assertEqual(row["answer"], "")

    def test_every_query_is_its_own_line(self):
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-dns.log")
            port = self._responder(log_path)
            for name in ("a.test", "b.test", "c.test"):
                self._ask(port, name, QTYPE_A)
            self.assertEqual([r["qname"] for r in read_jsonl(log_path)],
                             ["a.test", "b.test", "c.test"])

    def test_without_a_log_the_responder_still_answers_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            port = self._responder(None)
            reply, _ = self._ask(port, "blog.cloudflare.com", QTYPE_A)
            self.assertEqual(socket.inet_ntoa(reply[-4:]), "10.0.2.2")
            self.assertEqual(os.listdir(d), [])

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
        os.makedirs(site)
        with open(os.path.join(site, "a.txt"), "wb") as handle:
            handle.write(self.BODY)
        with open(os.path.join(site, "index.json"), "w") as handle:
            json.dump({"resources": {
                "https://example.test/a.txt": {"file": "a.txt", "status": 200,
                                               "mime": "text/plain"},
            }}, handle)
        return os.path.join(d, "corpus")

    def _server(self, d: str, log_path=None):
        from pathlib import Path

        exact, noquery, redirects = corpus_serve.load_indexes(Path(self._corpus(d)))
        log = corpus_serve.JsonlLog(log_path) if log_path else None
        if log is not None:
            self.addCleanup(log.close)

        # A subclass, so the handler's class-level tables and log stay private
        # to this test instead of mutating corpus_serve.Handler for the suite.
        class H(corpus_serve.Handler):
            pass

        H.exact, H.noquery, H.redirects = exact, noquery, redirects
        H.misses = []
        H.access_log = log
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server.server_port

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
            port = self._server(d, log_path)
            status, body, me = self._get(port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            (row,) = read_jsonl(log_path)
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
            port = self._server(d, log_path)
            status, _, _ = self._get(port, "/absent.js?v=1", host="cdn.test")
            self.assertEqual(status, 404)
            (row,) = read_jsonl(log_path)
            self.assertEqual(row["host"], "cdn.test")
            self.assertEqual(row["path"], "/absent.js?v=1")
            self.assertEqual(row["status"], 404)
            self.assertEqual(row["bytes"], 0)

    def test_the_host_header_port_is_kept_out_of_the_lookup_but_in_the_log(self):
        """The lookup strips :port (the guest sends Host: example.test:443 to
        the TLS listener); the log records the header as sent."""
        with tempfile.TemporaryDirectory() as d:
            log_path = os.path.join(d, "corpus-access.log")
            port = self._server(d, log_path)
            status, body, _ = self._get(port, "/a.txt", host="example.test:443")
            self.assertEqual((status, body), (200, self.BODY))
            (row,) = read_jsonl(log_path)
            self.assertEqual(row["host"], "example.test:443")

    def test_without_a_log_serving_is_unchanged_and_nothing_is_written(self):
        with tempfile.TemporaryDirectory() as d:
            port = self._server(d, None)
            status, body, _ = self._get(port, "/a.txt")
            self.assertEqual((status, body), (200, self.BODY))
            self.assertEqual(sorted(os.listdir(d)), ["corpus"])


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
