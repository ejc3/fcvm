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
                # those 48: an empty line gives the group its parent's CPUs again. The kernel
                # never sees a write of no bytes (on 7.0.0-1018-aws the group then kept 0-47, and
                # a step without bots ran on 48 cores), so what is written is a line.
                del os.environ["MC_CORES"]
                R.cg_setup()
                self.assertEqual(open(cpus).read(), "\n")
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

    def test_a_scene_post_that_times_out_is_sent_again_a_bounded_number_of_times(self):
        # 66 of 512 scene posts timed out in one step and were not sent again, so a step whose
        # every reading held did not count as held. A post that times out is sent again; one
        # that is answered is not, whatever the answer.
        import http.server, threading
        R = self.R
        seen = {"/slow": 0, "/dead": 0, "/refused": 0}

        class Tile(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("content-length", 0)))
                seen[self.path] += 1
                if self.path == "/dead" or (self.path == "/slow" and seen[self.path] == 1):
                    time.sleep(0.6)  # past the post's time
                code = 409 if self.path == "/refused" else 200
                try:
                    self.send_response(code)
                    self.send_header("content-length", "0")
                    self.end_headers()
                except OSError:
                    pass

            def log_message(self, *a):
                pass

        srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Tile)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        try:
            url = f"http://127.0.0.1:{srv.server_address[1]}"
            # the first send times out, the second is answered
            self.assertEqual(R.post_scene(f"{url}/slow", {}, timeout=0.2), (200, 2))
            # never answered in time: SCENE_SENDS sends, then the error, in bounded time
            t0 = time.monotonic()
            status, sends = R.post_scene(f"{url}/dead", {}, timeout=0.2)
            self.assertEqual((sends, seen["/dead"]), (R.SCENE_SENDS, R.SCENE_SENDS))
            self.assertIn("timed out", str(status))
            self.assertLess(time.monotonic() - t0, R.SCENE_SENDS * 0.2 + 1)
            # an answer that is not 200 is an answer: not sent again
            status, sends = R.post_scene(f"{url}/refused", {}, timeout=0.2)
            self.assertEqual((str(status)[:3], sends, seen["/refused"]), ("409", 1, 1))
        finally:
            srv.shutdown()

    def test_an_fcvm_command_that_hangs_ends_the_run_instead_of_blocking_it(self):
        # `fcvm ls` and `fcvm snapshot create` were waited on for ever: with either stalled, no
        # loop reached its deadline and no VM already started was stopped.
        R = self.R
        fcvm, ls, snap = R.FCVM, R.LS_TIMEOUT_S, R.SNAPSHOT_TIMEOUT_S
        with tempfile.TemporaryDirectory() as d:
            fake = f"{d}/fcvm"
            with open(fake, "w") as f:
                f.write("#!/bin/sh\nsleep 3\necho '[]'\n")
            os.chmod(fake, 0o755)
            R.FCVM, R.LS_TIMEOUT_S, R.SNAPSHOT_TIMEOUT_S = fake, 0.3, 0.3
            try:
                for call in (R.vms, lambda: R.fcvm_snapshot_create(1, "tag")):
                    t0 = time.monotonic()
                    with self.assertRaises(subprocess.TimeoutExpired):
                        call()
                    self.assertLess(time.monotonic() - t0, 2)
            finally:
                R.FCVM, R.LS_TIMEOUT_S, R.SNAPSHOT_TIMEOUT_S = fcvm, ls, snap

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
                f.write("100 store_mib=5000 load=1 2 3 fc=0\n115 store_mib=8840 load=1 2 3 fc=128 io_full_avg10=0.00 ndisc=960 arp=300\n"
                        "120 store_mib=8000 load=1 2 3 fc=128 ndisc=1280 arp=290\n")
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
        # the kernel's neighbour entries over every namespace: the most seen, and per server
        self.assertEqual(out["neighbour_entries_max"], {"ipv6": 1280, "ipv4": 300, "ipv6_per_server": 10.0})
        self.assertNotIn("never_all_running", out)
        self.assertEqual(out["wire"], {"snapshots": 4, "snapshot_bytes_mean": 1000, "status_msgs": 10, "status_bytes_mean": 500})
        # no coordinator's log beside the run: no time from it
        self.assertNotIn("all_up_s", out)

    def test_the_time_to_all_servers_up_is_read_from_the_coordinators_log(self):
        # The client first saw a restoring server late (at 35.4 s here), so its own figure is
        # short: the coordinator's log has when the gold was made and when each server came up.
        def summary(log, n=3):
            with tempfile.TemporaryDirectory() as d:
                with open(f"{d}/run.jsonl", "w") as f:
                    for e in ({"event": "start", "n": n}, {"event": "first_restoring", "at": 35.4},
                              {"event": "all_running", "at": 47.6, "restore_all_s": 12.2, "summary": {"launch_ms": {"p50": 22000}}}):
                        f.write(json.dumps(e) + "\n")
                with open(f"{d}/samples.jsonl", "w") as f:
                    f.write(json.dumps({"since_all_up": 5.0, "tps": {"n": n, "min": 20.0, "below_19_5": 0, "no_answer": 0},
                                        "wire": {"snapshots": 1, "snap_bytes": 1, "statuses": 1, "status_bytes": 1}}) + "\n")
                with open(f"{d}/disk.log", "w") as f:
                    f.write("100 store_mib=5000 load=1 2 3 fc=0\n")
                with open(f"{d}/host.txt", "w") as f:
                    f.write("kernel example\n")
                with open(f"{d}/coord.log", "w") as f:
                    f.write(log)
                return json.loads(subprocess.run([sys.executable, f"{HERE}/fleetfield_summary.py", d], capture_output=True, text=True, check=True).stdout)
        up = lambda at, name, gen=1: f"2026-01-02T03:04:{at}Z  INFO fcvm::fleet::minecraft: server up server={name} generation={gen} forked=false\n"
        hello = "2026-01-02T03:04:05.450000Z  INFO fcvm::fleet::coord: agent hello host=example-host conn=1\n"
        gold = "2026-01-02T03:04:19.250000Z  INFO fcvm::fleet::minecraft: gold snapshot made tag=example secs=13\n"
        servers = up("40.000000", "mc-1") + up("44.000000", "mc-2") + up("49.150000", "mc-3")
        made = summary("not a log line\n" + hello + gold + servers + up("58.000000", "mc-1", 2))
        self.assertEqual((made["restore_all_s"], made["all_up_s"]), (12.2, {"from": "gold snapshot made", "s": 29.9}))
        # the gold was already there: from the agent's hello
        self.assertEqual(summary(hello + servers)["all_up_s"], {"from": "agent hello", "s": 43.7})
        # not every server came up: no time
        self.assertIsNone(summary(hello + gold + servers, n=4)["all_up_s"])

    def test_every_committed_runs_time_to_all_up_is_rederived_from_its_log_excerpt(self):
        # A record keeps the lines of the coordinator's log its "all N up" figure is read from
        # (coord-up.log): the figure in its summary.json is what those lines give, and the
        # figure in the document's table (docs/fleet/minecraft.md) is that one.
        root = f"{HERE}/results/2026-11-24-fleet-path-c8g-metal-48xl"
        rows = {"256, root volume": "256-root-volume", "512, root volume, limit 1024": "512-root-volume-neighbour-limit-1024",
                "512, root volume": "512-root-volume", "512, RAM": "512-ram", "256, RAM (100 s)": "256-ram"}
        doc = open(f"{HERE}/../../../docs/fleet/minecraft.md").read()
        table = doc[doc.index("| run | all N up, from the first restore's start |"):].split("\n\n")[0].splitlines()[2:]
        said = {cells[1].strip(): cells[2].strip() for cells in (line.split("|") for line in table)}
        self.assertEqual(sorted(said), sorted(rows))
        self.assertEqual(sorted(rows.values()), sorted(os.listdir(root)))
        for label, run in rows.items():
            summary = json.load(open(f"{root}/{run}/summary.json"))
            self.assertTrue(os.path.exists(f"{root}/{run}/coord-up.log"), run)
            got = json.loads(subprocess.run([sys.executable, f"{HERE}/fleetfield_summary.py", "--all-up", f"{root}/{run}", str(summary["servers"])],
                                            capture_output=True, text=True, check=True).stdout)
            self.assertEqual(got, summary["all_up_s"], run)
            if got is None:
                self.assertTrue(said[label].startswith("never"), (label, said[label]))
            else:
                self.assertEqual(said[label], f"{got['s']} s", label)

    def test_the_documents_figures_for_a_hosts_other_vms_are_the_records(self):
        # docs/fleet/minecraft.md, "What the host needs for hundreds of servers": what a routed
        # VM and a potato ring held, read again from each run's neigh.log.
        root = f"{HERE}/results/2026-11-24-neighbour-entries-other-vms-c8g-metal-48xl"
        doc = " ".join(open(f"{HERE}/../../../docs/fleet/minecraft.md").read().split())

        def rows(run):
            with open(f"{root}/{run}/neigh.log") as f:
                return [dict(pair.split("=") for pair in line.split()[1:]) | {"t": line.split()[0]} for line in f]

        def most(rs, key):
            return max(int(r[key]) for r in rs)

        routed = rows("routed-vms")
        idle = (int(routed[0]["ndisc"]), int(routed[0]["arp"]))
        held = (most(routed, "ndisc"), most(routed, "arp"))
        self.assertEqual(most(routed, "fc"), 32)
        self.assertIn(f"held {held[0]} IPv6 and {held[1]} IPv4 entries over the idle host's {idle[0]} and {idle[1]}: "
                      f"{(held[0] - idle[0]) / 32:.1f} and {(held[1] - idle[1]) / 32:.1f} each", doc)
        long = rows("potato-ring-15min")
        self.assertEqual(most(long, "fc"), 32)
        thirds = [most(long[a * len(long) // 3:(a + 1) * len(long) // 3], "arp") for a in range(3)]
        self.assertIn(f"the count reached {most(long, 'ndisc'):,} IPv6 and {most(long, 'arp'):,} IPv4 entries and levelled off "
                      f"(the most IPv4 seen in each third of the run: {thirds[0]:,}, {thirds[1]:,}, {thirds[2]:,})", doc)
        self.assertEqual([int(long[-1][k]) - int(long[0][k]) for k in ("ndisc_table_fulls", "arp_table_fulls")], [0, 0])
        low = rows("potato-ring-limit-1024")
        first = next(r for r in low if int(r["arp_table_fulls"]) > 0)
        # the ring was started 10 s after the first sample
        self.assertIn(f"{int(first['t']) - int(low[0]['t']) - 10} s after it started: {int(low[-1]['arp_table_fulls'])} entries were refused", doc)
        self.assertEqual((int(low[-1]["ndisc_table_fulls"]), most(low, "ndisc") < 650), (0, True))

    def test_a_run_that_never_all_ran_says_where_it_stood(self):
        with tempfile.TemporaryDirectory() as d:
            with open(f"{d}/run.jsonl", "w") as f:
                f.write(json.dumps({"event": "start", "n": 512}) + "\n")
            with open(f"{d}/samples.jsonl", "w") as f:
                for at, running, failed, silent in ((5.0, 80, 0, 0), (347.0, 500, 10, 325)):
                    f.write(json.dumps({"at": at, "since_all_up": None, "phases": {"running": running, "restoring": 512 - running - failed, "failed": failed},
                                        "tps": {"n": running - silent, "min": 19.9, "below_19_5": 0, "no_answer": silent}, "host": {"busy": 101.7},
                                        "wire": {"snapshots": 1, "snap_bytes": 1, "statuses": 1, "status_bytes": 1}}) + "\n")
            with open(f"{d}/disk.log", "w") as f:
                f.write("100 store_mib=5000 load=1 2 3 fc=0\n")
            with open(f"{d}/host.txt", "w") as f:
                f.write("kernel example\n")
            out = json.loads(subprocess.run([sys.executable, f"{HERE}/fleetfield_summary.py", d], capture_output=True, text=True, check=True).stdout)
        self.assertNotIn("restore_all_s", out)
        self.assertEqual(out["never_all_running"], {"at_s": 347.0, "phases": {"running": 500, "restoring": 2, "failed": 10},
                                                    "running_unanswered": 325, "running_with_a_rate": 175, "host": {"busy": 101.7}})
        # no steady state, so no steady readings
        self.assertEqual((out["samples_all_up"], out["readings"]), (0, 0))
        self.assertNotIn("neighbour_entries_max", out)



if __name__ == "__main__":
    unittest.main()
