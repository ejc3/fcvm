"""Tests of the density harness (mcbench.py) and its summary (summarize.py). Run by
`make test-minecraft`; nothing here needs root, fcvm or a server."""
import json, os, signal, subprocess, sys, tempfile, time, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mcbench  # noqa: E402
import summarize  # noqa: E402


def alive(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().split(")")[-1].split()[0] != "Z"
    except FileNotFoundError:
        return False


class ChildrenDieWithTheHarness(unittest.TestCase):
    def test_a_killed_harness_takes_its_children(self):
        # SIGKILL runs no atexit handler: only the parent-death signal stops the child.
        harness = subprocess.Popen(
            [sys.executable, "-c",
             "import sys, time; sys.path.insert(0, sys.argv[1]); import mcbench; "
             "p = mcbench.popen(['sleep', '300']); print(p.pid, flush=True); time.sleep(300)", HERE],
            stdout=subprocess.PIPE, text=True)
        child = int(harness.stdout.readline())
        self.assertTrue(alive(child))
        harness.send_signal(signal.SIGKILL)
        harness.wait()
        deadline = time.monotonic() + 5
        while alive(child) and time.monotonic() < deadline:
            time.sleep(0.05)
        if alive(child):
            os.kill(child, signal.SIGKILL)
            self.fail("the child outlived its SIGKILLed harness")


class Forks(unittest.TestCase):
    def test_a_fork_with_no_copy_has_no_time(self):
        self.assertEqual(mcbench.fork_total(370.0, 0, 800.0), 1170.0)
        self.assertIsNone(mcbench.fork_total(370.0, 0, None))
        self.assertIsNone(mcbench.fork_total(370.0, 1, 800.0))

    def test_failed_forks_are_counted_not_timed(self):
        recs = [
            {"arm": "a", "snapshot_ms": 300, "snapshot_rc": 0, "restore_first_chunk_ms": 700, "fork_ms": 1000,
             "source_sessions_before": 3, "source_sessions_after": 3},
            {"arm": "a", "snapshot_ms": 300, "snapshot_rc": 0, "restore_first_chunk_ms": None, "fork_ms": None,
             "source_sessions_before": None, "source_sessions_after": None},
        ]
        with tempfile.TemporaryDirectory() as d:
            with open(f"{d}/fork.jsonl", "w") as f:
                f.write("".join(json.dumps(r) + "\n" for r in recs))
            out = subprocess.run([sys.executable, f"{HERE}/summarize.py", d], capture_output=True, text=True, check=True)
        a = json.loads(out.stdout)["a"]
        self.assertEqual(a["fork_failed"], 1)
        self.assertEqual(a["fork_total_ms"]["n"], 1)
        self.assertEqual(a["fork_total_ms"]["p50"], 1000)
        # Unread session counts are not a source that kept its bots.
        self.assertFalse(a["fork_source_kept_its_bots"])


if __name__ == "__main__":
    unittest.main()
