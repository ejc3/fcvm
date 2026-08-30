#!/usr/bin/env python3
"""The memory/CPU harness's gates and refusals, pinned.

corpus_mem.py runs for hours and publishes per-instance memory and per-render
CPU for fcvm against host containers. Every property here is a way for it to
finish and report a number that is not a measurement: a preflight that clears
the box because it could not look, a basis summed over a process set the sample
never saw, a subprocess that outlives the deadline meant to bound it, and a
recorded arm thrown away by the failure of the arm after it.

None of them raises on its own. Each one produces a run that looks finished.

Run: python3 -m unittest test_corpus_mem -v
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import corpus_mem  # noqa: E402

EXTRA = os.path.join(HERE, "corpus_extra.sh")


class Completed:
    """Stands in for subprocess.CompletedProcess without running anything."""

    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr


class StrayPreflight(unittest.TestCase):
    """A preflight that could not run has not cleared the box.

    The refusal exists because a leftover fcvm or firecracker is charged to
    whatever this run then measures. `pgrep ... || true` collapsed every way
    pgrep can fail into the same empty string the clean case produces: exit 127
    (pgrep absent), 2 (bad pattern), 3 (fatal). Only exit 1 means "no match".
    """

    def patch(self, fn):
        real = corpus_mem.sh
        corpus_mem.sh = fn
        self.addCleanup(setattr, corpus_mem, "sh", real)

    def test_a_clean_box_reports_no_strays(self):
        self.patch(lambda *_a, **_k: Completed(1, "", ""))
        self.assertEqual(corpus_mem.stray_vmm_processes(), "")

    def test_a_dirty_box_reports_the_processes_it_found(self):
        self.patch(lambda *_a, **_k: Completed(0, "4242 firecracker --api-sock x\n", ""))
        self.assertIn("firecracker", corpus_mem.stray_vmm_processes())

    def test_a_preflight_that_could_not_run_blocks_the_run(self):
        """exit 127 is "pgrep is not installed", not "the box is clean"."""
        self.patch(lambda *_a, **_k: Completed(127, "", "pgrep: command not found"))
        with self.assertRaises(SystemExit):
            corpus_mem.stray_vmm_processes()

    def test_a_preflight_that_errored_blocks_the_run(self):
        self.patch(lambda *_a, **_k: Completed(2, "", "pgrep: syntax error"))
        with self.assertRaises(SystemExit):
            corpus_mem.stray_vmm_processes()

    def test_a_preflight_that_cannot_be_spawned_blocks_the_run(self):
        def boom(*_a, **_k):
            raise FileNotFoundError("pgrep")
        self.patch(boom)
        with self.assertRaises(SystemExit):
            corpus_mem.stray_vmm_processes()


class ZeroBasis(unittest.TestCase):
    """A basis of zero is a sample that could not see the process set.

    Every basis is a sum over the processes of live instances that have each
    rendered a page, so with the instance count satisfied none of them can be
    zero. report.py's single-node cgroup.procs read produced exactly this on
    the container side: pool_containers = N from `podman ps`, pool_procs = 0 and
    pool_pss_kb = 0 from an empty process set. cell_values reads those with
    `.get(key, 0)`, so a zero becomes a real number in summary.json and in the
    least-squares fit, and the run's own instance-count check passes.
    """

    def test_a_complete_container_sample_is_accepted(self):
        s = {"pool_containers": 4, "pool_procs": 52, "pool_cgroup_kb": 900_000,
             "pool_pss_kb": 700_000}
        self.assertEqual(corpus_mem.empty_bases(s, "host-container"), [])

    def test_a_container_sample_with_no_pss_is_refused(self):
        s = {"pool_containers": 4, "pool_procs": 0, "pool_cgroup_kb": 900_000,
             "pool_pss_kb": 0}
        self.assertEqual(sorted(corpus_mem.empty_bases(s, "host-container")),
                         ["pool_procs", "pool_pss_kb"])

    def test_a_clone_sample_with_no_pss_is_refused(self):
        s = {"clones": 4, "clone_procs": 0, "clone_cgroup_kb": 900_000,
             "clone_pss_kb": 0}
        self.assertEqual(sorted(corpus_mem.empty_bases(s, "fcvm-clone")),
                         ["clone_procs", "clone_pss_kb"])

    def test_a_missing_basis_is_refused_like_a_zero_one(self):
        """`.get(key, 0)` downstream cannot tell the two apart, so neither does this."""
        self.assertEqual(sorted(corpus_mem.empty_bases({"pool_containers": 1},
                                                       "host-container")),
                         ["pool_cgroup_kb", "pool_procs", "pool_pss_kb"])

    def test_a_complete_clone_sample_is_accepted(self):
        s = {"clones": 2, "clone_procs": 8, "clone_cgroup_kb": 500_000,
             "clone_pss_kb": 400_000}
        self.assertEqual(corpus_mem.empty_bases(s, "fcvm-clone"), [])


class BoundedAttempt(unittest.TestCase):
    """A deadline checked after a hung subprocess is never checked again.

    The container readiness loops bound themselves with a 180 s deadline and
    then call `podman exec` with no timeout. One wedged container holds the
    harness there forever, and the deadline that was supposed to produce the
    diagnostic never gets evaluated. AGENTS.md: bound every attempt, not just
    the total.
    """

    def test_an_attempt_that_times_out_becomes_a_failed_attempt(self):
        def hang(cmd, **_k):
            raise subprocess.TimeoutExpired(cmd, 30)
        real = corpus_mem.sh
        corpus_mem.sh = hang
        self.addCleanup(setattr, corpus_mem, "sh", real)
        r = corpus_mem.sh_bounded(["podman", "exec", "x", "true"], 30)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("30", r.stderr)

    def test_an_attempt_that_answers_is_passed_through(self):
        real = corpus_mem.sh
        corpus_mem.sh = lambda *_a, **_k: Completed(0, "yes", "")
        self.addCleanup(setattr, corpus_mem, "sh", real)
        self.assertEqual(corpus_mem.sh_bounded(["true"], 30).stdout, "yes")

    @staticmethod
    def podman_exec_calls(src):
        """Every `podman exec` call site, with the function it went through."""
        out = []
        for m in re.finditer(r'(sh(?:_bounded)?)\(\[\s*"podman",\s*"exec"', src):
            depth = 0
            i = src.index("(", m.start())
            j = i
            while j < len(src):
                if src[j] == "(":
                    depth += 1
                elif src[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            out.append((m.group(1), src[i:j + 1]))
        return out

    def test_every_podman_exec_carries_a_bound(self):
        """A source-level pin: one unbounded `podman exec` reintroduces the hang.

        Either through sh_bounded, or through sh with an explicit timeout. A
        bare sh(["podman", "exec", ...]) is the shape that hangs.
        """
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            src = handle.read()
        calls = self.podman_exec_calls(src)
        self.assertTrue(calls, "the podman exec call sites are gone")
        for fn, call in calls:
            self.assertTrue(fn == "sh_bounded" or "timeout=" in call,
                            f"unbounded podman exec: {call!r}")


class CputimeRecordSurvivesTheHostArm(unittest.TestCase):
    """The fcvm arm costs 42 clone lifecycles. The host arm must not take it.

    The comment above the host arm already says a failure there is "recorded,
    not fatal", and cites losing a whole run to it on 2026-08-30 17:53. Only
    TimeoutError was caught, so every other refusal on the host side -- an
    unattributable cgroup path, a missing cpu.stat, a failed render -- called
    die() or raised past the one write of cputime.json and discarded the fcvm
    records with it.
    """

    def run_with_failing_host(self, boom):
        real = corpus_mem.cputime_host_arm
        corpus_mem.cputime_host_arm = boom
        self.addCleanup(setattr, corpus_mem, "cputime_host_arm", real)

        class Args:
            cputime_reps = 3
            urls = ["https://example.com/"]
        tmp = tempfile.mkdtemp()
        out = os.path.join(tmp, "cputime.json")
        try:
            corpus_mem.run_cputime(Args(), None, None, out)
        except SystemExit:
            pass
        return out

    def test_a_host_arm_that_refuses_still_leaves_the_record(self):
        def refuse(_args, res):
            res["host_error"] = "podman reports no container cgroup"
            corpus_mem.die("a CPU figure read from the root cgroup would be the whole machine")
        out = self.run_with_failing_host(refuse)
        self.assertTrue(os.path.exists(out), "cputime.json was not written")
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["host"], None)
        self.assertIn("host_error", rec)

    def test_the_recorded_reason_is_the_reason(self):
        """`die` exits with a CODE, so str(SystemExit) is "2", not the message.

        A record that says the host arm failed and cannot say why is the same
        shape as a diagnostic that reports nothing and a clean result: the
        reader cannot tell them apart. The host arm's own refusals carry their
        text.
        """
        def refuse(_args, _res):
            raise corpus_mem.HostArmRefused(
                "podman reports no container cgroup for cbmem-cpu-abc")
        out = self.run_with_failing_host(refuse)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertIn("no container cgroup", rec["host_error"],
                      f"the record does not name the refusal: {rec['host_error']!r}")

    def test_a_host_arm_that_raises_still_leaves_the_record(self):
        def crash(_args, _res):
            raise RuntimeError("podman went away")
        out = self.run_with_failing_host(crash)
        self.assertTrue(os.path.exists(out), "cputime.json was not written")
        with open(out) as handle:
            self.assertIsNone(json.load(handle)["host"])


class Resummarize(unittest.TestCase):
    """A recomputed host summary must not assert a failure count it never counted.

    resummarize.py exists to restate a hostcdp run's p50 under the median
    convention reqanalyze publishes, so the ratio compare.py takes against the
    VM arm is between two numbers computed the same way. It overwrites the
    summary.json hostcdp.sh wrote.

    hostcdp.sh can write "failures": 0 because it exits 4 on the first failed
    rep, so a summary it reaches is a run with none. resummarize.py has no such
    invariant: it is pointed at a directory. It filtered on `warmup` only, so a
    failed rep's wall_ms -- a timeout, the largest number in the file -- went
    into the distribution, and the field beside it still said no failures.
    """

    @staticmethod
    def run_on(rows):
        tmp = tempfile.mkdtemp()
        with open(os.path.join(tmp, "hostcdp.jsonl"), "w") as handle:
            for r in rows:
                handle.write(json.dumps(r) + "\n")
        proc = subprocess.run([sys.executable, os.path.join(HERE, "resummarize.py"), tmp],
                              capture_output=True, text=True, timeout=60)
        return tmp, proc

    @staticmethod
    def rep(rep, ok=True, warmup=False, wall_ms=100.0, load=0.5):
        return {"rep": rep, "ok": ok, "warmup": warmup, "wall_ms": wall_ms,
                "loadavg1": load, "url": "https://example.com/", "driver": "{}"}

    def test_a_clean_run_is_summarised(self):
        rows = [self.rep(0, warmup=True, wall_ms=900.0)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 6)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["n"], 5)
        self.assertEqual(rec["failures"], 0)
        self.assertEqual(rec["p50_ms"], 103.0)

    def test_a_run_holding_a_failed_rep_is_refused(self):
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 5)] + \
               [self.rep(5, ok=False, wall_ms=30000.0)]
        tmp, proc = self.run_on(rows)
        self.assertNotEqual(proc.returncode, 0,
                            "a run with a failed rep was summarised; its timeout "
                            f"is now in the p95\n{proc.stdout}{proc.stderr}")
        self.assertIn("1", proc.stdout + proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "a refused run still left a quotable summary.json")

    def test_the_contention_record_survives_the_overwrite(self):
        """hostcdp.sh writes loadavg1_measured; overwriting must not drop it.

        It is the field that answers "was the box busy while this was measured",
        and a summary.json that lost it cannot be checked against that question
        any more.
        """
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i), load=0.2 * i) for i in range(1, 6)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertIsNotNone(rec.get("loadavg1_measured"),
                             "the recomputed summary dropped the contention record")
        self.assertEqual(rec["loadavg1_measured"]["n"], 5)

    def test_a_run_with_no_measured_reps_is_refused(self):
        tmp, proc = self.run_on([self.rep(0, warmup=True)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))


class MedianConvention(unittest.TestCase):
    """A field named p50 has to hold statistics.median.

    hostcdp.sh and resummarize.py both write "p50_convention":
    "statistics.median" into their records, because a ratio between a host p50
    and a VM p50 is only meaningful when both are computed the same way, and a
    reader of the record alone cannot otherwise tell. The fcvm CPU arm computed
    sorted(v)[len(v) // 2] instead, which on an even-length list is the upper of
    the two middle values.

    Measured on this harness's own 42-record cputime run
    (results/corpusextra-memory-20260830-181830/memory/cputime.json): the field
    says 1643.5, the median of those records is 1486.0. 10.6% high, published
    under a key named p50 and set beside a host mean.
    """

    def test_an_even_length_list_gets_the_median_not_the_upper_middle(self):
        self.assertEqual(corpus_mem.median_ms([1.0, 2.0, 3.0, 4.0]), 2.5)

    def test_the_recorded_cputime_run_medians_to_1486(self):
        """The exact case that was published wrong."""
        rec = os.path.join(
            os.path.dirname(os.path.dirname(HERE)), "bench", "chromium", "results",
            "corpusextra-memory-20260830-181830", "memory", "cputime.json")
        if not os.path.exists(rec):
            self.skipTest("the recorded cputime run is not in this tree")
        with open(rec) as handle:
            vals = [r["cpu_ms"] for r in json.load(handle)["fcvm"]["records"]]
        self.assertEqual(len(vals), 42)
        self.assertEqual(corpus_mem.median_ms(vals), 1486.0)

    def test_an_odd_length_list_is_unchanged(self):
        self.assertEqual(corpus_mem.median_ms([5.0, 1.0, 3.0]), 3.0)


class FrozenCopiesAreRecordsNotTests(unittest.TestCase):
    """The sealed harness copies under results/ are provenance, not code CI runs.

    A run freezes the scripts that produced it beside its records, and some of
    those are test files. CI's Bench Harness Tests job is
    `unittest discover -s bench/chromium -p 'test_*.py'`, so a frozen copy that
    got loaded would run a stale test against the current tree: green or red for
    reasons that have nothing to do with the commit under test. It does not
    happen today because results/ holds no package, and this says so out loud
    rather than leaving it to a property nobody wrote down.
    """

    def test_discovery_loads_nothing_from_results(self):
        import unittest as ut
        suite = ut.defaultTestLoader.discover(HERE, pattern="test_*.py")

        def modules(t):
            for item in t:
                if isinstance(item, ut.TestSuite):
                    yield from modules(item)
                else:
                    yield type(item).__module__

        for mod in sorted(set(modules(suite))):
            path = getattr(sys.modules.get(mod), "__file__", "") or ""
            self.assertNotIn(os.sep + "results" + os.sep, path,
                             f"CI would run the frozen copy {path}")


class ProvenanceNamesBytes(unittest.TestCase):
    """A provenance record that cites empty strings cites nothing.

    corpus_extra.sh builds provenance.json by interpolating command
    substitutions into echo lines. A failing sha256sum or podman inspect leaves
    the field empty, echo still exits 0 under `set -e`, and the only gate is a
    json.load -- which an object full of "" passes. The numbers then name no
    binary, no image and no script.
    """

    @staticmethod
    def block():
        src = open(EXTRA).read()
        m = re.search(r"(\{\n *echo \"\{\".*?\n\} > \"\$RESULTS/provenance\.json\"\n.*?\n)(?=\n)",
                      src, re.S)
        assert m, "the provenance block is gone"
        return m.group(1)

    def drive(self, bench_has_files):
        tmp = tempfile.mkdtemp()
        bench = os.path.join(tmp, "bench")
        results = os.path.join(tmp, "results")
        os.makedirs(bench)
        os.makedirs(os.path.join(tmp, "repo", "target", "release"))
        os.makedirs(results)
        for name in ("hostcdp.sh", "cdpdrive.py", "render.py", "corpus_mem.py",
                     "corpus_serve.py", "report.py"):
            if bench_has_files:
                open(os.path.join(bench, name), "w").write(name)
        open(os.path.join(tmp, "repo", "target", "release", "fcvm"), "w").write("x")
        stub = os.path.join(tmp, "bin")
        os.makedirs(stub)
        with open(os.path.join(stub, "podman"), "w") as f:
            f.write('#!/bin/sh\necho sha256:deadbeef\n')
        with open(os.path.join(stub, "git"), "w") as f:
            f.write('#!/bin/sh\ncase "$*" in *rev-parse*) echo abc123 ;; *) : ;; esac\n')
        for name in ("podman", "git"):
            os.chmod(os.path.join(stub, name), 0o755)
        script = os.path.join(tmp, "run.sh")
        with open(script, "w") as f:
            f.write("set -euo pipefail\n"
                    f'export PATH="{stub}:$PATH"\n'
                    f'REPO="{tmp}/repo"\nBENCH="{bench}"\nRESULTS="{results}"\n'
                    'IMAGE=localhost/x\nTAG=t\nREPS=1\nWARMUP=0\n'
                    + self.block() + "\necho ACCEPTED\n")
        return subprocess.run(["bash", script], capture_output=True, text=True, timeout=60)

    def test_a_complete_provenance_record_is_accepted(self):
        r = self.drive(bench_has_files=True)
        self.assertIn("ACCEPTED", r.stdout,
                      f"the positive control was refused\n{r.stdout}{r.stderr}")

    def test_a_provenance_record_naming_no_bytes_is_refused(self):
        r = self.drive(bench_has_files=False)
        self.assertNotIn("ACCEPTED", r.stdout,
                         "provenance.json passed its gate while naming no script bytes; "
                         f"the numbers would cite nothing\n{r.stdout}{r.stderr}")
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
