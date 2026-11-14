#!/usr/bin/env python3
"""Tests of fleet/ami/release_gate.py and the workflow that runs it (fleet-release-gate.yml)."""
import json
import os
import pathlib
import re
import sys
import tempfile
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
import release_gate  # noqa: E402

WORKFLOW = (ROOT / ".github/workflows/fleet-release-gate.yml").read_text()


class Lambda:
    """`aws lambda invoke` that records each payload and answers from a list."""

    def __init__(self, *answers):
        self.answers, self.payloads = list(answers), []

    def __call__(self, argv, **_):
        assert argv[:5] == ["aws", "lambda", "invoke", "--function-name", release_gate.FUNCTION], argv
        self.payloads.append(json.loads(argv[argv.index("--payload") + 1]))
        pathlib.Path(argv[-1]).write_text(json.dumps(self.answers.pop(0)))
        return mock.Mock(returncode=0, stdout=json.dumps({"StatusCode": 200}))


class GateTests(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(os.environ, {"RELEASE_GATE": release_gate.FUNCTION}, clear=False)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_a_smoke_that_failed_on_either_family_rolls_the_generation_back(self):
        gate = Lambda({"status": "rolled_back", "generation": 16})
        self.assertEqual(release_gate.main(["decide", "--generation", "15", "--smoke", "failure"], run=gate), 0)
        self.assertEqual(gate.payloads, [{"action": "rollback", "generation": 15}])

    def test_a_smoke_that_passed_on_both_families_verifies_the_generation(self):
        gate = Lambda({"status": "verified", "generation": 15})
        self.assertEqual(release_gate.main(["decide", "--generation", "15", "--smoke", "success"], run=gate), 0)
        self.assertEqual(gate.payloads, [{"action": "verify", "generation": 15}])

    def test_a_cancelled_or_skipped_smoke_decides_nothing(self):
        for smoke in ("cancelled", "skipped"):
            gate = Lambda()
            self.assertEqual(release_gate.main(["decide", "--generation", "15", "--smoke", smoke], run=gate), 0)
            self.assertEqual(gate.payloads, [])

    def test_a_refused_rollback_fails_the_job(self):
        gate = Lambda({"status": "refused", "reason": "the live release has no previous release to roll back to"})
        with self.assertRaisesRegex(SystemExit, "no previous release"):
            release_gate.main(["decide", "--generation", "15", "--smoke", "failure"], run=gate)

    def test_status_writes_the_generation_and_whether_it_was_verified(self):
        with tempfile.NamedTemporaryFile("r") as out, mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}):
            gate = Lambda({"generation": 15, "source_sha": "a" * 40, "verified": False, "rollback": False})
            self.assertEqual(release_gate.main(["status"], run=gate), 0)
            self.assertEqual(out.read(), "generation=15\nverified=false\n")
        self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_only_the_gate_function_is_invoked(self):
        with mock.patch.dict(os.environ, {"RELEASE_GATE": "fcvm-fleet-ami-publish"}):
            with self.assertRaisesRegex(SystemExit, "must name"):
                release_gate.main(["status"], run=Lambda())

    def test_the_workflow_smokes_an_unverified_generation_and_reports_the_result(self):
        # The smoke runs only for an unverified generation, and its result, whatever it is, reaches
        # the decision: a failed smoke must not skip the job that rolls the generation back.
        self.assertRegex(WORKFLOW, r"(?m)^  smoke:\n(?:    .*\n)*?    if: needs\.status\.outputs\.verified == 'false'\n")
        self.assertRegex(WORKFLOW, r"(?m)^    uses: \./\.github/workflows/fleet-viewer-smoke\.yml\n    secrets: inherit$")
        decide = WORKFLOW.split("\n  decide:\n", 1)[1]
        self.assertIn("needs: [status, smoke]", decide)
        self.assertIn("if: ${{ !cancelled() && needs.status.outputs.verified == 'false' }}", decide)
        self.assertIn("SMOKE: ${{ needs.smoke.result }}", decide)
        self.assertIn('release_gate.py decide --generation "$GENERATION" --smoke "$SMOKE"', decide)
        smoke = (ROOT / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        self.assertRegex(smoke, r"(?m)^  workflow_call:$")


if __name__ == "__main__":
    unittest.main()
