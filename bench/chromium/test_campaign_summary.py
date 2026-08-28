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
import unittest.mock
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

    RED BEFORE THE THIRD FIX: campaign_summary requires corpus-dns.log and
    corpus-access.log beside dns-evidence.json and verifies their sha256
    before indexing a run, but both matched `results/**`, so
    `results/run-x/corpus-dns.log is ignored and would be lost on the next
    wipe` (two subtests) and the two literal negations were absent.
    """

    IGNORE = os.path.join(HERE, ".gitignore")
    NEGATIONS = (
        "!results/**/verify-dns.json",
        "!results/**/verify-dns-*.json",
        "!results/**/dns-evidence.json",
        "!results/**/diag/summary.json",
        "!results/**/dns-owner.log",
        "!results/**/corpus-dns.log",
        "!results/**/corpus-access.log",
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
        "results/run-x/corpus-dns.log",
        "results/run-x/corpus-access.log",
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
# write_run's diag default: a passing summary for a resolver run, none for a
# fixture run. Distinct from None, which omits the file on purpose.
DIAG_DEFAULT = object()


# The snapshot generation the fixture run measured and its diag diagnosed:
# the one SEAL names, so a diag_summary() binds to write_run's cell.
GENERATION_ID = SEAL["snapshot_generation_id"]
CONFIG_SHA256 = SEAL["snapshot_config_sha256"]


def diag_summary(urls=("https://example.com/",), passed=True, violations=(),
                 max_load_ms=812.5, engine="chromium", **identity):
    """diag/summary.json in the shape reqbench.sh diag writes. identity
    overrides the fields campaign_summary binds to the cell, and
    uffd_prefetch, which it holds to "off" (the diag's serve never records
    a working set; null on the file backend, which has no serve), and
    runtime_bundle_sha256, the sealed runtime the diag rendered from, which
    it holds to the run's seal."""
    return {
        "engine": engine,
        "tag": identity.get("tag", "cb-req-corpus"),
        "backend": identity.get("backend", "uffd"),
        "uffd_mode": identity.get("uffd_mode", "minor"),
        "uffd_prefetch": identity.get("uffd_prefetch", "off"),
        "snapshot_generation_id": identity.get("snapshot_generation_id", GENERATION_ID),
        "snapshot_config_sha256": identity.get("snapshot_config_sha256", CONFIG_SHA256),
        "runtime_bundle_intact": identity.get("runtime_bundle_intact", True),
        "runtime_bundle_sha256": identity.get(
            "runtime_bundle_sha256", SEAL["runtime_bundle_sha256"]),
        "reps": 3,
        "urls": {
            url: {"reps": 3, "renders_ok": 3, "max_load_ms": max_load_ms,
                  "max_pending_at_load": 2, "remote_ips": {"10.0.2.2": 42},
                  "errors": {}}
            for url in urls
        },
        "violations": list(violations),
        "teardown_failures": 0,
        "passed": passed,
        "limits": {"expect_ips": ["10.0.2.2"], "max_load_ms": 15000},
        "timestamp": "2026-08-28T00:00:00Z",
    }


def write_verify(path, passed=True, **overrides):
    """One HOP D evidence file in reqbench.sh's shape; overrides rewrite
    fields on top of a bracket that resolved every corpus host and URL
    through the replay resolver with the proxy variables ignored (hosts,
    for a bracket whose resolver answered elsewhere)."""
    record = {
        "dns_server": "10.0.2.2",
        "resolv_conf_vm": "nameserver 10.0.2.2\n",
        "resolv_conf_container": "nameserver 10.0.2.2\n",
        "hosts": {"example.com": {"answer": "10.0.2.2", "ok": passed}},
        "urls": {"https://example.com/": {"status": 200, "ok": passed,
                                          "proxy_env_ignored": []}},
        "proxies_disabled": True,
        "timestamp": "2026-08-28T00:00:00Z",
        "passed": passed,
    }
    record.update(overrides)
    with open(path, "w") as handle:
        json.dump(record, handle)


def write_run(
    run_dir,
    *,
    publishable=True,
    stall_passed=True,
    dns_verdict="clean",
    diag=DIAG_DEFAULT,
    guest_dns="10.0.2.2",
    guest_env=(),
    engine="chromium",
    stall_max_ms=15000,
    stall_evaluated=404,
    samples=12,
    load_max_1min=0.42,
    evidence_overrides=None,
    verify_overrides=None,
    verify_stage_overrides=None,
    cell_overrides=None,
    analysis_overrides=None,
    withdrawn=None,
):
    """A minimal run directory shaped like reqanalyze + the campaign evidence.

    dns_verdict=None omits dns-evidence.json and everything it names; diag=None
    omits diag/summary.json, and the default writes a passing one for a
    resolver run (guest_dns set) and none for a fixture run. stall_max_ms=None
    is what reqanalyze writes when
    it ran without --stall-max-ms (passed true, evaluated 0). The evidence
    names three passing verify brackets with their real sha256, an owner log
    with `samples` lines and the two replay logs with their real sha256, the
    way corpus_campaign.sh writes them; evidence_overrides rewrites evidence
    fields on top, verify_overrides rewrites bracket fields on every bracket
    before they are hashed, and verify_stage_overrides rewrites them on one
    named stage.
    load_max_1min is the 1-min load the campaign's sampler recorded, carried
    on every owner line and reported in the evidence; None writes an owner
    log and evidence from before the sampler recorded it.
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
        "guest_env": list(guest_env),
        "url": "https://example.com/",
        "snapshot": "cb-req-corpus",
        **SEAL,
    }
    for field, value in (cell_overrides or {}).items():
        if value is None:
            cell.pop(field, None)
        else:
            cell[field] = value
    if diag is DIAG_DEFAULT:
        diag = diag_summary(engine=engine) if guest_dns is not None else None
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
        verify_hashes = {}
        for stage in VERIFY_STAGES:
            verify_path = os.path.join(run_dir, f"verify-dns-{stage}.json")
            overrides = dict(verify_overrides or {})
            overrides.update((verify_stage_overrides or {}).get(stage, {}))
            write_verify(verify_path, **overrides)
            paths[f"verify-{stage}"] = verify_path
            verify_files.append(verify_path)
            verify_hashes[f"verify-dns-{stage}.json"] = sha256_file(verify_path)
        hashes = {}
        for name in CORPUS_LOGS:
            log_path = os.path.join(run_dir, name)
            with open(log_path, "w") as handle:
                handle.write('{"ts": 1.0, "qname": "example.com"}\n')
            paths[name] = log_path
            hashes[name] = sha256_file(log_path)
        owner_log = os.path.join(run_dir, "dns-owner.log")
        load_column = "" if load_max_1min is None else f" load1={load_max_1min}"
        with open(owner_log, "w") as handle:
            handle.write(
                f"2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive{load_column}\n"
                * samples
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
            "verify_file_sha256": verify_hashes,
            "corpus_dns_log_sha256": hashes["corpus-dns.log"],
            "corpus_access_log_sha256": hashes["corpus-access.log"],
            "corpus_serve_exit_status": 0,
            "reason": None,
            "verdict": dns_verdict,
        }
        if load_max_1min is not None:
            evidence["load_max_1min"] = load_max_1min
            evidence["load_samples"] = samples
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
            paths = write_run(run_dir)
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
        self.assertEqual(cell["diag"], {
            "diag_passed": True, "violations_count": 0,
            "max_load_ms": {"https://example.com/": 812.5},
        })

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

    def test_the_maximum_load_is_carried_into_the_cell(self):
        """The campaign samples the 1-min load through the measured run and
        dns-evidence.json reports the maximum as load_max_1min; the index
        carries it so a reader sees it beside the numbers without opening
        the run. Evidence from before the sampler recorded it indexes with
        null.

        RED BEFORE THE FIX: KeyError: 'load_max_1min'.
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, load_max_1min=1.87)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                index = json.load(handle)
        self.assertEqual(index["cells"][0]["load_max_1min"], 1.87)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, load_max_1min=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                index = json.load(handle)
        self.assertIsNone(index["cells"][0]["load_max_1min"])

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

    def test_diag_fields_flow_into_the_cell(self):
        """The cell carries the diag's verdict, its violation count and the
        slowest load event per URL, and the index names the summary among
        the files it was generated from.

        Watched red 2026-08-28 at 55d6fb7d: the cell's diag was the whole
        summary object (`AssertionError: {'engine': 'chromium', ...} != {'diag_passed': True, ...}`).
        """
        urls = ("https://example.com/", "https://news.ycombinator.com/")
        summary = diag_summary(urls=urls)
        summary["urls"]["https://news.ycombinator.com/"]["max_load_ms"] = 2210.0
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir, diag=summary)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                index = json.load(handle)
        cell = index["cells"][0]
        self.assertEqual(cell["diag"], {
            "diag_passed": True,
            "violations_count": 0,
            "max_load_ms": {"https://example.com/": 812.5,
                            "https://news.ycombinator.com/": 2210.0},
        })
        self.assertIn(paths["diag"], {entry["path"] for entry in index["generated_from"]})

    def test_a_corpus_cell_without_its_diag_is_refused(self):
        """A run whose guest resolved through the baked resolver had the
        diag run before it; a run directory without the summary is a run
        nobody diagnosed, and it is not indexed.

        Watched red 2026-08-28 at 55d6fb7d: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("diag/summary.json", text)

    def test_a_failed_diag_refuses(self):
        """Watched red 2026-08-28 at 55d6fb7d: AssertionError: 0 == 0 (a
        summary saying passed=false with a remote_ip violation was indexed)."""
        violation = {"url": "https://example.com/", "rep": 2, "kind": "remote_ip",
                     "detail": "93.184.216.34 served 3 request(s), first https://example.com/"}
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=diag_summary(passed=False, violations=[violation]))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("diag", text)
            self.assertIn("remote_ip", text)

    def test_a_diag_that_skipped_a_measured_url_is_refused(self):
        """A diag over other pages says nothing about the pages this run
        measured; every URL in the cell must appear in the summary."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=diag_summary(urls=("https://other.example/",)))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("https://example.com/", text)

    def test_a_diag_beside_a_cell_that_names_no_url_is_refused(self):
        """The coverage check compares the diag's urls with the cell's url
        list; a cell without one gives it nothing to compare, and a diag
        over any pages would pass. That is a refusal, not a pass."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir, diag=diag_summary(urls=("https://other.example/",)))
            with open(paths["analysis"]) as handle:
                analysis = json.load(handle)
            del analysis["cell"]["url"]
            with open(paths["analysis"], "w") as handle:
                json.dump(analysis, handle)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("url", text)

    def test_a_diag_of_another_snapshot_or_setup_is_refused(self):
        """The diag sits beside the run it vouches for; a summary naming
        another snapshot generation, config, tag, engine, backend or UFFD
        mode diagnosed something else and is not this run's evidence."""
        cases = {
            "snapshot_generation_id": "87654321-4321-4321-8321-cba987654321",
            "snapshot_config_sha256": "b" * 64,
            "tag": "cb-req-other",
            "engine": "webkit",
            "backend": "file",
            "uffd_mode": "major",
        }
        for field, other in cases.items():
            with self.subTest(field=field), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, diag=diag_summary(**{field: other}))
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{field}: {text}")
                self.assertFalse(os.path.exists(out))
                self.assertIn(field, text)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            summary = diag_summary()
            del summary["snapshot_generation_id"]
            write_run(run_dir, diag=summary)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, "a diag naming no generation was accepted")
            self.assertIn("snapshot_generation_id", text)

    def test_a_diag_without_a_load_event_for_a_measured_url_is_refused(self):
        """A passing diag has a load event for every rep of every URL; a
        null max_load_ms is a summary the index cannot quote a load from."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=diag_summary(max_load_ms=None))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("max_load_ms", text)

    def test_a_diag_whose_bundle_changed_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=diag_summary(runtime_bundle_intact=False))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertIn("runtime_bundle_intact", text)

    def test_a_diag_from_another_sealed_runtime_is_refused(self):
        """runtime_bundle_intact says the bundle did not change under the
        diag, not that it is the bundle the run measured from. A later
        standalone diag, staged from edited sources, writes its summary
        beside an earlier run and passes every other identity check, so the
        summary names its own sealed bundle and the index holds that to the
        run's seal. A summary carrying none is refused rather than read as
        this run's.
        """
        missing = diag_summary()
        missing.pop("runtime_bundle_sha256", None)
        cases = (
            ("another bundle", diag_summary(runtime_bundle_sha256="9" * 64)),
            ("no bundle at all", missing),
            ("null", diag_summary(runtime_bundle_sha256=None)),
        )
        for label, diag in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, diag=diag)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{label}: indexed")
                self.assertFalse(os.path.exists(out))
                self.assertIn("runtime_bundle_sha256", text)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, diag=diag_summary(
                runtime_bundle_sha256=SEAL["runtime_bundle_sha256"]))
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)

    def test_a_diag_whose_serve_could_have_recorded_its_renders_is_refused(self):
        """The diag's clones fault the golden's pages. A UFFD serve with
        working-set replay on records those faults into memory.bin.working-set
        beside the golden, and the measured run replays that file, so the run
        would restore the diag's working set instead of the golden's own. The
        diag serves with --uffd-prefetch off and records "off" (null on the
        file backend, which has no serve to carry the knob); a summary saying
        anything else, or one from before the field existed, is not evidence
        that the run measured the golden's own working set.

        Watched red 2026-08-28 at 8cd77713: every refused case was indexed
        (rc 0, `AssertionError: 0 == 0`).
        """
        file_cell = {"backend": "file", "uffd_mode": "file"}
        missing = diag_summary()
        del missing["uffd_prefetch"]
        cases = (
            ("on", diag_summary(uffd_prefetch="on"), None),
            ("null on the uffd backend", diag_summary(uffd_prefetch=None), None),
            ("missing", missing, None),
            ("on on the file backend",
             diag_summary(backend="file", uffd_mode="file", uffd_prefetch="on"), file_cell),
            ("off on the file backend, which had no serve",
             diag_summary(backend="file", uffd_mode="file", uffd_prefetch="off"), file_cell),
        )
        for label, diag, cell in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, diag=diag, cell_overrides=cell)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{label}: indexed")
                self.assertFalse(os.path.exists(out))
                self.assertIn("uffd_prefetch", text)
        # The two shapes the diag writes are the two that index.
        for label, diag, cell in (
            ("off on uffd", diag_summary(), None),
            ("null on file",
             diag_summary(backend="file", uffd_mode="file", uffd_prefetch=None), file_cell),
        ):
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, diag=diag, cell_overrides=cell)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertEqual(rc, 0, text)
                with open(out) as handle:
                    self.assertIs(json.load(handle)["cells"][0]["diag"]["diag_passed"], True)

    def test_a_diag_whose_limits_were_not_armed_is_refused(self):
        """passed=true from a diag run without DIAG_EXPECT_IPS or
        DIAG_MAX_LOAD_MS says only that nothing it was asked to check went
        wrong: every remote address was allowed, no load event was held to
        a limit. A standalone diag over the same RESULTS replaces the
        campaign's summary with one shaped like that, so the index holds
        the summary's limits to the run: expect_ips is the address set the
        run's records name, max_load_ms is a positive integer at or under
        the run's own stall gate, and every measured URL rendered reps
        times.

        Watched red 2026-08-28 at 51527021: every case was indexed
        (`AssertionError: 0 == 0`).
        """
        def with_limits(**limits):
            summary = diag_summary()
            summary["limits"].update(limits)
            return summary

        no_limits = diag_summary()
        del no_limits["limits"]
        unshaped = diag_summary()
        unshaped["limits"] = "armed"
        half = diag_summary()
        del half["limits"]["max_load_ms"]
        no_reps = diag_summary()
        no_reps["reps"] = 0
        short = diag_summary()
        short["urls"]["https://example.com/"]["renders_ok"] = 2
        cases = (
            ("no limits at all", no_limits, "limits"),
            ("limits not an object", unshaped, "limits"),
            ("limits without max_load_ms", half, "limits"),
            ("expect_ips null", with_limits(expect_ips=None), "expect_ips"),
            ("expect_ips empty", with_limits(expect_ips=[]), "expect_ips"),
            ("expect_ips not a list", with_limits(expect_ips="10.0.2.2"), "expect_ips"),
            ("expect_ips not an address", with_limits(expect_ips=["replay"]), "expect_ips"),
            ("expect_ips another address",
             with_limits(expect_ips=["93.184.216.34"]), "expect_ips"),
            ("expect_ips a superset",
             with_limits(expect_ips=["10.0.2.2", "93.184.216.34"]), "expect_ips"),
            ("max_load_ms null", with_limits(max_load_ms=None), "max_load_ms"),
            ("max_load_ms zero", with_limits(max_load_ms=0), "max_load_ms"),
            ("max_load_ms negative", with_limits(max_load_ms=-15000), "max_load_ms"),
            ("max_load_ms a float", with_limits(max_load_ms=15000.0), "max_load_ms"),
            ("max_load_ms a string", with_limits(max_load_ms="15000"), "max_load_ms"),
            ("max_load_ms a bool", with_limits(max_load_ms=True), "max_load_ms"),
            ("max_load_ms above the run's stall gate",
             with_limits(max_load_ms=15001), "stall_gate"),
            ("reps zero", no_reps, "reps"),
            ("a measured url rendered short of reps", short, "renders_ok"),
        )
        for label, diag, keyword in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, diag=diag)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{label}: indexed")
                self.assertFalse(os.path.exists(out))
                self.assertIn(keyword, text, label)

    def test_a_diag_limit_at_or_under_the_run_s_stall_gate_is_accepted(self):
        """The positive control for the limit rule: the campaign hands the
        diag the same 15 s it arms the run's stall gate with, and a
        stricter diag limit, or a looser run gate, is still a diag held at
        least as tightly as the run."""
        for label, diag_limit, stall in (
            ("equal", 15000, 15000),
            ("stricter than the gate", 5000, 15000),
            ("under a looser gate", 15000, 20000),
            ("under a fractional gate", 15000, 15000.5),
        ):
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                summary = diag_summary()
                summary["limits"]["max_load_ms"] = diag_limit
                write_run(run_dir, diag=summary, stall_max_ms=stall)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertEqual(rc, 0, text)
                with open(out) as handle:
                    self.assertIs(json.load(handle)["cells"][0]["diag"]["diag_passed"], True)

    def test_the_diag_s_expected_addresses_are_the_ones_the_run_s_records_name(self):
        """The address set the diag must have been held to comes from the
        run's own records, not from a constant: the resolver answers the
        verify brackets recorded inside the restored clone, the
        BENCH_RESOLVE_ALL_TO address a resolver-rule golden baked, and the
        IP-literal hosts of the measured URLs. A run whose records name no
        address gives the index nothing to hold expect_ips to, and its diag
        is refused rather than trusted; a passing bracket whose host was not
        ok, or whose answer is not an address, is refused as a record the
        index cannot read an answer from.

        Watched red 2026-08-28 at 51527021: every refused case was indexed
        (`AssertionError: 0 == 0`).
        """
        def diag_for(ips, **kwargs):
            summary = diag_summary(**kwargs)
            summary["limits"]["expect_ips"] = ips
            return summary

        def brackets(host):
            return {"hosts": {"example.com": host}}

        rule = ("BENCH_RESOLVE_ALL_TO=10.0.2.7",)
        fixture_url = "http://127.0.0.1:8000/medium.html"
        fixture = {"url": fixture_url}
        accepted = (
            ("brackets that answered 10.0.2.9",
             dict(verify_overrides=brackets({"answer": "10.0.2.9", "ok": True}),
                  diag=diag_for(["10.0.2.9"]))),
            ("a resolver rule naming 10.0.2.7",
             dict(dns_verdict=None, guest_dns=None, guest_env=rule,
                  diag=diag_for(["10.0.2.7"]))),
            ("a fixture url on the host loopback",
             dict(dns_verdict=None, guest_dns=None, cell_overrides=fixture,
                  diag=diag_for(["127.0.0.1"], urls=(fixture_url,)))),
        )
        for label, kwargs in accepted:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, **kwargs)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertEqual(rc, 0, f"{label}: {text}")
        refused = (
            ("the replay answer where the brackets answered 10.0.2.9",
             dict(verify_overrides=brackets({"answer": "10.0.2.9", "ok": True}),
                  diag=diag_for(["10.0.2.2"])), "10.0.2.9"),
            ("the replay answer where the rule names 10.0.2.7",
             dict(dns_verdict=None, guest_dns=None, guest_env=rule,
                  diag=diag_for(["10.0.2.2"])), "10.0.2.7"),
            ("the replay answer for a fixture url on the host loopback",
             dict(dns_verdict=None, guest_dns=None, cell_overrides=fixture,
                  diag=diag_for(["10.0.2.2"], urls=(fixture_url,))), "127.0.0.1"),
            ("a live cell whose records name no address",
             dict(dns_verdict=None, guest_dns=None, diag=diag_for(["93.184.216.34"])),
             "no address"),
            ("brackets that checked no host",
             dict(verify_overrides={"hosts": {}}, diag=diag_for(["10.0.2.2"])),
             "no address"),
            ("a bracket host that was not ok under passed=true",
             dict(verify_overrides=brackets({"answer": "10.0.2.2", "ok": False}),
                  diag=diag_for(["10.0.2.2"])), "ok"),
            ("a bracket answer that is not an address",
             dict(verify_overrides=brackets({"answer": "<exec rc=1>", "ok": True}),
                  diag=diag_for(["10.0.2.2"])), "answer"),
        )
        for label, kwargs, keyword in refused:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, **kwargs)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{label}: indexed")
                self.assertFalse(os.path.exists(out))
                self.assertIn(keyword, text, label)

    def test_a_webkit_diag_carries_no_ip_expectation_and_still_needs_its_load_limit(self):
        """reqbench refuses DIAG_EXPECT_IPS on webkit (its render carries no
        trace to hold it to), so a webkit summary records expect_ips null
        and the verify brackets hold the resolver; one carrying an address
        list was not written by the diag. max_load_ms is held as on
        Chromium.

        Watched red 2026-08-28 at 51527021: the address list and the null
        limit were both indexed (`AssertionError: 0 == 0`).
        """
        def webkit(**limits):
            summary = diag_summary(engine="webkit")
            summary["limits"].update(expect_ips=None)
            summary["limits"].update(limits)
            return summary

        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, engine="webkit", diag=webkit())
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
        for label, diag, keyword in (
            ("an address list on webkit", webkit(expect_ips=["10.0.2.2"]), "expect_ips"),
            ("no load limit on webkit", webkit(max_load_ms=None), "max_load_ms"),
        ):
            with self.subTest(case=label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                write_run(run_dir, engine="webkit", diag=diag)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, f"{label}: indexed")
                self.assertFalse(os.path.exists(out))
                self.assertIn(keyword, text, label)

    def test_a_resolver_rule_cell_needs_its_diag(self):
        """A golden with GUEST_ENV=BENCH_RESOLVE_ALL_TO=<ip> resolves through
        Chromium's rule, not resolv.conf, so its runs carry guest_dns null;
        the diag is what says the pages came from the replay, and a run
        without it is not indexed. The index carries guest_env so a reader
        can tell the two corpus arms apart."""
        rule = ["BENCH_RESOLVE_ALL_TO=10.0.2.2"]
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, dns_verdict=None, guest_dns=None, guest_env=rule, diag=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, "a resolver-rule run without its diag was indexed")
            self.assertFalse(os.path.exists(out))
            self.assertIn("diag/summary.json", text)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, dns_verdict=None, guest_dns=None, guest_env=rule,
                      diag=diag_summary())
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                cell = json.load(handle)["cells"][0]
            self.assertEqual(cell["guest_env"], rule)
            self.assertIsNone(cell["guest_dns"])
            self.assertIs(cell["diag"]["diag_passed"], True)

    def test_a_fixture_run_keeps_its_previous_shape(self):
        """No resolver, no diag: the medium.html runs index as before."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, dns_verdict=None, guest_dns=None, diag=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
            with open(out) as handle:
                cell = json.load(handle)["cells"][0]
        self.assertIsNone(cell["diag"])
        self.assertIsNone(cell["dns_verdict"])

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

    def test_a_verify_bracket_rewritten_after_the_verdict_refuses(self):
        """The bracket file is the whole record that a restored clone
        resolved the corpus through the replay server, and it is a plain file
        in the run directory. A bracket saying the clone resolved through
        8.8.8.8, with the proxy probes disabled=false and no host or URL
        checked at all, still carried passed=true and was indexed clean.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            write_verify(paths["verify-before-run"], dns_server="8.8.8.8",
                         proxies_disabled=False, hosts={}, urls={})
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("verify-dns-before-run.json", text)

    def test_evidence_that_does_not_pin_its_brackets_refuses(self):
        """Fail closed: evidence written before the verdict hashed its
        brackets cannot say whether the files beside it are the ones it read,
        so it is not evidence the index can revalidate.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            with open(paths["dns_evidence"]) as handle:
                evidence = json.load(handle)
            del evidence["verify_file_sha256"]
            with open(paths["dns_evidence"], "w") as handle:
                json.dump(evidence, handle)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("verify_file_sha256", text)

    # Each shape is a bracket the campaign would have refused at the verdict,
    # written with its own hash so only the index-time revalidation can catch
    # it: the resolver under test, the proxy state of the URL probes, and
    # that anything was checked at all.
    BRACKETS_THAT_PROVE_NOTHING = (
        ({"hosts": {}}, "no resolved host"),
        ({"urls": {}}, "no fetched URL"),
        ({"proxies_disabled": False}, "proxies_disabled"),
        ({"dns_server": "8.8.8.8",
          "hosts": {"example.com": {"answer": "8.8.8.8", "ok": True}}}, "8.8.8.8"),
        ({"hosts": {"example.com": {"answer": "93.184.216.34", "ok": True}}},
         "93.184.216.34"),
        ({"urls": {"https://example.com/": {"status": 500, "ok": True,
                                            "proxy_env_ignored": []}}}, "500"),
        ({"urls": {"https://example.com/": {"status": 200, "ok": True,
                                            "proxy_env_ignored": None}}},
         "proxy variables"),
    )

    def test_a_bracket_that_proves_nothing_refuses(self):
        """passed=true is also what HOP D writes when it was given nothing to
        check, so the index holds every bracket to what the campaign asserted
        when it ran it.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s), on all seven shapes.
        """
        for overrides, expected in self.BRACKETS_THAT_PROVE_NOTHING:
            with self.subTest(bracket=overrides), tempfile.TemporaryDirectory() as d:
                _paths, text = self._refused(d, verify_overrides=overrides)
                self.assertIn("verify-dns-", text)
                self.assertIn(expected, text)

    def test_brackets_that_disagree_on_the_resolver_refuse(self):
        """A cell with no guest_dns has no resolver to hold the brackets to,
        so the first bracket's is the reference and the other two are held to
        it: three brackets naming three resolvers describe three different
        runs. Each bracket carries its own sha256, so only the index-time
        revalidation can see this. The agreeing run is the control.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s)
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir, guest_dns=None)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertEqual(rc, 0, text)
        with tempfile.TemporaryDirectory() as d:
            _paths, text = self._refused(d, guest_dns=None, verify_stage_overrides={
                "after-run": {
                    "dns_server": "8.8.8.8",
                    "hosts": {"example.com": {"answer": "8.8.8.8", "ok": True}},
                },
            })
            self.assertIn("verify-dns-after-run.json", text)
            self.assertIn("8.8.8.8", text)

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

    def test_evidence_without_the_replay_server_exit_status_refuses(self):
        """corpus_serve exits 1 after a log line it could not write, with the
        response bytes already sent; the campaign records the status the
        server's wrapper leaves as corpus_serve_exit_status. A verdict that
        does not carry status 0 is a verdict over logs that may be short.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s) (every subtest)
        """
        cases = {"exit 1": 1, "exit 137": 137, "null": None, "bool": True, "string": "0"}
        for label, status in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as d:
                _paths, text = self._refused(
                    d, evidence_overrides={"corpus_serve_exit_status": status})
                self.assertIn("corpus_serve", text)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            paths = write_run(run_dir)
            with open(paths["dns_evidence"]) as handle:
                evidence = json.load(handle)
            del evidence["corpus_serve_exit_status"]
            with open(paths["dns_evidence"], "w") as handle:
                json.dump(evidence, handle)
            out = os.path.join(d, "campaign-x-summary.json")
            rc, text = self._summarize(out, [run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn("corpus_serve", text)

    def test_an_owner_log_whose_lines_contradict_the_evidence_refuses(self):
        """The owner log was accepted by line count, and first_mismatch: null
        was taken on trust: a log rewritten to name owner_pid=9999 on every
        line still indexed clean. The index now reads every sample and holds
        it to the rule the campaign applied at the verdict: each names
        serve_pid as the owner of 127.0.0.1:53 with dnsmasq inactive, the
        lines carrying load1 number load_samples and their maximum is
        load_max_1min, and every line parses as a sample. The first
        contradiction refuses the run.

        RED BEFORE THE FIX: AssertionError: 0 == 0 : wrote
        .../campaign-x-summary.json: 1 cell(s) (every subtest)
        """
        good = "2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive load1=0.42\n"
        cases = {
            "another owner on every line": (
                good.replace("owner_pid=4242", "owner_pid=9999") * 12, "9999"),
            "no owner on one line": (
                good * 5 + good.replace("owner_pid=4242", "owner_pid=none") + good * 6,
                "owner_pid=none"),
            "dnsmasq active on one line": (
                good * 11 + good.replace("dnsmasq=inactive", "dnsmasq=active"),
                "dnsmasq=active"),
            "a load above the recorded maximum": (
                good * 3 + good.replace("load1=0.42", "load1=5.00") + good * 8, "5.0"),
            "fewer load samples than recorded": (
                good * 11 + good.replace(" load1=0.42", ""), "load"),
            "a line that is not a sample": (
                good * 11 + "not a sample\n", "not a sample"),
        }
        for label, (log, quoted) in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as d:
                run_dir = os.path.join(d, "run")
                paths = write_run(run_dir)
                with open(paths["owner_log"], "w") as handle:
                    handle.write(log)
                out = os.path.join(d, "campaign-x-summary.json")
                rc, text = self._summarize(out, [run_dir])
                self.assertNotEqual(rc, 0, text)
                self.assertFalse(os.path.exists(out))
                self.assertIn("dns-owner.log", text)
                self.assertIn(quoted, text)
        with tempfile.TemporaryDirectory() as d:
            # Clean evidence naming no server pid has nothing to hold the
            # samples to.
            _paths, text = self._refused(d, evidence_overrides={"serve_pid": None})
            self.assertIn("serve_pid", text)

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

    def test_one_run_listed_under_two_names_refuses(self):
        """`seen` held the argument strings, so `results/run` and a symlink
        to it were loaded as two cells and one experiment was counted twice
        in the campaign. A second name for a run already listed is refused,
        naming both paths, rather than quietly deduped: the caller passed an
        argument list they did not mean, and an index that hid it would be
        quoted by someone who never saw the argument list.

        RED BEFORE THE FIX: rc 0 and 2 cells, both from one run directory.
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir)
            alias = os.path.join(d, "alias")
            os.symlink(run_dir, alias)
            out = os.path.join(d, "index.json")
            rc, text = self._summarize(out, [run_dir, alias])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out),
                             "an index was written over a double-counted run")
            self.assertIn(alias, text)
            self.assertIn(run_dir, text)
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir)
            out = os.path.join(d, "index.json")
            rc, text = self._summarize(out, [run_dir, run_dir])
            self.assertNotEqual(rc, 0, text)
            self.assertIn("more than once", text)
            self.assertFalse(os.path.exists(out))

    def test_one_run_reached_by_two_paths_refuses_on_its_inode(self):
        """A bind mount, unlike a symlink, gives the same directory two
        canonical paths, so the path key alone would let it through. The
        directory identity the filesystem reports (st_dev, st_ino) is the
        second key. Bind-mounting needs root, so the shape is reproduced by
        holding os.path.realpath to the identity function, which is what a
        bind mount does to these two paths.

        RED BEFORE THE FIX: rc 0 and 2 cells, both from one run directory.
        """
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "run")
            write_run(run_dir)
            alias = os.path.join(d, "alias")
            os.symlink(run_dir, alias)
            out = os.path.join(d, "index.json")
            with unittest.mock.patch("os.path.realpath", side_effect=lambda p: p):
                rc, text = self._summarize(out, [run_dir, alias])
            self.assertNotEqual(rc, 0, text)
            self.assertFalse(os.path.exists(out))
            self.assertIn(alias, text)
            self.assertIn(run_dir, text)

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
