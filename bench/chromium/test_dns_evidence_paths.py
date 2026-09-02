#!/usr/bin/env python3
"""Where the file names in dns-evidence.json resolve, pinned on the reader.

corpus_campaign.sh records owner_log and verify_files as names inside the
run directory (dns-owner.log, verify-dns-<stage>.json); test_campaign.py
holds the producer to that. The runs sealed before that change carry the
absolute paths of the box that produced them, and a sealed record is never
rewritten: each campaign index cites dns-evidence.json by sha256. So
campaign_summary.py resolves every cited name inside the run directory it
was handed, whichever form the record carries, and trusts neither: the
directory it indexes is a checkout on some other box.

Run: python3 -m unittest test_dns_evidence_paths -v
"""

import glob
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import campaign_summary  # noqa: E402
from test_campaign_summary import VERIFY_STAGES, write_run  # noqa: E402

# What dns-evidence.json cites by name: the owner sampler's log and the
# three verify brackets.
CITED_NAMES = ("dns-owner.log", *(f"verify-dns-{s}.json" for s in VERIFY_STAGES))


def summarize(out, run_dir):
    stdout, stderr = io.StringIO(), io.StringIO()
    with redirect_stdout(stdout), redirect_stderr(stderr):
        rc = campaign_summary.main_with(["--out", out, run_dir])
    return rc, stdout.getvalue() + stderr.getvalue()


def cited_paths(index):
    """The index's generated_from paths for the files the evidence cites."""
    return {
        os.path.basename(entry["path"]): entry["path"]
        for entry in index["generated_from"]
        if os.path.basename(entry["path"]) in CITED_NAMES
    }


def assert_cited_inside(test, index, run_dir):
    """Every cited file was read from run_dir, not from where the record said."""
    cited = cited_paths(index)
    test.assertEqual(set(cited), set(CITED_NAMES), cited)
    for name, path in cited.items():
        test.assertEqual(path, os.path.join(run_dir, name))
        test.assertTrue(os.path.isfile(path), path)


class EvidenceNamesResolveInTheRunDirectory(unittest.TestCase):
    """Both record forms index from a directory the record does not name.

    A contract pin, not a defect: campaign_summary resolved by basename
    before this test existed. It fails against a reader that opens the
    names as recorded: the run-relative form resolves against the working
    directory and the legacy form against a directory this box never had.
    """

    def _index(self, run_dir, evidence_overrides):
        write_run(run_dir, evidence_overrides=evidence_overrides)
        out = os.path.join(os.path.dirname(run_dir), "campaign-x-summary.json")
        rc, text = summarize(out, run_dir)
        self.assertEqual(rc, 0, text)
        with open(out) as handle:
            index = json.load(handle)
        self.assertEqual([cell["dns_verdict"] for cell in index["cells"]], ["clean"])
        assert_cited_inside(self, index, run_dir)
        return index

    def test_run_relative_names(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "reqbench-20260902-000000-corpus")
            self._index(run_dir, {
                "owner_log": "dns-owner.log",
                "verify_files": [f"verify-dns-{s}.json" for s in VERIFY_STAGES],
            })

    def test_legacy_absolute_names_of_a_box_that_is_gone(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "reqbench-20260902-000000-corpus")
            # The path shape of the sealed 2026-09-02 cells, under a
            # directory that exists on no box.
            gone = os.path.join(d, "box-that-wrote-it", "src", "fcvm", "bench",
                                "chromium", "results", "corpus-20260902-031115",
                                "reqbench")
            self.assertFalse(os.path.exists(gone))
            index = self._index(run_dir, {
                "owner_log": os.path.join(gone, "dns-owner.log"),
                "verify_files": [os.path.join(gone, f"verify-dns-{s}.json")
                                 for s in VERIFY_STAGES],
            })
            self.assertFalse(
                any(entry["path"].startswith(gone) for entry in index["generated_from"]),
                "the index read a file from the directory the record named")


class SealedRecordsIndexFromThisCheckout(unittest.TestCase):
    """Every clean, unwithdrawn dns-evidence.json committed under results/
    indexes from wherever this checkout lives.

    The records sealed before the producer wrote run-relative names carry
    absolute paths of the boxes that produced them and stay in that form for
    good; at least one of them has to be present, or this says nothing about
    the legacy form. A run is withdrawn by a WITHDRAWN file beside its
    records or by "withdrawn": true in its analysis.json (campaign_summary
    refuses either), so those are not expected to index.
    """

    RESULTS = os.path.join(HERE, "results")

    @staticmethod
    def withdrawn(run_dir):
        if os.path.exists(os.path.join(run_dir, "WITHDRAWN")):
            return True
        try:
            with open(os.path.join(run_dir, "analysis.json")) as handle:
                return json.load(handle).get("withdrawn") is True
        except (OSError, ValueError):
            return False

    def sealed_runs(self):
        runs = {}
        for path in sorted(glob.glob(os.path.join(self.RESULTS, "*", "dns-evidence.json"))):
            run_dir = os.path.dirname(path)
            with open(path) as handle:
                evidence = json.load(handle)
            if evidence.get("verdict") == "clean" and not self.withdrawn(run_dir):
                runs[run_dir] = evidence
        return runs

    def test_every_sealed_clean_record_indexes(self):
        runs = self.sealed_runs()
        legacy = [d for d, e in runs.items() if os.path.isabs(e.get("owner_log", ""))]
        self.assertTrue(
            legacy,
            "no sealed record in the legacy absolute form is left under results/, "
            "so only the fixture above covers that form")
        for run_dir in runs:
            with self.subTest(run=os.path.basename(run_dir)), \
                    tempfile.TemporaryDirectory() as d:
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = summarize(out, run_dir)
                self.assertEqual(rc, 0, text)
                with open(out) as handle:
                    index = json.load(handle)
                self.assertEqual([cell["dns_verdict"] for cell in index["cells"]], ["clean"])
                assert_cited_inside(self, index, run_dir)


if __name__ == "__main__":
    unittest.main()
