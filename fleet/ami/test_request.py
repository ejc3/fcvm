#!/usr/bin/env python3
"""Offline tests for priority, immutable inputs and the restricted request boundary."""
import datetime as dt
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest

spec = importlib.util.spec_from_file_location("image_request", Path(__file__).with_name("request.py"))
request = importlib.util.module_from_spec(spec)
spec.loader.exec_module(request)


class RequestTests(unittest.TestCase):
    def test_critical_labels_bypass_routine_batching(self):
        for name in request.CRITICAL_LABELS:
            self.assertEqual(request.priority_for_prs([{"labels": [{"name": name.upper()}]}]), "critical")
        self.assertEqual(request.priority_for_prs([{"labels": [{"name": "documentation"}]}]), "normal")

    def test_invalid_label_response_fails_instead_of_delaying_security_work(self):
        for value in (None, {}, [{"labels": "critical"}]):
            with self.subTest(value=value), self.assertRaises(ValueError):
                request.priority_for_prs(value)

    def test_source_identity_uses_git_objects_not_checkout_timestamps(self):
        calls = []
        def git(arguments, **kwargs):
            calls.append(arguments)
            return b"100644 blob " + b"b" * 40 + b"\tCargo.lock\0"
        self.assertEqual(request.tree_digest("a" * 40, git), request.tree_digest("c" * 40, git))
        self.assertEqual(calls[0][:4], ["git", "ls-tree", "-rz", "a" * 40])
        self.assertIn("rootfs-config.toml", calls[0])

    def test_untrusted_source_cannot_become_a_git_option(self):
        with self.assertRaises(ValueError):
            request.tree_digest("--upload-pack=bad", lambda *a, **k: self.fail("Git must not run"))

    def test_local_dependency_fixes_change_the_real_git_fingerprint(self):
        with tempfile.TemporaryDirectory(prefix="fcvm-image-inputs-") as directory:
            root = Path(directory)

            def git(*arguments):
                return subprocess.check_output(["git", "-C", directory,
                    "-c", "user.name=fixture", "-c", "user.email=fixture",
                    "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *arguments],
                    stderr=subprocess.DEVNULL)

            def snapshot():
                git("add", ".")
                git("commit", "-qm", "fixture")
                sha = git("rev-parse", "HEAD").decode().strip()
                return request.tree_digest(sha,
                    lambda command, **kwargs: subprocess.check_output(command, cwd=directory, **kwargs))

            git("init", "-q")
            (root / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.1.0'\n")
            for dependency in ("fuse-pipe", "exec-proto", "failpoint"):
                (root / dependency).mkdir()
                (root / dependency / "lib.rs").write_text("pub const VERSION: u8 = 1;\n")
            baseline = snapshot()
            (root / "README.md").write_text("Documentation only.\n")
            self.assertEqual(snapshot(), baseline)
            for dependency in ("fuse-pipe", "exec-proto", "failpoint"):
                with self.subTest(dependency=dependency):
                    (root / dependency / "lib.rs").write_text("pub const VERSION: u8 = 2;\n")
                    changed = snapshot()
                    self.assertNotEqual(changed, baseline,
                        "Compiled local dependency changes must request a new image")
                    baseline = changed

    def test_routine_identical_inputs_share_weekly_refresh(self):
        day = dt.datetime(2026, 10, 31, tzinfo=dt.timezone.utc)
        first = request.request_digest("a" * 64, "normal", "1", day)
        self.assertEqual(first, request.request_digest("a" * 64, "normal", "2", day))
        self.assertNotEqual(first, request.request_digest("a" * 64, "normal", "2", day + dt.timedelta(days=7)))

    def test_critical_upstream_fixes_can_rebuild_identical_source_immediately(self):
        day = dt.datetime(2026, 10, 31, tzinfo=dt.timezone.utc)
        self.assertNotEqual(request.request_digest("a" * 64, "critical", "1", day),
                            request.request_digest("a" * 64, "critical", "2", day))

    def invoke(self, status="queued", wrong_source=False, wrong_input=False, function_error=False):
        payload = {"action": "request", "source_sha": "a" * 40, "input_sha256": "b" * 64, "priority": "normal"}
        def aws(arguments, **kwargs):
            self.assertEqual(arguments[:3], ["aws", "lambda", "invoke"])
            self.assertNotIn("--debug", arguments)
            body = {"status": status, "request_id": "b" * 64, "input_sha256": ("c" if wrong_input else "b") * 64,
                    "source_sha": "c" * 40 if wrong_source else "a" * 40}
            Path(arguments[-1]).write_text(json.dumps(body))
            return SimpleNamespace(stdout=json.dumps({"StatusCode": 200, **({"FunctionError": "Unhandled"} if function_error else {})}))
        return request.invoke("arn:aws:lambda:us-west-1:123456789012:function:fleet-images:request", payload, aws)

    def test_queue_acknowledgement_remains_queued(self):
        self.assertEqual(self.invoke()["status"], "queued")

    def test_source_identical_cache_reports_the_actual_build_commit(self):
        for status in ("queued", "building", "promoted"):
            with self.subTest(status=status):
                self.assertEqual(self.invoke(status=status, wrong_source=True)["source_sha"], "c" * 40)

    def test_disabled_or_failed_pipeline_does_not_report_success(self):
        for status in ("disabled", "failed", "unknown"):
            with self.subTest(status=status), self.assertRaises(ValueError):
                self.invoke(status)

    def test_function_errors_and_foreign_inputs_are_rejected(self):
        for kwargs in ({"wrong_input": True}, {"function_error": True}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                self.invoke(**kwargs)

    def test_ordinary_ci_cannot_invoke_the_build_alias(self):
        with self.assertRaises(ValueError):
            request.invoke("arn:aws:lambda:us-west-1:123456789012:function:fleet-images:build", {},
                           lambda *a, **k: self.fail("AWS must not run"))


if __name__ == "__main__":
    unittest.main(buffer=True)
