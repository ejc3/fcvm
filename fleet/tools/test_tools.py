"""Tests for the fleet/tools operator scripts: the /live client (fleetlive.py), aggrun.py's
stream helpers (aggstream.py) and the run scripts' wait for the coordinator (lib.sh).
No GPU, coordinator or image library: fake servers on loopback ports the kernel picks.

    make -C fleet/tools test
"""
import os
import socket
import subprocess
import sys
import threading
import time
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "renderer", "tools"))
sys.path.insert(0, HERE)

import aggstream  # noqa: E402
import fleetlive  # noqa: E402


def server(handle):
    """A loopback server that runs `handle(conn)` for one connection; returns its port."""
    ls = socket.socket()
    ls.bind(("127.0.0.1", 0))
    ls.listen(1)

    def run():
        conn, _ = ls.accept()
        try:
            handle(conn)
        finally:
            conn.close()
            ls.close()

    threading.Thread(target=run, daemon=True).start()
    return ls.getsockname()[1]


def read_request(conn):
    head = b""
    while b"\r\n\r\n" not in head:
        c = conn.recv(1)
        if not c:
            break
        head += c
    return head


def free_port():
    """A port nothing listens on: bound by the kernel's choice, then released."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def in_thread(fn, timeout):
    """Run fn in a thread; returns (finished, its exception or None)."""
    out = {}

    def run():
        try:
            fn()
            out["error"] = None
        except BaseException as e:  # noqa: BLE001 - SystemExit included
            out["error"] = e

    t = threading.Thread(target=run, daemon=True)
    t.start()
    t.join(timeout)
    return not t.is_alive(), out.get("error")


class FleetLive(unittest.TestCase):
    def test_a_handshake_the_coordinator_closes_raises(self):
        # The coordinator reads the upgrade request and closes before answering it all.
        def handle(conn):
            read_request(conn)
            conn.sendall(b"HTTP/1.1 101 Switching")

        fleetlive.connect_to(f"ws://127.0.0.1:{server(handle)}/live")
        done, err = in_thread(fleetlive.Live, 5)
        self.assertTrue(done, "Live() still waits for the rest of a handshake on a closed socket")
        self.assertIsInstance(err, ConnectionError)

    def test_an_intent_whose_connection_closes_raises(self):
        # The coordinator accepts the WebSocket, reads the intent and goes away.
        def handle(conn):
            read_request(conn)
            conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                         b"Connection: Upgrade\r\n\r\n")
            conn.recv(4096)

        fleetlive.connect_to(f"ws://127.0.0.1:{server(handle)}/live")
        live = fleetlive.Live()
        self.addCleanup(live.close)
        done, err = in_thread(lambda: live.intent("relaunch", {"tile": "slot-1"}), 5)
        self.assertTrue(done, "intent() still waits for a receipt on a closed connection")
        self.assertIsInstance(err, ConnectionError)


class AggStream(unittest.TestCase):
    def test_no_listening_process_exits_with_the_reason(self):
        port = free_port()
        with self.assertRaises(SystemExit) as e:
            aggstream.require_listening_pid(port)
        self.assertIn(str(port), str(e.exception.code))

    def test_a_listening_process_of_this_user_is_found(self):
        ls = socket.socket()
        ls.bind(("127.0.0.1", 0))
        ls.listen(1)
        try:
            self.assertEqual(aggstream.require_listening_pid(ls.getsockname()[1]), os.getpid())
        finally:
            ls.close()

    def recorder(self, frames, alive):
        stop = threading.Event()
        t = threading.Thread(target=stop.wait, daemon=True)
        if alive:
            t.start()
            self.addCleanup(stop.set)
        return types.SimpleNamespace(frames=frames, t=t)

    def test_waiting_for_a_frame_ends_when_the_stream_ends(self):
        rec = self.recorder([], alive=False)
        done, err = in_thread(lambda: aggstream.wait_for_t(rec, 10.0, 60), 5)
        self.assertTrue(done, "the wait goes on after the recorder's stream ended")
        self.assertIsInstance(err, SystemExit)
        self.assertIn("ended", str(err.code))

    def test_waiting_for_a_frame_ends_at_its_deadline(self):
        rec = self.recorder([({"t": 1.0}, b"")], alive=True)
        t0 = time.time()
        done, err = in_thread(lambda: aggstream.wait_for_t(rec, 10.0, 0.5), 5)
        self.assertTrue(done, "the wait has no deadline")
        self.assertIsInstance(err, SystemExit)
        self.assertGreaterEqual(time.time() - t0, 0.5)

    def test_waiting_for_a_frame_returns_once_it_arrives(self):
        rec = self.recorder([({"t": 9.0}, b""), ({"t": 10.0}, b"")], alive=True)
        aggstream.wait_for_t(rec, 10.0, 5)


def wait_listening(port, secs, pid=""):
    return subprocess.run(
        ["bash", "-c", f'. "{HERE}/lib.sh"; wait_listening 127.0.0.1 {port} {secs} {pid}'],
        capture_output=True, text=True, timeout=30)


class RunScripts(unittest.TestCase):
    def test_wait_listening_returns_once_the_port_listens(self):
        port = free_port()
        ls = socket.socket()

        def bind_later():
            time.sleep(1.0)
            ls.bind(("127.0.0.1", port))
            ls.listen(8)

        threading.Thread(target=bind_later, daemon=True).start()
        try:
            r = wait_listening(port, 10)
            self.assertEqual(r.returncode, 0, r.stderr)
        finally:
            ls.close()

    def test_wait_listening_fails_after_its_time(self):
        r = wait_listening(free_port(), 1)
        self.assertEqual(r.returncode, 1)
        self.assertIn("nothing listens", r.stderr)

    def test_wait_listening_fails_when_the_server_exits(self):
        p = subprocess.Popen(["true"])
        p.wait()
        t0 = time.time()
        r = wait_listening(free_port(), 30, p.pid)
        self.assertEqual(r.returncode, 1)
        self.assertIn("exited", r.stderr)
        self.assertLess(time.time() - t0, 10)

    def test_the_run_scripts_wait_for_the_coordinator_instead_of_sleeping(self):
        for name in ("gpurun.sh", "aggrun.sh"):
            with open(os.path.join(HERE, name)) as f:
                text = f.read()
            lines = [line.strip() for line in text.splitlines()]
            self.assertNotIn("sleep 2", lines, f"{name} sleeps a fixed time for the coordinator")
            self.assertIn('. "$SRC/fleet/tools/lib.sh"', lines, name)
            self.assertIn("if ! wait_listening 127.0.0.1 20000 120 $CP; then", lines, name)


if __name__ == "__main__":
    unittest.main()
