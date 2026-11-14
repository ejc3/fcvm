#!/usr/bin/env python3
"""Tests for summary.py: the web page's summary carries the published page's numbers and every lane's configuration.

    python3 -m unittest bench/boot-race/test_summary.py
"""

import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import summary  # noqa: E402

MANIFEST = {"campaign": "c1", "repository": "ejc3/fcvm-bench-records", "records_commit": "a" * 40}


def page(rows, lanes):
    data = {"setup": {"region": "us-east-2", "rows": rows, "checks": [["fcvm-file", "{n} samples, {new_proc} process"]]},
            "first_sample_unix_ms": 1793851066938, "last_sample_unix_ms": 1793853766938, "rounds": 30,
            "lanes": lanes, "not_run": [{"id": "ecs-fargate", "name": "ECS on Fargate", "why": "off"}]}
    return f'<script id="data" type="application/json">{json.dumps(data)}</script>'


LANE = {"id": "fcvm-file", "name": "fcvm restore, memory file", "start": "x", "n": 30, "failures": 0,
        "median_ms": 81.4, "p95_ms": 87.6, "p99_ms": 129.9, "min_ms": 72.0, "max_ms": 129.9,
        "checks": {"new_proc": 1, "new_boot": 1, "same_proc_as_previous": 29, "init_duration": 0, "report": 0}}
ROWS = [["Campaign host", "c8gd.metal-24xl"], ["fcvm", "commit f0147cf7b023, fcvm 0.1.0"], ["fcvm VM", "1 vCPU"]]


class Summary(unittest.TestCase):
    def test_numbers_and_configuration_come_from_the_page(self):
        s = summary.summarize(summary.page_data(page(ROWS, [LANE])), MANIFEST)
        lane = s["lanes"][0]
        self.assertEqual((lane["median_ms"], lane["p99_ms"], lane["config"]), (81.4, 129.9, "1 vCPU"))
        self.assertEqual(lane["check"], "30 samples, 1 process")
        self.assertEqual(s["fcvm_commit"], "f0147cf7b023")
        self.assertEqual(s["window_utc"]["first_sample"], "2026-11-05T03:57:46Z")
        self.assertTrue(s["records"]["url"].endswith("/tree/" + "a" * 40 + "/bench/boot-race/results/c1"))
        self.assertEqual(s["not_run"][0]["id"], "ecs-fargate")

    def test_a_lane_without_its_configuration_is_refused(self):
        with self.assertRaisesRegex(SystemExit, "fcvm-file has no configuration"):
            summary.summarize(summary.page_data(page(ROWS[:2], [LANE])), MANIFEST)

    def test_the_published_page_summarizes(self):
        root = os.path.join(HERE, "..", "..")
        with open(os.path.join(root, "docs", "boot-race.html")) as f:
            data = summary.page_data(f.read())
        with open(os.path.join(root, "fleet", "web", "app", "boot-race", "summary.json")) as f:
            committed = json.load(f)
        with open(os.path.join(HERE, "results", f"records-{committed['campaign']}.json")) as f:
            manifest = json.load(f)
        self.assertEqual(summary.summarize(data, manifest), committed, "regenerate fleet/web/app/boot-race/summary.json")


if __name__ == "__main__":
    unittest.main()
