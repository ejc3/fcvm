#!/usr/bin/env python3
"""hostcdp.sh's host control carries the resolver-rule knob and records it.

The VM arm bakes BENCH_RESOLVE_ALL_TO into its golden through reqbench.sh's
GUEST_ENV; the host control has to run the same rule or the A/B has two
variables. hostcdp.sh forwards the knob into the container with `-e` and
writes it into run.json as `resolve_all_to` (null when unset), so a reader of
the record can tell which resolver rule the baseline ran under.

Driven with a stub podman on PATH that records the `run` argv NUL-separated,
and a python3 shim that answers for cdpdrive.py only, so the script runs to
its summary with no container and no browser. ALLOW_BUSY=1 passes the
quiet-box gate.

Watched red 2026-08-28 against hostcdp.sh at 13cb9543; the failure text is
quoted on each test.

Run: python3 -m unittest test_hostcdp -v
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SH = os.path.join(HERE, "hostcdp.sh")


def write_exec(path, body):
    with open(path, "w") as handle:
        handle.write(body)
    os.chmod(path, 0o755)


class HostCdpResolverRule(unittest.TestCase):
    def _run(self, resolve_all_to):
        d = tempfile.mkdtemp(prefix="hostcdp-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        run_argv = os.path.join(d, "podman-run-argv")
        write_exec(os.path.join(binx, "podman"), f'''#!/bin/bash
case "$1" in
  run) printf '%s\\0' "$@" > {run_argv}; echo 0123456789ab ;;
  inspect) echo sha256:{"c" * 64} ;;
esac
exit 0
''')
        write_exec(os.path.join(binx, "python3"), f'''#!/bin/bash
case "${{1:-}}" in
  *cdpdrive.py) echo '{{"stub": true}}'; exit 0 ;;
esac
exec {sys.executable} "$@"
''')
        env = dict(os.environ)
        env.pop("BENCH_RESOLVE_ALL_TO", None)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            RESULTS=os.path.join(d, "results"),
            ALLOW_BUSY="1",
            REPS="1",
            WARMUP="0",
            IMAGE="localhost/chromium-bench-req",
        )
        if resolve_all_to is not None:
            env["BENCH_RESOLVE_ALL_TO"] = resolve_all_to
        result = subprocess.run(["bash", SH], env=env, capture_output=True,
                                text=True, timeout=60)
        argv = None
        if os.path.exists(run_argv):
            with open(run_argv, "rb") as handle:
                argv = handle.read().decode().split("\0")[:-1]
        record = None
        run_json = os.path.join(env["RESULTS"], "run.json")
        if os.path.exists(run_json):
            with open(run_json) as handle:
                record = json.load(handle)
        return result, argv, record

    def test_the_knob_is_forwarded_with_dash_e_and_recorded(self):
        """Red: `AssertionError: ('-e', 'BENCH_RESOLVE_ALL_TO=10.0.2.2') not
        found in [('run', '-d'), ...]`; the container ran without the rule."""
        result, argv, record = self._run("10.0.2.2")
        self.assertEqual(result.returncode, 0, result.stderr[-2000:])
        self.assertIsNotNone(argv, "podman run was never invoked")
        pairs = list(zip(argv, argv[1:]))
        self.assertIn(("-e", "BENCH_RESOLVE_ALL_TO=10.0.2.2"), pairs, pairs)
        self.assertLess(argv.index("-e"), argv.index("localhost/chromium-bench-req"),
                        "the -e landed after the image, where podman reads it as "
                        "the container command")
        self.assertIsNotNone(record, "run.json was not written")
        self.assertEqual(record.get("resolve_all_to"), "10.0.2.2", record)

    def test_unset_forwards_nothing_and_records_null(self):
        """Red: `AssertionError: 'resolve_all_to' not found in {'image': ...}`;
        the record could not say which resolver rule the control ran under."""
        result, argv, record = self._run(None)
        self.assertEqual(result.returncode, 0, result.stderr[-2000:])
        self.assertIsNotNone(argv, "podman run was never invoked")
        self.assertEqual(
            [a for a in argv if "BENCH_RESOLVE_ALL_TO" in a], [],
            f"the knob was forwarded while unset: {argv}")
        self.assertIsNotNone(record, "run.json was not written")
        self.assertIn("resolve_all_to", record, record)
        self.assertIsNone(record["resolve_all_to"], record)


if __name__ == "__main__":
    unittest.main()
