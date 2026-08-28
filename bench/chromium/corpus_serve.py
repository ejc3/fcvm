#!/usr/bin/env python3
"""Host-aware replay server for the deep-captured corpus.

Serves the ORIGINAL urls: the replay Chromium runs with
--host-resolver-rules="MAP * 127.0.0.1", so every host resolves here; this
server picks the response by (Host header, path?query) from the capture
indexes and replays status + mime + body byte-identically. HTTP on --port and
HTTPS on --tls-port with a self-signed cert (the bench Chromium bakes in
--ignore-certificate-errors).

Resolution order for a request URL u:
  1. exact match on the full url (scheme-insensitive)
  2. match ignoring the query string (analytics beacons carry per-view ids)
  3. 404, counted in /––misses (read by the checker for the report)

Two optional logs, one JSON object per line, flushed per line so a log left
behind by a killed process is complete up to its last answered request:
  --dns-log PATH     {ts, peer, qname, qtype, answer} per DNS query; answer is
                     the address for A queries and "" for everything else
  --access-log PATH  {ts, peer, method, host, path, status, bytes, duration_ms}
                     per HTTP and HTTPS request; host is the Host header as sent

Either log is evidence the campaign hashes once this process has exited, so
the log has to be complete when it exits. SIGTERM and SIGINT run the shutdown
sequence (stop_listeners): every listener stops accepting, the DNS responder
ends, every handler already dequeued finishes and writes its line, the logs
are closed, and main() returns 0. A line that cannot be written stops the
server through the same sequence: the DNS responder closes its socket, an
access line that fails ends both HTTP listeners, and main() returns 1 with
the reason.
"""

import argparse
import http.server
import json
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_indexes(root: Path):
    exact, noquery, redirects = {}, {}, {}
    for idx in sorted(root.glob("*/index.json")):
        data = json.loads(idx.read_text())
        site = idx.parent
        for url, meta in data.get("resources", {}).items():
            u = urllib.parse.urlparse(url)
            key = (u.netloc, u.path, u.query)
            entry = (site / meta["file"], meta.get("status", 200), meta.get("mime", ""))
            exact.setdefault(key, entry)
            noquery.setdefault((u.netloc, u.path), entry)
        for url, r in data.get("redirects", {}).items():
            u = urllib.parse.urlparse(url)
            redirects.setdefault((u.netloc, u.path, u.query), r)
            redirects.setdefault((u.netloc, u.path), r)
    return exact, noquery, redirects


class JsonlLog:
    """Append-only JSON-lines file; every line is flushed as it is written.

    The campaign reads both replay logs while this process is still serving, so
    a line may not sit in a stdio buffer. One lock serialises the HTTP handler
    threads; the DNS responder is a single thread.
    """

    def __init__(self, path: str):
        self.path = path
        self._lock = threading.Lock()
        self._fh = open(path, "a", encoding="utf-8")

    def write(self, row: dict) -> None:
        line = json.dumps(row, separators=(",", ":")) + "\n"
        with self._lock:
            self._fh.write(line)
            self._fh.flush()

    def close(self) -> None:
        with self._lock:
            self._fh.close()


class Handler(http.server.BaseHTTPRequestHandler):
    exact = {}
    noquery = {}
    redirects = {}
    misses = []
    lock = threading.Lock()
    # --access-log: a JsonlLog, or None for no log. Set once by main().
    access_log = None
    _log_status = None
    _log_bytes = 0

    def handle_one_request(self):
        # One access line per parsed request, written after the response has
        # been flushed so `bytes` and `duration_ms` cover the whole exchange.
        # A keep-alive close or an empty request line sends no response and
        # gets no line; a 400 for an unparsable one is logged with what the
        # parser managed to read.
        ts = time.time()
        t0 = time.monotonic()
        self._log_status = None
        self._log_bytes = 0
        try:
            super().handle_one_request()
        finally:
            if self.access_log is not None and self._log_status is not None:
                headers = getattr(self, "headers", None)
                try:
                    self.access_log.write({
                        "ts": ts,
                        "peer": f"{self.client_address[0]}:{self.client_address[1]}",
                        "method": getattr(self, "command", None) or "",
                        "host": (headers.get("Host") or "") if headers is not None else "",
                        "path": getattr(self, "path", None) or "",
                        "status": self._log_status,
                        "bytes": self._log_bytes,
                        "duration_ms": (time.monotonic() - t0) * 1000,
                    })
                except Exception as exc:  # noqa: BLE001 - whatever it was, the line is not on disk
                    # An answered, unlogged request is a hole in the evidence
                    # the campaign hashes, as an unlogged DNS query is. Left to
                    # ThreadingHTTPServer this would end one handler thread and
                    # the server would keep answering, unlogged, for the rest
                    # of the run. Stop every listener instead; the re-raise
                    # puts the traceback in corpus_serve.log via handle_error.
                    self.server.fail_closed(exc, "access")
                    raise

    def send_response(self, code, message=None):
        self._log_status = int(code)
        super().send_response(code, message)

    def send_header(self, keyword, value):
        if keyword.lower() == "content-length":
            try:
                self._log_bytes = int(value)
            except (TypeError, ValueError):
                pass
        super().send_header(keyword, value)

    def _serve(self):
        host = (self.headers.get("Host") or "").split(":")[0]
        u = urllib.parse.urlparse(self.path)
        entry = self.exact.get((host, u.path, u.query)) or \
            self.noquery.get((host, u.path))
        if self.path == "/––misses":
            body = json.dumps(self.misses).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if entry is None:
            r = self.redirects.get((host, u.path, u.query)) or \
                self.redirects.get((host, u.path))
            if r:
                self.send_response(r.get("status", 302))
                self.send_header("Location", r.get("location", "/"))
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            with self.lock:
                self.misses.append(f"{host}{self.path}"[:200])
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        path, status, mime = entry
        data = path.read_bytes()
        # Replay the RECORDED status: coercing errors to 200 changed browser
        # behavior for captured 404/400 responses. Body-bearing 3xx entries
        # (no Location header captured) degrade to 200 so the browser does
        # not follow a redirect to nowhere; real redirect hops are replayed
        # from the captured redirect map above.
        status = status or 200
        if 300 <= status < 400:
            status = 200
        self.send_response(status)
        if mime:
            # CDP's mimeType drops the charset parameter, and captured TEXT
            # bodies are re-encoded utf-8 (getResponseBody returns decoded
            # text). Without an explicit charset Chromium assumes cp1252 and
            # every accented site renders mojibake (elmundo, 2026-08-13).
            if ("charset" not in mime
                    and (mime.startswith("text/")
                         or mime in ("application/javascript",
                                     "application/json",
                                     "image/svg+xml"))):
                mime += "; charset=utf-8"
            self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        # Cross-origin subresources (fonts, module scripts, manifests) are CORS
        # requests; the real CDNs send ACAO and dropping it turned every such
        # fetch into net::ERR_FAILED on replay (Guardian, 2026-08-13). Echo the
        # Origin rather than "*": credentialed fetches (CMP/geo APIs) reject
        # the wildcard, and those gate consent overlays the pixel check sees.
        origin = self.headers.get("Origin")
        self.send_header("Access-Control-Allow-Origin", origin or "*")
        if origin:
            self.send_header("Access-Control-Allow-Credentials", "true")
            self.send_header("Vary", "Origin")
        self.end_headers()
        self.wfile.write(data)

    def do_OPTIONS(self):
        # CORS preflight for credentialed fetches (CMP/geo chains): without
        # this the base handler answers 501 and the chain dies before any
        # ACAO header is ever consulted.
        origin = self.headers.get("Origin") or "*"
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", origin)
        self.send_header("Access-Control-Allow-Credentials", "true")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header(
            "Access-Control-Allow-Headers",
            self.headers.get("Access-Control-Request-Headers") or "*",
        )
        self.end_headers()

    do_GET = _serve
    do_POST = _serve  # beacons POST; replay their captured (or 404) response

    def log_message(self, *a):  # quiet
        pass


# Serialises fail_closed across handler threads: the first failure is the one
# recorded, and the listeners are shut down once.
_FAIL_CLOSED = threading.Lock()


class ReplayServer(http.server.ThreadingHTTPServer):
    """One replay listener; `peers` is the list of listeners sharing the log.

    A handler whose access line could not be written calls fail_closed(): the
    error is recorded as log_failure on every peer, with the log's name as
    failed_log, and every peer is asked to stop, so serve_http() runs the
    shutdown sequence and exits 1 with the reason instead of serving on with
    a truncated log. Both listeners stop because the guest fetches through
    both, and a log with only one side's lines is as short as one with
    neither. Nothing clears the failure.
    """

    # A handler thread outlives shutdown(): a request dequeued before the
    # serve loop returned is still being answered and logged. As
    # ThreadingHTTPServer ships them the handler threads are daemon threads,
    # which server_close() does not join and the interpreter kills at exit,
    # so an exit right after shutdown() dropped the last access line while
    # the client already held the response. Non-daemon threads are the ones
    # server_close() joins (block_on_close), which is what lets
    # stop_listeners close the log only once every line is on disk.
    daemon_threads = False
    block_on_close = True

    def __init__(self, address, handler, peers=None):
        super().__init__(address, handler)
        self.log_failure = None
        self.failed_log = None
        self.peers = peers if peers is not None else []
        self.peers.append(self)

    def fail_closed(self, exc: BaseException, log_name: str) -> None:
        with _FAIL_CLOSED:
            if any(peer.log_failure is not None for peer in self.peers):
                return
            for peer in self.peers:
                peer.log_failure = exc
                peer.failed_log = log_name
        self.request_stop()

    def request_stop(self) -> None:
        """Ask every peer to stop accepting; returns at once.

        shutdown() blocks until the serve loop has returned, so it cannot run
        on that loop's own thread: a handler thread here, or the main thread
        inside serve_forever, which is where a signal handler runs. A helper
        thread stops the peers in list order; main() blocks in the last
        one's serve_forever, and serve_http continues from there.
        """
        threading.Thread(target=self._stop_peers, name="stop", daemon=True).start()

    def _stop_peers(self) -> None:
        for peer in self.peers:
            peer.shutdown()


def stop_listeners(peers, logs=()) -> None:
    """The shutdown sequence, in this order: no listener accepts another
    connection, every handler already dequeued runs to its end, then the
    logs are closed.

    The access line is written after the response, so a client can hold the
    whole body while the line is still to come. The campaign sends SIGTERM
    as soon as its after-run verify returns and hashes both logs once this
    process is gone; without the wait, the last handler's line was lost and
    a truncated log was hashed as clean. Every handler answers one HTTP/1.0
    request and exits, so the wait is bounded by the slowest response in
    flight; a handler that never answered owes no line, and the campaign's
    stop_corpus_serve escalates to SIGKILL after 5 s.
    """
    for peer in peers:
        peer.shutdown()
    for peer in peers:
        peer.server_close()  # joins the handler threads (block_on_close)
    for log in logs:
        log.close()


def stop_dns(sock: socket.socket, thread: threading.Thread) -> None:
    """End serve_dns and wait for it: no query is answered after this
    returns, and every answered one has its line written before the DNS log
    is closed.

    close() alone does not wake a recvfrom blocked in the kernel (the call
    keeps the socket alive until a datagram arrives). shutdown(SHUT_RD) does,
    with an empty datagram; on an unconnected UDP socket it raises ENOTCONN
    and wakes the reader all the same. The responder then finds the socket
    closed and returns. A socket serve_dns already closed on a failed write
    is tolerated.
    """
    try:
        sock.shutdown(socket.SHUT_RD)
    except OSError:
        pass
    sock.close()
    thread.join()


def install_signal_handlers(server: ReplayServer):
    """SIGTERM and SIGINT stop the replay with its logs complete.

    The handler runs on the main thread, inside serve_forever, so it only
    asks the peers to stop; serve_http runs the shutdown sequence once the
    loops have returned. Returns the handler.
    """
    def on_signal(signum, _frame):
        print(f"stopping on {signal.Signals(signum).name}", flush=True)
        server.request_stop()

    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, on_signal)
    return on_signal


def serve_http(plain: ReplayServer, tls: ReplayServer, err=None, logs=(), dns=None) -> int:
    """Serve both listeners until a signal or a failed log write stops them.

    serve_forever() returns only through ReplayServer.request_stop (the
    signal handler, or fail_closed; nothing else calls shutdown). The shutdown
    sequence then runs: the DNS responder `dns` (its socket and thread) is
    ended and joined, and stop_listeners waits for the handlers in flight
    and closes `logs`. Returns 0 after a signal. After a failed log write the
    reason goes to `err` (stderr by default) and the status is 1; the process
    exits with it, the guest's fetches and lookups start failing, and the
    campaign records the run as unclean on the after-run bracket and the :53
    owner samples.
    """
    threading.Thread(target=plain.serve_forever, daemon=True).start()
    tls.serve_forever()
    if dns is not None:
        stop_dns(*dns)
    stop_listeners((tls, plain), logs)
    failure = tls.log_failure or plain.log_failure
    if failure is None:
        return 0
    print(f"FAILED: the {tls.failed_log} log could not be written, replay stopped: {failure!r}",
          file=err or sys.stderr, flush=True)
    return 1


def selfsigned(tmpdir: Path):
    crt, key = tmpdir / "replay.crt", tmpdir / "replay.key"
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
         "-keyout", str(key), "-out", str(crt), "-days", "2",
         "-subj", "/CN=corpus-replay",
         "-addext", "subjectAltName=DNS:*,DNS:localhost,IP:127.0.0.1"],
        check=True, capture_output=True)
    return crt, key


def bind_dns(addr: str, port: int) -> socket.socket:
    """Bind the responder's UDP socket; port 0 takes an ephemeral port.

    Bound by the caller rather than inside the serving loop so a bind failure
    (dnsmasq still holding :53) is a startup error, not a dead thread behind
    HTTP servers that keep answering.
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((addr, port))
    return sock


def serve_dns(sock: socket.socket, answer_ip: str = "127.0.0.1",
              log: JsonlLog | None = None):
    """Answer every A query on `sock` with answer_ip (AAAA answered empty, forcing v4).

    Browser-agnostic host mapping for the replay arm: the replay container
    mounts a resolv.conf pointing here, so ANY engine (Chromium today, WebKit
    later) resolves every corpus host to this box with no engine flags — a
    space-containing --host-resolver-rules value cannot survive the container
    env word-split, which is how the flag approach died (2026-08-13).

    With `log`, one line per answered query: {ts, peer, qname, qtype, answer}.
    Returns when the socket is closed under it; every other error on a query
    is dropped so one malformed packet cannot stop the replay. A log line that
    cannot be written is the one exception: an answered, unlogged query is a
    hole in the evidence the campaign hashes, so the responder closes its
    socket and raises. The guest then stops resolving and the campaign's :53
    owner sampler finds no owner, and the run is refused on both counts.
    """
    while True:
        try:
            data, peer = sock.recvfrom(512)
        except OSError:
            if sock.fileno() < 0:
                return
            continue
        try:
            if len(data) < 12:
                continue
            txid = data[:2]
            flags = b"\x81\x80"
            qd = data[12:]
            labels = []
            i = 0
            while i < len(qd) and qd[i] != 0:
                labels.append(qd[i + 1:i + 1 + qd[i]].decode("ascii", "replace"))
                i += qd[i] + 1
            qend = i + 1 + 4  # name + qtype/qclass
            question = qd[:qend]
            qtype = struct.unpack(">H", qd[i + 1:i + 3])[0]
            if qtype == 1:  # A
                answer = (b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 5, 4)
                          + socket.inet_aton(answer_ip))
                resp = txid + flags + struct.pack(">HHHH", 1, 1, 0, 0) + question + answer
                answered = answer_ip
            else:  # empty NOERROR (esp. AAAA -> fall back to A)
                resp = txid + flags + struct.pack(">HHHH", 1, 0, 0, 0) + question
                answered = ""
            sock.sendto(resp, peer)
        except Exception:  # noqa: BLE001 - a malformed query must not kill the server
            continue
        if log is not None:
            try:
                log.write({
                    "ts": time.time(),
                    "peer": f"{peer[0]}:{peer[1]}",
                    "qname": ".".join(labels),
                    "qtype": qtype,
                    "answer": answered,
                })
            except OSError:
                sock.close()
                raise


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=str(HERE / "corpus-live"))
    ap.add_argument("--port", type=int, default=80)
    ap.add_argument("--tls-port", type=int, default=443)
    ap.add_argument("--dns-addr", default="127.0.0.2")
    ap.add_argument("--dns-port", type=int, default=53)
    # The address DNS answers point at. Host-side replay: 127.0.0.1. In-guest
    # replay: 10.0.2.2, the pasta gateway, which maps guest connections onto
    # the host's loopback where this server listens.
    ap.add_argument("--answer-ip", default="127.0.0.1")
    ap.add_argument("--dns-log", default=None, metavar="PATH",
                    help="append one JSON line per DNS query: "
                         "ts, peer, qname, qtype, answer")
    ap.add_argument("--access-log", default=None, metavar="PATH",
                    help="append one JSON line per HTTP/HTTPS request: "
                         "ts, peer, method, host, path, status, bytes, duration_ms")
    return ap


def main():
    ap = build_parser()
    args = ap.parse_args()
    # Validate up front: inet_aton runs inside the responder's broad exception
    # handler, so a malformed address would silently drop every A query while
    # the servers keep running — the failure has to happen before any thread
    # starts.
    try:
        socket.inet_aton(args.answer_ip)
    except OSError:
        ap.error(f"--answer-ip is not a valid IPv4 address: {args.answer_ip!r}")

    Handler.exact, Handler.noquery, Handler.redirects = load_indexes(Path(args.root))
    Handler.access_log = JsonlLog(args.access_log) if args.access_log else None
    print(f"loaded {len(Handler.exact)} urls", flush=True)

    dns_log = JsonlLog(args.dns_log) if args.dns_log else None
    dns_sock = bind_dns(args.dns_addr, args.dns_port)

    # Both listeners exist before either serves: a failure on one must find
    # the other in `peers`, and neither answers a request before that.
    peers = []
    plain = ReplayServer(("127.0.0.1", args.port), Handler, peers)
    tls = ReplayServer(("127.0.0.1", args.tls_port), Handler, peers)
    # The key pair lives only until it is loaded: a fixed path under /tmp was
    # shared by every server on the box, whatever user each ran as.
    with tempfile.TemporaryDirectory(prefix="corpus-replay-cert-") as tmp:
        crt, key = selfsigned(Path(tmp))
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(str(crt), str(key))
    tls.socket = ctx.wrap_socket(tls.socket, server_side=True)

    dns_thread = threading.Thread(target=serve_dns, args=(dns_sock, args.answer_ip, dns_log),
                                  daemon=True)
    dns_thread.start()
    print(f"wildcard DNS on {args.dns_addr}:{args.dns_port} -> {args.answer_ip}", flush=True)
    install_signal_handlers(plain)
    print(f"replaying on http://127.0.0.1:{args.port} and https://127.0.0.1:{args.tls_port}", flush=True)
    return serve_http(plain, tls,
                      logs=[log for log in (Handler.access_log, dns_log) if log is not None],
                      dns=(dns_sock, dns_thread))


if __name__ == "__main__":
    sys.exit(main())
