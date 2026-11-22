import argparse
import contextlib
import io
import json
import os
import signal
import stat
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


class FakeFcvm(unittest.TestCase):
    """cmd_run against a fake fcvm and pidstat: real processes, no VMs."""

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


if __name__ == "__main__":
    unittest.main()
