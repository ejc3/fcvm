#!/usr/bin/env python3
"""reqscale_stages.py: per-rate stage medians and listen-log aggregates (#1159).

Run: python3 -m unittest test_reqscale_stages -v
"""

import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqscale_stages  # noqa: E402

WALL_MINUS_MONO_NS = 1_000_000_000_000_000_000
S = 1_000_000_000


def write_jsonl(path, rows):
    with open(path, "w") as handle:
        for row in rows:
            handle.write(json.dumps(row) + "\n")


def request(burst, rate, launched_s, navigate, ok=True, population="scored"):
    return {"kind": "request", "burst_id": burst, "population": population, "ok": ok,
            "target_rps": rate, "actual_launch_ns": int(launched_s * S),
            "spawn_to_port_ms": 100.0, "blocking_ms": navigate + 400.0,
            "render": {"stages": {"resolve_ms": 220.0, "navigate_ms": navigate,
                                  "screenshot_ms": 70.0, "total_ms": navigate + 300.0}}}


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        d = self.tmp.name
        # two scored bursts at 1.0 req/s and one at 2.0; scoring windows 10 s each
        bursts = [
            {"kind": "burst", "burst_id": "w", "population": "warmup", "target_rps": 1.0,
             "score_start_ns": 0, "score_end_ns": 10 * S},
            {"kind": "burst", "burst_id": "a", "population": "scored", "target_rps": 1.0,
             "score_start_ns": 100 * S, "score_end_ns": 110 * S},
            {"kind": "burst", "burst_id": "b", "population": "scored", "target_rps": 2.0,
             "score_start_ns": 200 * S, "score_end_ns": 210 * S},
        ]
        requests = [
            request("a", 1.0, 101, 300.0), request("a", 1.0, 102, 5000.0),
            request("a", 1.0, 99, 9999.0),                      # ramp, before the window
            request("a", 1.0, 103, 1.0, ok=False),              # failed: counted apart
            request("b", 2.0, 201, 600.0), request("b", 2.0, 202, 700.0),
            request("w", 1.0, 5, 8888.0, population="warmup"),  # warmup: ignored
        ]
        write_jsonl(os.path.join(d, "bursts.jsonl"), bursts)
        write_jsonl(os.path.join(d, "requests.jsonl"), requests)
        write_jsonl(os.path.join(d, "host-samples.jsonl"), [
            {"captured_monotonic_ns": 50 * S, "captured_wall_ns": 50 * S + WALL_MINUS_MONO_NS},
            {"captured_monotonic_ns": 60 * S, "captured_wall_ns": 60 * S + WALL_MINUS_MONO_NS},
        ])
        self.dir = d

    def listen_row(self, listener, mono_s, **kw):
        row = {"ts": WALL_MINUS_MONO_NS / 1e9 + mono_s, "listener": listener}
        if listener == "kernel":
            row.update(listen_overflows=0, listen_drops=0, accept_queue={"443": [0, 5]})
        else:
            row.update(accepts=0, accept_errors=0, accept_ms_sum=0.0, accept_ms_max=0.0,
                       accept_busy=0.0, handshakes=0, handshake_ms_max=0.0,
                       thread_start_ms_max=0.0, thread_start_ms_sum=0.0, thread_starts=0,
                       requests=0, request_ms_sum=0.0, request_ms_max=0.0, active_peak=0)
        row.update(kw)
        return row


class Requests(Fixture):
    def test_only_successful_scored_requests_launched_in_a_window_count(self):
        out = reqscale_stages.summarize(self.dir)["requests"]
        one, two = out[1.0], out[2.0]
        self.assertEqual((one["scored_ok"], one["scored_failed"]), (2, 1))
        self.assertEqual(one["navigate_ms"]["median"], 2650.0)
        self.assertEqual(one["navigate_over_2s"], 1)
        self.assertEqual((two["scored_ok"], two["navigate_ms"]["median"]), (2, 650.0))
        self.assertEqual(one["spawn_to_port_ms"]["median"], 100.0)


class Listen(Fixture):
    def test_rows_are_joined_to_windows_through_the_two_clocks(self):
        rows = [
            self.listen_row("tls", 101, accepts=40, accept_ms_sum=300.0, accept_ms_max=12.0,
                            accept_busy=0.3, requests=40, request_ms_sum=80.0, thread_starts=40,
                            thread_start_ms_sum=20.0, thread_start_ms_max=2.0, active_peak=7),
            self.listen_row("tls", 105, accepts=10, accept_ms_sum=900.0, accept_ms_max=80.0,
                            accept_busy=0.9, requests=10, request_ms_sum=70.0, thread_starts=10,
                            thread_start_ms_sum=30.0, thread_start_ms_max=9.0, active_peak=3),
            self.listen_row("tls", 150, accepts=999, accept_ms_sum=999.0, accept_busy=0.99),  # between bursts
            self.listen_row("kernel", 104, listen_overflows=3, listen_drops=3,
                            accept_queue={"443": [6, 5], "80": [0, 5]}),
            self.listen_row("kernel", 205, listen_overflows=11, listen_drops=12,
                            accept_queue={"443": [1, 5]}),
        ]
        path = os.path.join(self.dir, "corpus-listen.log")
        write_jsonl(path, [{"listener": "start", "tls_handshake": "accept", "backlog": 5}] + rows)
        out = reqscale_stages.summarize(self.dir, path)
        self.assertEqual(out["server"]["backlog"], 5)
        one, two = out["listen"][1.0], out["listen"][2.0]
        self.assertEqual(one["tls"]["accepts"], 50)
        self.assertEqual(one["tls"]["accept_busy_max_second"], 0.9)
        self.assertEqual(one["tls"]["accept_ms_mean"], 24.0)
        self.assertEqual(one["tls"]["request_ms_mean"], 3.0)
        self.assertEqual(one["tls"]["thread_start_ms_mean"], 1.0)
        self.assertEqual(one["tls"]["accept_ms_max"], 80.0)
        self.assertAlmostEqual(one["tls"]["accept_busy_mean"], 1200.0 / 10000.0)
        self.assertEqual(one["kernel"], {"listen_overflows": 3, "listen_drops": 3, "accept_queue_peak": 6})
        self.assertNotIn("tls", two)
        # a second whose counters could not be read makes the total unknown, not zero
        rows.append({"ts": WALL_MINUS_MONO_NS / 1e9 + 106, "listener": "kernel", "accept_queue": {}})
        write_jsonl(path, [{"listener": "start"}] + rows)
        again = reqscale_stages.summarize(self.dir, path)["listen"][1.0]["kernel"]
        self.assertIsNone(again["listen_overflows"])
        self.assertIsNone(again["listen_drops"])
        self.assertEqual(two["kernel"]["listen_overflows"], 11)


if __name__ == "__main__":
    unittest.main()
