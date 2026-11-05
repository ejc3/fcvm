#!/usr/bin/env python3
"""No committed benchmark record is larger than records.CAP_BYTES.

Raw records (per-request logs, analyzer dumps, server logs, traces) go to
ejc3/fcvm-bench-records and are listed in results/records-<campaign>.json;
results/ keeps the summaries. Without a cap the raw records of one campaign
(fe4d8666, 525 files, 813 MiB) were committed into every checkout of main.

RED BEFORE THE FIX: on 7c62d332 test_no_committed_record_exceeds_the_cap
failed with 312 files over 524288 bytes under bench/chromium/results, the
largest results/reqbench-40657613-s2/scale-20261105-020322-1485768/requests.jsonl
at 59,633,835 bytes.
"""

import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import records  # noqa: E402


def committed_sizes(prefixes):
    """{path: blob size} for every file the index holds under prefixes."""
    listing = subprocess.run(
        ["git", "ls-files", "-s", "-z", "--", *prefixes], cwd=records.REPO,
        check=True, capture_output=True, text=True,
    ).stdout.split("\0")[:-1]
    paths, blobs = [], []
    for line in listing:
        meta, path = line.split("\t", 1)
        paths.append(path)
        blobs.append(meta.split()[1])
    sizes = subprocess.run(
        ["git", "cat-file", "--batch-check=%(objectsize)"], cwd=records.REPO,
        input="\n".join(blobs) + "\n", check=True, capture_output=True, text=True,
    ).stdout.split()
    return dict(zip(paths, map(int, sizes)))


class RecordsCap(unittest.TestCase):
    def test_no_committed_record_exceeds_the_cap(self):
        sizes = committed_sizes(records.RECORD_DIRS)
        self.assertGreater(len(sizes), 100, "git ls-files found almost nothing; the check is vacuous")
        over = sorted(((size, path) for path, size in sizes.items() if size > records.CAP_BYTES),
                      reverse=True)
        if over:
            self.fail(f"{len(over)} committed records over {records.CAP_BYTES} bytes; raw records "
                      f"go to {records.RECORDS_REPOSITORY} (bench/chromium/records.py move, then "
                      f"manifest). Largest: {over[:5]}")

    def test_a_listed_record_is_not_also_committed(self):
        listed = set(records.listed())
        self.assertTrue(listed, "no manifest lists a moved record")
        committed = set(committed_sizes(records.RECORD_DIRS))
        self.assertEqual(sorted(listed & committed), [],
                         "a file is both committed and listed as moved")
        outside = [p for p in listed if not p.startswith(tuple(d + "/" for d in records.RECORD_DIRS))]
        self.assertEqual(outside, [], "a manifest lists a file outside the records directories")

    def test_a_moved_record_is_read_only_with_the_listed_bytes(self):
        path = records.listed("bench/chromium/results/reqbench-40657613-")[0]
        found = records.entry(os.path.join(records.REPO, path))
        data = records.read_moved(found)
        self.assertEqual(len(data), found["size"])
        with self.assertRaisesRegex(records.MissingRecord, "not the"):
            records.read_moved({**found, "sha256": "0" * 64})
        env = {k: v for k, v in os.environ.items() if k != records.RECORDS_ENV}
        with mock.patch.dict(os.environ, env, clear=True), \
                self.assertRaisesRegex(records.MissingRecord, "not in this checkout"):
            records.read_moved({**found, "fcvm_commit": "f" * 40})

    def test_a_listed_record_present_in_the_tree_is_held_to_the_manifest(self):
        """RED BEFORE THE FIX (codex review of 7c62d332..90c0c93e): a file at a
        listed path was returned from the tree unchecked, by records.read_bytes
        and by campaign_summary's Sources, so a regenerated or edited copy was
        read in place of the listed bytes."""
        import campaign_summary
        with tempfile.TemporaryDirectory() as run:
            path = os.path.join(run, "analysis.json")
            with open(path, "w") as handle:
                handle.write('{"run_id": "edited"}')
            listed = {records.repo_relative(path): {
                "path": records.repo_relative(path), "size": 2, "sha256": "0" * 64,
                "manifest": "results/records-test.json", "fcvm_commit": None,
                "repository": records.RECORDS_REPOSITORY, "records_commit": "f" * 40}}
            with mock.patch.object(records, "_manifests", return_value=listed):
                with self.assertRaisesRegex(records.MissingRecord, "not the 2 bytes"):
                    records.read_bytes(path)
                with self.assertRaisesRegex(campaign_summary.RunError, "not the 2 bytes"):
                    campaign_summary.Sources(run).read_json(path)

    def test_a_run_named_through_a_directory_alias_finds_its_moved_records(self):
        """RED BEFORE THE FIX (codex review of 7c62d332..90c0c93e): the manifest
        key kept the alias's own path, so a run directory named through a
        symlink had no moved records and campaign_summary called its
        analysis.json missing."""
        import campaign_summary
        path = records.listed("bench/chromium/results/reqbench-40657613-r1-c8/analysis.json")[0]
        run = os.path.dirname(os.path.join(records.REPO, path))
        with tempfile.TemporaryDirectory() as tmp:
            alias = os.path.join(tmp, "run")
            os.symlink(run, alias)
            self.assertIsNotNone(records.entry(os.path.join(alias, "analysis.json")))
            self.assertTrue(campaign_summary.Sources(alias).exists(os.path.join(alias, "analysis.json")))


if __name__ == "__main__":
    unittest.main()
