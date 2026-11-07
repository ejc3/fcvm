#!/usr/bin/env python3
"""Tests for render.py: the statistics the page prints, the public-data check, and that every recorded sample
reaches the page in its recorded order with its recorded time.

    python3 -m unittest bench/boot-race/test_render.py
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import render  # noqa: E402


def write_results(d, samples, reports=None):
    with open(os.path.join(d, "setup.json"), "w") as f:
        json.dump({"region": "us-east-2", "rows": [["Region", "us-east-2"]], "checks": []}, f)
    with open(os.path.join(d, "samples.jsonl"), "w") as f:
        for s in samples:
            f.write(json.dumps(s) + "\n")
    if reports is not None:
        with open(os.path.join(d, "lambda-reports.json"), "w") as f:
            json.dump(reports, f)


def sample(lane, rnd, t, ms, proc="p", boot="b", **kw):
    return {"lane": lane, "round": rnd, "ok": True, "t_start_unix_ms": t, "ms": ms,
            "answer": {"proc_id": proc, "boot_id": boot}, **kw}


class Stats(unittest.TestCase):
    def test_p95_is_nearest_rank(self):
        xs = list(range(1, 21))  # 20 samples: rank ceil(19) = 19
        self.assertEqual(render.p95(xs), 19)
        self.assertEqual(render.p95([5.0]), 5.0)
        self.assertEqual(render.p95(list(range(1, 101))), 95)
        self.assertEqual(render.p99(list(range(1, 23))), 22)  # 22 samples: p99 is the slowest

    def test_median_of_even_count_is_the_mean_of_the_middle_two(self):
        self.assertEqual(render.median([1, 2, 3, 10]), 2.5)


class Build(unittest.TestCase):
    def test_samples_keep_recorded_order_time_and_value(self):
        with tempfile.TemporaryDirectory() as d:
            recs = [
                sample("fcvm-file", 0, 1000, 210.04, proc="a"),
                sample("ec2-launch", 0, 2000, 30000.0),
                sample("fcvm-file", 1, 40000, 190.0, proc="b"),
                {"lane": "fcvm-file", "round": 2, "ok": False, "error": "TimeoutError: x", "t_failed_unix_ms": 50000},
            ]
            write_results(d, recs)
            data = render.build(d)
            fc = next(l for l in data["lanes"] if l["id"] == "fcvm-file")
            self.assertEqual([(s["i"], s["t"], s["ms"]) for s in fc["samples"]], [(1, 1000, 210.0), (2, 40000, 190.0)])
            self.assertEqual(fc["n"], 2)
            self.assertEqual(fc["failures"], 1)
            self.assertEqual(fc["median_ms"], 200.0)
            self.assertEqual(fc["checks"]["new_proc"], 2)
            self.assertEqual(data["rounds"], 3)

    def test_init_duration_is_joined_by_request_id(self):
        with tempfile.TemporaryDirectory() as d:
            recs = [sample("lambda-cold", 0, 1, 120.0, request_id="r1"), sample("lambda-cold", 1, 2, 130.0, request_id="r2")]
            write_results(d, recs, {"r1": {"init_duration": "30.12 ms"}, "r2": {"init_duration": None}})
            lc = render.build(d)["lanes"][0]
            self.assertEqual(lc["checks"]["report"], 2)
            self.assertEqual(lc["checks"]["init_duration"], 1)
            self.assertEqual(lc["samples"][0]["init_ms"], 30.12)


class Public(unittest.TestCase):
    def test_refuses_ids_and_addresses(self):
        for bad in ["i-0123456789abcdef0", "ami-0123456789abcdef0", "123456789012", "10.12.1.5", "arn:aws:iam::x"]:
            with self.assertRaises(SystemExit, msg=bad):
                render.check_public(json.dumps({"x": bad}))

    def test_allows_unix_ms_and_versions(self):
        render.check_public(json.dumps({"t": 1793851066938, "ami": "al2023-ami-2023.12.20261102.0-kernel-6.1-arm64"}))

    def test_page_renders_with_one_marker(self):
        with tempfile.TemporaryDirectory() as d:
            write_results(d, [sample("fcvm-file", 0, 1, 1.0)])
            out = os.path.join(d, "page.html")
            subprocess.run([sys.executable, os.path.join(HERE, "render.py"), d, "--out", out], check=True,
                           capture_output=True)
            with open(out) as f:
                html = f.read()
            self.assertNotIn("/*BOOT_RACE_DATA*/", html)
            self.assertIn('"fcvm-file"', html)

    def test_public_records_scrub_errors_and_keep_random_ids(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as out:
            recs = [sample("lambda-cold", 0, 1, 120.0, request_id="0b6a2c1e-1111-2222-3333-444455556666"),
                    {"lane": "ec2-start", "round": 0, "ok": False, "t_attempt_unix_ms": 5,
                     "error": "TimeoutError: no answer from 10.12.1.7:8080 for i-0123456789abcdef0"}]
            write_results(d, recs, {"0b6a2c1e-1111-2222-3333-444455556666": {"init_duration": "30 ms"}})
            names = render.public_records(d, out)
            self.assertEqual(names, ["lambda-reports.json", "samples.jsonl", "setup.json"])
            with open(os.path.join(out, "samples.jsonl")) as f:
                text = f.read()
            self.assertNotIn("10.12.1.7", text)
            self.assertNotIn("i-0123456789abcdef0", text)
            self.assertIn("0b6a2c1e-1111-2222-3333-444455556666", text)

    def test_public_records_refuse_an_unscrubbed_field(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as out:
            write_results(d, [sample("ec2-launch", 0, 1, 9.0, note="launched i-0123456789abcdef0")])
            with self.assertRaises(SystemExit):
                render.public_records(d, out)


if __name__ == "__main__":
    unittest.main()
