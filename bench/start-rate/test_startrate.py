import argparse
import contextlib
import io
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import pidstat_top  # noqa: E402
import startrate  # noqa: E402


class Summary(unittest.TestCase):
    def write_run(self, d, healthy_at):
        rows = []
        for i, h in enumerate(healthy_at):
            rows.append({
                "i": i, "name": f"c{i}", "spawn_at_ms": 0.0, "popen_ms": 1.0,
                "healthy_at_ms": h, "spawn_to_healthy_ms": h,
                "marks_ms": {"Cloning VM from serve PID": 5.0, "exec server ready": h - 10,
                             startrate.HEALTHY_MARKER: h},
                "error": None, "exit": None,
            })
        (d / "samples.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
        (d / "host.json").write_text(json.dumps({"instance_type": "t", "config": {}}))

    def test_best_window_counts_clones_inside_one_second(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            # 3 within [1000, 2000], then 2 far apart
            self.write_run(d, [1000, 1500, 2000, 4000, 9000])
            s = startrate.summarize(d)
            self.assertEqual(s["best_1s_window"], 3)
            self.assertEqual(s["healthy"], 5)
            self.assertEqual(s["healthy_per_second"][:5], [0, 2, 1, 0, 1])
            self.assertEqual(s["stages_ms"]["agent->healthy"]["p50"], 10.0)
            self.assertEqual(s["stages_ms"]["pre-log"]["p50"], 5.0)


class RateWithAlive(unittest.TestCase):
    @staticmethod
    def rows(healthy_at, exits=None):
        exits = exits or {}
        return [{"healthy_at_ms": h, "exit_at_ms": exits.get(i)} for i, h in enumerate(healthy_at)]

    def test_counts_from_the_floor_to_the_last_healthy(self):
        # 1000 healthy by 2 s, then 500 more over the next 2 s.
        hs = [2.0 * i for i in range(1, 1001)] + [2000 + 4.0 * i for i in range(1, 501)]
        r = startrate.rate_with_alive(self.rows(hs))
        self.assertEqual((r["window_s"], r["healthy_in_window"], r["rate_per_s"]), (2.0, 500, 250.0))
        self.assertEqual(r["min_alive_in_window"], 1000)

    def test_no_rate_under_the_floor_or_in_a_short_window(self):
        r = startrate.rate_with_alive(self.rows([float(i) for i in range(999)]))
        self.assertIsNone(r["rate_per_s"])
        self.assertIn("at most 999 clones were alive at once, under 1000", r["why"])
        r = startrate.rate_with_alive(self.rows([float(i) for i in range(1000)] + [1500.0]))
        self.assertEqual((r["rate_per_s"], r["healthy_in_window"], r["window_s"]), (None, 1, 0.501))
        self.assertIn("under 1.0 s", r["why"])
        self.assertIn("no clone turned healthy", startrate.rate_with_alive([])["why"])

    def test_a_clone_that_exits_before_the_floor_is_not_alive(self):
        # RED BEFORE THE FIX: 1000 healthy counted as 1000 alive whatever had exited.
        # Clone 998 exits at 500 ms, so the 1000th healthy clone makes 999 alive and the
        # 1001st makes 1000: the window starts there.
        hs = [float(i) for i in range(1000)] + [3000.0, 4000.0, 5000.0]
        r = startrate.rate_with_alive(self.rows(hs, {998: 500.0}))
        self.assertEqual((r["window_s"], r["healthy_in_window"], r["rate_per_s"]), (2.0, 2, 1.0))
        # With nothing after it, the 1000th healthy clone's predecessor having exited leaves no rate.
        r = startrate.rate_with_alive(self.rows([float(i) for i in range(1000)], {998: 500.0}))
        self.assertIsNone(r["rate_per_s"])
        self.assertIn("at most 999 clones were alive at once", r["why"])

    def test_no_rate_when_the_alive_count_falls_inside_the_window(self):
        hs = [float(i) for i in range(1000)] + [3000.0]
        r = startrate.rate_with_alive(self.rows(hs, {5: 2000.0}))
        self.assertIsNone(r["rate_per_s"])
        self.assertEqual(r["min_alive_in_window"], 999)
        self.assertIn("the alive count fell to 999, under 1000, inside the window", r["why"])
        # An exit after the last clone is healthy (the run's own teardown) is outside the window.
        r = startrate.rate_with_alive(self.rows(hs, {i: 9000.0 for i in range(1001)}))
        self.assertEqual((r["rate_per_s"], r["min_alive_in_window"]), (0.5, 1000))

    def test_a_summary_carries_it_and_names_the_mode(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            Summary().write_run(d, [float(i) for i in range(1, 2002)])
            s = startrate.summarize(d)
            self.assertEqual(s["rate_with_alive"]["rate_per_s"], 1000.0)
            self.assertIsNone(s["pacing"])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                startrate.print_summary(s)
            self.assertIn("with 1000+ alive (closed loop): 1001 healthy in 1.001 s = 1000.0/s, at least 1000 alive throughout", out.getvalue())
            s["pacing"] = startrate.pacing_summary([{"spawn_at_ms": 0.0, "due_at_ms": 0.0}], 800.0)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                startrate.print_summary(s)
            self.assertIn("with 1000+ alive (paced at 800.0/s):", out.getvalue())


class SteppingClock:
    """A clock that moves 1 ms each time it is read, and when a test moves it."""

    def __init__(self):
        self.t = 1_000_000_000
        self.lock = threading.Lock()

    def __call__(self):
        with self.lock:
            self.t += 1_000_000
            return self.t

    def advance(self, ms):
        with self.lock:
            self.t += ms * 1_000_000


class Paced(unittest.TestCase):
    def test_schedule_is_the_first_start_plus_i_over_rate(self):
        self.assertEqual(startrate.start_due_ns(5, 0, 1000.0), 5)
        self.assertEqual(startrate.start_due_ns(5, 3, 1000.0), 5 + 3_000_000)
        self.assertEqual(startrate.start_due_ns(0, 1, 0.5), 2_000_000_000)

    def test_no_start_is_issued_before_it_is_due(self):
        clock = SteppingClock()
        p = startrate.Progress(count=6, concurrency=6, rate=100.0, clock=clock)
        issued = []
        for _ in range(6):
            i = p.claim()
            issued.append((i, clock.t))
        self.assertEqual([i for i, _ in issued], list(range(6)))
        for i, at in issued:
            self.assertEqual(p.due_ns[i], p.t_first + i * 10_000_000)
            self.assertGreaterEqual(at, p.due_ns[i], f"start {i} was issued before it was due")
        self.assertIsNone(p.claim())

    def test_a_start_the_cap_holds_back_is_late_and_moves_no_other(self):
        clock = SteppingClock()
        p = startrate.Progress(count=3, concurrency=1, rate=1000.0, clock=clock)
        first = startrate.Clone(p.claim(), "c0")
        got = []
        th = threading.Thread(target=lambda: got.append((p.claim(), clock.t)))
        th.start()
        time.sleep(0.05)
        self.assertEqual(got, [], "a second start was issued over the cap")
        clock.advance(50)
        p.complete(first, t=clock.t)
        th.join(timeout=5)
        self.assertEqual(got[0][0], 1)
        # Due 1 ms after the first, issued at least 50 ms after it: late, and recorded as due then.
        self.assertEqual(p.due_ns[1], p.t_first + 1_000_000)
        self.assertGreaterEqual(got[0][1] - p.due_ns[1], 49_000_000)
        p.complete(startrate.Clone(1, "c1"), t=clock.t)
        self.assertEqual(p.claim(), 2)
        self.assertEqual(p.due_ns[2], p.t_first + 2_000_000, "the schedule moved with the late start")

    def test_a_closed_loop_run_records_no_schedule(self):
        p = startrate.Progress(count=2, concurrency=2)
        self.assertEqual([p.claim(), p.claim()], [0, 1])
        self.assertEqual(p.due_ns, {})

    def test_stop_ends_a_launcher_waiting_for_its_time(self):
        p = startrate.Progress(count=2, concurrency=2, rate=0.01)
        self.assertEqual(p.claim(), 0)
        got = []
        th = threading.Thread(target=lambda: got.append(p.claim()))
        th.start()
        time.sleep(0.05)
        p.stop()
        th.join(timeout=5)
        self.assertEqual(got, [None])

    def test_summary_counts_late_starts_and_starts_never_issued(self):
        rows = [
            {"spawn_at_ms": 0.0, "due_at_ms": 0.0},
            {"spawn_at_ms": 2.5, "due_at_ms": 2.0},     # 0.5 ms after due
            {"spawn_at_ms": 9.5, "due_at_ms": 4.0},     # 5.5 ms: late
            {"spawn_at_ms": 60.0, "due_at_ms": 6.0},    # 54 ms: late
            {"spawn_at_ms": None, "due_at_ms": None},   # the run stopped before it
        ]
        s = startrate.pacing_summary(rows, 500.0)
        self.assertEqual((s["offered_rate_per_s"], s["issued"], s["not_issued"]), (500.0, 4, 1))
        self.assertEqual((s["late_threshold_ms"], s["late_starts"]), (5.0, 2))
        self.assertEqual(s["late_ms"]["max"], 54.0)
        self.assertEqual(s["issued_rate_per_s"], 50.0)
        # At 100 a second one interval is 10 ms, so 5.5 ms is not late.
        self.assertEqual(startrate.pacing_summary(rows, 100.0)["late_starts"], 1)
        self.assertIsNone(startrate.pacing_summary(rows, None))

    def test_rate_must_be_a_number_above_zero(self):
        parser = startrate.build_parser()
        base = ["run", "--fcvm", "f", "--prep", "p", "--out", "o", "--count", "1", "--concurrency", "1"]
        self.assertIsNone(parser.parse_args(base).rate)
        self.assertEqual(parser.parse_args(base + ["--rate", "250"]).rate, 250.0)
        for bad in ("0", "-1", "nan", "inf", "fast"):
            with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                parser.parse_args(base + ["--rate", bad])


class FailClosed(unittest.TestCase):
    def fake_fcvm(self, d, body):
        p = d / "fcvm"
        p.write_text("#!/bin/sh\n" + body + "\n")
        p.chmod(p.stat().st_mode | stat.S_IEXEC)
        return str(p)

    def test_ls_failure_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "exit 3")
            with self.assertRaises(SystemExit) as e:
                startrate.list_vms(f, False)
            self.assertIn("BLOCKED", str(e.exception))

    def test_ls_garbage_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "echo not-json")
            with self.assertRaises(SystemExit):
                startrate.list_vms(f, False)

    def test_ls_object_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "echo '{}'")
            with self.assertRaises(SystemExit):
                startrate.list_vms(f, False)



class FakeRlimit:
    RLIMIT_NOFILE = 7

    def __init__(self, soft, hard):
        self.soft, self.hard = soft, hard

    def getrlimit(self, which):
        return (self.soft, self.hard)

    def setrlimit(self, which, limits):
        self.soft = min(limits[0], self.hard)


class FdLimit(unittest.TestCase):
    def test_raises_sudos_default_for_a_thousand_clones(self):
        r = FakeRlimit(1024, 1048576)
        self.assertGreaterEqual(startrate.ensure_fd_limit(1000, r), 2256)

    def test_blocks_when_the_hard_limit_is_too_low(self):
        with self.assertRaises(SystemExit) as e:
            startrate.ensure_fd_limit(1000, FakeRlimit(1024, 1500))
        self.assertIn("BLOCKED", str(e.exception))


def new_clone(i=0):
    return startrate.Clone(i, f"c{i}")


class OneShotCompletion(unittest.TestCase):
    def test_healthy_after_timeout_is_ignored(self):
        p = startrate.Progress(count=1, concurrency=1)
        self.assertEqual(p.claim(), 0)
        c = new_clone()
        self.assertTrue(p.complete(c, err="not healthy within 1s"))
        self.assertFalse(p.complete(c, t=123))
        self.assertEqual(p.counts(), (0, 1))
        self.assertEqual(p.pending, 0)
        self.assertIsNone(c.t_healthy)

    def test_timeout_after_healthy_is_ignored(self):
        p = startrate.Progress(count=1, concurrency=1)
        p.claim()
        c = new_clone()
        self.assertTrue(p.complete(c, t=123))
        self.assertFalse(p.complete(c, err="exited 1 before healthy"))
        self.assertEqual(p.counts(), (1, 0))
        self.assertEqual(p.pending, 0)
        self.assertIsNone(c.error)

    def test_concurrent_completions_count_once(self):
        p = startrate.Progress(count=1, concurrency=1)
        p.claim()
        c = new_clone()
        wins = []
        threads = [
            threading.Thread(target=lambda k=k: wins.append(p.complete(c, t=k) if k % 2 else p.complete(c, err="e")))
            for k in range(16)
        ]
        for th in threads:
            th.start()
        for th in threads:
            th.join()
        self.assertEqual(sum(wins), 1)
        self.assertEqual(sum(p.counts()), 1)
        self.assertEqual(p.pending, 0)


class StopLaunching(unittest.TestCase):
    def test_stop_wakes_a_waiting_launcher_and_ends_it(self):
        p = startrate.Progress(count=5, concurrency=1)
        self.assertEqual(p.claim(), 0)  # the only slot is taken
        got = []
        th = threading.Thread(target=lambda: got.append(p.claim()))
        th.start()
        time.sleep(0.1)
        p.stop()
        th.join(timeout=3)
        self.assertFalse(th.is_alive())
        self.assertEqual(got, [None])

    def test_no_claim_after_stop_even_with_free_slots(self):
        p = startrate.Progress(count=5, concurrency=4)
        p.stop()
        self.assertIsNone(p.claim())
        self.assertEqual(p.pending, 0)


class Arguments(unittest.TestCase):
    def parse(self, *extra):
        base = ["run", "--fcvm", "f", "--prep", "p", "--out", "o", "--count", "1"]
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            return startrate.build_parser().parse_args(base + list(extra))

    def test_zero_concurrency_is_refused(self):
        with self.assertRaises(SystemExit):
            self.parse("--concurrency", "0")

    def test_zero_launchers_is_refused(self):
        with self.assertRaises(SystemExit):
            self.parse("--concurrency", "1", "--launchers", "0")

    def test_negative_is_refused_and_one_is_accepted(self):
        with self.assertRaises(SystemExit):
            self.parse("--concurrency", "-3")
        a = self.parse("--concurrency", "1", "--launchers", "1")
        self.assertEqual((a.concurrency, a.launchers), (1, 1))


class Midnight(unittest.TestCase):
    def test_window_crossing_midnight_keeps_its_samples(self):
        base = time.mktime((2026, 11, 22, 23, 59, 50, 0, 0, -1))
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            (d / "phases.json").write_text(json.dumps({
                "t0_wall": base, "all_done_wall": base + 20, "hold_end_wall": base + 40,
            }))
            (d / "pidstat.log").write_text(
                "23:59:52 0 100 10.0 5.0 0.0 0.0 50.0 1 fcvm\n"
                "00:00:04 0 100 10.0 5.0 0.0 0.0 50.0 1 fcvm\n"
                "12:00:00 0 100 10.0 5.0 0.0 0.0 90.0 1 other\n"
            )
            out = io.StringIO()
            with mock.patch.object(sys, "argv", ["pidstat_top.py", str(d), "5", "start"]):
                with contextlib.redirect_stdout(out):
                    pidstat_top.main()
        self.assertIn("fcvm", out.getvalue())
        self.assertNotIn("other", out.getvalue())


class FakeBase(unittest.TestCase):
    """A fake fcvm and pidstat on PATH: real processes, no VMs."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.d = Path(self.tmp.name)
        self.state = self.d / "state"
        self.state.mkdir()
        bindir = self.d / "bin"
        bindir.mkdir()
        fcvm = bindir / "fcvm"
        fcvm.write_text(
            "#!/bin/sh\n"
            'case "$1" in\n'
            "ls)\n"
            '  if [ -n "$FAIL_SECOND_LS" ]; then\n'
            '    [ -e "$STATE/ls1" ] && exit 3\n'
            '    touch "$STATE/ls1"\n'
            "  fi\n"
            "  echo '[]';;\n"
            "snapshot)\n"
            '  echo $$ >> "$STATE/pids"\n'
            '  echo "VM healthy, switching to polling"\n'
            "  exec sleep 300;;\n"
            "esac\n"
        )
        pidstat = bindir / "pidstat"
        pidstat.write_text('#!/bin/sh\necho $$ >> "$STATE/pids"\nexec sleep 300\n')
        for f in (fcvm, pidstat):
            f.chmod(0o755)
        self.fcvm = str(fcvm)
        env = {"STATE": str(self.state), "PATH": f"{bindir}:{os.environ['PATH']}"}
        p = mock.patch.dict(os.environ, env)
        p.start()
        self.addCleanup(p.stop)
        h = mock.patch.object(startrate, "host_facts", lambda repo: {})
        h.start()
        self.addCleanup(h.stop)
        self.addCleanup(self.kill_leftovers)
        self.prep = self.d / "prep.json"
        self.prep.write_text(json.dumps({
            "serve_pid": None, "network": "rootless", "cpu": 1, "mem_mib": 256, "image": "i",
            "snapshot": "s", "sudo": False,
        }))

    def pids(self):
        f = self.state / "pids"
        return [int(x) for x in f.read_text().split()] if f.exists() else []

    def kill_leftovers(self):
        for pid in self.pids():
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)

    def alive(self, pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        # A zombie still answers kill(0); count it as gone.
        try:
            return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
        except OSError:
            return False

    def args(self, out, **kw):
        base = dict(
            fcvm=self.fcvm, prep=str(self.prep), out=str(out), count=1, concurrency=1, launchers=1,
            timeout=30.0, rust_log="fcvm=info", pidstat_interval=2, fail_fast=False, hold=0.0, rate=None,
        )
        return argparse.Namespace(**{**base, **kw})



class FakeFcvm(FakeBase):
    """cmd_run against the fake fcvm."""

    def test_failing_final_ls_still_reaps_every_child(self):
        os.environ["FAIL_SECOND_LS"] = "1"
        with self.assertRaises(SystemExit) as e:
            startrate.cmd_run(self.args(self.d / "run"))
        self.assertIn("BLOCKED", str(e.exception))
        pids = self.pids()
        self.assertEqual(len(pids), 2)  # one clone, one pidstat
        deadline = time.time() + 5
        while time.time() < deadline and any(self.alive(p) for p in pids):
            time.sleep(0.05)
        self.assertEqual([p for p in pids if self.alive(p)], [])

    def test_a_paced_run_issues_every_start_on_its_schedule(self):
        # 6 starts at 20 a second: the last is due 250 ms after the first, and none is early.
        out = self.d / "paced"
        # The fake fcvm lists no VM, so the run ends on the log and state disagreeing, after
        # it has written its records.
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            startrate.cmd_run(self.args(out, count=6, concurrency=6, launchers=3, rate=20.0))
        rows = [json.loads(ln) for ln in (out / "samples.jsonl").read_text().splitlines()]
        self.assertEqual(len(rows), 6)
        first = rows[0]["due_at_ms"]
        for r in rows:
            self.assertAlmostEqual(r["due_at_ms"] - first, r["i"] * 50.0, places=3)
            self.assertGreaterEqual(r["spawn_at_ms"], r["due_at_ms"])
            # Each clone's exit is stamped when its fcvm closes its output, here at the run's teardown.
            self.assertIsNotNone(r["exit_at_ms"], "a clone the run stopped has no exit time")
            self.assertGreaterEqual(r["exit_at_ms"], r["healthy_at_ms"])
        s = json.loads((out / "summary.json").read_text())
        self.assertEqual((s["healthy"], s["pacing"]["issued"], s["pacing"]["not_issued"]), (6, 6, 0))
        self.assertEqual(s["pacing"]["offered_rate_per_s"], 20.0)
        self.assertEqual(s["config"]["rate_per_s"], 20.0)
        self.assertGreaterEqual(s["span_s"], 0.25)

    def test_interrupt_reaps_every_child(self):
        # A clone that is never healthy keeps the run waiting; SIGTERM must stop everything.
        (self.d / "bin" / "fcvm").write_text(
            '#!/bin/sh\ncase "$1" in\nls) echo "[]";;\n'
            'snapshot) echo $$ >> "$STATE/pids"; exec sleep 300;;\nesac\n'
        )
        threading.Timer(1.5, os.kill, (os.getpid(), signal.SIGTERM)).start()
        with self.assertRaises(KeyboardInterrupt):
            startrate.cmd_run(self.args(self.d / "run"))
        pids = self.pids()
        self.assertEqual(len(pids), 2)
        deadline = time.time() + 5
        while time.time() < deadline and any(self.alive(p) for p in pids):
            time.sleep(0.05)
        self.assertEqual([p for p in pids if self.alive(p)], [])

    def test_run_refuses_an_existing_output_directory(self):
        out = self.d / "run"
        out.mkdir()
        (out / "summary.json").write_text("old")
        with self.assertRaises(SystemExit) as e:
            startrate.cmd_run(self.args(out))
        self.assertIn("already exists", str(e.exception))
        self.assertEqual((out / "summary.json").read_text(), "old")
        self.assertFalse((out / "host.json").exists())
        self.assertEqual(self.pids(), [])

    def test_prep_refuses_an_existing_output_directory(self):
        out = self.d / "prepdir"
        out.mkdir()
        a = argparse.Namespace(
            fcvm=self.fcvm, out=str(out), network="rootless", uffd=False, cpu=1, mem=256, image="i",
            ipv6_prefix=None, sudo=False, max_clones=8,
        )
        with self.assertRaises(SystemExit) as e:
            startrate.cmd_prep(a)
        self.assertIn("already exists", str(e.exception))
        self.assertEqual(list(out.iterdir()), [])



PREP_FCVM = """#!/bin/sh
cd "$STATE"
case "$1" in
podman)
  echo $$ >> pids
  while [ $# -gt 0 ]; do [ "$1" = --name ] && echo "$2" > name; shift; done
  [ "$FAIL_AT" = baseline ] && exit 4
  exec sleep 300;;
ls)
  case "$FAIL_AT" in
    health) exit 3;;
    timeout) echo '[]';;
    *) echo "[{\\"name\\":\\"$(cat name)\\",\\"health_status\\":\\"healthy\\",\\"pid\\":4242}]";;
  esac;;
snapshot)
  if [ "$2" = create ]; then [ "$FAIL_AT" = snapshot ] && exit 5; exit 0; fi
  echo $$ >> pids
  [ "$FAIL_AT" = serve ] && { echo boom; exit 1; }
  echo "Serve PID: 4243"
  exec sleep 300;;
esac
"""


class PrepOwnsItsChildren(FakeBase):
    def run_prep(self, fail_at):
        (self.d / "bin" / "fcvm").write_text(PREP_FCVM)
        os.environ["FAIL_AT"] = fail_at
        out = self.d / "prepdir"
        a = argparse.Namespace(
            fcvm=self.fcvm, out=str(out), network="rootless", uffd=True, cpu=1, mem=256, image="i",
            ipv6_prefix=None, sudo=False, max_clones=8,
        )
        with mock.patch.object(startrate, "BASELINE_TIMEOUT_S", 1, create=True):
            try:
                startrate.cmd_prep(a)
            except (SystemExit, subprocess.CalledProcessError):
                pass
        return out

    def wait_dead(self, pids):
        deadline = time.time() + 5
        while time.time() < deadline and any(self.alive(p) for p in pids):
            time.sleep(0.05)
        return [p for p in pids if self.alive(p)]

    def failure_leaves_nothing(self, fail_at, started):
        out = self.run_prep(fail_at)
        pids = self.pids()
        self.assertEqual(len(pids), started)
        self.assertEqual(self.wait_dead(pids), [])
        self.assertFalse((out / "prep.json").exists())

    def test_health_poll_failure_leaves_no_child(self):
        self.failure_leaves_nothing("health", 1)

    def test_baseline_timeout_leaves_no_child(self):
        self.failure_leaves_nothing("timeout", 1)

    def test_snapshot_create_failure_leaves_no_child(self):
        self.failure_leaves_nothing("snapshot", 1)

    def test_serve_start_failure_leaves_no_child(self):
        self.failure_leaves_nothing("serve", 2)

    def test_interrupt_during_prep_leaves_no_child(self):
        (self.d / "bin" / "fcvm").write_text(PREP_FCVM)
        os.environ["FAIL_AT"] = "timeout"
        a = argparse.Namespace(
            fcvm=self.fcvm, out=str(self.d / "prepdir"), network="rootless", uffd=False, cpu=1, mem=256,
            image="i", ipv6_prefix=None, sudo=False, max_clones=8,
        )
        threading.Timer(1.0, os.kill, (os.getpid(), signal.SIGTERM)).start()
        with self.assertRaises(KeyboardInterrupt):
            startrate.cmd_prep(a)
        self.assertEqual(self.wait_dead(self.pids()), [])

    def test_success_keeps_baseline_and_serve_and_writes_prep_json(self):
        out = self.run_prep("")
        pids = self.pids()
        self.assertEqual(len(pids), 2)
        self.assertTrue(all(self.alive(p) for p in pids))
        rec = json.loads((out / "prep.json").read_text())
        self.assertEqual((rec["baseline_pid"], rec["serve_pid"]), (4242, 4243))


class StoreQueueDepths(unittest.TestCase):
    def tree(self, root, devices):
        for path, value in devices.items():
            p = Path(root) / path / "queue"
            p.mkdir(parents=True)
            (p / "nr_requests").write_text(f"{value}\n")

    def test_a_plain_device_reports_its_own_queue(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {"loop3": 128})
            self.assertEqual(startrate.store_queue_depths("loop3", Path(root)), {"loop3": 128})

    def test_an_md_device_reports_its_members(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {"md127": 128, "nvme1n1": 127, "nvme2n1": 127})
            slaves = Path(root) / "md127" / "slaves"
            slaves.mkdir()
            for m in ("nvme1n1", "nvme2n1"):
                (slaves / m).symlink_to(Path(root) / m)
            self.assertEqual(
                startrate.store_queue_depths("md127", Path(root)),
                {"nvme1n1": 127, "nvme2n1": 127},
            )

    def test_a_missing_device_reports_none(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertIsNone(startrate.store_queue_depths("nvme9n1", Path(root)))
            self.assertIsNone(startrate.store_queue_depths("", Path(root)))

    def partition(self, root, disk, part):
        (Path(root) / disk / part).mkdir(parents=True)
        (Path(root) / part).symlink_to(Path(root) / disk / part)

    def test_a_partition_reports_its_disk(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {"nvme0n1": 255})
            self.partition(root, "nvme0n1", "nvme0n1p1")
            self.assertEqual(startrate.store_queue_depths("nvme0n1p1", Path(root)), {"nvme0n1": 255})

    def test_an_md_device_over_partitions_reports_their_disks(self):
        with tempfile.TemporaryDirectory() as root:
            self.tree(root, {"md127": 128, "nvme1n1": 127, "nvme2n1": 127})
            slaves = Path(root) / "md127" / "slaves"
            slaves.mkdir()
            for disk in ("nvme1n1", "nvme2n1"):
                self.partition(root, disk, disk + "p1")
                (slaves / (disk + "p1")).symlink_to(Path(root) / disk / (disk + "p1"))
            self.assertEqual(
                startrate.store_queue_depths("md127", Path(root)),
                {"nvme1n1": 127, "nvme2n1": 127},
            )


class ChurnVerdict(unittest.TestCase):
    """Each honesty condition of a churn window, on records made by hand."""

    CFG = {"alive": 20, "lifetime_s": 2.0, "concurrency": 4, "clone_timeout_s": 30.0,
           "window_start_s": 20.0, "window_end_s": 80.0}
    RUN = {"warm": True, "warm_detail": "warm", "measured": True, "state_steady": 20, "state_healthy": 20,
           "left": {"leased": 0, "resetting": 0, "listed": 0}, "polls_failed": 0}
    POOL = ["pool counters cover the window", "free slots flat", "no namespace built or destroyed",
            "slots cover the resets"]

    def records(self, rate=10, lifetime=2.0, end=90.0, slots=100, alive=20):
        """A steady run: a clone every 1/rate s, healthy 0.1 s after its spawn, stopped a
        lifetime later, gone 0.05 s after that; every namespace recycled; no builds. The
        pool is asked once a second from the run's start, each answer 10 ms later."""
        clones = []
        for i in range(int(end * rate)):
            spawn = i / rate
            healthy = spawn + 0.1
            clones.append({"spawn_s": spawn, "healthy_s": healthy, "stop_s": healthy + lifetime,
                           "exit_s": healthy + lifetime + 0.05, "generation": 2 + i // slots, "error": None})
        samples = [(t / 2, 1, alive) for t in range(int(end * 2))]
        pool = []
        for t in range(int(end)):
            hist = [0] * 24
            hist[14] = t * rate  # resets of 16 to 33 ms
            pool.append((float(t), t + 0.01, {
                "building": 0, "ready": slots - alive - 1, "leased": alive, "resetting": 1, "destroying": 0,
                "builds": slots, "destroys": 0, "grants": t * rate, "recycled_grants": t * rate,
                "misses": 0, "resets": t * rate, "reset_us_total": t * rate * 20000, "reset_us_log2": hist,
            }))
        return clones, samples, pool

    def failed(self, verdict):
        return [c["name"] for c in verdict["conditions"] if not c["ok"]]

    def test_a_steady_window_passes_and_reports_its_rates(self):
        v = startrate.churn_verdict(self.CFG, *self.records(), self.RUN)
        self.assertEqual(self.failed(v), [])
        self.assertTrue(v["ok"])
        o = v["observed"]
        self.assertEqual((o["starts"], o["stops"]), (600, 600))
        self.assertEqual((o["starts_per_s"], o["stops_per_s"], o["recycled_fraction"]), (10.0, 10.0, 1.0))
        self.assertEqual((o["alive_target"], o["alive_mean"], o["in_flight_mean"]), (20, 20.0, 1.0))
        self.assertEqual(o["spawn_to_healthy_ms"]["p50"], 100.0)
        self.assertEqual({k: v["pool"][k] for k in ("slots", "builds", "destroys", "reset_p99_s")},
                         {"slots": 100, "builds": 0, "destroys": 0, "reset_p99_s": 0.032768})
        self.assertEqual(v["failures"], {"window": 0, "still_starting_at_end": 0, "warmup": 0, "total": 0})

    def test_the_figures_are_called_sustained_only_when_every_condition_holds(self):
        v = startrate.churn_verdict(self.CFG, *self.records(), self.RUN)
        self.assertEqual(v["sustained"], v["observed"])
        v = startrate.churn_verdict(dict(self.CFG, window_end_s=70.0), *self.records(), self.RUN)
        self.assertFalse(v["ok"])
        self.assertIsNone(v["sustained"])
        self.assertEqual(v["observed"]["starts_per_s"], 10.0)
        self.assertNotIn("starts_per_s", v)

    def test_a_window_under_sixty_seconds_fails(self):
        cfg = dict(self.CFG, window_end_s=70.0)
        v = startrate.churn_verdict(cfg, *self.records())
        self.assertEqual(self.failed(v), ["window"])
        self.assertFalse(v["ok"])

    def test_a_window_under_ten_lifetimes_fails(self):
        cfg = dict(self.CFG, lifetime_s=7.0)
        v = startrate.churn_verdict(cfg, *self.records(lifetime=7.0, alive=70, slots=200))
        self.assertIn("window", self.failed(v))

    def test_more_starts_than_stops_fails(self):
        clones, samples, pool = self.records()
        for c in clones[::5]:
            c["stop_s"] = c["exit_s"] = None  # a fifth of the clones never stop
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertIn("starts equal stops", self.failed(v))

    def test_more_stops_than_starts_fails(self):
        # A backlog of stops from before the window: 100 clones healthy at 19 s that all
        # exit inside it, on top of the steady ones.
        clones, samples, pool = self.records()
        clones += [{"spawn_s": 18.9, "healthy_s": 19.0, "stop_s": 21.0, "exit_s": 21.05,
                    "generation": 2, "error": None} for _ in range(100)]
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertIn("starts equal stops", self.failed(v))
        self.assertEqual((v["observed"]["starts"], v["observed"]["stops"]), (600, 700))

    def test_an_event_on_the_windows_end_belongs_to_what_follows(self):
        clones, samples, pool = self.records()
        extra = {"spawn_s": 79.9, "healthy_s": 80.0, "stop_s": 82.0, "exit_s": 82.05, "generation": 2, "error": None}
        gone = {"spawn_s": 77.8, "healthy_s": 77.95, "stop_s": 79.95, "exit_s": 80.0, "generation": 2, "error": None}
        first = {"spawn_s": 19.9, "healthy_s": 20.0, "stop_s": 22.0, "exit_s": 22.05, "generation": 2, "error": None}
        base = startrate.churn_verdict(self.CFG, clones, samples, pool)["observed"]
        v = startrate.churn_verdict(self.CFG, clones + [extra, gone, first], samples, pool)["observed"]
        # `extra` is healthy exactly at the end: not a start. `gone` is healthy inside and
        # exits exactly at the end: a start, not a stop. `first` is healthy exactly at the
        # start: a start and a stop.
        self.assertEqual((v["starts"] - base["starts"], v["stops"] - base["stops"]), (2, 1))

    def test_a_failed_clone_fails(self):
        clones, samples, pool = self.records()
        clones[300].update(healthy_s=None, stop_s=None, exit_s=31.0, error="exited 1 before healthy")
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["no failed clone"])
        self.assertEqual(v["failures"]["window"], 1)

    def test_a_start_that_hangs_through_the_windows_end_fails(self):
        # Three clones spawned 55 s before the end that never get healthy and have no error
        # yet: their timeout is longer than what is left of the window.
        clones, samples, pool = self.records()
        clones += [{"spawn_s": 25.0, "healthy_s": None, "stop_s": None, "exit_s": 81.0,
                    "generation": None, "error": None} for _ in range(3)]
        v = startrate.churn_verdict(dict(self.CFG, clone_timeout_s=180.0), clones, samples, pool)
        self.assertEqual(self.failed(v), ["no failed clone"])
        self.assertEqual(v["failures"], {"window": 0, "still_starting_at_end": 3, "warmup": 0, "total": 3})

    def test_a_start_that_hangs_and_exits_just_after_the_window_fails(self):
        # Spawned 20 s before the window's end, never healthy, and its fcvm exits half a
        # second after the end: an error with a time outside the window. It was still
        # starting when the window ended.
        clones, samples, pool = self.records()
        clones.append({"spawn_s": 60.0, "healthy_s": None, "stop_s": None, "exit_s": 80.5, "fail_s": 80.5,
                       "generation": None, "error": "exited 1 before healthy"})
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["no failed clone"])
        self.assertEqual(v["failures"], {"window": 0, "still_starting_at_end": 1, "warmup": 0, "total": 1})

    def test_a_clone_just_spawned_at_the_windows_end_is_not_hung(self):
        # Ten times the window's p99 (100 ms) is 1 s: a clone 0.9 s old at the end is still
        # starting, one 1.1 s old is hung.
        for age, hung in ((0.9, 0), (1.1, 1)):
            clones, samples, pool = self.records()
            clones.append({"spawn_s": 80.0 - age, "healthy_s": None, "stop_s": None, "exit_s": 80.5,
                           "generation": None, "error": None})
            v = startrate.churn_verdict(self.CFG, clones, samples, pool)
            self.assertEqual(v["failures"]["still_starting_at_end"], hung, age)
            self.assertEqual(self.failed(v), ["no failed clone"] if hung else [], age)

    def test_a_warm_up_clone_that_times_out_in_the_window_fails(self):
        # Spawned at 5 s, in warm-up; the harness gave up on it at 35 s, in the window.
        clones, samples, pool = self.records()
        clones.append({"spawn_s": 5.0, "healthy_s": None, "stop_s": None, "exit_s": None, "fail_s": 35.0,
                       "generation": None, "error": "not healthy within 30.0s"})
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["no failed clone"])
        self.assertEqual((v["failures"]["window"], v["failures"]["warmup"]), (1, 0))

    def test_a_failure_in_warm_up_is_counted_apart(self):
        clones, samples, pool = self.records()
        clones.append({"spawn_s": 5.0, "healthy_s": None, "stop_s": None, "exit_s": 6.0, "fail_s": 6.0,
                       "generation": None, "error": "exited 1 before healthy"})
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), [])
        self.assertEqual(v["failures"], {"window": 0, "still_starting_at_end": 0, "warmup": 1, "total": 1})

    def test_a_rising_alive_count_fails(self):
        clones, _, pool = self.records()
        rising = [(t / 2, 1, 15 + int(t / 2 / 9)) for t in range(180)]  # 17 at 20 s, 23 at 80 s
        v = startrate.churn_verdict(self.CFG, clones, rising, pool)
        self.assertIn("alive count flat", self.failed(v))

    def test_no_alive_samples_fail(self):
        clones, _, pool = self.records()
        v = startrate.churn_verdict(self.CFG, clones, [], pool)
        self.assertEqual(self.failed(v), ["alive count flat", "alive count accounted for", "slots cover the resets"])

    def test_a_stalled_alive_sampler_fails_the_alive_conditions(self):
        clones, samples, pool = self.records()
        alive = ["alive count flat", "alive count accounted for", "slots cover the resets"]
        cases = {
            "one sample in the middle": [x for x in samples if x[0] == 50.0],
            "no sample in the last 3 s": [x for x in samples if x[0] < 77.0],
            "no sample in the first 3 s": [x for x in samples if x[0] > 23.0],
            "a gap of 4 s": [x for x in samples if not 50.2 < x[0] < 54.0],
        }
        for what, kept in cases.items():
            v = startrate.churn_verdict(self.CFG, clones, kept, pool)
            self.assertEqual(self.failed(v), alive, what)
            self.assertFalse(v["alive_coverage"]["ok"], what)
        # A gap of 3 s is allowed, and the samples need not start before the window.
        for kept in ([x for x in samples if not 50.2 < x[0] < 53.0], [x for x in samples if 21.5 <= x[0] <= 78.5]):
            self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, kept, pool)), [])

    def test_fewer_clones_up_and_starting_than_asked_for_fails(self):
        # Six up and none starting against the twenty asked for: steady, and not the run
        # that was asked for.
        clones, _, pool = self.records(rate=3)
        low = [(t / 2, 0, 6) for t in range(180)]
        v = startrate.churn_verdict(self.CFG, clones, low, pool)
        self.assertEqual(self.failed(v), ["alive count accounted for"])
        self.assertEqual((v["observed"]["alive_target"], v["observed"]["alive_mean"]), (20, 6.0))

    def test_an_alive_count_the_rate_does_not_explain_fails(self):
        # Twenty up and starting, as asked, but ten starts a second of clones that stay up
        # 1.05 s are ten or eleven clones, not nineteen.
        clones, _, pool = self.records(lifetime=1.0)
        samples = [(t / 2, 1, 19) for t in range(180)]
        v = startrate.churn_verdict(dict(self.CFG, lifetime_s=1.0), clones, samples, pool)
        self.assertEqual(self.failed(v), ["alive count accounted for"])

    def test_a_start_in_a_first_generation_namespace_fails(self):
        for generation in (1, None):  # a namespace no clone had before; a namespace of the clone's own
            clones, samples, pool = self.records()
            clones[400]["generation"] = generation
            v = startrate.churn_verdict(self.CFG, clones, samples, pool)
            self.assertEqual(self.failed(v), ["every start in a recycled namespace"], generation)
            self.assertEqual(v["observed"]["recycled_fraction"], round(599 / 600, 4))

    def test_free_slots_that_run_down_fail(self):
        clones, samples, pool = self.records()
        for t, _, counters in pool:
            counters["ready"] = 79 - int(t / 10)  # 77 at 20 s, 72 at 79 s: 5 of 100 slots
            counters["resetting"] = 100 - 20 - counters["ready"]
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["free slots flat"])

    def test_a_namespace_built_or_destroyed_in_the_window_fails(self):
        for counter in ("builds", "destroys"):
            clones, samples, pool = self.records()
            for t, _, counters in pool:
                counters[counter] += 1 if t >= 50 else 0
            v = startrate.churn_verdict(self.CFG, clones, samples, pool)
            self.assertEqual(self.failed(v), ["no namespace built or destroyed"], counter)

    def test_a_build_after_the_last_sample_inside_the_window_is_counted(self):
        # Five builds between 79.5 s and the window's end at 80 s: no sample inside the
        # window shows them, the sample asked for at 80 s does.
        clones, samples, pool = self.records()
        for t, _, counters in pool:
            counters["builds"] += 5 if t >= 80 else 0
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["no namespace built or destroyed"])
        self.assertEqual(v["pool"]["builds"], 5)

    def test_two_samples_in_the_middle_of_the_window_do_not_cover_it(self):
        # Samples at 40 s and 41 s alone, and five builds at 60 s: a pool too loaded to
        # answer looks like this.
        clones, samples, pool = self.records()
        sparse = [x for x in pool if x[0] in (40.0, 41.0)]
        v = startrate.churn_verdict(self.CFG, clones, samples, sparse)
        self.assertEqual(self.failed(v), self.POOL)
        self.assertIsNone(v["pool"]["builds"])
        self.assertFalse(v["pool"]["coverage"]["ok"])

    def test_the_pool_samples_must_reach_both_edges_and_leave_no_gap(self):
        clones, samples, pool = self.records()
        cases = {
            "the sample before the window is 3 s early": [x for x in pool if not 17.5 < x[0] < 20.5],
            "no sample after the window": [x for x in pool if x[0] < 80],
            "the sample after the window is 3 s late": [x for x in pool if not 79.5 < x[0] < 82.5],
            "a gap of 4 s": [x for x in pool if not 50.5 < x[0] < 53.5],
        }
        for what, kept in cases.items():
            v = startrate.churn_verdict(self.CFG, clones, samples, kept)
            self.assertEqual(self.failed(v), self.POOL, what)
        # A gap of 3 s is allowed, and so is an edge sample 2 s away.
        for kept in ([x for x in pool if not 50.5 < x[0] < 52.5], [x for x in pool if not 18.5 < x[0] < 20.5],
                     [x for x in pool if not 79.5 < x[0] < 81.5]):
            self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, samples, kept)), [])

    def test_a_sample_without_its_answer_time_was_answered_by_the_next_question(self):
        # Records of a sampler that kept the time it asked alone: the sample asked at 20 s
        # may have been answered inside the window, so the one before it is the edge.
        clones, samples, pool = self.records()
        asked_only = [(t, counters) for t, _, counters in pool]
        first, last, _, coverage = startrate.pool_bracket(asked_only, 20.0, 80.0)
        self.assertEqual((first[0], last[0], coverage["before_s"], coverage["ok"]), (19.0, 80.0, 0.0, True))
        self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, samples, asked_only)), [])

    def test_too_few_slots_for_the_resets_fail(self):
        # 20 alive + 10/s x 0.033 s + 4 in flight is 24.3; 24 slots are one short.
        clones, samples, pool = self.records(slots=24)
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["slots cover the resets"])
        clones, samples, pool = self.records(slots=25)
        self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, samples, pool)), [])

    def test_missing_pool_counters_fail_every_pool_condition(self):
        clones, samples, _ = self.records()
        v = startrate.churn_verdict(self.CFG, clones, samples, [])
        self.assertEqual(self.failed(v), self.POOL)
        self.assertFalse(v["ok"])

    def test_what_the_run_itself_knows_is_judged_too(self):
        clones, samples, pool = self.records()
        cases = [
            ({"warm": False}, "warm-up ended"),
            ({"measured": False}, "window completed"),
            ({"state_healthy": 19}, "state files agree"),
            ({"state_steady": 0, "state_healthy": 0}, "state files agree"),
            ({"left": {"leased": 1, "resetting": 0, "listed": 0}}, "nothing left after the drain"),
            ({"left": {"leased": 0, "resetting": 2, "listed": 0}}, "nothing left after the drain"),
            ({"left": {"leased": 0, "resetting": 0, "listed": 3}}, "nothing left after the drain"),
            # A listing that was not taken supports nothing.
            ({"left": {"leased": 0, "resetting": 0, "listed": None}}, "nothing left after the drain"),
        ]
        for change, name in cases:
            v = startrate.churn_verdict(self.CFG, clones, samples, pool, dict(self.RUN, **change))
            self.assertEqual(self.failed(v), [name], change)
            self.assertFalse(v["ok"])
        # A run that says nothing of itself fails all four.
        v = startrate.churn_verdict(self.CFG, clones, samples, pool, {})
        self.assertEqual(self.failed(v), ["warm-up ended", "window completed", "state files agree",
                                          "nothing left after the drain"])

    def test_the_reset_quantile_is_the_upper_edge_of_its_bucket_over_the_window(self):
        first = {"reset_us_log2": [5] + [0] * 23}
        last = {"reset_us_log2": [5] + [0] * 9 + [98, 0, 0, 2] + [0] * 10}  # 98 of ~1.5 ms, 2 of ~12 ms
        self.assertEqual(startrate.reset_quantile_s(first, last, 0.5), 2048 / 1e6)
        self.assertEqual(startrate.reset_quantile_s(first, last, 0.98), 2048 / 1e6)
        self.assertEqual(startrate.reset_quantile_s(first, last, 0.99), 16384 / 1e6)
        self.assertIsNone(startrate.reset_quantile_s(first, first, 0.99))

    def test_warm_up_ends_only_when_recent_starts_are_all_recycled(self):
        fresh = [(t / 10, 1) for t in range(100)]            # 10 s of first-generation starts
        mixed = fresh + [(10 + t / 10, 2) for t in range(15)]  # then 1.5 s of recycled ones
        done = fresh + [(10 + t / 10, 2) for t in range(25)]   # then 2.5 s of them
        self.assertFalse(startrate.recycled_only_since(fresh, 2.0, 10.0))
        self.assertFalse(startrate.recycled_only_since(mixed, 2.0, 11.5))
        self.assertTrue(startrate.recycled_only_since(done, 2.0, 12.5))
        # Not before a lifetime has passed since the first healthy clone.
        self.assertFalse(startrate.recycled_only_since([(t / 10, 2) for t in range(30)], 5.0, 3.0))
        # Not on a handful of starts.
        self.assertFalse(startrate.recycled_only_since([(0.0, 2), (9.5, 2)], 2.0, 10.0))

    def test_a_small_run_gets_a_longer_stretch_to_end_its_warm_up(self):
        # Five clones of 1 s make five starts a second: twenty starts never fit in 2 s.
        self.assertEqual(startrate.warmup_quiet_s(1000, 1.0), 2.0)
        self.assertEqual(startrate.warmup_quiet_s(20, 1.0), 2.0)
        self.assertEqual(startrate.warmup_quiet_s(5, 1.0), 8.0)
        slow = [(t / 5, 2) for t in range(100)]  # 5 starts a second for 20 s, all recycled
        self.assertFalse(startrate.recycled_only_since(slow, 1.0, 20.0))
        self.assertTrue(startrate.recycled_only_since(slow, 1.0, 20.0, startrate.warmup_quiet_s(5, 1.0)))

    def test_lifetimes_spread_around_the_mean_and_repeat(self):
        lives = [startrate.lifetime_of(i, 2.0, 0.2) for i in range(2000)]
        self.assertTrue(all(1.6 <= x <= 2.4 for x in lives))
        self.assertAlmostEqual(sum(lives) / len(lives), 2.0, delta=0.02)
        self.assertGreater(max(lives) - min(lives), 0.7)
        self.assertEqual(lives[:50], [startrate.lifetime_of(i, 2.0, 0.2) for i in range(50)])
        self.assertEqual(startrate.lifetime_of(7, 2.0, 0.0), 2.0)

    def test_a_small_run_may_swing_by_five_clones_and_no_more(self):
        clones, _, pool = self.records()
        swing = lambda low: [(t / 2, 1, low if t % 6 == 0 else 20) for t in range(180)]  # noqa: E731
        self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, swing(15), pool)), [])
        self.assertEqual(self.failed(startrate.churn_verdict(self.CFG, clones, swing(14), pool)), ["alive count flat"])

    def test_a_large_run_may_swing_by_a_fifth(self):
        cfg = dict(self.CFG, alive=200)
        clones, _, pool = self.records(rate=100, alive=200, slots=400)
        swing = lambda low: [(t / 2, 1, low if t % 6 == 0 else 200) for t in range(180)]  # noqa: E731
        self.assertEqual(self.failed(startrate.churn_verdict(cfg, clones, swing(160), pool)), [])
        self.assertEqual(self.failed(startrate.churn_verdict(cfg, clones, swing(159), pool)), ["alive count flat"])

    def churn_args(self, *extra):
        return startrate.build_parser().parse_args([
            "churn", "--fcvm", "/nonexistent", "--prep", "/nonexistent", "--out", "/nonexistent",
            "--alive", "10", "--concurrency", "2", *extra,
        ])

    def refusal(self, *extra):
        args = self.churn_args(*extra)
        with self.assertRaises(SystemExit) as refused:
            args.fn(args)
        return str(refused.exception)

    def test_churn_refuses_a_window_too_short_to_judge(self):
        self.assertIn("BLOCKED: --duration 120.0 is under 300 s", self.refusal("--lifetime", "30", "--duration", "120"))

    def test_churn_refuses_a_timeout_that_outlasts_the_window(self):
        self.assertIn("BLOCKED: --timeout 120.0 is not under --duration 120.0",
                      self.refusal("--lifetime", "1", "--duration", "120", "--timeout", "120"))
        # The default timeout is under the shortest window churn accepts.
        self.assertLess(self.churn_args("--lifetime", "1").timeout, startrate.CHURN_MIN_WINDOW_S)

    def test_churn_refuses_a_run_too_small_to_end_its_warm_up(self):
        # One clone of 30 s starts once in 30 s: twenty recycled starts take 1200 s.
        args = startrate.build_parser().parse_args([
            "churn", "--fcvm", "/nonexistent", "--prep", "/nonexistent", "--out", "/nonexistent",
            "--alive", "1", "--concurrency", "1", "--lifetime", "30", "--duration", "300",
        ])
        with self.assertRaises(SystemExit) as refused:
            args.fn(args)
        self.assertIn("start too seldom for a warm-up", str(refused.exception))


class ChurnRecords(unittest.TestCase):
    """A churn directory judged again from its files."""

    def write(self, d, run=None, summary=None, pool_rows=None):
        clones, samples, pool = ChurnVerdict().records()
        cfg = {"mode": "churn", "alive": 20, "lifetime_s": 2.0, "concurrency": 4, "clone_timeout_s": 30.0}
        (d / "host.json").write_text(json.dumps({"config": cfg}))
        (d / "samples.jsonl").write_text("".join(json.dumps(c) + "\n" for c in clones))
        (d / "alive.tsv").write_text("t_s\tin_flight\trunning\n" + "".join(f"{t}\t{f}\t{r}\n" for t, f, r in samples))
        rows = pool_rows if pool_rows is not None else [
            {"t_s": asked, "answered_s": answered, **counters} for asked, answered, counters in pool]
        (d / "pool.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
        if run is not None:
            (d / "run.json").write_text(json.dumps(run))
        if summary is not None:
            (d / "summary.json").write_text(json.dumps(summary))

    def test_report_judges_a_churn_directory_from_its_records(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            run = dict(ChurnVerdict.RUN, window_start_s=20.0, window_end_s=80.0,
                       drain={"clones": 20, "teardown_s": 0.1, "until_pool_idle_s": 0.2},
                       driver={"cpu_s": 6.0, "cpu_cores": 0.1})
            self.write(d, run=run)
            s = startrate.churn_from_records(d)
            self.assertTrue(s["ok"], [c for c in s["conditions"] if not c["ok"]])
            self.assertEqual(s["sustained"]["starts_per_s"], 10.0)
            self.assertEqual(s["driver"]["cpu_cores"], 0.1)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                startrate.cmd_report(argparse.Namespace(dirs=[t], json=False))
            self.assertIn("window 60.0 s: 10.0 starts/s", out.getvalue())
            self.assertNotIn("NOT SUSTAINED", out.getvalue())
            self.assertIn("20.0 up + 1.0 starting of 20 asked for", out.getvalue())

    def test_a_failed_window_is_printed_as_not_sustained(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            run = dict(ChurnVerdict.RUN, window_start_s=20.0, window_end_s=80.0, state_healthy=3)
            self.write(d, run=run)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                startrate.cmd_report(argparse.Namespace(dirs=[t], json=False))
            self.assertTrue(out.getvalue().startswith("NOT SUSTAINED: window 60.0 s"), out.getvalue()[:80])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                startrate.cmd_report(argparse.Namespace(dirs=[t], json=True))
            s = json.loads(out.getvalue())
            self.assertEqual((s["ok"], s["sustained"]), (False, None))

    def test_records_of_the_first_harness_cannot_show_that_nothing_was_left(self):
        # Its summary held the window as the warm-up's length and the window's, its two
        # run-time conditions as text, and no listing after the drain; its pool samples
        # have the time they were asked alone.
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            clones, samples, pool = ChurnVerdict().records()
            summary = {
                "config": {"mode": "churn", "alive": 20, "lifetime_s": 2.0, "concurrency": 4, "clone_timeout_s": 180},
                "window_s": 60.0, "warmup": {"s": 20.0},
                "drain": {"clones": 20, "teardown_s": 0.1, "until_pool_idle_s": 0.2},
                "conditions": [
                    {"name": "warm-up ended", "ok": True, "detail": "20.0 s until"},
                    {"name": "state files agree", "ok": True, "detail": "20 of 20 running clones healthy in fcvm ls"},
                ],
            }
            self.write(d, summary=summary, pool_rows=[{"t_s": asked, **counters} for asked, _, counters in pool])
            s = startrate.churn_from_records(d)
            self.assertEqual([c["name"] for c in s["conditions"] if not c["ok"]], ["nothing left after the drain"])
            self.assertIsNone(s["sustained"])
            self.assertTrue(s["pool"]["coverage"]["ok"])

    def test_a_failed_poll_is_a_row_and_leaves_its_gap(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            _, _, pool = ChurnVerdict().records()
            rows = [{"t_s": asked, "answered_s": answered, **counters} if not 50.5 < asked < 53.5
                    else {"t_s": asked, "answered_s": asked + 2.0, "failed": True} for asked, answered, counters in pool]
            run = dict(ChurnVerdict.RUN, window_start_s=20.0, window_end_s=80.0)
            del run["polls_failed"]
            self.write(d, run=run, pool_rows=rows)
            s = startrate.churn_from_records(d)
            self.assertEqual(s["pool"]["polls_failed"], 3)
            self.assertIn("pool counters cover the window", [c["name"] for c in s["conditions"] if not c["ok"]])


class ChurnRun(unittest.TestCase):
    """cmd_churn against a fake fcvm and a fake pool: real processes, no VMs."""

    FAKE = r"""#!/usr/bin/env python3
import json, os, signal, sys, time
state = os.environ["STATE"]
live = os.path.join(state, "live")
args = sys.argv[1:]
if args[:1] == ["ls"]:
    print(json.dumps([{"name": n, "health_status": "healthy"} for n in os.listdir(live)]))
elif args[:2] == ["netpool", "stats"]:
    if os.path.exists(os.path.join(state, "pool-silent")):
        sys.exit(1)
    leased = len(os.listdir(live))
    resets = len(os.listdir(os.path.join(state, "gone")))
    hist = [0] * 24
    hist[10] = resets
    print(json.dumps({"building": 0, "ready": 40 - leased, "leased": leased, "resetting": 0, "destroying": 0,
                      "builds": 40, "destroys": 0, "grants": 0, "recycled_grants": 0, "misses": 0,
                      "resets": resets, "reset_us_total": 0, "reset_us_log2": hist}))
elif args[:2] == ["snapshot", "run"]:
    name = args[args.index("--name") + 1]
    index = int(name.rsplit("-c", 1)[1])
    hang = os.environ.get("HANG_EVERY")
    def leave(*_):
        try:
            os.unlink(os.path.join(live, name))
            open(os.path.join(state, "gone", name), "w").close()
        except FileNotFoundError:
            pass
        sys.exit(0)
    signal.signal(signal.SIGTERM, leave)
    if hang and index % int(hang) == int(hang) - 1:
        time.sleep(300)
    open(os.path.join(live, name), "w").close()
    print("running in a pooled namespace generation=2", flush=True)
    print("VM healthy, switching to polling", flush=True)
    time.sleep(300)
"""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.d = Path(self.tmp.name)
        for sub in ("live", "gone"):
            (self.d / "state" / sub).mkdir(parents=True)
        self.fcvm = self.d / "fcvm"
        self.fcvm.write_text(self.FAKE)
        self.fcvm.chmod(0o755)
        self.prep = self.d / "prep.json"
        self.prep.write_text(json.dumps({"serve_pid": None, "network": "rootless", "cpu": 1, "mem_mib": 256,
                                         "image": "i", "snapshot": "s", "sudo": False}))
        for target, value in ((os.environ, {"STATE": str(self.d / "state")}),):
            p = mock.patch.dict(target, value)
            p.start()
            self.addCleanup(p.stop)
        for name, value in (("host_facts", lambda repo: {}), ("CHURN_MIN_WINDOW_S", 4)):
            p = mock.patch.object(startrate, name, value)
            p.start()
            self.addCleanup(p.stop)

    def args(self, **kw):
        base = dict(fcvm=str(self.fcvm), prep=str(self.prep), out=str(self.d / "out"), alive=6, lifetime=0.3,
                    lifetime_jitter=0.2, concurrency=6, duration=4.0, warmup_limit=30.0, launchers=2,
                    timeout=2.0, rust_log="fcvm=info", fail_fast=False)
        return argparse.Namespace(**{**base, **kw})

    def run_churn(self, **kw):
        out = io.StringIO()
        code = 0
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                startrate.cmd_churn(self.args(**kw))
            except SystemExit as e:
                code = e.code
        return code, out.getvalue(), json.loads((self.d / "out" / "summary.json").read_text())

    def test_a_steady_fake_run_passes_every_condition_and_records_its_own_cost(self):
        code, printed, s = self.run_churn()
        self.assertEqual([c for c in s["conditions"] if not c["ok"]], [])
        self.assertEqual(code, 0)
        self.assertTrue(printed.splitlines()[-len(s["conditions"]) - 5].startswith("window "), printed)
        self.assertEqual(s["sustained"], s["observed"])
        self.assertGreater(s["observed"]["starts"], 20)
        self.assertEqual(s["failures"]["total"], 0)
        self.assertEqual(s["left"], {"leased": 0, "resetting": 0, "listed": 0})
        self.assertTrue(s["pool"]["coverage"]["ok"])
        self.assertEqual(s["pool"]["polls_failed"], 0)
        self.assertGreater(s["driver"]["cpu_s"], 0)
        self.assertIsNotNone(s["driver"]["popen_ms"]["p50"])
        rows = [json.loads(x) for x in (self.d / "out" / "samples.jsonl").read_text().splitlines()]
        self.assertTrue(all(r["popen_ms"] is not None for r in rows))
        # The directory judged again says the same.
        again = startrate.churn_from_records(self.d / "out")
        self.assertEqual((again["ok"], again["observed"]), (s["ok"], s["observed"]))

    def test_a_start_that_hangs_is_a_failure_with_its_time(self):
        # Every ninth clone never gets healthy. Its 2 s timeout ends inside the 4 s window
        # or, for one spawned in its last 2 s, after it: both count.
        with mock.patch.dict(os.environ, {"HANG_EVERY": "9"}):
            code, printed, s = self.run_churn()
        self.assertEqual(code, 1)
        self.assertTrue(printed.startswith("{") and "NOT SUSTAINED: window" in printed, printed[:200])
        self.assertIn("no failed clone", [c["name"] for c in s["conditions"] if not c["ok"]])
        self.assertGreater(s["failures"]["window"] + s["failures"]["still_starting_at_end"], 0)
        self.assertIsNone(s["sustained"])
        rows = [json.loads(x) for x in (self.d / "out" / "samples.jsonl").read_text().splitlines()]
        timed_out = [r for r in rows if r["error"] and "not healthy within" in r["error"]]
        self.assertTrue(timed_out and all(r["fail_s"] is not None for r in timed_out))

    def test_a_pool_that_stops_answering_fails_the_pool_conditions(self):
        def silence():
            time.sleep(3.0)
            (self.d / "state" / "pool-silent").touch()
        quiet = threading.Thread(target=silence)
        quiet.start()
        code, _, s = self.run_churn(duration=8.0, timeout=2.0)
        quiet.join()
        self.assertEqual(code, 1)
        failed = [c["name"] for c in s["conditions"] if not c["ok"]]
        self.assertIn("pool counters cover the window", failed)
        self.assertGreater(s["pool"]["polls_failed"], 0)
        self.assertFalse((self.d / "state" / "live").exists() and os.listdir(self.d / "state" / "live"))

    def test_churn_refuses_a_bridged_prep(self):
        self.prep.write_text(json.dumps({"serve_pid": None, "network": "bridged", "cpu": 1, "mem_mib": 256,
                                         "image": "i", "snapshot": "s", "sudo": True}))
        with self.assertRaises(SystemExit) as refused:
            startrate.cmd_churn(self.args())
        self.assertIn("only a rootless clone is pooled", str(refused.exception))


if __name__ == "__main__":
    unittest.main()
