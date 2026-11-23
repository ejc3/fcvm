import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import vcpustat  # noqa: E402

# A vCPU thread's stat line: the comm holds a space, and fields 14, 15, 22 and 43 of
# proc(5) are utime, stime, starttime and guest_time.
STAT = ("4243 (fc_vcpu 0) S 4200 4242 4242 0 -1 4194368 1500 0 3 0 "
        "61 17 0 0 20 0 4 0 987654 1000000 250 18446744073709551615 "
        + " ".join(["0"] * 17) + " 44 0 0 0\n")


class ParseStat(unittest.TestCase):
    def test_fields_are_counted_from_the_end_of_the_comm(self):
        s = vcpustat.parse_stat(STAT)
        self.assertEqual(s, {"ppid": 4200, "minflt": 1500, "majflt": 3, "ut": 61, "st": 17,
                             "start": 987654, "gt": 44})

    def test_a_comm_with_a_parenthesis_does_not_shift_the_fields(self):
        self.assertEqual(vcpustat.parse_stat(STAT.replace("(fc_vcpu 0)", "(a) b)"))["ut"], 61)


class ClockAnchor(unittest.TestCase):
    def test_the_tightest_reading_is_kept_with_its_width(self):
        # Three readings: the first interrupted for 5 ms between its calls, the second for
        # 40 us, the third tight. The loop stops at the first tight one.
        mono = iter([1_000, 5_001_000, 6_000_000, 6_040_000, 7_000_000, 7_000_400, 9, 9])
        wall = iter([50_000_000_000, 50_006_000_000, 50_007_000_000, 0])
        a = vcpustat.clock_anchor(mono=lambda: next(mono), wall=lambda: next(wall))
        self.assertEqual(a, {"anchor": True, "mono_ns": 7_000_200, "wall_ns": 50_007_000_000,
                             "spread_ns": 400})

    def test_no_tight_reading_keeps_the_best_of_the_tries(self):
        mono = iter([0, 900_000, 1_000_000, 1_300_000, 2_000_000, 2_800_000])
        wall = iter([10, 20, 30])
        a = vcpustat.clock_anchor(tries=3, mono=lambda: next(mono), wall=lambda: next(wall))
        self.assertEqual((a["mono_ns"], a["wall_ns"], a["spread_ns"]), (1_150_000, 20, 300_000))


class Discover(unittest.TestCase):
    def proc(self, d, pid, comm, threads):
        (d / str(pid) / "task").mkdir(parents=True)
        (d / str(pid) / "comm").write_text(comm + "\n")
        (d / str(pid) / "stat").write_text(STAT.replace("4243 (fc_vcpu 0)", f"{pid} ({comm})"))
        (d / str(pid) / "schedstat").write_text("5 6 7\n")
        for tid, name in threads:
            t = d / str(pid) / "task" / str(tid)
            t.mkdir()
            (t / "comm").write_text(name + "\n")
            (t / "stat").write_text(STAT)
            (t / "schedstat").write_text("100 20 3\n")

    def test_a_slow_child_and_a_reused_pid_are_still_found(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            self.proc(d, 10, "fcvm", [(10, "fcvm"), (11, "tokio-runtime-w")])   # an fcvm process
            self.proc(d, 20, "tokio-runtime-w", [(20, "tokio-runtime-w")])       # its child, before exec
            self.proc(d, 30, "pasta", [(30, "pasta")])
            s = vcpustat.Sampler(12.0, proc=str(d))
            s.discover(0.0)
            self.assertEqual(s.young, {})
            self.assertEqual(s.seen, {10, 30})  # 20 is looked at again
            # Ten seconds on, the child execs firecracker; pid 30 is now another firecracker.
            import shutil
            for pid in (20, 30):
                shutil.rmtree(d / str(pid))
            self.proc(d, 20, "firecracker-def", [(20, "firecracker-def"), (21, "fc_vcpu 0")])
            s.discover(10.0)
            self.assertEqual(s.young, {20: (10.0, 21, 987654)})
            s.discover(10.1)  # pid 30 is gone from /proc and forgotten
            self.proc(d, 30, "firecracker-def", [(30, "firecracker-def"), (31, "fc_vcpu 0")])
            s.discover(10.2)
            self.assertEqual(sorted(s.young), [20, 30])
            out = io.StringIO()
            # The wall clock is read once per process, as each is read: the second process
            # of a pass is stamped later than the first.
            clock = iter([1000.000, 1000.030])
            s.sample(out, 10.3, wall_clock=lambda: next(clock))
            rows = [json.loads(line) for line in out.getvalue().splitlines()]
            self.assertEqual([(r["pid"], r["start"], r["run"], r["wait"], r["mrun"], r["w"]) for r in rows],
                             [(20, 987654, 100, 20, 5, 1000.000), (30, 987654, 100, 20, 5, 1000.030)])


if __name__ == "__main__":
    unittest.main()
