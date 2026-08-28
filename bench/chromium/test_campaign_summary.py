#!/usr/bin/env python3
"""campaign_summary.py and the evidence ignore rules, pinned.

campaign_summary.py indexes the cells of one campaign. It reads, never
writes, each run directory, and it writes the index only when every run is
publishable and every DNS verdict is clean: an index that quietly carried an
unpublishable cell would be quoted by someone who only opened the index.

The ignore rules matter for the reason the 2026-08-15 instance-store wipe
taught: the evidence files that back a claim (verify-dns.json,
dns-evidence.json, diag/summary.json, dns-owner.log, campaign-*-summary.json)
have to show up in `git status` so they get committed, while raw run output
stays ignored.

Every test here was watched red before the matching change; the failure each
produced is quoted in its docstring.

Run: python3 -m unittest test_campaign_summary -v
"""

import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import campaign_summary  # noqa: E402


class EvidenceIgnoreRules(unittest.TestCase):
    """Evidence files are negated out of the results ignore; raw output is not.

    RED BEFORE THE FIX: verify-dns.json, dns-evidence.json and dns-owner.log
    were ignored by `results/**` (git check-ignore exit 0), and the literal
    negations for them were absent from bench/chromium/.gitignore.
    """

    IGNORE = os.path.join(HERE, ".gitignore")
    NEGATIONS = (
        "!results/**/verify-dns.json",
        "!results/**/dns-evidence.json",
        "!results/**/diag/summary.json",
        "!results/**/dns-owner.log",
        "!results/**/campaign-*-summary.json",
    )
    EVIDENCE = (
        "results/run-x/verify-dns.json",
        "results/run-x/dns-evidence.json",
        "results/run-x/diag/summary.json",
        "results/run-x/dns-owner.log",
        "results/campaign-x-summary.json",
        "results/campaign-x/campaign-x-summary.json",
    )
    RAW = (
        "results/run-x/reqbench.jsonl",
        "results/run-x/requests/0.json",
        "results/run-x/raw.json",
        "results/run-x/report.md",
    )

    def _ignored(self, relative):
        result = subprocess.run(
            ["git", "-C", HERE, "check-ignore", "-q", "--no-index", relative],
            capture_output=True, text=True, timeout=30,
        )
        # 0 = ignored, 1 = not ignored; anything else means git could not
        # evaluate the rules, which must block rather than pass.
        self.assertIn(
            result.returncode, (0, 1),
            f"git check-ignore could not evaluate {relative}: {result.stderr}",
        )
        return result.returncode == 0

    def test_the_negations_are_written_down(self):
        with open(self.IGNORE) as handle:
            lines = [line.strip() for line in handle]
        for negation in self.NEGATIONS:
            self.assertIn(negation, lines, f"{self.IGNORE} lacks {negation}")

    def test_git_does_not_ignore_the_evidence_files(self):
        for relative in self.EVIDENCE:
            with self.subTest(path=relative):
                self.assertFalse(
                    self._ignored(relative),
                    f"{relative} is ignored and would be lost on the next wipe",
                )

    def test_git_still_ignores_raw_run_output(self):
        for relative in self.RAW:
            with self.subTest(path=relative):
                self.assertTrue(self._ignored(relative), f"{relative} is not ignored")


def write_run(
    run_dir,
    *,
    publishable=True,
    stall_passed=True,
    dns_verdict="clean",
    diag=None,
    guest_dns="10.0.2.2",
    engine="chromium",
):
    """A minimal run directory shaped like reqanalyze + the campaign evidence.

    dns_verdict=None omits dns-evidence.json; diag=None omits diag/summary.json.
    """
    os.makedirs(run_dir, exist_ok=True)
    analysis = {
        "publishable": publishable,
        "gate": {"passed": publishable, "reasons": [] if publishable else ["x"]},
        "run_id": "0" * 32,
        "backend": "uffd",
        "cell": {
            "backend": "uffd",
            "uffd_mode": "minor",
            "engine": engine,
            "cpu": 2,
            "memory_mib": 1024,
            "guest_dns": guest_dns,
            "url": "https://example.com/",
        },
        "arms": {
            "cdp": {
                "blocking_ms": {"median": 647.2, "lo": 567.6, "hi": 702.9, "n": 202},
                "wall_ms": {"median": 700.0, "lo": 600.0, "hi": 800.0, "n": 202},
            },
            "noop": {
                "blocking_ms": {"median": 41.1, "lo": 40.0, "hi": 42.5, "n": 202},
            },
        },
        "stall_gate": {"max_ms": 15000, "passed": stall_passed, "violations": []},
    }
    paths = {"analysis": os.path.join(run_dir, "analysis.json")}
    with open(paths["analysis"], "w") as handle:
        json.dump(analysis, handle)
    if dns_verdict is not None:
        paths["dns_evidence"] = os.path.join(run_dir, "dns-evidence.json")
        with open(paths["dns_evidence"], "w") as handle:
            json.dump({"verdict": dns_verdict, "queries": 14}, handle)
    if diag is not None:
        os.makedirs(os.path.join(run_dir, "diag"), exist_ok=True)
        paths["diag"] = os.path.join(run_dir, "diag", "summary.json")
        with open(paths["diag"], "w") as handle:
            json.dump(diag, handle)
    return paths


def sha256_file(path):
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def tree_digest(root):
    digests = {}
    for directory, _dirs, files in os.walk(root):
        for name in files:
            path = os.path.join(directory, name)
            digests[os.path.relpath(path, root)] = sha256_file(path)
    return digests


class CampaignSummary(unittest.TestCase):
    """One index per campaign, written only when every cell can be quoted.

    RED BEFORE THE FIX: ModuleNotFoundError: No module named 'campaign_summary'.
    """

    def _summarize(self, out, run_dirs):
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            rc = campaign_summary.main_with(["--out", out] + list(run_dirs))
        return rc, stdout.getvalue() + stderr.getvalue()

    def test_one_clean_run_is_indexed(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "reqbench-20260828-000000-corpus")
            paths = write_run(run_dir, diag={"dns_owner": "corpus_serve", "stalls": 0})
            out_dir = os.path.join(d, "index")
            os.makedirs(out_dir)
            out = os.path.join(out_dir, "campaign-20260828-summary.json")
            before = tree_digest(run_dir)
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            self.assertEqual(os.listdir(out_dir), [os.path.basename(out)])
            self.assertEqual(tree_digest(run_dir), before, "inputs were modified")
            with open(out) as handle:
                index = json.load(handle)
            self.assertEqual(
                {entry["path"] for entry in index["generated_from"]},
                set(paths.values()),
            )
            for entry in index["generated_from"]:
                self.assertEqual(entry["sha256"], sha256_file(entry["path"]))
        self.assertEqual(len(index["cells"]), 1)
        cell = index["cells"][0]
        self.assertEqual(cell["run_dir"], run_dir)
        self.assertEqual(cell["engine"], "chromium")
        self.assertEqual(cell["cpu"], 2)
        self.assertEqual(cell["memory_mib"], 1024)
        self.assertEqual(cell["guest_dns"], "10.0.2.2")
        self.assertIs(cell["publishable"], True)
        self.assertIs(cell["stall_gate_passed"], True)
        self.assertEqual(cell["dns_verdict"], "clean")
        self.assertEqual(cell["headline"]["cdp"]["blocking_ms"], 647.2)
        self.assertEqual(cell["headline"]["cdp"]["blocking_ms_ci"], [567.6, 702.9])
        self.assertEqual(cell["headline"]["cdp"]["n"], 202)
        self.assertEqual(cell["headline"]["noop"]["blocking_ms"], 41.1)
        self.assertEqual(cell["diag"], {"dns_owner": "corpus_serve", "stalls": 0})

    def test_an_unclean_dns_verdict_refuses_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            clean = os.path.join(d, "clean")
            tainted = os.path.join(d, "tainted")
            write_run(clean)
            write_run(tainted, dns_verdict="contaminated")
            out_dir = os.path.join(d, "index")
            os.makedirs(out_dir)
            out = os.path.join(out_dir, "campaign-x-summary.json")
            before = tree_digest(d)
            rc, text = self._summarize(out, [clean, tainted])
            self.assertNotEqual(rc, 0)
            self.assertEqual(os.listdir(out_dir), [], "the index was written anyway")
            self.assertIn("tainted", text)
            self.assertIn("contaminated", text)
            self.assertEqual(tree_digest(d), before, "inputs were modified")

    def test_an_unpublishable_analysis_refuses_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, publishable=False)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0)
            self.assertFalse(os.path.exists(out))
            self.assertIn("publishable", text)

    def test_a_failed_stall_gate_refuses_even_if_marked_publishable(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, stall_passed=False)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0)
            self.assertFalse(os.path.exists(out))
            self.assertIn("stall_gate", text)

    def test_a_missing_analysis_refuses(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            os.makedirs(run_dir)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0)
            self.assertFalse(os.path.exists(out))
            self.assertIn("analysis.json", text)

    def test_absent_dns_evidence_is_recorded_as_null(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, dns_verdict=None, guest_dns=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                index = json.load(handle)
        cell = index["cells"][0]
        self.assertIsNone(cell["dns_verdict"])
        self.assertIsNone(cell["guest_dns"])
        self.assertIsNone(cell["diag"])

    def test_the_index_cannot_alias_an_input(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            before = sha256_file(paths["analysis"])
            rc, text = self._summarize(paths["analysis"], [run_dir])
            self.assertNotEqual(rc, 0)
            self.assertEqual(sha256_file(paths["analysis"]), before)

    def test_the_cli_entry_point(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir)
            out = os.path.join(d, "campaign-x-summary.json")
            result = subprocess.run(
                [
                    sys.executable, os.path.join(HERE, "campaign_summary.py"),
                    "--out", out, run_dir,
                ],
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            with open(out) as handle:
                index = json.load(handle)
        self.assertEqual(index["cells"][0]["run_dir"], run_dir)


if __name__ == "__main__":
    unittest.main()
