import json
import subprocess
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "classify_ci_failure.py"
FIXTURES = ROOT / "tests" / "fixtures" / "ci-infrastructure"


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
