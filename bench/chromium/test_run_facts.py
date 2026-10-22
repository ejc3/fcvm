#!/usr/bin/env python3
"""Facts a run records that the benchmark page used to say no record held.

Each class pins one field: the host's instance type and CPU, the memory
server's prefetch setting, the browser build and Firecracker build a golden
ran on, when fcvm's restore milestones reached a clone's log, what each poll
for the page target found, the navigation timing marks that split navigate,
the html-fast arm, and the poll interval. Every test here failed on the
parent commit, which recorded none of them.

Run: python3 -m unittest test_run_facts -v
"""

import argparse
import hashlib
import io
import json
import os
import re
import sys
import tempfile
import tomllib
import types
import unittest
import urllib.error
from contextlib import redirect_stdout
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

import cdpdrive  # noqa: E402
import reqanalyze  # noqa: E402
import reqbench  # noqa: E402
import test_reqbench  # noqa: E402


def write(root, relative, text):
    path = os.path.join(root, relative.lstrip("/"))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as handle:
        handle.write(text)
    return path


class HostFacts(unittest.TestCase):
    """meta.host: the machine without its name, address or account."""

    def lscpu(self, model):
        def run(argv, **_kwargs):
            self.assertEqual(argv, ["lscpu"])
            return types.SimpleNamespace(stdout=f"Architecture: aarch64\nModel name:  {model}\n")
        return run

    def fixture(self, root, vendor):
        write(root, "/sys/class/dmi/id/sys_vendor", vendor + "\n")
        write(root, "/sys/class/dmi/id/product_name", "c8g.metal-48xl\n")
        write(root, "/proc/meminfo", "MemTotal:       395389732 kB\nMemFree: 1 kB\n")
        write(root, "/etc/os-release", 'NAME="Ubuntu"\nPRETTY_NAME="Ubuntu 24.04.4 LTS"\n')

    def test_an_ec2_host_reports_its_instance_type(self):
        with tempfile.TemporaryDirectory() as root:
            self.fixture(root, "Amazon EC2")
            facts = reqbench.host_facts(root, run=self.lscpu("Neoverse-V2"),
                                        cpu_count=lambda: 192)
        self.assertEqual(facts, {
            "instance_type": "c8g.metal-48xl", "cpu_model": "Neoverse-V2", "nproc": 192,
            "mem_total_kb": 395389732, "os": "Ubuntu 24.04.4 LTS",
        })

    def test_another_vendor_reports_no_instance_type(self):
        with tempfile.TemporaryDirectory() as root:
            self.fixture(root, "QEMU")
            facts = reqbench.host_facts(root, run=self.lscpu("x"), cpu_count=lambda: 4)
        self.assertIsNone(facts["instance_type"])

    def test_no_field_names_the_machine(self):
        with tempfile.TemporaryDirectory() as root:
            self.fixture(root, "Amazon EC2")
            facts = reqbench.host_facts(root, run=self.lscpu("x"), cpu_count=lambda: 4)
        self.assertEqual(set(facts), {"instance_type", "cpu_model", "nproc",
                                      "mem_total_kb", "os"})


class FirecrackerIdentity(unittest.TestCase):
    """provenance.firecracker: the binary, and the commit its name encodes."""

    PIN = ('[firecracker]\nrepo = "ejc3/firecracker"\n'
           'branch = "agent/uffd-minor"\ncommit = "' + "8" * 40 + '"\n')

    def test_a_binary_named_for_the_pin_records_the_commit(self):
        name = reqbench.firecracker_build_name(
            "default", "ejc3/firecracker", "agent/uffd-minor", "8" * 40,
            "aarch64", "libc-2.39")
        with tempfile.TemporaryDirectory() as d:
            binary = write(d, name, "firecracker bytes")
            record = reqbench.firecracker_identity(binary, self.PIN, "aarch64", "libc-2.39")
        self.assertEqual(record, {
            "binary": binary, "sha256": hashlib.sha256(b"firecracker bytes").hexdigest(),
            "repo": "ejc3/firecracker", "branch": "agent/uffd-minor", "commit": "8" * 40,
            "commit_matches_binary": True,
        })

    def test_a_binary_built_from_anything_else_records_no_commit(self):
        with tempfile.TemporaryDirectory() as d:
            binary = write(d, "firecracker-default-000000000000.bin", "x")
            record = reqbench.firecracker_identity(binary, self.PIN, "aarch64", "libc-2.39")
        self.assertIsNone(record["commit"])
        self.assertFalse(record["commit_matches_binary"])
        self.assertEqual(record["sha256"], hashlib.sha256(b"x").hexdigest())

    def test_the_known_build_name(self):
        """The binary `fcvm setup` built for rootfs-config.toml's pin on an
        aarch64 Ubuntu 24.04 host (glibc 2.39), observed on 2026-10-22."""
        self.assertEqual(
            reqbench.firecracker_build_name(
                "default", "ejc3/firecracker", "agent/uffd-minor-v1.17.0-discard-dirty",
                "8819f112ad79ad3efb1b4717c1175229fd21b3cb", "aarch64", "libc-2.39"),
            "firecracker-default-0834213edfce.bin")

    def test_the_repository_config_pins_a_commit(self):
        with open(os.path.join(REPO, "rootfs-config.toml"), "rb") as handle:
            pin = tomllib.load(handle)["firecracker"]
        for field in ("repo", "branch", "commit"):
            self.assertIsInstance(pin.get(field), str, field)


class FirecrackerBuildNamePin(unittest.TestCase):
    """firecracker_build_name mirrors src/setup/kernel.rs byte for byte."""

    def test_the_rust_side_names_builds_the_way_the_harness_reads_them(self):
        with open(os.path.join(REPO, "src", "setup", "kernel.rs")) as handle:
            source = handle.read()
        for needle in (
            'hasher.update(b"fcvm-firecracker-cache-v2\\0");',
            "for field in [repo, branch, commit_hash, arch, libc_tag] {",
            "hasher.update((field.len() as u64).to_le_bytes());",
            "hex::encode(&result[..6])",
            'dir.join(format!("firecracker-{profile_name}-{sha}.bin"))',
            'return format!("libc-{}", ver);',
        ):
            self.assertIn(needle, source)


class RestoreMarks(unittest.TestCase):
    """restore_marks: fcvm's restore milestones, in ms from the launch."""

    LOG = (
        "2026-10-22T02:00:00.010000Z  INFO fcvm::commands::common: snapshot load completed "
        "duration_ms=6 track_dirty_pages=false\n"
        "2026-10-22T02:00:00.012500Z  INFO fcvm::commands::common: disk patch completed "
        "duration_ms=1\n"
        "2026-10-22T02:00:00.020000Z  INFO fcvm::commands::common: VM resume completed "
        "duration_ms=7 total_snapshot_ms=14\n"
        "2026-10-22T02:00:00.090000Z  INFO fcvm::commands::common: VM lifecycle ready "
        "vm_id=vm-1\n"
    )

    def launch(self):
        import datetime
        return datetime.datetime.fromisoformat("2026-10-22T02:00:00Z").timestamp()

    def test_each_milestone_is_timed_from_the_launch(self):
        with tempfile.TemporaryDirectory() as d:
            log = write(d, "clone.log", "starting\n" + self.LOG)
            marks = reqbench.restore_marks(log, self.launch())
        self.assertEqual({k: round(v, 3) for k, v in marks.items()}, {
            "snapshot_loaded_ms": 10.0, "disk_patched_ms": 12.5,
            "resumed_ms": 20.0, "lifecycle_ready_ms": 90.0,
        })

    def test_a_missing_milestone_is_null_and_a_missing_log_says_why(self):
        with tempfile.TemporaryDirectory() as d:
            log = write(d, "clone.log", self.LOG.splitlines(True)[0])
            marks = reqbench.restore_marks(log, self.launch())
            missing = reqbench.restore_marks(os.path.join(d, "absent.log"), self.launch())
        self.assertIsNone(marks["resumed_ms"])
        self.assertIn("FileNotFoundError", missing["error"])


class RestoreAck(unittest.TestCase):
    """restore_ack: fcvm's restored record, with fc-agent's per-phase timing.

    fcvm prints one JSON line on stdout once the guest acknowledges the
    restore (restored_record in src/commands/snapshot.rs). Its guest_phases
    split the guest's restore work, which is where a clone spends the time
    between the VM resuming and its DevTools port answering."""

    RECORD = {"status": "restored", "name": "rb-1", "snapshot": "cb", "vm_id": "vm-1",
              "backend": "uffd", "hypervisor": "firecracker", "ack_ms": 367,
              "guest_phases": {"tcp_cleanup_ms": 130.3, "tcp_reopen_ms": 109.9,
                               "tcp_verified": True, "tcp_destroyed": 1, "total_ms": 303.6}}

    def test_the_restored_record_is_read_from_the_log(self):
        with tempfile.TemporaryDirectory() as d:
            log = write(d, "clone.log", RestoreMarks.LOG + json.dumps(self.RECORD) + "\n"
                        + "{not json\n")
            ack = reqbench.restore_ack(log)
        self.assertEqual(ack, {"ack_ms": 367, "guest_phases": self.RECORD["guest_phases"]})

    def test_a_log_without_the_record_says_so(self):
        with tempfile.TemporaryDirectory() as d:
            log = write(d, "clone.log", RestoreMarks.LOG + json.dumps({"status": "serving"}) + "\n")
            ack = reqbench.restore_ack(log)
            missing = reqbench.restore_ack(os.path.join(d, "absent.log"))
        self.assertEqual(set(ack), {"error"})
        self.assertIn("FileNotFoundError", missing["error"])

    def test_both_request_paths_record_it(self):
        """The cdp and noop request functions each set restore_ack beside restore_marks."""
        with open(os.path.join(HERE, "reqbench.py")) as f:
            source = f.read()
        self.assertEqual(source.count('rec["restore_ack"] = restore_ack(log)'),
                         source.count('rec["restore_marks"] = restore_marks(log, t_spawn_wall)'))
        self.assertGreaterEqual(source.count('rec["restore_ack"] = restore_ack(log)'), 2)

    def test_the_restored_record_is_a_println_in_fcvm(self):
        with open(os.path.join(REPO, "src", "commands", "snapshot.rs")) as f:
            source = f.read()
        self.assertIn('"status": "restored"', source)
        self.assertIn('"guest_phases": phases', source)
        self.assertIn('"ack_ms": ack_ms', source)


class RestoreAckExtraction(unittest.TestCase):
    """restore_ack.py: the same reading for runs whose records predate it."""

    def run_dir(self, d, logs):
        rows = [{"kind": "meta", "run_id": "r1"}]
        for i, text in enumerate(logs):
            name = f"rb-r1-{i}-norm.log"
            if text is not None:
                write(d, name, text)
            rows.append({"record_id": f"r1:cdp:{i}:0", "arm": "cdp", "rep": i, "warmup": False,
                         "log": f"/elsewhere/{name}"})
        write(d, "reqbench.jsonl", "".join(json.dumps(r) + "\n" for r in rows))

    def test_every_record_gets_its_log_hash_and_reading(self):
        import restore_ack
        good = RestoreMarks.LOG + json.dumps(RestoreAck.RECORD) + "\n"
        with tempfile.TemporaryDirectory() as d:
            self.run_dir(d, [good, RestoreMarks.LOG])
            with redirect_stdout(io.StringIO()):
                self.assertEqual(restore_ack.main(["restore_ack.py", d]), 0)
            with open(os.path.join(d, "restore-ack.jsonl")) as f:
                lines = [json.loads(line) for line in f]
            with open(os.path.join(d, "reqbench.jsonl"), "rb") as f:
                source = hashlib.sha256(f.read()).hexdigest()
        self.assertEqual(lines[0]["source_sha256"], source)
        self.assertEqual([x["record_id"] for x in lines[1:]], ["r1:cdp:0:0", "r1:cdp:1:0"])
        self.assertEqual(lines[1]["restore_ack"]["guest_phases"]["tcp_cleanup_ms"], 130.3)
        self.assertEqual(lines[1]["log_sha256"], hashlib.sha256(good.encode()).hexdigest())
        self.assertIn("error", lines[2]["restore_ack"])

    def test_a_missing_log_writes_nothing(self):
        import restore_ack
        with tempfile.TemporaryDirectory() as d:
            self.run_dir(d, [RestoreMarks.LOG, None])
            with redirect_stdout(io.StringIO()), mock.patch("sys.stderr", io.StringIO()):
                self.assertEqual(restore_ack.main(["restore_ack.py", d]), 2)
            self.assertFalse(os.path.exists(os.path.join(d, "restore-ack.jsonl")))


class MemoryRenderOperation(unittest.TestCase):
    """The memory phase renders each fcvm clone with a chosen operation.

    Cloudflare publishes memory for a screenshot and for HTML extraction; the
    memory phase rendered every clone with a screenshot, with no way to ask for
    the other. Its host containers render with the image's render.py, which
    takes the screenshot and the DOM both, and the record says so."""

    def test_the_driver_cli_takes_the_operation(self):
        seen = {}

        def drive(args):
            seen["op"] = args.op
            return {"ok": True}

        argv = ["cdpdrive.py", "127.0.0.1:9222", "https://example.com/", "--op", "html"]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(cdpdrive, "drive", drive), \
                redirect_stdout(io.StringIO()):
            self.assertEqual(cdpdrive.main(), 0)
        self.assertEqual(seen["op"], "html")

    def test_the_memory_render_passes_it_to_the_driver(self):
        import corpus_mem
        calls = []

        def sh(cmd, **_kw):
            calls.append(cmd)
            return types.SimpleNamespace(returncode=0, stdout="{}", stderr="")

        with mock.patch.object(corpus_mem, "sh", sh):
            corpus_mem.render("127.0.0.1:9222", "https://example.com/", op="html")
            corpus_mem.render("127.0.0.1:9222", "https://example.com/")
        self.assertEqual(calls[0][-2:], ["--op", "html"])
        self.assertEqual(calls[1][-2:], ["--op", "screenshot"])

    def test_the_phase_records_and_forwards_it(self):
        with open(os.path.join(HERE, "corpus_mem.py")) as f:
            mem = f.read()
        with open(os.path.join(HERE, "corpus_extra.sh")) as f:
            extra = f.read()
        self.assertIn('parser.add_argument("--op", choices=("screenshot", "html"), default="screenshot")', mem)
        self.assertIn('"render_op": args.op', mem)
        self.assertIn('render(c["endpoint"], url, op=self.args.op)', mem)
        self.assertIn('"container_render": ', mem)
        self.assertIn('MEM_OP="${MEM_OP:-screenshot}"', extra)
        self.assertIn('--op "$MEM_OP"', extra)


class MemoryContainerRemoval(unittest.TestCase):
    """A pool container is removed without podman's SIGTERM grace.

    Chromium in the pool container does not exit on SIGTERM, so `podman rm -f`
    waited its default 10 s stop timeout for every container, one after another
    (480 s to clear a 48-container cell), and on 2026-10-22 one removal on a
    loaded host took 72 s from the rm to the remove event, past the 30 s bound,
    which failed the memory phase after 110 of its 140 cells. The container's
    state is never read after it is sampled, so it is killed at once."""

    def test_rm_does_not_wait_for_sigterm(self):
        import corpus_mem
        calls = []

        def bounded(cmd, timeout):
            calls.append((cmd, timeout))
            rc = 1 if cmd[:3] == ["podman", "container", "exists"] else 0
            return types.SimpleNamespace(returncode=rc, stdout="", stderr="")

        with mock.patch.object(corpus_mem, "inspected_container_identity", lambda name: ("a" * 64, "tok")), \
                mock.patch.object(corpus_mem, "sh_bounded", bounded):
            corpus_mem.remove_owned_container("c", "tok")
        rm = [cmd for cmd, _t in calls if cmd[:2] == ["podman", "rm"]]
        self.assertEqual(len(rm), 1)
        self.assertIn("--time=0", rm[0])


class MemoryCloneGone(unittest.TestCase):
    """clone_gone lists the state directory, then reads each file. Another
    clone's fcvm deletes its own state file as it exits, so a file can vanish
    between the listing and the read; on 2026-10-22 that raised
    CloneStateReadError and failed the memory phase after 101 of its 140
    cells. A file that no longer exists holds no clone, so it is skipped; any
    other unreadable file still fails closed."""

    def test_a_state_file_deleted_after_the_listing_is_not_a_clone(self):
        import corpus_mem
        with tempfile.TemporaryDirectory() as d:
            gone = os.path.join(d, "vm-gone.json")
            other = write(d, "vm-other.json", json.dumps({"name": "someone-else"}))
            with mock.patch.object(corpus_mem, "state_files", lambda _d: [gone, other]):
                self.assertTrue(corpus_mem.clone_gone(d, "mine"))

    def test_an_unreadable_state_file_still_fails_closed(self):
        import corpus_mem
        with tempfile.TemporaryDirectory() as d:
            bad = write(d, "vm-bad.json", "{not json")
            with mock.patch.object(corpus_mem, "state_files", lambda _d: [bad]):
                with self.assertRaises(corpus_mem.CloneStateReadError):
                    corpus_mem.clone_gone(d, "mine")


class ScaleCleanTree(unittest.TestCase):
    """The scale run refuses a dirty source tree, so that what it measured is
    committed. It counted the run's own outputs as dirt: the corpus campaign
    writes the run's results under bench/chromium/results/ before the scale
    phase starts, so `make bench-chromium-corpus MEASURE=scale` refused every
    time ("benchmark source tree is dirty", 2026-10-22). Untracked files under
    bench/chromium/results/ are outputs, not harness; anything else still
    refuses."""

    def repo(self, d):
        import subprocess
        for args in (["init", "-q"], ["config", "user.email", "t@example.test"],
                     ["config", "user.name", "t"]):
            subprocess.run(["git", "-C", d] + args, check=True)
        write(d, "bench/chromium/reqbench.py", "x = 1\n")
        subprocess.run(["git", "-C", d, "add", "-A"], check=True)
        subprocess.run(["git", "-C", d, "commit", "-q", "-m", "c"], check=True)

    def test_results_outputs_are_not_dirt(self):
        import reqscale
        with tempfile.TemporaryDirectory() as d:
            self.repo(d)
            write(d, "bench/chromium/results/corpus-x/reqbench/analysis.json", "{}")
            self.assertEqual(reqscale.source_tree_dirt(d), "")

    def test_an_untracked_harness_file_is_dirt(self):
        import reqscale
        with tempfile.TemporaryDirectory() as d:
            self.repo(d)
            write(d, "bench/chromium/new_driver.py", "y = 2\n")
            self.assertIn("new_driver.py", reqscale.source_tree_dirt(d))

    def test_a_modified_harness_file_is_dirt(self):
        import reqscale
        with tempfile.TemporaryDirectory() as d:
            self.repo(d)
            write(d, "bench/chromium/reqbench.py", "x = 3\n")
            self.assertIn("reqbench.py", reqscale.source_tree_dirt(d))


class RestoreMarkSourcePin(unittest.TestCase):
    """Every marker is an info! line fcvm writes on the restore path."""

    def test_each_marker_is_an_info_line_in_fcvm(self):
        with open(os.path.join(REPO, "src", "commands", "common.rs")) as handle:
            source = handle.read()
        for _key, message in reqbench.RESTORE_MARKS:
            match = re.search(r'info!\((?:[^;]|\n)*?"' + re.escape(message) + '"', source)
            self.assertIsNotNone(match, f"no info! line carries {message!r}")
            self.assertNotIn("debug!(", match.group(0))

    def test_the_runs_log_level_keeps_info(self):
        with open(os.path.join(HERE, "reqbench.sh")) as handle:
            self.assertIn('FCVM_LOG must select fcvm=debug or fcvm=trace', handle.read())


class ResolvePolls(unittest.TestCase):
    """render.resolve_polls: what each /json/list poll found, and when."""

    class Response(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *_exc):
            return False

    def test_each_poll_records_its_outcome(self):
        answers = [
            urllib.error.URLError(ConnectionRefusedError(111, "refused")),
            [{"type": "browser"}],
            [{"type": "page", "url": "about:blank", "webSocketDebuggerUrl": "ws://x/p"}],
        ]

        def urlopen(_request, timeout):
            answer = answers.pop(0)
            if isinstance(answer, Exception):
                raise answer
            return self.Response(json.dumps(answer).encode())

        stats = {}
        with mock.patch.object(cdpdrive.urllib.request, "urlopen", urlopen):
            target = cdpdrive.resolve_target("127.0.0.1:9", 1e12, 10, "", stats, poll_s=0.0)
        self.assertEqual(target["webSocketDebuggerUrl"], "ws://x/p")
        self.assertEqual([p["outcome"] for p in stats["resolve_polls"]],
                         ["os-ECONNREFUSED", "no-page", "page"])
        times = [p["t_ms"] for p in stats["resolve_polls"]]
        self.assertEqual(times, sorted(times))

    def test_the_poll_interval_is_the_one_asked_for(self):
        slept = []

        def urlopen(_request, timeout):
            raise urllib.error.URLError(ConnectionRefusedError(111, "refused"))

        with mock.patch.object(cdpdrive.urllib.request, "urlopen", urlopen), \
                mock.patch.object(cdpdrive.time, "sleep", slept.append):
            with self.assertRaises(ConnectionError):
                cdpdrive.resolve_target("127.0.0.1:9", 1e12, 3, "", {}, poll_s=0.005)
        self.assertEqual(slept, [0.005, 0.005])

    def test_drive_passes_the_run_s_interval(self):
        seen = {}

        def resolve(host, deadline, retries, header, stats, poll_s):
            seen["poll_s"] = poll_s
            raise ConnectionError("stop here")

        args = argparse.Namespace(
            cdp_host="127.0.0.1:9", url="http://x/", format="jpeg", quality=80,
            timeout=5.0, idle_wait_ms=0.0, out_prefix="", ws_url="", connect_retries=1,
            nav_timing=True, print_target=False, host_header="", op="screenshot",
            resolve_poll_ms=5.0, render_module=os.path.join(HERE, "render.py"))
        with mock.patch.object(cdpdrive, "resolve_target", resolve):
            cdpdrive.drive(args)
        self.assertEqual(seen["poll_s"], 0.005)


class NavMarks(unittest.TestCase):
    """render.nav carries the marks that split navigate."""

    def test_the_marks_are_ms_from_the_navigation_start(self):
        entry = {"startTime": 0, "responseStart": 20.0, "requestStart": 5.0,
                 "responseEnd": 30.5, "domInteractive": 80.0,
                 "domContentLoadedEventEnd": 95.0, "loadEventStart": 210.0,
                 "loadEventEnd": 211.0}

        class Cdp:
            def cmd(self, method, params, deadline):
                return {"result": {"value": json.dumps(entry)}}

        nav = cdpdrive.nav_timing(Cdp(), 1e12)
        self.assertEqual(
            {k: nav[k] for k in ("response_end_ms", "dom_interactive_ms",
                                 "dom_content_loaded_end_ms", "load_event_start_ms")},
            {"response_end_ms": 30.5, "dom_interactive_ms": 80.0,
             "dom_content_loaded_end_ms": 95.0, "load_event_start_ms": 210.0})
        self.assertEqual(nav["ttfb_ms"], 15.0)


class RunMetaFacts(unittest.TestCase):
    """main() stamps the facts into the run's meta line."""

    SNAPSHOT = test_reqbench.ServeAcknowledgement.SNAPSHOT
    SEED = test_reqbench.ServeAcknowledgement.SEED
    FakeServer = test_reqbench.ServeAcknowledgement.FakeServer
    _drive_main = test_reqbench.ServeAcknowledgement._drive_main

    def meta(self, prefetch):
        with tempfile.TemporaryDirectory() as d:
            server = self.FakeServer(os.path.join(d, "serve.log"), prefetch=prefetch)
            facts = {"instance_type": "c8g.metal-48xl", "cpu_model": "Neoverse-V2",
                     "nproc": 192, "mem_total_kb": 1, "os": "Ubuntu"}
            self._drive_main(d, server, extra_patches={"host_facts": lambda: facts})
            with open(os.path.join(d, "results", "reqbench.jsonl")) as handle:
                return json.loads(handle.readline()), facts

    def test_the_meta_names_the_host_prefetch_and_poll_interval(self):
        for prefetch, word in ((True, "on"), (False, "off")):
            with self.subTest(prefetch=word):
                meta, facts = self.meta(prefetch)
                self.assertEqual(meta["uffd_prefetch"], word)
                self.assertEqual(meta["host"], facts)
                self.assertEqual(meta["resolve_poll_ms"], 50.0)
                self.assertFalse(meta["hugepages"])
                for field in ("browser_version", "firecracker"):
                    self.assertIn(field, meta)

    def test_the_snapshot_provenance_reaches_the_meta(self):
        with tempfile.TemporaryDirectory() as d:
            test_reqbench.SnapshotGenerationIdentity._write_generation(
                d, "44444444-4444-4444-8444-444444444444")
            path = os.path.join(d, "snapshots", self.SNAPSHOT, "reqbench-provenance.json")
            with open(path) as handle:
                provenance = json.load(handle)
            provenance["browser_version"] = "Chromium 151.0.0.0 fixture"
            provenance["firecracker"] = {"commit": "8" * 40}
            with open(path, "w") as handle:
                json.dump(provenance, handle)
            snapshot = reqbench.snapshot_generation(d, self.SNAPSHOT)
        self.assertEqual(snapshot["browser_version"], "Chromium 151.0.0.0 fixture")
        self.assertEqual(snapshot["firecracker"], {"commit": "8" * 40})


class AnalysisCarriesTheFacts(unittest.TestCase):
    """analysis.json: the cell carries the facts, and the arms their timeline."""

    _run_gate_fixture = test_reqbench.AnalyzerAvailability._run_gate_fixture
    _fast_median_ci = staticmethod(test_reqbench.AnalyzerAvailability._fast_median_ci)
    _fast_shift = staticmethod(test_reqbench.AnalyzerAvailability._fast_shift)

    FACTS = {
        "uffd_prefetch": "on", "browser_version": "Chromium 151.0.0.0 fixture",
        "firecracker": {"commit": "8" * 40}, "hugepages": False,
        "host": {"instance_type": "c8g.metal-48xl"}, "resolve_poll_ms": 50.0,
    }

    def analyze(self, d, mutate=None, **meta):
        src = os.path.join(d, "r.jsonl")
        dst = os.path.join(d, "r.json")
        test_reqbench.AnalyzerAvailability._write_clean_backend(src, "uffd", 6, 372.0, **meta)
        if mutate is not None:
            with open(src) as source:
                rows = [json.loads(line) for line in source]
            mutate(rows)
            with open(src, "w") as target:
                for row in rows:
                    target.write(json.dumps(row) + "\n")
        with redirect_stdout(io.StringIO()):
            rc = self._run_gate_fixture(["--json-out", dst, src])
        if not os.path.exists(dst):
            return rc, None
        with open(dst) as handle:
            return rc, json.load(handle)

    def cell(self, analysis):
        cells = [value for key, value in analysis.items() if key in ("cell", "cells")]
        if cells and isinstance(cells[0], dict):
            return cells[0]
        if cells and isinstance(cells[0], list):
            return cells[0][0]
        self.fail(f"no cell in {sorted(analysis)}")

    def test_the_cell_carries_the_recorded_facts(self):
        with tempfile.TemporaryDirectory() as d:
            _rc, analysis = self.analyze(d, **self.FACTS)
        cell = self.cell(analysis)
        for field, value in self.FACTS.items():
            self.assertEqual(cell.get(field), value, field)

    def test_a_uffd_run_without_its_prefetch_setting_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            errors = self._errors(d, dict(self.FACTS, uffd_prefetch=None))
        self.assertTrue(any("uffd_prefetch" in e for e in errors), errors)

    def test_a_malformed_fact_is_refused(self):
        for field, bad in (("uffd_prefetch", "yes"), ("hugepages", "no"),
                           ("host", "box"), ("resolve_poll_ms", 0)):
            with self.subTest(field), tempfile.TemporaryDirectory() as d:
                errors = self._errors(d, dict(self.FACTS, **{field: bad}))
                self.assertTrue(any(field in e for e in errors), errors)

    def _errors(self, d, meta):
        src = os.path.join(d, "r.jsonl")
        test_reqbench.AnalyzerAvailability._write_clean_backend(src, "uffd", 6, 372.0, **meta)
        return [e for ds in reqanalyze.load([src]) for e in ds["metadata_errors"]]

    def test_the_timeline_summarises_marks_polls_and_navigation(self):
        def stamp(rows):
            for row in rows:
                if row.get("kind") == "meta":
                    continue
                row["restore_marks"] = {"snapshot_loaded_ms": 10.0, "resumed_ms": 20.0}
                render = row.get("render")
                if render is not None:
                    render["resolve_polls"] = [
                        {"t_ms": 0.5, "outcome": "os-ECONNREFUSED"},
                        {"t_ms": 51.0, "outcome": "no-page"},
                        {"t_ms": 102.0, "outcome": "page"}]
                    render["nav"].update(response_end_ms=30.0, dom_interactive_ms=80.0,
                                         dom_content_loaded_end_ms=95.0,
                                         load_event_start_ms=210.0)
        with tempfile.TemporaryDirectory() as d:
            _rc, analysis = self.analyze(d, stamp, **self.FACTS)
        cdp = analysis["arms"]["cdp"]["timeline"]
        self.assertEqual(cdp["restore_marks_ms"]["resumed_ms"]["median"], 20.0)
        polls = cdp["resolve_polls"]
        self.assertEqual(polls["first_page_poll_ms"]["median"], 102.0)
        self.assertEqual(polls["last_miss_poll_ms"]["median"], 51.0)
        self.assertEqual(polls["polls_per_request"]["median"], 3)
        self.assertEqual(set(polls["outcomes"]), {"os-ECONNREFUSED", "no-page", "page"})
        self.assertEqual(cdp["nav_marks_ms"]["load_event_start_ms"]["median"], 210.0)
        self.assertEqual(analysis["arms"]["noop"]["timeline"]["restore_marks_ms"]
                         ["snapshot_loaded_ms"]["median"], 10.0)

    def test_the_timeline_summarises_the_guest_restore_phases(self):
        def stamp(rows):
            for row in rows:
                if row.get("kind") == "meta":
                    continue
                row["restore_ack"] = {"ack_ms": 300, "guest_phases": {
                    "tcp_cleanup_ms": 150.0, "tcp_reopen_ms": 90.0, "tcp_verified": True,
                    "tcp_destroyed": 1, "total_ms": 290.0}}
        with tempfile.TemporaryDirectory() as d:
            _rc, analysis = self.analyze(d, stamp, **self.FACTS)
        for armname in ("cdp", "noop"):
            timeline = analysis["arms"][armname]["timeline"]
            self.assertEqual(timeline["restore_ack_ms"]["median"], 300.0)
            phases = timeline["guest_phases_ms"]
            self.assertEqual(set(phases), {"tcp_cleanup_ms", "tcp_reopen_ms", "total_ms"})
            self.assertEqual(phases["tcp_cleanup_ms"]["median"], 150.0)

    def test_html_fast_publishes_cpu_like_cdp_fast(self):
        """html-fast: the html op with the fast teardown, so its CPU is read."""
        def to_html_fast(rows):
            for row in rows:
                if row.get("kind") == "meta":
                    row["arms"] = ["html-fast" if a == "cdp-fast" else a for a in row["arms"]]
                    continue
                if row["arm"] != "cdp-fast":
                    continue
                row["arm"] = "html-fast"
                row["record_id"] = row["record_id"].replace(":cdp-fast:", ":html-fast:")
                render = row["render"]
                for key in ("image_bytes", "image_sha256", "width", "height", "quality"):
                    render.pop(key)
                render.update(html_bytes=4096, html_sha256="7" * 64)
                stages = render["stages"]
                stages.pop("screenshot_ms")
                stages.pop("decode_ms")
                stages["extract_ms"] = 2.0
                row["teardown"].update(
                    lifetime_cpu_ms_by_child={"fcvm": 40.0, "firecracker": 600.0,
                                              "pasta": 30.0},
                    reaped_children_cpu_ms_by_child={"fcvm": 5.0, "firecracker": 0.0,
                                                     "pasta": 0.0},
                    per_child_cpu={"fcvm": {"reclaim_cpu_ms": 0.0, "complete": True},
                                   "firecracker": {"reclaim_cpu_ms": 110.0, "complete": True},
                                   "pasta": {"reclaim_cpu_ms": 0.0, "complete": True}},
                    tick_ms=10.0)
        with tempfile.TemporaryDirectory() as d:
            rc, analysis = self.analyze(d, to_html_fast, **self.FACTS)
        self.assertIsNotNone(analysis, f"the analyzer refused html-fast (rc={rc})")
        self.assertEqual(analysis["metadata_errors"], [])
        block = analysis["arms"]["html-fast"]["request_cpu_ms"]
        self.assertEqual(block["clone_mean_ms"]["mean"], 670.0 + 5.0 + 110.0)
        self.assertIn("extract_ms", analysis["arms"]["html-fast"])


    def test_noop_fast_publishes_the_cpu_of_a_restore_without_a_render(self):
        def to_noop_fast(rows):
            for row in rows:
                if row.get("kind") == "meta":
                    row["arms"] = ["noop-fast" if a == "cdp-fast" else a for a in row["arms"]]
                    continue
                if row["arm"] != "cdp-fast":
                    continue
                row["arm"] = "noop-fast"
                row["record_id"] = row["record_id"].replace(":cdp-fast:", ":noop-fast:")
                for key in ("render", "endpoint", "state_to_port_ms"):
                    row.pop(key, None)
                row["teardown"].update(
                    lifetime_cpu_ms_by_child={"fcvm": 40.0, "firecracker": 200.0,
                                              "pasta": 30.0},
                    reaped_children_cpu_ms_by_child={"fcvm": 5.0, "firecracker": 0.0,
                                                     "pasta": 0.0},
                    per_child_cpu={"fcvm": {"reclaim_cpu_ms": 0.0, "complete": True},
                                   "firecracker": {"reclaim_cpu_ms": 110.0, "complete": True},
                                   "pasta": {"reclaim_cpu_ms": 0.0, "complete": True}},
                    tick_ms=10.0)
        with tempfile.TemporaryDirectory() as d:
            rc, analysis = self.analyze(d, to_noop_fast, **self.FACTS)
        self.assertIsNotNone(analysis, f"the analyzer refused noop-fast (rc={rc})")
        self.assertEqual(analysis["metadata_errors"], [])
        block = analysis["arms"]["noop-fast"]["request_cpu_ms"]
        self.assertEqual(block["clone_mean_ms"]["mean"], 270.0 + 5.0 + 110.0)
        self.assertTrue(analysis["drift"]["evaluated"],
                        "the drift control is still the noop arm")

        def stale(rows):
            to_noop_fast(rows)
            next(r for r in rows if r.get("arm") == "noop-fast")["teardown"].pop(
                "accounting_version")
        with tempfile.TemporaryDirectory() as d:
            _rc, analysis = self.analyze(d, stale, **self.FACTS)
        self.assertTrue(any("stale accounting" in e for e in analysis["metadata_errors"]),
                        "a noop-fast teardown escaped the fast-teardown checks")


    def test_cpu_is_read_under_the_comm_names_fcvm_s_binaries_carry(self):
        """teardown_fast keys children by /proc comm. fcvm runs
        content-addressed binaries, so Firecracker shows as "firecracker-def"
        and pasta as "pasta-<hash>", and the holder as "sleep". The reducer
        looked for the key "firecracker" and withheld every figure of the
        first fast-arm corpus run (202 of 202 requests "without a complete
        reading")."""
        def real_names(rows):
            for row in rows:
                if row.get("arm") != "cdp-fast":
                    continue
                row["teardown"].update(
                    lifetime_cpu_ms_by_child={"fcvm": 50.0, "firecracker-def": 1690.0,
                                              "pasta-4707649d2": 20.0, "sleep": 0.0},
                    reaped_children_cpu_ms_by_child={"fcvm": 230.0, "firecracker-def": 0.0,
                                                     "pasta-4707649d2": 0.0, "sleep": 0.0},
                    per_child_cpu={name: {"reclaim_cpu_ms": cpu, "complete": True}
                                   for name, cpu in (("fcvm", 40.0), ("firecracker-def", 0.0),
                                                     ("pasta-4707649d2", 0.0), ("sleep", 0.0))},
                    tick_ms=10.0)
        with tempfile.TemporaryDirectory() as d:
            _rc, analysis = self.analyze(d, real_names, **self.FACTS)
        block = analysis["arms"]["cdp-fast"]["request_cpu_ms"]
        self.assertEqual(block["missing_records"], 0, block)
        self.assertEqual(block["clone_mean_ms"]["mean"], 50.0 + 1690.0 + 20.0 + 230.0 + 40.0)
        self.assertEqual(block["by_child_at_kill_mean_ms"],
                         {"fcvm": 50.0, "firecracker": 1690.0, "pasta": 20.0, "holder": 0.0})


class HtmlFastArm(unittest.TestCase):
    """reqbench routes html-fast to the html op with the fast teardown."""

    def test_dispatch(self):
        seen = []

        def run(args, rep, fast, probe=None, op="screenshot"):
            seen.append((fast, op))
            return {}

        with mock.patch.object(reqbench, "run_cdp_request", run):
            for arm in ("cdp", "cdp-fast", "html", "html-fast"):
                reqbench.dispatch_request(argparse.Namespace(), 0, arm, False)
        self.assertEqual(seen, [(False, "screenshot"), (True, "screenshot"),
                                (False, "html"), (True, "html")])
        noops = []
        with mock.patch.object(reqbench, "run_noop_request",
                               lambda args, rep, fast=False: noops.append(fast) or {}):
            for arm in ("noop", "noop-fast"):
                reqbench.dispatch_request(argparse.Namespace(), 0, arm, False)
        self.assertEqual(noops, [False, True])
        self.assertIn("html-fast", reqbench.allowed_arms_for_engine("chromium"))
        self.assertNotIn("html-fast", reqbench.allowed_arms_for_engine("webkit"))
        self.assertTrue(reqbench.publication_arms_ok({"noop", "html-fast"}))


class Subreaper(unittest.TestCase):
    """A fast teardown reads every child's final CPU, then collects it.

    Each scenario runs in a child Python, so this test runner never becomes a
    subreaper. The fake fcvm forks two children armed with PR_SET_PDEATHSIG,
    as fcvm arms Firecracker, the holder and pasta; a SIGKILL to it kills them.
    """

    SCRIPT = r"""
import ctypes, json, os, signal, subprocess, sys, time
sys.path.insert(0, sys.argv[1])
import reqbench
d = sys.argv[2]
assert reqbench.become_child_subreaper()
fake = (
    "import ctypes, os, signal, time\n"
    "libc = ctypes.CDLL('libc.so.6')\n"
    "for _ in range(2):\n"
    "    if os.fork() == 0:\n"
    "        libc.prctl(1, signal.SIGKILL)\n"
    "        end = time.monotonic() + 0.05\n"
    "        while time.monotonic() < end: pass\n"
    "        os.execvp('sleep', ['sleep', '300'])\n"
    "time.sleep(300)\n"
)
vm_id = "vm-" + "1" * 32
state = os.path.join(d, vm_id + ".json")
data = os.path.join(d, "vm-disks", vm_id)
os.makedirs(os.path.join(data, "disks"))
with open(state, "w") as f:
    json.dump({"vm_id": vm_id}, f)
proc = subprocess.Popen([sys.executable, "-c", fake])
deadline = time.monotonic() + 10
while len(reqbench.children_of(proc.pid)) < 2 and time.monotonic() < deadline:
    time.sleep(0.01)
time.sleep(0.2)  # let the children exec sleep
out = reqbench.teardown_fast(proc.pid, d, state, data, 5.0)
proc.wait()
me = os.getpid()
zombies = []
for entry in os.listdir("/proc"):
    if entry.isdigit():
        try:
            fields = open(f"/proc/{entry}/stat").read().rsplit(")", 1)[1].split()
        except OSError:
            continue
        if fields[0] == "Z" and int(fields[1]) == me:
            zombies.append(int(entry))
print(json.dumps({
    "complete": {k: v["complete"] for k, v in out["per_child_cpu"].items()},
    "reaped": out.get("children_reaped"),
    "zombies": zombies,
}))
"""

    def run_scenario(self):
        import subprocess
        with tempfile.TemporaryDirectory() as d:
            result = subprocess.run([sys.executable, "-c", self.SCRIPT, HERE, d],
                                    capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr[-2000:])
        return json.loads(result.stdout.strip().splitlines()[-1])

    def test_every_child_reading_is_complete_and_no_zombie_is_left(self):
        for attempt in range(5):
            with self.subTest(attempt=attempt):
                got = self.run_scenario()
                self.assertEqual(len(got["complete"]), 3, got)
                self.assertTrue(all(got["complete"].values()), got)
                self.assertEqual(sorted(got["reaped"].values()), ["reaped", "reaped"], got)
                self.assertEqual(got["zombies"], [], got)

    def test_main_becomes_the_subreaper_and_says_so(self):
        calls = []
        with tempfile.TemporaryDirectory() as d:
            server = test_reqbench.ServeAcknowledgement.FakeServer(os.path.join(d, "serve.log"))
            RunMetaFacts._drive_main(
                RunMetaFacts("test_the_meta_names_the_host_prefetch_and_poll_interval"),
                d, server, extra_patches={"become_child_subreaper": lambda: calls.append(1) or True})
            with open(os.path.join(d, "results", "reqbench.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
        self.assertEqual(calls, [1])
        self.assertIs(rows[0]["child_subreaper"], True)
        self.assertTrue(all(row.get("adopted_zombies_reaped") == 0 for row in rows[1:]), rows[1:])


class ContentAddressedFirecracker(unittest.TestCase):
    """faultbench finds Firecracker under its content-addressed comm."""

    def test_faultbench_finds_firecracker_def(self):
        import faultbench
        with tempfile.TemporaryDirectory() as proc:
            for pid, comm in ((11, "firecracker-def"), (12, "pasta-4707649d2"), (13, "bash")):
                write(proc, f"/{pid}/comm", comm + "\n")
            self.assertEqual(faultbench.firecracker_pids(proc), {11})


class RequestBundleManifestPin(unittest.TestCase):
    """corpus_extra.sh seals reqbench's runtime exactly as reqbench.sh does.

    The memory phase refuses a golden whose creator_runtime_bundle_sha256 is
    not the bundle it staged itself. reqbench.sh added the scale harness to its
    sealed bundle (418b7683) and corpus_extra.sh kept the old list, so every
    golden reqbench.sh made after that was refused: "snapshot
    creator_runtime_bundle_sha256 ... does not match current staged runtime".
    """

    def test_both_manifests_list_the_same_files_in_the_same_order(self):
        with open(os.path.join(HERE, "reqbench.sh")) as handle:
            sh = handle.read()
        sealed = ["fcvm", "fc-agent"] + re.search(
            r"runtime_sources=\(([^)]+)\)", sh).group(1).split()
        with open(os.path.join(HERE, "corpus_extra.sh")) as handle:
            extra = handle.read()
        block = re.search(r"sha256sum fcvm fc-agent (.*?)\) > \"\$reqbench_manifest_temp\"",
                          extra, re.S).group(0)
        listed = re.sub(r"#[^\n]*", "", block).replace("\\\n", " ").split()
        listed = listed[1:listed.index(")")]
        self.assertEqual(listed, sealed)
        staged = re.search(r"local sources=\(([^)]+)\)", extra).group(1).split()
        self.assertEqual(sorted(set(sealed[2:]) - set(staged)), [],
                         "corpus_extra.sh hashes a reqbench source it never staged")


class MemoryPoolHealthcheck(unittest.TestCase):
    """The memory phase's host containers run without the image's healthcheck."""

    def test_pool_containers_are_created_without_healthchecks(self):
        import subprocess
        import corpus_mem
        args = argparse.Namespace(image="image",
                                  urls=["https://example.com/", "https://example.org/"],
                                  container_owner_token="c" * 32)
        side = corpus_mem.ContainerSide(args, "a" * 32)
        created = []

        def shell(cmd, *_args, **_kwargs):
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return subprocess.CompletedProcess(cmd, 0, "c" * 64 + " " + "c" * 32 + "\n", "")
            return subprocess.CompletedProcess(cmd, 0, "c" * 64 + "\n", "")

        def create(command, *_args):
            created.append(command)
            return None, subprocess.CompletedProcess(command, 0, "c" * 64 + "\n", "")

        with mock.patch.object(corpus_mem, "sh", shell), \
                mock.patch.object(corpus_mem, "sh_bounded", shell), \
                mock.patch.object(corpus_mem, "start_container_create", create):
            side.bring_up(2, "host2r1", [0, 1])
        self.assertEqual(len(created), 2)
        for command in created:
            self.assertEqual(command[:3], ["podman", "create", "--no-healthcheck"])


class NonDumpablePss(unittest.TestCase):
    """report.py reads a non-dumpable process's PSS (pasta's) through sudo."""

    def test_a_refused_read_is_retried_with_sudo(self):
        import builtins
        import subprocess
        import report
        real_open = builtins.open

        def refusing_open(path, *args, **kwargs):
            if str(path) == "/proc/4242/smaps_rollup":
                raise PermissionError(13, "Permission denied", path)
            return real_open(path, *args, **kwargs)

        calls = []

        def run(argv, **_kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, 0, "Rss: 900 kB\nPss: 812 kB\n", "")

        with mock.patch("builtins.open", refusing_open), \
                mock.patch.object(report.subprocess, "run", run):
            self.assertEqual(report.pss_kb_of_pid(4242), 812)
        self.assertEqual(calls, [["sudo", "-n", "cat", "/proc/4242/smaps_rollup"]])

    def test_a_failed_sudo_read_still_refuses(self):
        import builtins
        import subprocess
        import report
        real_open = builtins.open

        def refusing_open(path, *args, **kwargs):
            if str(path) == "/proc/4242/smaps_rollup":
                raise PermissionError(13, "Permission denied", path)
            return real_open(path, *args, **kwargs)

        with mock.patch("builtins.open", refusing_open), \
                mock.patch.object(report.subprocess, "run",
                                  lambda argv, **_k: subprocess.CompletedProcess(
                                      argv, 1, "", "a password is required")):
            with self.assertRaises(report.CgroupReadError):
                report.pss_kb_of_pid(4242)


class MovingProcessSet(unittest.TestCase):
    """A memory read whose process set moved is taken again, a bounded number of times."""

    def read(self, sets):
        import report
        sets = list(sets)
        with tempfile.TemporaryDirectory() as cg, \
                mock.patch.object(report, "cgroup_procs", lambda _p: sets.pop(0) if len(sets) > 1 else sets[0]), \
                mock.patch.object(report, "cgroup_bytes", lambda _p: 4096), \
                mock.patch.object(report, "pss_kb_of_pid", lambda pid: 10), \
                mock.patch.object(report, "STABLE_READ_PAUSE_S", 0.0):
            return report.measure_complete_cgroup(cg, with_stat=False)

    def test_a_health_check_that_comes_and_goes_is_read_around(self):
        # before/after of attempt 1 differ (a health-check exec appeared);
        # attempt 2 is stable.
        procs, current, pss, _stat = self.read([[1, 2, 3, 4], [1, 2, 3, 4, 5], [1, 2, 3, 4]])
        self.assertEqual((sorted(procs), current, pss), ([1, 2, 3, 4], 4096, 40))

    def test_a_process_that_exits_mid_read_is_read_around(self):
        """RED BEFORE THE FIX: a health check that exited between the set read
        and its smaps_rollup read left a zombie in the set, and the read
        failed with ESRCH: "cannot read /proc/1081429/smaps_rollup: [Errno 3]
        No such process" (2026-10-22, 48 clones, after 102 of 140 cells)."""
        import report
        calls = []

        def pss(pid):
            calls.append(pid)
            if pid == 5 and calls.count(5) == 1:
                raise report.ProcessGone("cannot read /proc/5/smaps_rollup: [Errno 3] No such process")
            return 10

        with tempfile.TemporaryDirectory() as cg, \
                mock.patch.object(report, "cgroup_procs", lambda _p: [1, 2, 5]), \
                mock.patch.object(report, "cgroup_bytes", lambda _p: 4096), \
                mock.patch.object(report, "pss_kb_of_pid", pss), \
                mock.patch.object(report, "STABLE_READ_PAUSE_S", 0.0):
            procs, _current, total, _stat = report.measure_complete_cgroup(cg, with_stat=False)
        self.assertEqual(total, 30)

    def test_an_exited_process_reads_as_gone(self):
        import builtins
        import report
        real_open = builtins.open

        def gone(path, *args, **kwargs):
            if str(path) in ("/proc/4242/smaps_rollup", "/proc/4242/stat"):
                raise ProcessLookupError(3, "No such process", path)
            return real_open(path, *args, **kwargs)

        with mock.patch("builtins.open", gone):
            with self.assertRaises(report.ProcessGone):
                report.pss_kb_of_pid(4242)

    def test_a_zombie_holds_no_memory(self):
        """RED BEFORE THE FIX: a zombie in a clone's cgroup answered ESRCH to
        every smaps_rollup read for a second, and the sample refused after its
        20 re-reads: "cannot read /proc/3843212/smaps_rollup: [Errno 3] No
        such process" (2026-10-22, the same cell twice). A zombie has no
        address space, so its PSS is zero."""
        import builtins
        import report
        real_open = builtins.open

        def zombie(path, *args, **kwargs):
            if str(path) == "/proc/4242/smaps_rollup":
                raise ProcessLookupError(3, "No such process", path)
            if str(path) == "/proc/4242/stat":
                return io.StringIO("4242 (fcvm) Z 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1 0 0")
            return real_open(path, *args, **kwargs)

        with mock.patch("builtins.open", zombie):
            self.assertEqual(report.pss_kb_of_pid(4242), 0)

    def test_a_set_that_never_settles_still_refuses(self):
        import report
        flip = [[1, 2], [1, 2, 3]] * report.STABLE_READ_ATTEMPTS
        with self.assertRaises(report.CgroupReadError):
            self.read(flip + [[1, 2]])


class ScaleProvenanceNamesNoHost(unittest.TestCase):
    """reqscale's provenance records the instance type, never the host name."""

    def test_the_host_name_is_not_recorded(self):
        import platform
        import reqscale
        args = argparse.Namespace(
            fcvm="/bin/true", source_repo="/repo", control_chromium="/bin/true",
            control_url="https://example.com/", control_resolve_all_to="127.0.0.1",
            control_timeout=30.0, runtime={"source_revision": "e" * 40,
                                           "creator_fcvm_sha256": "c" * 64,
                                           "creator_runtime_bundle_sha256": "d" * 64},
            golden_creator={}, trace_faults=False, bpftrace="", max_trace_perturbation_pct=5.0,
            snapshot_generation_lease=argparse.Namespace(path="/x/t.lock"))
        commands = {("git", "-C", "/repo", "rev-parse", "HEAD"): "e" * 40}

        def command(argv):
            return commands.get(tuple(argv), "")

        facts = {"instance_type": "c8g.metal-48xl", "cpu_model": "Neoverse-V2",
                 "nproc": 192, "mem_total_kb": 1, "os": "Ubuntu"}
        with mock.patch.object(reqscale, "_command", command), \
                mock.patch.object(reqscale, "sha256_file", lambda _p: "a" * 64), \
                mock.patch.object(reqscale, "harness_sha256", lambda: "b" * 64), \
                mock.patch.object(reqscale, "schedule_sha256", lambda _s: "f" * 64), \
                mock.patch.object(reqscale, "quiet_host_snapshot",
                                  lambda: {"loadavg1": 0.1, "loadavg1_limit": 2.0,
                                           "vm_process_count": 0, "vm_processes": []}), \
                mock.patch.object(reqscale.reqbench, "host_facts", lambda: facts), \
                mock.patch.object(reqscale.platform, "node", lambda: "ip-10-0-0-1"):
            prov = reqscale.collect_provenance(args, {"run_id": "1" * 32}, {"tag": "t"})
        self.assertNotIn("ip-10-0-0-1", json.dumps(prov))
        self.assertEqual(prov["host"]["instance_type"], "c8g.metal-48xl")
        self.assertNotIn("hostname", prov["host"])


class HostControlArms(unittest.TestCase):
    """corpus_extra.sh's host arms: free, or cpuN for each ladder rung."""

    def budget(self, arm):
        with open(os.path.join(HERE, "corpus_extra.sh")) as handle:
            source = handle.read()
        start = source.index("hostcdp_arm_budget() {")
        end = source.index("\n}\n", start) + 3
        import subprocess
        return subprocess.run(
            ["bash", "-c", source[start:end] + f'\nhostcdp_arm_budget "{arm}"'],
            capture_output=True, text=True, timeout=10)

    def test_each_rung_has_a_cpu_matched_arm(self):
        for arm, want in (("free", "all unlimited"), ("cpu2", "2 vm-matched"),
                          ("cpu4", "4 vm-matched"), ("cpu8", "8 vm-matched")):
            with self.subTest(arm):
                result = self.budget(arm)
                self.assertEqual((result.returncode, result.stdout.strip()), (0, want))

    def test_other_names_are_refused(self):
        for arm in ("cpu0", "cpu", "cpux", "half"):
            with self.subTest(arm):
                self.assertEqual(self.budget(arm).returncode, 2)


if __name__ == "__main__":
    unittest.main()
