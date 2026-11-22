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
    def test_window_is_split_by_the_host_side_records(self):
        t0 = wall("2026-01-01T00:00:10.100000")
        samples = [
            {"w": t0 - 0.01, "pid": 4242, "run": 1_000_000, "wait": 0, "gt": 0, "st": 0, "ut": 0, "mrun": 0},
            {"w": t0 + 0.05, "pid": 4242, "run": 31_000_000, "wait": 10_000_000, "gt": 2, "st": 1, "ut": 2, "mrun": 0},
            {"w": t0 + 0.13, "pid": 4242, "run": 61_000_000, "wait": 20_000_000, "gt": 4, "st": 2, "ut": 4, "mrun": 0},
        ]
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            (d / "vcpustat.jsonl").write_text("".join(json.dumps(s) + "\n" for s in samples))
            (d / "s2fault.txt").write_text(
                "@s2[4242, 0]: 700\n@s2[4242, 1]: 200\n@s2[4242, 2]: 50\n@s2[4242, 9]: 5\n"
                "@ufns[4242, 0]: 5000000\n@ufn[4242, 0]: 40\n@s2[7, 0]: 9\n@execs: 12\n"
                "@mmio[4242, 1]: 300\n@mmio[4242, 5]: 1\n@device[1073750016]: 301\n")
            vs = restore_timeline.load_vcpustat(d / "vcpustat.jsonl")
            maps = restore_timeline.load_buckets(d / "s2fault.txt")
        # resume: the last sample at or before it; ready: the first at or after it.
        s0 = restore_timeline.at(vs[4242], t0, after=False)
        s1 = restore_timeline.at(vs[4242], t0 + 0.12, after=True)
        self.assertEqual((s0["run"], s1["run"]), (1_000_000, 61_000_000))
        self.assertIsNone(restore_timeline.at(vs[4242], t0 + 1.0, after=True))
        # A 120 ms window covers buckets 0 and 1 (100 ms each) and not buckets 2 and 9.
        row = restore_timeline.join_clone(
            {"fc_pid": 4242, "wall": {"resume": t0, "ready": t0 + 0.12}}, vs, maps)
        self.assertAlmostEqual(row["window_ms"], 120.0, places=3)
        self.assertEqual(row["vcpu_run_ms"], 60.0)
        self.assertEqual(row["vcpu_wait_ms"], 20.0)
        self.assertEqual(row["userfault_ms"], 5.0)
        self.assertEqual(row["userfaults"], 40)
        self.assertEqual(row["stage2_faults"], 900)
        self.assertEqual(row["device_aborts"], 300)
        self.assertAlmostEqual(row["rest_ms"], 35.0, places=3)

    def test_a_clone_the_sampler_never_saw_is_left_out(self):
        self.assertIsNone(restore_timeline.join_clone(
            {"fc_pid": 1, "wall": {"resume": 1.0, "ready": 2.0}}, {}, {}))


if __name__ == "__main__":
    unittest.main()
