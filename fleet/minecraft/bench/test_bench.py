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


class HostRamp(unittest.TestCase):
    """hostramp.py's rules that do not need a host: which CPUs the VMs' cgroup is left with, and
    when a step holds."""

    @classmethod
    def setUpClass(cls):
        os.environ.setdefault("FCVM", "fcvm")
        import hostramp
        cls.R = hostramp

    def test_a_host_that_already_runs_vms_or_a_fleet_agent_is_in_use(self):
        R = self.R
        quiet = [["/usr/sbin/sshd", "-D"], [""], [], ["python3", "hostramp.py", "ramp", "3", "/out", "256"],
                 ["/usr/bin/grep", "firecracker"], ["/opt/fcvm", "ls", "--json"], ["fcvm", "fleet", "pick"]]
        self.assertEqual(R.in_use(quiet), [])
        busy = quiet + [["/usr/local/bin/firecracker", "--api-sock", "/x"], ["firecracker"], ["/opt/cloud-hypervisor", "--api-socket", "/y"],
                        ["/opt/fcvm", "fleet", "agent", "--stdio"], ["fcvm", "fleet", "coord", "--listen", "127.0.0.1:1"]]
        self.assertEqual(R.in_use(busy), ["firecracker", "firecracker", "cloud-hypervisor", "fcvm fleet agent", "fcvm fleet coord"])

    def test_a_run_without_a_core_limit_does_not_keep_an_earlier_runs(self):
        R = self.R
        root, cores = R.CGROOT, os.environ.pop("MC_CORES", None)
        with tempfile.TemporaryDirectory() as d:
            R.CGROOT = d
            try:
                os.environ["MC_CORES"] = "48"
                R.cg_setup()
                cpus = f"{d}/{R.CG}/cpuset.cpus"
                self.assertEqual(open(cpus).read(), "0-47")
                # The group outlives the run. The next run, with no limit, must not be held to
                # those 48: empty gives the group its parent's CPUs again.
                del os.environ["MC_CORES"]
                R.cg_setup()
                self.assertEqual(open(cpus).read(), "")
                # What a record says the VMs were allowed is what the kernel says, not what was asked.
                with open(f"{d}/{R.CG}/cpuset.cpus.effective", "w") as f:
                    f.write("0-23,96-119\n")
                self.assertEqual(R.cpus_allowed(), 48)
                os.remove(f"{d}/{R.CG}/cpuset.cpus.effective")
                self.assertEqual(R.cpus_allowed(), R.NCPU)
            finally:
                R.CGROOT = root
                os.environ.pop("MC_CORES", None)
                if cores is not None:
                    os.environ["MC_CORES"] = cores

    def test_a_step_holds_on_unrounded_readings_and_only_with_every_scene_set(self):
        R = self.R
        ok = {"a": [20.0, 19.98], "b": [19.97, 20.01]}
        tps, held = R.qualify(ok, 3, 2, 2)
        self.assertTrue(held)
        self.assertEqual((tps["min"], tps["servers_below_hold"], tps["readings"]), (19.97, 0, 4))
        # 19.496 is under 19.5 and shows as 19.5: it must not hold
        tps, held = R.qualify({"a": [20.0, 19.496], "b": [19.97, 20.01]}, 3, 2, 2)
        self.assertFalse(held)
        self.assertEqual((tps["min"], tps["servers_below_hold"]), (19.5, 1))
        # a server that was not given its scene is not the workload the step is for
        self.assertFalse(R.qualify(ok, 3, 1, 2)[1])
        # without bots there is no tick rate to hold
        self.assertFalse(R.qualify({"a": [1.0], "b": [1.0]}, 0, 2, 2)[1])


if __name__ == "__main__":
    unittest.main()
