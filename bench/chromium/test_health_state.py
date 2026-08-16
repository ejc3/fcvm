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
import tempfile
import unittest
from pathlib import Path

READER = Path(__file__).resolve().parent / "health_state.sh"


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
        for budget in ("notanumber", "", "7s", "-1", "7.5"):
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
        for junk in ("\n", "healthy\n", "healthy notanumber exit=0\n"):
            with self.subTest(junk=junk):
                self.assertEqual(run_reader(junk).returncode, 1, f"accepted {junk!r}")

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
            source = shutil.copy(
                Path(__file__).resolve().parent / "cdp_health.py",
                os.path.join(directory, "cdp_health_under_test.py"),
            )
            spec = importlib.util.spec_from_file_location("cdp_health_under_test", source)
            cdp_health = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(cdp_health)

            state = os.path.join(directory, "bench-health")
            cdp_health.STATE_FILE = state
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
