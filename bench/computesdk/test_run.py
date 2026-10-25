"""run.sh refuses to share its output directory, and its checks pass on a fresh one.

The tools run.sh needs are replaced by stubs on PATH, and `git` exits 99, which is the
first thing run.sh does after its checks. Exit 99 therefore means the checks passed.
"""

import os
import shutil
import stat
import subprocess
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PASSED_CHECKS = 99


class RunShChecks(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="computesdk-run-test-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        bench = os.path.join(self.tmp, "repo", "bench", "computesdk")
        os.makedirs(bench)
        shutil.copy(os.path.join(HERE, "run.sh"), bench)
        self.run_sh = os.path.join(bench, "run.sh")
        fcvm = os.path.join(self.tmp, "repo", "target", "release", "fcvm")
        os.makedirs(os.path.dirname(fcvm))
        self._stub(fcvm, "exit 1")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        for tool in ("node", "curl", "jq", "python3"):
            self._stub(os.path.join(self.bin, tool), "exit 1")
        self._stub(os.path.join(self.bin, "git"), f"exit {PASSED_CHECKS}")

    @staticmethod
    def _stub(path, body):
        with open(path, "w") as f:
            f.write(f"#!/bin/sh\n{body}\n")
        os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)

    def run_script(self, work, out):
        env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"], WORK=work, OUT=out)
        return subprocess.run(
            ["bash", self.run_sh], env=env, capture_output=True, text=True, timeout=60
        )

    def test_fresh_work_and_out_pass_the_checks(self):
        r = self.run_script(os.path.join(self.tmp, "w"), os.path.join(self.tmp, "o"))
        self.assertEqual(r.returncode, PASSED_CHECKS, r.stderr)

    def test_a_second_run_with_another_work_and_the_same_out_is_refused(self):
        out = os.path.join(self.tmp, "o")
        os.makedirs(out)
        # The first run holds OUT's lock; it is running under a different WORK.
        holder = subprocess.Popen(
            ["flock", os.path.join(out, ".lock"), "sleep", "60"]
        )
        self.addCleanup(holder.wait)
        self.addCleanup(holder.kill)
        for _ in range(100):
            if os.path.exists(os.path.join(out, ".lock")):
                break
            time.sleep(0.05)
        r = self.run_script(os.path.join(self.tmp, "w2"), out)
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn(f"holds {out}/.lock", r.stderr)

    def test_an_out_with_results_is_refused(self):
        out = os.path.join(self.tmp, "o")
        os.makedirs(out)
        open(os.path.join(out, "burst-1.json"), "w").close()
        r = self.run_script(os.path.join(self.tmp, "w"), out)
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("already holds results", r.stderr)


if __name__ == "__main__":
    unittest.main()
