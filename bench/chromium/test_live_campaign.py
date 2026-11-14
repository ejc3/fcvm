#!/usr/bin/env python3
"""live_campaign.sh: the fcvm live arm and the hosted batches share one window.

Run with a fake `make` (the golden, verify and run targets) and a fake
browserrun.py, so the orchestration is checked without a VM or a credential.
Each test was observed failing against a mutation of the behaviour it pins.
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))

FAKE_MAKE = """#!/bin/bash
# Records its target and, for the run, takes long enough for batches to overlap.
printf '%s\\n' "$*" >> "$FAKE_MAKE_LOG"
for arg in "$@"; do case "$arg" in
  bench-chromium-request-run) env | grep -E '^(URL|ARMS)=' >> "$FAKE_MAKE_LOG"; sleep "${FAKE_RUN_SECS:-2}"; exit "${FAKE_RUN_RC:-0}" ;;
esac; done
exit 0
"""

FAKE_BROWSERRUN = """#!/usr/bin/env python3
import json, os, sys, time
out = sys.argv[sys.argv.index("--out") + 1]
os.makedirs(out)
open(os.path.join(out, "run.json"), "w").write("{}")
with open(os.path.join(out, "browserrun.jsonl"), "w") as h:
    for i in range(2):
        h.write(json.dumps({"ts": time.time()}) + "\\n")
        time.sleep(0.2)
open(os.path.join(out, "summary.json"), "w").write("{}")
"""


class LiveCampaign(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = os.path.join(self.tmp, "repo")
        self.bench = os.path.join(self.repo, "bench", "chromium")
        os.makedirs(self.bench)
        for name in ("live_campaign.sh", "report.py"):
            shutil.copy(os.path.join(HERE, name), self.bench)
        with open(os.path.join(self.bench, "browserrun.py"), "w") as h:
            h.write(FAKE_BROWSERRUN)
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        with open(os.path.join(self.bin, "make"), "w") as h:
            h.write(FAKE_MAKE)
        os.chmod(os.path.join(self.bin, "make"), 0o755)
        self.make_log = os.path.join(self.tmp, "make.log")
        self.out = os.path.join(self.tmp, "live")

    def run_campaign(self, **env):
        e = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", FAKE_MAKE_LOG=self.make_log,
                 LIVE_RESULTS=self.out, LIVE_STAMP="t", REPS="3", WARMUP="1", **env)
        return subprocess.run(["bash", os.path.join(self.bench, "live_campaign.sh")], env=e,
                              capture_output=True, text=True, timeout=120)

    def test_hosted_batches_span_the_fcvm_window(self):
        r = self.run_campaign(FAKE_RUN_SECS="3")
        self.assertEqual(r.returncode, 0, r.stderr)
        live = json.load(open(os.path.join(self.out, "live.json")))
        batches = live["browserrun_batches"]
        self.assertGreaterEqual(len(batches), 2, "batches repeat until the fcvm run ends")
        self.assertLessEqual(min(b["first_ts"] for b in batches), live["reqbench"]["start"] + 2)
        self.assertGreaterEqual(max(b["last_ts"] for b in batches), live["reqbench"]["end"] - 2)
        log = open(self.make_log).read()
        self.assertIn("bench-chromium-request-golden TAG=cb-live-t CPU=4", log)
        self.assertIn("URL=https://example.com/,https://news.ycombinator.com/", log)
        self.assertIn("ARMS=cdp,html-fast,noop", log)
        self.assertEqual(live["resolver"], "guest default (pasta, the host's DNS): live sites")

    def test_the_fcvm_run_status_is_the_campaign_status(self):
        r = self.run_campaign(FAKE_RUN_RC="5", FAKE_RUN_SECS="1")
        self.assertEqual(r.returncode, 5)
        self.assertEqual(json.load(open(os.path.join(self.out, "live.json")))["reqbench"]["exit"], 5)

    def test_an_existing_output_directory_is_refused(self):
        os.makedirs(self.out)
        r = self.run_campaign()
        self.assertEqual(r.returncode, 2)
        self.assertFalse(os.path.exists(self.make_log), "nothing ran")


if __name__ == "__main__":
    unittest.main()
