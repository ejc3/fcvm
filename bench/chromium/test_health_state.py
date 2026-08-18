#!/usr/bin/env python3
"""The HEALTHCHECK reader's contract: healthy means FRESH and healthy, both.

This reader replaced a per-second `python3 cdp_health.py` in every clone.
Measured in the bench image: 9.1ms of interpreter startup, 43.6ms for the whole
check even when it failed fast, once a second, in every clone, forever. The
check is now resident (`cdp_health.py --loop`) and this reads its verdict.

Everything here exists because the cheap reader introduces a way to be wrong
that the expensive one could not be: it can report a verdict its writer is no
longer producing. A reader that accepts a stale "healthy" says the checker was
healthy once, which differs from "is healthy now" exactly when the checker has
died. That is the green-by-absence shape this repo keeps finding in gates.
"""
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

READER = Path(__file__).resolve().parent / "health_state.sh"
SH_DIR = Path(__file__).resolve().parent


def monotonic_now() -> int:
    with open("/proc/uptime", "r", encoding="ascii") as handle:
        return int(float(handle.read().split()[0]))


def run_reader(state: str | None, max_age: str | None = None) -> subprocess.CompletedProcess:
    """Run the reader against a state file whose contents we control."""
    with tempfile.TemporaryDirectory() as directory:
        path = os.path.join(directory, "bench-health")
        if state is not None:
            Path(path).write_text(state, encoding="utf-8")
        env = dict(os.environ, BENCH_HEALTH_STATE=path)
        if max_age is not None:
            env["BENCH_HEALTH_MAX_AGE"] = max_age
        return subprocess.run(
            ["bash", str(READER)], env=env, capture_output=True, text=True, check=False
        )


class DefaultPathAgreement(unittest.TestCase):
    def test_the_default_paths_agree_without_the_env(self) -> None:
        """Writer and reader must resolve the same DEFAULT state-file path.

        Every other test here sets BENCH_HEALTH_STATE on both halves, so a
        writer default of /run/WRONG-PATH passes the whole file -- verified by
        mutation: 9/9 green with the default broken. Production sets the env
        nowhere (neither entry script, neither Containerfile, not reqbench.sh),
        so the DEFAULTS are the only contract in use, and nothing enforced
        them. A divergence reports every clone unhealthy -- or worse, healthy
        off a stale file at the old path.

        Both sides are asked, not read: the writer's answer is the real
        health_loop.state_file() with the env absent; the reader's is the real
        STATE_FILE assignment lifted from health_state.sh and EXECUTED under
        bash with the env absent -- the shipped bytes, not a paraphrase of
        them. A first version of this test proved the same thing end to end
        under `unshare -rm` with a private tmpfs /run; GitHub-hosted runners
        refuse unprivileged user namespaces (`unshare: write failed
        /proc/self/uid_map: Operation not permitted`), and AGENTS.md is
        explicit that a test must run where CI runs it. The write-then-read
        mechanics this drops are covered by the format round-trip test below.
        """
        import importlib

        env = {k: v for k, v in os.environ.items() if k != "BENCH_HEALTH_STATE"}

        old = os.environ.pop("BENCH_HEALTH_STATE", None)
        try:
            import health_loop
            importlib.reload(health_loop)
            writer_default = health_loop.state_file()
        finally:
            if old is not None:
                os.environ["BENCH_HEALTH_STATE"] = old

        reader_line = next(
            (line for line in READER.read_text().splitlines()
             if line.startswith("STATE_FILE=")),
            None,
        )
        self.assertIsNotNone(reader_line, "health_state.sh no longer assigns STATE_FILE")
        resolved = subprocess.run(
            ["bash", "-c", reader_line + '\nprintf "%s" "$STATE_FILE"'],
            capture_output=True, text=True, timeout=30, env=env,
        )
        self.assertEqual(resolved.returncode, 0, resolved.stderr)
        reader_default = resolved.stdout

        self.assertEqual(
            writer_default, reader_default,
            "writer and reader resolve DIFFERENT default paths; every verdict "
            "the loop publishes is invisible to the gate",
        )
        self.assertTrue(writer_default.startswith("/"),
                        f"the shared default {writer_default!r} is not absolute")


class ProbeContract(unittest.TestCase):
    def test_every_probe_branch_returns_a_reason_tuple(self) -> None:
        """main_with_reason() promises (int, str); every branch must keep it.

        health_loop.loop() unpacks the pair, so a branch that returns a bare
        int crashes the resident checker with `TypeError: cannot unpack
        non-iterable int object`. The loop's crash guard fails closed, so the
        symptom is not a wrong verdict but a checker that dies and a container
        that goes unhealthy with a traceback for a reason as mundane as "the
        warm marker is not there yet". wd_health's absent-marker branch shipped
        exactly that: every other branch returned the tuple and it returned 1.

        Driven for real: absent marker (the shipped bug's branch), then a
        marker with an unreadable session file (the next branch down).
        """
        import importlib
        with tempfile.TemporaryDirectory() as tmp:
            for env, why in (
                ({"BENCH_READY_FILE": os.path.join(tmp, "absent")},
                 "warm marker absent"),
                ({"BENCH_READY_FILE": __file__,
                  "BENCH_SESSION_FILE": os.path.join(tmp, "no-session")},
                 "session file unreadable"),
            ):
                with self.subTest(why=why):
                    old = {k: os.environ.get(k) for k in env}
                    os.environ.update(env)
                    try:
                        import wd_health
                        importlib.reload(wd_health)
                        result = wd_health.main_with_reason()
                    finally:
                        for k, v in old.items():
                            if v is None:
                                os.environ.pop(k, None)
                            else:
                                os.environ[k] = v
                    self.assertIsInstance(result, tuple,
                                          f"{why}: returned {result!r}, which "
                                          "health_loop.loop() cannot unpack")
                    self.assertEqual(len(result), 2)
                    self.assertIsInstance(result[0], int)
                    self.assertIsInstance(result[1], str)


class HealthStateReader(unittest.TestCase):
    def test_fresh_healthy_is_healthy(self) -> None:
        result = run_reader(f"healthy {monotonic_now()} exit=0\n")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_stale_healthy_is_not_healthy(self) -> None:
        """The one that matters. A verdict nobody is refreshing is not a verdict.

        The stamp is clamped at 0 and the budget tightened, rather than
        subtracting an hour: on a freshly booted box `now - 3600` is NEGATIVE,
        which the reader rejects as an unparsable stamp. That still fails
        closed, but it exercises the wrong branch, and this test exists for the
        staleness branch specifically.
        """
        result = run_reader(f"healthy {max(0, monotonic_now() - 60)} exit=0\n", max_age="1")
        self.assertEqual(
            result.returncode,
            1,
            "a stale healthy verdict was accepted; a dead checker would then report "
            "healthy forever\n" + result.stdout + result.stderr,
        )
        self.assertIn("old", result.stderr)

    def test_an_unparsable_budget_does_not_wave_a_stale_verdict_through(self) -> None:
        """The budget is the ONE input the reader took on trust.

        verdict, stamp and uptime are all validated before the arithmetic; the
        budget was not. The script runs under `set -uo pipefail` with no `-e`,
        so `[ "$age" -gt "$MAX_AGE" ]` with a non-numeric budget returns status
        2, the staleness branch is SKIPPED, and the reader falls through to the
        healthy exit. Measured before the fix, with a verdict 9999 seconds old:

            $ BENCH_HEALTH_MAX_AGE=notanumber health_state.sh
            health_state.sh: line 106: [: notanumber: integer expression expected
            healthy (age 9999s) ok
            exit=0

        A gate that cannot evaluate its own budget must refuse, not pass. This
        is the same fail-open shape as `jq: command not found` printing
        `verdict: CLEAR`: the check did not run, and its silence read as
        success.
        """
        stale = f"healthy {max(0, monotonic_now() - 9999)} exit=0\n"
        # The last two are all digits and still unusable: `[ -gt ]` compares as
        # 64-bit integers, so a value that does not fit returns status 2 exactly
        # like a non-numeric one. A digits-only check passes them and the gate
        # falls open, which is the same defect wearing a disguise.
        # 19 digits is the INT64 boundary case, and the only exploitable one:
        # `[ -gt ]` accepts up to 9223372036854775807 (19 digits), so a length
        # bound loosened to 19 admits budgets the comparison then chokes on.
        # The 20- and 48-digit probes below are past the boundary and even a
        # bound of 19 refuses them -- mutation testing showed the pair passing
        # with the bound at 19 while a 19-digit budget waved a stale verdict
        # through (`healthy (age 237s)`, exit 0).
        for budget in ("notanumber", "", "7s", "-1", "7.5",
                       "9999999999999999999",
                       "99999999999999999999",
                       "999999999999999999999999999999999999999999999999"):
            with self.subTest(budget=budget):
                result = run_reader(stale, max_age=budget)
                self.assertEqual(
                    result.returncode, 1,
                    f"a 9999s-old verdict passed the gate under budget {budget!r}; "
                    "an unevaluable budget must fail closed\n"
                    + result.stdout + result.stderr,
                )

    def test_missing_state_is_not_healthy(self) -> None:
        """Fail closed: no verdict is not a passing verdict."""
        result = run_reader(None)
        self.assertEqual(result.returncode, 1, result.stdout)

    def test_fresh_unhealthy_is_not_healthy(self) -> None:
        result = run_reader(f"unhealthy {monotonic_now()} exit=1\n")
        self.assertEqual(result.returncode, 1, result.stdout)

    def test_a_stamp_from_the_future_is_refused(self) -> None:
        """Monotonic time cannot advance across a reboot, so this file outlived one."""
        result = run_reader(f"healthy {monotonic_now() + 3600} exit=0\n")
        self.assertEqual(result.returncode, 1, result.stdout)

    def test_garbage_is_not_healthy(self) -> None:
        # (input, the diagnostic its OWN guard prints). Exit code alone is not
        # enough: with the validation deleted, `set -u` still aborts with 1 on
        # the arithmetic error -- the right exit for the wrong reason, which is
        # the accident the guard's comment says it exists to replace. The
        # message pins WHICH check refused.
        for junk, diagnostic in (
            ("\n", "unrecognised verdict"),
            ("healthy\n", "unparsable stamp"),
            ("healthy notanumber exit=0\n", "unparsable stamp"),
        ):
            with self.subTest(junk=junk):
                result = run_reader(junk)
                self.assertEqual(result.returncode, 1, f"accepted {junk!r}")
                self.assertIn(diagnostic, result.stderr,
                              f"refused {junk!r} but not by its own guard: "
                              f"stderr was {result.stderr!r}")

    def test_the_writer_and_reader_agree_on_the_format(self) -> None:
        """The contract that actually matters: publish() -> health_state.sh.

        Every other case here feeds the reader a hand-written string, so the
        writer could change its field order, its clock source, or its float
        format and all of them would still pass while every clone reported
        unhealthy. This runs the real writer and then the real reader against
        the file it wrote.
        """
        import importlib.util
        import shutil

        with tempfile.TemporaryDirectory() as directory:
            # Load from a COPY in a fresh directory. Loading the module in place
            # reads bench/chromium/__pycache__, and a stale .pyc there executed
            # a previous version of publish() while the source on disk was
            # correct: the test reported drift that did not exist, and would
            # equally have hidden drift that did. A directory with no cache
            # cannot lie about which source ran.
            # health_loop.py must come along: publish() lives there now, and
            # importing it from the real tree reads bench/chromium/__pycache__
            # -- the exact stale-bytecode hazard this copy exists to avoid.
            shutil.copy(
                Path(__file__).resolve().parent / "health_loop.py",
                os.path.join(directory, "health_loop.py"),
            )
            source = shutil.copy(
                Path(__file__).resolve().parent / "cdp_health.py",
                os.path.join(directory, "cdp_health_under_test.py"),
            )
            sys.path.insert(0, directory)
            self.addCleanup(sys.path.remove, directory)
            # `import health_loop` inside exec_module hits sys.modules first,
            # so a previously imported bench/chromium copy would satisfy it and
            # the temp copy above would never run -- the exact stale-bytecode
            # hazard this directory exists to avoid. Evict it so the import
            # resolves through sys.path to the copy; patch.dict restores the
            # whole mapping on cleanup.
            self.enterContext(mock.patch.dict(sys.modules))
            sys.modules.pop("health_loop", None)
            spec = importlib.util.spec_from_file_location("cdp_health_under_test", source)
            cdp_health = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(cdp_health)

            state = os.path.join(directory, "bench-health")
            # Set the ENV, which is what health_state.sh reads too, rather than
            # patching a module attribute. publish() resolves the path per call
            # via the same variable, so this exercises the one resolution both
            # sides actually use instead of a test-only back door.
            os.environ["BENCH_HEALTH_STATE"] = state
            self.addCleanup(os.environ.pop, "BENCH_HEALTH_STATE", None)
            cdp_health.publish("healthy", "pages=1 id=ABC")

            env = dict(os.environ, BENCH_HEALTH_STATE=state)
            result = subprocess.run(
                ["bash", str(READER)], env=env, capture_output=True, text=True, check=False
            )

        self.assertEqual(
            result.returncode,
            0,
            "the reader rejected what the writer just wrote; the two halves have "
            "drifted apart\n" + result.stdout + result.stderr,
        )
        self.assertIn("pages=1 id=ABC", result.stdout, "the detail did not survive the round trip")

    def test_the_budget_covers_a_slow_probe(self) -> None:
        """The writer loops at 1s but its CDP probe may take 3s, so ~4s gaps are
        normal. The default budget must not fail a healthy guest on one slow
        iteration, or the golden gate would flap."""
        result = run_reader(f"healthy {max(0, monotonic_now() - 4)} exit=0\n")
        self.assertEqual(
            result.returncode,
            0,
            "a 4s old verdict was refused; one slow CDP probe would then flap the "
            "gate\n" + result.stderr,
        )


if __name__ == "__main__":
    unittest.main()
