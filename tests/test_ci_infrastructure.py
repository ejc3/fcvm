import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "classify_ci_failure.py"
FIXTURES = ROOT / "tests" / "fixtures" / "ci-infrastructure"

CLASSIFIER_SPEC = importlib.util.spec_from_file_location(
    "classify_ci_failure", SCRIPT
)
assert CLASSIFIER_SPEC is not None and CLASSIFIER_SPEC.loader is not None
CLASSIFIER = importlib.util.module_from_spec(CLASSIFIER_SPEC)
CLASSIFIER_SPEC.loader.exec_module(CLASSIFIER)


def classify_fixture(name: str) -> dict:
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--fixture", str(FIXTURES / name)],
        check=False,
        capture_output=True,
        text=True,
        cwd=ROOT,
        timeout=10,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"classifier rejected {name} with {result.returncode}: {result.stderr}"
        )
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError(f"classifier returned invalid JSON: {result.stdout}") from error


class CiInfrastructureClassificationTests(unittest.TestCase):
    def test_log_fetch_uses_the_command_supported_by_old_gh(self) -> None:
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="HTTP/2 200\n\nrunner log", stderr=""
        )
        endpoint = "repos/owner/repo/actions/jobs/17/logs"

        with mock.patch.object(CLASSIFIER, "_run_gh", return_value=completed) as run_gh:
            evidence = CLASSIFIER._fetch_log("owner/repo", 17)

        self.assertEqual(evidence, {"status": "available", "text": completed.stdout})
        run_gh.assert_called_once_with(["api", "--include", endpoint])

    def test_log_fetch_retries_only_after_new_gh_refuses_escapes(self) -> None:
        refused = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr=CLASSIFIER.GH_ESCAPE_REFUSAL
        )
        completed = subprocess.CompletedProcess(
            args=[], returncode=0, stdout="HTTP/2 200\n\n\x1b[31mlog\x1b[0m", stderr=""
        )
        endpoint = "repos/owner/repo/actions/jobs/18/logs"

        with mock.patch.object(
            CLASSIFIER, "_run_gh", side_effect=[refused, completed]
        ) as run_gh:
            evidence = CLASSIFIER._fetch_log("owner/repo", 18)

        self.assertEqual(evidence, {"status": "available", "text": completed.stdout})
        self.assertEqual(
            [call.args[0] for call in run_gh.call_args_list],
            [
                ["api", "--include", endpoint],
                ["api", "--include", "--allow-escape-sequences", endpoint],
            ],
        )

    def test_log_fetch_fails_closed_when_escape_retry_fails(self) -> None:
        refused = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr=CLASSIFIER.GH_ESCAPE_REFUSAL
        )
        retry_failed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="unknown flag"
        )

        with mock.patch.object(
            CLASSIFIER, "_run_gh", side_effect=[refused, retry_failed]
        ):
            evidence = CLASSIFIER._fetch_log("owner/repo", 19)

        self.assertEqual(evidence, {"status": "unavailable"})

    def test_log_fetch_preserves_missing_blob_evidence_without_new_flag(self) -> None:
        missing = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout='HTTP/2 404\n\n{"code":"BlobNotFound"}',
            stderr="gh: HTTP 404: Not Found",
        )

        with mock.patch.object(CLASSIFIER, "_run_gh", return_value=missing) as run_gh:
            evidence = CLASSIFIER._fetch_log("owner/repo", 20)

        self.assertEqual(
            evidence,
            {
                "status": "missing_blob",
                "http_status": 404,
                "error_code": "BlobNotFound",
            },
        )
        run_gh.assert_called_once_with(
            ["api", "--include", "repos/owner/repo/actions/jobs/20/logs"]
        )

    def test_explicit_runner_shutdown_is_rerun_once(self) -> None:
        result = classify_fixture("explicit-runner-shutdown.json")

        self.assertEqual(result["classification"], "infrastructure")
        self.assertTrue(result["rerun_failed_jobs"])
        self.assertEqual(
            [job["kind"] for job in result["jobs"]],
            ["infrastructure_explicit", "derivative"],
        )

    def test_silent_null_step_with_missing_blob_is_infrastructure(self) -> None:
        result = classify_fixture("silent-runner-loss.json")

        self.assertEqual(result["classification"], "infrastructure")
        self.assertTrue(result["rerun_failed_jobs"])
        self.assertEqual(result["jobs"][0]["kind"], "infrastructure_silent")

    def test_genuine_failure_is_not_infrastructure(self) -> None:
        result = classify_fixture("genuine-failure.json")

        self.assertEqual(result["classification"], "not_infrastructure")
        self.assertFalse(result["rerun_failed_jobs"])
        self.assertEqual(result["jobs"][0]["kind"], "genuine")

    def test_mixed_failure_is_not_infrastructure(self) -> None:
        result = classify_fixture("mixed-failure.json")

        self.assertEqual(result["classification"], "not_infrastructure")
        self.assertFalse(result["rerun_failed_jobs"])
        self.assertEqual(
            [job["kind"] for job in result["jobs"]],
            ["infrastructure_explicit", "genuine", "derivative"],
        )

    def test_attempt_two_is_classified_but_never_rerun(self) -> None:
        result = classify_fixture("attempt-2.json")

        self.assertEqual(result["run_attempt"], 2)
        self.assertEqual(result["classification"], "infrastructure")
        self.assertFalse(result["rerun_failed_jobs"])


if __name__ == "__main__":
    unittest.main()
