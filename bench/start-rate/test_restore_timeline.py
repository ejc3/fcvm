import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import restore_timeline  # noqa: E402

CLONE_LOG = """\
   100.00 2026-01-01T00:00:10.000000Z  INFO fcvm::firecracker::vm: Firecracker Child.id() returned: Some(4242)
   200.00 2026-01-01T00:00:10.100000Z  INFO fcvm::commands::common: VM resume completed duration_ms=0 total_snapshot_ms=6
   220.00 2026-01-01T00:00:10.120000Z  INFO fcvm::console: [fc-agent] detected restore-epoch: e
   221.00 2026-01-01T00:00:10.121000Z  INFO fcvm::console: [fc-agent] handling restore (epoch=e)
   230.00 2026-01-01T00:00:10.130000Z  INFO fcvm::console: [fc-agent] restore phase=tcp-cleanup epoch=e begin
   240.00 2026-01-01T00:00:10.140000Z  INFO fcvm::console: [fc-agent] restore phase=tcp-cleanup epoch=e complete elapsed_ms=10.0 destroyed=1 already_gone=0
   250.00 2026-01-01T00:00:10.150000Z  INFO fcvm::console: [fc-agent] restore phases complete (epoch=e): exec + egress ready elapsed_ms=30.0
   290.00 2026-01-01T00:00:10.190000Z  INFO fcvm::commands::snapshot: fc-agent acknowledged exact restore generation vm_id=v restore_epoch=e guest_phases={"total_ms":30.0,"tcp_gate_netlink":true,"gpus":0}
   300.00 2026-01-01T00:00:10.200000Z  INFO fcvm::commands::snapshot: fc-agent output connected, exec server ready vm_id=v
   310.00 2026-01-01T00:00:10.210000Z  INFO fcvm::console: [fc-agent] vsock transport reset observed; fast-polling restore-epoch
   400.00 2026-01-01T00:00:10.300000Z  INFO health-monitor: VM healthy, switching to 10s polling
"""


def wall(text):
    return restore_timeline.datetime.datetime.fromisoformat(text).replace(
        tzinfo=restore_timeline.datetime.timezone.utc).timestamp()


class ParseClone(unittest.TestCase):
    def test_marks_phase_record_and_vmm_pid(self):
        with tempfile.TemporaryDirectory() as t:
            path = Path(t) / "c.log"
            path.write_text(CLONE_LOG)
            c = restore_timeline.parse_clone(path)
        self.assertEqual(c["fc_pid"], 4242)
        self.assertEqual(c["t"]["resume"], 200.0)
        self.assertEqual(c["t"]["tcp_begin"], 230.0)
        self.assertEqual(c["t"]["tcp_done"], 240.0)
        self.assertEqual(c["t"]["ready"], 300.0)
        self.assertEqual(c["t"]["reset_wake"], 310.0)
        self.assertEqual(c["phases"]["total_ms"], 30.0)
        self.assertAlmostEqual(c["wall"]["ready"] - c["wall"]["resume"], 0.1, places=6)

    def test_a_line_logged_before_fcvm_received_it_is_placed_when_it_was_logged(self):
        """fc-agent's progress lines reach fcvm after the acknowledgement. Placed at their
        receive times, every step between them would be zero and `phases_done -> ack`
        negative. They are placed where the guest logged them, which gives the steps of the
        same restore logged line by line on the console."""
        with tempfile.TemporaryDirectory() as t:
            direct, deferred = Path(t) / "direct.log", Path(t) / "deferred.log"
            direct.write_text(CLONE_LOG)
            deferred.write_text(CLONE_LOG_DEFERRED)
            want = restore_timeline.parse_clone(direct)
            got = restore_timeline.parse_clone(deferred)
        for key in ("resume", "epoch", "handling", "tcp_begin", "tcp_done", "phases_done", "ack", "ready"):
            # A wall-clock stamp as a float of seconds resolves about 0.2 us.
            self.assertAlmostEqual(got["t"][key], want["t"][key], places=3, msg=key)
            self.assertAlmostEqual(got["wall"][key], want["wall"][key], places=6, msg=key)
        self.assertEqual(restore_timeline.logged_before_receipt_ms(
            "   1.00 2026-01-01T00:00:10.000000Z  INFO fcvm::console: [fc-agent] x"), 0.0)


# The same restore with its progress lines on the output connection: fcvm receives them
# after the acknowledgement, each with the time the guest logged it. One line went to the
# serial console instead, 70 ms after it was logged.
CLONE_LOG_DEFERRED = """\
   100.00 2026-01-01T00:00:10.000000Z  INFO fcvm::firecracker::vm: Firecracker Child.id() returned: Some(4242)
   200.00 2026-01-01T00:00:10.100000Z  INFO fcvm::commands::common: VM resume completed duration_ms=0 total_snapshot_ms=6
   290.00 2026-01-01T00:00:10.190000Z  INFO fcvm::commands::snapshot: fc-agent acknowledged exact restore generation vm_id=v restore_epoch=e guest_phases={"total_ms":30.0,"tcp_gate_netlink":true,"gpus":0}
   300.00 2026-01-01T00:00:10.200000Z  INFO fcvm::commands::snapshot: fc-agent output connected, exec server ready vm_id=v
   301.00 2026-01-01T00:00:10.201000Z  INFO fcvm::agent: [fc-agent] detected restore-epoch: e logged=2026-01-01T00:00:10.120000Z
   301.00 2026-01-01T00:00:10.201000Z  INFO fcvm::agent: [fc-agent] handling restore (epoch=e) logged=2026-01-01T00:00:10.121000Z
   301.00 2026-01-01T00:00:10.201000Z  INFO fcvm::agent: [fc-agent] restore phase=tcp-cleanup epoch=e begin logged=2026-01-01T00:00:10.130000Z
   301.00 2026-01-01T00:00:10.201000Z  INFO fcvm::agent: [fc-agent] restore phase=tcp-cleanup epoch=e complete elapsed_ms=10.0 destroyed=1 already_gone=0 logged=2026-01-01T00:00:10.140000Z
   320.00 2026-01-01T00:00:10.220000Z  INFO fcvm::console: [fc-agent] restore phases complete (epoch=e): exec + egress ready elapsed_ms=30.0 held_ms=70.000
   302.00 2026-01-01T00:00:10.202000Z  INFO fcvm::console: [fc-agent] restore: 5 progress lines sent to the host over vsock
   400.00 2026-01-01T00:00:10.300000Z  INFO health-monitor: VM healthy, switching to 10s polling
"""

MONO0 = 6_000_000_000_000  # CLOCK_MONOTONIC at the anchor, ns
PID = 4242


class Join(unittest.TestCase):
    """One clone: resume at R, ready 35 ms later, and the vCPU's first stage-2 fault 5 ms
    after R (or wherever `first_fault_ms` puts it)."""

    def records(self, d, resume, first_fault_ms=5.0, samples_ms=(15, 45, 2100), start=77, pid=PID):
        t0 = resume + first_fault_ms / 1000
        anchor = {"anchor": True, "mono_ns": MONO0, "wall_ns": int(round((resume - 1.0) * 1e9)),
                  "spread_ns": 300}
        t0_mono = MONO0 + int(round((t0 - (resume - 1.0)) * 1e9))
        # The thread had run 3 ms and waited 0.5 ms before its first fault (the VMM restores
        # the vCPU's state on it); from there it runs 1 ms per ms, waits 1/15 ms per ms and
        # is in the guest two thirds of the time.
        samples = [{"w": t0 + ms / 1000, "pid": pid, "start": start,
                    "run": 3_000_000 + ms * 1_000_000, "wait": 500_000 + ms * 1_000_000 // 15,
                    "gt": (2 + ms * 2 // 3) // 10, "st": 0, "ut": 0, "mrun": 7_000_000 + ms * 10_000}
                   for ms in samples_ms]
        (d / "vcpustat.jsonl").write_text(
            json.dumps(anchor) + "\n" + "".join(json.dumps(s) + "\n" for s in samples))
        # Buckets count from the first fault. 10 ms buckets; bucket 100 is the first 100 ms
        # one (1.0 s to 1.1 s).
        (d / "s2fault.txt").write_text(
            "Attaching 7 probes...\nattached\n"
            f"@t0[{pid}]: {t0_mono}\n@run0[{pid}]: 3000000\n@wait0[{pid}]: 500000\n@guest0[{pid}]: 2000000\n"
            f"@s2[{pid}, 0]: 700\n@s2[{pid}, 1]: 200\n@s2[{pid}, 2]: 50\n@s2[{pid}, 3]: 40\n"
            f"@s2[{pid}, 4]: 1000\n@s2[{pid}, 50]: 9\n@s2[{pid}, 100]: 300\n"
            f"@s2ns[{pid}, 0]: 1400000\n@s2ns[{pid}, 3]: 600000\n@s2ns[{pid}, 4]: 9000000\n"
            f"@ufns[{pid}, 0]: 5000000\n@ufn[{pid}, 0]: 40\n@s2[7, 0]: 9\n@t0[7]: 5\n@execs: 12\n"
            f"@mmio[{pid}, 1]: 300\n@mmio[{pid}, 5]: 1\n@device[1073750016]: 301\n")
        by_pid, anchors = restore_timeline.load_vcpustat(d / "vcpustat.jsonl")
        return by_pid, anchors, restore_timeline.load_buckets(d / "s2fault.txt")

    def join(self, first_fault_ms=5.0, window_ms=35.0, pid=PID, **kw):
        resume = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            by_pid, anchors, maps = self.records(Path(t), resume, first_fault_ms, **kw)
        clone = {"fc_pid": pid, "wall": {"resume": resume, "ready": resume + window_ms / 1000,
                                         "healthy": resume + 0.060}}
        return restore_timeline.join_clone(clone, by_pid, anchors, maps)

    def test_the_buckets_are_shifted_by_the_clones_first_fault(self):
        """RED BEFORE THE FIX: the buckets were counted from the resume stamp, so a vCPU
        whose first fault came 5 ms after it had the 5 ms past ready counted in (half of
        bucket 3: 970 faults, not 950)."""
        row, reasons = self.join(first_fault_ms=5.0)
        self.assertEqual(reasons, [])
        self.assertAlmostEqual(row["window_ms"], 35.0, places=3)
        self.assertAlmostEqual(row["first_fault_after_resume_ms"], 5.0, places=3)
        # The window is the first 30 ms after the first fault: buckets 0, 1 and 2, nothing
        # of bucket 3 and nothing of the 1,000 faults of bucket 4.
        self.assertAlmostEqual(row["stage2_faults"], 700 + 200 + 50, places=1)
        self.assertAlmostEqual(row["stage2_fault_ms"], 1.4, places=3)
        self.assertAlmostEqual(row["us_per_stage2_fault"], 1400 / 950, places=3)
        self.assertAlmostEqual(row["device_aborts"], 300, places=1)
        self.assertAlmostEqual(row["userfault_ms"], 5.0, places=3)
        self.assertAlmostEqual(row["userfaults"], 40, places=3)

    def test_a_first_fault_before_the_resume_stamp_drops_the_part_before_it(self):
        # fcvm stamped the resume 5 ms after the vCPU's first fault: the window is 5 ms to
        # 40 ms after that fault. Half of bucket 0, then buckets 1, 2 and 3.
        row, _ = self.join(first_fault_ms=-5.0)
        self.assertAlmostEqual(row["first_fault_after_resume_ms"], -5.0, places=3)
        self.assertAlmostEqual(row["stage2_faults"], 350 + 200 + 50 + 40, places=1)
        self.assertAlmostEqual(row["stage2_fault_ms"], 0.7 + 0.6, places=3)
        self.assertAlmostEqual(row["userfaults"], 20, places=3)
        # On-CPU time starts at the resume stamp, 5 ms along the line from the first fault.
        self.assertAlmostEqual(row["vcpu_run_ms"], 35.0, places=3)

    def test_on_cpu_time_runs_from_the_first_fault_record_along_the_samples(self):
        """RED BEFORE THE FIX: on-CPU time was the first sample at or after ready minus the
        last at or before resume, or minus zero with no sample before resume: 48 ms here
        (the 3 ms before the first fault and the 15 ms after ready counted in), in a 35 ms
        window."""
        row, reasons = self.join()
        self.assertEqual(reasons, [])
        # From the first fault (5 ms into the window) to ready: 30 ms of 1 ms per ms.
        self.assertAlmostEqual(row["vcpu_run_ms"], 30.0, places=3)
        self.assertAlmostEqual(row["vcpu_wait_ms"], 2.0, places=3)
        self.assertAlmostEqual(row["rest_ms"], 35.0 - 30.0 - 2.0 - 5.0, places=3)
        # Guest time: 2 ms exactly at the first fault, then whole ticks (10 ms at +15 ms,
        # 30 ms at +45 ms): 20 ms on the line at ready.
        self.assertAlmostEqual(row["guest_ms"], 18.0, places=3)
        self.assertAlmostEqual(row["outside_guest_ms"], 12.0, places=3)
        self.assertTrue(row["sampled_in_window"])
        self.assertNotIn("vmm_user_ticks_ms", row)
        # Ready to healthy is 25 ms, and the 2 s after healthy end at the +2100 ms sample.
        self.assertAlmostEqual(row["ready_to_healthy_vcpu_run_ms"], 25.0, places=3)
        self.assertAlmostEqual(row["two_s_after_healthy_vcpu_run_ms"], 2000.0, places=3)
        self.assertAlmostEqual(row["two_s_after_healthy_vmm_run_ms"], 20.0, places=3)

    def test_a_clone_first_sampled_after_ready_is_still_in_the_on_cpu_rows(self):
        """RED BEFORE THE FIX: a VMM whose first sample came after its ready line was
        reported as never seen and left out, and those are the clones with short windows."""
        row, reasons = self.join(samples_ms=(45, 2100))
        self.assertEqual(reasons, [])
        self.assertAlmostEqual(row["vcpu_run_ms"], 30.0, places=3)
        self.assertFalse(row["sampled_in_window"])

    def test_buckets_are_ten_ms_for_a_second_then_a_hundred(self):
        self.assertEqual(restore_timeline.bucket_span(0), (0, 10_000_000))
        self.assertEqual(restore_timeline.bucket_span(99), (990_000_000, 1_000_000_000))
        self.assertEqual(restore_timeline.bucket_span(100), (1_000_000_000, 1_100_000_000))
        self.assertEqual(restore_timeline.bucket_span(109), (1_900_000_000, 2_000_000_000))
        # A 1.05 s window: everything of the first second and half of bucket 100.
        self.assertEqual(restore_timeline.within({0: 4, 99: 6, 100: 300, 101: 8}, 0, 1_050_000_000), 160)
        # The same window starting 995 ms in: half of bucket 99 and half of bucket 100.
        self.assertEqual(restore_timeline.within({0: 4, 99: 6, 100: 300, 101: 8}, 995_000_000, 1_050_000_000), 153)
        # A window that starts before the first fault takes nothing from before it.
        self.assertEqual(restore_timeline.within({0: 4}, -5_000_000, 5_000_000), 2)

    def test_a_clone_is_left_out_of_what_the_records_cannot_give_with_the_reason(self):
        # No stage-2 record at all: no row.
        self.assertEqual(self.join(pid=1), (None, ["no stage-2 fault of its vCPU in the trace"]))
        self.assertEqual(self.join(first_fault_ms=50.0),
                         (None, ["its vCPU's first stage-2 fault is stamped after its ready line"]))
        # In the trace but never sampled: the fault rows, and no on-CPU rows.
        row, reasons = self.join(samples_ms=())
        self.assertEqual(reasons, ["the sampler never saw its VMM (no on-CPU rows)"])
        self.assertAlmostEqual(row["stage2_faults"], 950, places=1)
        self.assertNotIn("vcpu_run_ms", row)
        # Ready after the last sample: the sampler stopped watching the VMM first.
        row, reasons = self.join(window_ms=3000.0)
        self.assertEqual(reasons, ["ready after the sampler's last sample of its VMM (no on-CPU rows)"])
        self.assertIn("stage2_faults", row)
        self.assertNotIn("vcpu_run_ms", row)
        resume = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            by_pid, anchors, maps = self.records(Path(t), resume)
        self.assertEqual(restore_timeline.join_clone({"fc_pid": PID, "wall": {}}, by_pid, anchors, maps)[1],
                         ["no wall-clock stamp on its resume or ready line"])
        clone = {"fc_pid": PID, "wall": {"resume": resume, "ready": resume + 0.035}}
        self.assertEqual(restore_timeline.join_clone(clone, by_pid, anchors, maps, shared_pids={PID}),
                         (None, ["its pid was another clone's VMM in this run, and the tracer's buckets are per pid"]))

    def test_a_reused_pid_is_two_processes_to_the_sampler(self):
        resume = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            self.records(d, resume)
            old = [{"w": resume - 60 + i, "pid": PID, "start": 5, "run": 9_000_000_000, "wait": 0,
                    "gt": 0, "st": 0, "ut": 0, "mrun": 0} for i in range(3)]
            with open(d / "vcpustat.jsonl", "a") as f:
                f.write("".join(json.dumps(s) + "\n" for s in old))
            by_pid, _ = restore_timeline.load_vcpustat(d / "vcpustat.jsonl")
        self.assertEqual(len(by_pid[PID]), 2)
        series = restore_timeline.series_for(by_pid, PID, resume)
        self.assertEqual({s["start"] for s in series}, {77})

    def test_a_time_of_the_tracer_is_placed_by_the_nearest_anchor(self):
        anchors = [{"mono_ns": 1_000, "wall_ns": 5_000_001_000}, {"mono_ns": 9_000_000_000, "wall_ns": 14_000_501_000}]
        # The wall clock was stepped 0.5 ms between the anchors; each time takes its side's.
        self.assertAlmostEqual(restore_timeline.wall_of(anchors, 2_000), 5.000002, places=9)
        self.assertAlmostEqual(restore_timeline.wall_of(anchors, 8_999_999_000), 14.0005, places=9)


class Report(unittest.TestCase):
    def run_main(self, d, argv):
        import contextlib
        import io
        out = io.StringIO()
        old = sys.argv
        sys.argv = ["restore_timeline.py"] + argv
        try:
            with contextlib.redirect_stdout(out):
                restore_timeline.main()
        finally:
            sys.argv = old
        return out.getvalue()

    def test_the_trace_rows_and_what_was_left_out(self):
        resume = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            (d / "run" / "clones").mkdir(parents=True)
            (d / "run" / "clones" / "a.log").write_text(CLONE_LOG)
            (d / "run" / "clones" / "b.log").write_text(CLONE_LOG.replace("Some(4242)", "Some(4343)"))
            (d / "trace").mkdir()
            Join().records(d / "trace", resume)
            text = self.run_main(d, [str(d / "run"), "--trace", str(d / "trace"), "--json", str(d / "rows.json")])
            rows = json.loads((d / "rows.json").read_text())
            self.assertIn("1 of 2 clones joined with s2fault.txt, 1 of them with the sampler's record too", text)
            self.assertIn("left out, no stage-2 fault of its vCPU in the trace: 1", text)
            self.assertIn("first_fault_after_resume_ms", text)
            self.assertNotIn("ticks", text.replace("whole 10 ms ticks", ""))
            self.assertEqual(len(rows), 1)
            # CLONE_LOG's window is 100 ms: the first 95 ms after the first fault.
            self.assertAlmostEqual(rows[0]["stage2_faults"], 700 + 200 + 50 + 40 + 1000, places=1)

            # Two clones with one VMM pid: the tracer's buckets cannot tell them apart.
            (d / "run" / "clones" / "b.log").write_text(CLONE_LOG)
            text = self.run_main(d, [str(d / "run"), "--trace", str(d / "trace")])
            self.assertIn("0 of 2 clones joined", text)
            self.assertIn("its pid was another clone's VMM in this run, and the tracer's buckets are per pid: 2", text)

            # A sampler record without clock anchors cannot place the buckets: refuse.
            lines = (d / "trace" / "vcpustat.jsonl").read_text().splitlines()
            (d / "trace" / "vcpustat.jsonl").write_text("\n".join(lines[1:]) + "\n")
            with self.assertRaises(SystemExit) as stop:
                self.run_main(d, [str(d / "run"), "--trace", str(d / "trace")])
            self.assertIn("no clock anchor", str(stop.exception))


if __name__ == "__main__":
    unittest.main()
