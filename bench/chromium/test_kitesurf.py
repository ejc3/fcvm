#!/usr/bin/env python3
"""The Kitesurf arm (kitesurf.sh and its helpers), checked without an engine.

Each test names the defect it holds. Every one was run against the code without
the fix (or with the behaviour it pins mutated) and observed failing first.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.parse
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, f"{name}.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def png(width, height, pixel_of):
    """An 8-bit RGB PNG, every row filter 0."""
    raw = b"".join(b"\x00" + b"".join(bytes(pixel_of(x, y)) for x in range(width))
                   for y in range(height))

    def chunk(kind, body):
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class ReplayServer:
    """corpus_serve.py on ephemeral ports, as a context manager."""

    def __init__(self, *extra):
        self.extra = list(extra)
        self.tmp = tempfile.mkdtemp()
        self.http, self.tls = free_port(), free_port()

    def __enter__(self):
        self.proc = subprocess.Popen(
            [sys.executable, os.path.join(HERE, "corpus_serve.py"), "--port", str(self.http),
             "--tls-port", str(self.tls), "--dns-addr", "127.0.0.1", "--dns-port", "0",
             *self.extra], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            line = self.proc.stdout.readline()
            if "replaying on" in line:
                return self
            if not line and self.proc.poll() is not None:
                raise RuntimeError("corpus_serve exited during start-up")
        raise RuntimeError("corpus_serve never started")

    def __exit__(self, *exc):
        self.proc.terminate()
        self.proc.wait(timeout=30)
        shutil.rmtree(self.tmp, ignore_errors=True)


def tls_handshake(port, host, cafile):
    ctx = ssl.create_default_context(cafile=cafile)
    with socket.create_connection(("127.0.0.1", port), timeout=10) as raw:
        with ctx.wrap_socket(raw, server_hostname=host):
            return True


class ReplayCertificateForVerifyingClients(unittest.TestCase):
    """workerd verifies certificates and has no switch to stop it. The replay's
    default certificate names `DNS:*`, which matches no host, so the Kitesurf
    engine could load no corpus page over HTTPS. --tls-ca-out serves a leaf naming
    every captured host, signed by a CA it writes for the engine to trust."""

    def test_every_captured_host_verifies_against_the_written_ca(self):
        serve = load("corpus_serve")
        exact, noquery, redirects = serve.load_indexes(serve.Path(os.path.join(HERE, "corpus-live")))
        hosts = serve.replay_hosts(exact, noquery, redirects)
        self.assertIn("example.com", hosts)
        self.assertIn("assets.guim.co.uk", hosts)
        with ReplayServer("--tls-ca-out", os.path.join(tempfile.gettempdir(), f"ca-{os.getpid()}.crt")) as s:
            ca = os.path.join(tempfile.gettempdir(), f"ca-{os.getpid()}.crt")
            try:
                for host in hosts:
                    self.assertTrue(tls_handshake(s.tls, host, ca), host)
                with self.assertRaises(ssl.SSLCertVerificationError):
                    tls_handshake(s.tls, "not-in-the-capture.example", ca)
            finally:
                os.unlink(ca)

    def test_the_default_certificate_stays_what_the_other_arms_get(self):
        """Without the flag nothing changes: one self-signed certificate, which a
        verifying client refuses (and Chromium, with --ignore-certificate-errors,
        accepts as before)."""
        with ReplayServer() as s:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            with socket.create_connection(("127.0.0.1", s.tls), timeout=10) as raw:
                with ctx.wrap_socket(raw, server_hostname="example.com") as tls:
                    der = tls.getpeercert(binary_form=True)
            pem = ssl.DER_cert_to_PEM_cert(der)
            text = subprocess.run(["openssl", "x509", "-noout", "-text"], input=pem,
                                  capture_output=True, text=True, check=True).stdout
            self.assertIn("DNS:*, DNS:localhost", text)
            self.assertNotIn("DNS:example.com", text)


class ConnectionClose(unittest.TestCase):
    """The replay answers HTTP/1.0 and closes after each reply; workerd's client
    reused those connections and failed about half the requests with "Network
    connection lost" (2 of 14 pages completed in the first box run, 14 of 14
    with the header). --connection-close says so on every reply; without the
    flag the reply is unchanged."""

    def reply_headers(self, *extra):
        with ReplayServer(*extra) as s:
            with socket.create_connection(("127.0.0.1", s.http), timeout=10) as conn:
                conn.sendall(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
                data = b""
                while b"\r\n\r\n" not in data:
                    chunk = conn.recv(4096)
                    if not chunk:
                        break
                    data += chunk
        return data.split(b"\r\n\r\n", 1)[0].decode().lower()

    def test_flag_adds_the_header(self):
        self.assertIn("\r\nconnection: close", self.reply_headers("--connection-close"))

    def test_default_reply_has_no_connection_header(self):
        self.assertNotIn("\r\nconnection:", self.reply_headers())


class BlankScreenshots(unittest.TestCase):
    """A page that comes back as one colour is a failure, not a fast render."""

    def test_uniform_image_is_blank(self):
        rc = load("render_check")
        stats = rc.colour_stats(png(40, 30, lambda x, y: (255, 255, 255)))
        self.assertTrue(stats["blank"])
        self.assertEqual(stats["distinct_colors"], 1)

    def test_text_like_image_is_not_blank(self):
        rc = load("render_check")
        # A white page with a dark band covering 5% of it, as a line of text does.
        stats = rc.colour_stats(png(40, 30, lambda x, y: (20, 20, 20) if y < 2 and x < 23 else (255, 255, 255)))
        self.assertFalse(stats["blank"])

    def test_every_png_filter_decodes(self):
        rc = load("render_check")
        width, height = 5, 5
        pixels = [[((x * 37 + y * 11) % 256, (x * 5) % 256, (y * 70) % 256) for x in range(width)]
                  for y in range(height)]
        # Encode with each filter type in turn, by hand.
        rows, prev = [], [0] * (width * 3)
        for y in range(height):
            line = [c for p in pixels[y] for c in p]
            ftype = y % 5
            enc = []
            for i, v in enumerate(line):
                a = line[i - 3] if i >= 3 else 0
                b = prev[i]
                c = prev[i - 3] if i >= 3 else 0
                if ftype == 0:
                    pred = 0
                elif ftype == 1:
                    pred = a
                elif ftype == 2:
                    pred = b
                elif ftype == 3:
                    pred = (a + b) >> 1
                else:
                    p = a + b - c
                    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                    pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                enc.append((v - pred) & 0xFF)
            rows.append(bytes([ftype] + enc))
            prev = line
        raw = b"".join(rows)

        def chunk(kind, body):
            return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
        data = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
        _w, _h, _c, decoded = rc.png_pixels(data)
        for y in range(height):
            self.assertEqual(list(decoded[y]), [c for p in pixels[y] for c in p], f"row {y}")


class Measurement(unittest.TestCase):
    """Kitesurf fails some requests by design of the comparison: the loop must
    record them and go on (hostcdp.sh stops at the first), and the summary must
    count a request on a page that did not complete in the check as a failure."""

    def fake_cgroup(self, root):
        os.makedirs(root)
        with open(os.path.join(root, "cpu.stat"), "w") as h:
            h.write("usage_usec 1000\n")
        with open(os.path.join(root, "memory.current"), "w") as h:
            h.write("4096\n")
        with open(os.path.join(root, "cgroup.procs"), "w") as h:
            h.write("")

    def test_loop_records_failures_and_continues(self):
        km = load("kitesurf_measure")
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp, True)
        cgroup = os.path.join(tmp, "cg")
        self.fake_cgroup(cgroup)
        run_json = os.path.join(tmp, "run.json")
        with open(run_json, "w") as h:
            h.write("{}")
        calls = []

        class FakeDrive:
            @staticmethod
            def drive(ns):
                calls.append((ns.url, ns.op, ns.format, ns.quality, ns.nav_timing))
                if len(calls) % 2:
                    return {"ok": False, "error": "RuntimeError: x", "stage": "screenshot"}
                return {"ok": True, "image_sha256": "a"}

        original = km.load_module
        km.load_module = lambda name, path: FakeDrive
        try:
            out = os.path.join(tmp, "k.jsonl")
            km.measure(argparse.Namespace(
                address="127.0.0.1:1", urls="https://a/,https://b/", arms="cdp,html",
                reps=2, warmup=1, cgroup=cgroup, loadavg_file="/proc/loadavg",
                run_json=run_json, out=out))
        finally:
            km.load_module = original
        rows = [json.loads(line) for line in open(out)]
        self.assertEqual(len(rows), 6, "three reps of two arms, failures included")
        self.assertEqual([r["ok"] for r in rows], [False, True] * 3)
        self.assertEqual([r["url"] for r in rows],
                         ["https://a/"] * 2 + ["https://b/"] * 2 + ["https://a/"] * 2)
        self.assertEqual([r["warmup"] for r in rows], [True, True] + [False] * 4)
        # The fcvm arm's driver arguments: JPEG q80, navigation timing on.
        self.assertTrue(all(c[2:] == ("jpeg", 80, True) for c in calls))
        self.assertEqual([c[1] for c in calls[:2]], ["screenshot", "html"])

    def test_summary_counts_pages_that_failed_the_check_as_failures(self):
        km = load("kitesurf_measure")
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp, True)
        rows = []
        for rep in range(4):
            for url in ("https://good/", "https://blank/"):
                rows.append({"run_json_sha256": "x", "engine": "kitesurf", "arm": "cdp", "rep": rep,
                             "warmup": False, "url": url, "ok": True, "wall_ms": 100.0 if url == "https://good/" else 5.0,
                             "loadavg1": 0.1, "cgroup_cpu_usec_before": 0, "cgroup_cpu_usec_after": 50000,
                             "proc_cpu_ms": {"workerd": 40.0}, "memory_current_after": 1,
                             "driver": json.dumps({"ok": True, "image_sha256": "h"})})
        jsonl = os.path.join(tmp, "k.jsonl")
        with open(jsonl, "w") as h:
            for r in rows:
                h.write(json.dumps(r) + "\n")
        os.makedirs(os.path.join(tmp, "c", "kitesurf"))
        with open(os.path.join(tmp, "c", "kitesurf", "correctness.jsonl"), "w") as h:
            h.write(json.dumps({"url": "https://good/", "site": "g", "completed": True}) + "\n")
            h.write(json.dumps({"url": "https://blank/", "site": "b", "completed": False, "blank": True}) + "\n")
        out = os.path.join(tmp, "s.json")
        km.summarize(argparse.Namespace(jsonl=jsonl, run_json="", memory_peak="10",
                                        correctness=os.path.join(tmp, "c"), out=out))
        s = json.load(open(out))
        cdp = s["arms"]["cdp"]
        self.assertEqual((cdp["attempts"], cdp["succeeded"], cdp["failed"]), (8, 4, 4))
        self.assertEqual(cdp["p50_ms"], 100.0, "the fast blank page must not pull the median down")
        self.assertEqual(cdp["per_page"]["https://blank/"]["succeeded"], 0)
        self.assertEqual(s["pages_completed"], {"kitesurf": 1})


class Seal(unittest.TestCase):
    """The run refuses a Kitesurf tree that changed after its build; wrangler's own
    run-time state must not count as a change. The first box run failed its
    after-run check because wrangler writes packages/engine/.wrangler/, and the
    seal skipped only a top-level .wrangler."""

    def tree(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp, True)
        src = os.path.join(tmp, "src")
        os.makedirs(os.path.join(src, "packages", "engine"))
        with open(os.path.join(src, "packages", "engine", "index.ts"), "w") as h:
            h.write("x")
        env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid")
        for args in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "t"]):
            subprocess.run(["git", "-C", src, *args], check=True, env=env, capture_output=True)
        os.makedirs(os.path.join(src, "packages", "core", "pkg"))
        with open(os.path.join(src, "packages", "core", "pkg", "core.wasm"), "wb") as h:
            h.write(b"\0asm")
        os.makedirs(os.path.join(src, "node_modules", ".pnpm",
                                 "@cloudflare+workerd-linux-x@1", "node_modules", "w", "bin"))
        with open(os.path.join(src, "node_modules", ".pnpm", "@cloudflare+workerd-linux-x@1",
                               "node_modules", "w", "bin", "workerd"), "wb") as h:
            h.write(b"ELF")
        rev = subprocess.run(["git", "-C", src, "rev-parse", "HEAD"], capture_output=True,
                             text=True, check=True).stdout.strip()
        seal = os.path.join(tmp, "BUILD.json")
        subprocess.run([sys.executable, os.path.join(HERE, "kitesurf_seal.py"), "write", "--src", src,
                        "--out", seal, "--rev", rev, "--node", "v", "--pnpm", "v", "--bazel", "v",
                        "--arch", "a"], check=True, capture_output=True)
        return src, seal, rev

    def verify(self, src, seal, rev):
        return subprocess.run([sys.executable, os.path.join(HERE, "kitesurf_seal.py"), "verify",
                               "--src", src, "--seal", seal, "--rev", rev], capture_output=True, text=True)

    def test_wrangler_state_anywhere_is_not_a_change(self):
        src, seal, rev = self.tree()
        state = os.path.join(src, "packages", "engine", ".wrangler", "state", "v3")
        os.makedirs(state)
        with open(os.path.join(state, "metadata.sqlite"), "w") as h:
            h.write("state")
        r = self.verify(src, seal, rev)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_a_changed_build_output_is_refused(self):
        src, seal, rev = self.tree()
        with open(os.path.join(src, "packages", "core", "pkg", "core.wasm"), "ab") as h:
            h.write(b"!")
        r = self.verify(src, seal, rev)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("changed since it was sealed", r.stderr)

    def test_another_revision_is_refused(self):
        src, seal, _rev = self.tree()
        r = self.verify(src, seal, "0" * 40)
        self.assertNotEqual(r.returncode, 0)


class Wiring(unittest.TestCase):
    """corpus_extra.sh runs the arm alone, with the replay it needs, from sealed sources."""

    SRC = open(os.path.join(HERE, "corpus_extra.sh")).read()

    def phases(self, phases):
        block = self.SRC[self.SRC.index("validate_phases() {"):]
        block = block[:block.index("\n}\n") + 3]
        return subprocess.run(["bash", "-c", block + "\nvalidate_phases"],
                              env=dict(os.environ, PHASES=phases), capture_output=True, text=True)

    def test_kitesurf_runs_alone(self):
        self.assertEqual(self.phases("kitesurf").returncode, 0)
        for phases in ("hostcdp,kitesurf", "kitesurf,memory"):
            r = self.phases(phases)
            self.assertNotEqual(r.returncode, 0, phases)
            self.assertIn("runs alone", r.stderr)

    def test_the_kitesurf_replay_gets_its_two_flags(self):
        self.assertIn('CORPUS_SERVE_ARGS="${CORPUS_SERVE_ARGS:-} --tls-ca-out $RESULTS/replay-ca.crt '
                      '--connection-close"', self.SRC)

    def test_every_source_the_arm_runs_is_staged_and_recorded(self):
        stage = self.SRC[self.SRC.index("local sources=("):]
        stage = stage[:stage.index(")")]
        prov = self.SRC[self.SRC.index("for f in corpus_extra.sh"):]
        prov = prov[:prov.index("; do")]
        for name in ("kitesurf.sh", "kitesurf_measure.py", "kitesurf_seal.py", "kitesurf_pins.sh",
                     "kitesurf_build.sh", "render_check.py", "cdpdrive.py", "render.py", "report.py"):
            self.assertIn(name, stage)
            self.assertIn(name, prov)
        ks = open(os.path.join(HERE, "kitesurf.sh")).read()
        for helper in ("kitesurf_pins.sh", "kitesurf_seal.py", "kitesurf_measure.py", "render_check.py",
                       "cdpdrive.py"):
            self.assertIn(f'"$HERE/{helper}"', ks, helper)

    def test_build_and_run_read_one_set_of_pins(self):
        for script in ("kitesurf.sh", "kitesurf_build.sh"):
            self.assertIn('. "$HERE/kitesurf_pins.sh"', open(os.path.join(HERE, script)).read())
        pins = open(os.path.join(HERE, "kitesurf_pins.sh")).read()
        rev = [l.split("=", 1)[1] for l in pins.splitlines() if l.startswith("KITESURF_REV=")]
        self.assertEqual(len(rev), 1)
        self.assertRegex(rev[0], r"^[0-9a-f]{40}$")


if __name__ == "__main__":
    unittest.main()
