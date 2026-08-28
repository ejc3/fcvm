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

    RED BEFORE THE SECOND FIX: the campaign keeps each verify bracket as
    verify-dns-<stage>.json (pre, before-run, after-run) and dns-evidence.json
    lists them in verify_files, but only the unsuffixed name was negated, so
    `results/run-x/verify-dns-pre.json is ignored and would be lost on the
    next wipe` (three subtests) and the literal `!results/**/verify-dns-*.json`
    was absent.
    """

    IGNORE = os.path.join(HERE, ".gitignore")
    NEGATIONS = (
        "!results/**/verify-dns.json",
        "!results/**/verify-dns-*.json",
        "!results/**/dns-evidence.json",
        "!results/**/diag/summary.json",
        "!results/**/dns-owner.log",
        "!results/**/campaign-*-summary.json",
        "!results/**/WITHDRAWN",
    )
    EVIDENCE = (
        "results/run-x/verify-dns.json",
        "results/run-x/verify-dns-pre.json",
        "results/run-x/verify-dns-before-run.json",
        "results/run-x/verify-dns-after-run.json",
        "results/run-x/dns-evidence.json",
        "results/run-x/diag/summary.json",
        "results/run-x/dns-owner.log",
        "results/campaign-x-summary.json",
        "results/campaign-x/campaign-x-summary.json",
        "results/run-x/WITHDRAWN",
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


VERIFY_STAGES = ("pre", "before-run", "after-run")
CORPUS_LOGS = ("corpus-dns.log", "corpus-access.log")
# The seal identity reqbench.py stamps into every record's meta and
# reqanalyze carries into the cell: the runtime bundle reqbench.sh sealed,
# the binaries and sources hashed into it, the source revision, and the
# image and snapshot generation that were measured.
SEAL = {
    "runtime_bundle_sha256": "8" * 64,
    "fcvm_sha256": "a" * 64,
    "harness_sha256": "c" * 64,
    "source_revision": "b" * 40,
    "image_id": "sha256:" + "e" * 64,
    "snapshot_generation_id": "33333333-3333-4333-8333-333333333333",
    "snapshot_config_sha256": "5" * 64,
}


def write_verify(path, passed=True):
    """One HOP D evidence file in reqbench.sh's shape."""
    with open(path, "w") as handle:
        json.dump({
            "dns_server": "10.0.2.2",
            "resolv_conf_vm": "nameserver 10.0.2.2\n",
            "resolv_conf_container": "nameserver 10.0.2.2\n",
            "hosts": {"example.com": {"answer": "10.0.2.2", "ok": passed}},
            "urls": {"https://example.com/": {"status": 200, "ok": passed}},
            "timestamp": "2026-08-28T00:00:00Z",
            "passed": passed,
        }, handle)


def write_run(
    run_dir,
    *,
    publishable=True,
    stall_passed=True,
    dns_verdict="clean",
    diag=None,
    guest_dns="10.0.2.2",
    engine="chromium",
    stall_max_ms=15000,
    stall_evaluated=404,
    samples=12,
    evidence_overrides=None,
    cell_overrides=None,
    analysis_overrides=None,
    withdrawn=None,
):
    """A minimal run directory shaped like reqanalyze + the campaign evidence.

    dns_verdict=None omits dns-evidence.json and everything it names; diag=None
    omits diag/summary.json. stall_max_ms=None is what reqanalyze writes when
    it ran without --stall-max-ms (passed true, evaluated 0). The evidence
    names three passing verify brackets, an owner log with `samples` lines
    and the two replay logs with their real sha256, the way
    corpus_campaign.sh writes them; evidence_overrides rewrites fields on top.
    The cell carries SEAL; cell_overrides rewrites cell fields (a None value
    removes the field) and analysis_overrides rewrites top-level fields.
    withdrawn=<reason> writes a WITHDRAWN marker whose first line is the
    reason. Returns every path the index is expected to read.
    """
    os.makedirs(run_dir, exist_ok=True)
    cell = {
        "backend": "uffd",
        "uffd_mode": "minor",
        "engine": engine,
        "cpu": 2,
        "memory_mib": 1024,
        "guest_dns": guest_dns,
        "url": "https://example.com/",
        **SEAL,
    }
    for field, value in (cell_overrides or {}).items():
        if value is None:
            cell.pop(field, None)
        else:
            cell[field] = value
    analysis = {
        "publishable": publishable,
        "gate": {"passed": publishable, "reasons": [] if publishable else ["x"]},
        "run_id": "0" * 32,
        "backend": "uffd",
        "cell": cell,
        "arms": {
            "cdp": {
                "blocking_ms": {"median": 647.2, "lo": 567.6, "hi": 702.9, "n": 202},
                "wall_ms": {"median": 700.0, "lo": 600.0, "hi": 800.0, "n": 202},
            },
            "noop": {
                "blocking_ms": {"median": 41.1, "lo": 40.0, "hi": 42.5, "n": 202},
            },
        },
        "stall_gate": {
            "max_ms": stall_max_ms,
            "passed": stall_passed,
            "evaluated": stall_evaluated,
            "violations": [],
        },
    }
    analysis.update(analysis_overrides or {})
    paths = {"analysis": os.path.join(run_dir, "analysis.json")}
    with open(paths["analysis"], "w") as handle:
        json.dump(analysis, handle)
    if withdrawn is not None:
        with open(os.path.join(run_dir, "WITHDRAWN"), "w") as handle:
            handle.write(f"{withdrawn}\nsecond line: detail the refusal need not quote\n")
    if dns_verdict is not None:
        verify_files = []
        for stage in VERIFY_STAGES:
            verify_path = os.path.join(run_dir, f"verify-dns-{stage}.json")
            write_verify(verify_path)
            paths[f"verify-{stage}"] = verify_path
            verify_files.append(verify_path)
        hashes = {}
        for name in CORPUS_LOGS:
            log_path = os.path.join(run_dir, name)
            with open(log_path, "w") as handle:
                handle.write('{"ts": 1.0, "qname": "example.com"}\n')
            paths[name] = log_path
            hashes[name] = sha256_file(log_path)
        owner_log = os.path.join(run_dir, "dns-owner.log")
        with open(owner_log, "w") as handle:
            handle.write(
                "2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive\n" * samples
            )
        paths["owner_log"] = owner_log
        evidence = {
            "serve_pid": 4242,
            "dnsmasq_was_active_before": True,
            "dnsmasq_active_after_restore": False,
            "dnsmasq_state_after_restore": "inactive",
            "sampler_alive_at_stop": True,
            "samples": samples,
            "sample_interval_s": 10,
            "owner_log": owner_log,
            "first_mismatch": None,
            "verify_files": verify_files,
            "corpus_dns_log_sha256": hashes["corpus-dns.log"],
            "corpus_access_log_sha256": hashes["corpus-access.log"],
            "reason": None,
            "verdict": dns_verdict,
        }
        evidence.update(evidence_overrides or {})
        paths["dns_evidence"] = os.path.join(run_dir, "dns-evidence.json")
        with open(paths["dns_evidence"], "w") as handle:
            json.dump(evidence, handle)
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
        self.assertEqual(cell["seal"], SEAL)
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

    def test_an_unarmed_stall_gate_refuses(self):
        """reqbench.sh runs reqanalyze without --stall-max-ms, and the
        analyzer then writes stall_gate {max_ms: null, passed: true,
        evaluated: 0}. A pass from a gate that evaluated nothing is not a
        pass; the index must refuse it rather than print stall_gate_passed.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, stall_max_ms=None, stall_evaluated=0)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("stall_gate", text)
            self.assertIn("--stall-max-ms", text)

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

    def test_an_armed_gate_that_evaluated_nothing_refuses(self):
        """max_ms alone is not proof the gate looked at anything.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, stall_evaluated=0)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("evaluated", text)

    def test_a_nan_stall_limit_refuses(self):
        """json.load accepts NaN, and `NaN <= 0` is False, so a NaN limit
        read as armed.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, stall_max_ms=float("nan"))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))

    def test_a_resolver_run_without_dns_evidence_refuses(self):
        """A run whose guest resolved through a baked resolver is a campaign
        run, and the bracket evidence is the only thing that says the
        resolver held for the whole measured run. Absent evidence was
        indexed as dns_verdict null.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, dns_verdict=None, guest_dns="10.0.2.2")
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("dns-evidence.json", text)

    def _refused(self, d, **run_kwargs):
        run_dir = os.path.join(d, "run")
        paths = write_run(run_dir, **run_kwargs)
        out = os.path.join(d, "campaign-x-summary.json")
        rc, text = self._summarize(out, [run_dir])
        self.assertNotEqual(rc, 0, text)
        self.assertFalse(os.path.exists(out))
        return paths, text

    def test_clean_evidence_naming_a_missing_verify_bracket_refuses(self):
        """The verdict is only as good as the brackets it cites.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote ...: 1 cell(s) (x3)
        """
        with tempfile.TemporaryDirectory() as d:
            paths = write_run(os.path.join(d, "run"))
            os.unlink(paths["verify-after-run"])
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [paths["analysis"].rsplit("/", 1)[0]])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("verify-dns-after-run.json", text)
        with tempfile.TemporaryDirectory() as d:
            paths = write_run(os.path.join(d, "run"))
            write_verify(paths["verify-before-run"], passed=False)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [os.path.join(d, "run")])
            self.assertNotEqual(rc, 0, text)
            self.assertIn("verify-dns-before-run.json", text)
        with tempfile.TemporaryDirectory() as d:
            # The evidence lists two brackets; the campaign runs three.
            _paths, text = self._refused(
                d, evidence_overrides={"verify_files": [
                    os.path.join(d, "run", "verify-dns-pre.json"),
                    os.path.join(d, "run", "verify-dns-after-run.json"),
                ]},
            )
            self.assertIn("before-run", text)

    def test_clean_evidence_whose_replay_log_changed_refuses(self):
        """The sha256 in the evidence pins the replay logs; a log that no
        longer matches is a log something appended to after the verdict.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote ...: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            with open(paths["corpus-dns.log"], "a") as handle:
                handle.write('{"ts": 2.0, "qname": "late.example"}\n')
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("corpus-dns.log", text)

    def test_clean_evidence_without_samples_or_a_live_sampler_refuses(self):
        """RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote ...: 1 cell(s) (x4)"""
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(d, evidence_overrides={"samples": 0})
            self.assertIn("samples", text)
        with tempfile.TemporaryDirectory() as d:
            # samples claims more lines than the owner log holds.
            _paths, text = self._refused(d, evidence_overrides={"samples": 13})
            self.assertIn("dns-owner.log", text)
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(
                d, evidence_overrides={"sampler_alive_at_stop": False},
            )
            self.assertIn("sampler", text)
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(
                d, evidence_overrides={"dnsmasq_state_after_restore": "unknown"},
            )
            self.assertIn("dnsmasq", text)

    def test_the_hash_names_the_bytes_that_were_parsed(self):
        """Parsing a file and hashing it later are two reads; an atomic
        replacement in between produced a cell from one generation and a
        hash for another. One read feeds both.

        RED BEFORE THE FIX: AttributeError: module 'campaign_summary' has no
        attribute 'read_bytes' (the parse and the hash were separate opens).
        """
        from unittest import mock
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            with open(paths["analysis"], "rb") as handle:
                parsed_bytes = handle.read()
            real_read = campaign_summary.read_bytes

            def read_then_replace(path):
                data = real_read(path)
                if path == paths["analysis"]:
                    replacement = json.loads(data)
                    replacement["run_id"] = "1" * 32
                    with open(path, "w") as handle:
                        json.dump(replacement, handle)
                return data

            out = os.path.join(d, "campaign-x-summary.json")
            with mock.patch.object(campaign_summary, "read_bytes", read_then_replace):
                rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                index = json.load(handle)
        entry = next(
            entry for entry in index["generated_from"]
            if entry["path"] == paths["analysis"]
        )
        self.assertEqual(entry["sha256"], hashlib.sha256(parsed_bytes).hexdigest())
        self.assertEqual(index["cells"][0]["run_id"], "0" * 32)

    def test_a_refused_rerun_removes_the_stale_index(self):
        """`REFUSED: no index written` left the previous index in place, so
        a reader who opened it after a failed re-index quoted cells the
        refusal had just rejected.

        RED BEFORE THE FIX: AssertionError: True is not false : the stale index survived
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(paths["dns_evidence"]) as handle:
                evidence = json.load(handle)
            evidence["verdict"] = "unclean"
            with open(paths["dns_evidence"], "w") as handle:
                json.dump(evidence, handle)
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0)
            self.assertFalse(os.path.exists(out), "the stale index survived")
            self.assertIn("removed", text)

    def test_dns_evidence_that_is_not_a_json_object_refuses(self):
        """A dns-evidence.json holding valid JSON that is not an object ([]
        here) was read as a missing verdict by the inline guard on the
        verdict line, so the refusal said `verdict is None, not 'clean'`
        about a file that has no verdict field at all, and every .get()
        after that line relied on the same guard. Non-object evidence is now
        rejected first, by name.

        RED BEFORE THE FIX: AssertionError: 'dns-evidence.json is not a JSON
        object' not found in "REFUSED: no index written\n  - .../run:
        dns-evidence.json verdict is None, not 'clean'\n  removed stale index
        .../campaign-x-summary.json\n"
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(paths["dns_evidence"], "w") as handle:
                handle.write("[]\n")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out), "the stale index survived")
            self.assertIn(run_dir, text)
            self.assertIn("dns-evidence.json is not a JSON object", text)

    def test_a_cell_without_its_seal_identity_refuses(self):
        """publishable=true was the only publication-state proof the index
        took. The rule (REVIEW.md) is to quote only sealed runs, and the
        seal is the cell's identity: reqbench.sh's runtime bundle hash, the
        fcvm and harness hashes sealed into it, the source revision, the
        image ID and the snapshot generation with its config digest. A cell
        missing any of them, or carrying one blank, was indexed.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s) (seven missing-field subtests
        and the blank one)
        """
        for field in SEAL:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as d:
                _paths, text = self._refused(d, cell_overrides={field: None})
                self.assertIn(field, text)
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(d, cell_overrides={"source_revision": "  "})
            self.assertIn("source_revision", text)

    def test_a_withdrawn_run_refuses_with_its_reason(self):
        """Withdrawn runs stay unquotable forever (REVIEW.md), and nothing in
        a run directory said so to the index. A file named WITHDRAWN in the
        run directory withdraws it; its first line is the reason and the
        refusal quotes it. An analysis.json carrying "withdrawn": true is
        refused the same way.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s) (both cases)
        """
        reason = "measured on a tree without pasta's pdeathsig"
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(d, withdrawn=reason)
            self.assertIn("withdrawn", text)
            self.assertIn(reason, text)
            self.assertNotIn("second line", text)
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(d, analysis_overrides={"withdrawn": True})
            self.assertIn("withdrawn", text)

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
