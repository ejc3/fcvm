#!/usr/bin/env python3
"""Deterministic tests for the open-loop Chromium request scalability harness.

No test in this file starts a VM, writes the real cgroup tree, or attaches BPF.
The production boundaries are injected so scheduling, accounting, provenance,
and fail-closed verdicts can be exercised with exact synthetic records.
"""

import contextlib
import dataclasses
import errno
import hashlib
import io
import json
import os
import re
import select
import shlex
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stderr
from types import SimpleNamespace
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqscale  # noqa: E402
import reqscale_analyze  # noqa: E402


RUN_ID = "0123456789abcdef0123456789abcdef"


class ScheduleIsAnArtifact(unittest.TestCase):
    def config(self, **overrides):
        values = dict(
            rates=(2.0, 4.0),
            scored_bursts=5,
            seed=776,
            criteria=reqscale.CapacityCriteria(
                max_offered_rps_error_pct=1.0,
                min_departure_ratio=0.95,
                max_score_end_backlog=8,
                max_p95_launch_lag_ms=25.0,
                max_control_median_drift_pct=10.0,
            ),
            trace_rate=None,
            trace_pairs=0,
            urls=("http://127.0.0.1/fixture",),
        )
        values.update(overrides)
        return reqscale.ScheduleConfig(**values)

    def test_same_seed_produces_the_same_interleaved_schedule(self):
        a = reqscale.build_schedule(self.config(), RUN_ID)
        b = reqscale.build_schedule(self.config(), RUN_ID)
        self.assertEqual(a, b)
        self.assertEqual(reqscale.schedule_sha256(a), reqscale.schedule_sha256(b))

        bursts = a["bursts"]
        self.assertEqual(len(bursts), 2 * 6)  # rate x (one warmup + five scored)
        self.assertEqual(a["randomization"]["unit"], "burst")
        self.assertEqual(len(a["cells"]), 4)
        self.assertTrue(all(cell["independent_bursts"] == 5 for cell in a["cells"]))
        self.assertTrue(all(cell["warmup_bursts"] == 1 for cell in a["cells"]))
        self.assertTrue(all(cell["planned_scored_requests_total"] >= 200 for cell in a["cells"]))
        first_measured = next(
            i for i, burst in enumerate(bursts) if burst["population"] != "warmup"
        )
        self.assertGreater(first_measured, 0)
        self.assertTrue(
            all(burst["population"] == "warmup" for burst in bursts[:first_measured])
        )
        self.assertNotIn(
            "warmup", {burst["population"] for burst in bursts[first_measured:]}
        )
        for burst in bursts:
            requests = burst["requests"]
            by_pair = {}
            for request in requests:
                by_pair.setdefault(request["pair_index"], []).append(request)
            for pair in by_pair.values():
                self.assertEqual({request["backend"] for request in pair}, {"file", "uffd"})
                self.assertEqual(len(pair), 2)
                self.assertEqual(
                    pair[1]["scheduled_offset_ns"] - pair[0]["scheduled_offset_ns"],
                    round(1_000_000_000 / (2 * burst["target_rps"])),
                )

    def test_a_different_seed_changes_order_and_window_seeds(self):
        a = reqscale.build_schedule(self.config(seed=1), RUN_ID)
        b = reqscale.build_schedule(self.config(seed=2), RUN_ID)
        self.assertNotEqual(a["bursts"], b["bursts"])

    def test_fewer_than_five_independent_windows_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "at least 5"):
            reqscale.build_schedule(self.config(scored_bursts=4), RUN_ID)

    def test_missing_explicit_warmup_window_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "warmup"):
            reqscale.build_schedule(self.config(warmup_bursts=0), RUN_ID)

    def test_fewer_than_200_scored_requests_per_backend_rate_is_rejected(self):
        # 0.2/s is the largest rate that both divides the 15s ramp and the 60s score
        # into whole requests and still starves the cell: 5 bursts x 12 = 60 < 200.
        # A rate of 0.5 cannot reach this branch at all, because 0.5 * 15 = 7.5 trips
        # the whole-request guard first.
        with self.assertRaisesRegex(ValueError, "fewer than 200"):
            reqscale.build_schedule(self.config(rates=(0.2,)), RUN_ID)

    def test_trace_pairs_are_explicit_and_randomized_with_their_controls(self):
        schedule = reqscale.build_schedule(
            self.config(trace_rate=4.0, trace_pairs=3), RUN_ID
        )
        pairs = {}
        for burst in schedule["bursts"]:
            if burst["population"] != "trace-perturbation":
                continue
            pairs.setdefault(burst["trace_pair_id"], []).append(burst)
        self.assertEqual(len(pairs), 3)
        for pair in pairs.values():
            self.assertEqual(len(pair), 2)
            self.assertEqual({burst["traced"] for burst in pair}, {False, True})
            self.assertEqual({burst["target_rps"] for burst in pair}, {4.0})
            self.assertEqual(pair[0]["requests"], pair[1]["requests"])


class FakeClock:
    def __init__(self):
        self.now_ns = 1_000_000_000
        self.sleeps = []

    def monotonic_ns(self):
        return self.now_ns

    def sleep_until_ns(self, deadline_ns):
        self.sleeps.append(deadline_ns)
        self.now_ns = max(self.now_ns, deadline_ns)


class DeferredLauncher:
    """Records every launch; creates results only when drain() is called."""

    def __init__(self):
        self.contexts = []
        self.drain_called = False

    def launch(self, context, request_fn):
        self.assert_not_draining()
        self.contexts.append(context)
        return context, request_fn

    def assert_not_draining(self):
        if self.drain_called:
            raise AssertionError("scheduler launched after entering drain")

    def drain(self, handles):
        self.drain_called = True
        self.assert_all_launched(handles)
        records = []
        for context, request_fn in handles:
            # Synthetic, exact monotonic milestones. request_fn is still called
            # so its metadata path is covered without making launch closed-loop.
            rec = request_fn(context)
            launch_ns = context.actual_launch_ns
            rec.update(
                request_id=context.request_id,
                request_index=context.request_index,
                pair_index=context.pair_index,
                burst_id=context.burst_id,
                population=context.population,
                segment=context.segment,
                backend=context.backend,
                target_rps=context.target_rps,
                request_seed=context.request_seed,
                scheduled_ns=context.scheduled_ns,
                actual_launch_ns=launch_ns,
                artifact_ns=launch_ns + 300_000_000,
                finished_ns=launch_ns + 500_000_000,
                blocking_ms=300.0,
                wall_ms=500.0,
                ok=True,
                teardown={"all_gone": True},
            )
            records.append(rec)
        return records

    def assert_all_launched(self, handles):
        if len(handles) != len(self.contexts):
            raise AssertionError("drain began before every open-loop launch")


class OpenLoopIsActuallyOpenLoop(unittest.TestCase):
    @staticmethod
    def one_request_spec():
        return reqscale.BurstSpec(
            burst_id="b0", block_id="b0", population="scored",
            target_rps=1.0, repeat=0, seed=4, traced=False,
            trace_pair_id=None, ramp_seconds=0.0, score_seconds=1.0,
            requests=(
                reqscale.RequestPlan(0, 0, "score", "file", 0, 10),
                reqscale.RequestPlan(1, 0, "score", "uffd", 500_000_000, 11),
            ),
        )

    def test_launches_follow_absolute_deadlines_and_drain_only_after_last_launch(self):
        spec = reqscale.BurstSpec(
            burst_id="b0",
            block_id="b0",
            population="scored",
            target_rps=2.0,
            repeat=0,
            seed=4,
            traced=False,
            trace_pair_id=None,
            ramp_seconds=0.0,
            score_seconds=2.0,
            requests=tuple(
                reqscale.RequestPlan(
                    request_index=index,
                    pair_index=index // 2,
                    segment="score",
                    backend="file" if index % 2 == 0 else "uffd",
                    scheduled_offset_ns=index * 250_000_000,
                    seed=100 + index,
                )
                for index in range(8)
            ),
        )
        clock = FakeClock()
        launcher = DeferredLauncher()
        records, summary = reqscale.run_open_loop_burst(
            RUN_ID, spec, lambda context: {"backend": context.backend,
                                       "url": "http://127.0.0.1/fixture"}, clock, launcher
        )

        self.assertTrue(launcher.drain_called)
        self.assertEqual(len(launcher.contexts), 8)
        base = launcher.contexts[0].scheduled_ns
        self.assertEqual(
            [c.scheduled_ns - base for c in launcher.contexts],
            [
                0, 250_000_000, 500_000_000, 750_000_000,
                1_000_000_000, 1_250_000_000, 1_500_000_000, 1_750_000_000,
            ],
        )
        self.assertEqual([r["request_index"] for r in records], list(range(8)))
        self.assertEqual(summary["planned"], 8)
        self.assertEqual(summary["launched"], 8)
        self.assertEqual(summary["artifact_completed"], 8)
        self.assertEqual(summary["drained"], 8)
        self.assertEqual(summary["launch_span_ms"], 1750.0)
        self.assertEqual(summary["completion_span_ms"], 2050.0)
        self.assertEqual(summary["drain_span_ms"], 2250.0)
        self.assertEqual(summary["latency_ms"]["median"], 300.0)
        self.assertEqual(summary["backends"]["file"]["planned"], 4)
        self.assertEqual(summary["backends"]["uffd"]["planned"], 4)

    def test_a_missing_milestone_invalidates_the_window(self):
        spec = self.one_request_spec()

        class Broken(DeferredLauncher):
            def drain(self, handles):
                rows = super().drain(handles)
                del rows[0]["artifact_ns"]
                return rows

        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "artifact_ns"):
            reqscale.run_open_loop_burst(
                RUN_ID, spec, lambda _context: {}, FakeClock(), Broken()
            )

    def test_a_reqbench_exception_keeps_its_teardown_evidence(self):
        class RecordedFailure(RuntimeError):
            def __init__(self):
                super().__init__("a child survived")
                # ThreadLauncher stamps real monotonic milestones, so a fabricated
                # 20ms wall time for a call that raises immediately is rejected by
                # the record validator before this test can assert anything. The
                # timings are incidental here; the teardown evidence is the subject.
                self.record = {
                    "blocking_ms": 0.0, "wall_ms": 0.0,
                    "teardown": {"all_gone": False, "survivors": [321]},
                }

        def fail(_context):
            raise RecordedFailure()

        records, summary = reqscale.run_open_loop_burst(
            RUN_ID, self.one_request_spec(), fail, FakeClock(), reqscale.ThreadLauncher()
        )
        # one_request_spec plans a file/uffd pair, so both requests raise and both
        # must keep their evidence — the survivor list is the whole point of the
        # record surviving an exception.
        self.assertEqual(len(records), 2)
        for record in records:
            self.assertEqual(record["teardown"]["survivors"], [321])
            self.assertIn("RecordedFailure", record["error"])
        self.assertEqual(summary["failed"], 2)

    def test_a_base_exception_unwinds_only_after_the_worker_joins(self):
        class Interrupt(BaseException):
            pass

        def interrupt(_context):
            raise Interrupt("stop")

        with self.assertRaisesRegex(Interrupt, "stop"):
            reqscale.run_open_loop_burst(
                RUN_ID, self.one_request_spec(), interrupt,
                FakeClock(), reqscale.ThreadLauncher(),
            )

    def test_a_scheduler_interruption_drains_every_already_owned_request(self):
        class InterruptingClock(FakeClock):
            def sleep_until_ns(self, deadline_ns):
                if self.sleeps:
                    raise KeyboardInterrupt("stop scheduling")
                super().sleep_until_ns(deadline_ns)

        launcher = DeferredLauncher()
        with self.assertRaisesRegex(KeyboardInterrupt, "stop scheduling"):
            reqscale.run_open_loop_burst(
                RUN_ID, self.one_request_spec(), lambda _context: {},
                InterruptingClock(), launcher,
            )
        self.assertTrue(launcher.drain_called)
        self.assertEqual(len(launcher.contexts), 1)


class ProcAndCgroupAccounting(unittest.TestCase):
    @staticmethod
    def write_proc_stat(root, pid, start_time):
        # fields 3..22; minflt=101, majflt=303, starttime supplied by caller.
        raw = (
            f"{pid} (worker {pid}) S 1 2 3 4 5 6 101 8 303 10 "
            f"11 12 13 14 15 16 17 18 {start_time}\n"
        )
        with open(os.path.join(root, str(pid), "stat"), "w") as f:
            f.write(raw)
        with open(os.path.join(root, str(pid), "comm"), "w") as f:
            f.write(f"worker-{pid}\n")

    def test_proc_stat_parsing_survives_parentheses_and_extracts_faults(self):
        # fields 3..22 after a comm containing both a space and a right paren.
        raw = (
            "42 (fc vcpu) worker) R 1 2 3 4 5 6 101 8 303 10 "
            "11 12 13 14 15 16 17 18 999\n"
        )
        stat = reqscale.parse_process_stat(raw)
        self.assertEqual(stat.pid, 42)
        self.assertEqual(stat.state, "R")
        self.assertEqual(stat.minor_faults, 101)
        self.assertEqual(stat.major_faults, 303)
        self.assertEqual(stat.start_time_ticks, 999)

    def test_cpu_stat_keeps_every_counter_and_delta(self):
        before = reqscale.parse_cpu_stat(
            "usage_usec 100\nuser_usec 60\nsystem_usec 40\nnr_throttled 2\n"
        )
        after = reqscale.parse_cpu_stat(
            "usage_usec 175\nuser_usec 100\nsystem_usec 75\nnr_throttled 5\n"
        )
        self.assertEqual(
            reqscale.counter_delta(before, after),
            {"usage_usec": 75, "user_usec": 40, "system_usec": 35, "nr_throttled": 3},
        )

    def test_machine_proc_stat_preserves_every_row_not_only_aggregate_cpu(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "stat")
            raw = (
                "cpu  10 1 2 30 4 5 6 7 8 9\n"
                "cpu0 5 0 1 15 2 2 3 3 4 4\n"
                "intr 99 1 2\nctxt 123\nprocesses 45\nprocs_running 2\n"
            )
            with open(path, "w") as f:
                f.write(raw)
            captured = reqscale.read_machine_proc_stat(path)
            self.assertEqual(captured["raw"], raw)
            self.assertEqual(captured["cpu"]["user"], 10)
            self.assertEqual(
                captured["raw_sha256"], hashlib.sha256(raw.encode()).hexdigest()
            )

    def test_cgroup_audit_fails_when_any_observed_process_is_outside_the_run(self):
        with tempfile.TemporaryDirectory() as d:
            proc = os.path.join(d, "proc")
            group = os.path.join(d, "sys", "fs", "cgroup", "run")
            os.makedirs(os.path.join(proc, "10"))
            os.makedirs(os.path.join(proc, "11"))
            os.makedirs(group)
            with open(os.path.join(proc, "10", "cgroup"), "w") as f:
                f.write("0::/run\n")
            with open(os.path.join(proc, "11", "cgroup"), "w") as f:
                f.write("0::/somewhere-else\n")
            self.write_proc_stat(proc, 10, 1000)
            self.write_proc_stat(proc, 11, 1100)
            with open(os.path.join(group, "cgroup.procs"), "w") as f:
                f.write("10\n")
            with open(os.path.join(group, "cpu.stat"), "w") as f:
                f.write("usage_usec 1\n")
            audit = reqscale.CgroupAudit(group, proc_root=proc, cgroup_root=os.path.join(d, "sys", "fs", "cgroup"))
            audit.observe(10, "harness")
            self.assertEqual(
                audit.record()["observed"],
                [{
                    "pid": 10, "pid_start_time_ticks": 1000,
                    "role": "harness", "comm": "worker-10",
                }],
            )
            with self.assertRaisesRegex(reqscale.MeasurementInvalid, "outside run cgroup"):
                audit.observe(11, "firecracker")

    def test_cgroup_audit_does_not_collapse_reused_pids(self):
        with tempfile.TemporaryDirectory() as d:
            proc = os.path.join(d, "proc")
            group = os.path.join(d, "sys", "fs", "cgroup", "run")
            os.makedirs(os.path.join(proc, "10"))
            os.makedirs(group)
            with open(os.path.join(proc, "10", "cgroup"), "w") as f:
                f.write("0::/run\n")
            with open(os.path.join(group, "cgroup.procs"), "w") as f:
                f.write("10\n")
            with open(os.path.join(group, "cpu.stat"), "w") as f:
                f.write("usage_usec 1\n")
            audit = reqscale.CgroupAudit(
                group, proc_root=proc,
                cgroup_root=os.path.join(d, "sys", "fs", "cgroup"),
            )
            self.write_proc_stat(proc, 10, 1000)
            audit.observe(10, "first")
            self.write_proc_stat(proc, 10, 2000)
            audit.observe(10, "replacement")
            self.assertEqual(
                [(row["pid_start_time_ticks"], row["role"]) for row in audit.record()["observed"]],
                [(1000, "first"), (2000, "replacement")],
            )

    def test_quiet_guard_matches_comm_not_an_fcvm_path_in_an_argv(self):
        rows = (
            "S codex\n"
            "S firecracker-a1b2\n"
            "Z fcvm\n"
            "S cloud-hypervis\n"
        )
        self.assertEqual(
            reqscale.parse_vm_process_rows(rows),
            [
                {"state": "S", "comm": "firecracker-a1b2"},
                {"state": "S", "comm": "cloud-hypervis"},
            ],
        )
        # A repository path lived in argv in the old pgrep -f guard. `comm` is
        # just codex here, so this is deliberately not a VM process.
        self.assertEqual(reqscale.parse_vm_process_rows("S codex\n"), [])

    def test_quiet_host_snapshot_records_the_gate_inputs(self):
        with tempfile.TemporaryDirectory() as d:
            loadavg = os.path.join(d, "loadavg")
            with open(loadavg, "w") as f:
                f.write("1.25 0.50 0.25 1/100 10\n")
            snapshot = reqscale.quiet_host_snapshot(loadavg, "S bash\n")
            self.assertEqual(snapshot["loadavg1"], 1.25)
            self.assertEqual(snapshot["vm_process_count"], 0)
            self.assertEqual(snapshot["loadavg1_limit"], 2.0)


class FakeProcReader:
    def __init__(self):
        self.children = {100: [200, 201, 202]}
        self.comms = {200: "firecracker", 201: "sleep", 202: "pasta"}
        self.samples = {
            200: [
                reqscale.ProcessStat(200, "R", 10, 1, 500),
                reqscale.ProcessStat(200, "R", 37, 3, 500),
            ]
        }

    def children_of(self, pid):
        return list(self.children.get(pid, []))

    def comm(self, pid):
        return self.comms.get(pid, "")

    def stat(self, pid):
        values = self.samples[pid]
        return values.pop(0) if len(values) > 1 else values[0]

    def identity(self, pid):
        return FakeProcIdentity(self, pid)


class FakeProcIdentity:
    def __init__(self, proc, pid):
        self.proc = proc
        self.pid = pid
        self.closed = False

    def comm(self):
        return self.proc.comm(self.pid)

    def stat(self):
        return self.proc.stat(self.pid)

    def close(self):
        self.closed = True


class FakeAudit:
    def __init__(self):
        self.observed = []

    def observe(self, pid, role):
        self.observed.append((pid, role))

    def observe_identity(self, identity, role):
        self.observed.append((identity.pid, role))


class PerFirecrackerFaults(unittest.TestCase):
    def test_fault_delta_is_bound_to_one_exact_firecracker_identity(self):
        proc = FakeProcReader()
        audit = FakeAudit()
        probe = reqscale.FirecrackerFaultProbe(proc, audit)
        probe.begin(100)
        out = probe.finish()
        self.assertEqual(
            audit.observed,
            [(100, "fcvm"), (200, "firecracker"), (201, "sleep"), (202, "pasta")],
        )
        self.assertEqual(out["pid"], 200)
        self.assertEqual(out["pid_start_time_ticks"], 500)
        self.assertEqual(out["minor_faults"], 27)
        self.assertEqual(out["major_faults"], 2)

    def test_the_content_addressed_firecracker_is_found(self):
        """fcvm runs firecracker-default-<hash>.bin, whose comm is
        "firecracker-def"; the probe matched only "firecracker" and failed
        every request with "0 firecracker children"."""
        proc = FakeProcReader()
        proc.comms[200] = "firecracker-def"
        proc.comms[202] = "pasta-4707649d2"
        probe = reqscale.FirecrackerFaultProbe(proc, FakeAudit())
        probe.begin(100)
        self.assertEqual(probe.finish()["pid"], 200)

    def test_zero_or_multiple_firecrackers_fails_closed(self):
        proc = FakeProcReader()
        proc.comms[201] = "firecracker"
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "exactly one"):
            reqscale.FirecrackerFaultProbe(proc, FakeAudit()).begin(100)

    def test_a_helper_that_exits_during_the_scan_is_skipped(self):
        """fcvm's short-lived helpers exit between the child listing and the
        /proc open. RED BEFORE THE FIX: the probe raised "cannot open /proc
        identity for pid N: No such file or directory" and failed the request;
        on 2026-10-22 only 7 and 12 of 84 requests per path completed at 1.4
        req/s."""
        proc = FakeProcReader()
        with tempfile.TemporaryDirectory() as empty_proc:
            real_identity = proc.identity
            proc.identity = lambda pid: (
                reqscale.ProcIdentity(empty_proc, pid) if pid == 201 else real_identity(pid))
            audit = FakeAudit()
            probe = reqscale.FirecrackerFaultProbe(proc, audit)
            probe.begin(100)
        self.assertEqual(probe.finish()["pid"], 200)
        self.assertEqual(audit.observed, [(100, "fcvm"), (200, "firecracker"), (202, "pasta")])

    def test_the_analyzer_finds_the_content_addressed_firecracker_in_the_audit(self):
        """The audit records each process under its comm, so Firecracker is
        "firecracker-def". RED BEFORE THE FIX: the analyzer looked for the role
        "firecracker" and refused every request of the first complete corpus
        scale run ("Firecracker identity is absent from its file cgroup audit")."""
        import reqscale_analyze
        status = {"final_cgroups": {
            "file": {"observed": [{"pid": 200, "pid_start_time_ticks": 500, "role": "firecracker-def"}]},
            "uffd": {"observed": [{"pid": 300, "pid_start_time_ticks": 600, "role": "firecracker-def"},
                                  {"pid": 301, "pid_start_time_ticks": 601, "role": "pasta-4707649d2"}]},
        }}
        row = lambda rid, backend, pid, start: {
            "request_id": rid, "backend": backend,
            "firecracker_process_faults_ready_to_artifact": {"pid": pid, "pid_start_time_ticks": start}}
        reqscale_analyze._validate_fault_cgroup_membership(
            [row("a", "file", 200, 500), row("b", "uffd", 300, 600)], status)
        for bad in (row("c", "uffd", 200, 500), row("d", "uffd", 301, 601)):
            with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "absent"):
                reqscale_analyze._validate_fault_cgroup_membership([bad], status)

    def test_pid_reuse_is_rejected(self):
        proc = FakeProcReader()
        proc.samples[200][1] = reqscale.ProcessStat(200, "R", 37, 3, 501)
        probe = reqscale.FirecrackerFaultProbe(proc, FakeAudit())
        probe.begin(100)
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "identity changed"):
            probe.finish()


class ContinuousAccounting(unittest.TestCase):
    class Audit:
        def __init__(self, name):
            self.relative = f"/run/{name}"

        def live_pids(self):
            return []

        def cpu_snapshot(self):
            return {"usage_usec": 10, "user_usec": 6, "system_usec": 4}

    def test_one_sample_keeps_proc_load_psi_memavailable_and_every_cgroup(self):
        with tempfile.TemporaryDirectory() as d:
            proc_stat = os.path.join(d, "stat")
            loadavg = os.path.join(d, "loadavg")
            meminfo = os.path.join(d, "meminfo")
            pressure = os.path.join(d, "pressure")
            os.mkdir(pressure)
            with open(proc_stat, "w") as f:
                f.write("cpu 1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4 5 6 7 8 9 10\nctxt 3\n")
            with open(loadavg, "w") as f:
                f.write("0.10 0.20 0.30 2/100 4321\n")
            with open(meminfo, "w") as f:
                f.write("MemTotal: 1000 kB\nMemAvailable: 750 kB\n")
            for resource in ("cpu", "memory", "io"):
                with open(os.path.join(pressure, resource), "w") as f:
                    f.write("some avg10=0.10 avg60=0.20 avg300=0.30 total=42\n")
                    if resource != "cpu":
                        f.write("full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
            source = reqscale.HostSampleSource(proc_stat, loadavg, pressure, meminfo)
            audits = {
                name: self.Audit(name)
                for name in ("run", "driver", "control", "file", "uffd")
            }
            sample = source.capture(audits, {"name": "burst", "burst_id": "b3"})
            self.assertEqual(set(sample["cgroups"]), set(audits))
            self.assertIn("cpu0", sample["proc_stat"]["raw"])
            self.assertEqual(sample["loadavg"]["parsed"]["running_tasks"], 2)
            self.assertEqual(
                sample["meminfo"]["parsed"]["MemAvailable"],
                {"value": 750, "unit": "kB"},
            )
            self.assertEqual(sample["pressure"]["io"]["parsed"]["some"]["total"], 42)

    def test_split_cgroup_paths_are_fixed_before_entry(self):
        with tempfile.TemporaryDirectory() as d:
            proc = os.path.join(d, "proc")
            root = os.path.join(d, "cgroup")
            os.makedirs(os.path.join(proc, "self"))
            os.mkdir(root)
            with open(os.path.join(proc, "self", "cgroup"), "w") as f:
                f.write("0::/\n")
            scope = reqscale.RunCgroups(RUN_ID, root, proc)
            self.assertEqual(set(scope.paths), {"driver", "control", "file", "uffd"})
            self.assertTrue(all(path.startswith(scope.path + os.sep) for path in scope.paths.values()))


class ChildLifecycle(unittest.TestCase):
    def test_a_deferred_first_signal_makes_later_cleanup_signals_inert(self):
        fence = reqscale.TerminationFence()
        with fence:
            deferred = reqscale.DeferredTermination()
            deferred.__enter__()
            deferred._handle(reqscale.signal.SIGTERM, None)
            with self.assertRaises(reqscale.MeasurementInterrupted):
                deferred.__exit__(None, None, None)
            self.assertEqual(fence.received, [reqscale.signal.SIGTERM])
            # This represents a user pressing Ctrl-C again while execute() is in
            # its finally block. Cleanup must continue instead of being unwound.
            fence._handle(reqscale.signal.SIGINT, None)
            self.assertEqual(
                fence.received, [reqscale.signal.SIGTERM, reqscale.signal.SIGINT]
            )

    def test_every_measured_command_uses_guardexec_and_the_selected_leaf(self):
        command = reqscale.guarded_command(
            "/sys/fs/cgroup/fcvm-reqscale-" + RUN_ID + "/file",
            ["/bin/echo", "ok"],
            parent_pid=123,
        )
        self.assertEqual(command[0], sys.executable)
        self.assertEqual(command[1], reqscale.GUARDEXEC)
        # The leaf must be asserted on the VALUE of --cgroup-procs. `assertIn` against
        # the argv list tests element equality, so a substring never matches and the
        # assertion can only ever fail.
        self.assertEqual(
            command[command.index("--cgroup-procs") + 1],
            "/sys/fs/cgroup/fcvm-reqscale-" + RUN_ID + "/file/cgroup.procs",
        )
        self.assertEqual(command[-2:], ["/bin/echo", "ok"])
        supervised = reqscale.supervised_command(
            "/sys/fs/cgroup/fcvm-reqscale-" + RUN_ID + "/control",
            ["chromium", "about:blank"],
            parent_pid=123,
        )
        self.assertEqual(supervised[1], reqscale.GUARDSUPERVISE)
        self.assertEqual(
            supervised[supervised.index("--cgroup-procs") + 1],
            "/sys/fs/cgroup/fcvm-reqscale-" + RUN_ID + "/control/cgroup.procs",
        )

    def test_run_scope_kills_owned_tree_after_a_survivor_audit(self):
        class Audit:
            def __init__(self, live):
                self.live = live

            def record(self):
                return {"live_pids": list(self.live)}

            def live_pids(self):
                # The synthetic cgroup.kill takes effect before verification.
                return []

        scope = object.__new__(reqscale.RunCgroups)
        scope.entered = True
        scope.original_path = "/fake/original"
        scope.path = "/fake/run"
        scope.paths = {
            name: f"/fake/run/{name}" for name in reqscale.RunCgroups.LEAVES
        }
        scope.audits = {
            "run": Audit([]),
            "driver": Audit([123]),
            "control": Audit([]),
            "file": Audit([456]),
            "uffd": Audit([]),
        }
        opened = mock.mock_open()
        with mock.patch("builtins.open", opened), mock.patch.object(
            reqscale.os, "getpid", return_value=123
        ), mock.patch.object(reqscale.os, "rmdir") as rmdir:
            with self.assertRaisesRegex(
                reqscale.MeasurementInvalid, "unexpected final membership"
            ):
                scope.leave()
        self.assertFalse(scope.entered)
        self.assertIn(
            mock.call("/fake/run/cgroup.kill", "w"), opened.mock_calls
        )
        self.assertEqual(rmdir.call_count, len(reqscale.RunCgroups.LEAVES) + 1)

    def test_guardexec_kills_the_execed_child_when_its_expected_parent_exits(self):
        parent_code = r'''
import os
import subprocess
import sys

guard, cgroup_procs = sys.argv[1:]
ready_read, ready_write = os.pipe()
target = [
    sys.executable, "-c",
    f"import os,time; os.write({ready_write}, b'x'); time.sleep(30)",
]
child = subprocess.Popen(
    [sys.executable, guard, "--expected-parent", str(os.getpid()),
     "--cgroup-procs", cgroup_procs, "--", *target],
    pass_fds=(ready_write,),
)
os.close(ready_write)
if os.read(ready_read, 1) != b'x':
    raise SystemExit("guarded child never reached exec")
os.close(ready_read)
print(child.pid, flush=True)
'''
        with tempfile.TemporaryDirectory() as d:
            cgroup_procs = os.path.join(d, "cgroup.procs")
            with open(cgroup_procs, "w"):
                pass
            parent = subprocess.run(
                [sys.executable, "-c", parent_code, reqscale.GUARDEXEC, cgroup_procs],
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(parent.returncode, 0, parent.stderr)
            child_pid = int(parent.stdout.strip())
            with open(cgroup_procs) as f:
                self.assertEqual(int(f.read().strip()), child_pid)
            pidfd = reqscale.reqbench.pidfd_open(child_pid)
            if pidfd is not None:
                try:
                    poller = select.poll()
                    poller.register(pidfd, select.POLLIN)
                    self.assertTrue(poller.poll(2000), "guarded child survived parent exit")
                finally:
                    os.close(pidfd)

    def test_guardsupervise_sweeps_the_cgroup_when_the_child_exits_normally(self):
        """A child exiting says nothing about the hops it spawned.

        Chromium's zygote, utility and crash-handler processes cannot arm
        PR_SET_PDEATHSIG, which is the whole reason this wrapper owns a cgroup. If
        cleanup only ran from the signal handler, a normal exit would leave them
        running while the harness treated the request as finished.
        """
        with tempfile.TemporaryDirectory() as tmp:
            cgroup_procs = os.path.join(tmp, "cgroup.procs")
            # A cgroup v2 directory always carries cgroup.kill; the wrapper uses its
            # presence to tell a real cgroup from an ordinary directory.
            open(os.path.join(tmp, "cgroup.kill"), "w").close()
            # A THIRD-PARTY survivor, not the supervisor's own enrollment: the wrapper
            # writes its own pid here at startup, and in a real cgroup `cgroup.kill`
            # would take that process too, so asserting on it would not distinguish
            # "a child outlived the browser" from "the supervisor enrolled itself".
            survivor = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
            self.addCleanup(survivor.wait)
            self.addCleanup(survivor.kill)
            with open(cgroup_procs, "w") as handle:
                handle.write(f"{survivor.pid}\n")

            done = subprocess.run(
                [
                    sys.executable,
                    reqscale.GUARDSUPERVISE,
                    "--expected-parent", str(os.getpid()),
                    "--cgroup-procs", cgroup_procs,
                    "--", sys.executable, "-c", "raise SystemExit(0)",
                ],
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(
                done.returncode, 126,
                "a cgroup that will not empty must fail the request, not report the "
                f"child's own status: {done.stderr}",
            )
            self.assertIn(
                str(survivor.pid), done.stderr,
                "the third-party survivor must be named so it can be chased",
            )

    def test_guardsupervise_terminates_the_third_party_process_group(self):
        with tempfile.TemporaryDirectory() as d:
            cgroup_procs = os.path.join(d, "cgroup.procs")
            with open(cgroup_procs, "w"):
                pass
            target = [
                sys.executable,
                "-c",
                "import os,time; print(os.getpid(), flush=True); time.sleep(30)",
            ]
            supervisor = subprocess.Popen(
                [
                    sys.executable, reqscale.GUARDSUPERVISE,
                    "--expected-parent", str(os.getpid()),
                    "--cgroup-procs", cgroup_procs,
                    "--", *target,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                # A target that never prints its pid would otherwise block the whole
                # suite here, with a 30 second child still running behind it.
                handshake = select.poll()
                handshake.register(supervisor.stdout, select.POLLIN)
                self.assertTrue(
                    handshake.poll(10_000),
                    "supervisor did not report the target pid within 10s",
                )
                target_pid = int(supervisor.stdout.readline().strip())
                target_pidfd = reqscale.reqbench.pidfd_open(target_pid)
                supervisor.send_signal(15)
                self.assertEqual(supervisor.wait(timeout=5), 0, supervisor.stderr.read())
                if target_pidfd is not None:
                    try:
                        poller = select.poll()
                        poller.register(target_pidfd, select.POLLIN)
                        self.assertTrue(
                            poller.poll(2000), "supervised process survived supervisor shutdown"
                        )
                    finally:
                        os.close(target_pidfd)
            finally:
                # Any failure above leaks the supervisor and its 30 second target.
                if supervisor.poll() is None:
                    supervisor.kill()
                    supervisor.wait(timeout=5)
                supervisor.stdout.close()
                supervisor.stderr.close()

    def test_tracer_abort_kills_waits_and_closes_all_streams(self):
        class Process:
            def __init__(self):
                self.killed = False
                self.waited = False

            def poll(self):
                return None if not self.killed else -9

            def kill(self):
                self.killed = True

            def wait(self, timeout):
                self.waited = True
                return -9

        with tempfile.TemporaryDirectory() as d:
            tracer = object.__new__(reqscale.BpftraceFaultTracer)
            tracer.process = Process()
            tracer.stdout_thread = None
            tracer.stderr_thread = None
            tracer.stdout_stream = open(os.path.join(d, "stdout"), "w")
            tracer.stderr_stream = open(os.path.join(d, "stderr"), "w")
            tracer.out_dir = d
            tracer.abort()
            self.assertTrue(tracer.process.killed)
            self.assertTrue(tracer.process.waited)
            self.assertTrue(tracer.stdout_stream.closed)
            self.assertTrue(tracer.stderr_stream.closed)

    def test_tracer_stop_reaps_an_exit_race_and_bounds_reader_joins(self):
        class Process:
            def send_signal(self, _signum):
                raise ProcessLookupError

            def wait(self, timeout):
                self.timeout = timeout
                return 0

        class Reader:
            name = "fake-reader"

            def __init__(self):
                self.timeout = None

            def join(self, timeout):
                self.timeout = timeout

            def is_alive(self):
                return False

        with tempfile.TemporaryDirectory() as d, mock.patch.object(
            reqscale, "parse_fault_trace", return_value={"ready": True, "processes": {}}
        ):
            tracer = object.__new__(reqscale.BpftraceFaultTracer)
            tracer.process = Process()
            tracer.stdout_thread = Reader()
            tracer.stderr_thread = Reader()
            tracer.stdout_stream = open(os.path.join(d, "stdout"), "w")
            tracer.stderr_stream = open(os.path.join(d, "stderr"), "w")
            tracer.out_dir = d
            tracer.reader_state_lock = reqscale.threading.Lock()
            tracer.reader_error = None
            tracer.lines = []
            self.assertEqual(
                tracer.stop(timeout_s=3.0), {"ready": True, "processes": {}}
            )
            self.assertEqual(tracer.process.timeout, 3.0)
            self.assertEqual(tracer.stdout_thread.timeout, 10)
            self.assertEqual(tracer.stderr_thread.timeout, 10)
            self.assertTrue(tracer.stdout_stream.closed)
            self.assertTrue(tracer.stderr_stream.closed)

    def test_uffd_stop_reaps_an_exit_that_races_with_sigterm(self):
        class Process:
            returncode = None

            def poll(self):
                return self.returncode

            def send_signal(self, _signum):
                raise ProcessLookupError

            def wait(self, timeout):
                self.returncode = -signal.SIGTERM
                return self.returncode

        class Audit:
            def live_pids(self):
                return []

        with tempfile.TemporaryDirectory() as d:
            serve = object.__new__(reqscale.UffdServe)
            serve.args = SimpleNamespace(teardown_timeout=1.0)
            serve.proc = Process()
            serve.audit = Audit()
            serve.cgroup_path = d
            serve.state_path = None
            serve.log_stream = open(os.path.join(d, "serve.log"), "w")
            serve.stop()
            self.assertTrue(serve.log_stream.closed)
            self.assertEqual(serve.proc.returncode, -signal.SIGTERM)

    def test_uffd_stop_kills_the_leaf_when_initial_membership_audit_fails(self):
        class Audit:
            def __init__(self):
                self.calls = 0

            def live_pids(self):
                self.calls += 1
                if self.calls == 1:
                    raise OSError("synthetic cgroup read failure")
                return []

        with tempfile.TemporaryDirectory() as d:
            kill_path = os.path.join(d, "cgroup.kill")
            with open(kill_path, "w"):
                pass
            serve = object.__new__(reqscale.UffdServe)
            serve.args = SimpleNamespace(teardown_timeout=1.0)
            serve.proc = None
            serve.audit = Audit()
            serve.cgroup_path = d
            serve.state_path = None
            serve.log_stream = open(os.path.join(d, "serve.log"), "w")
            with self.assertRaisesRegex(
                reqscale.MeasurementInvalid, "cannot verify UFFD cgroup cleanup"
            ):
                serve.stop()
            with open(kill_path) as stream:
                self.assertEqual(stream.read(), "1\n")
            self.assertTrue(serve.log_stream.closed)

    def test_control_stop_kills_the_leaf_when_membership_audit_fails(self):
        class Process:
            returncode = 0

            def poll(self):
                return self.returncode

            def wait(self, timeout):
                return self.returncode

        class Audit:
            def __init__(self):
                self.calls = 0

            def live_pids(self):
                self.calls += 1
                if self.calls < 3:
                    raise OSError("synthetic cgroup read failure")
                return []

        with tempfile.TemporaryDirectory() as d:
            kill_path = os.path.join(d, "cgroup.kill")
            with open(kill_path, "w"):
                pass
            profile = os.path.join(d, "profile")
            os.mkdir(profile)
            control = object.__new__(reqscale.NativeChromiumControl)
            control.args = SimpleNamespace(teardown_timeout=1.0)
            control.proc = Process()
            control.audit = Audit()
            control.cgroup_path = d
            control.log_stream = open(os.path.join(d, "control.log"), "w")
            control.profile_dir = profile
            with self.assertRaisesRegex(
                reqscale.MeasurementInvalid, "cannot snapshot control cgroup"
            ):
                control.stop()
            with open(kill_path) as stream:
                self.assertEqual(stream.read(), "1\n")
            self.assertTrue(control.log_stream.closed)
            self.assertFalse(os.path.exists(profile))


class CapacityGates(unittest.TestCase):
    CRITERIA = {
        "max_offered_rps_error_pct": 1.0,
        "min_departure_ratio": 0.95,
        "max_score_end_backlog": 2,
        "max_p95_launch_lag_ms": 20.0,
        "max_control_median_drift_pct": 10.0,
        "require_zero_failures": True,
    }

    @staticmethod
    def cell():
        return {
            "target_rps": 4.0,
            "planned": 240,
            "launched": 240,
            "launched_by_score_end": 240,
            "offered_rps": 4.0,
            "departure_rps": 3.85,
            "departure_ratio": 231 / 240,
            "artifact_completed_by_score_end": 231,
            "score_end_backlog": 2,
            "launch_lag_ms": {"p95": 5.0},
            "failed": 0,
            "ok": 240,
            "artifact_completed": 240,
            "drained": 240,
            "cleanup_confirmed": 240,
        }

    def test_every_declared_capacity_gate_is_independent_and_fail_closed(self):
        clean = reqscale_analyze.evaluate_cell_burst(self.cell(), self.CRITERIA)
        self.assertTrue(clean["passed"])
        mutations = {
            "offered": {"launched_by_score_end": 180, "offered_rps": 3.0},
            "departure": {
                "artifact_completed_by_score_end": 216,
                "departure_rps": 3.6,
                "departure_ratio": 0.9,
            },
            "backlog": {"score_end_backlog": 3},
            "lag": {"launch_lag_ms": {"p95": 21.0}},
            "zero_failure": {"failed": 1, "ok": 239},
        }
        for gate, changes in mutations.items():
            cell = self.cell()
            cell.update(changes)
            verdict = reqscale_analyze.evaluate_cell_burst(cell, self.CRITERIA)
            self.assertFalse(verdict["gates"][gate], gate)
            self.assertFalse(verdict["passed"], gate)

    def test_bootstrap_is_deterministic_and_resamples_bursts(self):
        values = [1.0, 2.0, 3.0, 4.0, 5.0]
        first = reqscale_analyze.bootstrap_mean_ci(values, seed=776, draws=1000)
        second = reqscale_analyze.bootstrap_mean_ci(values, seed=776, draws=1000)
        self.assertEqual(first, second)
        self.assertEqual(first["unit"], "burst")
        self.assertEqual(first["n"], 5)
        self.assertLessEqual(first["ci95_low"], first["point"])
        self.assertGreaterEqual(first["ci95_high"], first["point"])

    def test_seeded_control_deadlines_and_drift_gate_are_exact(self):
        schedule = {
            "control": {"interval_ns": 10_000_000_000, "phase_offset_ns": 123},
            "capacity_criteria": self.CRITERIA,
        }
        origin = 1000
        records = [
            {
                "schema": reqscale.RECORD_SCHEMA,
                "kind": "host-control",
                "control_index": index,
                "scheduled_ns": 1123 + index * 10_000_000_000,
                "actual_launch_ns": 1_000_000 + 1123 + index * 10_000_000_000,
                "artifact_ns": 1_000_000 + 1123 + index * 10_000_000_000
                               + round(latency * 1_000_000),
                "latency_ms": latency,
                "launch_lag_ms": 1.0,
                "ok": True,
                "result": {"ok": True},
            }
            for index, latency in enumerate((100.0, 100.0, 105.0, 105.0))
        ]
        stop_ns = records[-1]["scheduled_ns"] + 1_000_000_000
        status = {"control": {
            "started": True,
            "origin_monotonic_ns": origin,
            "phase_offset_ns": 123,
            "interval_ns": 10_000_000_000,
            "requests": 4,
            "stop_requested_monotonic_ns": stop_ns,
        }}
        self.assertTrue(reqscale_analyze._control_gate(
            records, schedule, status, origin, records[-1]["artifact_ns"]
        )["passed"])
        records[2]["scheduled_ns"] += 1
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "seeded schedule"):
            reqscale_analyze._control_gate(
                records, schedule, status, origin, records[-1]["artifact_ns"]
            )

    def test_impossible_cell_metrics_are_invalid_not_publishable_failures(self):
        cell = self.cell()
        cell["departure_ratio"] = float("inf")
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "finite"):
            reqscale_analyze.evaluate_cell_burst(cell, self.CRITERIA)


class AnalyzerRejectsCorruptEvidence(unittest.TestCase):
    GENERATION = "11111111-1111-4111-8111-111111111111"
    CONFIG_SHA = "a" * 64

    @staticmethod
    def _proc_stat(user):
        return (
            f"cpu {user} 0 0 100 0 0 0 0 0 0\n"
            f"cpu0 {user} 0 0 100 0 0 0 0 0 0\n"
            "intr 1\nctxt 1\nbtime 1\nprocesses 1\nprocs_running 1\n"
            "procs_blocked 0\nsoftirq 1 1\n"
        )

    @staticmethod
    def _machine(raw, captured_ns):
        return {
            "path": "/proc/stat",
            "raw": raw,
            "raw_sha256": hashlib.sha256(raw.encode()).hexdigest(),
            "cpu": reqscale_analyze._proc_cpu_from_raw(raw, "test"),
            "captured_wall_ns": captured_ns + 1_000_000_000_000,
            "captured_monotonic_ns": captured_ns,
            "clk_tck": 100,
        }

    @classmethod
    def fixture(cls):
        spec = OpenLoopIsActuallyOpenLoop.one_request_spec()
        records, summary = reqscale.run_open_loop_burst(
            RUN_ID, spec, lambda context: {"backend": context.backend,
                                       "url": "http://127.0.0.1/fixture"},
            FakeClock(), DeferredLauncher(),
        )
        for index, row in enumerate(records):
            row.update(
                schema=reqscale.RECORD_SCHEMA,
                kind="request",
                run_id=RUN_ID,
                block_id=spec.block_id,
                cell_id=f"{row['backend']}:r1",
                traced=False,
                trace_pair_id=None,
                snapshot_generation_id=cls.GENERATION,
                snapshot_config_sha256=cls.CONFIG_SHA,
                firecracker_process_faults_ready_to_artifact={
                    "pid": 1000 + index,
                    "pid_start_time_ticks": 5000 + index,
                    "minor_faults": 3,
                    "major_faults": 1,
                    "before": {"minor_faults": 10, "major_faults": 2},
                    "after": {"minor_faults": 13, "major_faults": 3},
                    "scope": (
                        "Firecracker process endpoint-ready through artifact return; "
                        "all process VMAs, not guest-RAM-filtered and not UFFD events"
                    ),
                },
            )
        before = cls._machine(cls._proc_stat(1), 900_000_000)
        after = cls._machine(cls._proc_stat(2), 3_000_000_000)
        summary.update(
            machine_proc_stat_before=before,
            machine_proc_stat_after=after,
            machine_proc_stat_delta=reqscale.counter_delta(before["cpu"], after["cpu"]),
            cgroup_cpu_stat_before={
                name: {"usage_usec": 10}
                for name in ("run", "driver", "control", "file", "uffd")
            },
            cgroup_cpu_stat_after={
                name: {"usage_usec": 20}
                for name in ("run", "driver", "control", "file", "uffd")
            },
            cgroup_cpu_stat_delta={
                name: {"usage_usec": 10}
                for name in ("run", "driver", "control", "file", "uffd")
            },
            interburst_cgroup_membership={
                "run": [], "driver": [10], "control": [11],
                "file": [], "uffd": [12],
            },
        )
        schedule = {"run_id": RUN_ID, "urls": ["http://127.0.0.1/fixture"],
                    "bursts": [spec.to_dict()]}
        return schedule, [summary], records

    def test_green_summary_cannot_hide_a_failed_raw_request(self):
        schedule, summaries, records = self.fixture()
        records[0]["ok"] = False
        # A failed request is data now (#1159), so the refusal is the summary
        # disagreeing with the records it summarizes.
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "differ from raw requests"):
            reqscale_analyze._validate_requests(
                schedule, summaries, records, self.GENERATION, self.CONFIG_SHA
            )

    def test_request_generation_must_match_run_provenance(self):
        schedule, summaries, records = self.fixture()
        records[0]["snapshot_generation_id"] = "22222222-2222-4222-8222-222222222222"
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "diverges"):
            reqscale_analyze._validate_requests(
                schedule, summaries, records, self.GENERATION, self.CONFIG_SHA
            )

    def test_seeded_schedule_is_rebuilt_before_analysis(self):
        config = ScheduleIsAnArtifact().config(rates=(1.0,))
        schedule = reqscale.build_schedule(config, RUN_ID)
        request = schedule["bursts"][0]["requests"][0]
        request["backend"] = "uffd" if request["backend"] == "file" else "file"
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "does not match"):
            reqscale_analyze._validate_schedule(schedule)

    def test_strict_loader_rejects_duplicate_keys_and_nonstandard_constants(self):
        with tempfile.TemporaryDirectory() as d:
            duplicate = os.path.join(d, "duplicate.json")
            constant = os.path.join(d, "constant.json")
            with open(duplicate, "w") as stream:
                stream.write('{"run_id":"a","run_id":"b"}')
            with open(constant, "w") as stream:
                stream.write('{"value":NaN}')
            with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "duplicate"):
                reqscale_analyze._load_json(duplicate)
            with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "non-standard"):
                reqscale_analyze._load_json(constant)

    @staticmethod
    def _raw(path, raw, parsed=None):
        value = {
            "path": path,
            "raw": raw,
            "raw_sha256": hashlib.sha256(raw.encode()).hexdigest(),
        }
        if parsed is not None:
            value["parsed"] = parsed
        return value

    @classmethod
    def _host_samples(cls):
        origin = 1_000_000_000
        interval = 5_000_000_000
        rows = []
        psi_raw = "some avg10=0.10 avg60=0.20 avg300=0.30 total=42\n"
        psi_parsed = reqscale.parse_psi(psi_raw, "/proc/pressure/test")
        for index in range(3):
            scheduled = origin + index * interval
            captured = scheduled + 1_000_000
            proc_raw = cls._proc_stat(10 + index)
            rows.append({
                "schema": "fcvm.chromium.reqscale.host-sample.v1",
                "sample_index": index,
                "scheduled_monotonic_ns": scheduled,
                "captured_monotonic_ns": captured,
                "completed_monotonic_ns": captured + 1_000_000,
                "captured_wall_ns": 1_000_000_000_000 + captured,
                "completed_wall_ns": 1_000_000_001_000 + captured,
                "launch_lag_ms": 1.0,
                "phase": {"name": "setup", "burst_id": None},
                "proc_stat": {
                    **cls._raw("/proc/stat", proc_raw),
                    "captured_monotonic_ns": captured,
                    "captured_wall_ns": 1_000_000_000_000 + captured,
                    "cpu": reqscale_analyze._proc_cpu_from_raw(proc_raw, "test"),
                    "clk_tck": 100,
                },
                "loadavg": cls._raw(
                    "/proc/loadavg", "0.1 0.2 0.3 1/10 99\n",
                    reqscale.parse_loadavg(
                        "0.1 0.2 0.3 1/10 99\n", "/proc/loadavg"
                    ),
                ),
                "pressure": {
                    resource: cls._raw(
                        f"/proc/pressure/{resource}", psi_raw, psi_parsed
                    )
                    for resource in ("cpu", "memory", "io")
                },
                "meminfo": cls._raw(
                    "/proc/meminfo", "MemAvailable: 100 kB\n",
                    {"MemAvailable": {"value": 100, "unit": "kB"}},
                ),
                "cgroups": {
                    name: {
                        "path": "/run" if name == "run" else f"/run/{name}",
                        "live_pids": [] if name == "run" else [10 + offset],
                        "cpu_stat": {"usage_usec": index * 10 + offset},
                    }
                    for offset, name in enumerate(
                        ("run", "driver", "control", "file", "uffd")
                    )
                },
            })
        stop_ns = origin + 11_000_000_000
        terminal = dict(rows[-1])
        terminal.update(
            sample_index=3,
            scheduled_monotonic_ns=None,
            captured_monotonic_ns=stop_ns + 1_000_000,
            completed_monotonic_ns=stop_ns + 2_000_000,
            captured_wall_ns=1_000_000_000_000 + stop_ns + 1_000_000,
            completed_wall_ns=1_000_000_000_000 + stop_ns + 2_000_000,
            launch_lag_ms=None,
            terminal=True,
            phase={"name": "teardown", "burst_id": None},
        )
        terminal["cgroups"] = {
            name: {
                **value,
                "live_pids": [11] if name == "driver" else [],
                "cpu_stat": {"usage_usec": 40 + offset},
            }
            for offset, (name, value) in enumerate(terminal["cgroups"].items())
        }
        # The terminal row is copied from the last periodic row and then moved past
        # stop_ns, so its nested /proc/stat stamps have to move with it. Leaving them
        # behind puts the capture outside the row's own boundary, which the sample gate
        # rejects before any of this test's assertions run.
        terminal["proc_stat"] = {
            **terminal["proc_stat"],
            "captured_monotonic_ns": terminal["captured_monotonic_ns"],
            "captured_wall_ns": terminal["captured_wall_ns"],
        }
        rows.append(terminal)
        status = {
            "sampler": {
                "started": True,
                "samples": 4,
                "periodic_samples": 3,
                "terminal_sample": True,
                "origin_monotonic_ns": origin,
                "interval_ns": interval,
                "stop_requested_monotonic_ns": stop_ns,
            },
            "final_cgroups": {
                name: {
                    "path": "/run" if name == "run" else f"/run/{name}",
                    "live_pids": [11] if name == "driver" else [],
                    "cpu_stat": {"usage_usec": 50 + offset},
                }
                for offset, name in enumerate(
                    ("run", "driver", "control", "file", "uffd")
                )
            },
            "run_before": {
                "machine_proc_stat": {
                    "cpu": reqscale_analyze._proc_cpu_from_raw(
                        cls._proc_stat(9), "test"
                    )
                },
                "cgroup_cpu_stat": {
                    name: {"usage_usec": offset}
                    for offset, name in enumerate(
                        ("run", "driver", "control", "file", "uffd")
                    )
                },
            },
            "run_after": {
                "machine_proc_stat": {
                    "cpu": reqscale_analyze._proc_cpu_from_raw(
                        cls._proc_stat(13), "test"
                    )
                },
                "cgroup_cpu_stat": {
                    name: {"usage_usec": 45 + offset}
                    for offset, name in enumerate(
                        ("run", "driver", "control", "file", "uffd")
                    )
                },
            },
        }
        schedule = {"host_sample_interval_ns": interval, "bursts": []}
        return rows, schedule, status, origin + 1, origin + 10_000_000_000

    def test_sample_gate_reconciles_cadence_coverage_and_terminal_sample(self):
        rows, schedule, status, start, end = self._host_samples()
        verdict = reqscale_analyze._validate_samples(
            rows, schedule, status, start, end
        )
        self.assertTrue(verdict["passed"])
        rows[1]["captured_monotonic_ns"] += schedule["host_sample_interval_ns"]
        rows[1]["completed_monotonic_ns"] += schedule["host_sample_interval_ns"]
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "missed"):
            reqscale_analyze._validate_samples(rows, schedule, status, start, end)

    def test_truncated_samples_cannot_claim_continuous_accounting(self):
        rows, schedule, status, start, end = self._host_samples()
        del rows[1]
        status["sampler"]["samples"] -= 1
        status["sampler"]["periodic_samples"] -= 1
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "deadlines elapsed"):
            reqscale_analyze._validate_samples(rows, schedule, status, start, end)


class CompleteAnalyzerFixture(unittest.TestCase):
    """Exercise every artifact boundary together without a VM or wall-clock wait."""

    GENERATION = AnalyzerRejectsCorruptEvidence.GENERATION
    CONFIG_SHA = AnalyzerRejectsCorruptEvidence.CONFIG_SHA
    HARNESS_PID = 10
    CONTROL_PID = 20
    SERVE_PID = 30

    @staticmethod
    def _write_json(path, value):
        with open(path, "w") as stream:
            json.dump(value, stream, sort_keys=True, allow_nan=False)
            stream.write("\n")

    @staticmethod
    def _write_jsonl(path, rows):
        with open(path, "w") as stream:
            for row in rows:
                stream.write(json.dumps(row, sort_keys=True, allow_nan=False) + "\n")

    @classmethod
    def _snapshot(cls):
        files = {
            name: {
                "path": path, "size": 100 + index,
                "mtime_ns": 1_000 + index, "inode": 2_000 + index,
            }
            for index, (name, path) in enumerate((
                ("memory_path", "memory.bin"),
                ("vmstate_path", "vmstate.bin"),
                ("disk_path", "disk.raw"),
                ("config", "config.json"),
            ))
        }
        return {
            "tag": "golden",
            "generation_id": cls.GENERATION,
            "created_at": "2026-08-09T00:00:00+00:00",
            "vm_id": "vm-golden",
            "config_sha256": cls.CONFIG_SHA,
            "shape": {
                "image": "chromium:test", "vcpu": 2, "memory_mib": 1024,
                "network_mode": "rootless",
                "port_mappings": [{"host_ip": None, "host_port": 9222,
                                   "guest_port": 9222, "proto": "tcp"}],
            },
            "files": files,
        }

    @classmethod
    def _proc_capture(cls, user, captured_ns):
        raw = AnalyzerRejectsCorruptEvidence._proc_stat(user)
        return {
            "path": "/proc/stat",
            "captured_wall_ns": 1_000_000_000_000 + captured_ns,
            "captured_monotonic_ns": captured_ns,
            "clk_tck": 100,
            "raw": raw,
            "raw_sha256": hashlib.sha256(raw.encode()).hexdigest(),
            "cpu": reqscale_analyze._proc_cpu_from_raw(raw, "fixture"),
        }

    @classmethod
    def _final_cgroups(cls, run_path, requests):
        harness = {
            "pid": cls.HARNESS_PID, "pid_start_time_ticks": 100,
            "role": "reqscale", "comm": "python3",
        }
        observed = {
            "run": [dict(harness)],
            "driver": [dict(harness)],
            "control": [{
                "pid": cls.CONTROL_PID, "pid_start_time_ticks": 200,
                "role": "host-control-chromium", "comm": "python3",
            }],
            "file": [],
            "uffd": [{
                "pid": cls.SERVE_PID, "pid_start_time_ticks": 300,
                "role": "uffd-serve", "comm": "fcvm",
            }],
        }
        for row in requests:
            faults = row.get("firecracker_process_faults_ready_to_artifact")
            if faults is None:
                continue
            observed[row["backend"]].append({
                "pid": faults["pid"],
                "pid_start_time_ticks": faults["pid_start_time_ticks"],
                "role": "firecracker", "comm": "firecracker",
            })
        return {
            name: {
                "path": run_path if name == "run" else f"{run_path}/{name}",
                "observed": sorted(
                    rows, key=lambda row: (row["pid"], row["pid_start_time_ticks"])
                ),
                "live_pids": [cls.HARNESS_PID] if name == "driver" else [],
                "cpu_stat": {"usage_usec": 100_000 + index},
            }
            for index, (name, rows) in enumerate(observed.items())
        }

    @classmethod
    def _sample(cls, index, scheduled, terminal, phase, run_path):
        captured = scheduled + 1_000_000
        proc = cls._proc_capture(1_000 + index, captured)
        load_raw = "0.1 0.2 0.3 1/10 99\n"
        psi_raw = "some avg10=0.10 avg60=0.20 avg300=0.30 total=42\n"
        mem_raw = "MemTotal: 1000 kB\nMemAvailable: 900 kB\n"
        return {
            "schema": "fcvm.chromium.reqscale.host-sample.v1",
            "sample_index": index,
            "scheduled_monotonic_ns": None if terminal else scheduled,
            "captured_monotonic_ns": captured,
            "completed_monotonic_ns": captured + 1_000_000,
            "captured_wall_ns": 1_000_000_000_000 + captured,
            "completed_wall_ns": 1_000_000_001_000 + captured,
            "launch_lag_ms": None if terminal else 1.0,
            **({"terminal": True} if terminal else {}),
            "phase": phase,
            "proc_stat": proc,
            "loadavg": AnalyzerRejectsCorruptEvidence._raw(
                "/proc/loadavg", load_raw,
                reqscale.parse_loadavg(load_raw, "/proc/loadavg"),
            ),
            "pressure": {
                resource: AnalyzerRejectsCorruptEvidence._raw(
                    f"/proc/pressure/{resource}", psi_raw,
                    reqscale.parse_psi(psi_raw, f"/proc/pressure/{resource}"),
                )
                for resource in ("cpu", "memory", "io")
            },
            "meminfo": AnalyzerRejectsCorruptEvidence._raw(
                "/proc/meminfo", mem_raw,
                reqscale.parse_meminfo(mem_raw, "/proc/meminfo"),
            ),
            "cgroups": {
                name: {
                    "path": run_path if name == "run" else f"{run_path}/{name}",
                    "live_pids": (
                        [cls.HARNESS_PID] if name == "driver"
                        else [] if terminal or name in ("run", "file")
                        else [cls.CONTROL_PID] if name == "control"
                        else [cls.SERVE_PID]
                    ),
                    "cpu_stat": {"usage_usec": index * 100 + offset},
                }
                for offset, name in enumerate(
                    ("run", "driver", "control", "file", "uffd")
                )
            },
        }

    @classmethod
    def _provenance(cls, schedule, snapshot, url, resolve_all_to):
        return {
            "schema": "fcvm.chromium.reqscale.provenance.v2",
            "run_id": RUN_ID,
            "created_at": "2026-08-09T00:00:00+00:00",
            "argv": ["reqscale.py", "--fixture"],
            "source_revision": "1" * 40,
            "source_dirty": False,
            "source_status_sha256": hashlib.sha256(b"").hexdigest(),
            "harness_sha256": "2" * 64,
            "fcvm_path": "/usr/bin/fcvm",
            "fcvm_sha256": "3" * 64,
            "runtime_bundle_sha256": "5" * 64,
            "golden_creator": {
                "creator_fcvm_sha256": "3" * 64,
                "creator_runtime_bundle_sha256": "5" * 64,
                "source_revision": "1" * 40,
            },
            "fcvm_version": "fcvm fixture",
            "schedule_sha256": reqscale.schedule_sha256(schedule),
            "snapshot": snapshot,
            "snapshot_generation_lease": {
                "path": "/mnt/fcvm-btrfs/snapshots/golden.lock",
                "mode": "shared",
                "held_from_identity_read_through_terminal_verification": True,
            },
            "host": {
                "instance_type": "c8g.metal-48xl", "cpu_model": "Neoverse-V2", "kernel": "fixture", "machine": "aarch64",
                "python": "3.13", "cpu_count": 64,
                "quiet_gate": {
                    "loadavg1": 0.1, "loadavg1_limit": 2.0,
                    "vm_process_count": 0, "vm_processes": [],
                },
            },
            "host_control": {
                "interval_seconds": 10.0,
                "chromium_path": "/usr/bin/chromium",
                "chromium_version": "Chromium fixture",
                "chromium_sha256": "4" * 64,
                "url": url,
                "resolve_all_to": resolve_all_to,
                "timeout_seconds": 8.0,
            },
            "fault_trace": {
                "enabled": False, "bpftrace_version": None,
                "max_median_delta_pct": None,
                "scope": (
                    "Firecracker process endpoint-ready through artifact return; "
                    "all VMAs, not guest-RAM-filtered and not UFFD events"
                ),
            },
        }

    @classmethod
    def _time_bound(cls, schedule, summaries, measurement_start, measurement_end, run_path):
        """The host-control and host-sample records a real run writes on wall
        time, and their status entries, covering the measured window."""
        control_origin = measurement_start - 10_000_000_000
        control_stop = measurement_end + 1
        first_deadline = control_origin + schedule["control"]["phase_offset_ns"]
        controls = []
        deadline = first_deadline
        while deadline < control_stop:
            actual = deadline + 1_000_000
            artifact = actual + 100_000_000
            controls.append({
                "schema": reqscale.RECORD_SCHEMA, "kind": "host-control",
                "control_index": len(controls), "scheduled_ns": deadline,
                "actual_launch_ns": actual, "artifact_ns": artifact,
                "launch_lag_ms": 1.0, "latency_ms": 100.0,
                "ok": True, "result": {"ok": True},
            })
            deadline += schedule["control"]["interval_ns"]

        sample_origin = measurement_start - 1_000_000_000
        sample_stop = measurement_end + 1
        samples = []
        deadline = sample_origin
        while deadline < sample_stop:
            captured = deadline + 1_000_000
            preceding = [
                summary for summary in summaries
                if summary["burst_start_ns"] <= captured
            ]
            phase = (
                {"name": "setup", "burst_id": None}
                if not preceding
                else {"name": "burst", "burst_id": preceding[-1]["burst_id"]}
            )
            samples.append(cls._sample(
                len(samples), deadline, False, phase, run_path
            ))
            deadline += schedule["host_sample_interval_ns"]
        terminal = cls._sample(
            len(samples), sample_stop, True,
            {"name": "teardown", "burst_id": None}, run_path,
        )
        samples.append(terminal)
        control_status = {
            "started": True, "origin_monotonic_ns": control_origin,
            "phase_offset_ns": schedule["control"]["phase_offset_ns"],
            "interval_ns": schedule["control"]["interval_ns"],
            "requests": len(controls),
            "stop_requested_monotonic_ns": control_stop,
        }
        sampler_status = {
            "started": True, "origin_monotonic_ns": sample_origin,
            "interval_ns": schedule["host_sample_interval_ns"],
            "samples": len(samples), "periodic_samples": len(samples) - 1,
            "terminal_sample": True,
            "stop_requested_monotonic_ns": sample_stop,
        }
        return controls, control_status, samples, sampler_status

    class FailingLauncher(DeferredLauncher):
        """DeferredLauncher whose `fail(context)` requests fail: "render" after
        the clone was ready (fault counters kept), "spawn" before it (none)."""

        def __init__(self, fail):
            super().__init__()
            self.fail = fail

        def drain(self, handles):
            records = super().drain(handles)
            for record, (context, _request_fn) in zip(records, handles):
                how = self.fail(context)
                if how:
                    record.update(ok=False, blocking_ms=50.0,
                                  error=f"fixture {how} failure", failed_how=how)
                    del record["artifact_ns"]
            return records

    @classmethod
    def build_run(cls, directory, url="http://127.0.0.1/fixture",
                  resolve_all_to=None, rates=(0.8,), fail=lambda _context: None):
        criteria = reqscale.CapacityCriteria(
            max_offered_rps_error_pct=1.0,
            min_departure_ratio=0.95,
            max_score_end_backlog=2,
            max_p95_launch_lag_ms=20.0,
            max_control_median_drift_pct=10.0,
        )
        schedule = reqscale.build_schedule(
            reqscale.ScheduleConfig(
                rates=rates, scored_bursts=5, seed=776, criteria=criteria,
                urls=(url,),
            ),
            RUN_ID,
        )
        summaries = []
        requests = []
        next_start = 100_000_000_000
        next_pid = 1_000
        for burst_number, raw_spec in enumerate(schedule["bursts"]):
            spec = reqscale.BurstSpec.from_dict(raw_spec)
            clock = FakeClock()
            clock.now_ns = next_start
            burst_rows, summary = reqscale.run_open_loop_burst(
                RUN_ID, spec, lambda context: {"backend": context.backend,
                                       "url": url},
                clock, cls.FailingLauncher(fail),
            )
            for row in burst_rows:
                row.update(
                    schema=reqscale.RECORD_SCHEMA,
                    kind="request",
                    run_id=RUN_ID,
                    block_id=spec.block_id,
                    cell_id=f"{row['backend']}:r{format(spec.target_rps, '.12g')}",
                    traced=False,
                    trace_pair_id=None,
                    snapshot_generation_id=cls.GENERATION,
                    snapshot_config_sha256=cls.CONFIG_SHA,
                    firecracker_process_faults_ready_to_artifact={
                        "pid": next_pid,
                        "pid_start_time_ticks": 10_000 + next_pid,
                        "minor_faults": 2,
                        "major_faults": 0,
                        "before": {"minor_faults": 10, "major_faults": 1},
                        "after": {"minor_faults": 12, "major_faults": 1},
                        "scope": (
                            "Firecracker process endpoint-ready through artifact return; "
                            "all process VMAs, not guest-RAM-filtered and not UFFD events"
                        ),
                    },
                )
                if row.pop("failed_how", None) == "spawn":
                    del row["firecracker_process_faults_ready_to_artifact"]
                next_pid += 1
            drain_ns = max(row["finished_ns"] for row in burst_rows)
            proc_before = cls._proc_capture(
                10 + burst_number * 2, summary["burst_start_ns"] - 1_000_000
            )
            proc_after = cls._proc_capture(
                11 + burst_number * 2, drain_ns + 1_000_000
            )
            cpu_before = {
                name: {"usage_usec": burst_number * 1_000 + offset}
                for offset, name in enumerate(
                    ("run", "driver", "control", "file", "uffd")
                )
            }
            cpu_after = {
                name: {"usage_usec": value["usage_usec"] + 100}
                for name, value in cpu_before.items()
            }
            summary.update(
                machine_proc_stat_before=proc_before,
                machine_proc_stat_after=proc_after,
                machine_proc_stat_delta=reqscale.counter_delta(
                    proc_before["cpu"], proc_after["cpu"]
                ),
                cgroup_cpu_stat_before=cpu_before,
                cgroup_cpu_stat_after=cpu_after,
                cgroup_cpu_stat_delta={
                    name: reqscale.counter_delta(cpu_before[name], cpu_after[name])
                    for name in cpu_before
                },
                interburst_cgroup_membership={
                    "run": [], "driver": [cls.HARNESS_PID],
                    "control": [cls.CONTROL_PID], "file": [],
                    "uffd": [cls.SERVE_PID],
                },
            )
            requests.extend(burst_rows)
            summaries.append(summary)
            next_start = drain_ns + 1_000_000_000

        snapshot = cls._snapshot()
        provenance = cls._provenance(schedule, snapshot, url, resolve_all_to)
        run_path = f"/bench/fcvm-reqscale-{RUN_ID}"
        final_cgroups = cls._final_cgroups(run_path, requests)
        measurement_start = summaries[0]["burst_start_ns"]
        measurement_end = max(row["finished_ns"] for row in requests)

        controls, control_status, samples, sampler_status = cls._time_bound(
            schedule, summaries, measurement_start, measurement_end, run_path)
        control_origin = control_status["origin_monotonic_ns"]
        terminal = samples[-1]
        final_cgroups = {
            name: {
                **row,
                "cpu_stat": {
                    "usage_usec": terminal["cgroups"][name]["cpu_stat"]["usage_usec"] + 30
                },
            }
            for name, row in final_cgroups.items()
        }
        before_cpu = {
            name: {"usage_usec": offset}
            for offset, name in enumerate(
                ("run", "driver", "control", "file", "uffd")
            )
        }
        after_cpu = {
            name: {
                "usage_usec": terminal["cgroups"][name]["cpu_stat"]["usage_usec"] + 10
            }
            for name in final_cgroups
        }
        harness_rows = {
            "run": [dict(final_cgroups["run"]["observed"][0])],
            "driver": [dict(final_cgroups["driver"]["observed"][0])],
            "control": [], "file": [], "uffd": [],
        }
        run_before = {
            "machine_proc_stat": cls._proc_capture(1, measurement_start - 2_000_000),
            "cgroup_cpu_stat": before_cpu,
            "cgroups": {
                name: {
                    "path": final_cgroups[name]["path"],
                    "observed": harness_rows[name],
                    "live_pids": final_cgroups[name]["live_pids"],
                    "cpu_stat": dict(before_cpu[name]),
                }
                for name in final_cgroups
            },
        }
        run_after = {
            "machine_proc_stat": cls._proc_capture(10_000, measurement_end + 2_000_000),
            "cgroup_cpu_stat": after_cpu,
            "cgroups": {
                name: {
                    "path": final_cgroups[name]["path"],
                    "observed": [dict(item) for item in final_cgroups[name]["observed"]],
                    "live_pids": final_cgroups[name]["live_pids"],
                    "cpu_stat": {
                        "usage_usec": after_cpu[name]["usage_usec"] + 10
                    },
                }
                for name in final_cgroups
            },
        }
        failed_rates = {}
        for summary in summaries:
            failure = reqscale.burst_request_failures(summary)
            if failure is not None:
                failed_rates.setdefault(summary["target_rps"], []).append(failure)
        status = {
            "schema": reqscale.STATUS_SCHEMA,
            "run_id": RUN_ID, "valid": True,
            "bursts_completed": len(summaries),
            "bursts_planned": len(schedule["bursts"]),
            "failed_rates": reqscale.failed_rate_records(failed_rates),
            "control": control_status,
            "sampler": sampler_status,
            "snapshot_identity_after": snapshot,
            "final_cgroups": final_cgroups,
            "run_before": run_before, "run_after": run_after,
            "error": None, "error_details": None, "errors": [],
        }
        warmup = {
            "schema": reqscale.RECORD_SCHEMA,
            "kind": "host-control-warmup", "included_in_analysis": False,
            "started_monotonic_ns": control_origin - 200_000_000,
            "artifact_monotonic_ns": control_origin - 100_000_000,
            "latency_ms": 100.0, "result": {"ok": True},
        }
        serve = {
            "schema": reqscale.RECORD_SCHEMA, "kind": "uffd-serve",
            "run_id": RUN_ID, "pid": cls.SERVE_PID,
            "pid_start_time_ticks": 300, "state_path": "state/serve.json",
            "uffd_mode": "copy", "uffd_prefetch": "on", "snapshot_tag": snapshot["tag"],
            "snapshot_generation_id": cls.GENERATION,
            "snapshot_config_sha256": cls.CONFIG_SHA,
        }
        cls._write_json(os.path.join(directory, "schedule.json"), schedule)
        cls._write_json(os.path.join(directory, "provenance.json"), provenance)
        cls._write_json(os.path.join(directory, "status.json"), status)
        cls._write_json(os.path.join(directory, "host-control-warmup.json"), warmup)
        cls._write_json(os.path.join(directory, "uffd-serve.json"), serve)
        cls._write_jsonl(os.path.join(directory, "bursts.jsonl"), summaries)
        cls._write_jsonl(os.path.join(directory, "requests.jsonl"), requests)
        cls._write_jsonl(os.path.join(directory, "host-control.jsonl"), controls)
        cls._write_jsonl(os.path.join(directory, "host-samples.jsonl"), samples)

    def test_complete_raw_fixture_is_publishable_and_reportable(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.object(
            reqscale_analyze, "BOOTSTRAP_DRAWS", 1000
        ):
            self.build_run(d)
            analysis = reqscale_analyze.analyze(d)
            self.assertTrue(analysis["publishable"])
            self.assertEqual(
                analysis["capacity"]["joint_highest_contiguous_passing_rate_per_second"],
                0.8,
            )
            report = reqscale_analyze.markdown_report(analysis)
            self.assertIn("Firecracker VMAs", report)
            self.assertIn("0.8", report)


class AnalyzerReportsAFailedRate(unittest.TestCase):
    """#1159: the analyzer reports a rate with a failed request as failed,
    publishes no figure for it, and keeps every other rate."""

    @staticmethod
    def fail_at_1_6(context):
        """One scored request of every 1.6 req/s burst fails: a render failure
        after the clone was ready (as on 2026-10-22) in odd bursts, a failure
        before it in even ones."""
        if context.target_rps != 1.6 or context.request_index != 100:
            return None
        return "render" if int(context.burst_id[1:5]) % 2 else "spawn"

    def _run(self, directory):
        CompleteAnalyzerFixture.build_run(directory, rates=(0.8, 1.6), fail=self.fail_at_1_6)
        with open(os.path.join(directory, "status.json")) as stream:
            return json.load(stream)

    @staticmethod
    def _rewrite_status(directory, **edits):
        path = os.path.join(directory, "status.json")
        with open(path) as stream:
            status = json.load(stream)
        status.update(edits)
        CompleteAnalyzerFixture._write_json(path, status)

    def test_a_failed_rate_is_reported_failed_and_the_other_rate_kept(self):
        """RED BEFORE THE FIX: analyze() raised "request (...) failed" for the
        first failed request, so the 0.8 req/s cells were never reported."""
        with tempfile.TemporaryDirectory() as d, mock.patch.object(
            reqscale_analyze, "BOOTSTRAP_DRAWS", 1000
        ):
            status = self._run(d)
            failed = status["failed_rates"]
            self.assertEqual([entry["target_rps"] for entry in failed], [1.6])
            self.assertGreaterEqual(len(failed[0]["bursts"]), 2, failed)
            analysis = reqscale_analyze.analyze(d)
            self.assertTrue(analysis["publishable"], analysis["publication_blocked_by"])
            self.assertEqual(analysis["failed_rates"], [1.6])
            cells = {(cell["backend"], cell["target_rps"]): cell for cell in analysis["cells"]}
            for backend in ("file", "uffd"):
                kept, lost = cells[(backend, 0.8)], cells[(backend, 1.6)]
                self.assertTrue(kept["passed"])
                self.assertFalse(kept["rate_failed"])
                self.assertIsNotNone(kept["artifact_latency_p95_ms"])
                self.assertFalse(lost["passed"])
                self.assertTrue(lost["rate_failed"])
                for name in reqscale_analyze.CELL_MEASUREMENTS:
                    self.assertIsNone(lost[name], name)
            self.assertEqual(
                sum(cells[(backend, 1.6)]["rate_failure"]["scored_failed"]
                    for backend in ("file", "uffd")),
                sum(entry["failed"] for entry in failed[0]["bursts"]
                    if entry["population"] == "scored"))
            self.assertEqual(
                analysis["capacity"]["joint_highest_contiguous_passing_rate_per_second"], 0.8)
            report = reqscale_analyze.markdown_report(analysis)
            self.assertIn("no, rate failed", report)
            self.assertIn("Failed rates per backend: 1.6.", report)

    def test_a_rate_whose_every_request_failed_is_reported_failed(self):
        """Codex review of #1293: with no artifact at 1.6 req/s, the ramp's
        requests count in the score-end backlog, which then exceeds the scored
        plan, and the per-burst capacity check refused the whole run ("score-end
        backlog is not a valid request count") before the failed rate was set
        aside."""
        with tempfile.TemporaryDirectory() as d, mock.patch.object(
            reqscale_analyze, "BOOTSTRAP_DRAWS", 1000
        ):
            CompleteAnalyzerFixture.build_run(
                d, rates=(0.8, 1.6),
                fail=lambda context: "spawn" if context.target_rps == 1.6 else None)
            analysis = reqscale_analyze.analyze(d)
            self.assertEqual(analysis["failed_rates"], [1.6])
            cells = {(cell["backend"], cell["target_rps"]): cell for cell in analysis["cells"]}
            for backend in ("file", "uffd"):
                self.assertTrue(cells[(backend, 0.8)]["passed"])
                self.assertTrue(cells[(backend, 1.6)]["rate_failed"])
                self.assertEqual(cells[(backend, 1.6)]["rate_failure"]["scored_failed"], 5 * 96)

    def test_the_status_must_name_exactly_the_failed_rates_the_records_show(self):
        for label, edit in (
            ("omits the failed rate", lambda rates: []),
            ("adds a clean rate", lambda rates: rates + [{"target_rps": 0.8, "bursts": []}]),
        ):
            with self.subTest(label), tempfile.TemporaryDirectory() as d:
                status = self._run(d)
                self._rewrite_status(d, failed_rates=edit(status["failed_rates"]))
                with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid,
                                            "status failed_rates differ"):
                    reqscale_analyze.analyze(d)


class FakeCgroupKernel:
    """cgroup v2 membership over a plain directory tree, for an unprivileged test.

    A pid written to a `cgroup.procs` file (by guardexec, guardsupervise or
    RunCgroups) has migrated there; the latest write by mtime wins. A process
    is in the cgroup of its nearest migrated ancestor when it descends from it
    by forks made after that migration, as a forked child is, and anything
    else keeps its real cgroup. A zombie still names its cgroup
    but is no member of it, and a reaped process has no cgroup to read. Every
    query re-reads the files first, so a pid the next guardexec overwrites in
    the same file stays where it was.
    """

    def __init__(self, root):
        self.root = os.path.realpath(root)
        self.where = {}
        self.ticks = 0
        self.lock = threading.Lock()

    @staticmethod
    def _stat(pid):
        try:
            with open(f"/proc/{pid}/stat") as stream:
                fields = stream.read().rpartition(")")[2].split()
        except OSError:
            return None
        return fields[0], int(fields[1]), int(fields[19])

    def _sync(self):
        for directory, _dirs, files in os.walk(self.root):
            if "cgroup.procs" not in files:
                continue
            path = os.path.join(directory, "cgroup.procs")
            mtime = os.stat(path).st_mtime_ns
            # The write's time in /proc/PID/stat's start-time unit (clock
            # ticks since boot), to tell children forked before it from after.
            migrated = (time.clock_gettime_ns(time.CLOCK_BOOTTIME) - (time.time_ns() - mtime)) \
                * os.sysconf("SC_CLK_TCK") // 1_000_000_000
            relative = "/" + os.path.relpath(directory, self.root)
            with open(path) as stream:
                pids = [int(value) for value in stream.read().split()]
            for pid in pids:
                stat = self._stat(pid)
                if stat is None:
                    continue
                key = (pid, stat[2])
                if key not in self.where or self.where[key][0] <= mtime:
                    self.where[key] = (mtime, relative, migrated)

    def _member(self, pid):
        """The fake cgroup of pid, None outside the tree, or "gone"."""
        if self._stat(pid) is None:
            return "gone"
        current, child_start = pid, None
        while current > 1:
            here = self._stat(current)
            if here is None:
                return None
            found = self.where.get((current, here[2]))
            if found is not None:
                inherited = child_start is None or child_start >= found[2]
                return found[1] if inherited else None
            current, child_start = here[1], here[2]
        return None

    def cgroup_of(self, pid):
        with self.lock:
            self._sync()
            member = self._member(pid)
        if member == "gone":
            raise reqscale.ProcessGone(f"pid {pid} exited")
        if member is not None:
            return member
        with open(f"/proc/{pid}/cgroup") as stream:
            return reqscale._unified_cgroup_from_raw(stream.read(), f"pid {pid}")

    def live(self, relative):
        with self.lock:
            self._sync()
            seeds = {pid for pid, _start in self.where}
            pids = set()
            while seeds:
                pid = seeds.pop()
                if pid in pids or self._stat(pid) is None:
                    continue
                pids.add(pid)
                for tasks in (f"/proc/{pid}/task",):
                    try:
                        tids = os.listdir(tasks)
                    except OSError:
                        continue
                    for tid in tids:
                        try:
                            with open(f"{tasks}/{tid}/children") as stream:
                                seeds.update(int(value) for value in stream.read().split())
                        except OSError:
                            continue
            return sorted(
                pid for pid in pids
                if (self._stat(pid) or ("X",))[0] not in ("Z", "X")
                and self._member(pid) == relative
            )

    def cpu(self):
        with self.lock:
            self.ticks += 1_000_000
            return {"usage_usec": self.ticks}

    def rmdir(self, path, *, dir_fd=None, real=os.rmdir):
        """rmdir of a cgroup: refused while it has members, and its interface
        files go with it."""
        if dir_fd is not None:
            return real(path, dir_fd=dir_fd)
        path = os.path.realpath(path)
        if os.path.commonpath((self.root, path)) == self.root:
            if self.live("/" + os.path.relpath(path, self.root)):
                raise OSError(errno.EBUSY, os.strerror(errno.EBUSY), path)
            for name in ("cgroup.procs", "cgroup.kill"):
                with contextlib.suppress(FileNotFoundError):
                    os.unlink(os.path.join(path, name))
        real(path)

    def patches(self):
        kernel = self
        return (
            mock.patch.object(reqscale.os, "rmdir", kernel.rmdir),
            mock.patch.object(reqscale.ProcIdentity, "cgroup",
                              lambda identity: kernel.cgroup_of(identity.pid)),
            mock.patch.object(reqscale.CgroupAudit, "live_pids",
                              lambda audit: kernel.live(audit.relative)),
            mock.patch.object(reqscale.CgroupAudit, "cpu_snapshot",
                              lambda _audit: kernel.cpu()),
        )


class HarnessOutputFeedsTheAnalyzer(unittest.TestCase):
    """#1173: what reqscale.py's own code writes is what reqscale_analyze.py reads.

    Four defects reached a real run because each side was tested against its
    own fixtures: the request function ignored the cgroup guard and the fault
    probe, the probe raced fcvm's helpers, and the analyzer could not read the
    port mappings or the audit roles the harness writes. Here the harness's
    own code produces every record those defects lived in, on a fake fcvm and
    a fake Chromium, and the analyzer reads the directory:

    - each request is the real request function (_make_request_fn, then
      reqbench.run_cdp_request) spawning a fake fcvm through the real cgroup
      guard, with the real Firecracker fault probe on its process tree (a
      Firecracker-named child and a short-lived helper) and the real teardown;
    - the memory server and the host control are the real UffdServe and
      NativeChromiumControl, and the cgroup audits are the real RunCgroups and
      CgroupAudit over FakeCgroupKernel;
    - the snapshot identity is the real lease over an fcvm-shaped snapshot;
    - bursts, their accounting and inter-burst membership are the real
      run_open_loop_burst, _augment_burst_accounting and _interburst_membership.

    What stays synthetic is what a real run produces on wall time: execute()'s
    loop itself, the 15 s ramp and 60 s window (requests run one after another
    and their milestones are set on the schedule's clock), the HostSampler and
    ControlScheduler records, /proc/stat, and the provenance's host facts.
    Driving those needs 75 s per burst and a delegated cgroup v2 tree.
    """

    RATE = 1 / 15  # one ramp request and 4 scored ones per path and burst

    def _stub(self, path, body):
        with open(path, "w") as stream:
            stream.write(body)
        os.chmod(path, 0o755)
        return path

    def _fakes(self, d, state_dir):
        os.makedirs(os.path.join(d, "bin"))
        # A script, not a renamed `sleep`: a multicall coreutils (uutils on
        # Ubuntu 26.04) dispatches on argv[0] and exits at once under another
        # name. The kernel names a script's process after the script file.
        firecracker = self._stub(os.path.join(d, "bin", "firecracker-0123456789abcdef"), (
            f"#!{sys.executable}\n"
            "import ctypes, signal, sys, time\n"
            "ctypes.CDLL(None).prctl(1, signal.SIGKILL)  # PR_SET_PDEATHSIG, as fcvm arms it\n"
            "open(sys.argv[1], 'w').close()\n"
            "time.sleep(600)\n"
        ))
        serve = (
            "import json,os,signal,sys,time\n"
            "state = sys.argv[1]\n"
            "def stop(*_):\n"
            "    os.unlink(state)\n"
            "    sys.exit(0)\n"
            "signal.signal(signal.SIGTERM, stop)\n"
            "start = int(open('/proc/self/stat').read().rpartition(')')[2].split()[19])\n"
            "with open(state + '.tmp', 'w') as f:\n"
            "    json.dump({'vm_id': 'serve', 'pid': os.getpid(), 'pid_start_time': start,\n"
            "               'config': {'process_type': 'serve', 'snapshot_name': sys.argv[2],\n"
            "                          'uffd_mode': sys.argv[3]}}, f)\n"
            "os.rename(state + '.tmp', state)\n"
            "while True:\n"
            "    time.sleep(60)\n"
        )
        fcvm = self._stub(os.path.join(d, "bin", "fcvm"), f"""#!/bin/bash
# A fake fcvm: `snapshot serve` publishes a serve state and waits; `snapshot run`
# starts a Firecracker-named child that dies with it and a helper that exits at
# once, then publishes a ready clone state with a loopback endpoint.
set -eu
if [ "${{1:-}}" = --version ]; then echo "fcvm fake"; exit 0; fi
if [ "$2" = serve ]; then
    exec python3 -c {shlex.quote(serve)} {state_dir}/serve-$$.json "$3" "$5"
fi
name=""
while [ $# -gt 0 ]; do [ "$1" = --name ] && name=$2; shift; done
{firecracker} {d}/firecracker-$$.armed &
until [ -e {d}/firecracker-$$.armed ]; do sleep 0.002; done
true &
read -r stat < /proc/$$/stat; stat=${{stat##*) }}; read -ra fields <<< "$stat"
vm=vm-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \\n')
printf '{{"vm_id":"%s","name":"%s","pid":%d,"pid_start_time":%d,"lifecycle_ready":true,"config":{{"network":{{"loopback_ip":"127.0.0.1"}}}}}}\\n' \\
    "$vm" "$name" $$ "${{fields[19]}}" > {state_dir}/$vm.json.tmp
: > {state_dir}/$vm.json.lock
mv {state_dir}/$vm.json.tmp {state_dir}/$vm.json
wait
""")
        chromium = self._stub(os.path.join(d, "bin", "chromium"), """#!/bin/bash
# A fake Chromium: publishes DevToolsActivePort in its profile and waits.
if [ "${1:-}" = --version ]; then echo "Chromium fake"; exit 0; fi
for arg in "$@"; do
    case "$arg" in --user-data-dir=*) profile=${arg#--user-data-dir=} ;; esac
done
printf '9\\n/devtools/browser/fake\\n' > "$profile/DevToolsActivePort.tmp"
mv "$profile/DevToolsActivePort.tmp" "$profile/DevToolsActivePort"
exec sleep 600
""")
        return fcvm, chromium

    class SerialLauncher:
        """reqscale.ThreadLauncher one request at a time, each record's
        milestones set on the schedule's clock: a real request takes about
        0.1 s of wall time, which the virtual 75 s burst cannot carry."""

        def __init__(self):
            self.pending = []

        def launch(self, context, request_fn):
            self.pending.append((context, request_fn))
            return context

        @staticmethod
        def on_schedule(record, context):
            launch = context.scheduled_ns
            record.update(actual_launch_ns=launch, finished_ns=launch + 500_000_000,
                          blocking_ms=300.0 if record.get("ok") else 50.0, wall_ms=500.0)
            if "artifact_ns" in record:
                record["artifact_ns"] = launch + 300_000_000
            return record

        def drain(self, _handles):
            threads = reqscale.ThreadLauncher()
            records = []
            for context, request_fn in self.pending:
                def timed(owned, context=context, request_fn=request_fn):
                    try:
                        record = request_fn(owned)
                    except Exception as error:
                        # A request that raises carries its record to the
                        # launcher, which keeps it as a failed request.
                        self.on_schedule(getattr(error, "record", {}), context)
                        raise
                    return self.on_schedule(record, context)

                records.extend(threads.drain([threads.launch(context, timed)]))
            return records

    def test_a_run_of_the_harness_on_a_fake_fcvm_is_analyzable(self):
        import cdpdrive

        with tempfile.TemporaryDirectory() as d, contextlib.ExitStack() as stack:
            data_root = os.path.join(d, "data")
            state_dir = os.path.join(data_root, "state")
            os.makedirs(state_dir)
            ProvenanceFailsClosed.make_snapshot(None, data_root)
            fcvm, chromium = self._fakes(d, state_dir)
            run_dir = os.path.join(d, "run")
            log_dir = os.path.join(d, "logs")
            os.makedirs(run_dir)
            os.makedirs(log_dir)

            kernel = FakeCgroupKernel(os.path.join(d, "cgroup"))
            original = reqscale._read_unified_cgroup("/proc/self/cgroup")
            os.makedirs(os.path.join(kernel.root, original.lstrip("/")))
            for patch in kernel.patches():
                stack.enter_context(patch)
            stack.enter_context(mock.patch.object(reqscale, "MIN_SCORED_REQUESTS", 20))
            stack.enter_context(mock.patch.object(
                cdpdrive, "drive", return_value={"ok": True, "stages": {}}))
            port = stack.enter_context(socket.socket())
            port.bind(("127.0.0.1", 0))
            port.listen(64)

            url = "http://127.0.0.1/fixture"
            criteria = reqscale.CapacityCriteria(
                max_offered_rps_error_pct=1.0, min_departure_ratio=0.95,
                max_score_end_backlog=2, max_p95_launch_lag_ms=20.0,
                max_control_median_drift_pct=10.0)
            schedule = reqscale.build_schedule(reqscale.ScheduleConfig(
                rates=(self.RATE,), scored_bursts=5, seed=776, criteria=criteria,
                urls=(url,)), RUN_ID)
            lease = stack.enter_context(reqscale.SnapshotGenerationLease(data_root, "golden"))
            args = SimpleNamespace(
                run_id=RUN_ID, snapshot_tag="golden", snapshot_identity=dict(lease.identity),
                url=url, urls=[url], fcvm=fcvm, data_root=data_root, state_dir=state_dir,
                timeout=20.0, teardown_timeout=10.0, rust_log="fcvm=debug",
                uffd_mode="copy", uffd_prefetch="on", format="jpeg", quality=80,
                cdp_port=port.getsockname()[1], ws_url="",
                control_url=url, control_resolve_all_to="", control_chromium=chromium,
                control_timeout=8.0, control_tmp_root=d,
            )

            scope = reqscale.RunCgroups(RUN_ID, kernel.root)
            audits = scope.enter()
            args.cgroup_paths = dict(scope.paths)
            run_before_cgroups = {
                "cgroup_cpu_stat": reqscale._cpu_snapshots(audits),
                "cgroups": reqscale._cgroup_records(audits),
            }
            serve = reqscale.UffdServe(args, audits["uffd"], scope.paths["uffd"], log_dir)
            control = reqscale.NativeChromiumControl(
                args, audits["control"], scope.paths["control"], log_dir)
            try:
                serve_pid = serve.start()
                warmup = control.start()
                # Every milestone below is on the schedule's clock, set after
                # the real warmup so the warmup precedes the control origin.
                start = time.monotonic_ns() + 60_000_000_000
                summaries, requests = [], []
                global_base = 0
                for number, raw in enumerate(schedule["bursts"]):
                    spec = reqscale.BurstSpec.from_dict(raw)
                    clock = FakeClock()
                    clock.now_ns = start
                    proc_before = CompleteAnalyzerFixture._proc_capture(10 + 2 * number, start - 1_000_000)
                    cpu_before = reqscale._cpu_snapshots(audits)
                    request_fn = reqscale._make_request_fn(
                        args, spec, serve_pid, log_dir, audits, None, global_base)
                    records, summary = reqscale.run_open_loop_burst(
                        RUN_ID, spec, request_fn, clock, self.SerialLauncher())
                    for record in records:
                        self.assertTrue(record["ok"], record.get("error"))
                    drain = max(record["finished_ns"] for record in records)
                    reqscale._augment_burst_accounting(
                        summary, proc_before,
                        CompleteAnalyzerFixture._proc_capture(11 + 2 * number, drain + 1_000_000),
                        cpu_before, reqscale._cpu_snapshots(audits))
                    summary["interburst_cgroup_membership"] = reqscale._interburst_membership(
                        audits, serve_pid, control.proc.pid)
                    summaries.append(summary)
                    requests.extend(records)
                    global_base += summary["total_planned"]
                    start = drain + 1_000_000_000
                failed = {}
                for summary in summaries:
                    failure = reqscale.burst_request_failures(summary)
                    if failure is not None:
                        failed.setdefault(summary["target_rps"], []).append(failure)
            finally:
                control.stop()
                serve.stop()
            run_after_cgroups = {
                "cgroup_cpu_stat": reqscale._cpu_snapshots(audits),
                "cgroups": reqscale._cgroup_records(audits),
            }
            final_cgroups = scope.leave()
            snapshot_after = lease.verify()

            measurement_start = summaries[0]["burst_start_ns"]
            measurement_end = max(row["finished_ns"] for row in requests)
            pids = type("LivePids", (CompleteAnalyzerFixture,), {
                "HARNESS_PID": os.getpid(), "CONTROL_PID": control.proc.pid,
                "SERVE_PID": serve_pid,
            })
            controls, control_status, samples, sampler_status = pids._time_bound(
                schedule, summaries, measurement_start, measurement_end,
                final_cgroups["run"]["path"])
            # Sample counters sit between the real run-level snapshots.
            base = max(row["usage_usec"] for row in run_before_cgroups["cgroup_cpu_stat"].values())
            for index, sample in enumerate(samples):
                for row in sample["cgroups"].values():
                    row["cpu_stat"] = {"usage_usec": base + 1 + index}
            provenance = CompleteAnalyzerFixture._provenance(
                schedule, dict(lease.identity), url, None)
            provenance["snapshot_generation_lease"]["path"] = os.path.abspath(lease.path)
            status = {
                "schema": reqscale.STATUS_SCHEMA, "run_id": RUN_ID, "valid": True,
                "bursts_completed": len(summaries), "bursts_planned": len(schedule["bursts"]),
                "failed_rates": reqscale.failed_rate_records(failed),
                "control": control_status, "sampler": sampler_status,
                "snapshot_identity_after": snapshot_after,
                "run_before": {
                    "machine_proc_stat": CompleteAnalyzerFixture._proc_capture(
                        1, measurement_start - 2_000_000),
                    **run_before_cgroups,
                },
                "run_after": {
                    "machine_proc_stat": CompleteAnalyzerFixture._proc_capture(
                        10_000, measurement_end + 2_000_000),
                    **run_after_cgroups,
                },
                "final_cgroups": final_cgroups,
                "error": None, "error_details": None, "errors": [],
            }
            write, write_lines = CompleteAnalyzerFixture._write_json, CompleteAnalyzerFixture._write_jsonl
            write(os.path.join(run_dir, "schedule.json"), schedule)
            write(os.path.join(run_dir, "provenance.json"), provenance)
            write(os.path.join(run_dir, "status.json"), status)
            write(os.path.join(run_dir, "host-control-warmup.json"), warmup)
            write(os.path.join(run_dir, "uffd-serve.json"), serve.record)
            write_lines(os.path.join(run_dir, "bursts.jsonl"), summaries)
            write_lines(os.path.join(run_dir, "requests.jsonl"), requests)
            write_lines(os.path.join(run_dir, "host-control.jsonl"), controls)
            write_lines(os.path.join(run_dir, "host-samples.jsonl"), samples)

            with mock.patch.object(reqscale_analyze, "BOOTSTRAP_DRAWS", 1000):
                analysis = reqscale_analyze.analyze(run_dir)
            self.assertTrue(analysis["publishable"], analysis["publication_blocked_by"])
            self.assertEqual(analysis["failed_rates"], [])
            self.assertEqual(len(analysis["cells"]), 2)
            # The real probe found one Firecracker per request, in its path's cgroup.
            roles = {name: {item["role"] for item in final_cgroups[name]["observed"]}
                     for name in ("file", "uffd")}
            self.assertIn("firecracker-012", roles["file"])
            self.assertIn("uffd-serve", roles["uffd"])
            self.assertEqual(len({(row["firecracker_process_faults_ready_to_artifact"]["pid"],
                                   row["firecracker_process_faults_ready_to_artifact"]["pid_start_time_ticks"])
                                  for row in requests}), len(requests))


class TracePerturbationIsAGate(unittest.TestCase):
    def pair(self, control_ms, traced_ms, pair_id="p0"):
        common = dict(
            trace_pair_id=pair_id, target_rps=4.0,
            request_plan_count=40, request_plan_sha256="a" * 64,
            total_artifact_completed=40, total_drained=40,
            total_cleanup_confirmed=40, failed=0,
        )
        cells = lambda value: {
            backend: {"artifact_latency_ms": {"median": value}}
            for backend in ("file", "uffd")
        }
        return [
            dict(common, traced=False, backends=cells(control_ms)),
            dict(common, traced=True, backends=cells(traced_ms)),
        ]

    def test_a_declared_limit_is_applied_to_each_matched_pair(self):
        verdict = reqscale.evaluate_trace_perturbation(self.pair(100.0, 103.0), 5.0)
        self.assertTrue(verdict["passed"])
        self.assertEqual(len(verdict["pairs"]), 2)
        self.assertAlmostEqual(verdict["pairs"][0]["median_delta_pct"], 3.0)

    def test_excess_perturbation_and_missing_controls_block_publication(self):
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "perturbation"):
            reqscale.evaluate_trace_perturbation(self.pair(100.0, 108.0), 5.0)
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "matched"):
            reqscale.evaluate_trace_perturbation(self.pair(100.0, 103.0)[:1], 5.0)


class FaultTraceIsJoinable(unittest.TestCase):
    def trace_lines(self):
        return [
            json.dumps({"type": "attached_probes", "data": {"probes": 4}}),
            json.dumps({"type": "printf", "data": "REQSCALE_TRACE_READY\\n"}),
            json.dumps({"type": "map", "data": {"@opened": {"200": 1}}}),
            json.dumps({"type": "map", "data": {"@closed": {"200": 1}}}),
            json.dumps({"type": "map", "data": {"@entered": {"200": 3}}}),
            json.dumps({"type": "map", "data": {"@completed": {"200": 3}}}),
            json.dumps({"type": "map", "data": {"@total_ns": {"200": 90}}}),
            json.dumps({"type": "hist", "data": {
                "@latency_ns": {"200": [
                    {"min": 16, "max": 31, "count": 1},
                    {"min": 32, "max": 63, "count": 2},
                ]}
            }}),
        ]

    def test_count_total_and_histogram_join_to_the_exact_firecracker(self):
        parsed = reqscale.parse_fault_trace(self.trace_lines())
        rows = [{
            "request_id": "r0",
            "firecracker_process_faults_ready_to_artifact": {
                "pid": 200, "pid_start_time_ticks": 500,
            },
        }]
        reqscale.join_fault_trace(rows, parsed)
        metric = rows[0]["firecracker_process_handle_mm_fault_ready_to_artifact"]
        self.assertEqual(metric["count"], 3)
        self.assertEqual(metric["total_ns"], 90)
        self.assertEqual(
            sum(bucket["count"] for bucket in metric["histogram"]),
            3,
        )
        self.assertIn("not filtered to guest RAM", metric["scope"])

    def test_zero_fault_interval_is_a_complete_measurement(self):
        parsed = reqscale.parse_fault_trace([
            json.dumps({"type": "printf", "data": "REQSCALE_TRACE_READY\\n"}),
            json.dumps({"type": "map", "data": {"@opened": {"200": 1}}}),
            json.dumps({"type": "map", "data": {"@closed": {"200": 1}}}),
        ])
        self.assertEqual(
            parsed["processes"][200],
            {"count": 0, "total_ns": 0, "histogram": []},
        )

    def test_readiness_requires_the_exact_json_printf_event(self):
        self.assertTrue(reqscale._is_trace_ready_line(json.dumps({
            "type": "printf", "data": "REQSCALE_TRACE_READY\\n",
        })))
        self.assertFalse(reqscale._is_trace_ready_line(json.dumps({
            "type": "log", "data": "mentions REQSCALE_TRACE_READY but is not readiness",
        })))
        self.assertFalse(reqscale._is_trace_ready_line("REQSCALE_TRACE_READY"))

    def test_incomplete_probes_or_pid_reuse_fail_closed(self):
        lines = self.trace_lines()
        lines[5] = json.dumps({"type": "map", "data": {"@completed": {"200": 2}}})
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "entered"):
            reqscale.parse_fault_trace(lines)

        parsed = reqscale.parse_fault_trace(self.trace_lines())
        rows = [
            {"request_id": "r0", "firecracker_process_faults_ready_to_artifact": {
                "pid": 200, "pid_start_time_ticks": 500}},
            {"request_id": "r1", "firecracker_process_faults_ready_to_artifact": {
                "pid": 200, "pid_start_time_ticks": 501}},
        ]
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "reused"):
            reqscale.join_fault_trace(rows, parsed)

    def test_a_successful_request_missing_from_the_trace_is_invalid(self):
        parsed = reqscale.parse_fault_trace(self.trace_lines())
        rows = [{
            "request_id": "r0", "ok": True,
            "firecracker_process_faults_ready_to_artifact": {
                "pid": 201, "pid_start_time_ticks": 600,
            },
        }]
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "no handle_mm_fault"):
            reqscale.join_fault_trace(rows, parsed)

    def test_an_unjoined_trace_process_is_invalid(self):
        parsed = reqscale.parse_fault_trace(self.trace_lines())
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "no exact request"):
            reqscale.join_fault_trace([], parsed)

    def test_trace_json_rejects_duplicate_keys_and_nonstandard_constants(self):
        lines = self.trace_lines()
        lines.append('{"type":"map","type":"hist","data":{}}')
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "duplicate"):
            reqscale.parse_fault_trace(lines)
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "non-standard"):
            reqscale.parse_fault_trace(["{\"value\":NaN}"])

    def test_analyzer_reconstructs_joined_fault_metrics_from_raw_bpftrace(self):
        spec = reqscale.BurstSpec(
            burst_id="b-traced", block_id="trace-on",
            population="trace-perturbation", target_rps=1.0, repeat=0,
            seed=1, traced=True, trace_pair_id="trace:0",
            ramp_seconds=0.0, score_seconds=1.0,
            requests=(reqscale.RequestPlan(0, 0, "score", "file", 0, 1),),
        )
        row = {
            "request_id": "request-0", "ok": True,
            "firecracker_process_faults_ready_to_artifact": {
                "pid": 200, "pid_start_time_ticks": 500,
            },
        }
        reqscale.join_fault_trace([row], reqscale.parse_fault_trace(self.trace_lines()))
        with tempfile.TemporaryDirectory() as d:
            trace_dir = os.path.join(d, "fault-trace")
            os.mkdir(trace_dir)
            paths = {
                "raw": os.path.join(trace_dir, "b-traced.bpftrace.jsonl"),
                "stderr": os.path.join(trace_dir, "b-traced.bpftrace.stderr"),
                "program": os.path.join(trace_dir, "b-traced.faulttrace.bt"),
            }
            with open(paths["raw"], "w") as stream:
                stream.write("\n".join(self.trace_lines()) + "\n")
            with open(paths["stderr"], "w") as stream:
                stream.write("")
            with open(paths["program"], "w") as stream:
                stream.write("test program\n")
            artifacts = {
                name: {
                    "path": os.path.relpath(path, d),
                    "sha256": reqscale.sha256_file(path),
                    "bytes": os.stat(path).st_size,
                }
                for name, path in paths.items()
            }
            summary = {
                "burst_id": spec.burst_id,
                "fault_trace": {
                    "scope": (
                        "Firecracker process endpoint-ready through artifact return; "
                        "all VMAs, not guest-RAM-filtered and not UFFD events"
                    ),
                    "processes": 1,
                    "artifacts": artifacts,
                },
            }
            reqscale_analyze._validate_fault_trace_artifacts(
                d, {"bursts": [spec.to_dict()]}, [summary], {spec.burst_id: [row]}
            )
            row["firecracker_process_handle_mm_fault_ready_to_artifact"]["count"] = 4
            with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "differs from raw"):
                reqscale_analyze._validate_fault_trace_artifacts(
                    d,
                    {"bursts": [spec.to_dict()]},
                    [summary],
                    {spec.burst_id: [row]},
                )


class ProvenanceFailsClosed(unittest.TestCase):
    def make_snapshot(self, root):
        snapshot_dir = os.path.join(root, "snapshots", "golden")
        os.makedirs(snapshot_dir)
        paths = {}
        for field, name in (
            ("memory_path", "memory.bin"),
            ("vmstate_path", "vmstate.bin"),
            ("disk_path", "disk.raw"),
        ):
            paths[field] = os.path.join(snapshot_dir, name)
            with open(paths[field], "wb") as f:
                f.write(field.encode())
        config = {
            "name": "golden", "vm_id": "vm-source",
            "generation_id": "11111111-1111-4111-8111-111111111111",
            "created_at": "2026-08-09T00:00:00Z", **paths,
            "metadata": {
                "image": "chromium:test", "vcpu": 2, "memory_mib": 1024,
                "network_mode": "rootless",
                "port_mappings": [{"host_ip": None, "host_port": 9222,
                                   "guest_port": 9222, "proto": "tcp"}],
            },
        }
        config_path = os.path.join(snapshot_dir, "config.json")
        with open(config_path, "w") as f:
            json.dump(config, f)
        return config_path, config

    def test_snapshot_identity_covers_shape_config_and_exact_artifact_stats(self):
        with tempfile.TemporaryDirectory() as d:
            self.make_snapshot(d)
            identity = reqscale.snapshot_identity(d, "golden")
            self.assertEqual(identity["vm_id"], "vm-source")
            self.assertEqual(identity["shape"]["vcpu"], 2)
            self.assertEqual(set(identity["files"]), {
                "memory_path", "vmstate_path", "disk_path", "config",
            })
            self.assertRegex(identity["config_sha256"], r"^[0-9a-f]{64}$")
            self.assertEqual(
                identity["generation_id"], "11111111-1111-4111-8111-111111111111"
            )

    def test_the_analyzer_accepts_the_snapshot_identity_the_harness_records(self):
        """fcvm writes each port mapping as an object; the analyzer only took
        strings ("9222:9222", its own fixtures' shape). RED BEFORE THE FIX: the
        first corpus scale run to finish every burst (2026-10-22, s12-r2) was
        refused with "provenance snapshot shape contains invalid values"."""
        import reqscale_analyze
        with tempfile.TemporaryDirectory() as d:
            self.make_snapshot(d)
            identity = reqscale.snapshot_identity(d, "golden")
            reqscale_analyze._validate_snapshot_provenance(identity)
            identity["shape"]["port_mappings"] = [{"host_port": 9222}]
            with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "invalid values"):
                reqscale_analyze._validate_snapshot_provenance(identity)

    def test_missing_or_noncanonical_generation_id_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            config_path, config = self.make_snapshot(d)
            del config["generation_id"]
            with open(config_path, "w") as f:
                json.dump(config, f)
            with self.assertRaisesRegex(reqscale.MeasurementInvalid, "generation_id"):
                reqscale.snapshot_identity(d, "golden")

    def test_duplicate_generation_id_is_rejected_instead_of_taking_the_last(self):
        with tempfile.TemporaryDirectory() as d:
            config_path, config = self.make_snapshot(d)
            encoded = json.dumps(config)
            encoded = encoded.replace(
                '"generation_id":',
                '"generation_id":"22222222-2222-4222-8222-222222222222",'
                '"generation_id":',
                1,
            )
            with open(config_path, "w") as stream:
                stream.write(encoded)
            with self.assertRaisesRegex(reqscale.MeasurementInvalid, "duplicate"):
                reqscale.snapshot_identity(d, "golden")

    def test_snapshot_artifact_outside_its_generation_directory_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            config_path, config = self.make_snapshot(d)
            outside = os.path.join(d, "outside-memory.bin")
            with open(outside, "wb") as f:
                f.write(b"outside")
            config["memory_path"] = outside
            with open(config_path, "w") as f:
                json.dump(config, f)
            with self.assertRaisesRegex(reqscale.MeasurementInvalid, "escapes"):
                reqscale.snapshot_identity(d, "golden")

    def test_generation_lease_blocks_tag_replacement_and_verifies_terminal_identity(self):
        with tempfile.TemporaryDirectory() as d:
            self.make_snapshot(d)
            lock_path = os.path.join(d, "snapshots", "golden.lock")
            with reqscale.SnapshotGenerationLease(d, "golden") as lease:
                self.assertEqual(lease.verify(), lease.identity)
                contender = open(lock_path, "a+")
                try:
                    with self.assertRaises(BlockingIOError):
                        reqscale.fcntl.flock(
                            contender,
                            reqscale.fcntl.LOCK_EX | reqscale.fcntl.LOCK_NB,
                        )
                finally:
                    contender.close()

    def test_unsafe_snapshot_tag_is_rejected_before_a_lock_path_is_opened(self):
        with tempfile.TemporaryDirectory() as d:
            os.mkdir(os.path.join(d, "snapshots"))
            with self.assertRaisesRegex(ValueError, "snapshot tag"):
                reqscale.SnapshotGenerationLease(d, "../escape")
            self.assertFalse(os.path.exists(os.path.join(d, "escape.lock")))


class AFailedRateDoesNotVoidTheRun(unittest.TestCase):
    """#1159: one teardown ENOENT in 2,015 requests refused an otherwise
    complete run on 2026-10-22, and a rate above the ceiling voided every rate
    measured with it. A failed request now fails its rate, not the run."""

    class Audit:
        def __init__(self):
            self.ticks = 0

        def cpu_snapshot(self):
            self.ticks += 1
            return {"usage_usec": self.ticks}

        def record(self):
            return {"path": "/fake", "live_pids": [], "observed": []}

    def _execute(self, schedule, fail):
        """Run execute() over `schedule` with every host boundary replaced;
        a burst fails one request when fail(spec) says so."""
        audit = self.Audit()

        class Scope:
            def __init__(self, *_args):
                self.paths = {name: f"/sys/fs/cgroup/fake/{name}"
                              for name in ("driver", "control", "file", "uffd")}

            def enter(self):
                return {name: audit for name in ("run", "driver", "control", "file", "uffd")}

            def leave(self):
                return {}

        class Serve:
            record = {"kind": "uffd-serve"}

            def __init__(self, *_args):
                pass

            def start(self):
                return 30

            def stop(self):
                pass

        class Control:
            proc = SimpleNamespace(pid=20)

            def __init__(self, *_args):
                pass

            def start(self):
                return {"kind": "host-control-warmup"}

            def stop(self):
                pass

        class Background:
            def __init__(self, *_args):
                pass

            def start(self):
                pass

            def check(self):
                pass

            def stop(self):
                return {"started": True}

        class Lease:
            def verify(self):
                return {"generation_id": "11111111-1111-4111-8111-111111111111"}

        def burst(_run_id, spec, _request_fn, *_args):
            failed = 1 if fail(spec) else 0
            planned = len(spec.requests)
            backend = {"artifact_completed": planned, "planned": planned}
            return [], {
                "burst_id": spec.burst_id, "population": spec.population,
                "total_planned": planned, "failed": failed,
                "total_artifact_completed": planned - failed,
                "total_drained": planned, "total_cleanup_confirmed": planned,
                "backends": {"file": backend, "uffd": backend}, "drain_span_ms": 1.0,
            }

        proc_stat = {"cpu": {"user": 1}, "raw": "cpu 1\n"}
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(reqscale, "RunCgroups", Scope), \
                mock.patch.object(reqscale, "UffdServe", Serve), \
                mock.patch.object(reqscale, "NativeChromiumControl", Control), \
                mock.patch.object(reqscale, "ControlScheduler", Background), \
                mock.patch.object(reqscale, "HostSampler", Background), \
                mock.patch.object(reqscale, "read_machine_proc_stat", return_value=proc_stat), \
                mock.patch.object(reqscale, "_interburst_membership", return_value={}), \
                mock.patch.object(reqscale, "_make_request_fn", return_value=None), \
                mock.patch.object(reqscale, "run_open_loop_burst", side_effect=burst):
            args = SimpleNamespace(
                out_dir=os.path.join(d, "run"), run_id=RUN_ID,
                cgroup_root="/sys/fs/cgroup", trace_faults=False,
                snapshot_generation_lease=Lease(),
            )
            with redirect_stderr(io.StringIO()), mock.patch("sys.stdout", io.StringIO()):
                rc = reqscale.execute(args, schedule, {"schema": "test"})
            with open(os.path.join(args.out_dir, "status.json")) as f:
                status = json.load(f)
            with open(os.path.join(args.out_dir, "bursts.jsonl")) as f:
                summaries = [json.loads(line) for line in f]
        return rc, status, summaries

    @staticmethod
    def _schedule():
        return reqscale.build_schedule(reqscale.ScheduleConfig(
            rates=(2.0, 4.0), scored_bursts=5, seed=776, urls=("http://x/",),
            criteria=reqscale.CapacityCriteria(
                max_offered_rps_error_pct=1.0, min_departure_ratio=0.95,
                max_score_end_backlog=8, max_p95_launch_lag_ms=25.0,
                max_control_median_drift_pct=10.0)), RUN_ID)

    def test_a_failed_request_fails_its_rate_and_the_run_measures_the_rest(self):
        """RED BEFORE THE FIX: execute() raised "burst ... has failed or
        unconfirmed-clean requests" at the first failed request, returned 4,
        and measured none of the bursts after it."""
        schedule = self._schedule()
        failing = [raw["burst_id"] for raw in schedule["bursts"]
                   if raw["target_rps"] == 4.0][1:3]
        rc, status, summaries = self._execute(
            schedule, lambda spec: spec.burst_id in failing)
        self.assertEqual(rc, 0, status["error"])
        self.assertTrue(status["valid"])
        self.assertEqual(status["schema"], reqscale.STATUS_SCHEMA)
        self.assertEqual(status["bursts_completed"], len(schedule["bursts"]))
        self.assertEqual(len(summaries), len(schedule["bursts"]))
        self.assertEqual(status["failed_rates"], [{
            "target_rps": 4.0,
            "bursts": [{
                "burst_id": burst_id,
                "population": next(raw["population"] for raw in schedule["bursts"]
                                   if raw["burst_id"] == burst_id),
                "failed": 1, "artifact_missing": 1, "undrained": 0,
                "cleanup_unconfirmed": 0,
            } for burst_id in failing],
        }])

    def test_a_failed_trace_perturbation_request_still_refuses_the_run(self):
        """The trace pairs compare one plan traced and untraced; a failure in
        either leaves no pair to compare, so it stays a refusal."""
        schedule = self._schedule()
        schedule["bursts"][3]["population"] = "trace-perturbation"
        trace_burst = schedule["bursts"][3]["burst_id"]
        rc, status, summaries = self._execute(
            schedule, lambda spec: spec.burst_id == trace_burst)
        self.assertEqual(rc, 4)
        self.assertFalse(status["valid"])
        self.assertIn(f"trace burst {trace_burst} has failed", status["error"])
        self.assertEqual(len(summaries), 4)


class DurableOutput(unittest.TestCase):
    def test_schedule_is_exclusive_and_fsynced_before_records(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "schedule.json")
            reqscale.write_json_exclusive(path, {"schema": "x", "windows": []})
            with open(path) as f:
                self.assertEqual(json.load(f)["schema"], "x")
            with self.assertRaises(FileExistsError):
                reqscale.write_json_exclusive(path, {"schema": "replacement"})

    def test_jsonl_sink_never_appends_to_an_old_run(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "requests.jsonl")
            with reqscale.JsonlSink(path) as sink:
                sink.write({"record_id": "r0"})
            with self.assertRaises(FileExistsError):
                reqscale.JsonlSink(path)

    def test_terminal_status_preserves_run_and_every_cleanup_failure(self):
        class Audit:
            def cpu_snapshot(self):
                return {"usage_usec": 1}

            def record(self):
                return {"path": "/fake", "live_pids": [], "observed": []}

        audit = Audit()

        class Scope:
            def __init__(self, *_args):
                self.path = "/sys/fs/cgroup/fake"
                self.paths = {
                    name: f"/sys/fs/cgroup/fake/{name}"
                    for name in ("driver", "control", "file", "uffd")
                }

            def enter(self):
                return {
                    name: audit for name in ("run", "driver", "control", "file", "uffd")
                }

            def leave(self):
                raise reqscale.MeasurementInvalid("leave failed", {"left": False})

        class Serve:
            def __init__(self, *_args):
                pass

            def start(self):
                raise reqscale.MeasurementInvalid("start failed")

            def stop(self):
                raise reqscale.MeasurementInvalid("stop failed")

        class Sampler:
            def __init__(self, *_args):
                pass

            def start(self):
                pass

            def stop(self):
                return {"samples": 0}

        class Lease:
            def verify(self):
                return {"generation_id": "11111111-1111-4111-8111-111111111111"}

        with tempfile.TemporaryDirectory() as d, mock.patch.object(
            reqscale, "RunCgroups", Scope
        ), mock.patch.object(reqscale, "UffdServe", Serve), mock.patch.object(
            reqscale, "HostSampler", Sampler
        ), mock.patch.object(
            reqscale,
            "read_machine_proc_stat",
            return_value={"cpu": {"user": 1}, "raw": "cpu 1\n"},
        ):
            args = SimpleNamespace(
                out_dir=os.path.join(d, "run"), run_id=RUN_ID,
                cgroup_root="/sys/fs/cgroup", trace_faults=False,
                snapshot_generation_lease=Lease(),
            )
            with redirect_stderr(io.StringIO()):
                rc = reqscale.execute(
                    args,
                    {
                        "run_id": RUN_ID, "bursts": [],
                        "host_sample_interval_ns": 5_000_000_000,
                        "control": {},
                    },
                    {"schema": "test"},
                )
            self.assertEqual(rc, 4)
            with open(os.path.join(args.out_dir, "status.json")) as f:
                status = json.load(f)
            self.assertFalse(status["valid"])
            self.assertEqual(
                [row["phase"] for row in status["errors"]],
                ["run", "serve-stop", "cgroup-leave"],
            )
            self.assertEqual(status["errors"][2]["details"], {"left": False})


class PlanOnlyCli(unittest.TestCase):
    SCRIPT = os.path.join(HERE, "reqscale.py")

    @staticmethod
    def criteria_args():
        return [
            "--max-offered-rps-error-pct", "1",
            "--min-departure-ratio", "0.95",
            "--max-score-end-backlog", "8",
            "--max-p95-launch-lag-ms", "25",
            "--max-control-median-drift-pct", "10",
        ]

    def test_plan_only_writes_the_complete_schedule_without_touching_a_vm(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "plan")
            result = subprocess.run(
                [
                    sys.executable, self.SCRIPT,
                    "--snapshot-tag", "not-opened-in-plan-mode",
                    "--url", "http://127.0.0.1:8000/medium.html",
                    "--rates", "2,4", "--bursts", "5",
                    "--seed", "776", "--run-id", RUN_ID,
                    "--out-dir", out, "--plan-only",
                    *self.criteria_args(),
                ],
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(os.path.join(out, "schedule.json")) as f:
                schedule = json.load(f)
            self.assertEqual(schedule["schema"], reqscale.SCHEDULE_SCHEMA)
            self.assertEqual(len(schedule["bursts"]), 12)
            self.assertEqual(schedule["ramp_seconds"], 15.0)
            self.assertEqual(schedule["score_seconds"], 60.0)
            self.assertFalse(os.path.exists(os.path.join(out, "requests.jsonl")))

    def test_tracing_without_a_predeclared_perturbation_limit_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            result = subprocess.run(
                [
                    sys.executable, self.SCRIPT,
                    "--snapshot-tag", "x", "--url", "http://x/",
                    "--rates", "2", "--bursts", "5",
                    "--seed", "776", "--run-id", RUN_ID,
                    "--out-dir", os.path.join(d, "out"),
                    "--trace-faults", "--trace-rate", "2", "--trace-pairs", "3",
                    "--plan-only",
                    *self.criteria_args(),
                ],
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("max-trace-perturbation-pct", result.stderr)
            self.assertFalse(os.path.exists(os.path.join(d, "out")))

    def _plan(self, *extra):
        with tempfile.TemporaryDirectory() as d:
            return subprocess.run(
                [
                    sys.executable, self.SCRIPT,
                    "--snapshot-tag", "not-opened-in-plan-mode",
                    "--rates", "2", "--bursts", "5",
                    "--seed", "776", "--run-id", RUN_ID,
                    "--out-dir", os.path.join(d, "plan"), "--plan-only",
                    *self.criteria_args(), *extra,
                ],
                capture_output=True, text=True, timeout=30,
            )

    def test_a_url_list_needs_a_single_control_url(self):
        """Red before URL lists: the list became the control's one URL."""
        corpus = "https://a.example/,https://b.example/"
        result = self._plan("--url", corpus)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--control-url is required", result.stderr)
        result = self._plan("--url", corpus, "--control-url", corpus)
        self.assertEqual(result.returncode, 2)
        self.assertIn("exactly one URL", result.stderr)
        result = self._plan("--url", corpus, "--control-url", "https://a.example/",
                            "--control-resolve-all-to", "127.0.0.1")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_resolver_rule_must_be_an_ipv4_address(self):
        result = self._plan("--url", "http://x/", "--control-resolve-all-to", "replay")
        self.assertEqual(result.returncode, 2)
        self.assertIn("IPv4", result.stderr)


class ScheduleCheckBeforeTheGolden(unittest.TestCase):
    """#1159: the rate rules are checked in seconds, before anything is built.

    `make bench-chromium-corpus MEASURE=scale` learned that SCALE_RATES broke
    reqscale.py's schedule rules only from the measured run, after the golden
    and the diag; two attempts (0.7 and 0.8 req/s) were refused that way.
    """

    REPO = os.path.dirname(os.path.dirname(HERE))

    def _check(self, rates, out, urls=None, control_url="https://site0.example/"):
        return subprocess.run(
            [
                sys.executable, PlanOnlyCli.SCRIPT, "--check-schedule",
                "--snapshot-tag", "not-opened-by-the-check",
                "--url", ",".join(urls or CORPUS), "--control-url", control_url,
                "--rates", rates, "--bursts", "5", "--seed", "776",
                "--out-dir", out, *PlanOnlyCli.criteria_args(),
            ],
            capture_output=True, text=True, timeout=30,
        )

    def test_check_schedule_refuses_a_split_corpus_cycle_and_writes_nothing(self):
        """RED BEFORE THE FIX: reqscale.py had no --check-schedule and exited 2
        with "unrecognized arguments: --check-schedule"."""
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "run")
            for rates, why in (
                ("0.7", "0.7 * 15.0 = 10.5"),
                ("0.8", "not a whole number of cycles of the 14-page corpus"),
            ):
                refused = self._check(rates, out)
                self.assertEqual(refused.returncode, 2, refused.stderr)
                self.assertIn(why, refused.stderr)
            accepted = self._check("0.933333333333,1.4,1.866666666667", out)
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertIn("schedule ok: 18 bursts", accepted.stdout)
            self.assertEqual(os.listdir(d), [], "a schedule check wrote a run directory")

    def test_the_make_check_runs_the_harness_rules_on_the_scale_arguments(self):
        """RED BEFORE THE FIX: there was no check-chromium-scale target."""
        variables = [
            "SCALE_BURSTS=5", "SCALE_SEED=776", "SCALE_TAG=not-opened",
            "SCALE_CONTROL_CHROMIUM=chromium", "SCALE_MAX_OFFERED_ERROR_PCT=1",
            "SCALE_MIN_DEPARTURE_RATIO=0.95", "SCALE_MAX_BACKLOG=8",
            "SCALE_MAX_LAUNCH_LAG_MS=25", "SCALE_MAX_CONTROL_DRIFT_PCT=10",
            f"SCALE_URL={','.join(CORPUS)}", "SCALE_CONTROL_URL=https://site0.example/",
        ]
        with tempfile.TemporaryDirectory() as d:
            for rates, expected in (("0.8", 2), ("1.4", 0)):
                result = subprocess.run(
                    ["make", "-s", "-C", self.REPO, "check-chromium-scale",
                     f"SCALE_RATES={rates}", f"SCALE_OUT={d}/run", *variables],
                    capture_output=True, text=True, timeout=60,
                )
                self.assertEqual(result.returncode != 0, expected != 0,
                                 (rates, result.stdout, result.stderr))
            self.assertIn("not a whole number of cycles", subprocess.run(
                ["make", "-s", "-C", self.REPO, "check-chromium-scale",
                 "SCALE_RATES=0.8", f"SCALE_OUT={d}/run", *variables],
                capture_output=True, text=True, timeout=60).stderr)
            self.assertEqual(os.listdir(d), [])

    def test_the_makefile_rate_examples_pass_the_harness_rules(self):
        """The Makefile's only SCALE_RATES example, 2,4,8, is refused for the
        14-page corpus. RED BEFORE THE FIX: no example in the message passed
        the corpus rule."""
        with open(os.path.join(self.REPO, "Makefile")) as handle:
            message = re.search(r'echo "ERROR: SCALE_RATES required([^"]*)"', handle.read())
        self.assertIsNotNone(message, "the Makefile no longer explains SCALE_RATES")
        examples = re.findall(r"\d[\d.]*(?:,\d[\d.]*)*", message.group(1))
        examples = [example for example in examples if "," in example or "." in example]
        self.assertTrue(examples, message.group(1))

        def refusal(rates, urls):
            config = dataclasses.replace(
                CorpusPairing._config(None, 1.0),
                rates=tuple(float(rate) for rate in rates.split(",")), urls=urls)
            try:
                reqscale.build_schedule(config, RUN_ID)
            except ValueError as error:
                return str(error)
            return None

        corpus = {example: refusal(example, CORPUS) for example in examples}
        self.assertTrue(any(error is None for error in corpus.values()),
                        f"no SCALE_RATES example passes the 14-page corpus: {corpus}")
        for example, error in corpus.items():
            if error is not None:
                self.assertIsNone(refusal(example, ("http://127.0.0.1/page",)),
                                  f"{example} passes neither one page nor the corpus")



class ConcurrentRequestRecords(unittest.TestCase):
    @staticmethod
    def _inputs(urls=None, pair_index=0):
        args = SimpleNamespace(
            url="http://x/", urls=urls, format="jpeg", quality=80, cdp_port=9222, ws_url="",
            fcvm="fcvm", data_root="/d", state_dir="/s", timeout=10.0,
            teardown_timeout=5.0, rust_log="off", run_id="r", snapshot_tag="t",
            cgroup_paths={"uffd": "/sys/fs/cgroup/x/uffd", "file": "/sys/fs/cgroup/x/file"},
            snapshot_identity={"generation_id": "g", "config_sha256": "c"},
        )
        spec = SimpleNamespace(burst_id="b", block_id=0, target_rps=2.0,
                               population="score", traced=False, trace_pair_id=None)
        context = reqscale.RequestContext(
            run_id="r", burst_id="b", population="score", segment="score",
            backend="uffd", target_rps=2.0, request_index=0, pair_index=pair_index,
            request_id="r:b:0", scheduled_ns=0, actual_launch_ns=1, request_seed=7,
        )
        return args, spec, context

    def _record(self, cdp_record, urls=None, pair_index=0):
        args, spec, context = self._inputs(urls, pair_index)
        request = reqscale._make_request_fn(
            args, spec, 1234, "/logs", {"uffd": mock.Mock(), "file": mock.Mock()}, None, 0)
        seen = []

        def run(request_args, rep, fast, **_options):
            seen.append(request_args.url)
            return dict(cdp_record)

        with mock.patch.object(reqscale.reqbench, "run_cdp_request", side_effect=run):
            record = request(context)
        self.assertEqual(seen, [record["url"]], "the record names the page it rendered")
        return record

    def test_a_concurrent_request_never_reads_the_memory_server(self):
        """Red while reqscale only dropped the samples from the record:
        run_cdp_request had already read the shared server's /proc/<pid>/stat
        before each UFFD clone's launch (and again after its teardown), which
        no FILE request does, so the UFFD arm alone carried that work."""
        args, spec, context = self._inputs()
        reads = []

        class Stop(Exception):
            pass

        def sample(pid):
            reads.append(pid)
            return {"applicable": True, "ms": 0.0, "starttime": 1}

        with tempfile.TemporaryDirectory() as state_dir:
            args.state_dir = state_dir
            request = reqscale._make_request_fn(
                args, spec, 1234, state_dir,
                {"uffd": mock.Mock(), "file": mock.Mock()}, None, 0)
            # The clone launch is stopped right after the point where the
            # request would have sampled the server before it.
            with mock.patch.object(reqscale.reqbench, "serve_cpu_sample", side_effect=sample), \
                    mock.patch.object(reqscale.reqbench, "state_path_baseline", side_effect=Stop):
                with self.assertRaises(Stop):
                    request(context)
        self.assertEqual(reads, [], "a concurrent UFFD request read the memory server")

    def test_a_corpus_request_renders_its_pairs_url_and_says_so(self):
        """Red before URL lists: every request rendered args.url, the whole
        comma-separated spec, and no record named its page."""
        urls = [f"https://site{i}.example/" for i in range(14)]
        record = self._record({"ok": True}, urls=urls, pair_index=15)
        self.assertEqual(record["url"], urls[1])


CORPUS = tuple(f"https://site{i}.example/" for i in range(14))


class CorpusPairing(unittest.TestCase):
    def _config(self, rate):
        return reqscale.ScheduleConfig(
            rates=(rate,), scored_bursts=5, seed=776, urls=CORPUS,
            criteria=reqscale.CapacityCriteria(
                max_offered_rps_error_pct=1.0, min_departure_ratio=0.95,
                max_score_end_backlog=8, max_p95_launch_lag_ms=25.0,
                max_control_median_drift_pct=10.0))

    def test_every_scored_window_renders_each_page_equally_often(self):
        """Red before the whole-cycle rule: at 2 rps the 120 scored pairs of a
        burst gave eight pages 9 renders and six pages 8, the same bias in
        every burst, so the rate curve was confounded with the page mix."""
        schedule = reqscale.build_schedule(self._config(1.4), RUN_ID)
        self.assertEqual(schedule["urls"], list(CORPUS))
        args = SimpleNamespace(url=",".join(CORPUS), urls=list(CORPUS))
        for raw in schedule["bursts"]:
            spec = reqscale.BurstSpec.from_dict(raw)
            for backend in ("file", "uffd"):
                pages = [reqscale.request_url(args, SimpleNamespace(pair_index=r.pair_index))
                         for r in spec.requests
                         if r.segment == "score" and r.backend == backend]
                counts = {url: pages.count(url) for url in CORPUS}
                self.assertEqual(set(counts.values()), {len(pages) // len(CORPUS)},
                                 (raw["burst_id"], backend, counts))
            halves = {}
            for r in spec.requests:
                halves.setdefault(r.pair_index, set()).add(
                    reqscale.request_url(args, SimpleNamespace(pair_index=r.pair_index)))
            self.assertTrue(all(len(urls) == 1 for urls in halves.values()))

    def test_a_rate_that_splits_a_corpus_cycle_is_refused(self):
        with self.assertRaisesRegex(ValueError, "whole number of cycles"):
            reqscale.build_schedule(self._config(2.0), RUN_ID)

    def test_a_single_url_is_unchanged(self):
        args = SimpleNamespace(url="http://x/", urls=None)
        self.assertEqual(reqscale.request_url(args, SimpleNamespace(pair_index=9)), "http://x/")


class AnalyzerHoldsTheCorpus(unittest.TestCase):
    def test_a_request_that_rendered_another_page_is_rejected(self):
        """Red before the corpus was in the schedule: a producer rendering one
        page for every request passed as the corpus experiment."""
        schedule, summaries, records = AnalyzerRejectsCorruptEvidence.fixture()
        records[0]["url"] = "http://elsewhere/"
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "url"):
            reqscale_analyze._validate_requests(
                schedule, summaries, records,
                AnalyzerRejectsCorruptEvidence.GENERATION,
                AnalyzerRejectsCorruptEvidence.CONFIG_SHA)

    def _gate(self, evidence):
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "scale")
            os.mkdir(run_dir)
            if evidence is not None:
                with open(os.path.join(d, "dns-evidence.json"), "w") as handle:
                    json.dump(evidence, handle)
            return reqscale_analyze.corpus_dns_gate(
                run_dir, {"run_id": RUN_ID, "urls": list(CORPUS)},
                {"host_control": {"resolve_all_to": "127.0.0.1"}})

    def test_a_corpus_run_needs_the_campaigns_clean_dns_evidence(self):
        """Red before the gate: a corpus run's analysis said publishable with
        no evidence that its clones resolved through the replay server."""
        self.assertIn("without the campaign's DNS evidence", self._gate(None))
        self.assertIn("not 'clean'", self._gate({"verdict": "unclean", "run_id": RUN_ID}))
        self.assertIn("records run_id='other'",
                      self._gate({"verdict": "clean", "run_id": "other"}))

    def test_the_dns_evidence_is_held_to_every_file_it_pins(self):
        """Red while the gate read only verdict and run_id: an object holding
        just those two fields, or a bundle whose replay log changed after the
        verdict, left a corpus run publishable. The gate is campaign_summary's
        check, so a bundle the index would refuse is refused here too."""
        from test_campaign_summary import write_run

        bare = self._gate({"verdict": "clean", "run_id": RUN_ID})
        self.assertIsNotNone(bare, "two fields passed as a whole DNS evidence bundle")
        self.assertIn("samples", bare)
        schedule = {"run_id": RUN_ID, "urls": ["https://example.com/"]}
        provenance = {"host_control": {"resolve_all_to": "127.0.0.1"}}
        with tempfile.TemporaryDirectory() as d:
            write_run(d, analysis_overrides={"run_id": RUN_ID})
            run_dir = os.path.join(d, "scale")
            os.mkdir(run_dir)
            self.assertIsNone(reqscale_analyze.corpus_dns_gate(run_dir, schedule, provenance))
            with open(os.path.join(d, "corpus-dns.log"), "a") as handle:
                handle.write("{}\n")
            refused = reqscale_analyze.corpus_dns_gate(run_dir, schedule, provenance)
        self.assertIsNotNone(refused, "a replay log changed after the verdict passed")
        self.assertIn("corpus-dns.log sha256", refused)

    def test_a_single_page_run_needs_no_resolver_evidence(self):
        self.assertIsNone(reqscale_analyze.corpus_dns_gate(
            "/nonexistent/scale", {"run_id": RUN_ID, "urls": ["http://127.0.0.1/f"]},
            {"host_control": {"resolve_all_to": None}}))

    def test_the_control_resolver_must_be_null_or_an_ipv4_address(self):
        for good in (None, "127.0.0.1"):
            self.assertTrue(reqscale_analyze._null_or_ipv4(good), good)
        for bad in ("", "replay", "127.0.0.01", 7):
            self.assertFalse(reqscale_analyze._null_or_ipv4(bad), bad)


class ScaleOutputHonoursThePublicationGate(unittest.TestCase):
    """main() is what the campaign and `make report-chromium-scale` run."""

    @staticmethod
    def _corpus_run(d, evidence=True):
        from test_campaign_summary import write_run

        run_dir = os.path.join(d, "scale")
        os.mkdir(run_dir)
        CompleteAnalyzerFixture.build_run(
            run_dir, url="https://example.com/", resolve_all_to="127.0.0.1")
        if evidence:
            write_run(d, analysis_overrides={"run_id": RUN_ID})
        return run_dir

    @staticmethod
    def _main(*argv):
        err = io.StringIO()
        with mock.patch.object(sys, "argv", ["reqscale_analyze.py", *argv]), \
                mock.patch.object(reqscale_analyze, "BOOTSTRAP_DRAWS", 1000), \
                redirect_stderr(err):
            return reqscale_analyze.main(), err.getvalue()

    @staticmethod
    def _load(path):
        with open(path) as stream:
            return json.load(stream)

    def test_an_unpublishable_analysis_gets_json_but_no_report(self):
        """RED BEFORE THE FIX: with the campaign's DNS evidence absent the
        analysis said publishable false, and --markdown-out still wrote a
        report saying the run passed its checks, at exit 0. The JSON-only
        analysis still exits 0, because the campaign writes it before the
        evidence exists to bind the run id the evidence names."""
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as out:
            run_dir = self._corpus_run(d, evidence=False)
            report = os.path.join(out, "report.md")
            rc, err = self._main("--run-dir", run_dir, "--markdown-out", report)
            self.assertNotEqual(rc, 0, "a report was written for an unpublishable analysis")
            self.assertFalse(os.path.lexists(report))
            self.assertIn("DNS evidence", err)
            pre = os.path.join(out, "analysis-pre-evidence.json")
            rc, err = self._main("--run-dir", run_dir, "--json-out", pre)
            self.assertEqual(rc, 0, err)
            self.assertFalse(self._load(pre)["publishable"])

    def test_a_withdrawn_run_or_campaign_is_not_publishable(self):
        """RED BEFORE THE FIX: the scale analyzer read no WITHDRAWN marker, so
        re-analysing a withdrawn campaign whose clean DNS evidence stayed in
        place said publishable, and its report was written."""
        for where in ("campaign", "run"):
            with self.subTest(where=where), tempfile.TemporaryDirectory() as d, \
                    tempfile.TemporaryDirectory() as out:
                run_dir = self._corpus_run(d)
                before = os.path.join(out, "before.json")
                rc, err = self._main("--run-dir", run_dir, "--json-out", before)
                self.assertEqual(rc, 0, err)
                self.assertTrue(self._load(before)["publishable"])
                marked = d if where == "campaign" else run_dir
                with open(os.path.join(marked, "WITHDRAWN"), "w") as marker:
                    marker.write("resolver was ambient\n")
                after = os.path.join(out, "after.json")
                rc, err = self._main("--run-dir", run_dir, "--json-out", after)
                self.assertEqual(rc, 0, err)
                analysis = self._load(after)
                self.assertFalse(analysis["publishable"], f"a withdrawn {where} stayed publishable")
                self.assertIn("withdrawn: resolver was ambient",
                              analysis["publication_blocked_by"])
                report = os.path.join(out, "report.md")
                rc, err = self._main("--run-dir", run_dir, "--markdown-out", report)
                self.assertNotEqual(rc, 0, err)
                self.assertFalse(os.path.lexists(report))

    def test_the_run_and_its_campaign_stay_locked_while_the_analysis_reads_them(self):
        """RED BEFORE THE FIX: the analyzer held no lock, so a withdrawal
        writer following AGENTS.md (exclusive flock on the directory, then the
        marker) took either directory in the middle of validation, and the
        analysis published from the state before its marker."""
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as out:
            run_dir = self._corpus_run(d)
            report = os.path.join(out, "report.md")
            real_analyze = reqscale_analyze.analyze
            writers = []

            def analyze_beside_a_writer(path):
                for directory in (run_dir, d):
                    writers.append(subprocess.run(
                        ["flock", "-n", "-x", directory, "sh", "-c",
                         'printf "late\\n" > "$1"', "sh",
                         os.path.join(directory, "WITHDRAWN")],
                        capture_output=True, text=True, timeout=10,
                    ).returncode)
                return real_analyze(path)

            with mock.patch.object(reqscale_analyze, "analyze",
                                   side_effect=analyze_beside_a_writer):
                rc, err = self._main("--run-dir", run_dir, "--markdown-out", report)
            self.assertEqual(writers, [1, 1],
                             "a withdrawal writer locked a directory mid-analysis")
            self.assertEqual(rc, 0, err)
            self.assertTrue(os.path.isfile(report))
            for directory in (run_dir, d):
                self.assertFalse(os.path.lexists(os.path.join(directory, "WITHDRAWN")))
                released = subprocess.run(
                    ["flock", "-n", "-x", directory, "true"],
                    capture_output=True, timeout=10,
                )
                self.assertEqual(released.returncode, 0,
                                 f"{directory}: the analysis kept its lock")


class RestorePathIsWhatItSays(unittest.TestCase):
    def test_a_file_arm_that_would_restore_through_uffd_is_refused(self):
        """Red before the check: hugepage, NV2 (kernel profile) and forced
        restores made the FILE arm an implicit UFFD one."""
        plain = {"metadata": {"hugepages": False, "kernel_profile": None}}
        self.assertIsNone(reqscale.file_restore_refusal(plain, {}))
        self.assertIn("hugepages", reqscale.file_restore_refusal(
            {"metadata": {"hugepages": True}}, {}))
        self.assertIn("nested", reqscale.file_restore_refusal(
            {"metadata": {"kernel_profile": "nested"}}, {}))
        self.assertIn("FCVM_FORCE_UFFD", reqscale.file_restore_refusal(
            plain, {"FCVM_FORCE_UFFD": "1"}))

    def test_the_server_must_run_the_requested_mode(self):
        """Red before the check: NV2 coerced minor to copy and the UFFD cells
        were labelled minor."""
        reqscale.require_serve_mode("minor", "minor")
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "runs copy mode"):
            reqscale.require_serve_mode("copy", "minor")


class SpawnedArgv(unittest.TestCase):
    """What start() hands to Popen, captured by a Popen that refuses."""

    def _capture(self, start):
        seen = []

        def refuse(argv, **_kw):
            seen.append(list(argv))
            raise RuntimeError("captured")

        with mock.patch.object(reqscale.subprocess, "Popen", side_effect=refuse):
            with self.assertRaises(Exception):
                start()
        self.assertEqual(len(seen), 1)
        return seen[0]

    def test_the_memory_server_starts_in_the_requested_mode(self):
        """Red on the parent: the serve argv had no --uffd-mode, so every run
        served copy mode."""
        with tempfile.TemporaryDirectory() as d:
            args = SimpleNamespace(fcvm="/f", snapshot_tag="cb-req-corpus", state_dir=d,
                                   data_root=d, uffd_mode="minor", uffd_prefetch="off",
                                   rust_log="fcvm=debug")
            serve = reqscale.UffdServe(args, mock.Mock(), "/sys/fs/cgroup/x/uffd", d)
            argv = self._capture(serve.start)
        self.assertEqual(argv[-8:], ["/f", "snapshot", "serve", "cb-req-corpus",
                                     "--uffd-mode", "minor", "--uffd-prefetch", "off"])

    def test_the_control_chromium_maps_names_to_the_replay_server(self):
        """Red on the parent: the control argv had no resolver rule, so a
        corpus control resolved on the live internet."""
        with tempfile.TemporaryDirectory() as d:
            args = SimpleNamespace(control_chromium="/c", control_resolve_all_to="127.0.0.1",
                                   control_tmp_root=d, run_id="r")
            control = reqscale.NativeChromiumControl(args, mock.Mock(), "/sys/fs/cgroup/x/c", d)
            argv = self._capture(control.start)
        self.assertIn("--host-resolver-rules=MAP * 127.0.0.1", argv)


if __name__ == "__main__":
    unittest.main(verbosity=2)
