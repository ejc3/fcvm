#!/usr/bin/env python3
"""hostcdp.sh runs the corpus schedule, not one URL repeated.

The VM arm cycles its URL list with reqbench.url_for_rep -- urls[rep % len(urls)],
rep counted from 0 across warmup and measured reps alike, over
range(warmup + reps), so its --reps is the MEASURED count and --warmup is extra.
hostcdp.sh reads REPS and WARMUP the same way. A host control that
rendered ONE page while the VM arm rendered fourteen would not be a control for
it, so these pin the cycle, the per-record url, the run.json fields, and the
two-full-cycles warmup floor. Every row also names the exact run.json bytes, so
metadata from one run cannot be placed beside samples from another.
The results directory is claimed exactly once; reuse is refused before an old
summary or record can be overwritten.

Driven with a stub podman and a python3 shim (write_python_shim) that hands
hostcdp.sh's in-process driver a stub cdpdrive module, which appends the URL it
was handed, so the schedule is checked with no container and no browser.
ALLOW_BUSY=1 passes the quiet-box gate.

The same script gained a CPUS budget knob, covered by HostCdpCpuBudget below,
because a host baseline on every core compared against a 2-vCPU VM arm is two
variables.

Watched red against the unpatched hostcdp.sh: the cycle test failed with every
rep recording https://a.example/ (the whole spec passed through as one URL),
the run.json test with KeyError 'urls', the warmup floor test with exit 0 where
2 was required, and the CPUS tests with '--cpus' absent from the podman argv
and KeyError 'cpus' out of run.json.

Run: python3 -m unittest test_hostcdp_corpus -v
"""

import hashlib
import json
import os
import statistics
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SH = os.environ.get("HOSTCDP_SH", os.path.join(HERE, "hostcdp.sh"))
URLS = ["https://a.example/", "https://b.example/", "https://c.example/"]
CONTAINER_ID = "c" * 64
CONTAINER_OWNER_TOKEN = "1" * 32


def write_exec(path, body):
    with open(path, "w") as handle:
        handle.write(body)
    os.chmod(path, 0o755)


def staged_runtime(directory):
    runtime = os.path.join(directory, "runtime")
    os.makedirs(runtime)
    payload = os.path.join(runtime, "payload")
    with open(payload, "w") as handle:
        handle.write("sealed\n")
    payload_digest = hashlib.sha256(b"sealed\n").hexdigest()
    manifest = os.path.join(runtime, "REQBENCH_MANIFEST.sha256")
    with open(manifest, "w") as handle:
        handle.write(f"{payload_digest}  payload\n")
    with open(manifest, "rb") as handle:
        identity = hashlib.sha256(handle.read()).hexdigest()
    return manifest, identity


def write_python_shim(binx, directory, seen=None):
    """A python3 on PATH that logs every start to <directory>/python3-calls.

    hostcdp.sh drives every rep from one `python3 - <cdpdrive.py> ...`
    process; the shim swaps that cdpdrive.py for a stub module whose drive()
    succeeds and, given `seen`, appends the URL it was handed. The old form, a
    rep running `python3 cdpdrive.py` as its own program, is still answered so
    a regression shows up in python3-calls and not as a connection failure.
    Returns the path of the call log.
    """
    calls = os.path.join(directory, "python3-calls")
    stub = os.path.join(directory, "stub_cdpdrive.py")
    with open(stub, "w") as handle:
        handle.write(f'''SEEN = {seen!r}
import os as _os


def _bump_cpu():
    """Advance the fixture cgroup's usage_usec by 2.5 ms, as a render would."""
    path = _os.environ.get("TEST_CPU_STAT")
    if not path or not _os.path.exists(path):
        return
    with open(path) as handle:
        usage = int(handle.read().split()[1])
    with open(path, "w") as handle:
        handle.write(f"usage_usec {{usage + 2500}}\\nuser_usec 0\\nsystem_usec 0\\n")


def drive(args):
    _bump_cpu()
    import os
    action = os.environ.get("TEST_DRIVE_ACTION", "")
    if action == "interrupt":
        raise KeyboardInterrupt
    if action == "nondict":
        return "not a result"
    if SEEN:
        with open(SEEN, "a") as handle:
            handle.write(args.url + "\\n")
    return {{"ok": True, "stub": True, "url": args.url}}
''')
    # The old form's argv is `cdpdrive.py ADDRESS URL ...`, so the URL is $3.
    record_url = f'printf "%s\\n" "$3" >> {seen}; ' if seen else ""
    write_exec(os.path.join(binx, "python3"), f'''#!/bin/bash
printf '%s\\n' "$*" >> {calls}
case "${{1:-}}" in
  *cdpdrive.py) {record_url}echo '{{"ok":true,"stub":true}}'; exit 0 ;;
esac
if [ "${{1:-}}" = - ] && [[ "${{2:-}}" == *cdpdrive.py ]]; then
  set -- - {stub} "${{@:3}}"
fi
exec {sys.executable} "$@"
''')
    return calls


def cgroup_fixture(directory):
    """A CGROUP_ROOT whose /fake-container.scope/cpu.stat the stub driver
    advances by 2.5 ms per drive(). Returns the env entries hostcdp.sh needs.
    The root has its own readable cpu.stat, as the real root cgroup does, so a
    path that resolves to the root reads a valid counter unless refused."""
    scope = os.path.join(directory, "cgroup", "fake-container.scope")
    os.makedirs(scope)
    with open(os.path.join(directory, "cgroup", "cpu.stat"), "w") as handle:
        handle.write("usage_usec 9000000000\nuser_usec 0\nsystem_usec 0\n")
    cpu_stat = os.path.join(scope, "cpu.stat")
    with open(cpu_stat, "w") as handle:
        handle.write("usage_usec 1000000\nuser_usec 0\nsystem_usec 0\n")
    # Podman puts the container's processes one level down.
    os.makedirs(os.path.join(scope, "container"))
    with open(os.path.join(scope, "container", "cgroup.procs"), "w") as handle:
        handle.write("4242\n")
    return {"CGROUP_ROOT": os.path.join(directory, "cgroup"), "TEST_CPU_STAT": cpu_stat}


def write_podman_stub(path, run_argv=None):
    present = path + ".container-present"
    record_argv = ""
    if run_argv is not None:
        record_argv = f'''printf '%s\\0' "$@" > "{run_argv}"\n'''
    write_exec(path, f'''#!/bin/bash
case "$1" in
  image)
    echo sha256:{"a" * 64}
    ;;
  create)
    {record_argv}    : > "{present}"
    echo {CONTAINER_ID}
    ;;
  start)
    ;;
  inspect)
    case "$*" in
      *'.Image'*) echo sha256:{"a" * 64} ;;
      *'Config.Labels'*) echo {CONTAINER_ID}'|'{CONTAINER_OWNER_TOKEN} ;;
      *'.State.CgroupPath'*) echo "${{TEST_CGROUP_PATH-/fake-container.scope}}" ;;
      *'.State.Pid'*) echo 4242 ;;
    esac
    ;;
  container)
    [ "${{2:-}}" = exists ] && [ -e "{present}" ] && exit 0
    exit 1
    ;;
  rm)
    rm -f -- "{present}"
    ;;
esac
exit 0
''')


class HostCdpCorpusSchedule(unittest.TestCase):
    def _run(self, url_spec, reps, warmup, existing_results=False):
        d = tempfile.mkdtemp(prefix="hostcdp-corpus-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        seen = os.path.join(d, "cdpdrive-urls")
        write_podman_stub(os.path.join(binx, "podman"))
        write_python_shim(binx, d, seen)
        loadavg = os.path.join(d, "loadavg")
        with open(loadavg, "w") as handle:
            handle.write("1.23 0.50 0.40 1/100 999\n")
        env = dict(os.environ)
        env.pop("CPUS", None)
        env.pop("CORPUS_EXTRA_RUNTIME_MANIFEST", None)
        env.pop("CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256", None)
        env.pop("SOURCE_REVISION", None)
        manifest, runtime_identity = staged_runtime(d)
        env.update({
            "PATH": binx + os.pathsep + env["PATH"],
            "ALLOW_BUSY": "1",
            "LOADAVG_FILE": loadavg,
            "RESULTS": os.path.join(d, "results"),
            "URL": url_spec,
            "REPS": str(reps),
            "WARMUP": str(warmup),
            "COMPARISON_LABEL": "corpus",
            "CPU_BUDGET": "unlimited",
            "CONTAINER_OWNER_TOKEN": CONTAINER_OWNER_TOKEN,
            "REQBENCH_RUNTIME_MANIFEST": manifest,
            "REQBENCH_RUNTIME_BUNDLE_SHA256": runtime_identity,
        })
        env.update(cgroup_fixture(d))
        if existing_results:
            os.makedirs(env["RESULTS"])
            with open(os.path.join(env["RESULTS"], "summary.json"), "w") as handle:
                json.dump({"stale": True, "failures": 0}, handle)
        proc = subprocess.run(["bash", SH], env=env, capture_output=True, text=True)
        urls = []
        if os.path.exists(seen):
            with open(seen) as handle:
                urls = handle.read().split()
        run_json = os.path.join(d, "results", "run.json")
        meta = None
        if os.path.exists(run_json):
            with open(run_json) as handle:
                meta = json.load(handle)
        return proc, urls, meta, d

    def test_reps_cycle_the_list_the_way_the_vm_arm_does(self):
        """Red: every rep recorded the whole comma-separated spec as one URL."""
        reps, warmup = 3, 6
        proc, urls, _, _ = self._run(",".join(URLS), reps, warmup)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        self.assertEqual(urls, [URLS[rep % len(URLS)] for rep in range(warmup + reps)])

    def test_each_record_carries_the_url_it_rendered(self):
        """Red: records had no url field, so per-URL medians were underivable."""
        reps, warmup = 3, 6
        proc, _, _, d = self._run(",".join(URLS), reps, warmup)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        with open(os.path.join(d, "results", "hostcdp.jsonl")) as handle:
            rows = [json.loads(line) for line in handle]
        self.assertEqual([r["url"] for r in rows],
                         [URLS[rep % len(URLS)] for rep in range(warmup + reps)])

    def test_every_record_is_bound_to_the_exact_run_metadata(self):
        """Rows from another host run must not fit beside this run.json."""
        proc, _, meta, d = self._run(URLS[0], 3, 1)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        run_path = os.path.join(d, "results", "run.json")
        with open(run_path, "rb") as handle:
            run_digest = hashlib.sha256(handle.read()).hexdigest()
        with open(os.path.join(d, "results", "hostcdp.jsonl")) as handle:
            rows = [json.loads(line) for line in handle]
        self.assertIsInstance(meta.get("run_id"), str, meta)
        self.assertTrue(meta["run_id"], meta)
        self.assertEqual(
            [row.get("run_json_sha256") for row in rows],
            [run_digest] * len(rows),
            "the producer did not bind every row to the exact run.json bytes",
        )

    def test_run_json_records_the_parsed_corpus(self):
        """Red: KeyError 'urls' -- the record could not say what was rendered."""
        _, _, meta, _ = self._run(",".join(URLS) + " , ", 3, 6)
        self.assertEqual(meta["urls"], URLS)
        self.assertEqual(meta["url_count"], 3)

    def test_a_single_url_is_unchanged(self):
        """One URL keeps today's contract: a one-element list, every rep on it."""
        proc, urls, meta, _ = self._run(URLS[0], 3, 1)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        self.assertEqual(urls, [URLS[0]] * 4)
        self.assertEqual(meta["urls"], [URLS[0]])
        self.assertEqual(meta["url_count"], 1)

    def test_reps_is_the_measured_count_like_the_vm_arm(self):
        """Red: REPS counted warmup in, so the campaign's REPS=202 WARMUP=28
        gave 174 measured host rows against the VM arm's 202 and a partial
        final URL cycle. reqbench.py runs range(warmup + reps)."""
        reps, warmup = 6, 6
        proc, urls, meta, d = self._run(",".join(URLS), reps, warmup)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        self.assertEqual(len(urls), warmup + reps)
        self.assertEqual(urls, [URLS[rep % len(URLS)] for rep in range(warmup + reps)])
        self.assertEqual(meta["reps"], reps)
        self.assertEqual(meta["warmup"], warmup)
        self.assertEqual(meta["total_reps"], warmup + reps)
        with open(os.path.join(d, "results", "summary.json")) as handle:
            summary = json.load(handle)
        self.assertEqual(summary["n"], reps)
        # A whole number of cycles measured means per_url is balanced, which is
        # what makes an aggregate median comparable to the VM arm's.
        self.assertEqual({u: v["n"] for u, v in summary["per_url"].items()},
                         {u: reps // len(URLS) for u in URLS})

    def test_zero_reps_refuses_before_running_anything(self):
        """Red: the summary died on IndexError after the warmup reps had run."""
        proc, urls, meta, _ = self._run(URLS[0], 0, 2)
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("REPS must be >= 1", proc.stderr)
        self.assertEqual(urls, [])
        self.assertIsNone(meta)

    def test_an_existing_results_directory_is_refused_untouched(self):
        """The final directory is one run's atomic ownership claim."""
        proc, urls, meta, d = self._run(
            URLS[0], 1, 0, existing_results=True)
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("RESULTS", proc.stderr)
        self.assertEqual(urls, [])
        self.assertIsNone(meta)
        with open(os.path.join(d, "results", "summary.json")) as handle:
            self.assertEqual(json.load(handle), {"stale": True, "failures": 0})

    def test_multi_url_refuses_less_than_two_full_cycles_of_warmup(self):
        """Red: exit 0 -- a cold host arm would have been compared to a warm VM arm."""
        proc, urls, meta, _ = self._run(",".join(URLS), 3, 5)
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("two full cycles", proc.stderr)
        self.assertEqual(urls, [])
        self.assertIsNone(meta)


class HostCdpSummaryProvenance(unittest.TestCase):
    """summary.json has to say how its p50 was computed and what load it ran under.

    A host p50 is only divisible into a VM p50 if both are statistics.median,
    and a reader of summary.json alone cannot check that unless the record says
    so; old summaries that omitted the convention cannot support a ratio.
    Contention is the other half: reqbench.py stamps loadavg1 on every record
    and reqanalyze reports min/median/max "during run", while this control
    recorded only the load at start, which cannot show contention that arrived
    mid-run.
    """

    def _summary_and_rows(self, reps, warmup):
        proc, _, _, d = HostCdpCorpusSchedule._run(self, URLS[0], reps, warmup)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        with open(os.path.join(d, "results", "summary.json")) as handle:
            summary = json.load(handle)
        with open(os.path.join(d, "results", "hostcdp.jsonl")) as handle:
            rows = [json.loads(line) for line in handle]
        return summary, rows

    def test_summary_names_its_p50_convention(self):
        """Red: KeyError 'p50_convention' -- only the code knew."""
        summary, rows = self._summary_and_rows(4, 1)
        self.assertEqual(summary["p50_convention"], "statistics.median")
        measured = [r["wall_ms"] for r in rows if not r["warmup"]]
        self.assertEqual(summary["p50_ms"], round(statistics.median(measured), 1))

    def test_every_rep_records_load_and_the_summary_reports_it(self):
        """Red: KeyError 'loadavg1' -- only the start-of-run reading existed."""
        summary, rows = self._summary_and_rows(4, 1)
        self.assertEqual([r["loadavg1"] for r in rows], [1.23] * 5)
        self.assertEqual(summary["loadavg1_measured"],
                         {"n": 4, "min": 1.23, "median": 1.23, "max": 1.23})


class HostCdpDrivesInProcess(unittest.TestCase):
    """The host control drives every rep from one process, as the VM arm does.

    reqbench.py imports cdpdrive once and times each drive() call. hostcdp.sh
    started a timing wrapper and a `python3 cdpdrive.py` for every rep, so each
    host wall_ms carried two interpreter start-ups the VM arm never paid; the
    report charged 60.7 ms of the host container's wall time to that wrapper.
    """

    def _calls(self, reps):
        proc, _, _, d = HostCdpCorpusSchedule._run(self, URLS[0], reps, 1)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        with open(os.path.join(d, "python3-calls")) as handle:
            return handle.read().splitlines()

    def test_no_rep_starts_an_interpreter(self):
        """Red on main: every rep ran `python3 <wrapper>` and then
        `python3 cdpdrive.py ...`, and the starts grew with REPS."""
        few, many = self._calls(2), self._calls(6)
        programs = [call for call in many if call.split(" ")[0].endswith("cdpdrive.py")]
        self.assertEqual(programs, [], "a rep ran cdpdrive.py as its own program")
        self.assertEqual(len(many), len(few),
                         f"python3 starts grew with the rep count: {len(few)} at 2 reps, "
                         f"{len(many)} at 6")


class HostCdpDriverFailures(unittest.TestCase):
    """A rep that ends the run still leaves its row, as the per-rep wrapper
    process did when its child crashed."""

    def _run(self, action):
        os.environ["TEST_DRIVE_ACTION"] = action
        try:
            proc, _, _, d = HostCdpCorpusSchedule._run(self, URLS[0], 2, 1)
        finally:
            del os.environ["TEST_DRIVE_ACTION"]
        with open(os.path.join(d, "results", "hostcdp.jsonl")) as handle:
            rows = [json.loads(line) for line in handle]
        return proc, rows

    def test_an_interrupt_writes_the_row_and_refuses(self):
        """Red on 8b2778aa: KeyboardInterrupt escaped before out.write(), so
        the interrupted rep left no row."""
        proc, rows = self._run("interrupt")
        self.assertEqual(proc.returncode, 5, proc.stderr[-2000:])
        self.assertEqual(len(rows), 1)
        self.assertIs(rows[0]["ok"], False)
        self.assertIn("KeyboardInterrupt", rows[0]["driver"])

    def test_a_result_that_is_not_a_dict_is_a_failed_rep(self):
        """Red on 8b2778aa: result.get() raised AttributeError outside the
        handler, so the rep left no row and the run refused instead of failing."""
        proc, rows = self._run("nondict")
        self.assertEqual(proc.returncode, 4, proc.stderr[-2000:])
        self.assertEqual(len(rows), 1)
        self.assertIs(rows[0]["ok"], False)
        self.assertIn("returned str", rows[0]["driver"])


class HostCdpContainerCpu(unittest.TestCase):
    """The host control records the container's CPU per request.

    A clone's CPU is all of its one-request life, so the comparable container
    figure counts the warm container's CPU across the whole run, between
    requests included, divided by the measured count. The stub driver
    advances the fixture cgroup's usage_usec by 2.5 ms per drive().
    """

    def test_each_rep_carries_the_counters_and_the_summary_reduces_them(self):
        """Red on #1046: no container_cpu_usec_* fields, no container_cpu_ms."""
        proc, _, _, d = HostCdpCorpusSchedule._run(self, URLS[0], 4, 1)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        with open(os.path.join(d, "results", "hostcdp.jsonl")) as handle:
            rows = [json.loads(line) for line in handle]
        deltas = [r["container_cpu_usec_after"] - r["container_cpu_usec_before"] for r in rows]
        self.assertEqual(deltas, [2500] * 5)
        with open(os.path.join(d, "results", "summary.json")) as handle:
            cpu = json.load(handle)["container_cpu_ms"]
        self.assertEqual(cpu, {"window_p50_ms": 2.5, "window_mean_ms": 2.5,
                               "run_average_ms": 2.5, "n": 4})

    def test_a_container_without_a_cpu_counter_is_refused(self):
        """Red on #1046: the run had no CPU reading to refuse on."""
        d = tempfile.mkdtemp(prefix="hostcdp-cpu-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        real = HostCdpCorpusSchedule._run.__get__(self)
        original = cgroup_fixture

        def broken(directory):
            env = original(directory)
            os.remove(env["TEST_CPU_STAT"])
            return env

        globals()["cgroup_fixture"] = broken
        try:
            proc, _, _, _ = real(URLS[0], 2, 1)
        finally:
            globals()["cgroup_fixture"] = original
        self.assertEqual(proc.returncode, 5, proc.stderr[-2000:])
        self.assertIn("no usage_usec", proc.stderr)


class HostCdpContainerCgroup(unittest.TestCase):
    """The CPU counter must be the container's own cgroup, not the machine's."""

    def _run_with(self, cgroup_path, make_dir=None):
        os.environ["TEST_CGROUP_PATH"] = cgroup_path
        try:
            proc, _, _, d = HostCdpCorpusSchedule._run(self, URLS[0], 2, 1)
        finally:
            del os.environ["TEST_CGROUP_PATH"]
        return proc

    def test_the_root_cgroup_is_refused(self):
        """Red on e9691729: "/" made CPU_STAT the root cgroup's cpu.stat,
        whose usage_usec is every process on the machine."""
        for path in ("/", ""):
            proc = self._run_with(path)
            self.assertEqual(proc.returncode, 5, (path, proc.stderr[-2000:]))
            self.assertIn("not a cgroup below the root", proc.stderr)

    def test_a_climbing_path_is_refused(self):
        proc = self._run_with("/fake-container.scope/../..")
        self.assertEqual(proc.returncode, 5, proc.stderr[-2000:])
        self.assertIn("not canonical", proc.stderr)

    def test_a_cgroup_without_the_container_is_refused(self):
        """Red on e9691729: any cgroup with a cpu.stat was accepted."""
        proc = self._run_with("/fake-container.scope/other")
        self.assertEqual(proc.returncode, 5, proc.stderr[-2000:])
        self.assertIn("is not in", proc.stderr)


class HostCdpCpuSummary(unittest.TestCase):
    """hostcdp.sh's summary step, run on records with chosen counters."""

    def test_the_run_average_counts_cpu_between_requests(self):
        """Red if the run average were the mean of the drive() windows: each
        window is 2 ms, the container spends 3 ms between requests, so the
        run average from the first measured start to the last measured end is
        (3 windows + 2 gaps) / 3. The warmup row is outside the window."""
        with open(SH) as handle:
            body = handle.read()
        import re
        block = re.search(r"^python3 - \"\$OUT\" \"\$WARMUP\" \"\$RESULTS/\.summary\.pending\" <<'PY'\n(.*?)\nPY\n",
                          body, re.S | re.M)
        self.assertIsNotNone(block, "summary step not found")
        rows = []
        counter = 0
        for rep in range(4):
            before = counter
            counter += 2000
            rows.append({"rep": rep, "warmup": rep == 0, "wall_ms": 10.0 + rep,
                         "loadavg1": 0.5, "url": URLS[0],
                         "container_cpu_usec_before": before,
                         "container_cpu_usec_after": counter})
            counter += 3000
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "hostcdp.jsonl")
            with open(out, "w") as handle:
                handle.writelines(json.dumps(r) + "\n" for r in rows)
            pending = os.path.join(d, "summary.pending")
            proc = subprocess.run([sys.executable, "-", out, "1", pending],
                                  input=block.group(1), text=True, capture_output=True)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            with open(pending) as handle:
                cpu = json.load(handle)["container_cpu_ms"]
        self.assertEqual(cpu["window_p50_ms"], 2.0)
        self.assertEqual(cpu["window_mean_ms"], 2.0)
        self.assertEqual(cpu["run_average_ms"], round((3 * 2.0 + 2 * 3.0) / 3, 1))
        self.assertEqual(cpu["n"], 3)


class HostCdpCpuBudget(unittest.TestCase):
    """The host control's CPU budget has to be settable and recorded.

    A host baseline run on every core, compared against a 2-vCPU VM arm, is two
    variables. CPU_BUDGET names the comparison semantics, CPUS carries the
    finite vm-matched limit to podman, and run.json records both. An unlimited
    arm has a null cpus field rather than pretending it had a numeric limit.
    """

    def _run(self, cpus):
        d = tempfile.mkdtemp(prefix="hostcdp-cpus-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        run_argv = os.path.join(d, "podman-run-argv")
        write_podman_stub(os.path.join(binx, "podman"), run_argv)
        write_python_shim(binx, d)
        env = dict(os.environ)
        env.pop("CPUS", None)
        env.pop("CORPUS_EXTRA_RUNTIME_MANIFEST", None)
        env.pop("CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256", None)
        env.pop("SOURCE_REVISION", None)
        manifest, runtime_identity = staged_runtime(d)
        env.update({
            "PATH": binx + os.pathsep + env["PATH"],
            "ALLOW_BUSY": "1",
            "RESULTS": os.path.join(d, "results"),
            "URL": URLS[0],
            "REPS": "3",
            "WARMUP": "1",
            "COMPARISON_LABEL": "cpu-budget",
            "CPU_BUDGET": "vm-matched" if cpus is not None else "unlimited",
            "CONTAINER_OWNER_TOKEN": CONTAINER_OWNER_TOKEN,
            "REQBENCH_RUNTIME_MANIFEST": manifest,
            "REQBENCH_RUNTIME_BUNDLE_SHA256": runtime_identity,
        })
        env.update(cgroup_fixture(d))
        if cpus is not None:
            env["CPUS"] = cpus
        proc = subprocess.run(["bash", SH], env=env, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        with open(run_argv) as handle:
            argv = handle.read().split("\0")
        with open(os.path.join(d, "results", "run.json")) as handle:
            meta = json.load(handle)
        return argv, meta

    def test_cpus_reaches_podman_and_the_record(self):
        """Red: no --cpus in the podman argv and KeyError 'cpus' in run.json."""
        argv, meta = self._run("2")
        self.assertIn("--cpus", argv)
        self.assertEqual(argv[argv.index("--cpus") + 1], "2")
        self.assertEqual(meta["cpu_budget"], "vm-matched")
        self.assertEqual(meta["cpus"], 2)
        self.assertIsInstance(meta["cpus"], int)

    def test_unset_cpus_leaves_the_budget_alone_and_records_null(self):
        """Unset means the whole machine, and the record says so rather than lying."""
        argv, meta = self._run(None)
        self.assertNotIn("--cpus", argv)
        self.assertEqual(meta["cpu_budget"], "unlimited")
        self.assertIsNone(meta["cpus"])


if __name__ == "__main__":
    unittest.main()
