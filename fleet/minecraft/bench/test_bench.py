"""Tests of the density harness (mcbench.py), its summary (summarize.py) and the fleet-path
watcher (fleetfield.py, fleetfield_summary.py). Run by
`make test-minecraft`; nothing here needs root, fcvm or a server."""
import base64, json, os, signal, struct, subprocess, sys, tempfile, time, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mcbench  # noqa: E402
import summarize  # noqa: E402
import fleetfield  # noqa: E402


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


class FleetField(unittest.TestCase):
    """fleetfield.py reads the coordinator's status rows as a page does (fleet/PROTOCOL.md, "Many
    servers"): 8 bytes a server, and what a host did not read is none, never zero."""

    @staticmethod
    def row(phase, tps10, players, age, launch):
        return bytes([phase, tps10, players, 0]) + struct.pack("<HH", age, launch)

    def test_rows_are_read_as_the_protocol_packs_them(self):
        rows = [self.row(3, 200, 3, 90, 410), self.row(3, 191, 3, 30, 520), self.row(3, 255, 255, 65535, 65535), self.row(1, 255, 255, 65535, 65535)]
        msg = {"rows": base64.b64encode(b"".join(rows)).decode()}
        s = fleetfield.summary(fleetfield.rows_of(msg))
        self.assertEqual(s["servers"], 4)
        self.assertEqual(s["phases"], {"none": 0, "restoring": 1, "forking": 0, "running": 3, "failed": 0})
        # a running server whose tile gave no tick rate is counted as no answer, not as a rate of zero
        self.assertEqual((s["tps"]["n"], s["tps"]["min"], s["tps"]["below_19_5"], s["tps"]["no_answer"]), (2, 19.1, 1, 1))
        self.assertEqual((s["launch_ms"]["n"], s["launch_ms"]["max"]), (2, 520))
        self.assertEqual((s["age_s"]["min"], s["age_s"]["max"]), (30, 90))

    def test_a_run_is_summarized_from_its_records(self):
        mark = {"event": "mark", "since_all_up": 300.0, "summary": {"tps": {"min": 20.0}}, "host": {"busy": 24.0, "mem_used_mib": 60000},
                "per_server_mib": {"pss_anon": 400.0}, "meminfo": {}}
        with tempfile.TemporaryDirectory() as d:
            with open(f"{d}/run.jsonl", "w") as f:
                for e in ({"event": "start", "n": 128}, {"event": "all_running", "at": 100.0, "restore_all_s": 12.04, "summary": {"launch_ms": {"p50": 5000}}}, mark,
                          {"event": "reset_all_sent", "at": 112.0}, {"event": "reset_all_done", "reset_all_s": 15.5, "summary": {"launch_ms": {"p50": 6000}}}):
                    f.write(json.dumps(e) + "\n")
            with open(f"{d}/samples.jsonl", "w") as f:
                for since, lo, below, silent in ((None, 19.0, 3, 0), (5.0, 20.0, 0, 0), (10.0, 12.5, 4, 2), (15.0, 3.0, 90, 30)):
                    f.write(json.dumps({"since_all_up": since, "tps": {"n": 128 - silent, "min": lo, "below_19_5": below, "no_answer": silent},
                                        "wire": {"snapshots": 4, "snap_bytes": 4000, "statuses": 10, "status_bytes": 5000}}) + "\n")
            with open(f"{d}/disk.log", "w") as f:
                f.write("100 store_mib=5000 load=1 2 3 fc=0\n115 store_mib=8840 load=1 2 3 fc=128 io_full_avg10=0.00\n")
            with open(f"{d}/host.txt", "w") as f:
                f.write("kernel example\n")
            out = json.loads(subprocess.run([sys.executable, f"{HERE}/fleetfield_summary.py", d], capture_output=True, text=True, check=True).stdout)
        self.assertEqual((out["servers"], out["restore_all_s"], out["reset_all_s"]), (128, 12.0, 15.5))
        self.assertEqual(out["mark_300s"]["busy_per_server"], 0.188)
        # Readings before every server ran, and after the reset of all was sent (12 s after), are not the
        # steady state's: of the two samples between, one had a reading under 19.5.
        self.assertEqual(out["reset_sent_s_after_all_ran"], 12.0)
        self.assertEqual((out["samples_all_up"], out["samples_with_a_reading_under_19_5"], out["lowest_tps_reading"], out["readings_no_answer_max"]), (2, 1, 12.5, 2))
        self.assertEqual((out["readings"], out["readings_under_19_5"], out["readings_unanswered"]), (256, 4, 2))
        self.assertEqual(out["low_or_unanswered"], [[10.0, 12.5, 4, 2]])
        self.assertEqual(out["store_mib"]["per_server_at_max"], 30.0)
        self.assertEqual(out["wire"], {"snapshots": 4, "snapshot_bytes_mean": 1000, "status_msgs": 10, "status_bytes_mean": 500})


if __name__ == "__main__":
    unittest.main()
