#!/usr/bin/env python3
"""Deterministic tests for the open-loop Chromium request scalability harness.

No test in this file starts a VM, writes the real cgroup tree, or attaches BPF.
The production boundaries are injected so scheduling, accounting, provenance,
and fail-closed verdicts can be exercised with exact synthetic records.
"""

import hashlib
import io
import json
import os
import select
import signal
import subprocess
import sys
import tempfile
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
            RUN_ID, spec, lambda context: {"backend": context.backend}, clock, launcher
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

    def test_zero_or_multiple_firecrackers_fails_closed(self):
        proc = FakeProcReader()
        proc.comms[201] = "firecracker"
        with self.assertRaisesRegex(reqscale.MeasurementInvalid, "exactly one"):
            reqscale.FirecrackerFaultProbe(proc, FakeAudit()).begin(100)

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
            RUN_ID, spec, lambda context: {"backend": context.backend},
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
        schedule = {"run_id": RUN_ID, "bursts": [spec.to_dict()]}
        return schedule, [summary], records

    def test_green_summary_cannot_hide_a_failed_raw_request(self):
        schedule, summaries, records = self.fixture()
        records[0]["ok"] = False
        with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "failed"):
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
                "network_mode": "rootless", "port_mappings": ["9222:9222"],
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
            faults = row["firecracker_process_faults_ready_to_artifact"]
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
    def build_run(cls, directory):
        criteria = reqscale.CapacityCriteria(
            max_offered_rps_error_pct=1.0,
            min_departure_ratio=0.95,
            max_score_end_backlog=2,
            max_p95_launch_lag_ms=20.0,
            max_control_median_drift_pct=10.0,
        )
        schedule = reqscale.build_schedule(
            reqscale.ScheduleConfig(
                rates=(0.8,), scored_bursts=5, seed=776, criteria=criteria,
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
                RUN_ID, spec, lambda context: {"backend": context.backend},
                clock, DeferredLauncher(),
            )
            for row in burst_rows:
                row.update(
                    schema=reqscale.RECORD_SCHEMA,
                    kind="request",
                    run_id=RUN_ID,
                    block_id=spec.block_id,
                    cell_id=f"{row['backend']}:r0.8",
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
        provenance = {
            "schema": "fcvm.chromium.reqscale.provenance.v1",
            "run_id": RUN_ID,
            "created_at": "2026-08-09T00:00:00+00:00",
            "argv": ["reqscale.py", "--fixture"],
            "source_revision": "1" * 40,
            "source_dirty": False,
            "source_status_sha256": hashlib.sha256(b"").hexdigest(),
            "harness_sha256": "2" * 64,
            "fcvm_path": "/usr/bin/fcvm",
            "fcvm_sha256": "3" * 64,
            "fcvm_version": "fcvm fixture",
            "schedule_sha256": reqscale.schedule_sha256(schedule),
            "snapshot": snapshot,
            "snapshot_generation_lease": {
                "path": "/mnt/fcvm-btrfs/snapshots/golden.lock",
                "mode": "shared",
                "held_from_identity_read_through_terminal_verification": True,
            },
            "host": {
                "hostname": "fixture", "kernel": "fixture", "machine": "aarch64",
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
                "url": "http://127.0.0.1/fixture",
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
        run_path = f"/bench/fcvm-reqscale-{RUN_ID}"
        final_cgroups = cls._final_cgroups(run_path, requests)
        measurement_start = summaries[0]["burst_start_ns"]
        measurement_end = max(row["finished_ns"] for row in requests)

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
        status = {
            "schema": "fcvm.chromium.reqscale.status.v2",
            "run_id": RUN_ID, "valid": True,
            "bursts_completed": len(summaries),
            "bursts_planned": len(schedule["bursts"]),
            "control": {
                "started": True, "origin_monotonic_ns": control_origin,
                "phase_offset_ns": schedule["control"]["phase_offset_ns"],
                "interval_ns": schedule["control"]["interval_ns"],
                "requests": len(controls),
                "stop_requested_monotonic_ns": control_stop,
            },
            "sampler": {
                "started": True, "origin_monotonic_ns": sample_origin,
                "interval_ns": schedule["host_sample_interval_ns"],
                "samples": len(samples), "periodic_samples": len(samples) - 1,
                "terminal_sample": True,
                "stop_requested_monotonic_ns": sample_stop,
            },
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
            "uffd_mode": "copy", "snapshot_tag": snapshot["tag"],
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
                "network_mode": "rootless", "port_mappings": ["9222:9222"],
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
