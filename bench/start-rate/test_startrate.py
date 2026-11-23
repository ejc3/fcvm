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
            timeout=30.0, rust_log="fcvm=info", pidstat_interval=2, fail_fast=False, hold=0.0,
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


class ChurnVerdict(unittest.TestCase):
    """Each honesty condition of a churn window, on records made by hand."""

    CFG = {"alive": 20, "lifetime_s": 2.0, "concurrency": 4, "window_start_s": 20.0, "window_end_s": 80.0}

    def records(self, rate=10, lifetime=2.0, end=90.0, slots=100, alive=20):
        """A steady run: a clone every 1/rate s, healthy 0.1 s after its spawn, stopped a
        lifetime later, gone 0.05 s after that; every namespace recycled; no builds."""
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
            pool.append((float(t), {
                "building": 0, "ready": slots - alive - 1, "leased": alive, "resetting": 1, "destroying": 0,
                "builds": slots, "destroys": 0, "grants": t * rate, "recycled_grants": t * rate,
                "misses": 0, "resets": t * rate, "reset_us_total": t * rate * 20000, "reset_us_log2": hist,
            }))
        return clones, samples, pool

    def failed(self, verdict):
        return [c["name"] for c in verdict["conditions"] if not c["ok"]]

    def test_a_steady_window_passes_and_reports_its_rates(self):
        v = startrate.churn_verdict(self.CFG, *self.records())
        self.assertEqual(self.failed(v), [])
        self.assertTrue(v["ok"])
        self.assertEqual((v["starts"], v["stops"]), (600, 600))
        self.assertEqual((v["starts_per_s"], v["stops_per_s"], v["recycled_fraction"]), (10.0, 10.0, 1.0))
        self.assertEqual(v["alive_mean"], 20.0)
        self.assertEqual(v["pool"], {"slots": 100, "builds": 0, "destroys": 0, "reset_p99_s": 0.032768})
        self.assertEqual(v["spawn_to_healthy_ms"]["p50"], 100.0)

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
        self.assertEqual(self.failed(v), ["starts equal stops"])

    def test_a_failed_clone_fails(self):
        clones, samples, pool = self.records()
        clones[300].update(healthy_s=None, stop_s=None, exit_s=31.0, error="exited 1 before healthy")
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["no failed clone"])

    def test_a_rising_alive_count_fails(self):
        clones, _, pool = self.records()
        rising = [(t / 2, 1, 15 + int(t / 2 / 9)) for t in range(180)]  # 17 at 20 s, 23 at 80 s
        v = startrate.churn_verdict(self.CFG, clones, rising, pool)
        self.assertEqual(self.failed(v), ["alive count flat"])

    def test_no_alive_samples_fail(self):
        clones, _, pool = self.records()
        v = startrate.churn_verdict(self.CFG, clones, [], pool)
        self.assertEqual(self.failed(v), ["alive count flat", "slots cover the resets"])

    def test_a_start_in_a_first_generation_namespace_fails(self):
        for generation in (1, None):  # a namespace no clone had before; a namespace of the clone's own
            clones, samples, pool = self.records()
            clones[400]["generation"] = generation
            v = startrate.churn_verdict(self.CFG, clones, samples, pool)
            self.assertEqual(self.failed(v), ["every start in a recycled namespace"], generation)
            self.assertEqual(v["recycled_fraction"], round(599 / 600, 4))

    def test_free_slots_that_run_down_fail(self):
        clones, samples, pool = self.records()
        for t, counters in pool:
            counters["ready"] = 79 - int(t / 10)  # 77 at 20 s, 72 at 79 s: 5 of 100 slots
            counters["resetting"] = 100 - 20 - counters["ready"]
        v = startrate.churn_verdict(self.CFG, clones, samples, pool)
        self.assertEqual(self.failed(v), ["free slots flat"])

    def test_a_namespace_built_or_destroyed_in_the_window_fails(self):
        for counter in ("builds", "destroys"):
            clones, samples, pool = self.records()
            for t, counters in pool:
                counters[counter] += 1 if t >= 50 else 0
            v = startrate.churn_verdict(self.CFG, clones, samples, pool)
            self.assertEqual(self.failed(v), ["no namespace built or destroyed"], counter)

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
        self.assertEqual(self.failed(v), ["free slots flat", "no namespace built or destroyed", "slots cover the resets"])
        self.assertFalse(v["ok"])

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

    def test_churn_refuses_a_window_too_short_to_judge(self):
        args = startrate.build_parser().parse_args([
            "churn", "--fcvm", "/nonexistent", "--prep", "/nonexistent", "--out", "/nonexistent",
            "--alive", "10", "--lifetime", "30", "--concurrency", "2", "--duration", "120",
        ])
        with self.assertRaises(SystemExit) as refused:
            args.fn(args)
        self.assertIn("BLOCKED: --duration 120.0 is under 300 s", str(refused.exception))


if __name__ == "__main__":
    unittest.main()
