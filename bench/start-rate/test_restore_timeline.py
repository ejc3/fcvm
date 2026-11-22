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


class Join(unittest.TestCase):
    def records(self, d, t0, start=77):
        samples = [
            {"w": t0 - 0.01, "pid": 4242, "start": start, "run": 1_000_000, "wait": 0, "gt": 0, "st": 0, "ut": 0, "mrun": 0},
            {"w": t0 + 0.02, "pid": 4242, "start": start, "run": 21_000_000, "wait": 2_000_000, "gt": 1, "st": 1, "ut": 2, "mrun": 0},
            {"w": t0 + 0.05, "pid": 4242, "start": start, "run": 41_000_000, "wait": 4_000_000, "gt": 1, "st": 2, "ut": 2, "mrun": 0},
        ]
        (d / "vcpustat.jsonl").write_text("".join(json.dumps(s) + "\n" for s in samples))
        # 10 ms buckets: 0..3 lie in a 35 ms window (the fourth by half), 4 and 50 do not;
        # bucket 100 is the first 100 ms one (1.0 s to 1.1 s).
        (d / "s2fault.txt").write_text(
            "@s2[4242, 0]: 700\n@s2[4242, 1]: 200\n@s2[4242, 2]: 50\n@s2[4242, 3]: 40\n"
            "@s2[4242, 4]: 1000\n@s2[4242, 50]: 9\n@s2[4242, 100]: 300\n"
            "@s2ns[4242, 0]: 1400000\n@s2ns[4242, 3]: 600000\n@s2ns[4242, 4]: 9000000\n"
            "@ufns[4242, 0]: 5000000\n@ufn[4242, 0]: 40\n@s2[7, 0]: 9\n@execs: 12\n"
            "@mmio[4242, 1]: 300\n@mmio[4242, 5]: 1\n@device[1073750016]: 301\n")
        return (restore_timeline.load_vcpustat(d / "vcpustat.jsonl"),
                restore_timeline.load_buckets(d / "s2fault.txt"))

    def test_a_short_window_counts_only_the_buckets_inside_it(self):
        t0 = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            vs, maps = self.records(Path(t), t0)
        row, reason = restore_timeline.join_clone(
            {"fc_pid": 4242, "wall": {"resume": t0, "ready": t0 + 0.035}}, vs, maps)
        self.assertIsNone(reason)
        self.assertAlmostEqual(row["window_ms"], 35.0, places=3)
        # resume: the last sample at or before it; ready: the first at or after it.
        self.assertEqual(row["vcpu_run_ms"], 40.0)
        self.assertEqual(row["vcpu_wait_ms"], 4.0)
        # Buckets 0, 1 and 2 whole and half of bucket 3; nothing of the 1,000 faults of
        # bucket 4, which a 100 ms bucket would have counted with the window's.
        self.assertAlmostEqual(row["stage2_faults"], 700 + 200 + 50 + 20, places=1)
        self.assertAlmostEqual(row["stage2_fault_ms"], 1.4 + 0.3, places=3)
        self.assertAlmostEqual(row["us_per_stage2_fault"], 1700 / 970, places=3)
        self.assertAlmostEqual(row["device_aborts"], 300, places=1)
        self.assertEqual(row["userfault_ms"], 5.0)
        self.assertEqual(row["userfaults"], 40)
        self.assertEqual(row["guest_ticks_ms"], 10)
        self.assertEqual(row["sys_ticks_ms"], 20)
        self.assertEqual(row["vmm_user_ticks_ms"], 10)

    def test_buckets_are_ten_ms_for_a_second_then_a_hundred(self):
        self.assertEqual(restore_timeline.bucket_span(0), (0, 10_000_000))
        self.assertEqual(restore_timeline.bucket_span(99), (990_000_000, 1_000_000_000))
        self.assertEqual(restore_timeline.bucket_span(100), (1_000_000_000, 1_100_000_000))
        self.assertEqual(restore_timeline.bucket_span(109), (1_900_000_000, 2_000_000_000))
        # A 1.05 s window: everything of the first second and half of bucket 100.
        self.assertEqual(restore_timeline.within({0: 4, 99: 6, 100: 300, 101: 8}, 1_050_000_000), 160)

    def test_a_clone_without_a_usable_record_is_left_out_with_its_reason(self):
        t0 = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            vs, maps = self.records(Path(t), t0)
        clone = lambda pid, ready: {"fc_pid": pid, "wall": {"resume": t0, "ready": ready}}
        self.assertEqual(restore_timeline.join_clone(clone(1, t0 + 1), vs, maps),
                         (None, "the sampler never saw its VMM"))
        # Ready after the last sample: the sampler stopped watching the VMM first.
        self.assertEqual(restore_timeline.join_clone(clone(4242, t0 + 1), vs, maps),
                         (None, "ready after the sampler's last sample of its VMM"))
        self.assertEqual(restore_timeline.join_clone({"fc_pid": 4242, "wall": {}}, vs, maps)[1],
                         "no wall-clock stamp on its resume or ready line")

    def test_a_reused_pid_is_two_processes(self):
        t0 = wall("2026-01-01T00:00:10.100000")
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            self.records(d, t0)
            old = [{"w": t0 - 60 + i, "pid": 4242, "start": 5, "run": 9_000_000_000, "wait": 0,
                    "gt": 0, "st": 0, "ut": 0, "mrun": 0} for i in range(3)]
            with open(d / "vcpustat.jsonl", "a") as f:
                f.write("".join(json.dumps(s) + "\n" for s in old))
            vs = restore_timeline.load_vcpustat(d / "vcpustat.jsonl")
        self.assertEqual(len(vs[4242]), 2)
        series = restore_timeline.series_for(vs, 4242, t0, t0 + 0.035)
        self.assertEqual({s["start"] for s in series}, {77})


if __name__ == "__main__":
    unittest.main()
