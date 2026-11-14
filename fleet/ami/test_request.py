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
pipeline_spec = importlib.util.spec_from_file_location("image_pipeline", Path(__file__).with_name("pipeline.py"))
pipeline = importlib.util.module_from_spec(pipeline_spec)
pipeline_spec.loader.exec_module(pipeline)


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
        self.assertEqual(calls[0], ["git", "ls-tree", "-rz", "a" * 40, "--"])

    def test_untrusted_source_cannot_become_a_git_option(self):
        with self.assertRaises(ValueError):
            request.tree_digest("--upload-pack=bad", lambda *a, **k: self.fail("Git must not run"))

    def test_all_tracked_inputs_change_the_real_git_fingerprint_and_executor_agrees(self):
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
                digest = request.tree_digest(sha,
                    lambda command, **kwargs: subprocess.check_output(command, cwd=directory, **kwargs))
                self.assertEqual(pipeline.input_digest(root, sha)[0], digest)
                return digest

            git("init", "-q")
            (root / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.1.0'\n")
            inputs = (".cargo/config.toml", "rust-toolchain.toml", "build-wrapper", "README.md",
                      "fuse-pipe/lib.rs", "exec-proto/lib.rs", "failpoint/lib.rs")
            for name in inputs:
                path = root / name
                path.parent.mkdir(exist_ok=True, parents=True)
                path.write_text("fixture version 1\n")
            baseline = snapshot()
            for name in inputs:
                with self.subTest(path=name):
                    (root / name).write_text("fixture version 2\n")
                    changed = snapshot()
                    self.assertNotEqual(changed, baseline, "Every tracked file belongs to the image fingerprint")
                    baseline = changed
            before = git("rev-parse", "HEAD").decode().strip()
            git("commit", "--allow-empty", "-qm", "same tree")
            after = git("rev-parse", "HEAD").decode().strip()
            self.assertNotEqual(after, before)
            digest = request.tree_digest(after,
                lambda command, **kwargs: subprocess.check_output(command, cwd=directory, **kwargs))
            self.assertEqual(digest, baseline)
            self.assertEqual(pipeline.input_digest(root, after)[0], baseline)

    def test_routine_identical_inputs_share_weekly_refresh(self):
        day = dt.datetime(2026, 10, 31, tzinfo=dt.timezone.utc)
        first = request.request_digest("a" * 64, "normal", "1", day)
        self.assertEqual(first, request.request_digest("a" * 64, "normal", "2", day))
        self.assertNotEqual(first, request.request_digest("a" * 64, "normal", "2", day + dt.timedelta(days=7)))

    def test_critical_upstream_fixes_can_rebuild_identical_source_immediately(self):
        day = dt.datetime(2026, 10, 31, tzinfo=dt.timezone.utc)
        self.assertNotEqual(request.request_digest("a" * 64, "critical", "1", day),
                            request.request_digest("a" * 64, "critical", "2", day))

    def invoke(self, status="queued", wrong_source=False, wrong_input=False, function_error=False, message=None):
        payload = {"action": "request", "source_sha": "a" * 40, "input_sha256": "b" * 64, "priority": "normal"}
        def aws(arguments, **kwargs):
            self.assertEqual(arguments[:3], ["aws", "lambda", "invoke"])
            self.assertNotIn("--debug", arguments)
            body = {"status": status, "request_id": "b" * 64, "input_sha256": ("c" if wrong_input else "b") * 64,
                    "source_sha": "c" * 40 if wrong_source else "a" * 40}
            if message is not None:
                body = {"status": status, "message": message}
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

    def test_a_refusal_fails_with_the_factory_reason(self):
        # 2026-11-02: two runs failed with "Invalid image request status", which hid the reason
        with self.assertRaises(request.Refused) as caught:
            self.invoke("refused", message="only the current main revision can enter the factory")
        self.assertIn("only the current main revision can enter the factory", str(caught.exception))

    def github(self, head, runs, passed=(), descendants=()):
        def fetch(path, token):
            if path == "git/ref/heads/main":
                return {"object": {"sha": head}}
            if path == "actions/workflows/ci.yml/runs?branch=main&status=success&per_page=20":
                return {"workflow_runs": [{"head_sha": sha, "conclusion": "success"} for sha in passed]}
            if path.startswith("compare/"):
                base, newer = path[len("compare/"):].split("...")
                return ({"status": "ahead", "behind_by": 0} if newer in descendants
                        else {"status": "behind", "behind_by": 1})
            self.assertTrue(path.startswith("actions/workflows/ci.yml/runs?branch=main&head_sha="))
            return {"workflow_runs": runs}
        return fetch

    def test_a_revision_main_moved_past_is_requested_unless_a_newer_one_passed_ci(self):
        # ejc3/aws#823: every merge cancels the CI run before it, so a revision main moved past
        # waited for a newer CI completion that never came (8d46a0ee passed CI behind 9c22f734
        # on 2026-11-13, run 43431415595, and no request was sent).
        self.assertIsNone(request.not_requestable("a" * 40, "t", self.github("b" * 40, [])))
        # an older revision that passed CI is no reason to skip
        self.assertIsNone(request.not_requestable("a" * 40, "t", self.github("b" * 40, [], passed=["c" * 40])))
        reason = request.not_requestable("a" * 40, "t", self.github("b" * 40, [], passed=["c" * 40, "d" * 40],
                                                                   descendants=["d" * 40]))
        self.assertIn("d" * 40 + " passed CI", reason)

    def test_the_schedule_skips_a_head_whose_ci_still_runs(self):
        # the weekly schedule at 02:24 fired on 38d63412 while its CI ran (run 41377552783)
        reason = request.not_requestable("a" * 40, "t", self.github("a" * 40, [{"status": "in_progress", "conclusion": None}]))
        self.assertIn("still running", reason)

    def test_the_head_with_successful_ci_is_requested(self):
        for runs in ([{"status": "completed", "conclusion": "success"}], [],
                     [{"status": "completed", "conclusion": "failure"}]):
            with self.subTest(runs=runs):
                self.assertIsNone(request.not_requestable("a" * 40, "t", self.github("a" * 40, runs)))

    def test_ordinary_ci_cannot_invoke_the_build_alias(self):
        with self.assertRaises(ValueError):
            request.invoke("arn:aws:lambda:us-west-1:123456789012:function:fleet-images:build", {},
                           lambda *a, **k: self.fail("AWS must not run"))


    def test_the_request_workflow_refuses_a_source_without_the_boot_fix_and_launch_gate(self):
        # ejc3/aws#823: generation twelve booted without its user data. Until a source has ENA
        # built into the host kernel and the launch-path acceptance, no revision is requested.
        workflow = (Path(__file__).parents[2] / ".github/workflows/fleet-ami-pipeline.yml").read_text()
        name = "- name: Refuse a source without the boot fix and the launch-path acceptance"
        self.assertIn(name, workflow)
        self.assertLess(workflow.index(name), workflow.index("aws-actions/configure-aws-credentials"))
        self.assertLess(workflow.index(name), workflow.index("- name: Request the immutable main image build"))
        block = workflow[workflow.index(name):].split("\n      - ", 1)[0]
        script = block.split("run: |\n", 1)[1]
        script = "\n".join(line[10:] for line in script.splitlines())

        def gate(files):
            with tempfile.TemporaryDirectory() as root:
                run = lambda *a: subprocess.run(a, cwd=root, check=True, capture_output=True)
                run("git", "init", "-q")
                for path, text in files.items():
                    Path(root, path).parent.mkdir(parents=True, exist_ok=True)
                    Path(root, path).write_text(text)
                run("git", "add", "-A")
                run("git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "s")
                sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, check=True, capture_output=True, text=True).stdout.strip()
                return subprocess.run(["bash", "-c", script], cwd=root, env={"SOURCE_SHA": sha, "PATH": "/usr/bin:/bin"},
                                      capture_output=True, text=True)

        fixed = {"kernel/host-x86.conf": "CONFIG_ENA_ETHERNET=y\n",
                 "fleet/ami/pipeline_guest.py": "def launch_path_proof(image):\n    pass\n"}
        self.assertEqual(gate(fixed).returncode, 0)
        for missing, text in (("kernel/host-x86.conf", None), ("kernel/host-x86.conf", "CONFIG_ENA_ETHERNET=m\n"),
                              ("fleet/ami/pipeline_guest.py", None), ("fleet/ami/pipeline_guest.py", "def verify():\n    pass\n")):
            with self.subTest(missing=missing, text=text):
                files = dict(fixed)
                if text is None:
                    del files[missing]
                else:
                    files[missing] = text
                result = gate(files)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not requested", result.stdout)
        # this repository's own main is refused today
        here = gate({"README.md": "x\n"})
        self.assertNotEqual(here.returncode, 0)

if __name__ == "__main__":
    unittest.main(buffer=True)
