import importlib.util
import json
import os
import shlex
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
        ps_output_after: str | None = None,
        hang_ps: bool = False,
        dry_run: bool = True,
        outer_timeout: float = 5,
        proc_root: Path | None = None,
        pgrep_output: str = "",
        dmesg_output: str = "mock dmesg line\n",
    ) -> tuple[subprocess.CompletedProcess[str], Path, str, str, float]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir = root / "bin"
        log_dir = root / "logs"
        bin_dir.mkdir()
        log_dir.mkdir()
        ps_calls = root / "ps-calls"
        sudo_calls = root / "sudo-calls"
        # Hermetic procfs: evidence reads must never touch the real /proc of the
        # machine running the test suite, so every run gets a synthetic tree
        # (empty unless the test builds one). pgrep and dmesg are mocked for the
        # same reason — the guard samples real kcompactd/kswapd tasks and the
        # real kernel ring buffer in production, which is exactly the
        # nondeterminism a unit test must not depend on.
        if proc_root is None:
            proc_root = root / "proc"
            proc_root.mkdir()

        fake_pgrep = bin_dir / "pgrep"
        fake_pgrep.write_text(
            "#!/usr/bin/env bash\n"
            'if [ -n "${MOCK_PGREP_OUTPUT:-}" ]; then\n'
            '  printf \'%s\\n\' "$MOCK_PGREP_OUTPUT"\n'
            "fi\n"
        )
        fake_pgrep.chmod(0o755)

        fake_dmesg = bin_dir / "dmesg"
        fake_dmesg.write_text(
            "#!/usr/bin/env bash\n" 'printf \'%s\' "${MOCK_DMESG_OUTPUT:-}"\n'
        )
        fake_dmesg.chmod(0o755)

        fake_ps = bin_dir / "ps"
        fake_ps.write_text(
            "#!/usr/bin/env bash\n"
            "printf '%s\\n' \"$*\" >>\"$MOCK_PS_CALLS\"\n"
            "case \"$*\" in\n"
            "  *args*|*cmdline*|*command*) exit 93 ;;\n"
            "esac\n"
            "if [ \"${MOCK_PS_HANG:-0}\" = 1 ]; then exec sleep 30; fi\n"
            "call_count=$(wc -l <\"$MOCK_PS_CALLS\")\n"
            "if [ \"$call_count\" -gt 1 ]; then\n"
            "  printf '%s' \"$MOCK_PS_OUTPUT_AFTER\"\n"
            "else\n"
            "  printf '%s' \"${MOCK_PS_OUTPUT:-}\"\n"
            "fi\n"
        )
        fake_ps.chmod(0o755)

        fake_sudo = bin_dir / "sudo"
        fake_sudo.write_text(
            "#!/usr/bin/env bash\n"
            "printf '%s\\n' \"$*\" >>\"$MOCK_SUDO_CALLS\"\n"
        )
        fake_sudo.chmod(0o755)

        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}:{env['PATH']}",
                "FCVM_TEST_LOG_DIR": str(log_dir),
                "FCVM_GUARD_SCAN_TIMEOUT_SECONDS": "1",
                "FCVM_PROC_ROOT": str(proc_root),
                "FCVM_MM_SAMPLE_INTERVAL_SECONDS": "0",
                "MOCK_PS_CALLS": str(ps_calls),
                "MOCK_PS_HANG": "1" if hang_ps else "0",
                "MOCK_PS_OUTPUT": ps_output,
                "MOCK_PS_OUTPUT_AFTER": (
                    ps_output if ps_output_after is None else ps_output_after
                ),
                "MOCK_PGREP_OUTPUT": pgrep_output,
                "MOCK_DMESG_OUTPUT": dmesg_output,
                "MOCK_SUDO_CALLS": str(sudo_calls),
            }
        )
        command = [str(STRAY_GUARD), "post"]
        if dry_run:
            command.append("--dry-run")
        started = time.monotonic()
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            timeout=outer_timeout,
            check=False,
        )
        elapsed = time.monotonic() - started
        return (
            result,
            log_dir,
            ps_calls.read_text() if ps_calls.exists() else "",
            sudo_calls.read_text() if sudo_calls.exists() else "",
            elapsed,
        )

    @staticmethod
    def make_fake_task(
        proc_root: Path,
        tgid: int,
        tid: int,
        *,
        name: str,
        state: str,
        sigpnd: str = "0000000000000000",
        shdpnd: str = "0000000000000000",
        wchan: str = "0",
        stack_lines: list[str] | None = None,
    ) -> None:
        """Materialize /proc/<tgid>/task/<tid>/{status,wchan,stack} in a fake tree."""
        task = proc_root / str(tgid) / "task" / str(tid)
        task.mkdir(parents=True)
        (task / "status").write_text(
            f"Name:\t{name}\n"
            f"State:\t{state}\n"
            f"SigPnd:\t{sigpnd}\n"
            f"ShdPnd:\t{shdpnd}\n"
            "SigBlk:\t0000000000000000\n"
        )
        (task / "wchan").write_text(wchan)
        if stack_lines is not None:
            (task / "stack").write_text("\n".join(stack_lines) + "\n")

    def make_wedged_group_proc_root(self) -> Path:
        """A firecracker group matching the ARM CI failure class: leader in D,
        vCPU sibling parked under a migration entry with SIGKILL pending."""
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        proc_root = Path(temporary.name) / "proc"
        proc_root.mkdir()
        # SIGKILL is signal 9: bit 9 of the pending mask = 0x100.
        stack = [
            "[<0>] softleaf_entry_wait_on_locked+0x1c8/0x2d0",
            "[<0>] migration_entry_wait+0x64/0x90",
            "[<0>] do_swap_page+0x8a0/0xb60",
        ] + [f"[<0>] filler_frame_{i:03d}+0x10/0x20" for i in range(4, 101)]
        self.make_fake_task(
            proc_root,
            100,
            100,
            name="firecracker-def",
            state="D (disk sleep)",
            shdpnd="0000000000000100",
            wchan="softleaf_entry_wait_on_locked",
            stack_lines=stack,
        )
        self.make_fake_task(
            proc_root,
            100,
            101,
            name="fc_vcpu 0",
            state="D (disk sleep)",
            shdpnd="0000000000000100",
            wchan="softleaf_entry_wait_on_locked",
            stack_lines=stack,
        )
        self.make_fake_task(
            proc_root,
            200,
            200,
            name="fcvm",
            state="S (sleeping)",
            wchan="do_wait",
            stack_lines=["[<0>] do_wait+0x100/0x200"],
        )
        return proc_root

    def run_guard_with_scanner_descendant_holding_stderr(
        self,
    ) -> tuple[subprocess.CompletedProcess[str], Path, bool, float]:
        """Keep a scanner descendant alive until the test releases it.

        The scanner itself exceeds the guard deadline and is killed. Before the
        fix, its descendant inherits the guard's stderr-to-tee descriptor, so
        communicate() cannot observe EOF even after the guard exits.
        """
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir = root / "bin"
        log_dir = root / "logs"
        release = root / "release-scanner-descendant"
        descendant_ready = root / "scanner-descendant-ready"
        descendant_done = root / "scanner-descendant-done"
        bin_dir.mkdir()
        log_dir.mkdir()

        fake_ps = bin_dir / "ps"
        fake_ps.write_text(
            "#!/usr/bin/env bash\n"
            "(\n"
            "  : >\"$MOCK_PS_DESCENDANT_READY\"\n"
            "  while [ ! -e \"$MOCK_PS_RELEASE\" ]; do /bin/sleep 0.05; done\n"
            "  exec 2>&-\n"
            "  : >\"$MOCK_PS_DESCENDANT_DONE\"\n"
            ") &\n"
            "exec /bin/sleep 30\n"
        )
        fake_ps.chmod(0o755)

        # The guard calls sleep between scanner polls. Hold that first poll until
        # the descriptor-holding descendant exists, then advance past the scan
        # deadline. This makes the inherited-FD interleaving deterministic even
        # on a heavily loaded runner.
        fake_sleep = bin_dir / "sleep"
        fake_sleep.write_text(
            "#!/usr/bin/env bash\n"
            "while [ ! -e \"$MOCK_PS_DESCENDANT_READY\" ]; do /bin/sleep 0.01; done\n"
            "exec /bin/sleep 1.1\n"
        )
        fake_sleep.chmod(0o755)

        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{bin_dir}:{env['PATH']}",
                "FCVM_TEST_LOG_DIR": str(log_dir),
                "FCVM_GUARD_SCAN_TIMEOUT_SECONDS": "1",
                "MOCK_PS_RELEASE": str(release),
                "MOCK_PS_DESCENDANT_READY": str(descendant_ready),
                "MOCK_PS_DESCENDANT_DONE": str(descendant_done),
            }
        )
        started = time.monotonic()
        process = subprocess.Popen(
            [str(STRAY_GUARD), "post", "--dry-run"],
            cwd=ROOT,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        drained_before_release = True
        try:
            stdout, stderr = process.communicate(timeout=2.5)
        except subprocess.TimeoutExpired:
            drained_before_release = False
            release.touch()
            stdout, stderr = process.communicate(timeout=3)
        finally:
            release.touch(exist_ok=True)
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
        descendant_deadline = time.monotonic() + 3
        while not descendant_done.exists() and time.monotonic() < descendant_deadline:
            time.sleep(0.01)
        if not descendant_done.exists():
            raise AssertionError("scanner descendant did not exit after release")
        elapsed = time.monotonic() - started
        return (
            subprocess.CompletedProcess(
                args=process.args,
                returncode=process.returncode,
                stdout=stdout,
                stderr=stderr,
            ),
            log_dir,
            drained_before_release,
            elapsed,
        )

    def test_enumerates_every_thread_without_reading_command_lines(self) -> None:
        # ps columns: TGID TID PPID STATE COMM. The leader is already a zombie,
        # but its vCPU sibling is blocked in D state and still owns KVM resources.
        output = (
            "100 100 1 Z firecracker-def\n"
            "100 101 1 D fc_vcpu 0\n"
            "100 102 1 S fc_vcpu 1\n"
            "200 200 1 S unrelated\n"
        )

        result, log_dir, ps_calls, _sudo_calls, _elapsed = self.run_guard(output)

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
        result, log_dir, ps_calls, _sudo_calls, elapsed = self.run_guard(
            "", hang_ps=True
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(elapsed, 4)
        self.assertNotRegex(ps_calls, r"args|cmdline|command")
        self.assertIn("timed out", result.stdout.lower())
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())
        self.assertTrue((log_dir / "stray-vm-threads-post-before.tsv").is_file())

    def test_zero_target_scan_still_emits_diagnostics(self) -> None:
        result, log_dir, _ps_calls, _sudo_calls, _elapsed = self.run_guard("")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0 stray process group", result.stdout)
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())
        self.assertTrue((log_dir / "stray-vm-threads-post-before.tsv").is_file())

    def test_scan_timeout_does_not_leave_the_report_pipe_open(self) -> None:
        result, log_dir, drained_before_release, elapsed = (
            self.run_guard_with_scanner_descendant_holding_stderr()
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(
            drained_before_release,
            "a scanner descendant inherited stderr and kept the report pipe open",
        )
        self.assertLess(elapsed, 2.5)
        self.assertIn("timed out", result.stdout.lower())
        self.assertTrue((log_dir / "stray-vm-guard-post.log").is_file())

    def test_cleanup_kills_each_group_once_and_reports_survivors(self) -> None:
        before = (
            "100 100 1 S firecracker\n"
            "100 101 1 D fc_vcpu 0\n"
            "200 200 1 S fcvm\n"
            "200 201 1 S worker\n"
        )
        after = "100 100 1 D firecracker\n100 101 1 D fc_vcpu 0\n"

        result, log_dir, _ps_calls, sudo_calls, _elapsed = self.run_guard(
            before,
            ps_output_after=after,
            dry_run=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        commands = [shlex.split(line) for line in sudo_calls.splitlines()]
        self.assertCountEqual(
            commands,
            [["kill", "-9", "--", "100"], ["kill", "-9", "--", "200"]],
        )
        self.assertIn(
            "killed 2 process group(s), still live after SIGKILL: 1", result.stdout
        )
        self.assertIn("Unkillable microVMs", result.stdout)
        after_report = log_dir / "stray-vm-threads-post-after.tsv"
        self.assertTrue(after_report.is_file())
        self.assertIn("100\t101\t1\tD\tfc_vcpu 0", after_report.read_text())

    def test_evidence_is_captured_per_tid_before_the_kill_and_for_survivors(self) -> None:
        """The August 2026 wedge was diagnosed by hand from /proc stacks that were
        never archived; the runner was recycled and the evidence was lost. The
        guard must capture stack/wchan/status for every reported thread BEFORE it
        attempts the kill, and again for whatever survives SIGKILL."""
        before = (
            "100 100 1 D firecracker-def\n"
            "100 101 1 D fc_vcpu 0\n"
            "200 200 1 S fcvm\n"
        )
        after = "100 100 1 D firecracker-def\n100 101 1 D fc_vcpu 0\n"

        result, _log_dir, _ps_calls, _sudo_calls, _elapsed = self.run_guard(
            before,
            ps_output_after=after,
            dry_run=False,
            proc_root=self.make_wedged_group_proc_root(),
        )

        self.assertEqual(result.returncode, 0, result.stderr)

        # Capture precedes the kill attempt: a task that only becomes evidence
        # after SIGKILL has already lost the state the kill may destroy.
        pre = result.stdout.index("per-thread evidence (pre-kill")
        kill = result.stdout.index("killed 2 process group(s)")
        self.assertLess(pre, kill)

        pre_section = result.stdout[pre:kill]
        for tid_line in ["tgid=100 tid=100", "tgid=100 tid=101", "tgid=200 tid=200"]:
            self.assertIn(tid_line, pre_section)
        self.assertIn("State:\tD (disk sleep)", pre_section)
        self.assertIn("wchan: softleaf_entry_wait_on_locked", pre_section)
        self.assertIn("migration_entry_wait", pre_section)

        # Stacks are capped so one wedged group cannot flood the CI log.
        self.assertIn("filler_frame_060", pre_section)
        self.assertNotIn("filler_frame_061", pre_section)

        # Survivors get a second capture: State + SigPnd/ShdPnd is the proof of
        # "SIGKILL pending on a non-zombie task".
        post = result.stdout.index("per-thread evidence (post-SIGKILL survivors")
        post_section = result.stdout[post:]
        self.assertIn("tgid=100 tid=100", post_section)
        self.assertIn("tgid=100 tid=101", post_section)
        self.assertNotIn("tgid=200", post_section)
        self.assertIn("ShdPnd:\t0000000000000100", post_section)

    def test_survivors_trigger_memory_management_diagnostics(self) -> None:
        """A SIGKILL survivor means some kernel path owns the task. The lost ARM
        evidence pointed at compaction (kcompactd owning a folio lock), so the
        guard must sample reclaim/compaction stacks and counters while the
        survivor still exists."""
        proc_root = self.make_wedged_group_proc_root()
        for pid, comm in [(300, "kcompactd0"), (301, "kswapd0")]:
            main = proc_root / str(pid)
            main.mkdir()
            (main / "comm").write_text(f"{comm}\n")
            (main / "stack").write_text(
                "[<0>] compaction_alloc+0x3c/0x68\n[<0>] migrate_pages+0x9c/0x200\n"
            )
        (proc_root / "vmstat").write_text(
            "nr_free_pages 12345\ncompact_stall 42\ncompact_fail 7\n"
        )
        (proc_root / "pressure").mkdir()
        (proc_root / "pressure" / "memory").write_text(
            "some avg10=0.45 avg60=0.10 avg300=0.02 total=123456\n"
            "full avg10=1.23 avg60=0.40 avg300=0.08 total=98765\n"
        )
        dmesg = "".join(f"DMESG_LINE_{i:03d}\n" for i in range(1, 201))

        before = "100 100 1 D firecracker-def\n100 101 1 D fc_vcpu 0\n"

        result, _log_dir, _ps_calls, _sudo_calls, _elapsed = self.run_guard(
            before,
            ps_output_after=before,
            dry_run=False,
            proc_root=proc_root,
            pgrep_output="300\n301",
            dmesg_output=dmesg,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("memory-management diagnostics", result.stdout)
        for sample in ["sample 1/3", "sample 2/3", "sample 3/3"]:
            self.assertIn(sample, result.stdout)
        self.assertIn("[kcompactd0] pid=300", result.stdout)
        self.assertIn("[kswapd0] pid=301", result.stdout)
        self.assertIn("compaction_alloc", result.stdout)
        self.assertIn("compact_stall 42", result.stdout)
        self.assertIn("full avg10=1.23", result.stdout)
        # dmesg is the LAST 150 lines, unfiltered: line 51 survives the cut,
        # line 50 does not.
        self.assertIn("DMESG_LINE_051", result.stdout)
        self.assertNotIn("DMESG_LINE_050", result.stdout)

    def test_healthy_and_dry_runs_stay_quiet_about_mm_diagnostics(self) -> None:
        """No stray groups: no evidence sections at all. The guard's report must
        stay empty-when-healthy so a green run's log carries zero noise."""
        result, _log_dir, _ps_calls, _sudo_calls, _elapsed = self.run_guard("")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("per-thread evidence", result.stdout)
        self.assertNotIn("memory-management diagnostics", result.stdout)


if __name__ == "__main__":
    unittest.main()
