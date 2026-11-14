#!/usr/bin/env python3
"""browserrun.py, the hosted Browser Run arms, against a fake api.cloudflare.com.

Each test was observed failing against a mutation of the behaviour it pins.
"""

import argparse
import http.client
import importlib.util
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
TOKEN = "secret-token-value-never-written"
ACCOUNT = "0123456789abcdef0123456789abcdef"


def load():
    spec = importlib.util.spec_from_file_location("browserrun", os.path.join(HERE, "browserrun.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def png(colour_of):
    w, h = 20, 10
    raw = b"".join(b"\x00" + b"".join(bytes(colour_of(x, y)) for x in range(w)) for y in range(h))

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


PAGE = png(lambda x, y: (0, 0, 0) if y < 3 and x < 12 else (255, 255, 255))
BLANK = png(lambda x, y: (255, 255, 255))
JPEG = b"\xff\xd8\xff\xe0jpeg"


class Reply(io.BytesIO):
    def __init__(self, status, body, headers):
        super().__init__(body)
        self.status = status
        self.headers = FakeHeaders(headers)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class FakeHeaders(dict):
    def items(self):
        return list(super().items())


class FakeCloudflare:
    """Answers like Browser Run; `script` overrides replies per (engine, op, url)."""

    def __init__(self, script=None):
        self.script = script or {}
        self.calls = []

    def open(self, req, timeout):
        body = json.loads(req.data)
        engine = "kitesurf" if req.full_url.endswith("?browser=kitesurf") else "chromium"
        op = req.full_url.split("?")[0].rsplit("/", 1)[1]
        self.calls.append((engine, op, body, dict(req.header_items())))
        key = (engine, op, body["url"])
        if key in self.script and self.script[key]:
            status, payload, headers = self.script[key].pop(0)
        elif op == "content":
            status, payload, headers = 200, json.dumps({"success": True, "result": "<html></html>"}).encode(), {}
        elif body.get("screenshotOptions", {}).get("type") == "jpeg":
            status, payload, headers = 200, JPEG, {}
        else:
            status, payload, headers = 200, PAGE, {}
        headers = {"cf-ray": "8f00000000000000-IAD", "X-Browser-Ms-Used": "123", **headers}
        if status >= 400:
            raise urllib.error.HTTPError(req.full_url, status, "err", FakeHeaders(headers), io.BytesIO(payload))
        return Reply(status, payload, headers)


def args(out, **kw):
    base = dict(out=out, engines="chromium,kitesurf", ops="screenshot,content", reps=14, warmup=0,
                seed=1, image_type="jpeg", timeout=5.0, max_retries=3, max_retry_wait=2.0,
                client_location="test-region")
    base.update(kw)
    return argparse.Namespace(**base)


class HostedArms(unittest.TestCase):
    def setUp(self):
        self.br = load()
        self.parent = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.parent, True)
        self.tmp = os.path.join(self.parent, "run")
        report = self.br.load("report")
        self.urls = list(report.KITESURF_CORPUS.values())

    def rows(self):
        return [json.loads(l) for l in open(os.path.join(self.tmp, "browserrun.jsonl"))]

    def test_both_engines_both_ops_every_page_and_kitesurf_by_query(self):
        cf = FakeCloudflare()
        self.br.run(args(self.tmp), TOKEN, ACCOUNT, opener=cf, sleep=lambda s: None)
        rows = self.rows()
        self.assertEqual(len(rows), 14 * 4)
        self.assertEqual({(r["engine"], r["op"]) for r in rows},
                         {(e, o) for e in ("chromium", "kitesurf") for o in ("screenshot", "content")})
        self.assertEqual({r["url"] for r in rows}, set(self.urls))
        kite = [c for c in cf.calls if c[0] == "kitesurf"]
        self.assertTrue(kite)
        measured_shots = [c for c in cf.calls if c[1] == "screenshot" and c[2].get("screenshotOptions")]
        self.assertTrue(all(c[2]["screenshotOptions"] == {"type": "jpeg", "quality": 80} for c in measured_shots))
        self.assertTrue(all(r["colo"] == "IAD" and r["browser_headers"] == {"X-Browser-Ms-Used": "123"}
                            for r in rows))
        self.assertTrue(all(isinstance(r["loadavg1"], float) for r in rows), "the client's load")
        run = json.load(open(os.path.join(self.tmp, "run.json")))
        self.assertEqual(run["client_location"], "test-region")
        self.assertIn("via_https_proxy", run)

    def test_the_token_is_sent_and_never_written(self):
        cf = FakeCloudflare()
        self.br.run(args(self.tmp, reps=2), TOKEN, ACCOUNT, opener=cf, sleep=lambda s: None)
        self.assertTrue(all(c[3].get("Authorization") == f"Bearer {TOKEN}" for c in cf.calls))
        for root, _dirs, files in os.walk(self.tmp):
            for name in files:
                with open(os.path.join(root, name), "rb") as handle:
                    self.assertNotIn(TOKEN.encode(), handle.read(), name)

    def test_default_credentials_are_read_for_the_local_host_only(self):
        for worker, wanted in ((False, "cloudflare-browser-run"),
                               (True, "cloudflare-browser-run-worker")):
            with self.subTest(worker=worker):
                def read(argv, **kwargs):
                    self.assertEqual(argv[:3], ["aws", "secretsmanager", "get-secret-value"])
                    self.assertEqual(argv[argv.index("--secret-id") + 1], wanted)
                    self.assertEqual(argv[argv.index("--version-stage") + 1], "AWSCURRENT")
                    self.assertNotIn(TOKEN, argv)
                    self.assertTrue(kwargs["capture_output"])
                    return subprocess.CompletedProcess(argv, 0, json.dumps({
                        "account_id": ACCOUNT, "api_token": TOKEN,
                    }))
                self.assertEqual(self.br.credentials({}, worker, run=read), (TOKEN, ACCOUNT))

    def test_an_override_needs_both_fields_and_never_reads_aws(self):
        def unexpected(*args, **kwargs):
            self.fail("read the secret with an explicit override")
        with self.assertRaisesRegex(SystemExit, "supply both"):
            self.br.credentials({"CLOUDFLARE_API_TOKEN": TOKEN}, True, run=unexpected)
        self.assertEqual(self.br.credentials({"CLOUDFLARE_API_TOKEN": TOKEN, "CLOUDFLARE_ACCOUNT_ID": ACCOUNT},
                                             True, run=unexpected), (TOKEN, ACCOUNT))

    def test_an_aws_failure_does_not_echo_data_or_switch_hosts(self):
        requests = []

        def denied(argv, **kwargs):
            requests.append(argv[argv.index("--secret-id") + 1])
            raise subprocess.CalledProcessError(1, argv, output=TOKEN, stderr=TOKEN)

        with self.assertRaisesRegex(SystemExit, "could not load the cloudflare-browser-run-worker credential") as err:
            self.br.credentials({}, True, run=denied)
        self.assertNotIn(TOKEN, str(err.exception))
        self.assertEqual(requests, ["cloudflare-browser-run-worker"])

    def test_the_direct_opener_ignores_proxy_and_declines_redirects(self):
        with patch.dict(os.environ, {"https_proxy": "http://not-the-worker:123"}, clear=True):
            opener = self.br.direct_opener()
        self.assertFalse(any(isinstance(h, urllib.request.ProxyHandler) and h.proxies for h in opener.handlers))
        handlers = [h for h in opener.handlers if isinstance(h, urllib.request.HTTPRedirectHandler)]
        self.assertEqual(len(handlers), 1)
        self.assertIsNone(handlers[0].redirect_request(None, None, 302, "", {}, "https://elsewhere.invalid"))

    def test_an_error_reply_cannot_echo_the_bearer_to_the_results(self):
        cf = FakeCloudflare({
            ("chromium", "content", self.urls[0]): [
                (403, TOKEN.encode(), {"X-Browser-Ms-Used": TOKEN}),
            ],
        })
        self.br.run(args(self.tmp, reps=1, engines="chromium", ops="content"), TOKEN, ACCOUNT,
                    opener=cf, sleep=lambda _: None)
        for root, _dirs, files in os.walk(self.tmp):
            for name in files:
                with open(os.path.join(root, name), "rb") as handle:
                    self.assertNotIn(TOKEN.encode(), handle.read(), name)

    def test_a_429_is_waited_out_and_not_timed_as_a_failure(self):
        url = self.urls[0]
        cf = FakeCloudflare({("chromium", "screenshot", url): [
            None, (429, b"{}", {"Retry-After": "1"})]})
        # The first reply for that key is the correctness pass's; make it normal.
        cf.script[("chromium", "screenshot", url)][0] = (200, PAGE, {})
        waits = []
        self.br.run(args(self.tmp, reps=1, engines="chromium", ops="screenshot"), TOKEN, ACCOUNT,
                    opener=cf, sleep=waits.append)
        (row,) = self.rows()
        self.assertTrue(row["ok"])
        self.assertEqual(waits, [1.0])
        self.assertEqual(row["rate_limit_wait_s"], 1.0)

    def test_failures_and_blank_pages_are_failures_and_the_run_goes_on(self):
        blank_url, broken_url = self.urls[0], self.urls[1]
        cf = FakeCloudflare({
            ("kitesurf", "screenshot", blank_url): [(200, BLANK, {})],   # correctness: blank
            ("kitesurf", "content", broken_url): [None, (500, b'{"errors":["x"]}', {})],
        })
        cf.script[("kitesurf", "content", broken_url)][0] = (
            200, json.dumps({"success": True, "result": "<html></html>"}).encode(), {})
        self.br.run(args(self.tmp, reps=14), TOKEN, ACCOUNT, opener=cf, sleep=lambda s: None)
        rows = self.rows()
        self.assertEqual(len(rows), 56, "the run went on past the failures")
        summary = json.load(open(os.path.join(self.tmp, "summary.json")))
        ks = summary["cells"]["kitesurf/screenshot"]["per_page"][blank_url]
        self.assertEqual((ks["attempts"], ks["succeeded"], ks["page_completed_in_check"]), (1, 0, False))
        kc = summary["cells"]["kitesurf/content"]["per_page"][broken_url]
        self.assertEqual((kc["succeeded"], kc["statuses"]), (0, [500]))
        self.assertEqual(summary["cells"]["chromium/screenshot"]["succeeded"], 14)
        shots = os.listdir(os.path.join(self.tmp, "correctness", "kitesurf"))
        self.assertIn("correctness.jsonl", shots)
        self.assertTrue(any(name.endswith(".png") for name in shots))

    def test_a_reused_output_directory_is_refused(self):
        """Codex, #1888: a second run into one directory appended its rows to the
        first's and summarized both under the second run's metadata."""
        self.br.run(args(self.tmp, reps=1), TOKEN, ACCOUNT, opener=FakeCloudflare(), sleep=lambda s: None)
        with self.assertRaises(SystemExit) as refused:
            self.br.run(args(self.tmp, reps=2), TOKEN, ACCOUNT, opener=FakeCloudflare(), sleep=lambda s: None)
        self.assertIn("exists", str(refused.exception))
        self.assertEqual(len(self.rows()), 4, "the first run's rows are untouched")

    def test_a_reply_cut_short_is_a_failed_request_and_the_run_goes_on(self):
        """Codex, #1888: http.client.IncompleteRead is not an OSError, escaped
        post() and ended the run."""
        url = self.urls[0]

        class Cut(Reply):
            def read(self, *a):
                raise http.client.IncompleteRead(b"partial", 100)
        cf = FakeCloudflare()
        original = cf.open
        state = {"n": 0}

        def open_(req, timeout):
            body = json.loads(req.data)
            if body["url"] == url and "screenshotOptions" in body and state["n"] == 0:
                state["n"] += 1
                return Cut(200, b"", {})
            return original(req, timeout)
        cf.open = open_
        self.br.run(args(self.tmp, reps=14, engines="chromium", ops="screenshot"), TOKEN, ACCOUNT,
                    opener=cf, sleep=lambda s: None)
        rows = self.rows()
        self.assertEqual(len(rows), 14)
        cut = [r for r in rows if r["url"] == url]
        self.assertEqual((cut[0]["ok"], cut[0]["status"]), (False, 0))
        self.assertIn("IncompleteRead", cut[0]["detail"])

    def test_judge_rejects_what_is_not_a_render(self):
        j = self.br.judge
        self.assertEqual(j("screenshot", "jpeg", 200, PAGE)[0], False)
        self.assertEqual(j("screenshot", "png", 200, PAGE)[0], True)
        self.assertEqual(j("content", "jpeg", 200, b'{"success":true,"result":"<p>"}')[0], False)
        self.assertEqual(j("content", "jpeg", 200, b'{"success":false,"result":"<html></html>"}')[0], False)
        self.assertEqual(j("screenshot", "jpeg", 503, JPEG)[0], False)
        self.assertEqual(j("screenshot", "jpeg", 0, b"URLError: x")[0], False)

    def test_endpoint(self):
        self.assertEqual(self.br.endpoint(ACCOUNT, "screenshot", "kitesurf"),
                         f"https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}/browser-run/screenshot?browser=kitesurf")
        self.assertEqual(self.br.endpoint(ACCOUNT, "content", "chromium"),
                         f"https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}/browser-run/content")


if __name__ == "__main__":
    unittest.main()
