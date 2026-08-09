import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "classify_ci_failure.py"
FIXTURES = ROOT / "tests" / "fixtures" / "ci-infrastructure"
STRAY_GUARD = ROOT / "scripts" / "ci-stray-vm-guard.sh"

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


class StrayVmGuardTests(unittest.TestCase):
    def run_guard(
        self,
        ps_output: str,
        *,
        hang_ps: bool = False,
        outer_timeout: float = 5,
    ) -> tuple[subprocess.CompletedProcess[str], Path, str, float]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir = root / "bin"
        log_dir = root / "logs"
        bin_dir.mkdir()
        log_dir.mkdir()
        ps_calls = root / "ps-calls"

        fake_ps = bin_dir / "ps"
        fake_ps.write_text(
            "#!/usr/bin/env bash\n"
            "printf '%s\\n' \"$*\" >>\"$MOCK_PS_CALLS\"\n"
            "case \"$*\" in\n"
            "  *args*|*cmdline*|*command*) exit 93 ;;\n"
            "esac\n"
            "if [ \"${MOCK_PS_HANG:-0}\" = 1 ]; then exec sleep 30; fi\n"
            "printf '%s' \"${MOCK_PS_OUTPUT:-}\"\n"
        )
        fake_ps.chmod(0o755)

        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}:{env['PATH']}",
                "FCVM_TEST_LOG_DIR": str(log_dir),
                "FCVM_GUARD_SCAN_TIMEOUT_SECONDS": "1",
                "MOCK_PS_CALLS": str(ps_calls),
                "MOCK_PS_HANG": "1" if hang_ps else "0",
                "MOCK_PS_OUTPUT": ps_output,
            }
        )
        started = time.monotonic()
        result = subprocess.run(
            [str(STRAY_GUARD), "post", "--dry-run"],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            timeout=outer_timeout,
            check=False,
        )
        elapsed = time.monotonic() - started
        return result, log_dir, ps_calls.read_text() if ps_calls.exists() else "", elapsed

    def test_enumerates_every_thread_without_reading_command_lines(self) -> None:
        # ps columns: TGID TID PPID STATE COMM. The leader is already a zombie,
        # but its vCPU sibling is blocked in D state and still owns KVM resources.
        output = (
            "100 100 1 Z firecracker-def\n"
            "100 101 1 D fc_vcpu 0\n"
            "100 102 1 S fc_vcpu 1\n"
            "200 200 1 S unrelated\n"
        )

        result, log_dir, ps_calls, _elapsed = self.run_guard(output)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotRegex(ps_calls, r"args|cmdline|command")
        self.assertRegex(result.stdout, r"100\s+101")
        self.assertIn("fc_vcpu 0", result.stdout)
        self.assertRegex(result.stdout, r"100\s+102")
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())
        thread_report = log_dir / "stray-vm-threads-post-before.tsv"
        self.assertTrue(thread_report.is_file())
        self.assertIn("100\t101\t1\tD\tfc_vcpu 0", thread_report.read_text())

    def test_scan_timeout_is_bounded_and_still_emits_artifacts(self) -> None:
        result, log_dir, ps_calls, elapsed = self.run_guard("", hang_ps=True)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(elapsed, 4)
        self.assertNotRegex(ps_calls, r"args|cmdline|command")
        self.assertIn("timed out", result.stdout.lower())
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())
        self.assertTrue((log_dir / "stray-vm-threads-post-before.tsv").is_file())

    def test_zero_target_scan_still_emits_diagnostics(self) -> None:
        result, log_dir, _ps_calls, _elapsed = self.run_guard("")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0 stray process group", result.stdout)
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())
        self.assertTrue((log_dir / "stray-vm-threads-post-before.tsv").is_file())


if __name__ == "__main__":
    unittest.main()
