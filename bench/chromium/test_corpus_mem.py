#!/usr/bin/env python3
"""The memory harness's gates and refusals, pinned.

corpus_mem.py runs for hours and publishes per-instance memory and per-render
results for fcvm against host containers. Every property here is a way for it
to finish and report a number that is not a measurement: a preflight that clears
the box because it could not look, a basis summed over a process set the sample
never saw, a subprocess that outlives the deadline meant to bound it, and a
recorded arm thrown away by the failure of the arm after it.

None of them raises on its own. Each one produces a run that looks finished.

Run: python3 -m unittest test_corpus_mem -v
"""

import errno
import fcntl
import hashlib
import io
import json
import math
import os
import random
import re
import runpy
import select
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import ExitStack
from types import SimpleNamespace
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import corpus_mem  # noqa: E402
import compare as bench_compare  # noqa: E402
import campaign_summary as bench_campaign_summary  # noqa: E402
import host_resource_finalizer  # noqa: E402
import phase_supervisor  # noqa: E402
import report as bench_report  # noqa: E402
import serve_guardian  # noqa: E402

EXTRA = os.path.join(HERE, "corpus_extra.sh")
CORPUS_MEM = os.path.join(HERE, "corpus_mem.py")
CAMPAIGN = os.path.join(HERE, "corpus_campaign.sh")
HOSTCDP = os.path.join(HERE, "hostcdp.sh")
OWNED_PROCESS = os.path.join(HERE, "owned_process.py")
PHASE_SUPERVISOR = os.path.join(HERE, "phase_supervisor.py")
HOST_RESOURCE_FINALIZER = os.path.join(HERE, "host_resource_finalizer.py")
SERVE_GUARDIAN = os.path.join(HERE, "serve_guardian.py")


def proc_state(pid):
    """Return one procfs process state, or None once that process is gone."""
    identity = phase_supervisor.read_process_stat(pid)
    return None if identity is None else identity["state"]


def kill_and_reap_test_process_group(process):
    """Kill the session rooted at process and drain its captured pipes."""
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    return process.communicate(timeout=5)


def communicate_test_process_group(process, timeout):
    try:
        return process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        kill_and_reap_test_process_group(process)
        raise


MAKEFILE = os.path.join(os.path.dirname(os.path.dirname(HERE)), "Makefile")


class Completed:
    """Stands in for subprocess.CompletedProcess without running anything."""

    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr


class CorpusExtraSchedule(unittest.TestCase):
    """The host control and VM campaign execute the same measured schedule."""

    @staticmethod
    def default(path, name):
        with open(path) as handle:
            source = handle.read()
        match = re.search(
            rf'^{name}="\$\{{{name}:-([0-9]+)\}}"', source, re.MULTILINE)
        if not match:
            raise AssertionError(f"no numeric {name} default in {path}")
        return int(match.group(1))

    def test_host_control_measured_reps_match_the_vm_campaign(self):
        self.assertEqual(self.default(EXTRA, "REPS"), self.default(CAMPAIGN, "REPS"),
                         "the host and VM arms weight the 14-page corpus differently")


class MemoryCellSchedule(unittest.TestCase):
    """Matched sides stay adjacent in a reproducible, recorded cell schedule."""

    def test_memory_cells_are_interleaved_by_recorded_seed(self):
        sides = ["fcvm", "container"]
        schedule = corpus_mem.build_cell_schedule(
            sides, [1, 2, 4, 8], 14, seed=9182, url_count=14)
        self.assertEqual(schedule,
                         corpus_mem.build_cell_schedule(
                             sides, [1, 2, 4, 8], 14, seed=9182, url_count=14))
        expected = sorted((side, n, rep)
                          for n in (1, 2, 4, 8)
                          for rep in range(1, 15)
                          for side in sides)
        self.assertEqual(sorted((side, n, rep) for side, n, rep, _urls in schedule),
                         expected)
        covered = set()
        url_by_rep = {}
        per_n_urls = {n: [] for n in (1, 2, 4, 8)}
        for offset in range(0, len(schedule), len(sides)):
            pair = schedule[offset:offset + len(sides)]
            self.assertEqual({side for side, _n, _rep, _urls in pair}, set(sides))
            self.assertEqual(len({(n, rep) for _side, n, rep, _urls in pair}), 1,
                             f"matched sides were separated in {pair}")
            self.assertEqual(pair[0][3], pair[1][3],
                             f"matched sides rendered different pages in {pair}")
            n, rep = pair[0][1:3]
            url_indices = pair[0][3]
            self.assertEqual(len(url_indices), n)
            self.assertEqual(len(set(url_indices)), 1,
                             f"N={n} gave its instances different page histories")
            if rep in url_by_rep:
                self.assertEqual(
                    url_indices[0], url_by_rep[rep],
                    f"N={n} changed the normalized page workload for repetition {rep}",
                )
            else:
                url_by_rep[rep] = url_indices[0]
            per_n_urls[n].append(url_indices[0])
            covered.update(url_indices)
        self.assertEqual(covered, set(range(14)),
                         "the default N/repetition grid omits corpus members")
        for n, urls in per_n_urls.items():
            self.assertEqual(sorted(urls), list(range(14)),
                             f"N={n} did not see one balanced corpus cycle per side")

        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            source = handle.read()
        self.assertIn('"schedule_seed"', source,
                      "the seed needed to reproduce the cell order is not recorded")
        main = source[source.index("def main():"):]
        self.assertIn("schedule = build_cell_schedule(", main,
                      "main can bypass the interleaved schedule the test exercises")
        self.assertIn("for side_name, n, rep, url_indices in schedule:", main)
        self.assertRegex(main, r"run_cell\(\s*sides\[side_name\], args, n, rep, url_indices, out\)")


class MemoryFitPublication(unittest.TestCase):
    """Published memory fits carry uncertainty and agree across bases."""

    @staticmethod
    def cell(side, n, rep, cgroup, pss, available_delta):
        pre_available = 1_000_000.0
        steady = {
            "clones" if side == "fcvm-clone" else "pool_containers": n,
            "clone_procs" if side == "fcvm-clone" else "pool_procs": n * 4,
            "clone_cgroup_kb" if side == "fcvm-clone" else "pool_cgroup_kb":
                cgroup * 1024,
            "clone_pss_kb" if side == "fcvm-clone" else "pool_pss_kb":
                pss * 1024,
            "mem_available_kb": pre_available - available_delta * 1024,
        }
        if side == "fcvm-clone":
            steady.update(
                serve_cgroup_kb=64 * 1024,
                serve_pss_kb=48 * 1024,
            )
        return {
            "side": side,
            "n": n,
            "rep": rep,
            "pre": {"mem_available_kb": pre_available},
            "steady": [dict(steady), dict(steady), dict(steady)],
            "post": {},
        }

    def cells(self, slopes=(100.0, 90.0, 110.0), intercepts=(40.0, 30.0, 50.0)):
        cells = []
        for side in ("fcvm-clone", "host-container"):
            for n in (1, 2, 4, 8):
                for rep, jitter in enumerate((-8.0, -3.0, 0.0, 4.0, 9.0), 1):
                    cells.append(self.cell(
                        side, n, rep,
                        intercepts[0] + slopes[0] * n + jitter * (1.0 + n / 4),
                        intercepts[1] + slopes[1] * n + jitter * (0.8 + n / 5),
                        intercepts[2] + slopes[2] * n + jitter * (1.2 + n / 6),
                    ))
        return cells

    def test_memory_fit_quotes_bootstrap_intervals_and_rounds_to_them(self):
        fits = corpus_mem.summarize_memory_fits(
            self.cells(), seed=9182, bootstrap_resamples=1000
        )
        record = fits["fcvm-clone"]["cgroup_mib"]
        uncertainty = record["uncertainty"]
        self.assertEqual(uncertainty["method"], "repetition-block bootstrap")
        self.assertEqual(uncertainty["confidence"], 0.95)
        self.assertEqual(uncertainty["resamples"], 1000)
        self.assertEqual(fits["fcvm-clone"]["n_range"], [1, 8])
        self.assertEqual(fits["fcvm-clone"]["repetition_blocks"], 5)
        for estimate_key, interval_key, rounding_key in (
                ("marginal_mib_per_instance",
                 "marginal_mib_per_instance_ci95", "marginal"),
                ("fixed_mib", "fixed_mib_ci95", "fixed")):
            estimate = record[estimate_key]
            low, high = record[interval_key]
            self.assertLess(low, high)
            self.assertLessEqual(low, estimate)
            self.assertLessEqual(estimate, high)
            step = uncertainty["rounding_mib"][rounding_key]
            for value in (low, estimate, high):
                self.assertAlmostEqual(value / step, round(value / step), places=7)

    def test_twofold_cross_basis_gap_blocks_memory_completion(self):
        cases = {
            "marginal": self.cells(
                slopes=(40.0, 100.0, 88.0),
                intercepts=(100.0, 0.0, 20.0),
            ),
            "observed totals": self.cells(
                slopes=(100.0, 100.0, 100.0),
                intercepts=(0.0, 50.0, 1_000.0),
            ),
        }
        for label, cells in cases.items():
            with self.subTest(label=label):
                with self.assertRaises(RuntimeError) as caught:
                    corpus_mem.summarize_memory_fits(
                        cells, seed=9182, bootstrap_resamples=200
                    )
                self.assertIn("cross-basis", str(caught.exception))
                self.assertIn("2x", str(caught.exception))

    def test_fewer_than_five_repetition_blocks_cannot_publish_a_ci(self):
        cells = [cell for cell in self.cells() if cell["rep"] <= 2]
        with self.assertRaises(RuntimeError) as caught:
            corpus_mem.summarize_memory_fits(
                cells, seed=9182, bootstrap_resamples=200
            )
        self.assertIn("five repetition blocks", str(caught.exception))

    def test_compatible_bases_record_the_reconciliation(self):
        fits = corpus_mem.summarize_memory_fits(
            self.cells(), seed=9182, bootstrap_resamples=200
        )
        for side in ("fcvm-clone", "host-container"):
            reconciliation = fits[side]["cross_basis_reconciliation"]
            self.assertEqual(reconciliation["status"], "accepted")
            self.assertLess(reconciliation["maximum_pairwise_ratio"], 2.0)
            self.assertEqual(reconciliation["refusal_ratio"], 2.0)

    def test_memory_fit_reports_density_at_each_measured_n(self):
        cells = self.cells()
        fits = corpus_mem.summarize_memory_fits(
            cells, seed=9182, bootstrap_resamples=200
        )
        n = 4
        totals = []
        for cell in cells:
            if cell["side"] != "fcvm-clone" or cell["n"] != n:
                continue
            values = corpus_mem.cell_values(cell)
            totals.append(values["cgroup_mib"] + values["serve_cgroup_mib"])
        density = [n * 1024.0 / total for total in totals]
        memory_record = fits["fcvm-clone"]["per_n"][n]["cgroup_mib"]
        self.assertEqual(
            memory_record["scope"],
            "arrangement total including shared snapshot serve",
        )
        self.assertIs(memory_record["includes_shared_serve"], True)
        self.assertEqual(
            fits["fcvm-clone"]["cgroup_mib"]["scope"],
            "clone-incremental fit; shared snapshot serve reported separately",
        )
        self.assertIn("shared_serve_fixed_cost", fits["fcvm-clone"])
        per_instance = [total / n for total in totals]
        memory_median = statistics.median(per_instance)
        memory_spread = max(
            memory_median - min(per_instance),
            max(per_instance) - memory_median,
        )
        memory_step = 10.0 ** math.floor(math.log10(memory_spread))
        self.assertEqual(
            memory_record["statistic"], "descriptive repetition-block median"
        )
        self.assertEqual(memory_record["rounding"], memory_step)
        memory_low, memory_high = memory_record["observed_range"]
        self.assertLessEqual(memory_low, min(per_instance))
        self.assertGreaterEqual(memory_high, max(per_instance))
        for value in (memory_low, memory_record["median"], memory_high):
            self.assertAlmostEqual(
                value / memory_step, round(value / memory_step), places=7
            )

        record = memory_record["requests_per_gib"]
        self.assertEqual(record["scope"], memory_record["scope"])
        self.assertIs(record["includes_shared_serve"], True)
        self.assertEqual(record["statistic"], "descriptive repetition-block median")
        raw_median = statistics.median(density)
        spread = max(raw_median - min(density), max(density) - raw_median)
        expected_step = 10.0 ** math.floor(math.log10(spread))
        self.assertEqual(record["rounding"], expected_step)
        low, high = record["observed_range"]
        self.assertLessEqual(low, min(density))
        self.assertGreaterEqual(high, max(density))
        for value in (low, record["median"], high):
            self.assertAlmostEqual(
                value / expected_step, round(value / expected_step), places=7
            )

    def test_completion_consumes_reconciled_uncertain_fits(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read()
        main = source[source.index("def main_with_resources(resources):") :]
        summarize = main.find(
            'summary["fits"] = summarize_memory_fits(cells, args.seed)'
        )
        self.assertGreaterEqual(
            summarize, 0,
            "the production summary bypasses the uncertainty and basis gate",
        )
        bootstrap = source[source.index("def bootstrap_memory_lifecycle"):
                           source.index("def main_with_resources(resources):")]
        lifecycle = bootstrap.find("status = run_memory_lifecycle(")
        complete = bootstrap.find("publish_completion(results, run_id)")
        self.assertGreaterEqual(lifecycle, 0)
        self.assertGreater(
            complete, lifecycle,
            "completion is published before the worker and its finalizer finish",
        )


class SnapshotGenerationLease(unittest.TestCase):
    """Every measured clone must consume the generation recorded in run.json."""

    def test_generation_is_read_under_a_shared_lease_held_by_the_caller(self):
        with tempfile.TemporaryDirectory() as data_root:
            snapshots = os.path.join(data_root, "snapshots")
            os.makedirs(snapshots)
            lock_path = os.path.join(snapshots, "golden.lock")
            open(lock_path, "a").close()
            generation = {"generation_id": "generation-under-test"}
            with mock.patch.object(corpus_mem, "snapshot_generation",
                                   return_value=generation):
                with ExitStack() as resources:
                    actual = corpus_mem.snapshot_generation_under_lease(
                        resources, data_root, "golden")
                    probe = subprocess.run(
                        ["flock", "-x", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=10)
                    self.assertNotEqual(
                        probe.returncode, 0,
                        "a creator could replace the recorded generation during the run",
                    )
                    self.assertIs(actual, generation)
                probe = subprocess.run(
                    ["flock", "-x", "-n", lock_path, "true"],
                    capture_output=True, text=True, timeout=10)
                self.assertEqual(probe.returncode, 0, probe.stderr)

    def test_main_uses_the_whole_run_resource_stack_for_the_snapshot_lease(self):
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            source = handle.read()
        self.assertIn("def main_with_resources(resources", source)
        body = source[source.index("def main_with_resources(resources"):]
        lease_match = re.search(
            r"snapshot_generation_under_lease\(\s*resources", body)
        lease = -1 if lease_match is None else lease_match.start()
        serve = body.find("fcvm_side.start_serve()")
        self.assertGreaterEqual(lease, 0)
        self.assertGreater(serve, lease,
                           "the serve can load a generation before the lease is held")


class RunScopedContainerCleanup(unittest.TestCase):
    """One run never names, waits for, or removes another run's containers."""

    def test_memory_container_names_include_the_full_run_id(self):
        args = SimpleNamespace()
        first = corpus_mem.ContainerSide(args, "a" * 32).prefix("host1r1")
        second = corpus_mem.ContainerSide(args, "b" * 32).prefix("host1r1")
        self.assertNotEqual(first, second)
        self.assertIn("a" * 32, first)
        self.assertIn("b" * 32, second)

    def test_teardown_does_not_wait_for_a_peer_runs_container(self):
        token = "c" * 32
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token=token), "a" * 32)
        container_id = "e" * 64
        owned = {"name": side.prefix("host1r1") + "0",
                 "container_id": container_id}
        calls = []

        def shell(cmd, *_args, **_kwargs):
            calls.append(cmd)
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, f"{container_id} {token}\n", "")
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(1, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", shell):
            side.tear_down([owned])
        self.assertFalse(any(cmd[:3] == ["podman", "ps", "-a"] for cmd in calls))

    def test_teardown_that_cannot_identify_the_target_keeps_it_owned(self):
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token="c" * 32), "a" * 32)
        owned = {"name": side.prefix("host1r1") + "0"}
        side.owned.add(owned["name"])

        def shell(cmd, *_args, **_kwargs):
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            return Completed(125, "", "podman unavailable")

        with mock.patch.object(corpus_mem, "sh_bounded", shell):
            with self.assertRaises(RuntimeError):
                side.tear_down([owned])
        self.assertIn(owned["name"], side.owned)

    def test_outer_cleanup_reaps_only_its_run_after_a_signal(self):
        with open(EXTRA) as handle:
            source = handle.read()
        match = re.search(r'^cleanup\(\) \{\n(.*?)^\}', source,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, "corpus_extra cleanup function is gone")
        self.assertIn("cleanup_owned_containers", match.group(1),
                      "SIGTERM can leave this run's detached containers alive")
        cleanup = re.search(r'^cleanup_owned_containers\(\) \{\n.*?^\}', source,
                            re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(cleanup, "run-owned container cleanup is gone")
        owner = "a" * 32
        peer = "b" * 32
        token = "c" * 32
        peer_token = "d" * 32
        names = (
            f"cbmem-{owner}-host1r1-0",
            f"hostcdp-{owner}-free",
            f"cbmem-{peer}-host1r1-0",
            f"hostcdp-{peer}-free",
        )
        ids = tuple(str(i) * 64 for i in range(4))
        with tempfile.TemporaryDirectory() as tmp:
            removed = os.path.join(tmp, "removed")
            podman = os.path.join(tmp, "podman")
            with open(podman, "w") as handle:
                handle.write(
                    "#!/bin/sh\n"
                    "last=\nfor arg do last=$arg; done\n"
                    "case $1 in\n"
                    f"  ps) printf '%s\\n' {' '.join(repr(f'{ids[i]} {name}') for i, name in enumerate(names))} ;;\n"
                    "  inspect) case \"$last\" in\n"
                    f"    {ids[0]}) echo '{ids[0]} {token}' ;;\n"
                    f"    {ids[1]}) echo '{ids[1]} {token}' ;;\n"
                    f"    *) echo \"$last {peer_token}\" ;; esac ;;\n"
                    "  rm) printf '%s\\n' \"$last\" >>\"$REMOVED\" ;;\n"
                    "  container) exit 1 ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n")
            os.chmod(podman, 0o755)
            script = ("set -euo pipefail\n"
                      f"RUN_ID={owner}\nCONTAINER_OWNER_TOKEN={token}\n"
                      + cleanup.group(0) + "\n"
                      + "cleanup_owned_containers\n")
            env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                       REMOVED=removed)
            proc = subprocess.run(["bash", "-c", script], env=env,
                                  capture_output=True, text=True, timeout=60)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(removed) as handle:
                actual = set(handle.read().splitlines())
        self.assertEqual(actual, set(ids[:2]),
                         "cleanup crossed the run ownership boundary")
        self.assertIn('--run-id "$RUN_ID"', source,
                      "the child and outer cleanup do not share an owner ID")
        self.assertIn('--container-owner-token "$CONTAINER_OWNER_TOKEN"', source,
                      "the child and outer cleanup do not share ownership proof")
        self.assertIn("stop_active_phase", match.group(1),
                      "cleanup can race a child that is still creating containers")
        self.assertLess(match.group(1).find("stop_active_phase"),
                        match.group(1).find("cleanup_owned_containers"),
                        "the child must stop before its owned containers are enumerated")
        self.assertIn("phase_supervisor.py", source,
                      "the phase process group has no stable supervisor")
        self.assertNotIn('setsid "$@"', source)
        self.assertNotIn('kill -TERM -- "-$pid"', source)

    def test_reused_root_pid_is_never_signalled(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("owned_process", OWNED_PROCESS)
        self.assertIsNotNone(spec)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        sent = []
        identities = iter((111, 222))
        result = module.signal_if_identity(
            4242, 111, 15,
            read_identity=lambda _pid: next(identities),
            open_pidfd=lambda _pid: 9,
            send_signal=lambda fd, sig: sent.append((fd, sig)),
            close_pidfd=lambda _fd: None,
        )
        self.assertFalse(result)
        self.assertEqual(sent, [], "a replacement process received the cleanup signal")

    def test_corpus_serve_ownership_is_cleared_after_stop(self):
        with open(EXTRA) as handle:
            source = handle.read()
        function = re.search(r'^stop_corpus_serve\(\) \{\n.*?^\}', source,
                             re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(function)
        body = function.group(0)
        self.assertIn('printf T >&"$control_fd"', body)
        self.assertIn('wait "$pid"', body)
        self.assertNotIn("owned_process.py", body)
        self.assertIn('SERVE_JOB_PID=""', body)
        self.assertIn('SERVE_CONTROL_FD=""', body)
        self.assertIn('SERVE_CONTROL_PATH=""', body)

    def test_a_phase_leader_cannot_leave_an_untracked_descendant(self):
        with open(EXTRA) as handle:
            source = handle.read()
        functions = []
        for name in ("stop_active_phase", "run_logged"):
            match = re.search(rf'^{name}\(\) \{{\n.*?^\}}', source,
                              re.MULTILINE | re.DOTALL)
            self.assertIsNotNone(match, f"{name} is gone")
            functions.append(match.group(0))
        with tempfile.TemporaryDirectory() as tmp:
            child_pid = os.path.join(tmp, "child.pid")
            log_path = os.path.join(tmp, "phase.log")
            script = (
                "set -uo pipefail\n"
                "say() { :; }\n"
                + f"BENCH={HERE!r}\n"
                "ACTIVE_PHASE_PID=\nACTIVE_PHASE_SIGNAL=\n"
                "ACTIVE_PHASE_CONTROL_FD=\nACTIVE_PHASE_CONTROL_PATH=\n"
                + "\n".join(functions) + "\n"
                + "set +e\n"
                + f"CHILD_PID={child_pid!r} run_logged {log_path!r} "
                  "sh -c 'sleep 60 </dev/null >/dev/null 2>&1 & "
                  "echo $! >\"$CHILD_PID\"'\n"
                + "phase_rc=$?\n"
                + f"child=$(cat {child_pid!r})\n"
                + "if kill -0 \"$child\" 2>/dev/null; then "
                  "echo SURVIVED; kill -KILL \"$child\" 2>/dev/null || true; fi\n"
                + "exit \"$phase_rc\"\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=20)
        self.assertNotEqual(
            proc.returncode, 0,
            "run_logged reported success after its process group outlived the leader",
        )
        self.assertNotIn("SURVIVED", proc.stdout,
                         "the descendant escaped both phase accounting and cleanup")

    def test_a_failed_name_collision_never_makes_the_peer_owned(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="a" * 32)
        side = corpus_mem.ContainerSide(args, "b" * 32)
        removed = []
        name = side.prefix("host1r1") + "0"

        def bounded(cmd, _timeout):
            if cmd[:2] == ["podman", "create"]:
                return Completed(125, "", "name is already in use")
            if cmd[:3] == ["podman", "rm", "-f"]:
                removed.append(cmd[-1])
                return Completed()
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, f"{'d' * 64} peer-owner\n", "")
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            if cmd[:3] == ["podman", "ps", "-a"]:
                return Completed(0, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
            with self.assertRaises(RuntimeError):
                side.stop_all()
        self.assertEqual(removed, [],
                         "cleanup deleted a same-name container this run did not create")
        self.assertIn(name, side.owned,
                      "failed ownership proof discarded the cleanup obligation")

    def test_partial_creation_is_cleaned_by_owner_label_and_exact_id(self):
        token = "a" * 32
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token=token)
        side = corpus_mem.ContainerSide(args, "b" * 32)
        removed = []
        name = side.prefix("host1r1") + "0"

        def bounded(cmd, _timeout):
            if cmd[:2] == ["podman", "create"]:
                return Completed(124, "", "timed out after create")
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, f"{'c' * 64} {token}\n", "")
            if cmd[:3] == ["podman", "rm", "-f"]:
                removed.append(cmd[-1])
                return Completed()
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(1, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
            side.stop_all()
        self.assertEqual(removed, ["c" * 64])
        self.assertNotIn(name, side.owned)

    def test_memory_container_finalizer_survives_worker_sigkill(self):
        """A started Podman container has a cleanup owner outside the worker.

        RED BEFORE THE FIX: killing corpus_mem.py after bring_up returned left
        the detached container alive because only the killed worker remembered
        its exact ID and owner token.
        """
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.mkdir(bindir)
            podman = os.path.join(bindir, "podman")
            state_path = os.path.join(tmp, "container.json")
            ready_path = os.path.join(tmp, "worker.ready")
            absence_path = os.path.join(tmp, "absence.proved")
            lock_dir = os.path.join(tmp, "create-ops")
            os.mkdir(lock_dir)
            lifecycle_dir = os.path.join(tmp, "lifecycle")
            os.mkdir(lifecycle_dir)
            container_id = "c" * 64
            token = "a" * 32
            run_id = "b" * 32
            name = f"cbmem-{run_id}-host1r1-0"
            with open(podman, "w") as handle:
                handle.write(
                    "#!/usr/bin/env python3\n"
                    "import json, os, sys, tempfile\n"
                    "args = sys.argv[1:]\n"
                    "state_path = os.environ['FAKE_CONTAINER_STATE']\n"
                    "absence_path = os.environ['FAKE_ABSENCE_PROOF']\n"
                    f"container_id = {container_id!r}\n"
                    "def read_state():\n"
                    "    try:\n"
                    "        with open(state_path) as source:\n"
                    "            return json.load(source)\n"
                    "    except FileNotFoundError:\n"
                    "        return None\n"
                    "command = args[0]\n"
                    "if command == 'ps':\n"
                    "    state = read_state()\n"
                    "    if state is not None:\n"
                    "        print(state['id'] + '|' + state['name'])\n"
                    "    else:\n"
                    "        with open(absence_path, 'w') as target:\n"
                    "            target.write('absent\\n')\n"
                    "elif command == 'create':\n"
                    "    name = args[args.index('--name') + 1]\n"
                    "    label = args[args.index('--label') + 1]\n"
                    "    token = label.split('=', 1)[1]\n"
                    "    fd, temporary = tempfile.mkstemp(dir=os.path.dirname(state_path))\n"
                    "    with os.fdopen(fd, 'w') as target:\n"
                    "        json.dump({'id': container_id, 'name': name, 'token': token}, target)\n"
                    "    os.replace(temporary, state_path)\n"
                    "    print(container_id)\n"
                    "elif command == 'inspect':\n"
                    "    state = read_state()\n"
                    "    if state is None or args[-1] not in (state['id'], state['name']):\n"
                    "        raise SystemExit(125)\n"
                    "    separator = '|' if '|' in ' '.join(args) else ' '\n"
                    "    print(state['id'] + separator + state['token'])\n"
                    "elif command == 'start':\n"
                    "    state = read_state()\n"
                    "    raise SystemExit(0 if state and args[-1] == state['id'] else 125)\n"
                    "elif command == 'exec':\n"
                    "    raise SystemExit(0)\n"
                    "elif command == 'logs':\n"
                    "    raise SystemExit(0)\n"
                    "elif command == 'rm':\n"
                    "    state = read_state()\n"
                    "    if state is None or args[-1] not in (state['id'], state['name']):\n"
                    "        raise SystemExit(125)\n"
                    "    os.unlink(state_path)\n"
                    "elif command == 'container' and args[1] == 'exists':\n"
                    "    state = read_state()\n"
                    "    if state is not None and args[-1] in (state['id'], state['name']):\n"
                    "        raise SystemExit(0)\n"
                    "    with open(absence_path, 'w') as target:\n"
                    "        target.write('absent\\n')\n"
                    "    raise SystemExit(1)\n"
                    "else:\n"
                    "    raise SystemExit(64)\n"
                )
            os.chmod(podman, 0o755)
            worker_code = (
                "import os,signal,sys\n"
                f"sys.path.insert(0, {HERE!r})\n"
                "import corpus_mem\n"
                "from types import SimpleNamespace\n"
                "args=SimpleNamespace(\n"
                " image='image', image_id='image',\n"
                " urls=['https://example.com/'],\n"
                f" container_owner_token={token!r},\n"
                f" container_create_ops_dir={lock_dir!r})\n"
                f"side=corpus_mem.ContainerSide(args, {run_id!r})\n"
                "side.bring_up(1, 'host1r1', [0])\n"
                f"with open({ready_path!r}, 'w') as target:\n"
                " target.write(str(os.getpid()))\n"
                "signal.pause()\n"
            )
            wrapper_code = (
                "import sys\n"
                f"sys.path.insert(0, {HERE!r})\n"
                "import corpus_mem\n"
                "raise SystemExit(corpus_mem.run_memory_lifecycle(\n"
                f" [sys.executable, '-c', {worker_code!r}],\n"
                f" {run_id!r}, {token!r}, {lock_dir!r}, {lifecycle_dir!r},\n"
                " term_grace=0.05, kill_grace=2.0))\n"
            )
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                FAKE_CONTAINER_STATE=state_path,
                FAKE_ABSENCE_PROOF=absence_path,
            )
            wrapper = subprocess.Popen(
                [sys.executable, "-c", wrapper_code], env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not os.path.isfile(ready_path):
                    if wrapper.poll() is not None:
                        stdout, stderr = wrapper.communicate()
                        self.fail(
                            f"memory worker exited before readiness: "
                            f"{wrapper.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail("memory worker never started its container")
                    time.sleep(0.01)
                self.assertTrue(os.path.isfile(state_path))

                with open(ready_path) as handle:
                    worker_pid = int(handle.read())
                os.kill(worker_pid, signal.SIGKILL)
                deadline = time.monotonic() + 10
                while (not os.path.isfile(absence_path)
                       and time.monotonic() < deadline):
                    time.sleep(0.01)
                self.assertTrue(
                    os.path.isfile(absence_path),
                    "SIGKILL left the memory container without a live finalizer",
                )
                self.assertFalse(os.path.exists(state_path))
                wrapper.wait(timeout=5)
            finally:
                if wrapper.poll() is None:
                    wrapper.kill()
                    wrapper.wait(timeout=5)
                if os.path.isfile(state_path):
                    subprocess.run(
                        [podman, "rm", "-f", "--", container_id],
                        env=env, capture_output=True, text=True, timeout=5,
                    )
                for pipe in (wrapper.stdout, wrapper.stderr):
                    if pipe is not None:
                        pipe.close()

        with open(CORPUS_MEM) as handle:
            memory_source = handle.read()
        bootstrap = memory_source.index("def bootstrap_memory_lifecycle")
        worker = memory_source.index("def main_with_resources")
        self.assertLess(bootstrap, worker)
        self.assertIn("run_memory_lifecycle(", memory_source[bootstrap:worker])
        with open(EXTRA) as handle:
            outer_source = handle.read()
        match = re.search(r'^run_logged\(\) \{\n.*?^\}', outer_source,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match)
        run_logged = match.group(0)
        self.assertIn('--finalizer "$BENCH/host_resource_finalizer.py"', run_logged)
        self.assertIn("FCVM_FINALIZER_MODE=container-set", run_logged)
        memory_call = outer_source[outer_source.index(
            'if [[ ",$PHASES," == *",memory,"* ]]'):]
        self.assertIn("ACTIVE_PHASE_FINALIZER=memory-containers", memory_call)

    def test_shared_replay_ports_are_locked_before_dnsmasq_is_touched(self):
        with open(EXTRA) as handle:
            source = handle.read()
        lock = source.find("flock -n 9")
        dnsmasq = source.find("systemctl stop dnsmasq")
        self.assertGreaterEqual(lock, 0, "the shared DNS/HTTP/HTTPS ports have no lease")
        self.assertGreaterEqual(dnsmasq, 0, "the dnsmasq handoff is gone")
        self.assertLess(lock, dnsmasq,
                        "the shared-port lease starts after host DNS is already changed")

    def test_shared_replay_ports_use_one_host_wide_lock(self):
        with open(EXTRA) as handle:
            source = handle.read()
        self.assertIn('CORPUS_EXTRA_LOCK="/run/lock/fcvm-corpus-extra.lock"', source,
                      "different UIDs can mutate the same host ports and dnsmasq concurrently")
        self.assertIn(
            'sudo -n install -d -o root -g root -m 0755 "$CORPUS_EXTRA_LOCK"',
            source,
            "the first caller can leave a lock inode another UID cannot open")
        self.assertIn('exec 9<"$CORPUS_EXTRA_LOCK"', source,
                      "opening a shared regular file with O_CREAT is denied across UIDs")

    def test_empty_phases_refuse_before_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "results")
            logs = os.path.join(tmp, "logs")
            env = dict(os.environ, PHASES="", RUN_ID="0" * 32,
                       RESULTS=results, LOGDIR=logs)
            result = subprocess.run(
                ["bash", EXTRA], env=env, capture_output=True, text=True,
                timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(os.path.exists(results))
            self.assertFalse(os.path.exists(logs))

    def test_host_control_name_is_derived_from_its_run_id(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        match = re.search(r'^CNAME="([^"]+)"', source, re.MULTILINE)
        self.assertIsNotNone(match)
        self.assertIn("$RUNID", match.group(1))
        self.assertNotIn("$$", match.group(1))

    def test_host_control_readiness_proves_its_container_owns_cdp(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        self.assertIn("container_owns_cdp", source)
        readiness = source[source.index("until "):source.index("log \"warm marker up")]
        self.assertIn("container_owns_cdp", readiness)

    def test_standalone_host_control_refuses_both_vmm_processes_fail_closed(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        self.assertIn("for process in fcvm firecracker", source)
        self.assertRegex(source, r'case "\$rc" in\s*0\)')
        self.assertRegex(source, r'\s1\)')

    def test_container_render_outputs_are_removed_before_sampling(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="c" * 32)
        side = corpus_mem.ContainerSide(args, "a" * 32)
        calls = []

        def shell(cmd, *_args, **_kwargs):
            calls.append(cmd)
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, "c" * 64 + " " + "c" * 32 + "\n", "")
            return Completed(0, "c" * 64 + "\n", "")

        with mock.patch.object(corpus_mem, "sh", shell), \
             mock.patch.object(corpus_mem, "sh_bounded", shell):
            side.bring_up(1, "host1r1", [0])
        cleanup_calls = [cmd for cmd in calls
                         if cmd[:4] == ["podman", "exec", side.prefix("host1r1") + "0", "rm"]]
        self.assertTrue(cleanup_calls,
                        "render.py's JPEG and DOM remain charged to the container cgroup")

    def test_failed_podman_listing_cannot_report_cleanup_complete(self):
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token="c" * 32), "a" * 32)
        name = side.prefix("host1r1") + "0"
        side.owned.add(name)

        def bounded(cmd, _timeout):
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            return Completed(125, "", "remove failed")

        with mock.patch.object(corpus_mem, "sh_bounded", bounded, create=True):
            with self.assertRaises(RuntimeError):
                side.tear_down([{"name": name}])
        self.assertIn(name, side.owned,
                      "failed cleanup discarded the only record of its live container")

    def test_failed_podman_run_keeps_the_name_owned_for_final_cleanup(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="c" * 32)
        side = corpus_mem.ContainerSide(args, "a" * 32)
        name = side.prefix("host1r1") + "0"
        with mock.patch.object(
                corpus_mem, "sh_bounded",
                return_value=Completed(125, "", "runtime failed after create")):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
        self.assertIn(name, side.owned)

    def test_failed_clone_readiness_keeps_the_process_owned_for_final_cleanup(self):
        proc = mock.Mock()
        proc.poll.return_value = None
        args = SimpleNamespace(
            results="/tmp", fcvm="fcvm", state_dir="/state", cdp_port=9222,
            urls=["https://example.com/"], tag="tag",
        )
        cg = mock.Mock()
        cg.leaf.return_value = "/cgroup/leaf"
        side = corpus_mem.FcvmSide(args, cg, "a" * 32)
        with mock.patch.object(corpus_mem, "spawn_in_cgroup", return_value=proc), \
             mock.patch.object(corpus_mem, "find_clone_state", return_value=None):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "fcvm1r1", [0])
        self.assertIn("mem-" + "a" * 32 + "-fcvm1r1-0", side.owned)

    def test_memory_clones_enable_fcvm_debug_logs(self):
        proc = mock.Mock()
        proc.poll.return_value = None
        args = SimpleNamespace(
            results="/tmp", fcvm="fcvm", state_dir="/state", cdp_port=9222,
            urls=["https://example.com/"], tag="tag",
        )
        cg = mock.Mock()
        cg.leaf.return_value = "/cgroup/leaf"
        side = corpus_mem.FcvmSide(args, cg, "a" * 32)
        environments = []

        def spawn(_cg_path, _argv, _log_path, env=None):
            environments.append(env)
            return proc

        with mock.patch.object(corpus_mem, "spawn_in_cgroup", side_effect=spawn), \
             mock.patch.object(corpus_mem, "find_clone_state", return_value=None):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "fcvm1r1", [0])
        self.assertEqual(environments[0]["RUST_LOG"], "fcvm=debug")

    def test_unreadable_state_directory_cannot_prove_a_clone_is_gone(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = os.path.join(tmp, "missing-state")
            with self.assertRaises(corpus_mem.CloneStateReadError):
                corpus_mem.clone_gone(missing, "clone-a")

    def test_malformed_state_cannot_prove_a_clone_is_gone(self):
        with tempfile.TemporaryDirectory() as state_dir:
            with open(os.path.join(state_dir, "unknown.json"), "w") as handle:
                handle.write("{not-json\n")
            with self.assertRaises(corpus_mem.CloneStateReadError):
                corpus_mem.clone_gone(state_dir, "clone-a")

    def test_state_proof_failure_does_not_skip_later_clone_teardown(self):
        args = SimpleNamespace(state_dir="/state")
        cg = mock.Mock()
        side = corpus_mem.FcvmSide(args, cg, "a" * 32)
        clones = []
        for index in range(2):
            proc = mock.Mock()
            clone = {"name": f"clone-{index}", "leaf": f"leaf-{index}",
                     "proc": proc}
            clones.append(clone)
            side.owned[clone["name"]] = clone
        with mock.patch.object(
                corpus_mem, "clone_gone",
                side_effect=[corpus_mem.CloneStateReadError("cannot read state"), True]):
            with self.assertRaises(corpus_mem.CloneStateReadError):
                side.tear_down(clones)
        self.assertEqual(cg.rm.call_count, 2,
                         "one proof error skipped teardown of a later clone")
        for clone in clones:
            clone["proc"].terminate.assert_called_once()
            clone["proc"].wait.assert_called_once()

    def test_one_cleanup_failure_does_not_skip_the_other_resources(self):
        calls = []

        class Side:
            def __init__(self, label, *methods):
                self.label = label
                for method in methods:
                    setattr(self, method, self.stop)

            def stop(self):
                calls.append(self.label)
                raise RuntimeError(self.label)

        class Cgroup:
            def rm_all(self):
                calls.append("cgroup")
                raise RuntimeError("cgroup")

        class Output:
            def close(self):
                calls.append("output")
                raise RuntimeError("output")

        with self.assertRaises(RuntimeError):
            corpus_mem.cleanup_harness_resources(
                Side("container", "stop_all"),
                Side("fcvm", "stop_all", "stop_serve"),
                Cgroup(), Output())
        self.assertEqual(calls,
                         ["container", "fcvm", "fcvm", "cgroup", "output"])


class HostCdpProducer(unittest.TestCase):
    """The standalone host control publishes one owned, attributable run."""

    CONTAINER_ID = "c" * 64
    PEER_ID = "d" * 64
    IMAGE_ID = "sha256:" + "a" * 64
    OTHER_IMAGE_ID = "sha256:" + "e" * 64

    def environment(self, tmp, mode="ok", **overrides):
        bindir = os.path.join(tmp, "bin")
        os.makedirs(bindir)
        state = os.path.join(tmp, "podman-state")
        removed = os.path.join(tmp, "removed")
        loadavg = os.path.join(tmp, "loadavg")
        with open(loadavg, "w") as handle:
            handle.write("0.1 0.2 0.3 1/1 1\n")

        podman = os.path.join(bindir, "podman")
        with open(podman, "w") as handle:
            handle.write(f'''#!/bin/bash
set -u
cmd="${{1:-}}"
[ "$#" -eq 0 ] || shift
case "$cmd" in
  image)
    [ "${{1:-}}" = inspect ] || exit 64
    shift
    target="${{@: -1}}"
    [ "$target" = "${{IMAGE:-localhost/chromium-bench-req}}" ] || exit 65
    sleep "${{PODMAN_PRE_CREATE_DELAY_SECS:-0}}"
    printf '%s\n' "${{PODMAN_TAG_IMAGE_ID-{self.IMAGE_ID}}}"
    ;;
  container)
    [ "${{1:-}}" = exists ] || exit 64
    : >"$PODMAN_EXISTS_CALLED_FILE"
    if [ "${{PODMAN_MODE:-ok}}" = guardian-ambiguous ] \
            && [ ! -e "$PODMAN_TEST_STATE.name" ] \
            && [ -e "$RESULTS/complete.json" ]; then
      : >"$PODMAN_POST_PUBLICATION_PROBE_FILE"
      exit 125
    fi
    [ -z "${{PODMAN_EXISTS_RC:-}}" ] || exit "$PODMAN_EXISTS_RC"
    [ -e "$PODMAN_TEST_STATE.name" ] && exit 0
    exit 1
    ;;
  create)
    launch_image="${{@: -1}}"
    name=
    owner=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift 2 ;;
        --label)
          case "$2" in
            io.fcvm.bench.owner=*) owner="${{2#*=}}" ;;
          esac
          shift 2
          ;;
        *) shift ;;
      esac
    done
    [ "${{PODMAN_MODE:-ok}}" != collision ] || exit 125
    if [ "${{PODMAN_MODE:-ok}}" = hung-create ]; then
      printf '%s\n' "$BASHPID" >"$PODMAN_CREATE_STARTED_FILE"
      trap '' TERM
      while :; do sleep 10; done
    fi
    if [ "${{PODMAN_MODE:-ok}}" = escaped-create ]; then
      setsid bash -c '
        trap "" TERM
        printf "%s\n" "$BASHPID" >"$PODMAN_ESCAPED_STARTED_FILE"
        while [ ! -e "$PODMAN_ESCAPED_RELEASE_FILE" ]; do sleep 0.01; done
      ' >/dev/null 2>&1 &
      while [ ! -e "$PODMAN_ESCAPED_STARTED_FILE" ]; do sleep 0.01; done
      exit 124
    fi
    if [ "${{PODMAN_MODE:-ok}}" = late-partial ]; then
      : >"$PODMAN_CREATE_STARTED_FILE"
      for fd in /proc/$$/fd/*; do
        target=$(readlink "$fd" 2>/dev/null || true)
        case "$target" in
          "$CONTAINER_CREATE_OPS_DIR"/*.lock)
            : >"$PODMAN_CREATE_LOCK_HELD_FILE"
            break
            ;;
        esac
      done
      while [ ! -e "$PODMAN_CREATE_RELEASE_FILE" ]; do sleep 0.01; done
    fi
    if [ "${{PODMAN_MODE:-ok}}" = closed-fd-late ]; then
      setsid bash -c '
        name=$1
        owner=$2
        launch_image=$3
        (
          trap "" TERM
          for descriptor_path in /proc/$BASHPID/fd/*; do
            descriptor=${{descriptor_path##*/}}
            case "$descriptor" in 0|1|2) ;; *) eval "exec $descriptor>&-" ;; esac
          done
          printf "%s\n" "$BASHPID" >"$PODMAN_ESCAPED_STARTED_FILE"
          while [ ! -e "$PODMAN_ESCAPED_RELEASE_FILE" ]; do sleep 0.01; done
          printf "%s\n" "$name" >"$PODMAN_TEST_STATE.name"
          printf "%s\n" "$owner" >"$PODMAN_TEST_STATE.owner"
          printf "%s\n" "$launch_image" >"$PODMAN_TEST_STATE.launch-image"
          printf "%s\n" "$PODMAN_CONTAINER_IMAGE_ID" >"$PODMAN_TEST_STATE.image"
          : >"$PODMAN_LATE_COMMIT_FILE"
          while [ ! -e "$PODMAN_LATE_ACK_FILE" ]; do sleep 0.01; done
        ) &
        exit 0
      ' bash "$name" "$owner" "$launch_image" >/dev/null 2>&1 &
      while [ ! -e "$PODMAN_ESCAPED_STARTED_FILE" ]; do sleep 0.01; done
      exit 124
    fi
    printf '%s\n' "$name" >"$PODMAN_TEST_STATE.name"
    printf '%s\n' "${{PODMAN_CONTAINER_OWNER_TOKEN-$owner}}" >"$PODMAN_TEST_STATE.owner"
    printf '%s\n' "$launch_image" >"$PODMAN_TEST_STATE.launch-image"
    printf '%s\n' "${{PODMAN_CONTAINER_IMAGE_ID-{self.IMAGE_ID}}}" >"$PODMAN_TEST_STATE.image"
    if [ "${{PODMAN_MODE:-ok}}" = committed-wait ]; then
      : >"$PODMAN_CREATE_COMMITTED_FILE"
      while [ ! -e "$PODMAN_CREATE_RETURN_FILE" ]; do sleep 0.01; done
    fi
    [ "${{PODMAN_MODE:-ok}}" != partial ] || exit 124
    [ "${{PODMAN_MODE:-ok}}" != late-partial ] || exit 124
    [ "${{PODMAN_MODE:-ok}}" != inspect-error ] || exit 124
    printf '%s\n' '{self.CONTAINER_ID}'
    ;;
  start) exit 0 ;;
  inspect)
    format=
    target=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --format) format="$2"; shift 2 ;;
        --type) shift 2 ;;
        *) target="$1"; shift ;;
      esac
    done
    if [ "${{PODMAN_MODE:-ok}}" = inspect-once-error ] \
        && [[ "$format" = *'Config.Labels'* ]] \
        && [ ! -e "$PODMAN_INSPECT_ONCE_MARKER" ]; then
      : >"$PODMAN_INSPECT_ONCE_MARKER"
      exit 70
    fi
    [ "${{PODMAN_MODE:-ok}}" != inspect-error ] || exit 70
    if [ "${{PODMAN_MODE:-ok}}" = collision ]; then
      printf '%s|%s\n' '{self.PEER_ID}' 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
      exit 0
    fi
    if [ ! -e "$PODMAN_TEST_STATE.name" ]; then
      : >"$PODMAN_INSPECT_CALLED_FILE"
      [ "${{PODMAN_MODE:-ok}}" != closed-fd-late ] \
        || : >"$PODMAN_PRIOR_ABSENCE_FILE"
      if [ "${{PODMAN_MODE:-ok}}" = late-partial ]; then
        exec 8>"$PODMAN_INSPECT_COUNT.lock"
        flock -x 8
        count=0
        [ ! -e "$PODMAN_INSPECT_COUNT" ] || read -r count <"$PODMAN_INSPECT_COUNT"
        count=$((count + 1))
        printf '%s\n' "$count" >"$PODMAN_INSPECT_COUNT"
        [ "$count" -lt 10 ] || : >"$PODMAN_PRIOR_ABSENCE_FILE"
      fi
      exit 1
    fi
    case "$format" in
      *'.Image'*) cat "$PODMAN_TEST_STATE.image" ;;
      *'Config.Labels'*)
        printf '%s|%s\n' '{self.CONTAINER_ID}' "$(cat "$PODMAN_TEST_STATE.owner")"
        ;;
      *) exit 64 ;;
    esac
    ;;
  exec) exit 0 ;;
  rm)
    target="${{@: -1}}"
    [ "${{PODMAN_RM_FAIL:-0}}" != 1 ] || exit 73
    printf '%s\n' "$target" >>"$PODMAN_REMOVED"
    [ "${{PODMAN_RM_LEAVES_CONTAINER:-0}}" = 1 ] \
      || rm -f -- "$PODMAN_TEST_STATE.name"
    exit 0
    ;;
  logs) exit 0 ;;
  *) exit 64 ;;
esac
''')
        os.chmod(podman, 0o755)

        pgrep = os.path.join(bindir, "pgrep")
        with open(pgrep, "w") as handle:
            handle.write("#!/bin/sh\nexit 1\n")
        os.chmod(pgrep, 0o755)

        python = os.path.join(bindir, "python3")
        with open(python, "w") as handle:
            handle.write(f'''#!/bin/bash
if [ "${{1:-}}" = "$HOSTCDP_DRIVER" ]; then
  [ -z "${{DRIVER_STARTED_FILE:-}}" ] || : >"$DRIVER_STARTED_FILE"
  if [ -n "${{DRIVER_WAIT_FILE:-}}" ]; then
    while [ ! -e "$DRIVER_WAIT_FILE" ]; do /bin/sleep 0.01; done
  fi
  case "${{DRIVER_LOAD_ACTION:-}}" in
    nonnumeric) printf '%s\n' 'not-a-load' >"$LOADAVG_FILE" ;;
    missing) rm -f -- "$LOADAVG_FILE" ;;
  esac
  case "${{DRIVER_RUNTIME_ACTION:-}}" in
    tamper) printf '%s\n' mutated >"$RUNTIME_PAYLOAD" ;;
  esac
  case "${{DRIVER_LOAD_ACTION:-}}" in
    numeric-read-error) : >"$LOAD_READ_FAILED_MARKER" ;;
  esac
  if [ "${{DRIVER_LEAVE_DESCENDANT:-0}}" = 1 ]; then
    setsid /bin/sleep 30 </dev/null >/dev/null 2>&1 &
  fi
  printf '%s\n' '{{"ok":true,"url":"https://example.com/","stages":{{"total_ms":1.0}},"nav":{{"load_ms":1.0}}}}'
  exit 0
fi
exec {sys.executable!r} "$@"
''')
        os.chmod(python, 0o755)

        real_cut = shutil.which("cut")
        cut = os.path.join(bindir, "cut")
        with open(cut, "w") as handle:
            handle.write(f'''#!/bin/bash
if [ "${{@: -1}}" = "$LOADAVG_FILE" ] \
    && {{ [ "${{LOAD_READ_FAIL_FROM_START:-0}}" = 1 ] \
         || {{ [ -n "${{LOAD_READ_FAILED_MARKER:-}}" ] \
              && [ -e "$LOAD_READ_FAILED_MARKER" ]; }}; }}; then
  printf '%s\n' '0.42'
  exit 9
fi
exec {real_cut!r} "$@"
''')
        os.chmod(cut, 0o755)

        real_date = shutil.which("date")
        wall_clock_state = os.path.join(tmp, "wall-clock-state")
        date = os.path.join(bindir, "date")
        with open(date, "w") as handle:
            handle.write(f'''#!/bin/bash
if [ "${{1:-}}" = '+%s.%N' ] && [ -n "${{WALL_CLOCK_STEPS:-}}" ]; then
  index=0
  [ ! -e {wall_clock_state!r} ] || read -r index < {wall_clock_state!r}
  IFS=, read -r -a values <<<"$WALL_CLOCK_STEPS"
  [ "$index" -lt "${{#values[@]}}" ] || exit 67
  printf '%s\n' "${{values[$index]}}"
  printf '%s\n' "$((index + 1))" > {wall_clock_state!r}
  exit 0
fi
exec {real_date!r} "$@"
''')
        os.chmod(date, 0o755)

        runtime = os.path.join(tmp, "runtime")
        os.mkdir(runtime)
        payload = os.path.join(runtime, "payload")
        with open(payload, "w") as handle:
            handle.write("sealed\n")
        payload_digest = hashlib.sha256(b"sealed\n").hexdigest()
        manifest = os.path.join(runtime, "REQBENCH_MANIFEST.sha256")
        with open(manifest, "w") as handle:
            handle.write(f"{payload_digest}  payload\n")
        with open(manifest, "rb") as handle:
            runtime_digest = hashlib.sha256(handle.read()).hexdigest()
        outer_manifest = os.path.join(runtime, "MANIFEST.sha256")
        with open(manifest, "rb") as source, open(outer_manifest, "wb") as target:
            target.write(source.read())
        revision = subprocess.check_output(
            ["git", "-C", os.path.dirname(os.path.dirname(HERE)),
             "rev-parse", "HEAD"], text=True).strip()
        create_ops = os.path.join(tmp, "container-create-ops")
        os.mkdir(create_ops)
        create_started = os.path.join(tmp, "create-started")
        create_lock_held = os.path.join(tmp, "create-lock-held")
        create_release = os.path.join(tmp, "create-release")
        inspect_count = os.path.join(tmp, "inspect-count")
        prior_absence = os.path.join(tmp, "prior-absence")
        inspect_called = os.path.join(tmp, "inspect-called")
        inspect_once = os.path.join(tmp, "inspect-once")
        exists_called = os.path.join(tmp, "exists-called")
        escaped_started = os.path.join(tmp, "escaped-started")
        escaped_release = os.path.join(tmp, "escaped-release")
        late_commit = os.path.join(tmp, "late-commit")
        late_ack = os.path.join(tmp, "late-ack")
        create_committed = os.path.join(tmp, "create-committed")
        create_return = os.path.join(tmp, "create-return")

        env = dict(
            os.environ,
            PATH=bindir + os.pathsep + os.environ["PATH"],
            PODMAN_MODE=mode,
            PODMAN_TEST_STATE=state,
            PODMAN_REMOVED=removed,
            HOSTCDP_DRIVER=os.path.join(HERE, "cdpdrive.py"),
            LOADAVG_FILE=loadavg,
            RESULTS=os.path.join(tmp, "results"),
            RUNID="1" * 32,
            REPS="1",
            WARMUP="0",
            ALLOW_BUSY="1",
            SETTLE_WAIT_SECS="0",
            URL="https://example.com/",
            IMAGE="localhost/chromium-bench-req",
            IMAGE_ID=self.IMAGE_ID,
            PODMAN_TAG_IMAGE_ID=self.IMAGE_ID,
            PODMAN_CONTAINER_IMAGE_ID=self.IMAGE_ID,
            PODMAN_CREATE_TIMEOUT_SECS="120",
            CONTAINER_CREATE_OPS_DIR=create_ops,
            PODMAN_CREATE_STARTED_FILE=create_started,
            PODMAN_CREATE_LOCK_HELD_FILE=create_lock_held,
            PODMAN_CREATE_RELEASE_FILE=create_release,
            PODMAN_INSPECT_COUNT=inspect_count,
            PODMAN_PRIOR_ABSENCE_FILE=prior_absence,
            PODMAN_INSPECT_CALLED_FILE=inspect_called,
            PODMAN_INSPECT_ONCE_MARKER=inspect_once,
            PODMAN_EXISTS_CALLED_FILE=exists_called,
            PODMAN_POST_PUBLICATION_PROBE_FILE=os.path.join(
                tmp, "post-publication-probe"),
            PODMAN_ESCAPED_STARTED_FILE=escaped_started,
            PODMAN_ESCAPED_RELEASE_FILE=escaped_release,
            PODMAN_LATE_COMMIT_FILE=late_commit,
            PODMAN_LATE_ACK_FILE=late_ack,
            PODMAN_CREATE_COMMITTED_FILE=create_committed,
            PODMAN_CREATE_RETURN_FILE=create_return,
            COMPARISON_LABEL="free",
            CPU_BUDGET="unlimited",
            SOURCE_REVISION=revision,
            REQBENCH_RUNTIME_MANIFEST=manifest,
            REQBENCH_RUNTIME_BUNDLE_SHA256=runtime_digest,
            CORPUS_EXTRA_RUNTIME_MANIFEST=outer_manifest,
            CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256=runtime_digest,
            RUNTIME_PAYLOAD=payload,
            LOAD_READ_FAILED_MARKER=os.path.join(tmp, "load-read-failed"),
            WALL_CLOCK_STATE=wall_clock_state,
        )
        env.pop("CPUS", None)
        env.pop("CONTAINER_OWNER_TOKEN", None)
        env.update(overrides)
        return env, removed, state

    @staticmethod
    def run_host(env):
        return subprocess.run(
            ["bash", HOSTCDP], env=env, capture_output=True, text=True,
            timeout=30,
        )

    @staticmethod
    def removed_ids(path):
        if not os.path.exists(path):
            return []
        with open(path) as handle:
            return handle.read().splitlines()

    def test_a_name_collision_never_deletes_the_peer(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(tmp, mode="collision")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(
                self.removed_ids(removed), [],
                "a failed podman create made the pre-existing same-name container owned",
            )

    def test_successful_create_output_is_not_owned_until_the_token_matches(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, PODMAN_CONTAINER_OWNER_TOKEN="b" * 32)
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(
                self.removed_ids(removed), [],
                "an exact ID without the private owner token was removed",
            )
            self.assertTrue(os.path.exists(state + ".name"))

    def test_cleanup_retries_identity_after_post_create_inspect_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(tmp, mode="inspect-once-error")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("could not be inspected", proc.stderr)
            self.assertEqual(
                self.removed_ids(removed), [self.CONTAINER_ID],
                "cleanup never retried the exact ID and owner-token proof",
            )

    def test_partial_create_timeout_adopts_only_its_labeled_exact_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(tmp, mode="partial")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])
            with open(state + ".owner") as handle:
                token = handle.read().strip()
            self.assertRegex(token, r"^[0-9a-f]{32}$")

    def test_term_after_create_commit_reconciles_the_owned_container(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(tmp, mode="committed-wait")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(env["PODMAN_CREATE_COMMITTED_FILE"]):
                    if producer.poll() is not None:
                        stdout, stderr = producer.communicate()
                        self.fail(
                            f"hostcdp exited before create committed: "
                            f"{producer.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail("fake podman create did not commit")
                    time.sleep(0.01)

                producer.send_signal(signal.SIGTERM)
                with open(env["PODMAN_CREATE_RETURN_FILE"], "w"):
                    pass
                stdout, stderr = producer.communicate(timeout=10)

                self.assertEqual(producer.returncode, 143, stdout + stderr)
                self.assertEqual(
                    self.removed_ids(removed), [self.CONTAINER_ID],
                    "TERM after create committed leaked the owned container",
                )
                self.assertFalse(os.path.exists(state + ".name"))
            finally:
                with open(env["PODMAN_CREATE_RETURN_FILE"], "w"):
                    pass
                if producer.poll() is None:
                    producer.kill()
                    producer.communicate(timeout=5)

    def test_sigkill_after_start_reaps_the_standalone_container(self):
        """The lifecycle guardian outlives the standalone producer.

        RED BEFORE THE FIX: SIGKILL bypassed the shell EXIT trap after the
        detached container started, leaving its state present and recording no
        removal.
        """
        with tempfile.TemporaryDirectory() as tmp:
            started = os.path.join(tmp, "driver-started")
            release = os.path.join(tmp, "driver-release")
            env, removed, state = self.environment(
                tmp, DRIVER_STARTED_FILE=started, DRIVER_WAIT_FILE=release,
            )
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(started):
                    if producer.poll() is not None:
                        stdout, stderr = producer.communicate()
                        self.fail(
                            "hostcdp exited before the held rep: "
                            + stdout + stderr)
                    if time.monotonic() >= deadline:
                        self.fail("hostcdp never started its container driver")
                    time.sleep(0.01)

                os.kill(producer.pid, signal.SIGKILL)
                producer.wait(timeout=5)
                deadline = time.monotonic() + 10
                while (os.path.exists(state + ".name")
                       and time.monotonic() < deadline):
                    time.sleep(0.01)

                self.assertFalse(
                    os.path.exists(state + ".name"),
                    "SIGKILL left the standalone host container alive",
                )
                self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])
            finally:
                with open(release, "w"):
                    pass
                if producer.poll() is None:
                    producer.kill()
                    producer.wait(timeout=5)
                for pipe in (producer.stdout, producer.stderr):
                    if pipe is not None:
                        pipe.close()

    def test_sigkill_during_create_commit_reaps_the_owned_container(self):
        """Final cleanup waits for a committed create to become quiescent.

        RED BEFORE THE FIX: the create supervisor drained its CLI after parent
        death, but nothing surviving the producer inspected and removed the
        container that had already committed.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(tmp, mode="committed-wait")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(env["PODMAN_CREATE_COMMITTED_FILE"]):
                    if producer.poll() is not None:
                        stdout, stderr = producer.communicate()
                        self.fail(
                            "hostcdp exited before create committed: "
                            + stdout + stderr)
                    if time.monotonic() >= deadline:
                        self.fail("fake podman create did not commit")
                    time.sleep(0.01)

                os.kill(producer.pid, signal.SIGKILL)
                with open(env["PODMAN_CREATE_RETURN_FILE"], "w"):
                    pass
                producer.wait(timeout=5)
                deadline = time.monotonic() + 10
                while (os.path.exists(state + ".name")
                       and time.monotonic() < deadline):
                    time.sleep(0.01)

                self.assertFalse(
                    os.path.exists(state + ".name"),
                    "SIGKILL between create commit and ID capture leaked the container",
                )
                self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])
            finally:
                with open(env["PODMAN_CREATE_RETURN_FILE"], "w"):
                    pass
                if producer.poll() is None:
                    producer.kill()
                    producer.wait(timeout=5)
                for pipe in (producer.stdout, producer.stderr):
                    if pipe is not None:
                        pipe.close()

    def test_failed_inspect_cannot_claim_absence_when_container_exists(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(
                tmp, mode="inspect-error", PODMAN_EXISTS_RC="0")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertTrue(
                os.path.exists(env["PODMAN_EXISTS_CALLED_FILE"]),
                "an inspect error was treated as container absence",
            )
            self.assertIn("exists but its identity could not be inspected", proc.stderr)
            self.assertEqual(self.removed_ids(removed), [])

    def test_late_create_completion_is_reaped_without_touching_a_peer(self):
        """A timed-out create client can return before its container commits."""
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, mode="late-partial", PODMAN_CREATE_TIMEOUT_SECS="1")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            deadline = time.monotonic() + 10
            release_reason = None
            while time.monotonic() < deadline:
                if os.path.exists(env["PODMAN_CREATE_LOCK_HELD_FILE"]):
                    self.assertFalse(
                        os.path.exists(env["PODMAN_PRIOR_ABSENCE_FILE"]),
                        "cleanup probed absence while the create operation was live",
                    )
                    release_reason = "locked-create"
                    break
                if os.path.exists(env["PODMAN_PRIOR_ABSENCE_FILE"]):
                    release_reason = "prior-absence"
                    break
                if producer.poll() is not None:
                    break
                time.sleep(0.01)
            self.assertIsNotNone(release_reason, "create failpoint was never reached")
            with open(env["PODMAN_CREATE_RELEASE_FILE"], "w"):
                pass
            stdout, stderr = producer.communicate(timeout=10)
            self.assertEqual(release_reason, "locked-create", stdout + stderr)
            self.assertTrue(os.path.exists(state + ".launch-image"), stdout + stderr)
            self.assertNotEqual(producer.returncode, 0, stdout + stderr)
            self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])
            self.assertNotIn(self.PEER_ID, self.removed_ids(removed))

    def test_closed_fd_double_fork_cannot_commit_after_absence(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, mode="closed-fd-late", PODMAN_CREATE_TIMEOUT_SECS="1",
                PODMAN_CREATE_KILL_AFTER_SECS="1",
                PODMAN_CREATE_QUIESCE_TIMEOUT_SECS="1")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            escaped_pid = None
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    if (os.path.exists(env["PODMAN_PRIOR_ABSENCE_FILE"])
                            and os.path.exists(env["PODMAN_ESCAPED_STARTED_FILE"])):
                        with open(env["PODMAN_ESCAPED_STARTED_FILE"]) as handle:
                            escaped_pid = int(handle.read().strip())
                        break
                    if producer.poll() is not None:
                        break
                    time.sleep(0.01)
                self.assertIsNotNone(
                    escaped_pid, "closed-FD create descendant never reached reconciliation")
                with open(env["PODMAN_ESCAPED_RELEASE_FILE"], "w"):
                    pass
                stdout, stderr = producer.communicate(timeout=10)
                self.assertNotEqual(producer.returncode, 0, stdout + stderr)

                outcome = None
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    if os.path.exists(env["PODMAN_LATE_COMMIT_FILE"]):
                        outcome = "late-commit"
                        break
                    if not os.path.exists(f"/proc/{escaped_pid}"):
                        outcome = "reaped"
                        break
                    time.sleep(0.01)
                self.assertEqual(outcome, "reaped", stdout + stderr)
                self.assertFalse(os.path.exists(state + ".name"),
                                 "container committed after absence reconciliation")
                self.assertEqual(self.removed_ids(removed), [])
            finally:
                with open(env["PODMAN_ESCAPED_RELEASE_FILE"], "w"):
                    pass
                with open(env["PODMAN_LATE_ACK_FILE"], "w"):
                    pass
                if producer.poll() is None:
                    producer.terminate()
                    producer.wait(timeout=5)
                if escaped_pid is not None:
                    deadline = time.monotonic() + 5
                    while (os.path.exists(f"/proc/{escaped_pid}")
                           and time.monotonic() < deadline):
                        time.sleep(0.01)

    def test_hung_create_is_terminated_and_reaped_within_the_bound(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, mode="hung-create", PODMAN_CREATE_TIMEOUT_SECS="1",
                PODMAN_CREATE_KILL_AFTER_SECS="1",
                PODMAN_CREATE_QUIESCE_TIMEOUT_SECS="1",
                PODMAN_PRE_CREATE_DELAY_SECS="5")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, start_new_session=True,
            )
            deadline = time.monotonic() + 10
            while not os.path.exists(env["PODMAN_CREATE_STARTED_FILE"]):
                if producer.poll() is not None:
                    stdout, stderr = producer.communicate()
                    self.fail("hostcdp exited before create started: " + stdout + stderr)
                if time.monotonic() >= deadline:
                    stdout, stderr = kill_and_reap_test_process_group(producer)
                    self.fail("podman create did not start: " + stdout + stderr)
                time.sleep(0.01)
            started = time.monotonic()
            stdout, stderr = communicate_test_process_group(producer, 10)
            elapsed = time.monotonic() - started
            self.assertNotEqual(producer.returncode, 0, stdout + stderr)
            self.assertLess(elapsed, 5, f"create deadline took {elapsed:.3f}s")
            self.assertFalse(os.path.exists(state + ".name"))
            self.assertEqual(self.removed_ids(removed), [])
            locks = os.listdir(env["CONTAINER_CREATE_OPS_DIR"])
            self.assertEqual(len(locks), 1, locks)
            lock = os.path.join(env["CONTAINER_CREATE_OPS_DIR"], locks[0])
            self.assertEqual(
                subprocess.run(["flock", "-n", lock, "true"]).returncode, 0,
                "the killed create operation still holds its shared lock",
            )

    def test_hung_create_timeout_cleanup_reaps_the_process_tree(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, mode="hung-create", PODMAN_CREATE_TIMEOUT_SECS="30",
                PODMAN_CREATE_KILL_AFTER_SECS="1",
                PODMAN_CREATE_QUIESCE_TIMEOUT_SECS="1")
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, start_new_session=True,
            )
            create_pid = None
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(env["PODMAN_CREATE_STARTED_FILE"]):
                    if producer.poll() is not None:
                        stdout, stderr = producer.communicate()
                        self.fail(
                            "hostcdp exited before create started: " + stdout + stderr)
                    if time.monotonic() >= deadline:
                        self.fail("podman create did not start")
                    time.sleep(0.01)
                with open(env["PODMAN_CREATE_STARTED_FILE"]) as handle:
                    create_pid = int(handle.read().strip())

                with self.assertRaises(subprocess.TimeoutExpired):
                    communicate_test_process_group(producer, 0.05)
                self.assertIsNotNone(
                    producer.poll(), "timed-out test left hostcdp running")
                deadline = time.monotonic() + 5
                while proc_state(create_pid) not in (None, "Z"):
                    if time.monotonic() >= deadline:
                        self.fail("timed-out test left podman create running")
                    time.sleep(0.01)
            finally:
                if producer.poll() is None:
                    os.killpg(producer.pid, signal.SIGKILL)
                    producer.communicate(timeout=5)
                if create_pid is not None and proc_state(create_pid) not in (None, "Z"):
                    try:
                        os.kill(create_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    deadline = time.monotonic() + 5
                    while proc_state(create_pid) not in (None, "Z"):
                        if time.monotonic() >= deadline:
                            self.fail("emergency cleanup could not reap podman create")
                        time.sleep(0.01)

    def test_hung_create_start_deadline_uses_process_group_cleanup(self):
        """The pre-create timeout must not leave a pipe-holding child alive."""
        with open(__file__) as handle:
            source = handle.read()
        start = source.index(
            "    def test_hung_create_is_terminated_and_reaped_within_the_bound"
        )
        end = source.index("\n    def test_", start + 1)
        method = source[start:end]
        self.assertNotIn(
            "producer.kill()",
            method,
            "the create-start deadline kills only the shell leader",
        )
        self.assertIn(
            "kill_and_reap_test_process_group(producer)",
            method,
            "the create-start deadline does not reap the producer's process group",
        )

        with tempfile.TemporaryDirectory() as tmp:
            child_file = os.path.join(tmp, "child-pid")
            producer = subprocess.Popen(
                [
                    "bash", "-c",
                    "trap '' TERM\n"
                    "sleep 300 &\n"
                    "child=$!\n"
                    "printf '%s\\n' \"$child\" > \"$1\"\n"
                    "wait \"$child\"\n",
                    "bash", child_file,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            child_pid = None
            try:
                deadline = time.monotonic() + 5
                while not os.path.exists(child_file):
                    if producer.poll() is not None:
                        stdout, stderr = producer.communicate()
                        self.fail(
                            "cleanup fixture exited before starting its child: "
                            + stdout + stderr
                        )
                    if time.monotonic() >= deadline:
                        self.fail("cleanup fixture did not record its child")
                    time.sleep(0.01)
                with open(child_file) as handle:
                    child_pid = int(handle.read().strip())

                kill_and_reap_test_process_group(producer)
                self.assertIsNotNone(
                    producer.poll(), "group cleanup left its session leader alive"
                )
                deadline = time.monotonic() + 5
                while proc_state(child_pid) not in (None, "Z"):
                    if time.monotonic() >= deadline:
                        self.fail("group cleanup left its pipe-holding child alive")
                    time.sleep(0.01)
            finally:
                try:
                    os.killpg(producer.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if producer.poll() is None:
                    producer.communicate(timeout=5)
                if child_pid is not None and proc_state(child_pid) not in (None, "Z"):
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_escaped_lock_holder_is_reaped_before_absence_reconciliation(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, mode="escaped-create",
                PODMAN_CREATE_KILL_AFTER_SECS="1",
                PODMAN_CREATE_QUIESCE_TIMEOUT_SECS="1")
            with open(os.path.join(tmp, "bin", "podman")) as handle:
                podman = handle.read()
            fixture_start = podman.index(
                'if [ "${PODMAN_MODE:-ok}" = escaped-create ]; then')
            fixture_end = podman.index(
                'if [ "${PODMAN_MODE:-ok}" = late-partial ]; then',
                fixture_start)
            fixture = podman[fixture_start:fixture_end]
            descendant_started = (
                'while [ ! -e "$PODMAN_ESCAPED_STARTED_FILE" ]; '
                'do sleep 0.01; done'
            )
            self.assertIn(
                descendant_started, fixture,
                "the escaped-create parent can exit before its child is observable",
            )
            self.assertLess(
                fixture.index("' >/dev/null 2>&1 &"),
                fixture.index(descendant_started),
                "the escaped-create parent waits before launching its child",
            )
            self.assertLess(
                fixture.index(descendant_started), fixture.index("exit 124"),
                "the escaped-create synchronization follows the parent exit",
            )
            try:
                started = time.monotonic()
                proc = self.run_host(env)
                elapsed = time.monotonic() - started
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertLess(elapsed, 5, f"escaped create drain took {elapsed:.3f}s")
                self.assertIn("live descendants", proc.stderr)
                self.assertTrue(os.path.exists(env["PODMAN_ESCAPED_STARTED_FILE"]))
                with open(env["PODMAN_ESCAPED_STARTED_FILE"]) as handle:
                    escaped_pid = int(handle.read().strip())
                self.assertFalse(os.path.exists(f"/proc/{escaped_pid}"),
                                 "create descendant survived reconciliation")
                self.assertTrue(os.path.exists(env["PODMAN_INSPECT_CALLED_FILE"]))
                self.assertFalse(os.path.exists(state + ".name"))
                self.assertEqual(self.removed_ids(removed), [])
            finally:
                with open(env["PODMAN_ESCAPED_RELEASE_FILE"], "w"):
                    pass
                locks = os.listdir(env["CONTAINER_CREATE_OPS_DIR"])
                if locks:
                    lock = os.path.join(env["CONTAINER_CREATE_OPS_DIR"], locks[0])
                    subprocess.run(["flock", "-w", "5", lock, "true"], check=True)

    def test_create_supervisor_has_term_kill_and_bounded_quiescence(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        create = source.index("podman create --name")
        prefix = source[:create]
        self.assertIn('python3 "$HERE/phase_supervisor.py"', prefix)
        self.assertIn('--timeout "$PODMAN_CREATE_TIMEOUT_SECS"', prefix)
        self.assertIn('--term-grace "$PODMAN_CREATE_KILL_AFTER_SECS"', prefix)
        self.assertIn('--kill-grace "$PODMAN_CREATE_QUIESCE_TIMEOUT_SECS"', prefix)
        self.assertIn('--pass-fd "$CREATE_OP_LOCK_FD"', prefix)
        quiesce_match = re.search(
            r'^quiesce_create_operation\(\) \{\n.*?^\}',
            source, re.MULTILINE | re.DOTALL,
        )
        self.assertIsNotNone(quiesce_match)
        quiesce_function = quiesce_match.group(0)
        self.assertIn(
            'flock -x -w "$PODMAN_CREATE_QUIESCE_TIMEOUT_SECS"',
            quiesce_function,
        )
        quiesce = source.index("if quiesce_create_operation", create)
        outcome = source.index('if [ "$podman_create_rc" -eq 0 ]', create)
        self.assertLess(quiesce, outcome)
        ownership = source.index('inspect_owned_container "$CONTAINER_ID"', outcome)
        start = source.index('podman start -- "$CONTAINER_ID"', ownership)
        self.assertLess(ownership, start)

    def test_logical_image_is_recorded_but_the_exact_id_is_executed(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, state = self.environment(tmp)
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(state + ".launch-image") as handle:
                self.assertEqual(handle.read().strip(), self.IMAGE_ID)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            self.assertEqual(run["image"], env["IMAGE"])
            self.assertEqual(run["image_id"], self.IMAGE_ID)

    def test_image_identity_accepts_bare_and_prefixed_sha256_shapes(self):
        shapes = (
            (self.IMAGE_ID.removeprefix("sha256:"), self.IMAGE_ID,
             self.IMAGE_ID.removeprefix("sha256:")),
            (self.IMAGE_ID, self.IMAGE_ID.removeprefix("sha256:"),
             self.IMAGE_ID),
        )
        for tag_shape, container_shape, supplied_shape in shapes:
            with self.subTest(tag=tag_shape[:8], container=container_shape[:8],
                              supplied=supplied_shape[:8]), \
                    tempfile.TemporaryDirectory() as tmp:
                env, _removed, state = self.environment(
                    tmp, PODMAN_TAG_IMAGE_ID=tag_shape,
                    PODMAN_CONTAINER_IMAGE_ID=container_shape,
                    IMAGE_ID=supplied_shape)
                proc = self.run_host(env)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                with open(state + ".launch-image") as handle:
                    self.assertEqual(handle.read().strip(), self.IMAGE_ID)
                with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                    self.assertEqual(json.load(handle)["image_id"], self.IMAGE_ID)

    def test_image_identity_rejects_malformed_and_multiple_output(self):
        malformed = (
            "",
            "a" * 63,
            "A" * 64,
            self.IMAGE_ID + "\n" + self.OTHER_IMAGE_ID,
        )
        for identity in malformed:
            with self.subTest(identity=repr(identity)), \
                    tempfile.TemporaryDirectory() as tmp:
                env, removed, state = self.environment(
                    tmp, PODMAN_TAG_IMAGE_ID=identity)
                proc = self.run_host(env)
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertFalse(os.path.exists(state + ".name"))
                self.assertEqual(self.removed_ids(removed), [])

        for identity in malformed:
            with self.subTest(container_identity=repr(identity)), \
                    tempfile.TemporaryDirectory() as tmp:
                env, removed, _state = self.environment(
                    tmp, PODMAN_CONTAINER_IMAGE_ID=identity)
                proc = self.run_host(env)
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])

    def test_tag_must_resolve_to_the_supplied_exact_image_before_create(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, PODMAN_TAG_IMAGE_ID=self.OTHER_IMAGE_ID)
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("image", proc.stderr.lower())
            self.assertFalse(os.path.exists(state + ".name"),
                             "container creation followed a failed image preflight")
            self.assertEqual(self.removed_ids(removed), [])

    def test_created_container_must_use_the_preflighted_exact_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(
                tmp, PODMAN_CONTAINER_IMAGE_ID=self.OTHER_IMAGE_ID)
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("image", proc.stderr.lower())
            self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])

    def test_wall_clock_steps_do_not_change_rep_elapsed_time(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, REPS="2", WALL_CLOCK_STEPS="1000,999,1000,1000000")
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
            self.assertEqual(len(rows), 2)
            for row in rows:
                self.assertGreaterEqual(row["wall_ms"], 0, row)
                self.assertLess(row["wall_ms"], 5000, row)
            self.assertFalse(os.path.exists(env["WALL_CLOCK_STATE"]),
                             "elapsed timing still reads CLOCK_REALTIME through date")

    def test_numeric_output_from_a_failed_load_read_is_invalid(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LOAD_ACTION="numeric-read-error")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("status=9", proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                row = json.loads(handle.readline())
            self.assertEqual(row["loadavg1_raw"], "0.42")
            self.assertEqual(row["loadavg1_read_status"], 9)
            self.assertIsNone(row["loadavg1"])
            self.assertIs(row["measurement_valid"], False)

    def test_failed_initial_load_read_refuses_before_container_create(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(
                tmp, LOAD_READ_FAIL_FROM_START="1")
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
            self.assertIn("status=9", proc.stderr)
            self.assertFalse(os.path.exists(state + ".name"))
            self.assertEqual(self.removed_ids(removed), [])

    def test_complete_manifest_binds_the_exact_raw_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(tmp)
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "complete.json")) as handle:
                complete = json.load(handle)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            self.assertEqual(complete["schema_version"], 1)
            self.assertEqual(complete["run_id"], run["run_id"])
            self.assertEqual(set(complete["artifacts"]), {"run.json", "hostcdp.jsonl"})
            for name in complete["artifacts"]:
                path = os.path.join(env["RESULTS"], name)
                with open(path, "rb") as source:
                    raw = source.read()
                self.assertEqual(
                    complete["artifacts"][name],
                    {"size": len(raw), "sha256": hashlib.sha256(raw).hexdigest()},
                )

    def test_post_rename_publication_failure_withdraws_completion(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, HOSTCDP_COMPLETE_FAIL_AFTER_RENAME="1")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertTrue(os.path.exists(os.path.join(env["RESULTS"], "WITHDRAWN")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "complete.json")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_success_retires_cleanup_before_publication(self):
        """Published success has no fallible container proof after the worker."""
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, mode="guardian-ambiguous")
            proc = self.run_host(env)
            complete = os.path.join(env["RESULTS"], "complete.json")

            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertTrue(os.path.exists(complete))
            self.assertFalse(
                os.path.exists(env["PODMAN_POST_PUBLICATION_PROBE_FILE"]),
                "guardian performed a fallible Podman proof after publication",
            )

    def test_outer_descendant_failure_cannot_leave_completion(self):
        """The bootstrap authorizes only after the outer tree is drained."""
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LEAVE_DESCENDANT="1")
            proc = self.run_host(env)

            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("phase leader exited with live descendants", proc.stderr)
            self.assertTrue(
                os.path.exists(os.path.join(env["RESULTS"], "WITHDRAWN")))
            self.assertFalse(
                os.path.exists(os.path.join(env["RESULTS"], "complete.json")),
                "worker authorized the run before its outer lifecycle gate passed",
            )
            self.assertFalse(
                os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            self.assertFalse(
                os.path.exists(os.path.join(env["RESULTS"], ".summary.pending")))

    def test_cleanup_failure_withdraws_every_authorizing_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(tmp, PODMAN_RM_FAIL="1")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("could not remove owned container", proc.stderr)
            self.assertEqual(self.removed_ids(removed), [])
            self.assertTrue(os.path.exists(os.path.join(env["RESULTS"], "WITHDRAWN")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "complete.json")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_successful_rm_must_be_followed_by_exact_absence_proof(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(
                tmp, PODMAN_RM_LEAVES_CONTAINER="1")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("survived podman rm", proc.stderr)
            self.assertEqual(
                self.removed_ids(removed),
                [self.CONTAINER_ID, self.CONTAINER_ID, self.CONTAINER_ID],
                "all cleanup layers must target only the exact ID",
            )
            self.assertTrue(os.path.exists(os.path.join(env["RESULTS"], "WITHDRAWN")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "complete.json")))
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_completion_is_atomic_and_follows_the_outer_lifecycle(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        publisher = source.find("publish_complete() {")
        self.assertGreaterEqual(publisher, 0, "the producer has no completion publisher")
        bootstrap = source.find('if [ "$HOSTCDP_PROCESS_ROLE" = bootstrap ]')
        worker = source.find("\nREPO=", bootstrap)
        wait = source.find('wait "$guardian_pid"', bootstrap, worker)
        publish = source.find("if publish_complete; then", wait, worker)
        self.assertGreaterEqual(publish, 0, "the bootstrap never publishes completion")
        self.assertIn("os.replace(temporary, output_path)",
                      source[publisher:bootstrap])
        self.assertIn("os.replace(pending_summary_path, summary_path)",
                      source[publisher:bootstrap])
        self.assertGreater(publish, wait,
                           "completion precedes the outer lifecycle verdict")
        self.assertIn('if [ "$guardian_rc" -eq 0 ]', source[wait:publish],
                      "a failed outer lifecycle can publish completion")
        final_runtime = source.rfind(
            'verify_runtime_manifest "$CORPUS_EXTRA_RUNTIME_MANIFEST"',
            worker,
        )
        removal = source.rfind("remove_owned_container ||", worker)
        summary = source.find(
            'python3 - "$OUT" "$WARMUP" "$RESULTS/.summary.pending"',
            worker,
        )
        self.assertGreater(final_runtime, worker,
                           "the worker has no final runtime verification")
        self.assertGreater(removal, final_runtime,
                           "completion precedes exact container removal")
        self.assertGreater(summary, removal,
                           "completion precedes derived summary validation")
        self.assertNotIn("\npublish_complete", source[worker:],
                         "the worker can authorize before outer cleanup finishes")

    def test_mid_run_nonnumeric_load_is_raw_evidence_not_a_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LOAD_ACTION="nonnumeric")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("REFUSING", proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
            self.assertEqual(len(rows), 1)
            self.assertIsNone(rows[0]["loadavg1"])
            self.assertEqual(rows[0]["loadavg1_raw"], "not-a-load")
            self.assertEqual(rows[0]["loadavg1_read_status"], 0)
            self.assertIs(rows[0]["measurement_valid"], False)

    def test_mid_run_missing_load_is_raw_evidence_not_a_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LOAD_ACTION="missing")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("REFUSING", proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                row = json.loads(handle.readline())
            self.assertIsNone(row["loadavg1"])
            self.assertNotEqual(row["loadavg1_read_status"], 0)
            self.assertIsInstance(row["loadavg1_raw"], str)
            self.assertIs(row["measurement_valid"], False)

    def test_run_metadata_binds_host_source_and_comparison_semantics(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, state = self.environment(tmp)
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            with open("/proc/sys/kernel/random/boot_id") as handle:
                boot_id = handle.read().strip()
            revision = subprocess.check_output(
                ["git", "-C", os.path.dirname(os.path.dirname(HERE)),
                 "rev-parse", "HEAD"], text=True).strip()
            with open(HOSTCDP, "rb") as handle:
                producer_hash = hashlib.sha256(handle.read()).hexdigest()
            with open(os.path.join(HERE, "phase_supervisor.py"), "rb") as handle:
                supervisor_hash = hashlib.sha256(handle.read()).hexdigest()
            with open(HOST_RESOURCE_FINALIZER, "rb") as handle:
                finalizer_hash = hashlib.sha256(handle.read()).hexdigest()
            import reqbench
            expected = {
                "host_boot_id": boot_id,
                "host_machine": os.uname().machine,
                "host_kernel": os.uname().release,
                "source_revision": revision,
                "runtime_bundle_sha256": env["REQBENCH_RUNTIME_BUNDLE_SHA256"],
                "corpus_extra_runtime_bundle_sha256":
                    env["CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256"],
                "harness_sha256": reqbench.harness_sha256(),
                "hostcdp_sha256": producer_hash,
                "phase_supervisor_sha256": supervisor_hash,
                "host_resource_finalizer_sha256": finalizer_hash,
                "driver": "cdpdrive.py",
                "network": "host (no VM, no DNAT)",
                "comparison_label": "free",
                "cpu_budget": "unlimited",
                "cpus": None,
            }
            for name, value in expected.items():
                self.assertEqual(run.get(name), value, name)
            self.assertRegex(run.get("container_owner_token", ""), r"^[0-9a-f]{32}$")
            with open(state + ".owner") as handle:
                self.assertEqual(handle.read().strip(), run["container_owner_token"])

    def test_runtime_manifest_drift_refuses_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_RUNTIME_ACTION="tamper")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("runtime", proc.stderr.lower())
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_vm_matched_cpu_budget_records_a_finite_json_number(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, CPU_BUDGET="vm-matched", CPUS="2.5")
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            self.assertEqual(run["cpu_budget"], "vm-matched")
            self.assertEqual(run["cpus"], 2.5)
            self.assertIsInstance(run["cpus"], float)

    def test_cpu_budget_and_comparison_label_are_explicit(self):
        cases = (
            ({"CPU_BUDGET": "unlimited", "CPUS": "2"}, "unlimited"),
            ({"CPU_BUDGET": "vm-matched", "CPUS": ""}, "vm-matched"),
            ({"CPU_BUDGET": "unknown"}, "CPU_BUDGET"),
            ({"COMPARISON_LABEL": ""}, "COMPARISON_LABEL"),
        )
        for overrides, expected in cases:
            with self.subTest(overrides=overrides), tempfile.TemporaryDirectory() as tmp:
                env, _removed, _state = self.environment(tmp, **overrides)
                proc = self.run_host(env)
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertIn(expected, proc.stderr)
                self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_producer_and_resummarizer_serialize_on_one_permanent_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            started = os.path.join(tmp, "driver-started")
            release = os.path.join(tmp, "release-driver")
            env, _removed, _state = self.environment(
                tmp,
                DRIVER_STARTED_FILE=started,
                DRIVER_WAIT_FILE=release,
                CORPUS_EXTRA_RUNTIME_MANIFEST="",
                CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256="",
            )
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            resummarizer = None
            producer_output = ("", "")
            resummarizer_output = ("", "")
            opened_shared_lock = False
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(started) and time.monotonic() < deadline:
                    if producer.poll() is not None:
                        output = producer.communicate()
                        self.fail("producer exited before the held rep: " + "".join(output))
                    time.sleep(0.01)
                self.assertTrue(os.path.exists(started), "producer never reached the held rep")
                resummarizer = subprocess.Popen(
                    [sys.executable, os.path.join(HERE, "resummarize.py"), env["RESULTS"]],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                lock_path = os.path.realpath(os.path.join(env["RESULTS"], ".summary.lock"))
                deadline = time.monotonic() + 10
                while resummarizer.poll() is None and time.monotonic() < deadline:
                    fd_dir = f"/proc/{resummarizer.pid}/fd"
                    try:
                        targets = [os.path.realpath(os.path.join(fd_dir, fd))
                                   for fd in os.listdir(fd_dir)]
                    except FileNotFoundError:
                        break
                    if lock_path in targets:
                        opened_shared_lock = True
                        break
                    time.sleep(0.01)
                self.assertTrue(
                    opened_shared_lock,
                    "resummarize did not open the producer's permanent summary lock",
                )
                self.assertIsNone(
                    resummarizer.poll(),
                    "resummarize passed the producer while its records were incomplete",
                )
            finally:
                with open(release, "a"):
                    pass
                producer_output = producer.communicate(timeout=20)
                if resummarizer is not None:
                    resummarizer_output = resummarizer.communicate(timeout=20)
            self.assertEqual(producer.returncode, 0, "".join(producer_output))
            self.assertEqual(resummarizer.returncode, 0, "".join(resummarizer_output))
            self.assertTrue(os.path.exists(os.path.join(env["RESULTS"], ".summary.lock")))
            with open(os.path.join(env["RESULTS"], "summary.json")) as handle:
                self.assertEqual(json.load(handle)["n"], 1)

    def test_both_summary_writers_publish_by_atomic_replace(self):
        with open(HOSTCDP) as handle:
            producer = handle.read()
        with open(os.path.join(HERE, "resummarize.py")) as handle:
            resummarizer = handle.read()
        self.assertIn('"$RESULTS/.summary.lock"', producer)
        self.assertIn("os.replace(temporary, output_path)", producer)
        self.assertNotIn('open(sys.argv[3], "w")', producer)
        self.assertIn("summary_target = open_output_target(summary_path)", resummarizer)
        self.assertIn("lock_target = dict(summary_target)", resummarizer)
        self.assertIn('"name": ".summary"', resummarizer)
        self.assertIn("open_output_lock(lock_target)", resummarizer)
        self.assertIn("acquire_output_lock(lock_target, lock_fd)", resummarizer)
        self.assertNotIn(".resummarize.lock", resummarizer)
        self.assertIn("write_json_atomic(", resummarizer)
        self.assertIn("output_target=summary_target", resummarizer)
        self.assertIn("revalidate_host_inputs(d, identities)", resummarizer)
        self.assertIn("validate_output_lock(lock_target, lock_fd)", resummarizer)
        self.assertIn("before_publish=recheck_inputs_before_publication", resummarizer)


class HostResourceFinalizer(unittest.TestCase):
    """Host-resource cleanup is bounded, exact, and fail-closed."""

    def test_dnsmasq_finalizer_restores_and_verifies_the_prior_active_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            state = os.path.join(tmp, "dnsmasq.state")
            calls = os.path.join(tmp, "systemctl.calls")
            systemctl = os.path.join(bindir, "systemctl")
            with open(systemctl, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "printf '%s\\n' \"$*\" >>\"$SYSTEMCTL_CALLS\"\n"
                    "case \"$1\" in\n"
                    "  start)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    printf 'active\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  is-active)\n"
                    "    [ \"$2\" = --quiet ]\n"
                    "    [ \"$3\" = dnsmasq ]\n"
                    "    grep -qx active \"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n"
                )
            os.chmod(systemctl, 0o755)
            with open(state, "w") as handle:
                handle.write("inactive\n")
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                FCVM_FINALIZER_MODE="dnsmasq",
                FCVM_DNSMASQ_WAS_ACTIVE="yes",
                SYSTEMCTL_CALLS=calls,
                SYSTEMCTL_STATE=state,
            )

            proc = subprocess.run(
                [sys.executable, HOST_RESOURCE_FINALIZER], env=env,
                capture_output=True, text=True, timeout=10,
            )

            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(state) as handle:
                self.assertEqual(handle.read(), "active\n")
            with open(calls) as handle:
                self.assertEqual(
                    handle.read().splitlines(),
                    ["start dnsmasq", "is-active --quiet dnsmasq"],
                )

    def test_missing_container_create_lock_never_queries_podman(self):
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            calls = os.path.join(tmp, "podman.calls")
            podman = os.path.join(bindir, "podman")
            with open(podman, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "printf '%s\\n' \"$*\" >>\"$PODMAN_CALLS\"\n"
                    "exit 99\n"
                )
            os.chmod(podman, 0o755)
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                FCVM_FINALIZER_MODE="container",
                FCVM_CONTAINER_NAME="hostcdp-run",
                FCVM_CONTAINER_OWNER_TOKEN="a" * 32,
                FCVM_CONTAINER_CREATE_LOCK_PATH=os.path.join(tmp, "missing.lock"),
                PODMAN_CALLS=calls,
            )

            proc = subprocess.run(
                [sys.executable, HOST_RESOURCE_FINALIZER], env=env,
                capture_output=True, text=True, timeout=10,
            )

            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertFalse(
                os.path.exists(calls),
                "cleanup queried a global container name before create began",
            )

    def test_retired_container_create_lock_never_queries_podman(self):
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            calls = os.path.join(tmp, "podman.calls")
            podman = os.path.join(bindir, "podman")
            with open(podman, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "printf '%s\\n' \"$*\" >>\"$PODMAN_CALLS\"\n"
                    "exit 99\n"
                )
            os.chmod(podman, 0o755)
            lock_path = os.path.join(tmp, "create.lock")
            with open(lock_path, "w") as handle:
                handle.write("retired\n")
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                FCVM_FINALIZER_MODE="container",
                FCVM_CONTAINER_NAME="hostcdp-run",
                FCVM_CONTAINER_OWNER_TOKEN="a" * 32,
                FCVM_CONTAINER_CREATE_LOCK_PATH=lock_path,
                PODMAN_CALLS=calls,
            )

            proc = subprocess.run(
                [sys.executable, HOST_RESOURCE_FINALIZER], env=env,
                capture_output=True, text=True, timeout=10,
            )

            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertFalse(
                os.path.exists(calls),
                "retired cleanup performed a fallible post-publication Podman proof",
            )

    def test_container_set_removes_owned_ids_before_reporting_foreign_collision(self):
        run_id = "a" * 32
        owner_token = "b" * 32
        foreign_token = "c" * 32
        owned_id = "d" * 64
        foreign_id = "e" * 64
        containers = {
            owned_id: (f"cbmem-{run_id}-host1r1-0", owner_token),
            foreign_id: (f"cbmem-{run_id}-host1r1-1", foreign_token),
        }
        removals = []

        def podman(argv, *_args, **_kwargs):
            command = tuple(argv)
            if command[1:3] == ("ps", "-a"):
                listing = "".join(
                    f"{container_id}|{name}\n"
                    for container_id, (name, _token) in containers.items()
                )
                return host_resource_finalizer.CommandResult(
                    0, listing.encode(), b"")
            if command[1] == "inspect":
                container_id = command[-1]
                if container_id not in containers:
                    return host_resource_finalizer.CommandResult(125, b"", b"missing")
                _name, token = containers[container_id]
                return host_resource_finalizer.CommandResult(
                    0, f"{container_id}|{token}\n".encode(), b"")
            if command[1:3] == ("rm", "-f"):
                identifiers = command[command.index("--") + 1:]
                removals.append(identifiers)
                for container_id in identifiers:
                    containers.pop(container_id, None)
                return host_resource_finalizer.CommandResult(0, b"", b"")
            if command[1:3] == ("container", "exists"):
                return host_resource_finalizer.CommandResult(
                    0 if command[-1] in containers else 1, b"", b"")
            self.fail(f"unexpected Podman command: {command}")

        with tempfile.TemporaryDirectory() as lock_dir, \
             mock.patch.dict(os.environ, {
                 "FCVM_CONTAINER_RUN_ID": run_id,
                 "FCVM_CONTAINER_OWNER_TOKEN": owner_token,
                 "FCVM_CONTAINER_CREATE_LOCK_DIR": lock_dir,
             }), \
             mock.patch.object(
                 host_resource_finalizer, "run_bounded", side_effect=podman):
            with self.assertRaisesRegex(
                    host_resource_finalizer.FinalizerError,
                    "different owner"):
                host_resource_finalizer.finalize_container_set()

        self.assertEqual(removals, [(owned_id,)])
        self.assertNotIn(owned_id, containers)
        self.assertIn(foreign_id, containers)


class CorpusExtraFailClosed(unittest.TestCase):
    """The shell driver cannot publish after a preflight or replay failure."""

    @classmethod
    def source(cls):
        with open(EXTRA) as handle:
            return handle.read()

    def test_unknown_or_empty_phases_are_validated(self):
        source = self.source()
        self.assertIn("validate_phases", source)
        self.assertLess(source.find("validate_phases"), source.find("claim_output_dirs"))

    def test_output_directories_are_claimed_exclusively(self):
        source = self.source()
        helper = re.search(r'^claim_output_dir\(\) \{\n.*?^\}', source,
                           re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(helper, "output ownership helper is gone")
        self.assertLess(source.find("claim_output_dirs"),
                        source.find("provenance.json"),
                        "a reused directory can be modified before it is refused")
        with tempfile.TemporaryDirectory() as tmp:
            existing = os.path.join(tmp, "existing")
            os.mkdir(existing)
            marker = os.path.join(existing, "prior-result")
            with open(marker, "w") as handle:
                handle.write("prior\n")
            script = ("set -euo pipefail\n" + helper.group(0) + "\n"
                      + f"claim_output_dir {existing!r} results\n")
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=10)
            self.assertNotEqual(proc.returncode, 0)
            with open(marker) as handle:
                self.assertEqual(handle.read(), "prior\n")

    def test_all_critical_external_dependencies_are_preflighted(self):
        source = self.source()
        tools = re.search(r'^for tool in (.*?); do$', source,
                          re.MULTILINE | re.DOTALL).group(1).replace("\\", "").split()
        for required in ("git", "sha256sum", "systemctl"):
            self.assertIn(required, tools)

    def test_outer_stray_preflight_distinguishes_no_match_from_error(self):
        source = self.source()
        self.assertIn("find_stray_vmms", source)
        self.assertRegex(source, r'case "\$rc" in\s*0\)')
        self.assertRegex(source, r'\s1\)')
        self.assertIn("pgrep", re.search(r'^for tool in .*?; do$', source,
                                         re.MULTILINE | re.DOTALL).group(0))

    def test_replay_readiness_is_bound_to_this_server(self):
        source = self.source()
        helper = re.search(
            r'^replay_probe_logged\(\) \{\n.*?^\}', source,
            re.MULTILINE | re.DOTALL,
        )
        self.assertIsNotNone(helper, "startup responses are not bound to this server's logs")
        launch = source[
            source.index('SERVE_CONTROL_PATH='):
            source.index('# Every corpus member')
        ]
        self.assertIn('replay_probe_logged "$readiness_qname" "$readiness_path"',
                      launch)

        with tempfile.TemporaryDirectory() as tmp:
            dns_log = os.path.join(tmp, "corpus-dns.log")
            access_log = os.path.join(tmp, "corpus-access.log")
            stale_qname = "ready-stale.blog.cloudflare.com"
            stale_path = "/?fcvm-ready=stale"
            with open(dns_log, "w") as handle:
                handle.write(json.dumps({
                    "qname": stale_qname, "qtype": 1, "answer": "10.0.2.2",
                }) + "\n")
            with open(access_log, "w") as handle:
                handle.write(json.dumps({
                    "host": "blog.cloudflare.com", "path": stale_path,
                    "status": 200,
                }) + "\n")

            def probe(qname, path):
                script = (
                    "set -euo pipefail\n"
                    f"RESULTS={tmp!r}\n"
                    + helper.group(0) + "\n"
                    + f"replay_probe_logged {qname!r} {path!r}\n"
                )
                return subprocess.run(
                    ["bash", "-c", script], capture_output=True, text=True,
                    timeout=10,
                )

            self.assertNotEqual(
                probe("ready-current.blog.cloudflare.com",
                      "/?fcvm-ready=current").returncode,
                0,
                "a stale response from this run was accepted for a later probe",
            )
            self.assertEqual(probe(stale_qname, stale_path).returncode, 0)

    def test_replay_retry_exhaustion_requires_log_evidence(self):
        """The last DNS and HTTPS response cannot bypass the log check.

        RED BEFORE THE FIX: the final retry returned the expected DNS answer
        and HTTPS status while replay_probe_logged failed, but the two scalar
        checks after the loop still accepted startup.
        """
        source = self.source()
        launch = source[
            source.index('answer=""'):
            source.index('# Every corpus member')
        ]
        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "set -euo pipefail\n"
                f"RESULTS={tmp!r}\n"
                f"LOGDIR={tmp!r}\n"
                "RUN_ID=readiness-test\n"
                "seq() { printf '100\\n'; }\n"
                "dig() { printf '10.0.2.2\\n'; }\n"
                "curl() { printf '200'; }\n"
                "sleep() { :; }\n"
                "replay_probe_logged() { return 1; }\n"
                + launch
            )
            proc = subprocess.run(
                ["bash", "-c", script],
                capture_output=True, text=True, timeout=10,
            )

        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(
            "replay readiness never satisfied DNS, HTTPS, and log evidence",
            proc.stderr,
        )

    def test_replay_nonzero_exit_prevents_success(self):
        source = self.source()
        final_records = source.rfind('say "records:')
        stop = source.rfind("stop_corpus_serve", 0, final_records)
        require = source.rfind("require_corpus_serve_clean", 0, final_records)
        self.assertGreater(stop, source.index('if [[ ",$PHASES,"'))
        self.assertGreater(require, stop)


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

    def assert_blocks(self):
        """SystemExit with a NON-ZERO code. `sys.exit(0)` is also a SystemExit,
        and a preflight that exits clean is the fail-open this refuses."""
        with self.assertRaises(SystemExit) as caught:
            corpus_mem.stray_vmm_processes()
        self.assertNotIn(caught.exception.code, (0, None),
                         "the preflight exited cleanly; the run would continue")

    def test_a_preflight_that_could_not_run_blocks_the_run(self):
        """exit 127 is "pgrep is not installed", not "the box is clean"."""
        self.patch(lambda *_a, **_k: Completed(127, "", "pgrep: command not found"))
        self.assert_blocks()

    def test_a_preflight_that_errored_blocks_the_run(self):
        self.patch(lambda *_a, **_k: Completed(2, "", "pgrep: syntax error"))
        self.assert_blocks()

    def test_a_preflight_that_cannot_be_spawned_blocks_the_run(self):
        def boom(*_a, **_k):
            raise FileNotFoundError("pgrep")
        self.patch(boom)
        self.assert_blocks()


class ZeroBasis(unittest.TestCase):
    """A basis of zero is a sample that could not see the process set.

    Every basis is a sum over the processes of live instances that have each
    rendered a page, so with the instance count satisfied none of them can be
    zero. report.py's single-node cgroup.procs read produced exactly this on
    the container side: pool_containers = N from `podman ps`, pool_procs = 0 and
    pool_pss_kb = 0 from an empty process set. cell_values reads those with
    with `.get(key, 0)`, so a zero used to become a real number in summary.json
    and in the least-squares fit while the run's own instance-count check passed.
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

    def test_an_unreadable_cgroup_root_is_not_a_zero_measurement(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = os.path.join(tmp, "disappeared")
            with self.assertRaises(bench_report.CgroupReadError):
                bench_report.measure_cgroup_set(missing, "serve-")

    def test_a_serve_sample_with_no_process_basis_is_refused(self):
        args = SimpleNamespace(state_dir="/state")
        side = corpus_mem.FcvmSide(args, SimpleNamespace(base="/cgroup"), "run")
        clone = {
            "clones": 1,
            "clone_procs": 4,
            "clone_cgroup_kb": 4096,
            "clone_pss_kb": 2048,
        }
        invalid_serve_samples = (
            {},
            {"clone_procs": 0, "clone_cgroup_kb": 1024, "clone_pss_kb": 0},
        )
        for serve in invalid_serve_samples:
            with self.subTest(serve=serve), mock.patch.object(
                    corpus_mem, "sample", side_effect=(clone, serve)):
                with self.assertRaises(SystemExit) as caught:
                    side.sample({}, "fcvm1r1")
                self.assertNotIn(caught.exception.code, (0, None))

    def test_nonmonotonic_steady_samples_use_per_basis_medians(self):
        cell = {
            "side": "fcvm-clone",
            "n": 1,
            "rep": 1,
            "pre": {"mem_available_kb": 1_000 * 1024},
            "steady": [
                {"clones": 1, "clone_cgroup_kb": 100 * 1024,
                 "clone_pss_kb": 300 * 1024,
                 "mem_available_kb": 850 * 1024},
                {"clones": 1, "clone_cgroup_kb": 300 * 1024,
                 "clone_pss_kb": 100 * 1024,
                 "mem_available_kb": 900 * 1024},
                {"clones": 1, "clone_cgroup_kb": 200 * 1024,
                 "clone_pss_kb": 200 * 1024,
                 "mem_available_kb": 800 * 1024},
            ],
        }
        values = corpus_mem.cell_values(cell)
        self.assertEqual(values["cgroup_mib"], 200.0)
        self.assertEqual(values["pss_mib"], 200.0)
        self.assertEqual(values["mem_available_delta_mib"], 150.0)

    def test_every_steady_sample_is_validated_before_the_median(self):
        good = {
            "pool_containers": 1, "pool_procs": 4,
            "pool_cgroup_kb": 200 * 1024, "pool_pss_kb": 180 * 1024,
            "mem_available_kb": 800 * 1024,
        }
        missing_pss = dict(good, pool_procs=0, pool_pss_kb=0)

        class Side:
            name = "host-container"

            def __init__(self):
                self.samples = iter((
                    {"mem_available_kb": 1_000 * 1024},
                    missing_pss,
                    good,
                    good,
                    {"mem_available_kb": 1_000 * 1024},
                ))

            def sample(self, _common, _cell_tag):
                return next(self.samples)

            def bring_up(self, _n, _cell_tag, _url_indices):
                return [{"url": "https://example.com/"}]

            def tear_down(self, _live):
                pass

        args = SimpleNamespace(
            run_id="a" * 32, tag="tag", image="image", uffd_mode="minor",
            uffd_prefetch="on", settle=0,
        )
        with mock.patch.object(corpus_mem, "quiesce"), \
             mock.patch.object(corpus_mem.time, "sleep"):
            with self.assertRaises(SystemExit) as caught:
                corpus_mem.run_cell(Side(), args, 1, 1, [0], io.StringIO())
        self.assertNotIn(caught.exception.code, (0, None))


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

    def test_every_podman_lifecycle_call_carries_a_bound(self):
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            src = handle.read()
        for operation in ("rm", "ps", "logs", "inspect"):
            for match in re.finditer(
                    rf'(sh(?:_bounded)?)\(\[\s*"podman",\s*"{operation}"', src):
                self.assertEqual(match.group(1), "sh_bounded",
                                 f"unbounded podman {operation} at byte {match.start()}")


class CgroupLifecycle(unittest.TestCase):
    """A stale or unremovable cgroup is contamination, not cleanup success."""

    def test_setup_refuses_an_existing_run_cgroup(self):
        cg = corpus_mem.CgroupSet("/sys/fs/cgroup/cbmem-existing.slice")
        with mock.patch("os.path.exists", return_value=True), \
             mock.patch.object(corpus_mem, "sh_bounded") as run:
            with self.assertRaises(SystemExit):
                cg.setup()
        run.assert_not_called()

    def test_failed_rmdir_is_reported_while_the_cgroup_still_exists(self):
        cg = corpus_mem.CgroupSet("/sys/fs/cgroup/cbmem-owned.slice")
        with mock.patch.object(
                corpus_mem, "sh_bounded",
                return_value=Completed(1, "", "Device or resource busy")), \
             mock.patch("os.path.isdir", return_value=True):
            with self.assertRaises(RuntimeError):
                cg.rm("leaf")


class RunOutputOwnership(unittest.TestCase):
    """A reused result path must not mix records from different runs."""

    def test_results_directory_can_only_be_claimed_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "memory-run")
            corpus_mem.claim_results_dir(results)
            marker = os.path.join(results, "prior-result")
            with open(marker, "w") as handle:
                handle.write("prior\n")
            with self.assertRaises(SystemExit):
                corpus_mem.claim_results_dir(results)
            with open(marker) as handle:
                self.assertEqual(handle.read(), "prior\n")


class SnapshotIdentity(unittest.TestCase):
    """A tag's existence does not identify the image or resolver it captured."""

    @staticmethod
    def generation(**overrides):
        record = {
            "image": "localhost/chromium-bench-req",
            "image_id": "sha256:" + "a" * 64,
            "guest_dns": "10.0.2.2",
            "dns_server": "10.0.2.2",
            "guest_env": [],
            "creator_fcvm_sha256": "b" * 64,
            "creator_runtime_bundle_sha256": "c" * 64,
            "source_revision": "d" * 40,
        }
        record.update(overrides)
        return record

    def test_matching_snapshot_identity_is_accepted(self):
        corpus_mem.validate_snapshot_for_benchmark(
            self.generation(), "localhost/chromium-bench-req",
            "sha256:" + "a" * 64, "10.0.2.2", "b" * 64,
            "c" * 64, "d" * 40)

    def test_snapshot_of_another_image_is_refused(self):
        with self.assertRaises(SystemExit):
            corpus_mem.validate_snapshot_for_benchmark(
                self.generation(), "localhost/chromium-bench-req",
                "sha256:" + "b" * 64, "10.0.2.2", "b" * 64,
                "c" * 64, "d" * 40)

    def test_snapshot_without_the_replay_resolver_is_refused(self):
        with self.assertRaises(SystemExit):
            corpus_mem.validate_snapshot_for_benchmark(
                self.generation(guest_dns=None, dns_server="127.0.0.53"),
                "localhost/chromium-bench-req", "sha256:" + "a" * 64,
                "10.0.2.2", "b" * 64, "c" * 64, "d" * 40)

    def test_snapshot_creator_must_match_every_staged_runtime_identity(self):
        expected = ("b" * 64, "c" * 64, "d" * 40)
        fields = (
            ("creator_fcvm_sha256", "e" * 64),
            ("creator_runtime_bundle_sha256", "f" * 64),
            ("source_revision", "1" * 40),
        )
        for field, wrong in fields:
            with self.subTest(field=field):
                with self.assertRaises(SystemExit):
                    corpus_mem.validate_snapshot_for_benchmark(
                        self.generation(**{field: wrong}),
                        "localhost/chromium-bench-req", "sha256:" + "a" * 64,
                        "10.0.2.2", *expected)

    def test_fcvm_digest_is_computed_before_snapshot_validation(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read()
        main = source[source.index("def main_with_resources(resources") :]
        digest = main.find("fcvm_sha256 = sha256_file(args.fcvm)")
        validate = main.find("validate_snapshot_for_benchmark(")
        self.assertGreaterEqual(digest, 0, "the current fcvm bytes are never identified")
        self.assertGreater(validate, digest,
                           "snapshot provenance is accepted before current fcvm is identified")


class ArgumentValidation(unittest.TestCase):
    """An empty measurement grid is not a successful run."""

    @staticmethod
    def args(**overrides):
        args = SimpleNamespace(
            urls=["https://example.com/"], ns=[1, 2, 4, 8], reps=5,
            settle=5.0, quiet_limit=1.0,
            quiet_wait=300.0, run_id="a" * 32,
            container_owner_token="b" * 32,
            source_revision="c" * 40,
            runtime_bundle_sha256="d" * 64,
            corpus_extra_runtime_bundle_sha256="e" * 64,
        )
        for key, value in overrides.items():
            setattr(args, key, value)
        return args

    def assert_refused(self, **overrides):
        with self.assertRaises(SystemExit) as caught:
            corpus_mem.validate_args(self.args(**overrides))
        self.assertNotIn(caught.exception.code, (0, None))

    def test_empty_or_nonpositive_grids_are_refused(self):
        for overrides in ({"ns": []}, {"ns": [0, 2]}, {"ns": [-1]},
                          {"reps": 0}, {"reps": -1}, {"urls": []}):
            with self.subTest(overrides=overrides):
                self.assert_refused(**overrides)

    def test_invalid_run_id_is_refused(self):
        for value in ("", "short", "A" * 32, "a/b", "has space", "a" * 100):
            with self.subTest(run_id=value):
                self.assert_refused(run_id=value)

    def test_valid_arguments_are_accepted(self):
        corpus_mem.validate_args(self.args())

    def test_reps_must_cover_whole_corpus_cycles(self):
        self.assert_refused(urls=["one", "two", "three"], reps=2)
        corpus_mem.validate_args(
            self.args(urls=["one", "two", "three"], reps=6))

    def test_reps_must_supply_five_uncertainty_blocks(self):
        self.assert_refused(reps=4)

    def test_unpaired_cpu_measurement_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = subprocess.run(
                [sys.executable, CORPUS_MEM, "--results", os.path.join(tmp, "out"),
                 "--tag", "tag", "--urls", "https://example.com/",
                 "--source-revision", "a" * 40,
                 "--runtime-bundle-sha256", "b" * 64,
                 "--corpus-extra-runtime-bundle-sha256", "c" * 64,
                 "--cputime-reps", "1"],
                capture_output=True, text=True, timeout=30,
            )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unrecognized arguments: --cputime-reps 1", proc.stderr)

    def test_invalid_container_owner_token_is_refused(self):
        for value in ("", "short", "A" * 32, "a/b", "has space", "a" * 100):
            with self.subTest(container_owner_token=value):
                self.assert_refused(container_owner_token=value)

    def test_invalid_runtime_identity_is_refused(self):
        fields = (
            ("source_revision", ("", "abc", "g" * 40, "a" * 41)),
            ("runtime_bundle_sha256", ("", "a" * 63, "g" * 64)),
            ("corpus_extra_runtime_bundle_sha256", ("", "a" * 65, "G" * 64)),
        )
        for field, values in fields:
            for value in values:
                with self.subTest(field=field, value=value):
                    self.assert_refused(**{field: value})

    def test_csv_parsing_rejects_empty_members(self):
        for value in ("", ",", "one,", ",one", "one,,two"):
            with self.subTest(value=value):
                with self.assertRaises(SystemExit):
                    corpus_mem.parse_csv(value, "--values")
        self.assertEqual(corpus_mem.parse_csv("one,two", "--values"),
                         ["one", "two"])


class CanonicalImageIdentity(unittest.TestCase):
    """Podman prints bare IDs while snapshot provenance stores sha256: IDs."""

    def test_bare_and_prefixed_ids_have_one_identity(self):
        digest = "a" * 64
        self.assertEqual(corpus_mem.canonical_image_id(digest), "sha256:" + digest)
        self.assertEqual(corpus_mem.canonical_image_id("sha256:" + digest),
                         "sha256:" + digest)

    def test_malformed_image_id_is_refused(self):
        for value in ("", "a" * 63, "sha256:" + "g" * 64):
            with self.subTest(value=value):
                with self.assertRaises(SystemExit):
                    corpus_mem.canonical_image_id(value)

    def test_outer_preflight_canonicalizes_bare_and_prefixed_image_ids(self):
        with open(EXTRA) as handle:
            source = handle.read()
        match = re.search(
            r'^canonical_runtime_image_id\(\) \{\n.*?^\}', source,
            re.MULTILINE | re.DOTALL,
        )
        self.assertIsNotNone(match, "the outer image identity has no canonicalizer")
        digest = "a" * 64
        script = (match.group(0) + "\n"
                  + f"canonical_runtime_image_id {digest!r}\n"
                  + f"canonical_runtime_image_id {'sha256:' + digest!r}\n"
                  + "canonical_runtime_image_id malformed\n")
        proc = subprocess.run(["bash", "-c", script], capture_output=True,
                              text=True, timeout=30)
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout.splitlines(),
                         ["sha256:" + digest, "sha256:" + digest])

    def test_campaign_preserves_logical_tag_and_passes_exact_image_id(self):
        with open(EXTRA) as handle:
            source = handle.read()
        self.assertIn("podman image inspect", source)
        host_start = source.index('run_logged "$LOGDIR/hostcdp-$arm.log"')
        host_end = source.index('RESULTS="$RESULTS/hostcdp-$arm"', host_start)
        host = source[host_start:host_end]
        self.assertIn('IMAGE="$IMAGE"', host)
        self.assertIn('IMAGE_ID="$RUNTIME_IMAGE"', host)
        memory = source[source.index('run_logged "$LOGDIR/memory.log"'):]
        self.assertIn('--image "$IMAGE" --image-id "$RUNTIME_IMAGE"', memory)

    def test_memory_preflight_uses_image_namespace_and_launches_exact_id(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read()
        self.assertIn(
            '["podman", "image", "inspect", "--format", "{{.Id}}", args.image]',
            source,
        )
        self.assertIn('getattr(self.args, "image_id", self.args.image)', source)


class CampaignIntegrityRegression(unittest.TestCase):
    """The campaign cannot publish after an ambiguous create or failed phase."""

    @staticmethod
    def shell_function(source, name):
        match = re.search(rf'^{name}\(\) \{{\n.*?^\}}', source,
                          re.MULTILINE | re.DOTALL)
        if match is None:
            raise AssertionError(f"{name} is missing")
        return match.group(0)

    @staticmethod
    def memory_completion_record(run_id):
        return {
            "schema_version": 1,
            "run_id": run_id,
            "artifacts": [
                {"path": name, "size": ordinal,
                 "sha256": str(ordinal) * 64}
                for ordinal, name in enumerate(
                    ("run.json", "samples.jsonl", "summary.json"), 1
                )
            ],
        }

    @classmethod
    def memory_completion_bytes(cls, run_id):
        return (
            json.dumps(
                cls.memory_completion_record(run_id),
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
            + b"\n"
        )

    def test_podman_timeouts_escalate_to_kill(self):
        """Every bounded Podman command escalates from TERM to KILL.

        RED BEFORE THE FIX: thirteen Podman calls used `timeout 30`, which can
        wait forever after sending TERM to a command that does not exit.
        """
        for path in (EXTRA, HOSTCDP):
            with self.subTest(path=os.path.basename(path)):
                with open(path) as handle:
                    source = handle.read().replace("\\\n", " ")
                commands = [
                    line.strip()
                    for line in source.splitlines()
                    if re.search(r'(?<![-\w])timeout\b.*\bpodman\b', line)
                ]

                self.assertTrue(commands, "no bounded Podman commands found")
                for command in commands:
                    self.assertIn(
                        "--kill-after=", command,
                        f"Podman timeout has no KILL escalation: {command}",
                    )

    def test_withdrawal_writers_lock_before_invalidating_or_marking_a_run(self):
        """Every producer of WITHDRAWN must take the exclusive side of the
        run-directory lock before it removes completion records or publishes
        the marker. campaign_summary.py and compare.py hold the shared side
        while reading and committing, so source order is the deterministic race
        invariant.

        RED BEFORE THE FIX: neither writer opened or locked its result
        directory, and both removed publication state before any lock.
        """
        for path, function, first_invalidated in (
            (EXTRA, "mark_campaign_withdrawn", "campaign-complete.json"),
            (HOSTCDP, "withdraw_failed_run", "complete.json"),
        ):
            with self.subTest(path=os.path.basename(path)):
                with open(path) as handle:
                    body = self.shell_function(handle.read(), function)
                opened = body.index('exec {withdrawal_lock_fd}<"$RESULTS"')
                locked = body.index('flock -x "$withdrawal_lock_fd"')
                invalidated = body.index(first_invalidated)
                marker = body.index('"$RESULTS/WITHDRAWN"')
                self.assertLess(opened, locked)
                self.assertLess(locked, invalidated)
                self.assertLess(invalidated, marker)

    def test_failed_host_withdrawal_invalidation_is_marked_and_returns_failure(self):
        """A failed invalidation still withdraws the run and returns failure.

        WITHDRAWN is the fail-closed signal when an authorization file cannot
        be removed. The writer must publish that signal under its lock, but it
        cannot report a successful state transition after incomplete cleanup.

        RED BEFORE THE FIX: withdraw_failed_run left complete.json beside the
        marker and returned success after logging the failed removal.
        """
        with tempfile.TemporaryDirectory() as tmp:
            with open(HOSTCDP) as handle:
                body = self.shell_function(handle.read(), "withdraw_failed_run")
            os.mkdir(os.path.join(tmp, "complete.json"))
            with open(os.path.join(tmp, "summary.json"), "w") as handle:
                handle.write("{}\n")
            script = (
                "set -u\n"
                f"RESULTS={tmp!r}\n"
                "log() { printf '%s\\n' \"$*\" >&2; }\n"
                + body + "\n"
                + "withdraw_failed_run 'deterministic rm failure'\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script],
                capture_output=True, text=True, timeout=30,
            )

            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("could not remove derived authorization", proc.stderr)
            self.assertTrue(os.path.isdir(os.path.join(tmp, "complete.json")))
            self.assertFalse(os.path.lexists(os.path.join(tmp, "summary.json")))
            with open(os.path.join(tmp, "WITHDRAWN")) as handle:
                self.assertEqual(handle.read(), "deterministic rm failure\n")

    def test_cleanup_preserves_primary_status_when_withdrawal_reports_failure(self):
        """A secondary withdrawal error cannot replace the primary failure.

        The EXIT trap deliberately carries final_rc through cleanup. Its call
        to a fallible withdrawal writer must be in a conditional so errexit
        reaches the explicit final exit instead of replacing that status.

        RED BEFORE THE FIX: an original exit 7 became exit 1 when withdrawal
        returned failure after publishing its marker.
        """
        with tempfile.TemporaryDirectory() as tmp:
            with open(HOSTCDP) as handle:
                source = handle.read()
            withdrawal = self.shell_function(source, "withdraw_failed_run")
            cleanup = self.shell_function(source, "cleanup")
            os.mkdir(os.path.join(tmp, "complete.json"))
            with open(os.path.join(tmp, "summary.json"), "w") as handle:
                handle.write("{}\n")
            script = (
                "set -eu\n"
                f"RESULTS={tmp!r}\n"
                "CREATE_OP_STARTED=false\n"
                "CREATE_OP_QUIESCENT=false\n"
                "CREATE_OUTPUT_PATH=\n"
                "CREATE_OUTCOME_CHECKED=false\n"
                "CONTAINER_OWNERSHIP_VERIFIED=false\n"
                "CREATE_OP_LOCK_FD=\n"
                "log() { printf '%s\\n' \"$*\" >&2; }\n"
                + withdrawal + "\n"
                + cleanup + "\n"
                + "trap cleanup EXIT\n"
                + "exit 7\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script],
                capture_output=True, text=True, timeout=30,
            )

            self.assertEqual(proc.returncode, 7, proc.stdout + proc.stderr)
            with open(os.path.join(tmp, "WITHDRAWN")) as handle:
                self.assertEqual(
                    handle.read(),
                    "hostcdp exited 7; raw completion is not authorized\n",
                )

    def test_withdrawal_writers_hold_the_exclusive_lock_through_publication(self):
        """Pause each real writer after it invalidates completion but before
        its atomic marker publication. A nonblocking shared reader must still
        be excluded at that point, proving the directory FD remains open and
        locked for the whole state transition.

        RED IN THE CODE-ONLY REVERT PROOF: both shared readers acquired the
        directory while the writer was paused inside marker publication.
        """
        for path, function, tool, real_tool, invalidated in (
            (EXTRA, "mark_campaign_withdrawn", "mv", shutil.which("mv"),
             "campaign-complete.json"),
            (HOSTCDP, "withdraw_failed_run", "python3", sys.executable,
             "complete.json"),
        ):
            with self.subTest(path=os.path.basename(path)), \
                 tempfile.TemporaryDirectory() as tmp:
                with open(path) as handle:
                    body = self.shell_function(handle.read(), function)
                tools = os.path.join(tmp, "tools")
                os.mkdir(tools)
                reached = os.path.join(tmp, "writer-reached")
                release = os.path.join(tmp, "writer-release")
                wrapper = os.path.join(tools, tool)
                with open(wrapper, "w") as handle:
                    handle.write(
                        "#!/bin/sh\n"
                        ': > "$WRITER_REACHED"\n'
                        'while [ ! -e "$WRITER_RELEASE" ]; do sleep 0.01; done\n'
                        'exec "$REAL_WITHDRAWAL_TOOL" "$@"\n'
                    )
                os.chmod(wrapper, 0o755)
                for name in ("campaign-complete.json", "complete.json",
                             "summary.json"):
                    with open(os.path.join(tmp, name), "w") as handle:
                        handle.write("{}\n")
                script = (
                    "set -u\n"
                    f"RESULTS={tmp!r}\n"
                    "log() { :; }\n"
                    + body + "\n"
                    + f"{function} 'deterministic lock test'\n"
                )
                env = dict(
                    os.environ,
                    PATH=tools + os.pathsep + os.environ["PATH"],
                    WRITER_REACHED=reached,
                    WRITER_RELEASE=release,
                    REAL_WITHDRAWAL_TOOL=real_tool,
                )
                writer = subprocess.Popen(
                    ["bash", "-c", script], env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                try:
                    deadline = time.monotonic() + 10
                    while not os.path.exists(reached) and time.monotonic() < deadline:
                        if writer.poll() is not None:
                            break
                        time.sleep(0.01)
                    self.assertTrue(
                        os.path.exists(reached),
                        f"writer exited before marker publication: {writer.poll()}",
                    )
                    self.assertFalse(os.path.exists(os.path.join(tmp, invalidated)))
                    reader = subprocess.run(
                        ["flock", "-n", "-s", tmp, "true"],
                        capture_output=True, text=True, timeout=10,
                    )
                    self.assertEqual(
                        reader.returncode, 1,
                        "a shared publication reader crossed an in-progress withdrawal",
                    )
                finally:
                    with open(release, "w"):
                        pass
                    stdout, stderr = writer.communicate(timeout=10)
                self.assertEqual(writer.returncode, 0, stdout + stderr)
                self.assertTrue(os.path.isfile(os.path.join(tmp, "WITHDRAWN")))
                self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_podman_run_must_return_one_full_lowercase_container_id(self):
        args = SimpleNamespace(
            image="image", urls=["https://example.com/"],
            container_owner_token="a" * 32,
            container_create_ops_dir=None,
        )
        side = corpus_mem.ContainerSide(args, "b" * 32)
        with mock.patch.object(corpus_mem, "sh_bounded",
                               return_value=Completed(0, "short-id\n", "")), \
             mock.patch.object(corpus_mem, "sh", return_value=Completed()):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
        self.assertIn(side.prefix("host1r1") + "0", side.owned,
                      "an ambiguous create lost its cleanup ownership")

    def test_malformed_success_keeps_create_lease_until_reconciliation(self):
        args = SimpleNamespace(
            image="image", urls=["https://example.com/"],
            container_owner_token="a" * 32,
            container_create_ops_dir="unused",
        )
        side = corpus_mem.ContainerSide(args, "b" * 32)
        name = side.prefix("host1r1") + "0"

        class Operation:
            released = False

            def finish(self, _timeout):
                return Completed(0, "short-id\n", "")

            def acquire_reconciliation(self, _timeout):
                pass

            def release(self):
                self.released = True

        operation = Operation()
        with mock.patch.object(
                corpus_mem, "start_container_create",
                return_value=(operation, Completed(0, "short-id\n", ""))):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
        self.assertIs(side.create_operations.get(name), operation)
        self.assertFalse(operation.released,
                         "malformed output released the unknown create outcome")

        with mock.patch.object(corpus_mem, "inspected_container_identity",
                               return_value=None):
            side.stop_all()
        self.assertTrue(operation.released)
        self.assertNotIn(name, side.owned)

    def test_hung_create_is_killed_reaped_and_holds_lease_until_reconciled(self):
        with tempfile.TemporaryDirectory() as tmp:
            operation = corpus_mem.ContainerCreateOperation(
                [sys.executable, "-c",
                 "import os,signal,time; "
                 "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                 "print(os.getpid(), flush=True); time.sleep(60)"],
                tmp, "hung-create", 0.05, 2.0, 0.2,
            )
            child_pid = int(operation.process.stdout.readline())
            lock_path = os.path.join(tmp, "hung-create.lock")
            result = None
            reaped = False
            still_locked = False
            try:
                result = operation.finish(0.01)
                reaped = operation.process.poll() is not None
                probe = subprocess.run(
                    ["flock", "-x", "-n", lock_path, "true"],
                    capture_output=True, text=True, timeout=10)
                still_locked = probe.returncode != 0
            finally:
                if operation.process.poll() is None:
                    operation.process.kill()
                    operation.process.communicate(timeout=10)
                if getattr(operation, "lock_fd", None) is not None:
                    os.close(operation.lock_fd)
                    operation.lock_fd = None
            self.assertIsNotNone(result)
            self.assertEqual(result.returncode, 124)
            self.assertTrue(reaped)
            self.assertFalse(os.path.exists(f"/proc/{child_pid}"))
            self.assertTrue(still_locked,
                            "the create lease was released before reconciliation")
            probe = subprocess.run(
                ["flock", "-x", "-n", lock_path, "true"],
                capture_output=True, text=True, timeout=10)
            self.assertEqual(probe.returncode, 0, probe.stderr)

    def test_late_create_commit_precedes_exclusive_reconciliation(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "committed")
            lock_path = os.path.join(tmp, "late-create.lock")
            operation = corpus_mem.ContainerCreateOperation(
                [sys.executable, "-c", "print('leader-done', flush=True)"],
                tmp, "late-create",
            )
            result = operation.finish(10)
            self.assertEqual(result.returncode, 0, result.stderr)
            holder = subprocess.Popen(
                [sys.executable, "-c",
                 "import fcntl,os,sys; "
                 "fd=os.open(sys.argv[1], os.O_RDWR); "
                 "fcntl.flock(fd, fcntl.LOCK_SH); "
                 "print('locked', flush=True); sys.stdin.buffer.read(1); "
                 "open(sys.argv[2], 'w').write('committed\\n'); os.close(fd)",
                 lock_path, marker],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            self.assertEqual(holder.stdout.readline().strip(), "locked")
            reconciliation_started = threading.Event()
            writer_error = []

            def allow_commit():
                if not reconciliation_started.wait(10):
                    writer_error.append("exclusive reconciliation never started")
                    return
                try:
                    holder.stdin.write("1")
                    holder.stdin.flush()
                    holder.stdin.close()
                except (OSError, ValueError) as exc:
                    writer_error.append(str(exc))

            writer = threading.Thread(target=allow_commit)
            writer.start()
            shared_fd = operation.lock_fd
            real_close = os.close

            def observed_close(fd):
                result = real_close(fd)
                if fd == shared_fd:
                    reconciliation_started.set()
                return result

            try:
                with mock.patch.object(corpus_mem.os, "close", observed_close):
                    operation.acquire_reconciliation(10)
                writer.join(timeout=10)
                self.assertFalse(writer.is_alive())
                self.assertEqual(writer_error, [])
                holder.wait(timeout=10)
                holder_error = holder.stderr.read()
                holder.stdout.close()
                holder.stderr.close()
                self.assertEqual(holder.returncode, 0, holder_error)
                self.assertTrue(os.path.isfile(marker),
                                "absence could be inspected before the late commit")
            finally:
                reconciliation_started.set()
                writer.join(timeout=10)
                if holder.poll() is None:
                    holder.kill()
                    holder.communicate(timeout=5)
                if holder.stdout is not None and not holder.stdout.closed:
                    holder.stdout.close()
                if holder.stderr is not None and not holder.stderr.closed:
                    holder.stderr.close()
                if getattr(operation, "lock_fd", None) is not None:
                    os.close(operation.lock_fd)
                    operation.lock_fd = None
                if getattr(operation, "reconcile_fd", None) is not None:
                    os.close(operation.reconcile_fd)
                    operation.reconcile_fd = None

    def test_create_supervisor_drains_fd_closing_escaped_committer(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "late-commit")
            child_pid_path = os.path.join(tmp, "late-child")
            child_ready_path = os.path.join(tmp, "late-child-ready")
            child = (
                "import os,signal,time; "
                "[os.close(fd) for fd in range(3,256) "
                "if os.path.exists(f'/proc/self/fd/{fd}')]; "
                "time.sleep(0.25); "
                f"signal.signal(signal.SIGTERM, lambda *_: "
                f"(open({marker!r}, 'w').write('committed\\n'), "
                "raise_exit())[1]); "
                f"ready_fd=os.open({child_ready_path!r}, "
                "os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600); "
                "os.write(ready_fd, b'ready\\n'); os.close(ready_fd); "
                "time.sleep(60)"
            )
            child = child.replace(
                "import os,signal,time; ",
                "import os,signal,time; raise_exit=lambda: "
                "(_ for _ in ()).throw(SystemExit(0)); ",
            )
            leader = (
                "import os,subprocess,sys,time\n"
                "proc=subprocess.Popen([sys.executable, '-c', sys.argv[1]], "
                "start_new_session=True, stdin=subprocess.DEVNULL, "
                "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
                f"with open({child_pid_path!r}, 'w') as handle:\n"
                "    handle.write(str(proc.pid))\n"
                f"ready={child_ready_path!r}\n"
                "deadline=time.monotonic()+5\n"
                "while not os.path.isfile(ready):\n"
                "    if proc.poll() is not None:\n"
                "        raise SystemExit('escaped child exited before readiness')\n"
                "    if time.monotonic() >= deadline:\n"
                "        raise SystemExit('escaped child never became ready')\n"
                "    time.sleep(0.001)\n"
                "print('leader-finished', flush=True)\n"
            )
            operation = corpus_mem.ContainerCreateOperation(
                [sys.executable, "-c", leader, child], tmp, "fd-closing-create")
            result = operation.finish(10)
            with open(child_pid_path) as handle:
                child_pid = int(handle.read())
            try:
                self.assertEqual(
                    result.returncode, 1,
                    "an escaped create committer was not part of operation completion",
                )
                self.assertTrue(os.path.isfile(marker))
                operation.acquire_reconciliation(10)
            finally:
                try:
                    pidfd = os.pidfd_open(child_pid)
                except ProcessLookupError:
                    pidfd = None
                if pidfd is not None:
                    try:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    finally:
                        os.close(pidfd)
                if getattr(operation, "lock_fd", None) is not None:
                    os.close(operation.lock_fd)
                    operation.lock_fd = None
                if getattr(operation, "reconcile_fd", None) is not None:
                    os.close(operation.reconcile_fd)
                    operation.reconcile_fd = None

    def test_container_inspection_runs_under_exclusive_create_lease(self):
        token = "a" * 32
        args = SimpleNamespace(container_owner_token=token)
        side = corpus_mem.ContainerSide(args, "b" * 32)
        name = side.prefix("host1r1") + "0"
        state = {"exclusive": False, "released": False}

        class Operation:
            def finish(self, _timeout):
                return Completed(125, "", "create failed")

            def acquire_reconciliation(self, _timeout):
                state["exclusive"] = True

            def release(self):
                state["released"] = True

        side.owned.add(name)
        side.create_operations[name] = Operation()

        def inspect(_name):
            self.assertTrue(state["exclusive"])
            return None

        with mock.patch.object(corpus_mem, "inspected_container_identity", inspect):
            side.stop_all()
        self.assertTrue(state["released"])
        self.assertNotIn(name, side.owned)

    def test_memory_uses_supervised_create_then_starts_only_the_owned_exact_id(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read()
        bring_up = source[source.index("    def bring_up(self, n, cell_tag, url_indices):",
                                       source.index("class ContainerSide")):
                          source.index("    def tear_down(self, live):",
                                       source.index("class ContainerSide"))]
        create = bring_up.find('["podman", "create"')
        reconcile = bring_up.find("inspected_container_identity(name)")
        start = bring_up.find('["podman", "start", "--", container_id]')
        self.assertGreaterEqual(create, 0, "memory still uses podman run -d")
        self.assertGreater(reconcile, create)
        self.assertGreater(start, reconcile,
                           "an unowned/unverified ID can be started")

    def test_finish_exception_cannot_lose_the_create_operation(self):
        args = SimpleNamespace(
            image="image", urls=["https://example.com/"],
            container_owner_token="a" * 32,
            container_create_ops_dir="unused",
        )
        side = corpus_mem.ContainerSide(args, "b" * 32)
        name = side.prefix("host1r1") + "0"

        class Operation:
            calls = 0
            acquired = False
            released = False

            def finish(self, _timeout):
                self.calls += 1
                if self.calls == 1:
                    raise RuntimeError("injected reap failure")
                return Completed(125, "", "injected create failure")

            def acquire_reconciliation(self, _timeout):
                self.acquired = True

            def release(self):
                self.released = True

        operation = Operation()
        with mock.patch.object(corpus_mem, "ContainerCreateOperation",
                               return_value=operation):
            with self.assertRaisesRegex(RuntimeError, "injected reap failure"):
                side.bring_up(1, "host1r1", [0])
        self.assertIs(side.create_operations.get(name), operation,
                      "finish failed before cleanup retained the operation")
        with mock.patch.object(corpus_mem, "inspected_container_identity",
                               return_value=None):
            side.stop_all()
        self.assertTrue(operation.acquired)
        self.assertTrue(operation.released)
        self.assertNotIn(name, side.owned)

    def test_matching_owner_with_a_different_exact_id_fails_cleanup(self):
        token = "a" * 32
        expected = "b" * 64
        actual = "c" * 64
        args = SimpleNamespace(container_owner_token=token,
                               container_create_ops_dir=None)
        side = corpus_mem.ContainerSide(args, "d" * 32)
        name = side.prefix("host1r1") + "0"
        side.owned.add(name)
        side.owned_ids[name] = expected
        with mock.patch.object(corpus_mem, "inspected_container_identity",
                               return_value=(actual, token)):
            with self.assertRaises(RuntimeError):
                side.stop_all()
        self.assertIn(name, side.owned,
                      "an ID mismatch discarded the only cleanup ownership record")

    def test_timed_out_create_is_quiesced_before_absence_can_clear_ownership(self):
        token = "a" * 32
        container_id = "b" * 64
        args = SimpleNamespace(container_owner_token=token,
                               container_create_ops_dir=None)
        side = corpus_mem.ContainerSide(args, "c" * 32)
        name = side.prefix("host1r1") + "0"
        side.owned.add(name)
        state = {"complete": False}

        class LateCreate:
            def finish(self, _timeout):
                state["complete"] = True
                return Completed(0, container_id + "\n", "")

            def acquire_reconciliation(self, _timeout):
                self.exclusive = True

            def release(self):
                self.released = True

        side.create_operations[name] = LateCreate()

        def identity(_name):
            return (container_id, token) if state["complete"] else None

        removed = []

        def bounded(cmd, _timeout):
            if cmd[:3] == ["podman", "rm", "-f"]:
                removed.append(cmd[-1])
                return Completed()
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(1, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "inspected_container_identity", identity), \
             mock.patch.object(corpus_mem, "sh_bounded", bounded):
            side.stop_all()
        self.assertTrue(state["complete"], "cleanup observed absence before create completed")
        self.assertEqual(removed, [container_id])
        self.assertNotIn(name, side.owned)

    def test_outer_cleanup_waits_for_create_locks_after_phase_quiescence(self):
        with open(EXTRA) as handle:
            source = handle.read()
        cleanup = self.shell_function(source, "cleanup")
        wait = self.shell_function(source, "wait_for_container_create_operations")
        self.assertLess(cleanup.find("stop_active_phase"),
                        cleanup.find("wait_for_container_create_operations"))
        self.assertLess(cleanup.find("wait_for_container_create_operations"),
                        cleanup.find("cleanup_owned_containers"))
        self.assertIn("flock -x", wait)
        self.assertNotIn("sleep", wait)
        self.assertIn('CONTAINER_CREATE_OPS_DIR="$RESULTS/container-create-ops"', source)
        self.assertIn('CONTAINER_CREATE_OPS_DIR="$CONTAINER_CREATE_OPS_DIR"', source)
        self.assertIn('--container-create-ops-dir "$CONTAINER_CREATE_OPS_DIR"', source)

    def test_outer_cleanup_refuses_rm_success_when_exact_id_survives(self):
        with open(EXTRA) as handle:
            source = handle.read()
        cleanup_owned = self.shell_function(source, "cleanup_owned_containers")
        run_id = "a" * 32
        token = "b" * 32
        container_id = "c" * 64
        name = f"cbmem-{run_id}-host1r1-0"
        with tempfile.TemporaryDirectory() as tmp:
            calls = os.path.join(tmp, "calls")
            podman = os.path.join(tmp, "podman")
            with open(podman, "w") as handle:
                handle.write(
                    "#!/bin/sh\n"
                    "printf '%s\\n' \"$*\" >>\"$CALLS\"\n"
                    "case \"$1 $2\" in\n"
                    f"  'ps -a') echo '{container_id} {name}' ;;\n"
                    f"  'inspect --format') echo '{container_id} {token}' ;;\n"
                    "  'rm -f') exit 0 ;;\n"
                    "  'container exists') exit 0 ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n"
                )
            os.chmod(podman, 0o755)
            script = (
                "set -uo pipefail\n"
                f"RUN_ID={run_id!r}\nCONTAINER_OWNER_TOKEN={token!r}\n"
                + cleanup_owned + "\ncleanup_owned_containers\n"
            )
            env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                       CALLS=calls)
            proc = subprocess.run(["bash", "-c", script], env=env,
                                  capture_output=True, text=True, timeout=30)
            self.assertNotEqual(proc.returncode, 0,
                                "rm exit 0 was mistaken for proof of absence")
            with open(calls) as handle:
                invocations = handle.read()
            self.assertIn(f"container exists {container_id}", invocations)

    def test_failed_phase_atomically_withdraws_and_unpublishes_summary(self):
        with open(EXTRA) as handle:
            source = handle.read()
        marker = self.shell_function(source, "mark_campaign_withdrawn")
        cleanup = self.shell_function(source, "cleanup")
        with tempfile.TemporaryDirectory() as tmp:
            summary = os.path.join(tmp, "summary.json")
            with open(summary, "w") as handle:
                handle.write("{}\n")
            script = (
                "set +e\n"
                f"RESULTS={tmp!r}\nDNSMASQ_WAS_ACTIVE=no\n"
                "stop_active_phase() { return 0; }\n"
                "verify_runtime_bundle() { return 0; }\n"
                "wait_for_container_create_operations() { return 0; }\n"
                "cleanup_owned_containers() { return 0; }\n"
                "stop_corpus_serve() { return 0; }\n"
                "require_corpus_serve_clean() { return 0; }\n"
                + marker + "\n" + cleanup + "\n"
                + "false\ncleanup\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=30)
            self.assertNotEqual(proc.returncode, 0)
            with open(os.path.join(tmp, "WITHDRAWN")) as handle:
                self.assertIn("phase exited", handle.readline())
            self.assertFalse(os.path.exists(summary),
                             "an earlier summary remained publishable after failure")
            self.assertEqual(
                [name for name in os.listdir(tmp) if name.startswith(".WITHDRAWN.")],
                [], "the atomic withdrawal left a temporary marker",
            )

    def test_cleanup_failure_withdraws_a_nominally_successful_phase(self):
        with open(EXTRA) as handle:
            source = handle.read()
        marker = self.shell_function(source, "mark_campaign_withdrawn")
        cleanup = self.shell_function(source, "cleanup")
        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "set +e\n"
                f"RESULTS={tmp!r}\nDNSMASQ_WAS_ACTIVE=no\n"
                "stop_active_phase() { return 0; }\n"
                "verify_runtime_bundle() { return 0; }\n"
                "wait_for_container_create_operations() { return 0; }\n"
                "cleanup_owned_containers() { return 1; }\n"
                "stop_corpus_serve() { return 0; }\n"
                "require_corpus_serve_clean() { return 0; }\n"
                + marker + "\n" + cleanup + "\ncleanup\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=30)
            self.assertNotEqual(proc.returncode, 0)
            self.assertTrue(os.path.isfile(os.path.join(tmp, "WITHDRAWN")))

    def test_memory_completion_binds_exact_final_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_id = "a" * 32
            payloads = {
                "run.json": json.dumps({"run_id": run_id}).encode() + b"\n",
                "samples.jsonl": b'{"sample":1}\n{"sample":2}\n',
                "summary.json": json.dumps(
                    {"run_id": run_id, "result": 3}
                ).encode() + b"\n",
            }
            for name, payload in payloads.items():
                with open(os.path.join(tmp, name), "wb") as handle:
                    handle.write(payload)

            corpus_mem.publish_completion(tmp, run_id)

            with open(os.path.join(tmp, "complete.json")) as handle:
                completion = json.load(handle)
            self.assertEqual(set(completion), {
                "schema_version", "run_id", "artifacts",
            })
            self.assertEqual(completion["schema_version"], 1)
            self.assertEqual(completion["run_id"], run_id)
            self.assertEqual(completion["artifacts"], [
                {
                    "path": name,
                    "size": len(payloads[name]),
                    "sha256": hashlib.sha256(payloads[name]).hexdigest(),
                }
                for name in sorted(payloads)
            ])
            self.assertFalse(any(
                name.startswith(".complete.") for name in os.listdir(tmp)
            ))

    def test_memory_completion_is_published_after_final_summary(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read()
        main = source[source.index("def main_with_resources(resources):") :]
        summary = main.find(
            'with open(os.path.join(args.results, "summary.json"), "w")'
        )
        self.assertGreaterEqual(summary, 0, "memory has no final summary writer")
        bootstrap = source[source.index("def bootstrap_memory_lifecycle"):
                           source.index("def main_with_resources(resources):")]
        lifecycle = bootstrap.find("status = run_memory_lifecycle(")
        publish = bootstrap.find("publish_completion(results, run_id)")
        self.assertGreaterEqual(
            lifecycle, 0, "memory lifecycle call is absent")
        self.assertGreater(
            publish, lifecycle,
            "memory completion precedes the worker and finalizer completion",
        )

    def test_memory_completion_order_rejects_an_absent_lifecycle_call(self):
        with open(CORPUS_MEM) as handle:
            source = handle.read().replace(
                "status = run_memory_lifecycle(",
                "status = missing_memory_lifecycle(",
                1,
            )
        with mock.patch("builtins.open", mock.mock_open(read_data=source)):
            with self.assertRaisesRegex(
                    AssertionError, "memory lifecycle call is absent"):
                self.test_memory_completion_is_published_after_final_summary()

    def test_memory_completion_does_not_retain_samples_payload(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_id = "a" * 32
            payloads = {
                "run.json": json.dumps({"run_id": run_id}).encode(),
                "samples.jsonl": b'{"sample":1}\n',
                "summary.json": json.dumps({"run_id": run_id}).encode(),
            }
            for name, payload in payloads.items():
                with open(os.path.join(tmp, name), "wb") as handle:
                    handle.write(payload)
            original = corpus_mem.read_memory_artifact
            captures = {}

            def record_capture(directory_fd, name, *args, **kwargs):
                captures[name] = kwargs.get("capture", False)
                return original(directory_fd, name, *args, **kwargs)

            with mock.patch.object(
                    corpus_mem, "read_memory_artifact",
                    side_effect=record_capture):
                corpus_mem.publish_completion(tmp, run_id)
            self.assertEqual(captures, {
                "run.json": True,
                "samples.jsonl": False,
                "summary.json": True,
            })

    def test_memory_completion_rechecks_artifacts_before_publication(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_id = "a" * 32
            paths = {
                "run.json": json.dumps({"run_id": run_id}).encode(),
                "samples.jsonl": b'{"sample":1}\n',
                "summary.json": json.dumps({"run_id": run_id}).encode(),
            }
            for name, payload in paths.items():
                with open(os.path.join(tmp, name), "wb") as handle:
                    handle.write(payload)
            samples = os.path.join(tmp, "samples.jsonl")
            real_fsync = os.fsync
            changed = False

            def change_after_completion_flush(fd):
                nonlocal changed
                try:
                    target = os.path.basename(os.readlink(f"/proc/self/fd/{fd}"))
                except OSError:
                    target = ""
                if not changed and target.startswith(".complete."):
                    with open(samples, "ab") as handle:
                        handle.write(b'{"sample":2}\n')
                    changed = True
                return real_fsync(fd)

            with mock.patch.object(
                    os, "fsync", side_effect=change_after_completion_flush):
                with self.assertRaises(
                        RuntimeError,
                        msg="changed samples were committed by complete.json"):
                    corpus_mem.publish_completion(tmp, run_id)
            self.assertTrue(changed, "the artifact mutation was not injected")
            self.assertFalse(os.path.exists(os.path.join(tmp, "complete.json")))
            self.assertFalse(any(
                name.startswith(".complete.") for name in os.listdir(tmp)
            ))

    def test_campaign_completion_binds_every_requested_host_arm(self):
        with open(EXTRA) as handle:
            source = handle.read()
        start = source.index("publish_campaign_completion() {")
        end = source.index("\ncleanup() {", start)
        publisher = source[start:end]
        with tempfile.TemporaryDirectory() as tmp:
            expected = []
            for arm, payload in (("free", b"free-complete\n"),
                                 ("cpu2", b"cpu2-complete\n")):
                directory = os.path.join(tmp, f"hostcdp-{arm}")
                os.mkdir(directory)
                path = os.path.join(directory, "complete.json")
                with open(path, "wb") as handle:
                    handle.write(payload)
                expected.append({
                    "path": f"hostcdp-{arm}/complete.json",
                    "size": len(payload),
                    "sha256": hashlib.sha256(payload).hexdigest(),
                })
            memory_directory = os.path.join(tmp, "memory")
            os.mkdir(memory_directory)
            memory_payload = self.memory_completion_bytes("a" * 32)
            with open(os.path.join(memory_directory, "complete.json"), "wb") as handle:
                handle.write(memory_payload)
            script = (
                "set -euo pipefail\n"
                f"RESULTS={tmp!r}\nRUN_ID={'a' * 32!r}\n"
                f"CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256={'b' * 64!r}\n"
                "PHASES=hostcdp,memory\nHOSTCDP_ARMS=free,cpu2\n"
                + publisher + "\npublish_campaign_completion\n"
            )
            subprocess.run(["bash", "-c", script], check=True,
                           capture_output=True, text=True, timeout=30)
            with open(os.path.join(tmp, "campaign-complete.json")) as handle:
                completion = json.load(handle)
            self.assertEqual(completion["schema_version"], 2)
            self.assertEqual(set(completion), {
                "schema_version", "run_id", "runtime_bundle_sha256",
                "phases", "host_completes", "memory_complete",
            })
            self.assertEqual(completion["run_id"], "a" * 32)
            self.assertEqual(completion["runtime_bundle_sha256"], "b" * 64)
            self.assertEqual(completion["phases"], ["hostcdp", "memory"])
            self.assertEqual(completion["host_completes"],
                             sorted(expected, key=lambda item: item["path"]))
            self.assertEqual(completion["memory_complete"], {
                "path": "memory/complete.json",
                "size": len(memory_payload),
                "sha256": hashlib.sha256(memory_payload).hexdigest(),
            })
            self.assertFalse(any(name.startswith(".campaign-complete.")
                                 for name in os.listdir(tmp)))

    def test_memory_only_campaign_requires_and_binds_memory_completion(self):
        with open(EXTRA) as handle:
            source = handle.read()
        start = source.index("publish_campaign_completion() {")
        end = source.index("\ncleanup() {", start)
        publisher = source[start:end]

        with tempfile.TemporaryDirectory() as missing:
            script = (
                "set -euo pipefail\n"
                f"RESULTS={missing!r}\nRUN_ID={'a' * 32!r}\n"
                f"CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256={'b' * 64!r}\n"
                "PHASES=memory\nHOSTCDP_ARMS=free,cpu2\n"
                + publisher + "\npublish_campaign_completion\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True, timeout=30
            )
            self.assertNotEqual(
                proc.returncode, 0,
                "memory-only campaign authorized zero completion artifacts",
            )
            self.assertFalse(os.path.exists(
                os.path.join(missing, "campaign-complete.json")
            ))

        with tempfile.TemporaryDirectory() as tmp:
            memory = os.path.join(tmp, "memory")
            os.mkdir(memory)
            payload = self.memory_completion_bytes("a" * 32)
            with open(os.path.join(memory, "complete.json"), "wb") as handle:
                handle.write(payload)
            script = (
                "set -euo pipefail\n"
                f"RESULTS={tmp!r}\nRUN_ID={'a' * 32!r}\n"
                f"CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256={'b' * 64!r}\n"
                "PHASES=memory\nHOSTCDP_ARMS=free,cpu2\n"
                + publisher + "\npublish_campaign_completion\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True, timeout=30
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(tmp, "campaign-complete.json")) as handle:
                completion = json.load(handle)
            self.assertEqual(completion["schema_version"], 2)
            self.assertEqual(completion["phases"], ["memory"])
            self.assertEqual(completion["host_completes"], [])
            self.assertEqual(completion["memory_complete"], {
                "path": "memory/complete.json",
                "size": len(payload),
                "sha256": hashlib.sha256(payload).hexdigest(),
            })

    def test_campaign_rejects_invalid_memory_completion(self):
        with open(EXTRA) as handle:
            source = handle.read()
        start = source.index("publish_campaign_completion() {")
        end = source.index("\ncleanup() {", start)
        publisher = source[start:end]
        mutations = {
            "old schema": lambda record: record.update(schema_version=0),
            "boolean schema": lambda record: record.update(schema_version=True),
            "wrong run": lambda record: record.update(run_id="c" * 32),
            "missing artifact": lambda record: record["artifacts"].pop(),
            "unsorted artifacts": lambda record: record["artifacts"].reverse(),
            "unexpected field": lambda record: record.update(extra=True),
            "invalid size": lambda record:
                record["artifacts"][0].update(size=True),
            "invalid digest": lambda record:
                record["artifacts"][0].update(sha256="A" * 64),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as tmp:
                memory = os.path.join(tmp, "memory")
                os.mkdir(memory)
                record = self.memory_completion_record("a" * 32)
                mutate(record)
                with open(os.path.join(memory, "complete.json"), "w") as handle:
                    json.dump(record, handle)
                script = (
                    "set -euo pipefail\n"
                    f"RESULTS={tmp!r}\nRUN_ID={'a' * 32!r}\n"
                    f"CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256={'b' * 64!r}\n"
                    "PHASES=memory\nHOSTCDP_ARMS=free,cpu2\n"
                    + publisher + "\npublish_campaign_completion\n"
                )
                proc = subprocess.run(
                    ["bash", "-c", script], capture_output=True,
                    text=True, timeout=30,
                )
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"campaign accepted memory completion with {label}",
                )
                self.assertFalse(os.path.exists(
                    os.path.join(tmp, "campaign-complete.json")
                ))

    def test_campaign_rechecks_memory_completion_before_publication(self):
        with open(EXTRA) as handle:
            source = handle.read()
        start = source.index("publish_campaign_completion() {")
        end = source.index("\ncleanup() {", start)
        publisher = source[start:end]
        with tempfile.TemporaryDirectory() as tmp, \
                tempfile.TemporaryDirectory() as hook_dir:
            memory = os.path.join(tmp, "memory")
            os.mkdir(memory)
            completion_path = os.path.join(memory, "complete.json")
            with open(completion_path, "wb") as handle:
                handle.write(self.memory_completion_bytes("a" * 32))
            hook = f'''\
import os

_real_fsync = os.fsync
_changed = False

def _fsync(fd):
    global _changed
    try:
        target = os.path.basename(os.readlink(f"/proc/self/fd/{{fd}}"))
    except OSError:
        target = ""
    if not _changed and target.startswith(".campaign-complete."):
        with open({completion_path!r}, "ab") as handle:
            handle.write(b"changed after load\\n")
        _changed = True
    return _real_fsync(fd)

os.fsync = _fsync
'''
            with open(os.path.join(hook_dir, "sitecustomize.py"), "w") as handle:
                handle.write(hook)
            script = (
                "set -euo pipefail\n"
                f"RESULTS={tmp!r}\nRUN_ID={'a' * 32!r}\n"
                f"CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256={'b' * 64!r}\n"
                "PHASES=memory\nHOSTCDP_ARMS=free,cpu2\n"
                + publisher + "\npublish_campaign_completion\n"
            )
            env = os.environ.copy()
            env["PYTHONPATH"] = hook_dir
            proc = subprocess.run(
                ["bash", "-c", script], env=env,
                capture_output=True, text=True, timeout=30,
            )
            self.assertNotEqual(
                proc.returncode, 0,
                "changed memory completion was published into campaign completion",
            )
            self.assertFalse(os.path.exists(
                os.path.join(tmp, "campaign-complete.json")
            ))

    def test_only_successful_cleanup_publishes_campaign_completion(self):
        with open(EXTRA) as handle:
            source = handle.read()
        cleanup = self.shell_function(source, "cleanup")

        def run(cleanup_result, withdrawal_result):
            with tempfile.TemporaryDirectory() as tmp:
                script = (
                    "set +e\n"
                    f"RESULTS={tmp!r}\nDNSMASQ_WAS_ACTIVE=no\n"
                    "stop_active_phase() { return 0; }\n"
                    "verify_runtime_bundle() { return 0; }\n"
                    "wait_for_container_create_operations() { return 0; }\n"
                    f"cleanup_owned_containers() {{ return {cleanup_result}; }}\n"
                    "stop_corpus_serve() { return 0; }\n"
                    "require_corpus_serve_clean() { return 0; }\n"
                    f"mark_campaign_withdrawn() {{ return {withdrawal_result}; }}\n"
                    "publish_campaign_completion() { : > \"$RESULTS/campaign-complete.json\"; }\n"
                    + cleanup + "\ncleanup\n"
                )
                proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                      text=True, timeout=30)
                return proc.returncode, os.path.exists(
                    os.path.join(tmp, "campaign-complete.json"))

        self.assertEqual(run(0, 0), (0, True))
        self.assertEqual(
            run(1, 1), (1, False),
            "failed cleanup became publishable when WITHDRAWN could not be written",
        )

    def test_late_completion_failure_removes_the_visible_commit(self):
        with open(EXTRA) as handle:
            source = handle.read()
        marker = self.shell_function(source, "mark_campaign_withdrawn")
        cleanup = self.shell_function(source, "cleanup")
        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "set +e\n"
                f"RESULTS={tmp!r}\nDNSMASQ_WAS_ACTIVE=no\n"
                "stop_active_phase() { return 0; }\n"
                "verify_runtime_bundle() { return 0; }\n"
                "wait_for_container_create_operations() { return 0; }\n"
                "cleanup_owned_containers() { return 0; }\n"
                "stop_corpus_serve() { return 0; }\n"
                "require_corpus_serve_clean() { return 0; }\n"
                "publish_campaign_completion() {\n"
                "  printf '{\"schema_version\":1}\n' >"
                "\"$RESULTS/campaign-complete.json\"\n"
                "  return 1\n"
                "}\n"
                + marker + "\n" + cleanup + "\ncleanup\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=30)
            self.assertNotEqual(proc.returncode, 0)
            self.assertFalse(os.path.exists(
                os.path.join(tmp, "campaign-complete.json")),
                "a failed late publication left an authorizing record",
            )
            self.assertTrue(os.path.isfile(os.path.join(tmp, "WITHDRAWN")))

    def test_phase_supervisor_reaps_a_descendant_after_its_leader_exits(self):
        with tempfile.TemporaryDirectory() as tmp:
            child_file = os.path.join(tmp, "child")
            proc = subprocess.run(
                [sys.executable, PHASE_SUPERVISOR,
                 "--expected-parent", str(os.getpid()), "--", "sh", "-c",
                 f"sleep 60 & echo $! > {child_file!r}"],
                capture_output=True, text=True, timeout=20,
            )
            self.assertEqual(
                proc.returncode, 1,
                "internal descendant cleanup was misreported as an operator signal",
            )
            with open(child_file) as handle:
                child = int(handle.read())
            self.assertFalse(os.path.exists(f"/proc/{child}"),
                             "the supervised phase left a descendant alive")

    def test_phase_supervisor_drains_a_descendant_that_escaped_the_leader_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            child_file = os.path.join(tmp, "escaped-child")
            leader = (
                "import os,subprocess,sys; "
                "child=subprocess.Popen([sys.executable, '-c', "
                "'import time; time.sleep(60)'], start_new_session=True, "
                "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); "
                f"open({child_file!r}, 'w').write(str(child.pid))"
            )
            proc = subprocess.run(
                [sys.executable, PHASE_SUPERVISOR,
                 "--expected-parent", str(os.getpid()), "--",
                 sys.executable, "-c", leader],
                capture_output=True, text=True, timeout=20,
            )
            with open(child_file) as handle:
                child = int(handle.read())

            try:
                self.assertEqual(
                    proc.returncode, 1,
                    "an escaped descendant was omitted from phase integrity",
                )
                self.assertIn(proc_state(child), (None, "Z"),
                              "the escaped descendant survived supervision")
            finally:
                if proc_state(child) not in (None, "Z"):
                    pidfd = os.pidfd_open(child)
                    try:
                        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        poller = select.poll()
                        poller.register(pidfd, select.POLLIN)
                        poller.poll(5000)
                    finally:
                        os.close(pidfd)

    def test_phase_supervisor_rechecks_after_adoption_moves_during_proc_scan(self):
        identity = {
            "pid": 4242,
            "state": "S",
            "ppid": os.getpid(),
            "pgid": 4242,
            "starttime": 99,
        }

        class Selector:
            @staticmethod
            def select(_timeout=None):
                return []

        scans = iter(([], [identity]))
        child_set = iter((None, None, ChildProcessError()))
        sent = []

        def waitid(*_args):
            result = next(child_set)
            if isinstance(result, BaseException):
                raise result
            return result

        with mock.patch.object(
                phase_supervisor, "direct_live_children",
                side_effect=lambda _parent: next(scans)), \
             mock.patch.object(phase_supervisor.os, "waitid",
                               side_effect=waitid), \
             mock.patch.object(
                 phase_supervisor, "signal_direct_children",
                 side_effect=lambda children, parent, signum:
                 sent.append((children, parent, signum))):
            escaped, external_signal = phase_supervisor.drain_adopted_children(
                Selector(), -1, [], None, os.getpid(), 0.01, 0.02,
            )

        self.assertTrue(
            escaped,
            "a child adopted during the procfs scan escaped supervision",
        )
        self.assertIsNone(external_signal)
        self.assertEqual(sent, [([identity], os.getpid(), signal.SIGTERM)])

    def test_post_leader_drain_error_still_reaps_adopted_descendant(self):
        with tempfile.TemporaryDirectory() as tmp:
            child_file = os.path.join(tmp, "child")
            leader = (
                "import subprocess,sys; "
                "child=subprocess.Popen([sys.executable, '-c', "
                "'import time; time.sleep(60)'], start_new_session=True, "
                "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); "
                f"open({child_file!r}, 'w').write(str(child.pid))"
            )
            real_scan = phase_supervisor.direct_live_children
            scan_calls = 0

            def fail_once(parent_pid, proc_root="/proc"):
                nonlocal scan_calls
                scan_calls += 1
                if scan_calls == 1:
                    raise RuntimeError("injected descendant scan failure")
                return real_scan(parent_pid, proc_root)

            child = None
            try:
                with mock.patch.object(
                        phase_supervisor, "direct_live_children",
                        side_effect=fail_once):
                    with self.assertRaisesRegex(
                            RuntimeError, "injected descendant scan failure"):
                        phase_supervisor.supervise(
                            [sys.executable, "-c", leader], os.getppid(),
                            term_grace=0.05, kill_grace=1.0,
                        )
                with open(child_file) as handle:
                    child = int(handle.read())
                self.assertIn(
                    proc_state(child), (None, "Z"),
                    "a post-leader drain error left an adopted descendant alive",
                )
                self.assertGreaterEqual(
                    scan_calls, 2,
                    "the original drain error skipped emergency descendant cleanup",
                )
            finally:
                if child is None and os.path.isfile(child_file):
                    with open(child_file) as handle:
                        child = int(handle.read())
                if child is not None and proc_state(child) not in (None, "Z"):
                    pidfd = os.pidfd_open(child)
                    try:
                        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        poller = select.poll()
                        poller.register(pidfd, select.POLLIN)
                        poller.poll(5000)
                    finally:
                        os.close(pidfd)
                if child is not None:
                    try:
                        os.waitpid(child, 0)
                    except ChildProcessError:
                        pass

    def test_phase_supervisor_escalates_when_phase_leader_ignores_term(self):
        driver = (
            "import os,sys; "
            f"sys.path.insert(0, {HERE!r}); "
            "import phase_supervisor as supervisor; "
            "supervisor.GRACE_SECONDS=0.05; "
            "raise SystemExit(supervisor.supervise([sys.executable, '-c', "
            "'import os,signal,time; "
            "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "print(os.getpid(), flush=True); time.sleep(60)'], os.getppid()))"
        )
        supervisor = subprocess.Popen(
            [sys.executable, "-c", driver], stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )
        leader_pid = int(supervisor.stdout.readline())
        timed_out = False
        returncode = None
        try:
            supervisor.send_signal(signal.SIGTERM)
            try:
                supervisor.communicate(timeout=5)
                returncode = supervisor.returncode
            except subprocess.TimeoutExpired:
                timed_out = True
        finally:
            try:
                pidfd = os.pidfd_open(leader_pid)
            except ProcessLookupError:
                pidfd = None
            killed_supervisor = supervisor.poll() is None
            if killed_supervisor:
                supervisor.kill()
            if pidfd is not None:
                try:
                    try:
                        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    poller = select.poll()
                    poller.register(pidfd, select.POLLIN)
                    poller.poll(5000)
                finally:
                    os.close(pidfd)
            if killed_supervisor:
                supervisor.communicate(timeout=5)
        self.assertFalse(timed_out, "TERM left the phase supervisor waiting forever")
        self.assertEqual(returncode, 128 + signal.SIGTERM)

    def test_phase_supervisor_owns_the_normal_command_deadline(self):
        command = [
            sys.executable, "-c",
            "import signal,time; "
            "signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
        ]
        started = time.monotonic()
        result = phase_supervisor.supervise(
            command, os.getppid(), term_grace=0.05, kill_grace=1.0,
            command_timeout=0.05,
        )
        elapsed = time.monotonic() - started
        self.assertEqual(result, 124)
        self.assertLess(elapsed, 5)

    def test_phase_supervisor_restores_callers_process_controls(self):
        original_subreaper = phase_supervisor.get_process_control(
            phase_supervisor.PR_GET_CHILD_SUBREAPER)
        original_pdeathsig = phase_supervisor.get_process_control(
            phase_supervisor.PR_GET_PDEATHSIG)
        try:
            phase_supervisor.set_process_control(
                phase_supervisor.PR_SET_CHILD_SUBREAPER, 0)
            phase_supervisor.set_process_control(
                phase_supervisor.PR_SET_PDEATHSIG, 0)
            result = phase_supervisor.supervise(
                [sys.executable, "-c", "pass"], os.getppid(),
                term_grace=0.05, kill_grace=1.0,
            )
            self.assertEqual(result, 0)
            self.assertEqual(
                phase_supervisor.get_process_control(
                    phase_supervisor.PR_GET_CHILD_SUBREAPER),
                0,
                "supervise left its caller adopting unrelated descendants",
            )
            self.assertEqual(
                phase_supervisor.get_process_control(
                    phase_supervisor.PR_GET_PDEATHSIG),
                0,
                "supervise left its caller armed against its own parent",
            )
        finally:
            phase_supervisor.set_process_control(
                phase_supervisor.PR_SET_CHILD_SUBREAPER, original_subreaper)
            phase_supervisor.set_process_control(
                phase_supervisor.PR_SET_PDEATHSIG, original_pdeathsig)

    def test_phase_supervisor_control_is_open_before_the_phase_launches(self):
        with open(PHASE_SUPERVISOR) as handle:
            source = handle.read()
        with tempfile.TemporaryDirectory() as tmp:
            control = os.path.join(tmp, "control")
            phase_pid_path = os.path.join(tmp, "phase.pid")
            os.mkfifo(control)
            control_fd = os.open(control, os.O_RDWR | os.O_NONBLOCK)
            command = (
                "import os,signal,time; "
                "stop=lambda *_: (_ for _ in ()).throw(SystemExit(0)); "
                f"open({phase_pid_path!r}, 'w').write(str(os.getpid())); "
                "signal.signal(signal.SIGTERM, stop); time.sleep(60)"
            )
            proc = subprocess.Popen(
                [sys.executable, PHASE_SUPERVISOR,
                 "--expected-parent", str(os.getpid()),
                 "--control-path", control,
                 "--return-command-status-on-signal", "--",
                 sys.executable, "-c", command],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            phase_pid = None
            try:
                deadline = time.monotonic() + 5
                while not os.path.isfile(phase_pid_path):
                    if proc.poll() is not None:
                        stdout, stderr = proc.communicate()
                        self.fail(
                            f"supervisor exited before phase readiness: "
                            f"{proc.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail("controlled phase did not start")
                    time.sleep(0.01)
                with open(phase_pid_path) as handle:
                    phase_pid = int(handle.read())
                os.write(control_fd, b"T")
                stdout, stderr = proc.communicate(timeout=10)
                self.assertEqual(proc.returncode, 0, stdout + stderr)
                self.assertFalse(os.path.exists(f"/proc/{phase_pid}"))
                opened = source.index("open_control_path(control_path)")
                launched = source.index("subprocess.Popen(argv")
                self.assertLess(opened, launched)
            finally:
                os.close(control_fd)
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate(timeout=5)
                if phase_pid is not None and os.path.exists(f"/proc/{phase_pid}"):
                    try:
                        pidfd = os.pidfd_open(phase_pid)
                    except ProcessLookupError:
                        pidfd = None
                    if pidfd is not None:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                            poller = select.poll()
                            poller.register(pidfd, select.POLLIN)
                            poller.poll(5000)
                        finally:
                            os.close(pidfd)

    def test_phase_supervisor_bounds_post_kill_waits(self):
        signal_key = SimpleNamespace(data="signal")

        class Selector:
            def __init__(self, events):
                self.events = iter(events)
                self.timeouts = []

            def select(self, timeout=None):
                self.timeouts.append(timeout)
                return next(self.events)

        identity = {"pid": 4242, "state": "D", "ppid": os.getpid(),
                    "pgid": 4242, "starttime": 99}
        leader_selector = Selector(([(signal_key, 1)], [], []))
        with mock.patch.object(phase_supervisor, "drain"), \
             mock.patch.object(phase_supervisor.os, "killpg"), \
             mock.patch.object(phase_supervisor, "read_process_stat",
                               return_value=identity):
            with self.assertRaisesRegex(RuntimeError, "survived SIGKILL"):
                phase_supervisor.wait_for_phase_leader(
                    leader_selector, SimpleNamespace(pid=4242),
                    [signal.SIGTERM], -1, 0.01, 0.02,
                )
        self.assertEqual(len(leader_selector.timeouts), 3)
        self.assertIsNone(leader_selector.timeouts[0])
        self.assertGreater(leader_selector.timeouts[1], 0)
        self.assertGreater(leader_selector.timeouts[2], 0)

        descendant_selector = Selector(([], []))
        sent = []
        with mock.patch.object(
                phase_supervisor, "direct_live_children",
                return_value=[identity]), \
             mock.patch.object(phase_supervisor, "direct_children_remain",
                               return_value=True), \
             mock.patch.object(
                 phase_supervisor, "signal_direct_children",
                 side_effect=lambda children, parent, signum: sent.append(signum)):
            with self.assertRaisesRegex(RuntimeError, "survived SIGKILL"):
                phase_supervisor.drain_adopted_children(
                    descendant_selector, -1, [], None, os.getpid(),
                    0.01, 0.02,
                )
        self.assertEqual(sent, [signal.SIGTERM, signal.SIGKILL])
        self.assertEqual(len(descendant_selector.timeouts), 2)
        self.assertGreater(descendant_selector.timeouts[0], 0)
        self.assertGreater(descendant_selector.timeouts[1], 0)

    def test_internal_supervisor_failure_cannot_leak_the_spawned_phase(self):
        real_popen = subprocess.Popen
        real_selector = phase_supervisor.selectors.DefaultSelector
        spawned = {}

        def capture_spawn(argv, **kwargs):
            kwargs["stdout"] = subprocess.PIPE
            kwargs["stderr"] = subprocess.PIPE
            kwargs["text"] = True
            proc = real_popen(argv, **kwargs)
            spawned["process"] = proc
            spawned["pid"] = int(proc.stdout.readline())
            return proc

        class BrokenSelector:
            def __init__(self):
                self.selector = real_selector()
                self.registrations = 0

            def register(self, *args, **kwargs):
                self.registrations += 1
                if self.registrations == 2:
                    raise RuntimeError("injected selector failure")
                return self.selector.register(*args, **kwargs)

            def close(self):
                self.selector.close()

        selectors_to_return = iter((BrokenSelector(), real_selector()))
        command = [
            sys.executable, "-c",
            "import os,signal,time; "
            "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "print(os.getpid(), flush=True); time.sleep(60)",
        ]
        try:
            with mock.patch.object(phase_supervisor.subprocess, "Popen",
                                   side_effect=capture_spawn), \
                 mock.patch.object(
                     phase_supervisor.selectors, "DefaultSelector",
                     side_effect=lambda: next(selectors_to_return)), \
                 mock.patch.object(phase_supervisor, "GRACE_SECONDS", 0.05), \
                 mock.patch.object(phase_supervisor, "KILL_REAP_SECONDS", 1.0):
                with self.assertRaisesRegex(RuntimeError,
                                            "injected selector failure"):
                    phase_supervisor.supervise(command, os.getppid())
            self.assertIsNotNone(spawned["process"].returncode,
                                 "internal failure left the phase unreaped")
            self.assertFalse(os.path.exists(f"/proc/{spawned['pid']}"),
                             "internal failure left the phase running")
        finally:
            proc = spawned.get("process")
            if proc is not None and proc.poll() is None:
                proc.kill()
                proc.communicate(timeout=5)
            elif proc is not None:
                if proc.stdout is not None:
                    proc.stdout.close()
                if proc.stderr is not None:
                    proc.stderr.close()

    def test_phase_supervisor_dies_with_parent_and_drains_its_phase(self):
        with open(PHASE_SUPERVISOR) as handle:
            supervisor_source = handle.read()
        armed = supervisor_source.index("arm_parent_death(expected_parent)")
        launched = supervisor_source.index("subprocess.Popen(argv")
        self.assertLess(armed, launched,
                        "the phase can launch before its parent-death guard is armed")
        with tempfile.TemporaryDirectory() as tmp:
            phase_pid_path = os.path.join(tmp, "phase.pid")
            parent_code = (
                "import os,signal,subprocess,sys\n"
                "phase = [sys.executable, '-c', "
                + repr("import os,signal,time; "
                       f"open({phase_pid_path!r}, 'w').write(str(os.getpid())); "
                       "signal.signal(signal.SIGTERM, lambda *_: raise_exit()); "
                       "time.sleep(60)")
                + "]\n"
                "supervisor = subprocess.Popen([sys.executable, "
                + repr(PHASE_SUPERVISOR)
                + ", '--expected-parent', str(os.getpid()), '--'] + phase)\n"
                "print(supervisor.pid, flush=True)\n"
                "signal.pause()\n"
            )
            # The phase's SIGTERM handler calls this name. SystemExit is used
            # instead of ignoring the signal so the parent-death path finishes
            # without waiting for its escalation deadline.
            parent_code = parent_code.replace(
                "import os,signal,time; ",
                "import os,signal,time; raise_exit=lambda: (_ for _ in ()).throw(SystemExit(143)); ",
            )
            parent = subprocess.Popen(
                [sys.executable, "-c", parent_code], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            supervisor_pid = int(parent.stdout.readline())
            deadline = time.monotonic() + 10
            while not (os.path.isfile(phase_pid_path)
                       and os.path.getsize(phase_pid_path) > 0):
                if time.monotonic() >= deadline:
                    self.fail("supervised phase never started")
                time.sleep(0.01)
            with open(phase_pid_path) as handle:
                phase_pid = int(handle.read())
            os.kill(parent.pid, signal.SIGKILL)
            parent.communicate(timeout=20)

            self.assertIn(proc_state(supervisor_pid), (None, "Z"))
            self.assertIn(proc_state(phase_pid), (None, "Z"),
                          "parent death left the measured phase alive")

    def test_phase_supervisor_finalizes_after_parent_sigkill(self):
        """A registered finalizer runs after the phase has been reaped.

        RED BEFORE THE FIX: phase_supervisor had no finalizer boundary, so a
        resource released only by the killed parent could not be restored.
        """
        with tempfile.TemporaryDirectory() as tmp:
            phase_pid_path = os.path.join(tmp, "phase.pid")
            ready_path = os.path.join(tmp, "phase.ready")
            state_path = os.path.join(tmp, "resource.state")
            finalizer = os.path.join(tmp, "finalizer")
            with open(finalizer, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "phase_pid=$(cat \"$PHASE_PID_PATH\")\n"
                    "[ ! -e \"/proc/$phase_pid\" ]\n"
                    "printf 'active\\n' >\"$RESOURCE_STATE_PATH\"\n"
                )
            os.chmod(finalizer, 0o755)
            phase_code = (
                "import os,signal,time; "
                "stop=lambda *_: (_ for _ in ()).throw(SystemExit(0)); "
                "signal.signal(signal.SIGTERM, stop); "
                f"open({phase_pid_path!r}, 'w').write(str(os.getpid())); "
                f"open({state_path!r}, 'w').write('inactive\\n'); "
                f"open({ready_path!r}, 'w').write('ready\\n'); "
                "time.sleep(60)"
            )
            parent_code = (
                "import os,signal,subprocess,sys\n"
                "command = [sys.executable, " + repr(PHASE_SUPERVISOR)
                + ", '--expected-parent', str(os.getpid()), '--finalizer', "
                + repr(finalizer)
                + ", '--', sys.executable, '-c', " + repr(phase_code) + "]\n"
                "supervisor = subprocess.Popen(command)\n"
                "print(supervisor.pid, flush=True)\n"
                "signal.pause()\n"
            )
            parent = subprocess.Popen(
                [sys.executable, "-c", parent_code],
                env=dict(
                    os.environ,
                    PHASE_PID_PATH=phase_pid_path,
                    RESOURCE_STATE_PATH=state_path,
                ),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            supervisor_pid = int(parent.stdout.readline())
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(ready_path):
                    if parent.poll() is not None:
                        stdout, stderr = parent.communicate()
                        self.fail(
                            "finalized phase never started: " + stdout + stderr)
                    if time.monotonic() >= deadline:
                        self.fail("finalized phase never became ready")
                    time.sleep(0.01)
                with open(phase_pid_path) as handle:
                    phase_pid = int(handle.read())

                os.kill(parent.pid, signal.SIGKILL)
                parent.communicate(timeout=20)

                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "active\n")
                self.assertIn(proc_state(supervisor_pid), (None, "Z"))
                self.assertIn(proc_state(phase_pid), (None, "Z"))
            finally:
                if parent.poll() is None:
                    parent.kill()
                    parent.communicate(timeout=5)

    def test_parent_death_after_finalizer_start_does_not_interrupt_cleanup(self):
        """The completed phase no longer ties cleanup to its former parent.

        RED BEFORE THE FIX: the supervisor kept its parent-death SIGTERM armed
        while supervising the finalizer, so killing the expected parent after
        finalizer readiness killed the finalizer before resource restoration.
        """
        with tempfile.TemporaryDirectory() as tmp:
            ready_path = os.path.join(tmp, "finalizer.ready")
            release_path = os.path.join(tmp, "finalizer.release")
            state_path = os.path.join(tmp, "resource.state")
            finalizer_pid_path = os.path.join(tmp, "finalizer.pid")
            finalizer = os.path.join(tmp, "finalizer")
            os.mkfifo(release_path)
            release_fd = os.open(
                release_path, os.O_RDWR | os.O_NONBLOCK | os.O_CLOEXEC)
            with open(state_path, "w") as handle:
                handle.write("inactive\n")
            with open(finalizer, "w") as handle:
                handle.write(
                    "#!/usr/bin/env python3\n"
                    "import os\n"
                    "with open(os.environ['FINALIZER_PID_PATH'], 'w') as out:\n"
                    "    out.write(str(os.getpid()))\n"
                    "with open(os.environ['FINALIZER_READY_PATH'], 'w') as out:\n"
                    "    out.write('ready\\n')\n"
                    "with open(os.environ['FINALIZER_RELEASE_PATH'], 'rb', "
                    "buffering=0) as release:\n"
                    "    if release.read(1) != b'R':\n"
                    "        raise SystemExit(64)\n"
                    "with open(os.environ['RESOURCE_STATE_PATH'], 'w') as out:\n"
                    "    out.write('active\\n')\n"
                )
            os.chmod(finalizer, 0o755)
            parent_code = (
                "import os,signal,subprocess,sys\n"
                "supervisor = subprocess.Popen([sys.executable, "
                + repr(PHASE_SUPERVISOR)
                + ", '--expected-parent', str(os.getpid()), "
                "'--term-grace', '0', '--kill-grace', '1', "
                "'--finalizer-timeout', '10', '--finalizer', "
                + repr(finalizer)
                + ", '--', sys.executable, '-c', 'pass'])\n"
                "print(supervisor.pid, flush=True)\n"
                "signal.pause()\n"
            )
            parent = subprocess.Popen(
                [sys.executable, "-c", parent_code],
                env=dict(
                    os.environ,
                    FINALIZER_PID_PATH=finalizer_pid_path,
                    FINALIZER_READY_PATH=ready_path,
                    FINALIZER_RELEASE_PATH=release_path,
                    RESOURCE_STATE_PATH=state_path,
                ),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            supervisor_pid = int(parent.stdout.readline())
            supervisor_pidfd = os.pidfd_open(supervisor_pid)
            finalizer_pidfd = None
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(ready_path):
                    if parent.poll() is not None:
                        self.fail("parent exited before finalizer readiness")
                    if time.monotonic() >= deadline:
                        self.fail("finalizer never became ready")
                    time.sleep(0.01)
                with open(finalizer_pid_path) as handle:
                    finalizer_pid = int(handle.read())
                finalizer_pidfd = os.pidfd_open(finalizer_pid)

                os.kill(parent.pid, signal.SIGKILL)
                parent.wait(timeout=5)
                poller = select.poll()
                poller.register(supervisor_pidfd, select.POLLIN)
                self.assertEqual(
                    poller.poll(1000), [],
                    "late parent death terminated the mandatory finalizer",
                )

                os.write(release_fd, b"R")
                self.assertTrue(
                    poller.poll(5000),
                    "supervisor did not finish after finalizer release",
                )
                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "active\n")
                self.assertIn(proc_state(finalizer_pid), (None, "Z"))
            finally:
                try:
                    os.write(release_fd, b"R")
                except OSError:
                    pass
                os.close(release_fd)
                if parent.poll() is None:
                    parent.kill()
                    parent.wait(timeout=5)
                if (finalizer_pidfd is not None
                        and proc_state(finalizer_pid) not in (None, "Z")):
                    signal.pidfd_send_signal(finalizer_pidfd, signal.SIGKILL)
                if proc_state(supervisor_pid) not in (None, "Z"):
                    signal.pidfd_send_signal(supervisor_pidfd, signal.SIGKILL)
                if finalizer_pidfd is not None:
                    os.close(finalizer_pidfd)
                os.close(supervisor_pidfd)
                for pipe in (parent.stdout, parent.stderr):
                    if pipe is not None:
                        pipe.close()

    def test_dnsmasq_is_restored_after_the_campaign_parent_is_sigkilled(self):
        """The real host finalizer closes the replay-server SIGKILL path."""
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            state_path = os.path.join(tmp, "dnsmasq.state")
            calls_path = os.path.join(tmp, "systemctl.calls")
            ready_path = os.path.join(tmp, "serve.ready")
            systemctl = os.path.join(bindir, "systemctl")
            with open(systemctl, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "printf '%s\\n' \"$*\" >>\"$SYSTEMCTL_CALLS\"\n"
                    "case \"$1\" in\n"
                    "  stop)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    printf 'inactive\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  start)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    printf 'active\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  is-active)\n"
                    "    [ \"$2\" = --quiet ]\n"
                    "    [ \"$3\" = dnsmasq ]\n"
                    "    grep -qx active \"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n"
                )
            os.chmod(systemctl, 0o755)
            with open(state_path, "w") as handle:
                handle.write("active\n")
            phase = (
                "systemctl stop dnsmasq && "
                f"printf ready >{ready_path!r} && "
                "exec sleep 60"
            )
            parent_code = (
                "import os,signal,subprocess,sys\n"
                "command = [sys.executable, " + repr(PHASE_SUPERVISOR)
                + ", '--expected-parent', str(os.getpid()), '--finalizer', "
                + repr(HOST_RESOURCE_FINALIZER)
                + ", '--finalizer-timeout', '10', '--', 'bash', '-c', "
                + repr(phase) + "]\n"
                "supervisor = subprocess.Popen(command)\n"
                "print(supervisor.pid, flush=True)\n"
                "signal.pause()\n"
            )
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                FCVM_FINALIZER_MODE="dnsmasq",
                FCVM_DNSMASQ_WAS_ACTIVE="yes",
                SYSTEMCTL_CALLS=calls_path,
                SYSTEMCTL_STATE=state_path,
            )
            parent = subprocess.Popen(
                [sys.executable, "-c", parent_code], env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            supervisor_pid = int(parent.stdout.readline())
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(ready_path):
                    if parent.poll() is not None:
                        stdout, stderr = parent.communicate()
                        self.fail(
                            "replay phase never stopped dnsmasq: "
                            + stdout + stderr)
                    if time.monotonic() >= deadline:
                        self.fail("replay phase never stopped dnsmasq")
                    time.sleep(0.01)

                os.kill(parent.pid, signal.SIGKILL)
                parent.communicate(timeout=20)
                deadline = time.monotonic() + 10
                state = None
                while time.monotonic() < deadline:
                    with open(state_path) as handle:
                        state = handle.read()
                    if state == "active\n" and proc_state(supervisor_pid) in (None, "Z"):
                        break
                    time.sleep(0.01)
                self.assertEqual(state, "active\n")
                self.assertIn(proc_state(supervisor_pid), (None, "Z"))
                with open(calls_path) as handle:
                    self.assertEqual(
                        handle.read().splitlines(),
                        ["stop dnsmasq", "start dnsmasq",
                         "is-active --quiet dnsmasq"],
                    )
            finally:
                if parent.poll() is None:
                    parent.kill()
                    parent.communicate(timeout=5)

    def test_replay_lease_survives_campaign_process_group_sigkill(self):
        """The host-wide lease covers the detached DNS finalizer.

        RED BEFORE THE FIX: the campaign shell and its background serve job
        were the only processes holding fd 9. Killing their process group
        released the lease while the surviving root finalizer was still
        restoring dnsmasq.
        """
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            lock_path = os.path.join(tmp, "corpus-extra.lock")
            control_path = os.path.join(tmp, "serve.control")
            guardian_ready = os.path.join(tmp, "guardian.ready")
            guardian_pid_path = os.path.join(tmp, "guardian.pid")
            serve_ready = os.path.join(tmp, "serve.ready")
            serve_pid_path = os.path.join(tmp, "serve.pid")
            supervisor_pgid_path = os.path.join(tmp, "supervisor.pgid")
            status_path = os.path.join(tmp, "serve.status")
            completion_path = os.path.join(tmp, "serve.completion")
            completion_token = "c" * 32
            state_path = os.path.join(tmp, "dnsmasq.state")
            restore_started = os.path.join(tmp, "restore.started")
            restore_release = os.path.join(tmp, "restore.release")
            log_path = os.path.join(tmp, "guardian.log")
            systemctl = os.path.join(bindir, "systemctl")
            supervisor_wrapper = os.path.join(tmp, "supervisor-wrapper")
            with open(lock_path, "w"):
                pass
            with open(completion_path, "w") as handle:
                handle.write("complete " + "0" * 32 + "\n")
            with open(state_path, "w") as handle:
                handle.write("active\n")
            with open(systemctl, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "case \"$1\" in\n"
                    "  stop)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    [ \"$(cat \"$SUPERVISOR_PGID_PATH\")\" "
                    "!= \"$CAMPAIGN_PGID\" ]\n"
                    "    printf 'inactive\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  start)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    : >\"$RESTORE_STARTED\"\n"
                    "    while [ ! -e \"$RESTORE_RELEASE\" ]; do sleep 0.01; done\n"
                    "    printf 'active\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  is-active)\n"
                    "    [ \"$2\" = --quiet ]\n"
                    "    [ \"$3\" = dnsmasq ]\n"
                    "    grep -qx active \"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n"
                )
            os.chmod(systemctl, 0o755)
            with open(supervisor_wrapper, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "python3 -c 'import os; print(os.getpgrp())' "
                    ">\"$SUPERVISOR_PGID_PATH\"\n"
                    "exec python3 \"$PHASE_SUPERVISOR\" "
                    "--expected-parent \"$PPID\" "
                    "--control-path \"$CONTROL_PATH\" "
                    "--return-command-status-on-signal "
                    "--completion-path \"$COMPLETION_PATH\" "
                    "--completion-token \"$COMPLETION_TOKEN\" "
                    "--finalizer \"$HOST_RESOURCE_FINALIZER\" "
                    "--finalizer-timeout 10 -- "
                    "bash -c 'printf \"%s\\n\" \"$$\" "
                    ">\"$SERVE_PID_PATH\" && systemctl stop dnsmasq && "
                    ": >\"$SERVE_READY\" && exec sleep 60'\n"
                )
            os.chmod(supervisor_wrapper, 0o755)
            launcher = (
                "set -euo pipefail\n"
                "export CAMPAIGN_PGID=$BASHPID\n"
                "exec 9<\"$LOCK_PATH\"\n"
                "flock -x 9\n"
                "mkfifo -- \"$CONTROL_PATH\"\n"
                "exec {control_fd}<>\"$CONTROL_PATH\"\n"
                "python3 \"$SERVE_GUARDIAN\" --lease-fd 9 "
                "--control-fd \"$control_fd\" "
                "--ready-path \"$GUARDIAN_READY\" "
                "--status-path \"$STATUS_PATH\" "
                "--completion-path \"$COMPLETION_PATH\" "
                "--completion-token \"$COMPLETION_TOKEN\" -- "
                "\"$SUPERVISOR_WRAPPER\" "
                ">\"$GUARDIAN_LOG\" 2>&1 &\n"
                "guardian=$!\n"
                "printf '%s\\n' \"$guardian\" >\"$GUARDIAN_PID_PATH\"\n"
                "for _ in $(seq 1 500); do\n"
                "  [ ! -e \"$GUARDIAN_READY\" ] || break\n"
                "  sleep 0.01\n"
                "done\n"
                "[ -e \"$GUARDIAN_READY\" ]\n"
                "wait \"$guardian\"\n"
            )
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                LOCK_PATH=lock_path,
                CONTROL_PATH=control_path,
                SERVE_GUARDIAN=SERVE_GUARDIAN,
                GUARDIAN_READY=guardian_ready,
                GUARDIAN_PID_PATH=guardian_pid_path,
                STATUS_PATH=status_path,
                COMPLETION_PATH=completion_path,
                COMPLETION_TOKEN=completion_token,
                GUARDIAN_LOG=log_path,
                PHASE_SUPERVISOR=PHASE_SUPERVISOR,
                HOST_RESOURCE_FINALIZER=HOST_RESOURCE_FINALIZER,
                SUPERVISOR_WRAPPER=supervisor_wrapper,
                SERVE_READY=serve_ready,
                SERVE_PID_PATH=serve_pid_path,
                SUPERVISOR_PGID_PATH=supervisor_pgid_path,
                FCVM_FINALIZER_MODE="dnsmasq",
                FCVM_DNSMASQ_WAS_ACTIVE="yes",
                SYSTEMCTL_STATE=state_path,
                RESTORE_STARTED=restore_started,
                RESTORE_RELEASE=restore_release,
            )
            campaign = subprocess.Popen(
                ["bash", "-c", launcher], env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            guardian_pid = None
            guardian_pidfd = None
            detached_ready = False

            def wait_for(path, description, timeout=10, campaign_must_live=True):
                deadline = time.monotonic() + timeout
                while not os.path.exists(path):
                    if campaign_must_live and campaign.poll() is not None:
                        stdout, stderr = campaign.communicate()
                        log = ""
                        if os.path.exists(log_path):
                            with open(log_path) as handle:
                                log = handle.read()
                        self.fail(
                            f"campaign exited before {description}: "
                            f"{campaign.returncode}: {stdout}{stderr}{log}")
                    if time.monotonic() >= deadline:
                        self.fail(f"timed out waiting for {description}")
                    time.sleep(0.01)

            try:
                wait_for(guardian_pid_path, "the guardian pid")
                with open(guardian_pid_path) as handle:
                    guardian_pid = int(handle.read())
                wait_for(guardian_ready, "the guardian to detach")
                detached_ready = True
                guardian_pidfd = os.pidfd_open(guardian_pid)
                wait_for(supervisor_pgid_path, "the root supervisor process group")
                wait_for(serve_pid_path, "the replay phase pid")
                wait_for(serve_ready, "the replay server to stop dnsmasq")
                with open(serve_pid_path) as handle:
                    serve_pid = int(handle.read())
                with open(supervisor_pgid_path) as handle:
                    supervisor_pgid = int(handle.read())
                self.assertEqual(os.getpgid(guardian_pid), guardian_pid)
                self.assertEqual(supervisor_pgid, guardian_pid)
                self.assertEqual(os.getpgid(serve_pid), serve_pid)
                self.assertNotEqual(os.getpgid(guardian_pid), campaign.pid)
                self.assertNotEqual(supervisor_pgid, campaign.pid)
                self.assertNotEqual(os.getpgid(serve_pid), campaign.pid)
                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "inactive\n")

                os.killpg(campaign.pid, signal.SIGKILL)
                campaign.communicate(timeout=5)
                self.assertNotEqual(
                    subprocess.run(
                        ["flock", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=5,
                    ).returncode,
                    0,
                    "campaign SIGKILL released the host lease before DNS restoration",
                )
                wait_for(
                    restore_started,
                    "the DNS finalizer to start",
                    campaign_must_live=False,
                )
                with open(restore_release, "w"):
                    pass
                wait_for(
                    status_path,
                    "the replay guardian status",
                    campaign_must_live=False,
                )
                poller = select.poll()
                poller.register(guardian_pidfd, select.POLLIN)
                self.assertTrue(
                    poller.poll(5000),
                    "the guardian retained the lease after its finalizer completed",
                )
                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "active\n")
                with open(status_path) as handle:
                    self.assertEqual(handle.read(), "143\n")
                with open(completion_path) as handle:
                    self.assertEqual(
                        handle.read(), f"complete {completion_token}\n")
                self.assertEqual(
                    subprocess.run(
                        ["flock", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=5,
                    ).returncode,
                    0,
                    "the host lease remained held after DNS restoration completed",
                )
            finally:
                with open(restore_release, "w"):
                    pass
                if campaign.poll() is None:
                    os.killpg(campaign.pid, signal.SIGKILL)
                    campaign.communicate(timeout=5)
                if (detached_ready and guardian_pid is not None
                        and proc_state(guardian_pid) not in (None, "Z")):
                    try:
                        os.killpg(guardian_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if guardian_pidfd is not None:
                    os.close(guardian_pidfd)

    def test_phase_supervisor_propagates_finalizer_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            completion_path = os.path.join(tmp, "completion")
            token = "d" * 32
            with self.assertRaisesRegex(RuntimeError, "phase finalizer exited 7"):
                phase_supervisor.supervise(
                    [sys.executable, "-c", "pass"],
                    os.getppid(),
                    finalizer=[sys.executable, "-c", "raise SystemExit(7)"],
                    finalizer_timeout=5,
                    completion_path=completion_path,
                    completion_token=token,
                )
            with open(completion_path) as handle:
                self.assertEqual(handle.read(), f"armed {token}\n")

    def test_completion_stays_armed_without_a_finalizer_drain_certificate(self):
        """A failed nested supervisor cannot certify an unknown process set."""
        with tempfile.TemporaryDirectory() as tmp:
            completion_path = os.path.join(tmp, "completion")
            token = "e" * 32
            supervise_armed = phase_supervisor._supervise_armed
            with mock.patch.object(
                    phase_supervisor, "_supervise_armed",
                    side_effect=RuntimeError(
                        "finalizer supervision lost its process tree")):
                with self.assertRaisesRegex(RuntimeError, "lost its process tree"):
                    supervise_armed(
                        [sys.executable, "-c", "pass"],
                        finalizer=[sys.executable, "-c", "pass"],
                        finalizer_timeout=5,
                        completion_path=completion_path,
                        completion_token=token,
                    )
            with open(completion_path) as handle:
                self.assertEqual(handle.read(), f"armed {token}\n")

    def test_replay_lease_waits_for_completion_after_sudo_parent_sigkill(self):
        """A dead sudo parent cannot release the lease ahead of finalization.

        RED BEFORE THE FIX: the guardian waited only for its direct sudo child,
        so killing that child released fd 9 while the orphaned root supervisor
        was still blocked in the DNS finalizer.
        """
        with tempfile.TemporaryDirectory() as tmp:
            bindir = os.path.join(tmp, "bin")
            os.makedirs(bindir)
            lock_path = os.path.join(tmp, "corpus-extra.lock")
            control_path = os.path.join(tmp, "serve.control")
            guardian_ready = os.path.join(tmp, "guardian.ready")
            guardian_pid_path = os.path.join(tmp, "guardian.pid")
            sudo_pid_path = os.path.join(tmp, "sudo.pid")
            supervisor_pid_path = os.path.join(tmp, "supervisor.pid")
            serve_ready = os.path.join(tmp, "serve.ready")
            status_path = os.path.join(tmp, "serve.status")
            completion_path = os.path.join(tmp, "serve.completion")
            completion_token = "a" * 32
            state_path = os.path.join(tmp, "dnsmasq.state")
            restore_started = os.path.join(tmp, "restore.started")
            restore_release = os.path.join(tmp, "restore.release")
            log_path = os.path.join(tmp, "guardian.log")
            systemctl = os.path.join(bindir, "systemctl")
            sudo_parent = os.path.join(tmp, "sudo-parent")
            with open(lock_path, "w"):
                pass
            with open(state_path, "w") as handle:
                handle.write("active\n")
            with open(systemctl, "w") as handle:
                handle.write(
                    "#!/bin/bash\n"
                    "set -eu\n"
                    "case \"$1\" in\n"
                    "  stop)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    printf 'inactive\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  start)\n"
                    "    [ \"$2\" = dnsmasq ]\n"
                    "    : >\"$RESTORE_STARTED\"\n"
                    "    while [ ! -e \"$RESTORE_RELEASE\" ]; do sleep 0.01; done\n"
                    "    printf 'active\\n' >\"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  is-active)\n"
                    "    [ \"$2\" = --quiet ]\n"
                    "    [ \"$3\" = dnsmasq ]\n"
                    "    grep -qx active \"$SYSTEMCTL_STATE\"\n"
                    "    ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n"
                )
            os.chmod(systemctl, 0o755)
            with open(sudo_parent, "w") as handle:
                handle.write(
                    "#!/usr/bin/env python3\n"
                    "import os, subprocess, sys\n"
                    "with open(os.environ['SUDO_PID_PATH'], 'w') as out:\n"
                    "    out.write(str(os.getpid()))\n"
                    "phase = (\"systemctl stop dnsmasq && \"\n"
                    "         \": >\\\"$SERVE_READY\\\" && exec sleep 60\")\n"
                    "command = [sys.executable, os.environ['PHASE_SUPERVISOR'],\n"
                    "           '--expected-parent', str(os.getpid()),\n"
                    "           '--term-grace', '0', '--kill-grace', '1',\n"
                    "           '--completion-path', os.environ['COMPLETION_PATH'],\n"
                    "           '--completion-token', os.environ['COMPLETION_TOKEN'],\n"
                    "           '--finalizer', os.environ['HOST_RESOURCE_FINALIZER'],\n"
                    "           '--finalizer-timeout', '10', '--',\n"
                    "           'bash', '-c', phase]\n"
                    "supervisor = subprocess.Popen(command)\n"
                    "with open(os.environ['SUPERVISOR_PID_PATH'], 'w') as out:\n"
                    "    out.write(str(supervisor.pid))\n"
                    "raise SystemExit(supervisor.wait())\n"
                )
            os.chmod(sudo_parent, 0o755)
            launcher = (
                "set -euo pipefail\n"
                "exec 9<\"$LOCK_PATH\"\n"
                "flock -x 9\n"
                "mkfifo -- \"$CONTROL_PATH\"\n"
                "exec {control_fd}<>\"$CONTROL_PATH\"\n"
                "python3 \"$SERVE_GUARDIAN\" --lease-fd 9 "
                "--control-fd \"$control_fd\" "
                "--ready-path \"$GUARDIAN_READY\" "
                "--status-path \"$STATUS_PATH\" "
                "--completion-path \"$COMPLETION_PATH\" "
                "--completion-token \"$COMPLETION_TOKEN\" -- "
                "\"$SUDO_PARENT\" >\"$GUARDIAN_LOG\" 2>&1 &\n"
                "guardian=$!\n"
                "printf '%s\\n' \"$guardian\" >\"$GUARDIAN_PID_PATH\"\n"
                "for _ in $(seq 1 500); do\n"
                "  [ ! -e \"$GUARDIAN_READY\" ] || break\n"
                "  sleep 0.01\n"
                "done\n"
                "[ -e \"$GUARDIAN_READY\" ]\n"
                "exec 9>&-\n"
                "wait \"$guardian\"\n"
            )
            env = dict(
                os.environ,
                PATH=bindir + os.pathsep + os.environ["PATH"],
                LOCK_PATH=lock_path,
                CONTROL_PATH=control_path,
                SERVE_GUARDIAN=SERVE_GUARDIAN,
                GUARDIAN_READY=guardian_ready,
                GUARDIAN_PID_PATH=guardian_pid_path,
                STATUS_PATH=status_path,
                COMPLETION_PATH=completion_path,
                COMPLETION_TOKEN=completion_token,
                GUARDIAN_LOG=log_path,
                SUDO_PARENT=sudo_parent,
                SUDO_PID_PATH=sudo_pid_path,
                SUPERVISOR_PID_PATH=supervisor_pid_path,
                PHASE_SUPERVISOR=PHASE_SUPERVISOR,
                HOST_RESOURCE_FINALIZER=HOST_RESOURCE_FINALIZER,
                SERVE_READY=serve_ready,
                FCVM_FINALIZER_MODE="dnsmasq",
                FCVM_DNSMASQ_WAS_ACTIVE="yes",
                SYSTEMCTL_STATE=state_path,
                RESTORE_STARTED=restore_started,
                RESTORE_RELEASE=restore_release,
            )
            campaign = subprocess.Popen(
                ["bash", "-c", launcher], env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            guardian_pid = None
            sudo_pid = None
            supervisor_pid = None

            def wait_for(path, description, timeout=10):
                deadline = time.monotonic() + timeout
                while not os.path.exists(path):
                    if campaign.poll() is not None:
                        stdout, stderr = campaign.communicate()
                        log = ""
                        if os.path.exists(log_path):
                            with open(log_path) as handle:
                                log = handle.read()
                        self.fail(
                            f"campaign exited before {description}: "
                            f"{campaign.returncode}: {stdout}{stderr}{log}")
                    if time.monotonic() >= deadline:
                        self.fail(f"timed out waiting for {description}")
                    time.sleep(0.01)

            try:
                wait_for(guardian_pid_path, "the guardian pid")
                wait_for(guardian_ready, "the guardian readiness record")
                wait_for(sudo_pid_path, "the sudo parent pid")
                wait_for(supervisor_pid_path, "the root supervisor pid")
                wait_for(serve_ready, "the DNS stop")
                with open(guardian_pid_path) as handle:
                    guardian_pid = int(handle.read())
                with open(sudo_pid_path) as handle:
                    sudo_pid = int(handle.read())
                with open(supervisor_pid_path) as handle:
                    supervisor_pid = int(handle.read())
                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "inactive\n")
                with open(completion_path) as handle:
                    self.assertEqual(
                        handle.read(), f"armed {completion_token}\n")

                os.kill(sudo_pid, signal.SIGKILL)
                wait_for(restore_started, "the blocked DNS finalizer")
                self.assertIsNone(campaign.poll())
                self.assertNotIn(proc_state(guardian_pid), (None, "Z"))
                self.assertNotIn(proc_state(supervisor_pid), (None, "Z"))
                self.assertNotEqual(
                    subprocess.run(
                        ["flock", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=5,
                    ).returncode,
                    0,
                    "killed sudo released the lease before finalizer completion",
                )

                with open(restore_release, "w"):
                    pass
                deadline = time.monotonic() + 10
                completion = None
                while time.monotonic() < deadline:
                    try:
                        with open(completion_path) as handle:
                            completion = handle.read()
                    except FileNotFoundError:
                        pass
                    if completion == f"complete {completion_token}\n":
                        break
                    time.sleep(0.01)
                self.assertEqual(
                    completion, f"complete {completion_token}\n",
                    "root supervisor did not acknowledge finalizer completion",
                )
                campaign.communicate(timeout=10)
                self.assertEqual(campaign.returncode, 137)
                with open(status_path) as handle:
                    self.assertEqual(handle.read(), "137\n")
                with open(state_path) as handle:
                    self.assertEqual(handle.read(), "active\n")
                self.assertEqual(
                    subprocess.run(
                        ["flock", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=5,
                    ).returncode,
                    0,
                    "the lease remained held after the completion ack",
                )
            finally:
                with open(restore_release, "w"):
                    pass
                if campaign.poll() is None:
                    os.killpg(campaign.pid, signal.SIGKILL)
                    campaign.communicate(timeout=5)
                for pid in (guardian_pid, supervisor_pid, sudo_pid):
                    if pid is not None and proc_state(pid) not in (None, "Z"):
                        try:
                            os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_replay_guardian_does_not_wait_for_an_unarmed_startup_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            lock_path = os.path.join(tmp, "lease")
            ready_path = os.path.join(tmp, "guardian.ready")
            status_path = os.path.join(tmp, "guardian.status")
            completion_path = os.path.join(tmp, "completion")
            token = "b" * 32
            with open(lock_path, "w"):
                pass
            lease_fd = os.open(lock_path, os.O_RDONLY | os.O_CLOEXEC)
            control_fd = os.open(os.devnull, os.O_RDONLY | os.O_CLOEXEC)
            fcntl.flock(lease_fd, fcntl.LOCK_EX)
            guardian = subprocess.Popen(
                [
                    sys.executable, SERVE_GUARDIAN,
                    "--lease-fd", str(lease_fd),
                    "--control-fd", str(control_fd),
                    "--ready-path", ready_path,
                    "--status-path", status_path,
                    "--completion-path", completion_path,
                    "--completion-token", token,
                    "--", "sh", "-c", "exit 23",
                ],
                pass_fds=(lease_fd, control_fd),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            os.close(lease_fd)
            os.close(control_fd)
            stdout, stderr = guardian.communicate(timeout=5)
            self.assertEqual(guardian.returncode, 23, stdout + stderr)
            with open(status_path) as handle:
                self.assertEqual(handle.read(), "23\n")
            self.assertFalse(os.path.exists(completion_path))

    def test_replay_guardian_never_publishes_success_after_completion_error(self):
        """A verifier failure cannot preserve the child's zero status."""
        published = []

        class SuccessfulChild:
            @staticmethod
            def wait():
                return 0

        def record_publication(path, contents):
            published.append((path, contents))

        with mock.patch.object(
                serve_guardian, "protect_lease_from_signals"), \
                mock.patch.object(serve_guardian.os, "setsid"), \
                mock.patch.object(serve_guardian.os, "fstat"), \
                mock.patch.object(serve_guardian.fcntl, "flock"), \
                mock.patch.object(serve_guardian.os, "close"), \
                mock.patch.object(serve_guardian, "remove_record"), \
                mock.patch.object(
                    serve_guardian, "publish",
                    side_effect=record_publication), \
                mock.patch.object(
                    serve_guardian.subprocess, "Popen",
                    return_value=SuccessfulChild()), \
                mock.patch.object(
                    serve_guardian, "wait_for_completion",
                    side_effect=RuntimeError("completion state unreadable")):
            status = serve_guardian.guard(
                ["true"], 9, 10, "ready", "status", "completion",
                "f" * 32,
            )

        self.assertEqual(status, 125)
        self.assertEqual(published, [("ready", mock.ANY), ("status", "125\n")])

    def test_replay_guardian_ignores_term_before_its_child_finishes(self):
        """A control-plane signal cannot drop the host lease.

        RED BEFORE THE FIX: signal handlers were installed only after the
        direct child exited. TERM after readiness killed the guardian, released
        fd 9, and left the child alive.
        """
        with tempfile.TemporaryDirectory() as tmp:
            lock_path = os.path.join(tmp, "lease")
            ready_path = os.path.join(tmp, "guardian.ready")
            status_path = os.path.join(tmp, "guardian.status")
            completion_path = os.path.join(tmp, "completion")
            child_pid_path = os.path.join(tmp, "child.pid")
            child_release = os.path.join(tmp, "child.release")
            guardian_pid_path = os.path.join(tmp, "guardian.pid")
            log_path = os.path.join(tmp, "guardian.log")
            token = "e" * 32
            with open(lock_path, "w"):
                pass
            launcher = (
                "set -u\n"
                "exec 9<\"$LOCK_PATH\"\n"
                "flock -x 9\n"
                "exec {control_fd}</dev/null\n"
                "python3 \"$SERVE_GUARDIAN\" --lease-fd 9 "
                "--control-fd \"$control_fd\" "
                "--ready-path \"$READY_PATH\" "
                "--status-path \"$STATUS_PATH\" "
                "--completion-path \"$COMPLETION_PATH\" "
                "--completion-token \"$COMPLETION_TOKEN\" -- "
                "bash -c 'printf \"%s\\n\" \"$$\" >\"$CHILD_PID_PATH\"; "
                "while [ ! -e \"$CHILD_RELEASE\" ]; do sleep 0.01; done; "
                "exit 23' >\"$LOG_PATH\" 2>&1 &\n"
                "guardian=$!\n"
                "printf '%s\\n' \"$guardian\" >\"$GUARDIAN_PID_PATH\"\n"
                "for _ in $(seq 1 500); do\n"
                "  [ ! -e \"$READY_PATH\" ] || break\n"
                "  sleep 0.01\n"
                "done\n"
                "[ -e \"$READY_PATH\" ] || exit 125\n"
                "exec 9>&-\n"
                "wait \"$guardian\"\n"
            )
            env = dict(
                os.environ,
                LOCK_PATH=lock_path,
                SERVE_GUARDIAN=SERVE_GUARDIAN,
                READY_PATH=ready_path,
                STATUS_PATH=status_path,
                COMPLETION_PATH=completion_path,
                COMPLETION_TOKEN=token,
                CHILD_PID_PATH=child_pid_path,
                CHILD_RELEASE=child_release,
                GUARDIAN_PID_PATH=guardian_pid_path,
                LOG_PATH=log_path,
            )
            launcher_process = subprocess.Popen(
                ["bash", "-c", launcher], env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            guardian_pid = None
            child_pid = None

            def wait_for(path, description):
                deadline = time.monotonic() + 10
                while not os.path.exists(path):
                    if launcher_process.poll() is not None:
                        stdout, stderr = launcher_process.communicate()
                        self.fail(
                            f"launcher exited before {description}: "
                            f"{launcher_process.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail(f"timed out waiting for {description}")
                    time.sleep(0.01)

            try:
                wait_for(guardian_pid_path, "the guardian pid")
                wait_for(ready_path, "guardian readiness")
                wait_for(child_pid_path, "the long-lived child")
                with open(guardian_pid_path) as handle:
                    guardian_pid = int(handle.read())
                with open(child_pid_path) as handle:
                    child_pid = int(handle.read())

                os.kill(guardian_pid, signal.SIGTERM)
                time.sleep(0.1)
                self.assertNotIn(
                    proc_state(guardian_pid), (None, "Z"),
                    "TERM killed the guardian before its child finished",
                )
                self.assertNotIn(proc_state(child_pid), (None, "Z"))
                self.assertFalse(os.path.exists(status_path))
                self.assertNotEqual(
                    subprocess.run(
                        ["flock", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=5,
                    ).returncode,
                    0,
                    "TERM released the replay lease while the child was alive",
                )

                with open(child_release, "w"):
                    pass
                launcher_process.communicate(timeout=10)
                self.assertEqual(launcher_process.returncode, 23)
                with open(status_path) as handle:
                    self.assertEqual(handle.read(), "23\n")
            finally:
                with open(child_release, "w"):
                    pass
                for pid in (child_pid, guardian_pid):
                    if pid is not None and proc_state(pid) not in (None, "Z"):
                        try:
                            os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                if launcher_process.poll() is None:
                    os.killpg(launcher_process.pid, signal.SIGKILL)
                    launcher_process.communicate(timeout=5)

    def test_dnsmasq_restore_is_registered_before_the_stop(self):
        """The surviving replay supervisor owns both sides of the handoff.

        RED BEFORE THE FIX: the campaign shell stopped dnsmasq before it
        launched any process capable of restoring it after SIGKILL.
        """
        with open(EXTRA) as handle:
            source = handle.read()
        finalizer = source.find('--finalizer "$finalizer"')
        completion = source.find('--completion-path "$completion_path"')
        stop = source.find("systemctl stop dnsmasq")
        self.assertGreaterEqual(finalizer, 0, "replay has no surviving finalizer")
        self.assertGreaterEqual(
            completion, 0, "replay has no root-supervisor completion ack")
        self.assertGreaterEqual(stop, 0, "the dnsmasq handoff is gone")
        self.assertLess(
            finalizer, stop,
            "dnsmasq can be stopped before its restore finalizer is armed",
        )
        self.assertLess(
            completion, stop,
            "dnsmasq can be stopped before completion tracking is armed",
        )
        self.assertIn("export FCVM_FINALIZER_MODE=dnsmasq", source)
        self.assertIn(
            'export FCVM_DNSMASQ_WAS_ACTIVE="$dnsmasq_was_active"', source)
        self.assertIn('"$BENCH/host_resource_finalizer.py"', source)
        guardian = source.find('python3 "$BENCH/serve_guardian.py"')
        self.assertGreaterEqual(guardian, 0, "replay has no lease guardian")
        self.assertLess(
            guardian, stop,
            "dnsmasq can be stopped before the lease guardian is launched",
        )
        self.assertIn('--lease-fd 9 --control-fd "$SERVE_CONTROL_FD"', source)
        self.assertIn('--completion-path "$SERVE_COMPLETION_PATH"', source)
        self.assertIn('--completion-token "$SERVE_COMPLETION_TOKEN"', source)

    def test_proc_state_readers_treat_disappearance_during_read_as_gone(self):
        class VanishedProcStat:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            @staticmethod
            def read():
                raise ProcessLookupError(errno.ESRCH, "process exited")

        readers = (proc_state, phase_supervisor.read_process_stat)
        for reader in readers:
            with self.subTest(reader=reader.__module__ + "." + reader.__name__), \
                    mock.patch("builtins.open", return_value=VanishedProcStat()):
                self.assertIsNone(
                    reader(4242),
                    "a process exiting after open was reported as an error",
                )

    def test_process_stat_read_error_other_than_disappearance_blocks(self):
        class UnreadableProcStat:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            @staticmethod
            def read():
                raise OSError(errno.EIO, "procfs read failed")

        with mock.patch("builtins.open", return_value=UnreadableProcStat()):
            with self.assertRaisesRegex(RuntimeError, "procfs read failed"):
                phase_supervisor.read_process_stat(4242)

    def test_outer_stops_only_through_the_preopened_supervisor_control(self):
        with open(EXTRA) as handle:
            source = handle.read()
        stop = self.shell_function(source, "stop_active_phase")
        run = self.shell_function(source, "run_logged")
        self.assertIn("phase_supervisor.py", run)
        self.assertIn("mkfifo", run)
        self.assertIn('--control-path "$ACTIVE_PHASE_CONTROL_PATH"', run)
        self.assertIn('printf T >&"$control_fd"', stop)
        self.assertNotIn("owned_process.py", run + stop)
        self.assertNotRegex(stop, r'\bkill\b')
        self.assertNotIn("kill -0", stop)

    def test_run_logged_preserves_a_signal_during_finished_phase_cleanup(self):
        with open(EXTRA) as handle:
            source = handle.read()
        run = self.shell_function(source, "run_logged")
        stop = self.shell_function(source, "stop_active_phase")
        boundary = (
            "    else\n"
            "        exec {ACTIVE_PHASE_CONTROL_FD}>&-\n"
        )
        self.assertEqual(run.count(boundary), 1)
        run = run.replace(
            boundary,
            "    else\n"
            "        kill -TERM \"$BASHPID\"\n"
            "        exec {ACTIVE_PHASE_CONTROL_FD}>&-\n",
        )

        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "set -uo pipefail\n"
                f"BENCH={HERE!r}\nLOGDIR={tmp!r}\n"
                "ACTIVE_PHASE_PID=\nACTIVE_PHASE_SIGNAL=\n"
                "ACTIVE_PHASE_CONTROL_FD=\nACTIVE_PHASE_CONTROL_PATH=\n"
                "say() { :; }\n"
                + run + "\n" + stop + "\n"
                "set +e\n"
                "run_logged \"$LOGDIR/phase.log\" true\n"
                "rc=$?\n"
                "printf 'remembered=%s\\n' \"$ACTIVE_PHASE_SIGNAL\"\n"
                "exit \"$rc\"\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True,
                timeout=10,
            )

        self.assertEqual(
            proc.returncode, 143,
            "a signal received after the phase wait was reported as success: "
            + proc.stdout + proc.stderr,
        )

    def test_failed_phase_identity_capture_cannot_leave_the_phase_running(self):
        with open(EXTRA) as handle:
            source = handle.read()
        run = self.shell_function(source, "run_logged")
        stop = self.shell_function(source, "stop_active_phase")
        with tempfile.TemporaryDirectory() as tmp:
            bench = os.path.join(tmp, "bench")
            os.mkdir(bench)
            fake_supervisor = os.path.join(bench, "phase_supervisor.py")
            with open(fake_supervisor, "w") as handle:
                handle.write(
                    "import os,sys,time\n"
                    "args=sys.argv[1:]\n"
                    "control=None\n"
                    "if '--control-path' in args:\n"
                    "    control=args[args.index('--control-path')+1]\n"
                    "with open(os.environ['FAKE_PHASE_PID'], 'w') as out:\n"
                    "    out.write(str(os.getpid()))\n"
                    "os.close(1); os.close(2)\n"
                    "if control is None:\n"
                    "    time.sleep(60)\n"
                    "fd=os.open(control, os.O_RDONLY)\n"
                    "try:\n"
                    "    command=os.read(fd, 1)\n"
                    "finally:\n"
                    "    os.close(fd)\n"
                    "raise SystemExit(143 if command == b'T' else 125)\n"
                )
            fake_identity = os.path.join(bench, "owned_process.py")
            with open(fake_identity, "w") as handle:
                handle.write("raise SystemExit(3)\n")
            phase_pid_path = os.path.join(tmp, "phase.pid")
            script = (
                "set -uo pipefail\n"
                f"BENCH={bench!r}\nLOGDIR={tmp!r}\n"
                "ACTIVE_PHASE_PID=\nACTIVE_PHASE_START_TIME=\n"
                "ACTIVE_PHASE_SIGNAL=\nACTIVE_PHASE_CONTROL_FD=\n"
                "ACTIVE_PHASE_CONTROL_PATH=\n"
                "say() { :; }\n"
                + run + "\n" + stop + "\n"
                "set +e\nrun_logged \"$LOGDIR/phase.log\" ignored\n"
                "exit $?\n"
            )
            proc = subprocess.Popen(
                ["bash", "-c", script],
                env=dict(os.environ, FAKE_PHASE_PID=phase_pid_path),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            phase_pid = None
            try:
                deadline = time.monotonic() + 5
                while not os.path.isfile(phase_pid_path):
                    if proc.poll() is not None:
                        stdout, stderr = proc.communicate()
                        self.fail(
                            f"phase launcher exited before readiness: "
                            f"{proc.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail("fake phase supervisor did not start")
                    time.sleep(0.01)
                with open(phase_pid_path) as handle:
                    phase_pid = int(handle.read())
                proc.send_signal(signal.SIGTERM)
                proc.communicate(timeout=10)
                self.assertEqual(proc.returncode, 143)
                self.assertFalse(
                    os.path.exists(f"/proc/{phase_pid}"),
                    "identity capture failed and the still-running phase escaped",
                )
            finally:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.communicate(timeout=5)
                if phase_pid is not None and os.path.exists(f"/proc/{phase_pid}"):
                    try:
                        pidfd = os.pidfd_open(phase_pid)
                    except ProcessLookupError:
                        pidfd = None
                    if pidfd is not None:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                            poller = select.poll()
                            poller.register(pidfd, select.POLLIN)
                            poller.poll(5000)
                        finally:
                            os.close(pidfd)

    def test_outer_sigkill_closes_control_and_drains_the_phase(self):
        """Campaign process-group death cannot orphan a detached phase.

        RED BEFORE THE FIX: run_logged left its supervisor and tee in the
        campaign process group while the measured phase was in a new session.
        SIGKILL removed the cleanup owner and left the phase alive.
        """
        with open(EXTRA) as handle:
            source = handle.read()
        run = self.shell_function(source, "run_logged")
        stop = self.shell_function(source, "stop_active_phase")
        with tempfile.TemporaryDirectory() as tmp:
            phase_pid_path = os.path.join(tmp, "phase.pid")
            phase_code = (
                "import os,signal,time; "
                "stop=lambda *_: (_ for _ in ()).throw(SystemExit(0)); "
                "signal.signal(signal.SIGTERM, stop); "
                f"open({phase_pid_path!r}, 'w').write(str(os.getpid())); "
                "time.sleep(60)"
            )
            script = (
                "set -uo pipefail\n"
                f"BENCH={HERE!r}\nLOGDIR={tmp!r}\n"
                "ACTIVE_PHASE_PID=\nACTIVE_PHASE_SIGNAL=\n"
                "ACTIVE_PHASE_CONTROL_FD=\nACTIVE_PHASE_CONTROL_PATH=\n"
                "say() { :; }\n"
                + run + "\n" + stop + "\n"
                "run_logged \"$LOGDIR/phase.log\" python3 -c \"$PHASE_CODE\"\n"
            )
            proc = subprocess.Popen(
                ["bash", "-c", script],
                env=dict(os.environ, PHASE_CODE=phase_code),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                start_new_session=True,
            )
            phase_pid = None
            pidfd = None
            drained = False
            try:
                deadline = time.monotonic() + 5
                while not os.path.isfile(phase_pid_path):
                    if proc.poll() is not None:
                        stdout, stderr = proc.communicate()
                        self.fail(
                            f"phase launcher exited before readiness: "
                            f"{proc.returncode}: {stdout}{stderr}")
                    if time.monotonic() >= deadline:
                        self.fail("supervised phase did not start")
                    time.sleep(0.01)
                with open(phase_pid_path) as handle:
                    phase_pid = int(handle.read())
                pidfd = os.pidfd_open(phase_pid)
                os.killpg(proc.pid, signal.SIGKILL)
                proc.communicate(timeout=5)
                poller = select.poll()
                poller.register(pidfd, select.POLLIN)
                drained = bool(poller.poll(3000))
            finally:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.communicate(timeout=5)
                if pidfd is not None:
                    if not drained:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        poller = select.poll()
                        poller.register(pidfd, select.POLLIN)
                        poller.poll(5000)
                    os.close(pidfd)
            self.assertTrue(
                drained,
                "the measured phase survived campaign process-group SIGKILL",
            )

    def test_run_logged_captures_parent_before_async_bash_expansion(self):
        with open(EXTRA) as handle:
            source = handle.read()
        run = self.shell_function(source, "run_logged")
        stop = self.shell_function(source, "stop_active_phase")
        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "set -uo pipefail\n"
                f"BENCH={HERE!r}\nLOGDIR={tmp!r}\n"
                "ACTIVE_PHASE_PID=\nACTIVE_PHASE_SIGNAL=\n"
                "ACTIVE_PHASE_CONTROL_FD=\nACTIVE_PHASE_CONTROL_PATH=\n"
                "say() { :; }\n"
                + run + "\n" + stop + "\n"
                "run_logged \"$LOGDIR/phase.log\" true\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True,
                timeout=10,
            )
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertNotIn("expected parent is already gone", proc.stdout + proc.stderr)

    def test_server_startup_identity_failure_retains_a_cleanup_handle(self):
        with open(EXTRA) as handle:
            source = handle.read()
        stop = self.shell_function(source, "stop_corpus_serve")
        with tempfile.TemporaryDirectory() as tmp:
            fake = os.path.join(tmp, "fake_server.py")
            with open(fake, "w") as handle:
                handle.write(
                    "import os,sys,time\n"
                    "control,pid_path,status_path,ready=sys.argv[1:]\n"
                    "with open(pid_path, 'w') as out:\n"
                    "    out.write(str(os.getpid()))\n"
                    "with open(ready, 'w') as out:\n"
                    "    out.write('ready\\n')\n"
                    "fd=os.open(control, os.O_RDONLY)\n"
                    "try:\n"
                    "    command=os.read(fd, 1)\n"
                    "finally:\n"
                    "    os.close(fd)\n"
                    "if command != b'T':\n"
                    "    time.sleep(60)\n"
                    "with open(status_path + '.tmp', 'w') as out:\n"
                    "    out.write('0\\n')\n"
                    "os.replace(status_path + '.tmp', status_path)\n"
                )
            control = os.path.join(tmp, "server.control")
            ready = os.path.join(tmp, "server.ready")
            pid_path = os.path.join(tmp, "server.pid")
            status_path = os.path.join(tmp, "corpus-serve.status")
            script = (
                "set -uo pipefail\n"
                f"RESULTS={tmp!r}\nBENCH={HERE!r}\n"
                f"SERVE_CONTROL_PATH={control!r}\n"
                "mkfifo -- \"$SERVE_CONTROL_PATH\"\n"
                f"mkfifo -- {ready!r}\n"
                "exec {SERVE_CONTROL_FD}<>\"$SERVE_CONTROL_PATH\"\n"
                f"python3 {fake!r} \"$SERVE_CONTROL_PATH\" "
                f"{pid_path!r} {status_path!r} {ready!r} >/dev/null 2>&1 &\n"
                "SERVE_JOB_PID=$!\nSERVE_PID=\nSERVE_START_TIME=\n"
                f"IFS= read -r _ < {ready!r}\n"
                "say() { :; }\n"
                + stop + "\n"
                "stop_corpus_serve\n"
            )
            proc = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True,
                timeout=10,
            )
            self.assertTrue(os.path.isfile(pid_path), proc.stderr)
            with open(pid_path) as handle:
                server_pid = int(handle.read())
            try:
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertFalse(
                    os.path.exists(f"/proc/{server_pid}"),
                    "a failed server pidfile/identity capture lost the live server",
                )
                with open(status_path) as handle:
                    self.assertEqual(handle.read().strip(), "0")
            finally:
                if os.path.exists(f"/proc/{server_pid}"):
                    try:
                        pidfd = os.pidfd_open(server_pid)
                    except ProcessLookupError:
                        pidfd = None
                    if pidfd is not None:
                        try:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                            poller = select.poll()
                            poller.register(pidfd, select.POLLIN)
                            poller.poll(5000)
                        finally:
                            os.close(pidfd)

    def test_dead_container_resolver_option_is_removed(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = subprocess.run(
                [sys.executable, CORPUS_MEM,
                 "--results", os.path.join(tmp, "result"), "--tag", "tag",
                 "--urls", "https://example.com/",
                 "--source-revision", "a" * 40,
                 "--runtime-bundle-sha256", "b" * 64,
                 "--corpus-extra-runtime-bundle-sha256", "c" * 64,
                 "--container-resolve-to", "127.0.0.9"],
                capture_output=True, text=True, timeout=30,
            )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unrecognized arguments: --container-resolve-to", proc.stderr)

    def test_make_entry_enforces_clean_build_and_setup_prerequisites(self):
        with open(MAKEFILE) as handle:
            source = handle.read()
        match = re.search(
            r'^bench-chromium-corpus-extra:\s*([^\n]*)\n((?:\t[^\n]*\n)+)',
            source, re.MULTILINE,
        )
        self.assertIsNotNone(match, "corpus-extra has no Make entry point")
        dependencies = set(match.group(1).split())
        self.assertTrue({"require-clean-tree", "build", "setup-default"}
                        <= dependencies)
        self.assertIn("bench/chromium/corpus_extra.sh", match.group(2))


class ExactHostListener(unittest.TestCase):
    """A generic host port can belong to another Chromium process."""

    def test_listener_probe_runs_inside_the_named_container(self):
        seen = []

        def bounded(cmd, timeout):
            seen.append((cmd, timeout))
            return Completed(0, "", "")

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            self.assertTrue(corpus_mem.container_owns_tcp_listener("owned", 9222))
        self.assertEqual(seen[0][0][:3], ["podman", "exec", "owned"])
        self.assertIn("9222", seen[0][0])

    def test_listener_probe_rejects_an_unowned_port(self):
        with mock.patch.object(corpus_mem, "sh_bounded",
                               return_value=Completed(1, "", "not owned")):
            self.assertFalse(corpus_mem.container_owns_tcp_listener("owned", 9222))


class Resummarize(unittest.TestCase):
    """A recomputed host summary must describe one complete successful run.

    resummarize.py exists to restate a hostcdp run's p50 under the median
    convention reqanalyze publishes, so its descriptive host table is directly
    comparable to the VM table. It overwrites the summary.json hostcdp.sh wrote.

    hostcdp.sh can write "failures": 0 because it exits 4 on the first failed
    rep, so a summary it reaches is a run with none. resummarize.py has no such
    process invariant: it is pointed at a directory. It must prove the declared
    record count, schedule, and successes, and remove an earlier summary when
    that proof fails.
    """

    @staticmethod
    def run_on(rows, meta=None, stale_summary=False, complete=True,
               withdrawn=False, campaign_runtime=False):
        if campaign_runtime:
            campaign = tempfile.mkdtemp()
            tmp = os.path.join(campaign, "hostcdp-free")
            os.mkdir(tmp)
        else:
            tmp = tempfile.mkdtemp()
        if meta is None:
            warmup = sum(r.get("warmup") is True for r in rows)
            urls = list(dict.fromkeys(r.get("url") for r in rows))
            meta = {
                "reps": len(rows) - warmup,
                "warmup": warmup,
                "total_reps": len(rows),
                "urls": urls,
                "url_count": len(urls),
            }
        if meta is not False:
            meta = dict(meta)
            meta.setdefault(
                "run_id",
                f"{'1' * 32}-free" if campaign_runtime
                else "resummarize-fixture",
            )
            meta.setdefault(
                "corpus_extra_runtime_bundle_sha256",
                "9" * 64 if campaign_runtime else None,
            )
            if campaign_runtime:
                meta.setdefault("comparison_label", "free")
            with open(os.path.join(tmp, "run.json"), "w") as handle:
                json.dump(meta, handle)
            with open(os.path.join(tmp, "run.json"), "rb") as handle:
                run_json_sha256 = hashlib.sha256(handle.read()).hexdigest()
        else:
            run_json_sha256 = None
        with open(os.path.join(tmp, "hostcdp.jsonl"), "w") as handle:
            for r in rows:
                record = dict(r)
                if run_json_sha256 is not None:
                    record["run_json_sha256"] = run_json_sha256
                handle.write(json.dumps(record) + "\n")
        if meta is not False and complete:
            artifacts = {}
            for name in ("run.json", "hostcdp.jsonl"):
                path = os.path.join(tmp, name)
                with open(path, "rb") as handle:
                    raw = handle.read()
                artifacts[name] = {
                    "size": len(raw),
                    "sha256": hashlib.sha256(raw).hexdigest(),
                }
            with open(os.path.join(tmp, "complete.json"), "w") as handle:
                json.dump({"schema_version": 1,
                           "run_id": meta["run_id"],
                           "artifacts": artifacts}, handle)
        if withdrawn:
            with open(os.path.join(tmp, "WITHDRAWN"), "w") as handle:
                handle.write("fixture was withdrawn\n")
        if stale_summary:
            with open(os.path.join(tmp, "summary.json"), "w") as handle:
                json.dump({"n": 999, "failures": 0, "passed": True}, handle)
        proc = subprocess.run([sys.executable, os.path.join(HERE, "resummarize.py"), tmp],
                              capture_output=True, text=True, timeout=60)
        return tmp, proc

    @staticmethod
    def rep(rep, ok=True, warmup=False, wall_ms=100.0, load=0.5):
        return {"rep": rep, "ok": ok, "warmup": warmup, "wall_ms": wall_ms,
                "loadavg1": load, "loadavg1_read_status": 0,
                "measurement_valid": True,
                "url": "https://example.com/", "driver": "{}"}

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

    def test_a_complete_legacy_run_is_summarised(self):
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 6)]
        meta = {"reps": 6, "warmup": 1,
                "urls": ["https://example.com/"], "url_count": 1}
        tmp, proc = self.run_on(rows, meta=meta)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["n"], 5)
        self.assertEqual(rec["p50_ms"], 103.0)

    def test_a_run_holding_a_failed_rep_is_refused(self):
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 5)] + \
               [self.rep(5, ok=False, wall_ms=30000.0)]
        tmp, proc = self.run_on(rows)
        self.assertNotEqual(proc.returncode, 0,
                            "a run with a failed rep was summarised; its timeout "
                            f"is now in the p95\n{proc.stdout}{proc.stderr}")
        # A refusal, not a traceback: the unfixed script died inside
        # statistics.median on the empty-run case, which is also a non-zero
        # exit and would satisfy a bare returncode check.
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("1 of 5 measured reps", proc.stderr, proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
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
        """Refused, not crashed. The unfixed script raised StatisticsError from
        an empty median, which exits non-zero for a reason no reader can act on."""
        tmp, proc = self.run_on([self.rep(0, warmup=True)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_a_truncated_successful_prefix_is_refused(self):
        """Every row present can say ok=true while the run is still partial.

        The producer records the promised measured, warmup, and total counts in
        run.json. Four successful measured rows are not a completed five-rep
        run, so resummarizing them would turn interruption into a fast-looking
        result with failures=0.
        """
        rows = [self.rep(0, warmup=True)] + [self.rep(i) for i in range(1, 5)]
        meta = {"reps": 5, "warmup": 1, "total_reps": 6,
                "urls": ["https://example.com/"], "url_count": 1}
        tmp, proc = self.run_on(rows, meta=meta)
        self.assertNotEqual(proc.returncode, 0,
                            "a successful prefix was published as a completed run")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("total_reps", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_a_refusal_removes_an_earlier_successful_summary(self):
        """A non-zero exit beside passed-looking summary.json fails open."""
        rows = [self.rep(0, warmup=True), self.rep(1),
                self.rep(2, ok=False, wall_ms=30000.0)]
        tmp, proc = self.run_on(rows, stale_summary=True)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "the refused recomputation left the stale successful summary")

    def test_missing_run_metadata_is_refused(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows, meta=False)
        self.assertNotEqual(proc.returncode, 0,
                            "records with no declared count were summarized")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("run.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_a_run_without_its_completion_commit_is_refused(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows, stale_summary=True, complete=False)
        self.assertNotEqual(proc.returncode, 0,
                            "an interrupted producer was resummarized")
        self.assertIn("complete.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "the refusal left a stale summary quotable")

    def test_a_campaign_run_without_parent_completion_is_refused(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(
            rows, stale_summary=True, campaign_runtime=True
        )
        self.assertNotEqual(
            proc.returncode, 0,
            "leftover child completion authorized campaign resummarization",
        )
        self.assertIn("campaign-complete.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_completion_is_rechecked_at_summary_publication(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary = os.path.join(tmp, "summary.json")
        os.unlink(summary)
        completion = os.path.join(tmp, "complete.json")
        original = bench_compare.write_json_atomic

        def change_completion_before_publication(*args, **kwargs):
            with open(completion) as handle:
                record = json.load(handle)
            record["run_id"] = "changed-before-publication"
            with open(completion, "w") as handle:
                json.dump(record, handle)
            return original(*args, **kwargs)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(
                bench_compare, "write_json_atomic",
                side_effect=change_completion_before_publication), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg="changed completion raced summary publication"):
                runpy.run_path(
                    os.path.join(HERE, "resummarize.py"), run_name="__main__"
                )
        self.assertFalse(os.path.exists(summary))

    def test_completion_is_rechecked_after_summary_publication(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary = os.path.join(tmp, "summary.json")
        os.unlink(summary)
        completion = os.path.join(tmp, "complete.json")
        original = bench_compare.write_json_atomic

        def change_completion_after_publication(*args, **kwargs):
            caller_after_publish = kwargs.get("after_publish")

            def after_publish():
                with open(completion) as handle:
                    record = json.load(handle)
                record["run_id"] = "changed-after-publication"
                with open(completion, "w") as handle:
                    json.dump(record, handle)
                if caller_after_publish is not None:
                    caller_after_publish()

            kwargs["after_publish"] = after_publish
            return original(*args, **kwargs)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(
                bench_compare, "write_json_atomic",
                side_effect=change_completion_after_publication), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg="changed completion survived summary publication"):
                runpy.run_path(
                    os.path.join(HERE, "resummarize.py"), run_name="__main__"
                )
        self.assertFalse(os.path.exists(summary))

    def test_stale_summary_removal_uses_the_pinned_directory(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary = os.path.join(tmp, "summary.json")
        stale = b'{"stale": true}\n'
        with open(summary, "wb") as handle:
            handle.write(stale)

        detached = f"{tmp}.detached"
        replacement = f"{tmp}.replacement"
        os.mkdir(replacement)
        replacement_summary = os.path.join(replacement, "summary.json")
        sentinel = b'{"replacement": true}\n'
        with open(replacement_summary, "wb") as handle:
            handle.write(sentinel)

        real_unlink = os.unlink
        swapped = False

        def swap_before_unlink(path, *args, **kwargs):
            nonlocal swapped
            if not swapped and os.path.basename(path) == "summary.json":
                os.rename(tmp, detached)
                os.rename(replacement, tmp)
                swapped = True
            return real_unlink(path, *args, **kwargs)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(os, "unlink", side_effect=swap_before_unlink), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    SystemExit,
                    msg="directory replacement escaped a bounded refusal"):
                runpy.run_path(
                    os.path.join(HERE, "resummarize.py"), run_name="__main__"
                )
        self.assertTrue(swapped, "the directory replacement was not injected")
        with open(os.path.join(tmp, "summary.json"), "rb") as handle:
            self.assertEqual(
                handle.read(), sentinel,
                "stale-summary cleanup removed an entry from the replacement directory",
            )
        self.assertFalse(
            os.path.exists(os.path.join(detached, "summary.json")),
            "stale-summary cleanup did not remove the entry from the pinned directory",
        )

    def test_summary_publication_uses_the_pinned_directory(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        os.unlink(os.path.join(tmp, "summary.json"))

        detached = f"{tmp}.detached"
        replacement = f"{tmp}.replacement"
        os.mkdir(replacement)
        replacement_summary = os.path.join(replacement, "summary.json")
        sentinel = b'{"replacement": true}\n'
        with open(replacement_summary, "wb") as handle:
            handle.write(sentinel)

        real_link = os.link
        real_replace = os.replace
        swapped = False

        def swap_directories():
            nonlocal swapped
            if swapped:
                return
            os.rename(tmp, detached)
            os.rename(replacement, tmp)
            swapped = True

        def link_after_swap(source, destination, *args, **kwargs):
            swap_directories()
            return real_link(source, destination, *args, **kwargs)

        def replace_after_swap(source, destination, *args, **kwargs):
            swap_directories()
            # The path-based writer's temporary moved with the detached
            # directory. A replacement directory can contain the same entry
            # name by the time rename(2) resolves both pathnames.
            with open(source, "wb") as handle:
                handle.write(b'{"substituted": true}\n')
            return real_replace(source, destination, *args, **kwargs)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(os, "link", side_effect=link_after_swap), \
                mock.patch.object(os, "replace", side_effect=replace_after_swap), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg="a replaced results directory authorized summary publication"):
                runpy.run_path(
                    os.path.join(HERE, "resummarize.py"), run_name="__main__"
                )
        self.assertTrue(swapped, "the directory replacement was not injected")
        with open(os.path.join(tmp, "summary.json"), "rb") as handle:
            self.assertEqual(
                handle.read(), sentinel,
                "summary publication replaced an entry outside the pinned directory",
            )
        self.assertFalse(
            os.path.exists(os.path.join(detached, "summary.json")),
            "a refused publication left a summary in the detached directory",
        )

    def test_summary_lock_contention_is_bounded(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary = os.path.join(tmp, "summary.json")
        os.unlink(summary)
        ready = os.path.join(tmp, "summary-lock-holder-ready")
        holder_source = """
import fcntl
import os
import sys

fd = os.open(sys.argv[1], os.O_RDWR)
fcntl.flock(fd, fcntl.LOCK_EX)
with open(sys.argv[2], "x"):
    pass
sys.stdin.read(1)
"""
        holder = subprocess.Popen(
            [sys.executable, "-c", holder_source,
             os.path.join(tmp, ".summary.lock"), ready],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 2
            while not os.path.exists(ready) and time.monotonic() < deadline:
                if holder.poll() is not None:
                    self.fail(
                        "summary lock holder exited early: "
                        f"{holder.communicate()[1]}"
                    )
                time.sleep(0.01)
            self.assertTrue(os.path.exists(ready), "summary lock holder was not ready")
            try:
                blocked = subprocess.run(
                    [sys.executable, os.path.join(HERE, "resummarize.py"), tmp],
                    capture_output=True,
                    text=True,
                    timeout=7,
                )
            except subprocess.TimeoutExpired:
                self.fail("resummarize blocked indefinitely on its held lock")
            self.assertNotEqual(blocked.returncode, 0)
            self.assertIn("lock", blocked.stderr.lower(), blocked.stderr)
            self.assertIn("held", blocked.stderr.lower(), blocked.stderr)
            self.assertFalse(os.path.exists(summary))
        finally:
            if holder.poll() is None:
                holder.communicate(input="x", timeout=5)

    def test_pending_worker_summary_waits_for_bootstrap_publication(self):
        """A reader yields when the worker is done but its guardian is not."""
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary_path = os.path.join(tmp, "summary.json")
        completion_path = os.path.join(tmp, "complete.json")
        pending_path = os.path.join(tmp, ".summary.pending")
        with open(summary_path, "rb") as handle:
            summary_bytes = handle.read()
        with open(completion_path, "rb") as handle:
            completion_bytes = handle.read()
        os.replace(summary_path, pending_path)
        os.unlink(completion_path)

        original_acquire = bench_compare.acquire_output_lock
        acquire_count = 0

        def publish_on_reacquire(target, lock_fd, wait_seconds=5.0):
            nonlocal acquire_count
            original_acquire(target, lock_fd, wait_seconds)
            acquire_count += 1
            if acquire_count == 2:
                os.replace(pending_path, summary_path)
                temporary = completion_path + ".temporary"
                with open(temporary, "wb") as handle:
                    handle.write(completion_bytes)
                    handle.flush()
                    os.fsync(handle.fileno())
                os.replace(temporary, completion_path)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(
                bench_compare, "acquire_output_lock",
                side_effect=publish_on_reacquire), \
                mock.patch.object(sys, "argv", argv):
            runpy.run_path(
                os.path.join(HERE, "resummarize.py"), run_name="__main__"
            )

        self.assertEqual(acquire_count, 2)
        with open(summary_path) as handle:
            self.assertEqual(json.load(handle)["n"], 1)

    def test_summary_lock_refuses_symlinks_and_multiple_links(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        for kind in ("symlink", "hardlink"):
            with self.subTest(kind=kind):
                tmp, proc = self.run_on(rows)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                os.unlink(os.path.join(tmp, "summary.json"))
                lock_path = os.path.join(tmp, ".summary.lock")
                os.unlink(lock_path)
                backing = os.path.join(tmp, f"summary-lock-{kind}-backing")
                backing_bytes = b"not a lock domain\n"
                with open(backing, "wb") as handle:
                    handle.write(backing_bytes)
                if kind == "symlink":
                    os.symlink(backing, lock_path)
                else:
                    os.link(backing, lock_path)
                refused = subprocess.run(
                    [sys.executable, os.path.join(HERE, "resummarize.py"), tmp],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                self.assertNotEqual(
                    refused.returncode, 0,
                    f"resummarize used a {kind} as its lock domain",
                )
                self.assertIn("lock", refused.stderr.lower(), refused.stderr)
                self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))
                with open(backing, "rb") as handle:
                    self.assertEqual(handle.read(), backing_bytes)

    def test_summary_lock_entry_is_rechecked_at_publication(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        summary = os.path.join(tmp, "summary.json")
        os.unlink(summary)
        lock_path = os.path.join(tmp, ".summary.lock")
        detached = os.path.join(tmp, ".summary.lock.detached")
        original = bench_compare.write_json_atomic

        def replace_lock_before_publication(*args, **kwargs):
            os.rename(lock_path, detached)
            with open(lock_path, "w"):
                pass
            return original(*args, **kwargs)

        argv = ["resummarize.py", tmp]
        with mock.patch.object(
                bench_compare, "write_json_atomic",
                side_effect=replace_lock_before_publication), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg="a detached summary lock authorized publication"):
                runpy.run_path(
                    os.path.join(HERE, "resummarize.py"), run_name="__main__"
                )
        self.assertFalse(os.path.exists(summary))

    def test_a_withdrawn_run_is_refused(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows, stale_summary=True, withdrawn=True)
        self.assertNotEqual(proc.returncode, 0,
                            "a withdrawn host run was resummarized")
        self.assertIn("WITHDRAWN", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "the refusal left a stale summary quotable")


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


class ArchivedCorpusExtraDisposition(unittest.TestCase):
    """Legacy artifacts stay available, but no unprovable result is quotable."""

    RESULTS = os.path.join(HERE, "results")

    def marker(self, directory):
        path = os.path.join(self.RESULTS, directory, "WITHDRAWN")
        self.assertTrue(os.path.isfile(path), f"{path} is missing")
        with open(path) as handle:
            reason = handle.readline().strip()
        self.assertTrue(reason, f"{path} names no reason")

    def test_legacy_host_ratio_is_withdrawn_not_relabelled(self):
        directory = "corpusextra-hostcdp-20260830-172413"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory)
        self.assertFalse(os.path.exists(os.path.join(root, "comparison.json")),
                         "the unbound host ratio remains quotable")
        host = os.path.join(root, "hostcdp-free")
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        self.assertNotIn("run_id", meta,
                         "do not invent an identity the old producer never recorded")
        with open(os.path.join(host, "hostcdp.jsonl")) as handle:
            row = json.loads(next(handle))
        self.assertNotIn("run_json_sha256", row)
        self.assertNotIn("loadavg1", row)

    def test_failed_memory_attempt_is_permanently_withdrawn(self):
        directory = "corpusextra-memory-20260830-173915"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory, "memory")
        self.assertFalse(os.path.exists(os.path.join(root, "summary.json")))
        with open(os.path.join(root, "phase.log")) as handle:
            last = [line.strip() for line in handle if line.strip()][-1]
        self.assertIn(" BLOCKED:", last)

    def test_blocked_memory_and_cpu_design_is_permanently_withdrawn(self):
        directory = "corpusextra-memory-20260830-181830"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory, "memory")
        with open(os.path.join(root, "run.json")) as handle:
            meta = json.load(handle)
        self.assertNotIn("schedule", meta)
        self.assertNotIn("snapshot_generation", meta)
        with open(os.path.join(root, "samples.jsonl")) as handle:
            sides = [json.loads(line)["side"] for line in handle]
        switches = sum(left != right for left, right in zip(sides, sides[1:]))
        self.assertEqual(switches, 1,
                         "the invalidating blocked-side order changed; re-audit the archive")

    def test_withdrawn_memory_analysis_publishes_no_derived_figures(self):
        root = os.path.join(
            self.RESULTS, "corpusextra-memory-20260830-181830"
        )
        with open(os.path.join(root, "memory-cpu-analysis.md")) as handle:
            analysis = handle.read()
        self.assertIn("withdrawn", analysis.lower())
        for published_claim in (
                "## Memory, per instance", "## CPU time", "2.93x",
                "recompute_memory_cpu.py"):
            self.assertNotIn(
                published_claim,
                analysis,
                f"withdrawn analysis still publishes {published_claim!r}",
            )

class ComparePublicationGate(unittest.TestCase):
    """compare.py writes descriptive host and VM tables to comparison.json.

    The comparison must bind the raw VM bytes to the analysis that passed its
    publication gate, and must prove that every scheduled VM and host input is
    complete and compatible. Separately timed runs publish no effect ratio.
    """

    # Most comparator fixtures exercise record and host binding, not the corpus
    # resolver. Keep those fixtures outside the resolver publication contract;
    # the resolver-specific cases below opt into a hostname and its evidence.
    URL = "https://192.0.2.1/"
    HOSTNAME_URL = "https://example.com/"
    IMAGE_ID = "sha256:" + "a" * 64
    CELL = {"cpu": 2, "memory_mib": 1024, "backend": "uffd", "uffd_mode": "minor",
            "snapshot": "cb-req-corpus", "image": "localhost/chromium-bench-req",
            "image_id": IMAGE_ID, "url": URL, "urls": [URL],
            "guest_dns": None,
            "guest_env": [], "engine": "chromium", "cdp_port": 9222,
            "source_revision": "b" * 40, "harness_sha256": "c" * 64,
            "fcvm_sha256": "d" * 64, "runtime_bundle_sha256": "e" * 64,
            "snapshot_generation_id": "33333333-3333-4333-8333-333333333333",
            "snapshot_config_sha256": "5" * 64,
            "host_boot_id": "00000000-0000-4000-8000-000000000001",
            "host_kernel_release": "k", "host_machine": "aarch64"}

    @staticmethod
    def artifact_identity(path):
        with open(path, "rb") as handle:
            raw = handle.read()
        return {"path": path, "realpath": os.path.realpath(path),
                "size": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}

    def make_run(self, publishable=True, passed=True, vm_rows=(), cell=None,
                 include_identity=True, warmup=0, meta_overrides=None):
        tmp = tempfile.mkdtemp()
        cell = dict(cell or self.CELL)
        measured = [dict(row) for row in vm_rows]
        template = dict(measured[0]) if measured else self.vm_rep(1.0)
        run_id = "1" * 32
        urls = list(cell.get("urls") or [cell["url"]])

        scheduled = []
        for rep in range(warmup):
            row = dict(template)
            row.update(rep=rep, warmup=True, url=urls[rep % len(urls)],
                       run_id=run_id,
                       record_id=f"{run_id}:cdp:{rep}:1")
            scheduled.append(row)
        for offset, original in enumerate(measured):
            rep = warmup + offset
            row = dict(original)
            row.update(rep=rep, warmup=False, url=urls[rep % len(urls)],
                       run_id=run_id,
                       record_id=f"{run_id}:cdp:{rep}:0")
            scheduled.append(row)

        records = os.path.join(tmp, "reqbench.jsonl")
        meta = {
            "kind": "meta", "run_id": run_id, "seed": 1,
            "arms": ["cdp"], "reps": len(measured), "warmup": warmup,
            "url": ",".join(urls), "urls": urls,
            "image": cell["image"], "image_id": cell["image_id"],
            "guest_dns": cell["guest_dns"], "guest_env": cell["guest_env"],
            "source_revision": cell["source_revision"],
            "memory_mib": cell["memory_mib"],
            "backend": cell["backend"], "uffd_mode": cell["uffd_mode"],
            "snapshot": cell["snapshot"],
            "fcvm_sha256": cell["fcvm_sha256"],
            "harness_sha256": cell["harness_sha256"],
            "runtime_bundle_sha256": cell["runtime_bundle_sha256"],
            "snapshot_generation_id": cell["snapshot_generation_id"],
            "snapshot_config_sha256": cell["snapshot_config_sha256"],
            "host_boot_id": cell["host_boot_id"],
            "host_kernel_release": cell["host_kernel_release"],
            "host_machine": cell["host_machine"],
            "cpu": cell["cpu"],
        }
        if meta_overrides:
            meta.update(meta_overrides)
        with open(records, "w") as handle:
            handle.write(json.dumps(meta) + "\n")
            for row in scheduled:
                handle.write(json.dumps(row) + "\n")
        analysis = {
            "publishable": publishable,
            "gate": {
                "passed": passed,
                "reasons": [] if passed else ["fixture publication failure"],
            },
            "run_id": run_id,
            "cell": cell,
            "arms": {
                "cdp": {
                    "blocking_ms": {
                        "median": 700.0, "lo": 600.0, "hi": 800.0,
                        "n": max(1, len(measured)),
                    },
                },
            },
            "stall_gate": {
                "max_ms": 15000,
                "passed": True,
                "evaluated": max(1, len(measured)),
                "violations": [],
            },
        }
        if include_identity:
            analysis["analysis_identity"] = {
                "schema_version": 6,
                "inputs": [self.artifact_identity(records)],
            }
        with open(os.path.join(tmp, "analysis.json"), "w") as handle:
            json.dump(analysis, handle)
        return tmp

    def rewrite_vm_records(self, run, mutate):
        records = os.path.join(run, "reqbench.jsonl")
        with open(records) as handle:
            rows = [json.loads(line) for line in handle]
        mutate(rows)
        with open(records, "w") as handle:
            for row in rows:
                handle.write(json.dumps(row) + "\n")
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        analysis["analysis_identity"]["inputs"] = [self.artifact_identity(records)]
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)

    @staticmethod
    def rewrite_analysis(run, mutate):
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        mutate(analysis)
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)

    def make_two_arm_run(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])

        def add_noop(rows):
            meta, cdp_rows = rows[0], rows[1:]
            meta["arms"] = ["cdp", "noop"]
            schedule = []
            rng = random.Random(meta["seed"])
            for rep, cdp in enumerate(cdp_rows):
                order = list(meta["arms"])
                rng.shuffle(order)
                for arm in order:
                    if arm == "cdp":
                        schedule.append(cdp)
                    else:
                        schedule.append({
                            "arm": "noop", "rep": rep, "warmup": False,
                            "run_id": meta["run_id"],
                            "record_id": f"{meta['run_id']}:noop:{rep}:0",
                            "url": meta["urls"][rep % len(meta["urls"])],
                            "ok": True, "blocking_ms": 40.0,
                            "wall_ms": 200.0, "loadavg1": 0.5,
                        })
            rows[:] = [meta, *schedule]

        self.rewrite_vm_records(run, add_noop)
        self.rewrite_analysis(
            run,
            lambda analysis: analysis["arms"].update(noop={
                "blocking_ms": {
                    "median": 40.0, "lo": 40.0, "hi": 40.0, "n": 2,
                },
            }),
        )
        return run

    @staticmethod
    def vm_rep(blocking_ms, ok=True, include_ok=True):
        rec = {"arm": "cdp", "warmup": False, "blocking_ms": blocking_ms,
               "wall_ms": blocking_ms, "loadavg1": 0.5,
               "url": ComparePublicationGate.URL,
               "render": {"ok": True, "url": ComparePublicationGate.URL,
                          "stages": {"total_ms": blocking_ms},
                          "nav": {"load_ms": blocking_ms}}}
        if include_ok:
            rec["ok"] = ok
        return rec

    @staticmethod
    def host_rep(rep, warmup, url=URL, ok=True, wall_ms=100.0,
                 complete_driver=True):
        driver = {"ok": True, "url": url,
                  "stages": {"total_ms": wall_ms},
                  "nav": {"load_ms": wall_ms}}
        if not complete_driver:
            driver = {"ok": True, "url": url, "stages": {},
                      "nav": {"load_ms": wall_ms}}
        return {"rep": rep, "ok": ok, "warmup": warmup,
                "wall_ms": wall_ms, "loadavg1": 0.2,
                "loadavg1_read_status": 0, "measurement_valid": True,
                "url": url, "driver": json.dumps(driver)}

    def make_host(self, rows, meta_overrides=None):
        tmp = tempfile.mkdtemp()
        warmup = sum(r["warmup"] is True for r in rows)
        urls = list(dict.fromkeys(r["url"] for r in rows))
        meta = {
            "run_id": os.path.basename(tmp),
            "image": self.CELL["image"], "image_id": self.IMAGE_ID.removeprefix("sha256:"),
            "reps": len(rows) - warmup, "warmup": warmup,
            "total_reps": len(rows), "url": ",".join(urls),
            "urls": urls, "url_count": len(urls), "cdp_port": 9222,
            "comparison_label": "host", "cpu_budget": "vm-matched",
            "cpus": 2, "driver": "cdpdrive.py",
            "network": "host (no VM, no DNAT)",
            "resolve_all_to": "127.0.0.1", "host_kernel": "k",
            "host_boot_id": self.CELL["host_boot_id"],
            "host_machine": self.CELL["host_machine"],
            "source_revision": self.CELL["source_revision"],
            "harness_sha256": self.CELL["harness_sha256"],
            "runtime_bundle_sha256": self.CELL["runtime_bundle_sha256"],
            "corpus_extra_runtime_bundle_sha256": None,
            "hostcdp_sha256": "f" * 64,
        }
        if meta_overrides:
            meta.update(meta_overrides)
        with open(os.path.join(tmp, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(tmp, rows)
        return tmp

    @staticmethod
    def write_host_complete(host):
        artifacts = {}
        for name in ("run.json", "hostcdp.jsonl"):
            path = os.path.join(host, name)
            with open(path, "rb") as handle:
                raw = handle.read()
            artifacts[name] = {
                "size": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }
        with open(os.path.join(host, "run.json")) as handle:
            run_id = json.load(handle)["run_id"]
        with open(os.path.join(host, "complete.json"), "w") as handle:
            json.dump({"schema_version": 1, "run_id": run_id,
                       "artifacts": artifacts}, handle)

    @staticmethod
    def rewrite_host_complete(host, mutate):
        path = os.path.join(host, "complete.json")
        with open(path) as handle:
            complete = json.load(handle)
        mutate(complete)
        with open(path, "w") as handle:
            json.dump(complete, handle)

    @staticmethod
    def write_campaign_complete(campaign, hosts, run_id, runtime_sha256,
                                memory=None, phases=None):
        host_completes = []
        for host in hosts:
            complete = os.path.join(host, "complete.json")
            with open(complete, "rb") as handle:
                raw = handle.read()
            host_completes.append({
                "path": os.path.relpath(complete, campaign),
                "size": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            })
        host_completes.sort(key=lambda record: record["path"])
        memory_complete = None
        if memory is not None:
            with open(memory, "rb") as handle:
                raw = handle.read()
            memory_complete = {
                "path": os.path.relpath(memory, campaign),
                "size": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }
        if phases is None:
            phases = ["hostcdp"] + (["memory"] if memory is not None else [])
        with open(os.path.join(campaign, "campaign-complete.json"), "w") as handle:
            json.dump({
                "schema_version": 2,
                "run_id": run_id,
                "runtime_bundle_sha256": runtime_sha256,
                "phases": phases,
                "host_completes": host_completes,
                "memory_complete": memory_complete,
            }, handle)

    def make_campaign_host(self, rows=None, arm="free"):
        campaign = tempfile.mkdtemp()
        campaign_run_id = "1" * 32
        runtime_sha256 = "9" * 64
        if rows is None:
            rows = [self.host_rep(0, True)] + [
                self.host_rep(i, False, wall_ms=float(100 + i))
                for i in range(1, 4)
            ]
        temporary_host = self.make_host(rows, meta_overrides={
            "run_id": f"{campaign_run_id}-{arm}",
            "comparison_label": arm,
            "corpus_extra_runtime_bundle_sha256": runtime_sha256,
            "cpu_budget": "unlimited" if arm == "free" else "vm-matched",
            "cpus": None if arm == "free" else 2,
        })
        host = os.path.join(campaign, f"hostcdp-{arm}")
        shutil.move(temporary_host, host)
        self.write_campaign_complete(
            campaign, [host], campaign_run_id, runtime_sha256
        )
        return campaign, host

    @staticmethod
    def rewrite_campaign_complete(campaign, mutate):
        path = os.path.join(campaign, "campaign-complete.json")
        with open(path) as handle:
            complete = json.load(handle)
        mutate(complete)
        with open(path, "w") as handle:
            json.dump(complete, handle)

    @staticmethod
    def rewrite_memory_complete(campaign, host, mutate):
        memory = os.path.join(campaign, "memory", "complete.json")
        with open(memory) as handle:
            complete = json.load(handle)
        mutate(complete)
        with open(memory, "w") as handle:
            json.dump(complete, handle)
        ComparePublicationGate.write_campaign_complete(
            campaign, [host], "1" * 32, "9" * 64, memory=memory
        )

    @staticmethod
    def rewrite_memory_artifact(campaign, host, name, payload):
        memory_directory = os.path.join(campaign, "memory")
        path = os.path.join(memory_directory, name)
        with open(path, "wb") as handle:
            handle.write(payload)
        complete_path = os.path.join(memory_directory, "complete.json")
        with open(complete_path) as handle:
            complete = json.load(handle)
        record = next(
            artifact for artifact in complete["artifacts"]
            if artifact["path"] == name
        )
        record.update(size=len(payload), sha256=hashlib.sha256(payload).hexdigest())
        with open(complete_path, "w") as handle:
            json.dump(complete, handle)
        ComparePublicationGate.write_campaign_complete(
            campaign, [host], "1" * 32, "9" * 64, memory=complete_path
        )

    @staticmethod
    def bind_host_rows(host, rows=None):
        if rows is None:
            with open(os.path.join(host, "hostcdp.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
        with open(os.path.join(host, "run.json"), "rb") as handle:
            run_json_sha256 = hashlib.sha256(handle.read()).hexdigest()
        with open(os.path.join(host, "hostcdp.jsonl"), "w") as handle:
            for row in rows:
                record = dict(row)
                record["run_json_sha256"] = run_json_sha256
                handle.write(json.dumps(record) + "\n")
        ComparePublicationGate.write_host_complete(host)

    def run_compare(self, run_dir, hosts=(), out=None, script=None):
        out = out or os.path.join(run_dir, "comparison.json")
        argv = [sys.executable, script or os.path.join(HERE, "compare.py"),
                "--vm-run", run_dir]
        for label, host_dir in hosts:
            argv.extend(["--host", f"{label}={host_dir}"])
        argv.extend(["--out", out])
        return subprocess.run(
            argv,
            capture_output=True, text=True, timeout=60), out

    def test_output_cannot_name_or_alias_any_input(self):
        for input_name in ("analysis", "reqbench", "host-run", "host-rows",
                           "host-complete"):
            for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
                with self.subTest(input=input_name, alias=alias_kind):
                    run, host = self.valid_comparison()
                    inputs = {
                        "analysis": os.path.join(run, "analysis.json"),
                        "reqbench": os.path.join(run, "reqbench.jsonl"),
                        "host-run": os.path.join(host, "run.json"),
                        "host-rows": os.path.join(host, "hostcdp.jsonl"),
                        "host-complete": os.path.join(host, "complete.json"),
                    }
                    protected = inputs[input_name]
                    if alias_kind == "direct":
                        out = protected
                    elif alias_kind == "realpath":
                        alias_root = tempfile.mkdtemp()
                        os.symlink(os.path.dirname(protected),
                                   os.path.join(alias_root, "through-directory"))
                        out = os.path.join(alias_root, "through-directory",
                                           os.path.basename(protected))
                    else:
                        out = os.path.join(run, f"output-{input_name}-{alias_kind}")
                        if alias_kind == "symlink":
                            os.symlink(protected, out)
                        else:
                            os.link(protected, out)
                    with open(protected, "rb") as handle:
                        before = handle.read()
                    proc, _ = self.run_compare(run, [("host", host)], out=out)
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"--out accepted a {alias_kind} alias of {input_name}",
                    )
                    self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                    with open(protected, "rb") as handle:
                        self.assertEqual(
                            handle.read(), before,
                            f"--out destroyed or rewrote {input_name}",
                        )
                    if alias_kind in ("symlink", "hardlink"):
                        self.assertTrue(
                            os.path.lexists(out),
                            "the refused alias itself was unlinked",
                        )

    def test_output_cannot_remove_a_withdrawal_marker(self):
        for location in ("vm", "host", "campaign"):
            for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
                with self.subTest(location=location, alias=alias_kind):
                    if location == "vm":
                        run, host = self.valid_comparison()
                        hosts = [("host", host)]
                        marker = os.path.join(run, "WITHDRAWN")
                    elif location == "campaign":
                        run, campaign, host = self.valid_campaign_comparison()
                        hosts = [("free", host)]
                        marker = os.path.join(campaign, "WITHDRAWN")
                    else:
                        run, host = self.valid_comparison()
                        hosts = [("host", host)]
                        marker = os.path.join(host, "WITHDRAWN")
                    marker_bytes = b"fixture was withdrawn\n"
                    with open(marker, "wb") as handle:
                        handle.write(marker_bytes)
                    if alias_kind == "direct":
                        out = marker
                    elif alias_kind == "realpath":
                        alias_root = tempfile.mkdtemp()
                        os.symlink(
                            os.path.dirname(marker),
                            os.path.join(alias_root, "marker-owner"),
                        )
                        out = os.path.join(alias_root, "marker-owner", "WITHDRAWN")
                    else:
                        out = os.path.join(
                            run, f"withdrawn-{location}-{alias_kind}"
                        )
                        if alias_kind == "symlink":
                            os.symlink(marker, out)
                        else:
                            os.link(marker, out)
                    proc, _ = self.run_compare(run, hosts, out=out)
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"--out removed the {location} withdrawal marker",
                    )
                    self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                    with open(marker, "rb") as handle:
                        self.assertEqual(handle.read(), marker_bytes)
                    if alias_kind in ("symlink", "hardlink"):
                        self.assertTrue(
                            os.path.lexists(out),
                            "the refused alias itself was unlinked",
                        )

    def test_output_cannot_name_an_absent_vm_withdrawal_marker(self):
        run, host = self.valid_comparison()
        marker = os.path.join(run, "WITHDRAWN")
        self.assertFalse(os.path.lexists(marker))
        proc, _ = self.run_compare(
            run, [("host", host)], out=marker
        )
        self.assertNotEqual(
            proc.returncode, 0,
            "--out published comparison bytes at the VM withdrawal marker",
        )
        self.assertIn("alias", proc.stderr.lower(), proc.stderr)
        self.assertFalse(
            os.path.lexists(marker),
            "the refused comparison created a withdrawal marker",
        )

    def test_vm_withdrawal_is_refused_before_stale_output_cleanup(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        marker = os.path.join(run, "WITHDRAWN")
        marker_bytes = b"fixture was withdrawn\n"
        with open(marker, "wb") as handle:
            handle.write(marker_bytes)
        out = os.path.join(run, "comparison.json")
        argv = ["compare.py", "--vm-run", run, "--out", out]
        with mock.patch.object(
                bench_compare, "clear_stale_output",
                side_effect=AssertionError(
                    "withdrawal was checked after stale-output cleanup"
                )), mock.patch.object(sys, "argv", argv):
            with self.assertRaisesRegex(
                    bench_compare.Refusal, "WITHDRAWN|withdrawn"):
                bench_compare.main()
        with open(marker, "rb") as handle:
            self.assertEqual(handle.read(), marker_bytes)
        self.assertFalse(os.path.exists(out))

    def test_output_cannot_alias_running_comparison_source(self):
        for source_name in ("compare.py", "campaign_summary.py"):
            for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
                with self.subTest(source=source_name, alias=alias_kind):
                    run, host = self.valid_comparison()
                    with tempfile.TemporaryDirectory() as tmp:
                        comparator = os.path.join(tmp, "compare.py")
                        validator = os.path.join(tmp, "campaign_summary.py")
                        shutil.copyfile(os.path.join(HERE, "compare.py"), comparator)
                        shutil.copyfile(
                            os.path.join(HERE, "campaign_summary.py"), validator
                        )
                        protected = os.path.join(tmp, source_name)
                        if alias_kind == "direct":
                            out = protected
                        elif alias_kind == "realpath":
                            os.mkdir(os.path.join(tmp, "unused"))
                            out = os.path.join(tmp, "unused", "..", source_name)
                        else:
                            out = os.path.join(
                                tmp, f"output-{source_name}-{alias_kind}"
                            )
                            if alias_kind == "symlink":
                                os.symlink(protected, out)
                            else:
                                os.link(protected, out)
                        with open(protected, "rb") as handle:
                            before = handle.read()
                        proc, _ = self.run_compare(
                            run, [("host", host)], out=out, script=comparator
                        )
                        self.assertNotEqual(
                            proc.returncode, 0,
                            f"--out accepted a {alias_kind} alias of {source_name}",
                        )
                        self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                        with open(protected, "rb") as handle:
                            self.assertEqual(handle.read(), before)
                        if alias_kind in ("symlink", "hardlink"):
                            self.assertTrue(os.path.lexists(out))

    def test_output_parent_cannot_be_swapped_onto_an_input_after_preflight(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as tmp:
            output_parent = os.path.join(tmp, "output")
            moved_parent = os.path.join(tmp, "moved-output")
            os.mkdir(output_parent)
            out = os.path.join(output_parent, "analysis.json")
            with open(out, "w") as handle:
                json.dump({"publishable": True, "stale": True}, handle)

            original = bench_compare.reject_output_alias

            def swap_parent_after_preflight(*args, **kwargs):
                protected_inputs = original(*args, **kwargs)
                os.rename(output_parent, moved_parent)
                os.symlink(run, output_parent)
                return protected_inputs

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=swap_parent_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            self.assertTrue(os.path.exists(protected),
                            "a parent swap let stale-output removal delete an input")
            with open(protected, "rb") as handle:
                self.assertEqual(handle.read(), before)

    def test_input_moved_onto_output_after_preflight_is_not_unlinked(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            original = bench_compare.reject_output_alias

            def move_input_after_preflight(*args, **kwargs):
                protected_inputs = original(*args, **kwargs)
                os.rename(protected, out)
                return protected_inputs

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=move_input_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            self.assertTrue(os.path.exists(out),
                            "stale-output removal unlinked the raced input")
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), before)

    def test_output_replaced_after_preflight_is_not_deleted(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            concurrent = os.path.join(output_parent, "concurrent.json")
            with open(out, "wb") as handle:
                handle.write(b"stale comparison\n")
            replacement = b"concurrent writer result\n"
            with open(concurrent, "wb") as handle:
                handle.write(replacement)
            original = bench_compare.reject_output_alias

            def replace_output_after_preflight(*args, **kwargs):
                preflight = original(*args, **kwargs)
                os.replace(concurrent, out)
                return preflight

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=replace_output_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(out, "rb") as handle:
                self.assertEqual(
                    handle.read(), replacement,
                    "stale-output removal deleted a concurrent writer's inode",
                )

    def test_output_parent_cannot_be_swapped_onto_an_input_before_publish(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as tmp:
            output_parent = os.path.join(tmp, "output")
            moved_parent = os.path.join(tmp, "moved-output")
            os.mkdir(output_parent)
            out = os.path.join(output_parent, "analysis.json")
            original = bench_compare.write_json_atomic

            def swap_parent_before_publish(*args, **kwargs):
                os.rename(output_parent, moved_parent)
                os.symlink(run, output_parent)
                return original(*args, **kwargs)

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "write_json_atomic",
                    side_effect=swap_parent_before_publish), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(protected, "rb") as handle:
                self.assertEqual(
                    handle.read(), before,
                    "a parent swap let atomic publication replace an input",
                )

    def test_input_moved_onto_output_before_publish_is_not_replaced(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            original = bench_compare.write_json_atomic

            def move_input_before_publish(*args, **kwargs):
                os.rename(protected, out)
                return original(*args, **kwargs)

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "write_json_atomic",
                    side_effect=move_input_before_publish), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(out, "rb") as handle:
                self.assertEqual(
                    handle.read(), before,
                    "atomic publication replaced the raced input",
                )

    def test_output_lock_refuses_aliases_and_non_regular_entries(self):
        for kind in ("symlink", "hardlink", "fifo"):
            with self.subTest(kind=kind):
                run, host = self.valid_comparison()
                out = os.path.join(run, f"comparison-{kind}.json")
                lock = out + ".lock"
                if kind == "fifo":
                    os.mkfifo(lock)
                else:
                    backing = os.path.join(run, f"lock-backing-{kind}")
                    with open(backing, "w"):
                        pass
                    if kind == "symlink":
                        os.symlink(backing, lock)
                    else:
                        os.link(backing, lock)
                proc, _ = self.run_compare(
                    run, [("host", host)], out=out)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"the comparator used a {kind} as its lock domain",
                )
                self.assertIn("REFUSING", proc.stderr, proc.stderr)
                self.assertIn("lock", proc.stderr.lower(), proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_output_lock_entry_cannot_change_while_the_waiter_acquires_it(self):
        run, host = self.valid_comparison()
        out = os.path.join(run, "comparison-lock-race.json")
        lock = out + ".lock"
        detached = lock + ".detached"
        original_flock = bench_compare.fcntl.flock
        replaced = False

        def replace_lock_after_acquiring(fd, operation):
            nonlocal replaced
            result = original_flock(fd, operation)
            if not replaced:
                replaced = True
                os.rename(lock, detached)
                with open(lock, "w"):
                    pass
            return result

        argv = ["compare.py", "--vm-run", run,
                "--host", f"host={host}", "--out", out]
        with mock.patch.object(
                bench_compare.fcntl, "flock",
                side_effect=replace_lock_after_acquiring), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(bench_compare.Refusal):
                bench_compare.main()
        self.assertFalse(os.path.exists(out),
                         "a detached lock inode still authorized publication")

    def test_output_lock_contention_is_bounded(self):
        run, host = self.valid_comparison()
        out = os.path.join(run, "comparison-contended.json")
        ready = os.path.join(run, "lock-holder-ready")
        holder_source = """
import fcntl
import os
import sys

fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o600)
fcntl.flock(fd, fcntl.LOCK_EX)
with open(sys.argv[2], "x"):
    pass
sys.stdin.read(1)
"""
        holder = subprocess.Popen(
            [sys.executable, "-c", holder_source, out + ".lock", ready],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 2
            while not os.path.exists(ready) and time.monotonic() < deadline:
                if holder.poll() is not None:
                    self.fail(
                        "lock holder exited before acquiring the lock: "
                        f"{holder.communicate()[1]}"
                    )
                time.sleep(0.01)
            self.assertTrue(os.path.exists(ready), "lock holder was not ready")
            argv = [sys.executable, os.path.join(HERE, "compare.py"),
                    "--vm-run", run, "--host", f"host={host}",
                    "--out", out]
            try:
                proc = subprocess.run(
                    argv, capture_output=True, text=True, timeout=7
                )
            except subprocess.TimeoutExpired:
                self.fail("comparator blocked indefinitely on a held output lock")
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("lock", proc.stderr.lower(), proc.stderr)
            self.assertIn("held", proc.stderr.lower(), proc.stderr)
            self.assertFalse(os.path.exists(out))
        finally:
            if holder.poll() is None:
                holder.communicate(input="x", timeout=5)

    def test_a_run_that_failed_its_gate_is_refused(self):
        run = self.make_run(passed=False, vm_rows=[self.vm_rep(700.0)])
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a run that did not pass its publication gate was quoted")
        self.assertFalse(os.path.exists(out))

    def test_vm_gate_requires_boolean_agreement_and_no_reasons(self):
        mutations = {
            "truthy publishable": lambda analysis:
                analysis.update(publishable=1),
            "truthy passed": lambda analysis:
                analysis["gate"].update(passed=1),
            "passed with reasons": lambda analysis:
                analysis["gate"].update(reasons=["recorded defect"]),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run = self.make_run(vm_rows=[self.vm_rep(700.0)])
                self.rewrite_analysis(run, mutate)
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"VM analysis with {label} authorized a comparison",
                )
                self.assertIn("analysis.json", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_vm_stall_gate_must_be_armed_and_evaluate_records(self):
        mutations = {
            "unarmed": lambda analysis:
                analysis["stall_gate"].update(max_ms=None, evaluated=0),
            "empty": lambda analysis:
                analysis["stall_gate"].update(evaluated=0),
            "failed": lambda analysis:
                analysis["stall_gate"].update(
                    passed=False, violations=[{"record_id": "r1:cdp:0:0"}]
                ),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run = self.make_run(vm_rows=[self.vm_rep(700.0)])
                self.rewrite_analysis(run, mutate)
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"VM analysis with {label} stall gate authorized output",
                )
                self.assertIn("stall_gate", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_analysis_level_withdrawal_is_refused(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        self.rewrite_analysis(
            run, lambda analysis: analysis.update(withdrawn=True)
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(
            proc.returncode, 0,
            "analysis.json withdrew the VM run but compare published it",
        )
        self.assertIn("withdrawn", proc.stderr.lower(), proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_resolver_vm_requires_campaign_dns_and_diag_evidence(self):
        cell = dict(
            self.CELL,
            url=self.HOSTNAME_URL,
            urls=[self.HOSTNAME_URL],
            guest_dns="10.0.2.2",
        )
        resolver_rep = self.vm_rep(700.0)
        resolver_rep["render"]["url"] = self.HOSTNAME_URL
        run = self.make_run(
            vm_rows=[resolver_rep],
            cell=cell,
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(
            proc.returncode, 0,
            "a resolver VM with no campaign evidence was compared",
        )
        self.assertIn("dns-evidence.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_refusal_removes_an_earlier_comparison(self):
        """A failed rerun beside an old comparison fails open."""
        run = self.make_run(passed=False, vm_rows=[self.vm_rep(700.0)])
        out = os.path.join(run, "comparison.json")
        with open(out, "w") as handle:
            json.dump({"publishable": True, "stale": True}, handle)
        proc, returned_out = self.run_compare(run)
        self.assertEqual(returned_out, out)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(out),
                         "the refused rerun left an earlier comparison quotable")

    def test_an_unreadable_nonalias_input_still_removes_an_earlier_comparison(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        out = os.path.join(run, "comparison.json")
        with open(out, "w") as handle:
            json.dump({"publishable": True, "stale": True}, handle)
        records = os.path.join(run, "reqbench.jsonl")
        os.unlink(records)
        os.symlink("reqbench.jsonl", records)
        proc, _ = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(
            os.path.exists(out),
            "alias preflight left an earlier comparison quotable when stat failed",
        )

    def test_an_unpublishable_run_is_refused(self):
        run = self.make_run(publishable=False, vm_rows=[self.vm_rep(700.0)])
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(out))

    def test_a_passing_run_is_summarised(self):
        """The positive control: without it the two refusals could pass for
        any reason at all."""
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        proc, out = self.run_compare(run)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["vm"]["blocking_ms"]["p50"], 700.0)
        self.assertEqual(rec["vm"]["blocking_ms"]["n"], 3)

    def test_every_published_cell_field_is_bound_to_raw_vm_metadata(self):
        mismatches = {
            "cpu": 3,
            "memory_mib": 2048,
            "backend": "file",
            "uffd_mode": "copy",
            "snapshot": "another-snapshot",
            "image_id": "sha256:" + "f" * 64,
            "source_revision": "0" * 40,
            "fcvm_sha256": "1" * 64,
            "runtime_bundle_sha256": "2" * 64,
            "host_kernel_release": "another-kernel",
            "host_machine": "x86_64",
        }
        for field, value in mismatches.items():
            with self.subTest(field=field):
                run = self.make_run(vm_rows=[self.vm_rep(700.0)])
                self.rewrite_analysis(
                    run, lambda analysis, field=field, value=value:
                    analysis["cell"].update({field: value}))
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"analysis.json relabelled the raw VM {field}",
                )
                self.assertIn(field, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_published_cell_fields_are_validated_not_only_compared(self):
        invalid = {
            "cpu": True,
            "memory_mib": "1024",
            "backend": "unknown",
            "uffd_mode": "unknown",
            "snapshot": " padded ",
            "image_id": "sha256:short",
            "source_revision": "z" * 40,
            "fcvm_sha256": "z" * 64,
            "runtime_bundle_sha256": "z" * 64,
            "host_kernel_release": "",
            "host_machine": 7,
        }
        for field, value in invalid.items():
            with self.subTest(field=field):
                run = self.make_run(vm_rows=[self.vm_rep(700.0)])
                self.rewrite_vm_records(
                    run, lambda rows, field=field, value=value:
                    rows[0].update({field: value}))
                self.rewrite_analysis(
                    run, lambda analysis, field=field, value=value:
                    analysis["cell"].update({field: value}))
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"an invalid but equal {field} was published",
                )
                self.assertIn(field, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_snapshot_cell_uses_the_fcvm_snapshot_name_shape(self):
        for value in (".", "..", "slash/name", "snowman-\N{SNOWMAN}", "x" * 129):
            with self.subTest(value=value):
                run = self.make_run(vm_rows=[self.vm_rep(700.0)])
                self.rewrite_vm_records(
                    run, lambda rows, value=value:
                    rows[0].update(snapshot=value))
                self.rewrite_analysis(
                    run, lambda analysis, value=value:
                    analysis["cell"].update(snapshot=value))
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"invalid snapshot name {value!r} was published",
                )
                self.assertIn("snapshot", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_image_id_prefix_spelling_is_equivalent_and_output_is_canonical(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        self.rewrite_analysis(
            run, lambda analysis:
            analysis["cell"].update({"image_id": "a" * 64}))
        proc, out = self.run_compare(run)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            comparison = json.load(handle)
        self.assertEqual(comparison["cell"]["image_id"], self.IMAGE_ID)

    def test_source_revision_accepts_sha1_and_sha256_git_object_formats(self):
        for length in (40, 64):
            with self.subTest(length=length):
                cell = dict(self.CELL, source_revision="b" * length)
                run = self.make_run(
                    vm_rows=[self.vm_rep(700.0)], cell=cell)
                proc, out = self.run_compare(run)
                self.assertEqual(
                    proc.returncode, 0,
                    f"a supported {length}-hex Git object ID was refused: "
                    f"{proc.stderr}",
                )
                with open(out) as handle:
                    comparison = json.load(handle)
                self.assertEqual(
                    comparison["cell"]["source_revision"], "b" * length)

    def test_a_vm_record_that_does_not_say_it_succeeded_is_refused(self):
        rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        rows.append(self.vm_rep(30000.0, include_ok=False))
        run = self.make_run(vm_rows=rows)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a missing VM success silently reduced n")
        self.assertIn("successful", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_failed_vm_record_is_refused(self):
        rows = [self.vm_rep(600.0), self.vm_rep(700.0, ok=False),
                self.vm_rep(800.0)]
        run = self.make_run(vm_rows=rows)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a failed VM rep silently reduced n")
        self.assertIn("successful", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_meta_run_id_must_match_the_gated_analysis(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        analysis["run_id"] = "different-run"
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "analysis and raw records named different VM runs")
        self.assertIn("run_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_meta_must_be_the_first_record(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows.append(rows.pop(0)))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "records before VM metadata were accepted")
        self.assertIn("first", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_every_vm_row_must_name_the_meta_run_id(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(run_id="different-run"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a row from another VM run entered the medians")
        self.assertIn("run_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_declared_arm_schedule_must_be_complete(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)],
            meta_overrides={"arms": ["cdp", "noop"]},
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a wholly missing declared VM arm was ignored")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_schedule_cannot_have_a_missing_rep(self):
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        self.rewrite_vm_records(run, lambda rows: rows.pop(2))
        self.rewrite_analysis(
            run,
            lambda analysis: analysis["arms"]["cdp"]["blocking_ms"].update(n=2),
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a gap in the VM schedule silently reduced n")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_schedule_cannot_duplicate_a_rep(self):
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        self.rewrite_vm_records(run, lambda rows: rows[2].update(rep=0))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a duplicated VM rep silently replaced a scheduled rep")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_rep_must_be_an_integer_not_a_boolean(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(run, lambda rows: rows[1].update(rep=False))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "boolean False was accepted as VM rep zero")
        self.assertIn("invalid rep", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_warmup_flag_must_match_its_rep(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        self.rewrite_vm_records(run, lambda rows: rows[-1].update(warmup=True))
        self.rewrite_analysis(
            run,
            lambda analysis: analysis["arms"]["cdp"]["blocking_ms"].update(n=2),
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a measured VM rep relabelled as warmup silently reduced n")
        self.assertIn("warmup", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_record_id_must_name_its_exact_schedule_cell(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(record_id="r1:cdp:0:0"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a VM row carried another schedule cell's identity")
        self.assertIn("record_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_url_must_match_the_declared_corpus_schedule(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(url="https://different.example/"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a different page entered the VM corpus medians")
        self.assertIn("corpus schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_url_and_urls_metadata_must_name_the_same_corpus(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[0].update(url="https://different.example/"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "contradictory VM corpus declarations were accepted")
        self.assertIn("corpus", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_every_vm_cdp_success_has_every_compared_metric(self):
        mutations = {
            "blocking_ms": lambda row: row.pop("blocking_ms"),
            "wall_ms": lambda row: row.pop("wall_ms"),
            "total_ms": lambda row: row["render"]["stages"].pop("total_ms"),
            "load_ms": lambda row: row["render"]["nav"].pop("load_ms"),
            "render success": lambda row: row["render"].update(ok=False),
        }
        for metric, mutate in mutations.items():
            with self.subTest(metric=metric):
                run = self.make_run(
                    vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
                self.rewrite_vm_records(run, lambda rows: mutate(rows[2]))
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a VM cdp row missing {metric} silently reduced its own n",
                )
                self.assertIn(metric.split()[0], proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_every_vm_noop_success_has_every_compared_metric(self):
        for metric in ("blocking_ms", "wall_ms"):
            with self.subTest(metric=metric):
                run = self.make_two_arm_run()

                def remove_metric(rows):
                    next(row for row in rows if row.get("arm") == "noop").pop(metric)

                self.rewrite_vm_records(run, remove_metric)
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a VM noop row missing {metric} silently reduced its own n",
                )
                self.assertIn(metric, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_analysis_must_name_the_current_reqbench_bytes(self):
        rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        run = self.make_run(vm_rows=rows)
        path = os.path.join(run, "reqbench.jsonl")
        with open(path) as handle:
            before = handle.read()
        after = before.replace('"blocking_ms": 800.0',
                               '"blocking_ms": 900.0', 1)
        self.assertNotEqual(after, before)
        self.assertEqual(len(after), len(before),
                         "this test must isolate the content digest from size")
        with open(path, "w") as handle:
            handle.write(after)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "compare used records other than the gated input bytes")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("analysis_identity.inputs", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_analysis_without_an_input_identity_is_refused(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)], include_identity=False)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("analysis_identity.inputs", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_analysis_input_identity_is_portable_by_content(self):
        """Archived records move, so inode and absolute path are not identity."""
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        recorded = analysis["analysis_identity"]["inputs"][0]
        recorded.update(path="/recording-host/reqbench.jsonl",
                        realpath="/recording-host/reqbench.jsonl",
                        device=123, inode=456, mtime_ns=1, ctime_ns=1)
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)
        proc, out = self.run_compare(run)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(os.path.exists(out))

    def valid_comparison(self, host_rows=None, meta_overrides=None, vm_rows=None,
                         vm_warmup=None):
        if vm_rows is None:
            vm_rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        if host_rows is None:
            host_rows = [self.host_rep(0, True)] + [
                self.host_rep(i, False, wall_ms=float(100 + i))
                for i in range(1, 4)
            ]
        if vm_warmup is None:
            vm_warmup = sum(row["warmup"] is True for row in host_rows)
        run = self.make_run(vm_rows=vm_rows, warmup=vm_warmup)
        host = self.make_host(host_rows, meta_overrides=meta_overrides)
        return run, host

    def valid_campaign_comparison(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(value) for value in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        campaign, host = self.make_campaign_host()
        return run, campaign, host

    def valid_campaign_memory_comparison(self):
        run, campaign, host = self.valid_campaign_comparison()
        run_id = "1" * 32
        memory_directory = os.path.join(campaign, "memory")
        os.mkdir(memory_directory)
        payloads = {
            "run.json": json.dumps({"run_id": run_id}).encode() + b"\n",
            "samples.jsonl": b'{"sample":1}\n',
            "summary.json": json.dumps(
                {"run_id": run_id, "result": 3}
            ).encode() + b"\n",
        }
        for name, payload in payloads.items():
            with open(os.path.join(memory_directory, name), "wb") as handle:
                handle.write(payload)
        memory = os.path.join(memory_directory, "complete.json")
        corpus_mem.publish_completion(memory_directory, run_id)
        self.write_campaign_complete(
            campaign, [host], run_id, "9" * 64, memory=memory
        )
        return run, campaign, host, memory

    def test_a_complete_compatible_host_is_compared(self):
        run, host = self.valid_comparison()
        proc, out = self.run_compare(run, [("host", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["hosts"]["host"]["wall_ms"]["n"], 3)
        self.assertEqual(rec["hosts"]["host"]["driver_total_ms"]["n"], 3)
        identities = rec["input_identity"]
        self.assertEqual(
            identities["reqbench_jsonl"]["sha256"],
            self.artifact_identity(os.path.join(run, "reqbench.jsonl"))["sha256"],
        )
        self.assertEqual(
            identities["hosts"]["host"]["hostcdp_jsonl"]["sha256"],
            self.artifact_identity(os.path.join(host, "hostcdp.jsonl"))["sha256"],
        )
        self.assertEqual(
            identities["hosts"]["host"]["complete_json"]["sha256"],
            self.artifact_identity(os.path.join(host, "complete.json"))["sha256"],
        )

    def test_separate_host_and_vm_runs_publish_no_effect_ratios(self):
        run, host = self.valid_comparison()
        proc, out = self.run_compare(run, [("host", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            comparison = json.load(handle)
        ratio = comparison["ratios"]["host"]
        self.assertEqual(set(ratio), {"publishable", "reason"})
        self.assertIs(ratio["publishable"], False)
        self.assertIn("joint request-level schedule", ratio["reason"])
        self.assertIn("drift-control", ratio["reason"])
        self.assertIn("uncertainty", ratio["reason"])

    def test_a_completed_campaign_host_is_bound_into_the_comparison(self):
        run, campaign, host = self.valid_campaign_comparison()
        proc, out = self.run_compare(run, [("free", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            comparison = json.load(handle)
        self.assertEqual(
            comparison["input_identity"]["hosts"]["free"]
                      ["campaign_complete_json"]["sha256"],
            self.artifact_identity(
                os.path.join(campaign, "campaign-complete.json")
            )["sha256"],
        )

    def test_campaign_memory_completion_is_bound_and_revalidated(self):
        run, _campaign, host, memory = self.valid_campaign_memory_comparison()

        proc, out = self.run_compare(run, [("free", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            comparison = json.load(handle)
        self.assertEqual(
            comparison["input_identity"]["hosts"]["free"]
                      ["memory_complete_json"]["sha256"],
            self.artifact_identity(memory)["sha256"],
        )
        for name, filename, identity_key in (
            ("run", "run.json", "memory_run_json"),
            ("samples", "samples.jsonl", "memory_samples_jsonl"),
            ("summary", "summary.json", "memory_summary_json"),
        ):
            path = os.path.join(os.path.dirname(memory), filename)
            self.assertEqual(
                comparison["input_identity"]["hosts"]["free"]
                          [identity_key]["sha256"],
                self.artifact_identity(path)["sha256"],
            )

        os.unlink(out)
        with open(memory, "ab") as handle:
            handle.write(b"changed after campaign completion\n")
        proc, out = self.run_compare(run, [("free", host)])
        self.assertNotEqual(
            proc.returncode, 0,
            "campaign completion authorized changed memory completion bytes",
        )
        self.assertIn("memory/complete.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_campaign_memory_completion_validates_its_nested_contract(self):
        mutations = {
            "schema version": lambda complete:
                complete.update(schema_version=2),
            "boolean schema version": lambda complete:
                complete.update(schema_version=True),
            "wrong run": lambda complete:
                complete.update(run_id="2" * 32),
            "missing artifact": lambda complete:
                complete["artifacts"].pop(),
            "unsafe path": lambda complete:
                complete["artifacts"][0].update(path="../run.json"),
            "boolean size": lambda complete:
                complete["artifacts"][0].update(size=True),
            "invalid digest": lambda complete:
                complete["artifacts"][0].update(sha256="A" * 64),
            "unexpected field": lambda complete:
                complete.update(extra=True),
            "unexpected artifact field": lambda complete:
                complete["artifacts"][0].update(extra=True),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run, campaign, host, _memory = \
                    self.valid_campaign_memory_comparison()
                self.rewrite_memory_complete(campaign, host, mutate)
                proc, out = self.run_compare(run, [("free", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"campaign accepted nested memory completion with {label}",
                )
                self.assertIn("memory/complete.json", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

        for name, payload in (
            ("run.json", b"[]\n"),
            ("summary.json", b"[]\n"),
            ("run.json", json.dumps({"run_id": "2" * 32}).encode() + b"\n"),
            ("summary.json", json.dumps({"run_id": "2" * 32}).encode() + b"\n"),
        ):
            with self.subTest(artifact=name, payload=payload):
                run, campaign, host, _memory = \
                    self.valid_campaign_memory_comparison()
                self.rewrite_memory_artifact(
                    campaign, host, name, payload
                )
                proc, out = self.run_compare(run, [("free", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"campaign accepted invalid memory document {name}",
                )
                self.assertIn(name, proc.stderr, proc.stderr)
                self.assertIn("REFUSING", proc.stderr, proc.stderr)
                self.assertNotIn("Traceback", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_campaign_memory_artifacts_must_exist_and_match_completion(self):
        for name in ("run.json", "samples.jsonl", "summary.json"):
            for change in ("missing", "changed", "symlink"):
                with self.subTest(artifact=name, change=change):
                    run, campaign, host, _memory = \
                        self.valid_campaign_memory_comparison()
                    path = os.path.join(campaign, "memory", name)
                    if change == "missing":
                        os.unlink(path)
                    elif change == "changed":
                        with open(path, "ab") as handle:
                            handle.write(b"changed after completion\n")
                    else:
                        backing = path + ".backing"
                        os.rename(path, backing)
                        os.symlink(backing, path)
                    proc, out = self.run_compare(run, [("free", host)])
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"campaign accepted {change} memory artifact {name}",
                    )
                    self.assertIn(name, proc.stderr, proc.stderr)
                    self.assertFalse(os.path.exists(out))

    def test_campaign_memory_artifacts_are_rechecked_at_publication(self):
        run, campaign, host, _memory = self.valid_campaign_memory_comparison()
        changed = os.path.join(campaign, "memory", "summary.json")
        out = os.path.join(run, "comparison.json")
        original = bench_compare.write_json_atomic

        def change_memory_before_publication(*args, **kwargs):
            with open(changed, "ab") as handle:
                handle.write(b"changed before publication\n")
            return original(*args, **kwargs)

        argv = ["compare.py", "--vm-run", run,
                "--host", f"free={host}", "--out", out]
        with mock.patch.object(
                bench_compare, "write_json_atomic",
                side_effect=change_memory_before_publication), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg="changed memory artifact raced comparison publication"):
                bench_compare.main()
        self.assertFalse(os.path.exists(out))

    def assert_memory_revalidation_refuses_post_fstat_race(self, action):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "summary.json")
            replacement = os.path.join(tmp, "replacement.json")
            with open(path, "wb") as handle:
                handle.write(b'{"run_id":"' + b"a" * 32 + b'"}\n')
            with open(replacement, "wb") as handle:
                handle.write(b'{"run_id":"' + b"b" * 32 + b'"}\n')
            expected = bench_compare.artifact_identity(
                bench_compare.read_artifact_nofollow(path)
            )
            real_fstat = os.fstat
            target_fstats = 0

            def race_after_final_fstat(fd):
                nonlocal target_fstats
                observed = real_fstat(fd)
                if os.path.realpath(f"/proc/self/fd/{fd}") == path:
                    target_fstats += 1
                    if target_fstats == 2:
                        if action == "replace":
                            os.replace(replacement, path)
                        else:
                            os.unlink(path)
                return observed

            with mock.patch.object(
                    bench_compare.os, "fstat",
                    side_effect=race_after_final_fstat):
                with self.assertRaises(
                        bench_compare.Refusal,
                        msg=f"memory path {action} after final fstat was accepted"):
                    bench_compare.revalidate_artifact_identity(
                        "memory summary", expected, nofollow=True
                    )
            self.assertEqual(target_fstats, 2)

    def test_memory_revalidation_refuses_replace_after_final_fd_stat(self):
        self.assert_memory_revalidation_refuses_post_fstat_race("replace")

    def test_memory_revalidation_refuses_unlink_after_final_fd_stat(self):
        self.assert_memory_revalidation_refuses_post_fstat_race("unlink")

    def require_immediate_inode_reuse(self, directory):
        """Skip ABA regressions only when this filesystem will not reuse an inode."""
        probe = os.path.join(directory, "inode-reuse-probe")
        try:
            for attempt in range(32):
                with open(probe, "xb") as handle:
                    handle.write(f"old-{attempt}".encode())
                before = os.stat(probe)
                os.unlink(probe)
                with open(probe, "xb") as handle:
                    handle.write(f"new-{attempt}".encode())
                after = os.stat(probe)
                os.unlink(probe)
                if os.path.samestat(before, after):
                    return
        finally:
            try:
                os.unlink(probe)
            except FileNotFoundError:
                pass
        self.skipTest("filesystem did not reuse an immediately freed inode")

    def test_memory_reader_pins_inode_through_path_validation(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.require_immediate_inode_reuse(tmp)
            path = os.path.join(tmp, "summary.json")
            old = b'{"run_id":"' + b"a" * 32 + b'","value":1}\n'
            new = b'{"run_id":"' + b"b" * 32 + b'","value":2}\n'
            self.assertEqual(len(old), len(new))
            with open(path, "wb") as handle:
                handle.write(old)
            original = os.stat(path)
            real_stat = os.stat
            raced = False
            replacement = None

            def replace_at_path_validation(candidate, *args, **kwargs):
                nonlocal raced, replacement
                if (
                    not raced
                    and os.fspath(candidate) == path
                    and kwargs.get("follow_symlinks") is False
                    and kwargs.get("dir_fd") is None
                ):
                    raced = True
                    os.unlink(path)
                    with open(path, "xb") as handle:
                        handle.write(new)
                    replacement = real_stat(path, follow_symlinks=False)
                return real_stat(candidate, *args, **kwargs)

            with mock.patch.object(
                    bench_compare.os, "stat",
                    side_effect=replace_at_path_validation):
                with self.assertRaises(
                        bench_compare.Refusal,
                        msg="an after-close same-inode replacement was accepted"):
                    artifact = bench_compare.read_artifact_nofollow(path)
                    self.fail(
                        "reader returned old bytes after the pathname was replaced: "
                        f"original_inode={original.st_ino} "
                        f"replacement_inode={replacement.st_ino} "
                        f"returned_old={artifact['text'].encode() == old}"
                    )
            self.assertTrue(raced)
            self.assertFalse(
                os.path.samestat(original, replacement),
                "the live reader fd did not keep its inode out of reuse",
            )
            with open(path, "rb") as handle:
                self.assertEqual(handle.read(), new)

    def test_memory_reader_refuses_same_inode_mutation_at_path_validation(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "summary.json")
            old = b'{"run_id":"' + b"a" * 32 + b'","value":1}\n'
            new = b'{"run_id":"' + b"b" * 32 + b'","value":2}\n'
            self.assertEqual(len(old), len(new))
            with open(path, "wb") as handle:
                handle.write(old)
            real_stat = os.stat
            before = real_stat(path)
            raced = False
            changed = None

            def mutate_at_path_validation(candidate, *args, **kwargs):
                nonlocal raced, changed
                if (
                    not raced
                    and os.fspath(candidate) == path
                    and kwargs.get("follow_symlinks") is False
                    and kwargs.get("dir_fd") is None
                ):
                    raced = True
                    with open(path, "r+b") as handle:
                        handle.write(new)
                        handle.flush()
                        os.fsync(handle.fileno())
                    os.utime(
                        path,
                        ns=(before.st_atime_ns, before.st_mtime_ns + 1_000_000_000),
                    )
                    changed = real_stat(path, follow_symlinks=False)
                return real_stat(candidate, *args, **kwargs)

            with mock.patch.object(
                    bench_compare.os, "stat",
                    side_effect=mutate_at_path_validation):
                with self.assertRaises(
                        bench_compare.Refusal,
                        msg="same-inode bytes changed after final fstat were accepted"):
                    bench_compare.read_artifact_nofollow(path)
            self.assertTrue(raced)
            self.assertTrue(os.path.samestat(before, changed))
            self.assertEqual(changed.st_size, before.st_size)
            self.assertNotEqual(changed.st_mtime_ns, before.st_mtime_ns)
            with open(path, "rb") as handle:
                self.assertEqual(handle.read(), new)

    def assert_memory_publication_rolls_back_path_race(self, action):
        run, campaign, host, _memory = self.valid_campaign_memory_comparison()
        changed = os.path.join(campaign, "memory", "summary.json")
        replacement = changed + ".replacement"
        with open(replacement, "wb") as handle:
            handle.write(b'{"run_id":"' + b"b" * 32 + b'"}\n')
        out = os.path.join(run, "comparison.json")
        real_link = os.link
        raced = False

        def race_at_publication(source, destination, *args, **kwargs):
            nonlocal raced
            if not raced and destination == os.path.basename(out):
                if action == "replace":
                    os.replace(replacement, changed)
                else:
                    os.unlink(changed)
                raced = True
            return real_link(source, destination, *args, **kwargs)

        argv = ["compare.py", "--vm-run", run,
                "--host", f"free={host}", "--out", out]
        with mock.patch.object(
                bench_compare.os, "link", side_effect=race_at_publication), \
                mock.patch.object(sys, "argv", argv):
            with self.assertRaises(
                    bench_compare.Refusal,
                    msg=f"memory path {action} at publication was accepted"):
                bench_compare.main()
        self.assertTrue(raced)
        self.assertFalse(os.path.exists(out))

    def test_memory_publication_rolls_back_atomic_replace_race(self):
        self.assert_memory_publication_rolls_back_path_race("replace")

    def test_memory_publication_rolls_back_unlink_race(self):
        self.assert_memory_publication_rolls_back_path_race("unlink")

    def test_publication_rollback_preserves_replaced_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "comparison.json")
            attacker = os.path.join(tmp, "attacker.json")
            with open(attacker, "wb") as handle:
                handle.write(b"attacker output\n")
            target = bench_compare.open_output_target(out)
            try:
                def replace_output_then_refuse():
                    os.replace(attacker, out)
                    raise bench_compare.Refusal("input changed after publication")

                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.write_json_atomic(
                        out, {"result": "ours"}, target,
                        after_publish=replace_output_then_refuse,
                    )
            finally:
                os.close(target["directory_fd"])
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), b"attacker output\n")

    def test_publication_inode_pin_refuses_unlink_recreate_aba(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.require_immediate_inode_reuse(tmp)
            out = os.path.join(tmp, "comparison.json")
            attacker_bytes = b"attacker output\n"
            target = bench_compare.open_output_target(out)
            real_unlink = os.unlink
            raced = False
            published = None
            replacement = None

            def replace_after_temporary_unlink(candidate, *args, **kwargs):
                nonlocal raced, published, replacement
                result = real_unlink(candidate, *args, **kwargs)
                if (
                    not raced
                    and kwargs.get("dir_fd") == target["directory_fd"]
                    and candidate != target["name"]
                    and os.path.exists(out)
                ):
                    raced = True
                    published = os.stat(out, follow_symlinks=False)
                    real_unlink(target["name"], dir_fd=target["directory_fd"])
                    with open(out, "xb") as handle:
                        handle.write(attacker_bytes)
                    replacement = os.stat(out, follow_symlinks=False)
                return result

            try:
                with mock.patch.object(
                        bench_compare.os, "unlink",
                        side_effect=replace_after_temporary_unlink):
                    with self.assertRaises(
                            bench_compare.Refusal,
                            msg="publication accepted a same-inode replacement"):
                        bench_compare.write_json_atomic(
                            out, {"result": "ours"}, target
                        )
            finally:
                os.close(target["directory_fd"])
            self.assertTrue(raced)
            self.assertIsNotNone(published)
            self.assertIsNotNone(replacement)
            self.assertFalse(
                os.path.samestat(published, replacement),
                "the live writer fd did not keep its inode out of reuse",
            )
            self.assertTrue(os.path.exists(out))
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), attacker_bytes)

    def test_publication_refuses_restored_mtime_mutation_before_first_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "comparison.json")
            target = bench_compare.open_output_target(out)
            real_unlink = os.unlink
            raced = False

            def mutate_after_temporary_unlink(candidate, *args, **kwargs):
                nonlocal raced
                result = real_unlink(candidate, *args, **kwargs)
                if (
                    not raced
                    and kwargs.get("dir_fd") == target["directory_fd"]
                    and candidate != target["name"]
                    and os.path.exists(out)
                ):
                    raced = True
                    before = os.stat(out, follow_symlinks=False)
                    with open(out, "r+b") as handle:
                        raw = handle.read()
                        changed = raw.replace(b"ours", b"evil", 1)
                        self.assertNotEqual(changed, raw)
                        self.assertEqual(len(changed), len(raw))
                        handle.seek(0)
                        handle.write(changed)
                        handle.flush()
                        os.fsync(handle.fileno())
                    os.utime(
                        out,
                        ns=(before.st_atime_ns, before.st_mtime_ns),
                    )
                return result

            try:
                with mock.patch.object(
                        bench_compare.os, "unlink",
                        side_effect=mutate_after_temporary_unlink):
                    with self.assertRaises(
                            bench_compare.Refusal,
                            msg="publication adopted mutated bytes as its baseline"):
                        bench_compare.write_json_atomic(
                            out, {"result": "ours"}, target
                        )
            finally:
                os.close(target["directory_fd"])
            self.assertTrue(raced)
            self.assertFalse(os.path.exists(out))

    def test_publication_rollback_preserves_unlink_recreate_aba(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.require_immediate_inode_reuse(tmp)
            out = os.path.join(tmp, "comparison.json")
            attacker_bytes = b"attacker output\n"
            target = bench_compare.open_output_target(out)
            published = None
            replacement = None

            def replace_output_then_refuse():
                nonlocal published, replacement
                published = os.stat(out, follow_symlinks=False)
                os.unlink(out)
                with open(out, "xb") as handle:
                    handle.write(attacker_bytes)
                replacement = os.stat(out, follow_symlinks=False)
                raise bench_compare.Refusal("input changed after publication")

            try:
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.write_json_atomic(
                        out, {"result": "ours"}, target,
                        after_publish=replace_output_then_refuse,
                    )
            finally:
                os.close(target["directory_fd"])
            self.assertIsNotNone(published)
            self.assertIsNotNone(replacement)
            self.assertFalse(
                os.path.samestat(published, replacement),
                "the live rollback pin did not keep its inode out of reuse",
            )
            self.assertTrue(
                os.path.exists(out),
                "rollback deleted the replacement after inode reuse",
            )
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), attacker_bytes)

    def assert_publication_rollback_preserves_post_check_replacement(
            self, explicit_target):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "comparison.json")
            attacker = os.path.join(tmp, "attacker.json")
            attacker_bytes = b"attacker output\n"
            with open(attacker, "wb") as handle:
                handle.write(attacker_bytes)
            target = (
                bench_compare.open_output_target(out)
                if explicit_target else None
            )
            target_name = os.path.basename(out)
            target_directory_fd = (
                target["directory_fd"] if target is not None else None
            )
            real_stat = bench_compare.os.stat
            real_rename_noreplace = bench_compare.rename_noreplace
            armed = False
            raced = False

            def install_replacement():
                nonlocal raced
                raced = True
                os.replace(attacker, out)

            def replace_after_rollback_stat(candidate, *args, **kwargs):
                result = real_stat(candidate, *args, **kwargs)
                candidate_name = os.fspath(candidate)
                is_output = (
                    candidate_name == out
                    or (
                        candidate_name == target_name
                        and kwargs.get("dir_fd") == target_directory_fd
                    )
                )
                if armed and not raced and is_output:
                    install_replacement()
                return result

            def replace_before_atomic_rollback(
                    directory_fd, source, destination):
                if armed and not raced and source == target_name:
                    install_replacement()
                return real_rename_noreplace(
                    directory_fd, source, destination
                )

            def refuse_after_publication():
                nonlocal armed
                armed = True
                raise bench_compare.Refusal("input changed after publication")

            try:
                with mock.patch.object(
                        bench_compare.os, "stat",
                        side_effect=replace_after_rollback_stat), \
                     mock.patch.object(
                         bench_compare, "rename_noreplace",
                         side_effect=replace_before_atomic_rollback):
                    with self.assertRaises(bench_compare.Refusal):
                        bench_compare.write_json_atomic(
                            out, {"result": "ours"}, target,
                            after_publish=refuse_after_publication,
                        )
            finally:
                if target is not None:
                    os.close(target["directory_fd"])
            self.assertTrue(raced)
            self.assertTrue(os.path.exists(out))
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), attacker_bytes)

    def test_publication_rollback_preserves_post_check_replacement(self):
        self.assert_publication_rollback_preserves_post_check_replacement(True)

    def test_implicit_publication_rollback_preserves_post_check_replacement(self):
        self.assert_publication_rollback_preserves_post_check_replacement(False)

    def test_campaign_memory_artifacts_are_protected_inputs(self):
        for name in ("complete.json", "run.json", "samples.jsonl", "summary.json"):
            for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
                with self.subTest(artifact=name, alias=alias_kind):
                    run, campaign, host, _memory = \
                        self.valid_campaign_memory_comparison()
                    protected = os.path.join(campaign, "memory", name)
                    if alias_kind == "direct":
                        out = protected
                    elif alias_kind == "realpath":
                        alias_root = tempfile.mkdtemp()
                        self.addCleanup(shutil.rmtree, alias_root, ignore_errors=True)
                        os.symlink(campaign, os.path.join(alias_root, "campaign"))
                        out = os.path.join(alias_root, "campaign", "memory", name)
                    else:
                        out = os.path.join(
                            run, f"memory-{name}-{alias_kind}.json"
                        )
                        if alias_kind == "symlink":
                            os.symlink(protected, out)
                        else:
                            os.link(protected, out)
                    with open(protected, "rb") as handle:
                        before = handle.read()
                    proc, _out = self.run_compare(
                        run, [("free", host)], out=out
                    )
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"--out used a {alias_kind} alias of memory {name}",
                    )
                    self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                    with open(protected, "rb") as handle:
                        self.assertEqual(handle.read(), before)
                    if alias_kind in ("symlink", "hardlink"):
                        self.assertTrue(os.path.lexists(out))

    def test_campaign_completion_validates_memory_binding(self):
        mutations = {
            "missing record": lambda complete:
                complete.update(memory_complete=None),
            "undeclared phase": lambda complete:
                complete.update(phases=["hostcdp"]),
            "unsafe path": lambda complete:
                complete["memory_complete"].update(path="memory/../complete.json"),
            "boolean size": lambda complete:
                complete["memory_complete"].update(size=True),
            "invalid digest": lambda complete:
                complete["memory_complete"].update(sha256="A" * 64),
            "unexpected field": lambda complete:
                complete["memory_complete"].update(extra=True),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run, campaign, host, _memory = \
                    self.valid_campaign_memory_comparison()
                self.rewrite_campaign_complete(campaign, mutate)
                proc, out = self.run_compare(run, [("free", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"campaign accepted memory binding with {label}",
                )
                self.assertIn("campaign-complete.json", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_one_campaign_completion_binds_both_host_arms(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(value) for value in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        campaign = tempfile.mkdtemp()
        campaign_run_id = "1" * 32
        runtime_sha256 = "9" * 64
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, wall_ms=float(100 + i))
            for i in range(1, 4)
        ]
        hosts = []
        for arm, cpu_budget, cpus in (
            ("free", "unlimited", None),
            ("cpu2", "vm-matched", 2),
        ):
            temporary = self.make_host(rows, meta_overrides={
                "run_id": f"{campaign_run_id}-{arm}",
                "comparison_label": arm,
                "corpus_extra_runtime_bundle_sha256": runtime_sha256,
                "cpu_budget": cpu_budget,
                "cpus": cpus,
            })
            host = os.path.join(campaign, f"hostcdp-{arm}")
            shutil.move(temporary, host)
            hosts.append((arm, host))
        self.write_campaign_complete(
            campaign,
            [host for _arm, host in hosts],
            campaign_run_id,
            runtime_sha256,
        )
        proc, out = self.run_compare(run, hosts)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            comparison = json.load(handle)
        self.assertEqual(set(comparison["hosts"]), {"free", "cpu2"})
        marker_hashes = {
            comparison["input_identity"]["hosts"][arm]
                      ["campaign_complete_json"]["sha256"]
            for arm, _host in hosts
        }
        self.assertEqual(len(marker_hashes), 1)

    def test_campaign_host_requires_the_parent_completion_commit(self):
        run, campaign, host = self.valid_campaign_comparison()
        os.unlink(os.path.join(campaign, "campaign-complete.json"))
        self.assertTrue(os.path.exists(os.path.join(host, "complete.json")))
        proc, out = self.run_compare(run, [("free", host)])
        self.assertNotEqual(
            proc.returncode, 0,
            "leftover successful child records authorized an incomplete campaign",
        )
        self.assertIn("campaign-complete.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_corpus_extra_runtime_binding_is_explicit(self):
        invalid = {
            "missing": None,
            "empty": "",
            "uppercase": "A" * 64,
            "short": "a" * 63,
            "boolean": True,
        }
        for label, value in invalid.items():
            with self.subTest(label=label):
                run, host = self.valid_comparison()
                with open(os.path.join(host, "run.json")) as handle:
                    meta = json.load(handle)
                if label == "missing":
                    meta.pop("corpus_extra_runtime_bundle_sha256")
                else:
                    meta["corpus_extra_runtime_bundle_sha256"] = value
                with open(os.path.join(host, "run.json"), "w") as handle:
                    json.dump(meta, handle)
                self.bind_host_rows(host)
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"invalid corpus-extra runtime binding {label} was accepted",
                )
                self.assertIn(
                    "corpus_extra_runtime_bundle_sha256",
                    proc.stderr,
                    proc.stderr,
                )
                self.assertFalse(os.path.exists(out))

    def test_campaign_completion_binds_parent_run_runtime_and_child(self):
        def append_unsorted(complete):
            complete["host_completes"].insert(0, {
                "path": "hostcdp-zulu/complete.json",
                "size": 0,
                "sha256": "0" * 64,
            })

        mutations = {
            "schema version": lambda complete:
                complete.update(schema_version=3),
            "old schema version": lambda complete:
                complete.update(schema_version=1),
            "boolean schema version": lambda complete:
                complete.update(schema_version=True),
            "float schema version": lambda complete:
                complete.update(schema_version=2.0),
            "parent run": lambda complete:
                complete.update(run_id="2" * 32),
            "unsafe parent run": lambda complete:
                complete.update(run_id="not-a-campaign-run"),
            "runtime bundle": lambda complete:
                complete.update(runtime_bundle_sha256="8" * 64),
            "missing phase": lambda complete:
                complete.update(phases=[]),
            "unknown phase": lambda complete:
                complete.update(phases=["hostcdp", "unknown"]),
            "duplicate phase": lambda complete:
                complete.update(phases=["hostcdp", "hostcdp"]),
            "host child without hostcdp phase": lambda complete:
                complete.update(phases=["memory"], memory_complete=None),
            "missing selected child": lambda complete:
                complete.update(host_completes=[]),
            "duplicate selected child": lambda complete:
                complete["host_completes"].append(
                    dict(complete["host_completes"][0])),
            "unsorted children": append_unsorted,
            "child size": lambda complete:
                complete["host_completes"][0].update(
                    size=complete["host_completes"][0]["size"] + 1),
            "boolean child size": lambda complete:
                complete["host_completes"][0].update(size=True),
            "child digest": lambda complete:
                complete["host_completes"][0].update(sha256="0" * 64),
            "uppercase child digest": lambda complete:
                complete["host_completes"][0].update(sha256="A" * 64),
            "unsafe child path": lambda complete:
                complete["host_completes"][0].update(
                    path="hostcdp-free/../complete.json"),
            "absolute child path": lambda complete:
                complete["host_completes"][0].update(
                    path="/hostcdp-free/complete.json"),
            "unexpected top-level field": lambda complete:
                complete.update(extra=True),
            "unexpected child field": lambda complete:
                complete["host_completes"][0].update(extra=True),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run, campaign, host = self.valid_campaign_comparison()
                self.rewrite_campaign_complete(campaign, mutate)
                proc, out = self.run_compare(run, [("free", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"campaign-complete with invalid {label} authorized output",
                )
                self.assertIn("campaign-complete.json", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_campaign_run_id_is_derived_from_the_bound_child_arm(self):
        run, campaign, host = self.valid_campaign_comparison()
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        meta["run_id"] = "unrelated-child-run"
        with open(os.path.join(host, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        self.write_campaign_complete(
            campaign, [host], "1" * 32, "9" * 64
        )
        proc, out = self.run_compare(run, [("free", host)])
        self.assertNotEqual(
            proc.returncode, 0,
            "campaign completion did not bind the child run_id to its arm",
        )
        self.assertIn("run_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_campaign_completion_is_a_protected_input(self):
        for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
            with self.subTest(alias=alias_kind):
                run, campaign, host = self.valid_campaign_comparison()
                marker = os.path.join(campaign, "campaign-complete.json")
                if alias_kind == "direct":
                    out = marker
                elif alias_kind == "realpath":
                    alias_root = tempfile.mkdtemp()
                    os.symlink(campaign, os.path.join(alias_root, "campaign"))
                    out = os.path.join(
                        alias_root, "campaign", "campaign-complete.json"
                    )
                else:
                    out = os.path.join(run, f"campaign-{alias_kind}.json")
                    if alias_kind == "symlink":
                        os.symlink(marker, out)
                    else:
                        os.link(marker, out)
                with open(marker, "rb") as handle:
                    before = handle.read()
                proc, _out = self.run_compare(
                    run, [("free", host)], out=out
                )
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"--out used a {alias_kind} alias of campaign completion",
                )
                self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                with open(marker, "rb") as handle:
                    self.assertEqual(handle.read(), before)

    def test_campaign_authorization_is_rechecked_at_publication(self):
        actions = {
            "campaign completion changed": lambda campaign, host:
                self.rewrite_campaign_complete(
                    campaign,
                    lambda complete: complete.update(runtime_bundle_sha256="8" * 64),
                ),
            "child completion changed": lambda _campaign, host:
                self.rewrite_host_complete(
                    host, lambda complete: complete.update(run_id="changed")
                ),
            "campaign withdrawn": lambda campaign, _host:
                open(os.path.join(campaign, "WITHDRAWN"), "w").close(),
        }
        for label, action in actions.items():
            with self.subTest(label=label):
                run, campaign, host = self.valid_campaign_comparison()
                out = os.path.join(run, "comparison.json")
                original = bench_compare.write_json_atomic

                def change_input_before_publication(*args, **kwargs):
                    action(campaign, host)
                    return original(*args, **kwargs)

                argv = ["compare.py", "--vm-run", run,
                        "--host", f"free={host}", "--out", out]
                with mock.patch.object(
                        bench_compare, "write_json_atomic",
                        side_effect=change_input_before_publication), \
                        mock.patch.object(sys, "argv", argv):
                    with self.assertRaises(
                            bench_compare.Refusal,
                            msg=f"{label} raced comparison publication"):
                        bench_compare.main()
                self.assertFalse(os.path.exists(out))

    def test_withdrawal_writers_cannot_cross_the_final_comparison_check(self):
        """The VM run, host run and campaign parent stay shared-locked until
        comparison publication returns. Otherwise an exclusive writer can add
        WITHDRAWN after the post-publication callback has already accepted the
        inputs, leaving the just-written comparison visible.

        RED BEFORE THE FIX: each exclusive writer acquired its directory and
        left WITHDRAWN beside a successful comparison.
        """
        for location in ("vm", "host", "campaign"):
            with self.subTest(location=location):
                run, campaign, host = self.valid_campaign_comparison()
                directory = {
                    "vm": run,
                    "host": host,
                    "campaign": campaign,
                }[location]
                marker = os.path.join(directory, "WITHDRAWN")
                out = os.path.join(run, "comparison.json")
                original = bench_compare.write_json_atomic
                writer_status = []

                def publish_then_try_writer(*args, **kwargs):
                    result = original(*args, **kwargs)
                    writer = subprocess.run(
                        [
                            "flock", "-n", "-x", directory,
                            "sh", "-c", 'printf "%s\\n" late > "$1"',
                            "sh", marker,
                        ],
                        capture_output=True,
                        text=True,
                        timeout=10,
                    )
                    writer_status.append(writer.returncode)
                    return result

                argv = [
                    "compare.py", "--vm-run", run,
                    "--host", f"free={host}", "--out", out,
                ]
                with mock.patch.object(
                        bench_compare, "write_json_atomic",
                        side_effect=publish_then_try_writer), \
                     mock.patch.object(sys, "argv", argv), \
                     mock.patch.object(sys, "stdout", new=io.StringIO()):
                    bench_compare.main()

                self.assertEqual(writer_status, [1])
                self.assertTrue(os.path.isfile(out))
                self.assertFalse(os.path.lexists(marker))
                released = subprocess.run(
                    ["flock", "-n", "-x", directory, "true"],
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(
                    released.returncode, 0,
                    f"comparison leaked its {location} directory lock",
                )

    def test_comparison_reads_and_publishes_against_locked_run_directories(self):
        """A pathname replacement cannot escape a run directory's lock."""
        for location in ("vm", "host", "campaign"):
            with self.subTest(location=location):
                run, campaign, host = self.valid_campaign_comparison()
                directory = {
                    "vm": run,
                    "host": host,
                    "campaign": campaign,
                }[location]
                displaced = directory + "-before-replacement"
                out = os.path.join(os.path.dirname(run), f"{location}-comparison.json")
                original = bench_compare.reject_output_alias
                replaced = False

                def replace_after_locks(*args, **kwargs):
                    nonlocal replaced
                    if not replaced:
                        os.rename(directory, displaced)
                        shutil.copytree(displaced, directory)
                        replaced = True
                    return original(*args, **kwargs)

                argv = [
                    "compare.py", "--vm-run", run,
                    "--host", f"free={host}", "--out", out,
                ]
                with mock.patch.object(
                        bench_compare, "reject_output_alias",
                        side_effect=replace_after_locks), \
                     mock.patch.object(sys, "argv", argv), \
                     mock.patch.object(sys, "stdout", new=io.StringIO()):
                    with self.assertRaisesRegex(
                            bench_compare.Refusal,
                            "changed after its directory lock was acquired|locked host"):
                        bench_compare.main()

                self.assertTrue(replaced)
                self.assertFalse(os.path.lexists(out))

    def test_comparison_publishes_stable_display_input_paths(self):
        run, campaign, host = self.valid_campaign_comparison()
        out = os.path.join(os.path.dirname(run), "display-path-comparison.json")

        proc, _ = self.run_compare(run, [("free", host)], out=out)

        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        with open(out) as handle:
            identities = json.load(handle)["input_identity"]
        self.assertNotIn("/proc/self/fd/", json.dumps(identities))
        self.assertEqual(
            identities["analysis_json"]["path"],
            os.path.join(run, "analysis.json"),
        )
        self.assertEqual(
            identities["analysis_json"]["realpath"],
            os.path.realpath(os.path.join(run, "analysis.json")),
        )
        self.assertEqual(
            identities["hosts"]["free"]["run_json"]["path"],
            os.path.join(host, "run.json"),
        )
        self.assertEqual(
            identities["hosts"]["free"]["campaign_complete_json"]["path"],
            os.path.join(campaign, "campaign-complete.json"),
        )

    def test_replacement_input_alias_is_preserved_before_identity_refusal(self):
        for location in ("vm", "host", "campaign"):
            with self.subTest(location=location):
                run, campaign, host = self.valid_campaign_comparison()
                directory, relative = {
                    "vm": (run, "analysis.json"),
                    "host": (host, "run.json"),
                    "campaign": (campaign, "campaign-complete.json"),
                }[location]
                out = os.path.join(directory, relative)
                with open(out, "rb") as handle:
                    expected = handle.read()
                displaced = directory + "-before-output-open"
                original = bench_compare.open_output_target
                replaced = False

                def replace_before_output_open(path):
                    nonlocal replaced
                    if not replaced:
                        os.rename(directory, displaced)
                        shutil.copytree(displaced, directory)
                        replaced = True
                    return original(path)

                argv = [
                    "compare.py", "--vm-run", run,
                    "--host", f"free={host}", "--out", out,
                ]
                with mock.patch.object(
                        bench_compare, "open_output_target",
                        side_effect=replace_before_output_open), \
                     mock.patch.object(sys, "argv", argv):
                    with self.assertRaises(bench_compare.Refusal):
                        bench_compare.main()

                self.assertTrue(replaced)
                with open(out, "rb") as handle:
                    self.assertEqual(
                        handle.read(), expected,
                        "alias preflight deleted a replacement input",
                    )

    def test_run_identity_is_checked_after_final_output_validation(self):
        run, host = self.valid_comparison()
        displaced = run + "-after-output-validation"
        out = os.path.join(os.path.dirname(run), "late-run-replacement.json")
        original = bench_compare.validate_published_output
        calls = 0

        def validate_then_replace(*args, **kwargs):
            nonlocal calls
            result = original(*args, **kwargs)
            calls += 1
            if calls == 2:
                os.rename(run, displaced)
                shutil.copytree(displaced, run)
            return result

        argv = [
            "compare.py", "--vm-run", run,
            "--host", f"host={host}", "--out", out,
        ]
        with mock.patch.object(
                bench_compare, "validate_published_output",
                side_effect=validate_then_replace), \
             mock.patch.object(sys, "argv", argv), \
             mock.patch.object(sys, "stdout", new=io.StringIO()):
            with self.assertRaisesRegex(
                    bench_compare.Refusal,
                    "changed after its directory lock was acquired"):
                bench_compare.main()

        self.assertEqual(calls, 2)
        self.assertFalse(os.path.lexists(out))

    def test_final_validation_refuses_labeled_campaign_child_replacement(self):
        run, campaign, host = self.valid_campaign_comparison()
        holding = os.path.join(campaign, "holding")
        decoy = os.path.join(campaign, "decoy")
        shutil.copytree(host, decoy)
        out = os.path.join(os.path.dirname(run), "late-host-shuffle.json")
        original = bench_compare.validate_published_output
        calls = 0

        def validate_then_shuffle(*args, **kwargs):
            nonlocal calls
            result = original(*args, **kwargs)
            calls += 1
            if calls == 2:
                os.rename(host, holding)
                os.rename(decoy, host)
            return result

        argv = [
            "compare.py", "--vm-run", run,
            "--host", f"free={host}", "--out", out,
        ]
        with mock.patch.object(
                bench_compare, "validate_published_output",
                side_effect=validate_then_shuffle), \
             mock.patch.object(sys, "argv", argv), \
             mock.patch.object(sys, "stdout", new=io.StringIO()):
            with self.assertRaisesRegex(
                    bench_compare.Refusal,
                    "changed after its directory lock was acquired|"
                    "campaign child hostcdp-free"):
                bench_compare.main()

        self.assertEqual(calls, 2)
        self.assertFalse(os.path.lexists(out))

    def test_campaign_alias_cannot_mask_a_labeled_child_replacement(self):
        run, campaign, host = self.valid_campaign_comparison()
        caller = os.path.join(run, "selected-host")
        os.symlink(host, caller)
        holding = os.path.join(campaign, "holding")
        decoy = os.path.join(campaign, "decoy")
        shutil.copytree(host, decoy)
        out = os.path.join(os.path.dirname(run), "intercheck-host-shuffle.json")
        original = bench_compare.campaign_summary.locked_run_directory_errors
        raced = False

        def shuffle_then_check(run_dirs):
            nonlocal raced
            os.rename(host, holding)
            os.rename(decoy, host)
            os.unlink(caller)
            os.symlink(holding, caller)
            raced = True
            return original(run_dirs)

        argv = [
            "compare.py", "--vm-run", run,
            "--host", f"free={caller}", "--out", out,
        ]
        with mock.patch.object(
                bench_compare.campaign_summary,
                "locked_run_directory_errors",
                side_effect=shuffle_then_check), \
             mock.patch.object(sys, "argv", argv), \
             mock.patch.object(sys, "stdout", new=io.StringIO()):
            with self.assertRaisesRegex(
                    bench_compare.Refusal,
                    "canonical campaign child|campaign child hostcdp-free"):
                bench_compare.main()

        self.assertFalse(raced, "a campaign alias reached the publication boundary")
        self.assertFalse(os.path.lexists(out))

    def test_canonical_campaign_child_is_checked_after_pinned_relations(self):
        run, campaign, host = self.valid_campaign_comparison()
        moved = campaign + "-before-final-canonical-check"
        out = os.path.join(os.path.dirname(run), "late-campaign-replacement.json")
        original = bench_compare.validate_locked_host_campaign
        parent_checks = 0
        raced = False

        def replace_campaign_on_final_parent(
                host_run, campaign_run, label=None):
            nonlocal parent_checks, raced
            if label is None:
                parent_checks += 1
                if parent_checks == 2:
                    os.rename(campaign, moved)
                    shutil.copytree(moved, campaign)
                    raced = True
            return original(host_run, campaign_run, label)

        argv = [
            "compare.py", "--vm-run", run,
            "--host", f"free={host}", "--out", out,
        ]
        with mock.patch.object(
                bench_compare,
                "validate_locked_host_campaign",
                side_effect=replace_campaign_on_final_parent), \
             mock.patch.object(sys, "argv", argv), \
             mock.patch.object(sys, "stdout", new=io.StringIO()):
            with self.assertRaisesRegex(
                    bench_compare.Refusal,
                    "canonical campaign child|changed after its directory lock"):
                bench_compare.main()

        self.assertTrue(raced, "the late campaign replacement was not exercised")
        self.assertFalse(os.path.lexists(out))

    def test_pinned_host_withdrawal_check_does_not_dereference_to_a_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = os.path.join(tmp, "host")
            moved = os.path.join(tmp, "moved")
            os.mkdir(host)
            with open(os.path.join(host, "WITHDRAWN"), "w") as handle:
                handle.write("withdrawn\n")
            descriptor = os.open(host, os.O_RDONLY | os.O_DIRECTORY)
            identity = os.fstat(descriptor)
            pinned = bench_campaign_summary.LockedRunDirectory(
                host, descriptor, (identity.st_dev, identity.st_ino))
            original = bench_compare.os.path.realpath
            raced = False

            def move_after_dereference(path):
                nonlocal raced
                result = original(path)
                if os.fspath(path) == os.fspath(pinned):
                    os.rename(host, moved)
                    os.mkdir(host)
                    raced = True
                return result

            try:
                with mock.patch.object(
                        bench_compare.os.path, "realpath",
                        side_effect=move_after_dereference):
                    with self.assertRaisesRegex(
                            bench_compare.Refusal, "WITHDRAWN"):
                        bench_compare.reject_withdrawn(pinned, tmp)
            finally:
                os.close(descriptor)
            self.assertFalse(
                raced,
                "withdrawal check converted the pinned directory to a pathname",
            )

    def test_locked_host_parent_must_be_its_locked_campaign(self):
        with tempfile.TemporaryDirectory() as tmp:
            campaign_a = os.path.join(tmp, "campaign-a")
            campaign_b = os.path.join(tmp, "campaign-b")
            host_a = os.path.join(campaign_a, "host")
            host_b = os.path.join(campaign_b, "host")
            vm = os.path.join(tmp, "vm")
            for directory in (campaign_a, campaign_b, host_a, host_b, vm):
                os.makedirs(directory, exist_ok=True)
            link = os.path.join(tmp, "selected-host")
            os.symlink(host_a, link)
            os.unlink(link)
            os.symlink(host_b, link)
            paths = (vm, host_b, campaign_a)
            descriptors = [
                os.open(path, os.O_RDONLY | os.O_DIRECTORY) for path in paths
            ]
            locked = []
            try:
                for path, descriptor in zip(paths, descriptors):
                    info = os.fstat(descriptor)
                    locked.append(bench_campaign_summary.LockedRunDirectory(
                        path, descriptor, (info.st_dev, info.st_ino)))
                args = SimpleNamespace(
                    vm_run=vm, host=[f"free={link}"], out="unused")
                with self.assertRaisesRegex(
                        bench_compare.Refusal, "locked campaign directory"):
                    bench_compare.bind_locked_comparison_directories(args, locked)
            finally:
                for descriptor in descriptors:
                    os.close(descriptor)

    def test_load_host_dataset_keeps_campaign_withdrawal_checks_pinned(self):
        _run, campaign, host = self.valid_campaign_comparison()
        descriptors = [
            os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            for path in (host, campaign)
        ]
        pinned = []
        for path, descriptor in zip((host, campaign), descriptors):
            info = os.fstat(descriptor)
            pinned.append(bench_campaign_summary.LockedRunDirectory(
                path, descriptor, (info.st_dev, info.st_ino)))
        original = bench_compare.reject_withdrawn

        def require_campaign_pin(directory, campaign_directory=None):
            if os.fspath(directory).startswith("/proc/self/fd/"):
                self.assertIsNotNone(
                    campaign_directory,
                    "pinned host withdrawal check lost its campaign fd",
                )
            return original(directory, campaign_directory)

        try:
            with mock.patch.object(
                    bench_compare, "reject_withdrawn",
                    side_effect=require_campaign_pin):
                bench_compare.load_host_dataset(
                    pinned[0], require_driver=True,
                    campaign_directory=pinned[1],
                )
        finally:
            for descriptor in descriptors:
                os.close(descriptor)

    def test_locked_campaign_arm_cannot_be_spoofed_by_realpath_aba(self):
        _run, campaign, host = self.valid_campaign_comparison()
        run_json = os.path.join(host, "run.json")
        with open(run_json) as handle:
            meta = json.load(handle)
        meta["comparison_label"] = "cpu2"
        meta["run_id"] = "1" * 32 + "-cpu2"
        with open(run_json, "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        complete_path = os.path.join(campaign, "campaign-complete.json")
        with open(complete_path) as handle:
            complete = json.load(handle)
        child_complete = os.path.join(host, "complete.json")
        with open(child_complete, "rb") as handle:
            child_bytes = handle.read()
        complete["host_completes"][0].update(
            path="hostcdp-cpu2/complete.json",
            size=len(child_bytes),
            sha256=hashlib.sha256(child_bytes).hexdigest(),
        )
        with open(complete_path, "w") as handle:
            json.dump(complete, handle)

        descriptors = [
            os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            for path in (host, campaign)
        ]
        pinned = []
        for path, descriptor in zip((host, campaign), descriptors):
            info = os.fstat(descriptor)
            pinned.append(bench_campaign_summary.LockedRunDirectory(
                path, descriptor, (info.st_dev, info.st_ino)))
        original = bench_compare.os.path.realpath

        def spoof_arm(path):
            if path == host:
                return os.path.join(campaign, "hostcdp-cpu2")
            return original(path)

        try:
            with mock.patch.object(
                    bench_compare.os.path, "realpath", side_effect=spoof_arm):
                with self.assertRaisesRegex(
                        bench_compare.Refusal, "locked host|hostcdp-cpu2"):
                    bench_compare.load_host_dataset(
                        pinned[0], require_driver=True,
                        campaign_directory=pinned[1],
                    )
        finally:
            for descriptor in descriptors:
                os.close(descriptor)

    def test_display_identity_keeps_the_read_time_canonical_realpath(self):
        identity = {
            "path": "/proc/self/fd/10/run.json",
            "realpath": "/stable/campaign/hostcdp-free/run.json",
            "size": 1,
            "sha256": "a" * 64,
        }
        with mock.patch.object(
                bench_compare.os.path, "realpath",
                return_value="/spoof/campaign/hostcdp-cpu2/run.json"):
            displayed = bench_compare.display_artifact_identity(
                identity,
                [("/proc/self/fd/10", "/caller/hostcdp-free")],
            )
        self.assertEqual(
            displayed["realpath"],
            "/stable/campaign/hostcdp-free/run.json",
        )

    def test_vm_inputs_are_rechecked_at_publication(self):
        for filename in ("analysis.json", "reqbench.jsonl"):
            with self.subTest(filename=filename):
                run, host = self.valid_comparison()
                out = os.path.join(run, "comparison.json")
                changed = os.path.join(run, filename)
                original = bench_compare.write_json_atomic

                def change_vm_input_before_publication(*args, **kwargs):
                    with open(changed, "a") as handle:
                        handle.write(" ")
                    return original(*args, **kwargs)

                argv = ["compare.py", "--vm-run", run,
                        "--host", f"host={host}", "--out", out]
                with mock.patch.object(
                        bench_compare, "write_json_atomic",
                        side_effect=change_vm_input_before_publication), \
                        mock.patch.object(sys, "argv", argv):
                    with self.assertRaises(
                            bench_compare.Refusal,
                            msg=f"changed {filename} raced publication"):
                        bench_compare.main()
                self.assertFalse(os.path.exists(out))

    def test_vm_withdrawal_is_rechecked_after_contract_loading(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        marker = os.path.join(run, "WITHDRAWN")
        marker_bytes = b"withdrawn after contract loading\n"
        out = os.path.join(run, "comparison.json")
        original = bench_campaign_summary.load_cell

        def withdraw_after_load(*args, **kwargs):
            result = original(*args, **kwargs)
            with open(marker, "wb") as handle:
                handle.write(marker_bytes)
            return result

        argv = ["compare.py", "--vm-run", run, "--out", out]
        with mock.patch.object(
                bench_campaign_summary, "load_cell",
                side_effect=withdraw_after_load), mock.patch.object(
                bench_compare, "load_vm",
                side_effect=AssertionError(
                    "comparison continued after the VM was withdrawn"
                )), mock.patch.object(sys, "argv", argv):
            with self.assertRaisesRegex(
                    bench_compare.Refusal, "WITHDRAWN|withdrawn"):
                bench_compare.main()
        with open(marker, "rb") as handle:
            self.assertEqual(handle.read(), marker_bytes)
        self.assertFalse(os.path.exists(out))

    def test_vm_withdrawal_is_rechecked_immediately_before_publication(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        marker = os.path.join(run, "WITHDRAWN")
        marker_bytes = b"withdrawn before publication\n"
        out = os.path.join(run, "comparison.json")
        original = bench_compare.write_json_atomic

        def withdraw_before_publication(*args, **kwargs):
            with open(marker, "wb") as handle:
                handle.write(marker_bytes)
            return original(*args, **kwargs)

        argv = ["compare.py", "--vm-run", run, "--out", out]
        with mock.patch.object(
                bench_compare, "write_json_atomic",
                side_effect=withdraw_before_publication), mock.patch.object(
                sys, "argv", argv):
            with self.assertRaisesRegex(
                    bench_compare.Refusal, "WITHDRAWN|withdrawn"):
                bench_compare.main()
        with open(marker, "rb") as handle:
            self.assertEqual(handle.read(), marker_bytes)
        self.assertFalse(os.path.exists(out))

    def test_host_completion_commit_is_required(self):
        run, host = self.valid_comparison()
        os.unlink(os.path.join(host, "complete.json"))
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "an interrupted host producer was compared")
        self.assertIn("complete.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_completion_commit_binds_run_rows_and_run_id(self):
        mutations = {
            "schema_version": lambda complete:
                complete.update(schema_version=2),
            "boolean schema_version": lambda complete:
                complete.update(schema_version=True),
            "float schema_version": lambda complete:
                complete.update(schema_version=1.0),
            "run_id": lambda complete:
                complete.update(run_id="another-run"),
            "run.json size": lambda complete:
                complete["artifacts"]["run.json"].update(
                    size=complete["artifacts"]["run.json"]["size"] + 1),
            "run.json sha256": lambda complete:
                complete["artifacts"]["run.json"].update(sha256="0" * 64),
            "hostcdp.jsonl size": lambda complete:
                complete["artifacts"]["hostcdp.jsonl"].update(
                    size=complete["artifacts"]["hostcdp.jsonl"]["size"] + 1),
            "hostcdp.jsonl sha256": lambda complete:
                complete["artifacts"]["hostcdp.jsonl"].update(sha256="0" * 64),
            "missing artifact": lambda complete:
                complete["artifacts"].pop("hostcdp.jsonl"),
            "unexpected key": lambda complete:
                complete.update(extra=True),
            "boolean size": lambda complete:
                complete["artifacts"]["run.json"].update(size=True),
            "uppercase digest": lambda complete:
                complete["artifacts"]["run.json"].update(sha256="A" * 64),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                run, host = self.valid_comparison()
                self.rewrite_host_complete(host, mutate)
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"complete.json with invalid {label} authorized comparison",
                )
                self.assertIn("complete.json", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_withdrawn_host_input_or_campaign_is_refused(self):
        for location in ("host", "campaign", "campaign-via-symlink"):
            with self.subTest(location=location):
                run, original_host = self.valid_comparison()
                if location == "host":
                    host = original_host
                    marker = os.path.join(host, "WITHDRAWN")
                else:
                    campaign = tempfile.mkdtemp()
                    physical_host = os.path.join(campaign, "host")
                    shutil.move(original_host, physical_host)
                    marker = os.path.join(campaign, "WITHDRAWN")
                    if location == "campaign-via-symlink":
                        alias_root = tempfile.mkdtemp()
                        host = os.path.join(alias_root, "host-alias")
                        os.symlink(physical_host, host)
                    else:
                        host = physical_host
                with open(marker, "w") as handle:
                    handle.write("fixture was withdrawn\n")
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a WITHDRAWN marker in the {location} was ignored",
                )
                self.assertIn("WITHDRAWN", proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_every_host_row_requires_a_successful_load_capture(self):
        mutations = {
            "measurement_valid": lambda row: row.update(measurement_valid=False),
            "loadavg1_read_status": lambda row:
                row.update(loadavg1_read_status=1),
        }
        for field, mutate in mutations.items():
            with self.subTest(field=field):
                run, host = self.valid_comparison()
                with open(os.path.join(host, "hostcdp.jsonl")) as handle:
                    rows = [json.loads(line) for line in handle]
                mutate(rows[0])
                self.bind_host_rows(host, rows)
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a row with invalid {field} entered the comparison",
                )
                self.assertIn(field, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_host_environment_and_producer_must_match_the_vm_arm(self):
        mutations = {
            "host_boot_id": ("00000000-0000-4000-8000-000000000099", "boot"),
            "host_machine": ("x86_64", "machine"),
            "host_kernel": ("totally-other-kernel", "kernel"),
            "source_revision": ("9" * 40, "source_revision"),
            "harness_sha256": ("9" * 64, "harness_sha256"),
            "runtime_bundle_sha256": ("9" * 64, "runtime_bundle_sha256"),
            "hostcdp_sha256": (None, "hostcdp_sha256"),
            "driver": ("different-driver.py", "driver"),
            "network": ("bridge", "network"),
        }
        for field, (value, message) in mutations.items():
            with self.subTest(field=field):
                run, host = self.valid_comparison(
                    meta_overrides={field: value})
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"host {field} was not part of compatibility",
                )
                self.assertIn(message, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_host_cpu_budget_and_output_label_are_explicit(self):
        mutations = (
            ({"comparison_label": "not-host"}, "label"),
            ({"cpu_budget": "unlimited", "cpus": 2}, "cpu_budget"),
            ({"cpu_budget": "vm-matched", "cpus": 3}, "cpus"),
            ({"cpu_budget": "vm-matched", "cpus": "2"}, "cpus"),
        )
        for overrides, message in mutations:
            with self.subTest(overrides=overrides):
                run, host = self.valid_comparison(meta_overrides=overrides)
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    "an ambiguous host CPU arm was labelled and ratioed",
                )
                self.assertIn(message, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_the_same_host_dataset_cannot_fill_two_comparison_arms(self):
        run, host = self.valid_comparison()
        proc, out = self.run_compare(
            run, [("host", host), ("second-label", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "one host recording filled two ratio arms")
        self.assertIn("same host dataset", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_arms_must_name_the_same_hostcdp_producer(self):
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, wall_ms=float(100 + i))
            for i in range(1, 4)
        ]
        run = self.make_run(
            vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        free = self.make_host(rows, meta_overrides={
            "comparison_label": "free", "cpu_budget": "unlimited",
            "cpus": None, "hostcdp_sha256": "f" * 64,
        })
        cpu2 = self.make_host(rows, meta_overrides={
            "comparison_label": "cpu2", "cpu_budget": "vm-matched",
            "cpus": 2, "hostcdp_sha256": "e" * 64,
        })
        proc, out = self.run_compare(
            run, [("free", free), ("cpu2", cpu2)])
        self.assertNotEqual(proc.returncode, 0,
                            "two host producers were presented as one comparison")
        self.assertIn("hostcdp_sha256", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_truncated_successful_host_prefix_is_refused(self):
        rows = [self.host_rep(0, True), self.host_rep(1, False),
                self.host_rep(2, False)]
        run, host = self.valid_comparison(host_rows=rows)
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        meta.update(reps=3, warmup=1, total_reps=4)
        with open(os.path.join(host, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "compare treated a successful prefix as a completed host arm")
        self.assertIn("total_reps", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_rows_must_belong_to_their_run_metadata(self):
        """Separate file hashes do not prevent a metadata/rows splice."""
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, wall_ms=float(100 + i))
            for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        other = self.make_host(
            rows, meta_overrides={"resolve_all_to": "203.0.113.9"})
        with open(os.path.join(other, "hostcdp.jsonl"), "rb") as handle:
            other_rows = handle.read()
        with open(os.path.join(host, "hostcdp.jsonl"), "wb") as handle:
            handle.write(other_rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "rows from another run were attributed to this run.json")
        self.assertIn("complete.json", proc.stderr, proc.stderr)
        self.assertIn("hostcdp.jsonl", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_measured_count_must_match_the_vm_arm(self):
        host_rows = [self.host_rep(0, True), self.host_rep(1, False),
                     self.host_rep(2, False)]
        run, host = self.valid_comparison(host_rows=host_rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different host and VM sample counts were ratioed")
        self.assertIn("measured", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_warmup_count_must_match_the_vm_arm(self):
        run, host = self.valid_comparison(vm_warmup=2)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "host and VM runs with different warmup schedules were ratioed")
        self.assertIn("warmup", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_corpus_must_match_the_vm_arm(self):
        other = "https://different.example/"
        rows = [self.host_rep(0, True, url=other)] + [
            self.host_rep(i, False, url=other) for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different corpora were presented as one comparison")
        self.assertIn("corpus", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_image_identity_must_match_the_vm_arm(self):
        run, host = self.valid_comparison(meta_overrides={"image_id": "b" * 64})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different container images were ratioed")
        self.assertIn("image_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_image_identity_must_be_present(self):
        run, host = self.valid_comparison(meta_overrides={"image_id": None})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("image_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def resolver_compatibility_inputs(self, host_resolver):
        cell = dict(
            self.CELL,
            url=self.HOSTNAME_URL,
            urls=[self.HOSTNAME_URL],
            guest_dns="10.0.2.2",
        )
        vm_rep = self.vm_rep(700.0)
        vm_rep["render"]["url"] = self.HOSTNAME_URL
        run = self.make_run(vm_rows=[vm_rep], cell=cell)
        with open(os.path.join(run, "analysis.json")) as handle:
            analysis = json.load(handle)
        vm_meta, vm_rows, _identity = bench_compare.load_vm(run, analysis)

        host = self.make_host(
            [self.host_rep(0, False, url=self.HOSTNAME_URL)],
            meta_overrides={"resolve_all_to": host_resolver},
        )
        host_meta, _records, host_rows, counts, _identities = (
            bench_compare.load_host_dataset(host, require_driver=True)
        )
        return vm_meta, vm_rows, host_meta, host_rows, counts

    def test_host_resolver_identity_must_match_the_vm_arm(self):
        vm_meta, vm_rows, host_meta, host_rows, counts = (
            self.resolver_compatibility_inputs("203.0.113.9")
        )
        with self.assertRaisesRegex(bench_compare.Refusal, "resolver"):
            bench_compare.validate_host_compatibility(
                "host", host_meta, host_rows, counts, vm_meta, vm_rows
            )

    def test_host_resolver_identity_must_be_present_for_a_hostname_corpus(self):
        vm_meta, vm_rows, host_meta, host_rows, counts = (
            self.resolver_compatibility_inputs(None)
        )
        with self.assertRaisesRegex(bench_compare.Refusal, "resolver"):
            bench_compare.validate_host_compatibility(
                "host", host_meta, host_rows, counts, vm_meta, vm_rows
            )

    def test_a_legacy_total_count_is_interpreted_unambiguously(self):
        """Before total_reps existed, run.json's reps field meant total.

        The tracked 2026-08-30 host records use this documented schema:
        reps=230, warmup=28 means 202 measured. The compatibility rule is
        deterministic and must not reject those complete archived bytes.
        """
        run, host = self.valid_comparison()
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        del meta["total_reps"]
        meta["reps"] += meta["warmup"]
        with open(os.path.join(host, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["hosts"]["host"]["wall_ms"]["n"], 3)

    def test_every_host_success_must_have_complete_driver_metrics(self):
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, complete_driver=(i != 2)) for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "a missing driver measurement silently reduced its own n")
        self.assertIn("total_ms", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))


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
        with open(EXTRA) as handle:
            src = handle.read()
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
        for name in ("corpus_extra.sh", "corpus_mem.py", "hostcdp.sh",
                     "cdpdrive.py", "render.py", "corpus_serve.py", "report.py",
                     "reqbench.py", "reqbench.sh", "reqanalyze.py", "wddrive.py",
                     "owned_process.py", "phase_supervisor.py",
                     "host_resource_finalizer.py", "serve_guardian.py",
                     "corpus_campaign.sh", "fcvm"):
            if bench_has_files:
                with open(os.path.join(bench, name), "w") as handle:
                    handle.write(name)
        with open(os.path.join(tmp, "repo", "target", "release", "fcvm"), "w") as handle:
            handle.write("x")
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
                    'SOURCE_REVISION=abc123\nSOURCE_GIT_DIRTY=\n'
                    'CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n'
                    'REQBENCH_RUNTIME_BUNDLE_SHA256=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n'
                    'RUNTIME_IMAGE=sha256:deadbeef\n'
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


class CorpusExtraRuntimeBundle(unittest.TestCase):
    """The recorded scripts and binary are the immutable bytes that execute."""

    SOURCES = (
        "corpus_extra.sh", "corpus_mem.py", "hostcdp.sh", "cdpdrive.py",
        "render.py", "corpus_serve.py", "report.py", "reqbench.py",
        "reqbench.sh", "reqanalyze.py", "wddrive.py", "owned_process.py",
        "phase_supervisor.py", "host_resource_finalizer.py", "serve_guardian.py",
        "corpus_campaign.sh",
    )
    REQBENCH_SOURCES = (
        "fcvm", "fc-agent", "reqbench.sh", "reqbench.py", "reqanalyze.py",
        "cdpdrive.py", "render.py", "wddrive.py",
    )

    def test_staged_bundle_is_independent_of_later_repository_edits(self):
        with open(EXTRA) as handle:
            source = handle.read()
        match = re.search(r'^stage_runtime_bundle\(\) \{\n.*?^\}', source,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, "corpus-extra has no runtime staging function")
        with tempfile.TemporaryDirectory() as tmp:
            bench = os.path.join(tmp, "source")
            repo = os.path.join(tmp, "repo")
            results = os.path.join(tmp, "results")
            os.makedirs(bench)
            os.makedirs(results)
            os.makedirs(os.path.join(repo, "target", "release"))
            corpus = os.path.join(bench, "corpus-live")
            os.makedirs(corpus)
            with open(os.path.join(corpus, "page.html"), "w") as handle:
                handle.write("original corpus\n")
            for name in self.SOURCES:
                with open(os.path.join(bench, name), "w") as handle:
                    handle.write(f"original {name}\n")
            fcvm = os.path.join(repo, "target", "release", "fcvm")
            with open(fcvm, "w") as handle:
                handle.write("original fcvm\n")
            fc_agent = os.path.join(repo, "target", "release", "fc-agent")
            with open(fc_agent, "w") as handle:
                handle.write("original fc-agent\n")
            script = (
                "set -euo pipefail\n"
                f"SOURCE_BENCH={bench!r}\nREPO={repo!r}\nRESULTS={results!r}\n"
                + match.group(0) + "\n"
                + "stage_runtime_bundle\nprintf '%s\\n%s\\n' \"$BUNDLE_DIR\" \"$REQBENCH_BUNDLE_SHA256\"\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=60)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            bundle, reqbench_digest = proc.stdout.strip().splitlines()[-2:]
            with open(os.path.join(bench, "corpus_mem.py"), "w") as handle:
                handle.write("mutated after staging\n")
            with open(os.path.join(bundle, "corpus_mem.py")) as handle:
                self.assertEqual(handle.read(), "original corpus_mem.py\n")
            for name in self.SOURCES + ("fcvm", "fc-agent"):
                self.assertTrue(os.path.isfile(os.path.join(bundle, name)), name)
            with open(os.path.join(bundle, "REQBENCH_MANIFEST.sha256"), "rb") as handle:
                manifest_bytes = handle.read()
            self.assertEqual(hashlib.sha256(manifest_bytes).hexdigest(), reqbench_digest)
            self.assertEqual(
                [line.split()[-1].decode() for line in manifest_bytes.splitlines()],
                list(self.REQBENCH_SOURCES),
            )
            verified = subprocess.run(
                ["sha256sum", "--check", "--status", "MANIFEST.sha256"],
                cwd=bundle, capture_output=True, text=True, timeout=60)
            self.assertEqual(verified.returncode, 0, verified.stderr)

    def test_only_the_staged_bundle_drives_measured_phases(self):
        with open(EXTRA) as handle:
            source = handle.read()
        self.assertIn('bash "$bundle_dir/corpus_extra.sh"', source)
        self.assertIn('verify_runtime_bundle || cleanup_rc=1', source)
        for path in ("$BENCH/hostcdp.sh", "$BENCH/corpus_mem.py",
                     "$BENCH/corpus_serve.py", "$BENCH/fcvm"):
            self.assertIn(path, source)
        self.assertNotIn("CPUTIME_REPS", source)
        self.assertIn('--source-revision "$SOURCE_REVISION"', source)
        self.assertIn('--runtime-bundle-sha256 "$REQBENCH_RUNTIME_BUNDLE_SHA256"', source)
        self.assertIn(
            '--corpus-extra-runtime-bundle-sha256 "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256"',
            source,
        )


if __name__ == "__main__":
    unittest.main()
