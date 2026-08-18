#!/usr/bin/env python3
"""The webkit engine seam: dispatch, discovery, and the driver contract.

The seam exists so ONE harness (reqbench) produces records for two engines with
identical accounting. The failure modes it must not have: silently rendering
with the wrong driver (a chromium record labelled webkit), a lost session id
surfacing 202 reps later as uniform navigate failures, and a webkit result
whose field names the analyzer cannot read.

Run: python3 -m unittest test_engine_seam -v
"""

import argparse
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

import random

import reqanalyze
import reqbench
import wddrive


class DriveContract(unittest.TestCase):
    """wddrive.drive() must honour cdpdrive.drive()'s record contract."""

    def drive_against(self, handler_cls, session="warm-sess", timeout=15.0):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler_cls)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return wddrive.drive(argparse.Namespace(
            cdp_host=f"127.0.0.1:{server.server_port}",
            url="http://page/x", timeout=timeout,
            session_id=session, out_prefix="",
        ))

    def test_a_successful_render_carries_the_analyzer_fields(self):
        """ok, stages{navigate_ms, screenshot_ms, total_ms}, image_bytes,
        image_sha256 -- the names reqanalyze aggregates. A webkit record with
        different names is not wrong, it is INVISIBLE: the analyzer's medians
        simply omit it and the run publishes with silently thinner stages.
        """
        png = b"\x89PNG\r\n\x1a\n" + b"x" * 64
        import base64
        b64 = base64.b64encode(png).decode()

        class Driver(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def _send(self, value):
                body = json.dumps({"value": value}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def do_GET(self):
                if self.path.endswith("/screenshot"):
                    self._send(b64)
                elif self.path.endswith("/url"):
                    self._send("http://page/x")
                else:
                    self._send({"ready": True})
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                if self.path.endswith("/execute/sync"):
                    self._send(["complete", True])
                else:
                    self._send(None)

        result = self.drive_against(Driver)
        self.assertTrue(result["ok"], result.get("error"))
        self.assertEqual(result["engine"], "webkit")
        for key in ("navigate_ms", "screenshot_ms", "total_ms", "resolve_ms"):
            self.assertIn(key, result["stages"], f"stage {key} missing")
        self.assertEqual(result["image_bytes"], len(png))
        self.assertEqual(len(result["image_sha256"]), 64)

    def test_a_failure_is_a_labelled_record_not_a_traceback(self):
        """cdpdrive's rule, inherited: drive() NEVER raises for a driver-side
        failure. reqbench stores whatever comes back; an exception would be
        swallowed by its broad except and every rep would fail with the cause
        off-screen.
        """
        result = wddrive.drive(argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", timeout=0.3,
            session_id="s", out_prefix=""))
        self.assertFalse(result["ok"])
        self.assertIn("error", result)
        # The seam contract key is "stage" (cdpdrive.py writes out["stage"];
        # reqbench.py lifts result.get("stage") into rec["failure_stage"]).
        # This test once pinned wddrive's deviant "failed_stage", so the suite
        # passed while the harness recorded failure_stage="" for every
        # webkit failure.
        self.assertEqual(result["stage"], "status")
        self.assertIn("total_ms", result["stages"])

    def test_a_missing_session_id_fails_before_any_network(self):
        result = wddrive.drive(argparse.Namespace(
            cdp_host="127.0.0.1:1", url="http://x/", timeout=5.0,
            session_id="", out_prefix=""))
        self.assertFalse(result["ok"])
        self.assertIn("no session id", result["error"])


class SessionDiscovery(unittest.TestCase):
    def test_discovery_raises_loudly_when_exec_yields_nothing(self):
        """An empty session id must stop the run AT DISCOVERY.

        Returned silently, it becomes 202 navigate failures with the real cause
        (the golden never baked a session file) nowhere in the record.
        """
        args = argparse.Namespace(fcvm="/bin/false")
        with mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr="")
            with self.assertRaises(reqbench.SessionDiscoveryFailed) as caught:
                reqbench.discover_wd_session(args, 1234)
        self.assertIn("session discovery failed", str(caught.exception))

    def test_discovery_failure_escalates_out_of_the_rep_handler(self):
        """The raise alone is not the abort: the per-rep handler catches
        BaseException and, before this predicate existed, recorded the failure
        and let the schedule continue -- every later rep re-spawned a clone,
        re-ran discovery, and failed the same way, exactly the 202-doomed-reps
        outcome the discovery docstring promises to prevent. The handler must
        escalate SessionDiscoveryFailed (after that rep's teardown) the same
        way it escalates a host interrupt, and must NOT escalate an ordinary
        per-rep failure."""
        self.assertTrue(reqbench.rep_error_escalates(
            reqbench.SessionDiscoveryFailed("no session")))
        self.assertTrue(reqbench.rep_error_escalates(KeyboardInterrupt()))
        self.assertFalse(reqbench.rep_error_escalates(RuntimeError("one bad rep")))
        self.assertFalse(reqbench.rep_error_escalates(OSError("transient")))

    def test_discovery_strips_and_returns_the_id(self):
        args = argparse.Namespace(fcvm="/bin/true")
        with mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0, stdout="  abc-123\n", stderr="")
            self.assertEqual(reqbench.discover_wd_session(args, 1), "abc-123")


class EngineDispatch(unittest.TestCase):
    def test_reqbench_exposes_the_engine_flag_with_chromium_default(self):
        """The flag is the seam. Its default must stay chromium: every existing
        record and every chromium invocation predates the flag, and a changed
        default silently relabels them.
        """
        import io
        import contextlib
        buf = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stdout(buf):
            saved = sys.argv
            try:
                sys.argv = ["reqbench.py", "--help"]
                reqbench.main()
            finally:
                sys.argv = saved
        text = buf.getvalue()
        self.assertIn("--engine", text)
        self.assertIn("webkit", text)

    def test_the_webkit_branch_exists_and_is_reached_by_engine(self):
        """Structural pin on the dispatch itself: run_cdp_request must consult
        args.engine and route webkit to wddrive. The behavioural half (a full
        fake clone) needs a VM; the routing decision does not.
        """
        import inspect
        src = inspect.getsource(reqbench.run_cdp_request)
        self.assertIn('== "webkit"', src)
        self.assertIn("wddrive.drive", src)
        self.assertIn("discover_wd_session", src)
        self.assertIn("cdpdrive.drive", src,
                      "the chromium path was lost while adding webkit")


class WebkitAnalyzerSchema(unittest.TestCase):
    """The publication gate judges each engine's renders by that engine's schema.

    Before the analyzer learned the webkit schema, a healthy webkit render
    failed ~13 structural checks written for cdpdrive records (cdp_host,
    idle policy, prewire, the six WebSocket/idle/decode stages, width/height,
    quality, navigation timing), so an ENGINE=webkit run could never gate
    clean even with every render healthy.
    """

    CHROMIUM_ONLY_COMPLAINTS = (
        "does not match",           # cdp_host / format held to the wrong keys
        "idle policy",
        "prewire",
        "invalid tcp_ms",
        "invalid upgrade_ms",
        "invalid enable_ms",
        "invalid idle_ms",
        "invalid decode_ms",
        "invalid nav_timing_ms",
        "invalid width",
        "invalid height",
        "quality mismatches",
        "no navigation timing",
    )

    @staticmethod
    def webkit_run_lines():
        """One healthy ENGINE=webkit run as (meta, records) JSONL lines.

        The records realise the seeded schedule exactly, in order, so the only
        errors a test can observe are the schema checks under test.
        """
        meta = {
            "kind": "meta", "run_id": "wk", "engine": "webkit",
            "backend": "uffd", "uffd_mode": "copy",
            "format": "png", "quality": 85, "url": "http://c/p",
            "urls": None, "arms": ["cdp", "noop"], "reps": 1, "warmup": 1,
            "seed": 7, "started": 1.0,
            "image": "localhost/webkit-bench-req",
            "image_id": "sha256:" + "d" * 64, "snapshot": "snapshot-wk",
            "snapshot_generation_id": "22222222-2222-4222-8222-222222222222",
            "snapshot_config_sha256": "7" * 64,
            "snapshot_created_at": "2026-08-09T00:00:00Z",
            "snapshot_vm_id": "vm-" + "f" * 32,
            "fcvm_sha256": "a" * 64, "harness_sha256": "c" * 64,
            "runtime_bundle_sha256": "8" * 64,
            "source_revision": "b" * 40,
            "cdp_port": 9515, "network_mode": "rootless", "cpu": 2,
            "port_mappings": [{
                "host_ip": None, "host_port": 9515, "guest_port": 9515,
                "proto": "tcp",
            }],
            "memory_mib": 1024,
            "rust_log": "fcvm=debug", "ws_url_prewired": False,
            "allow_busy": False,
            "host_boot_id": "00000000-0000-0000-0000-000000000001",
            "host_kernel_release": "6.18.0-fixture", "host_machine": "aarch64",
            "loadavg": [0.5, 0.5, 0.5], "quiet_loadavg1_limit": 2.0,
            "quiet_vm_processes": 0, "quiet_guard_loadavg1": 0.5,
            "quiet_guard_passed": True,
        }
        rng = random.Random(meta["seed"])
        schedule = []
        for rep in range(meta["warmup"] + meta["reps"]):
            order = list(meta["arms"])
            rng.shuffle(order)
            schedule.extend((arm, rep, rep < meta["warmup"]) for arm in order)
        records = []
        for arm, rep, is_warmup in schedule:
            record = {
                "kind": "request", "arm": arm, "rep": rep, "warmup": is_warmup,
                "ok": True, "blocking_ms": 12.0, "wall_ms": 20.0,
                "record_id": f"wk:{arm}:{rep}:{int(is_warmup)}", "run_id": "wk",
                "url": meta["url"],
            }
            if arm == "cdp":
                record.update({
                    "endpoint": "127.0.0.1:9515",
                    "state_to_port_ms": 1.0, "spawn_to_port_ms": 2.0,
                    "render": {
                        "ok": True, "engine": "webkit",
                        "wd_host": "127.0.0.1:9515", "url": meta["url"],
                        "format": "png", "session_prewired": True,
                        "session_id": "s", "image_bytes": 10,
                        "image_sha256": "a" * 64,
                        "stages": {
                            "resolve_ms": 1.0, "connect_total_ms": 2.0,
                            "navigate_ms": 3.0, "screenshot_ms": 4.0,
                            "total_ms": 10.0,
                        },
                    },
                })
            records.append(record)
        return meta, records

    def load_dataset(self, mutate=None):
        """Round-trip the fixture run through reqanalyze.load(), as real
        datasets arrive: the loader builds the envelope, stamps provenance,
        and runs the schedule validation whose errors the tests read."""
        meta, records = self.webkit_run_lines()
        if mutate is not None:
            mutate(records)
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "wk.jsonl")
            with open(path, "w") as f:
                for line in [meta, *records]:
                    f.write(json.dumps(line) + "\n")
            (dataset,) = reqanalyze.load([path])
        return dataset

    def test_a_healthy_webkit_render_raises_no_chromium_schema_errors(self):
        dataset = self.load_dataset()
        schema_noise = [
            error for error in dataset["metadata_errors"]
            if any(complaint in error for complaint in self.CHROMIUM_ONLY_COMPLAINTS)
        ]
        self.assertEqual(
            schema_noise, [],
            "a healthy webkit render must not fail chromium-schema checks",
        )

    def test_a_webkit_render_missing_its_own_stages_still_fails(self):
        def drop_screenshot_stage(records):
            measured_cdp = next(
                record for record in records
                if record["arm"] == "cdp" and not record["warmup"]
            )
            del measured_cdp["render"]["stages"]["screenshot_ms"]

        dataset = self.load_dataset(mutate=drop_screenshot_stage)
        self.assertTrue(
            any("invalid screenshot_ms" in error for error in dataset["metadata_errors"]),
            f"webkit schema must still gate its own stages: {dataset['metadata_errors']}",
        )


if __name__ == "__main__":
    unittest.main()
