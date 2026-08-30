#!/usr/bin/env python3
"""The memory harness's gates and refusals, pinned.

corpus_mem.py runs for hours and publishes per-instance memory and per-render
results for fcvm against host containers. Every property here is a way for it
to finish and report a number that is not a measurement: a preflight that clears
the box because it could not look, a basis summed over a process set the sample
never saw, a subprocess that outlives the deadline meant to bound it, and a
recorded arm thrown away by the failure of the arm after it.

None of them raises on its own. Each one produces a run that looks finished.

Run: python3 -m unittest test_corpus_mem -v
"""

import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import ExitStack
from types import SimpleNamespace
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import corpus_mem  # noqa: E402
import compare as bench_compare  # noqa: E402
import report as bench_report  # noqa: E402

EXTRA = os.path.join(HERE, "corpus_extra.sh")
CORPUS_MEM = os.path.join(HERE, "corpus_mem.py")
CAMPAIGN = os.path.join(HERE, "corpus_campaign.sh")
HOSTCDP = os.path.join(HERE, "hostcdp.sh")
OWNED_PROCESS = os.path.join(HERE, "owned_process.py")


class Completed:
    """Stands in for subprocess.CompletedProcess without running anything."""

    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr


class CorpusExtraSchedule(unittest.TestCase):
    """The host control and VM campaign execute the same measured schedule."""

    @staticmethod
    def default(path, name):
        with open(path) as handle:
            source = handle.read()
        match = re.search(
            rf'^{name}="\$\{{{name}:-([0-9]+)\}}"', source, re.MULTILINE)
        if not match:
            raise AssertionError(f"no numeric {name} default in {path}")
        return int(match.group(1))

    def test_host_control_measured_reps_match_the_vm_campaign(self):
        self.assertEqual(self.default(EXTRA, "REPS"), self.default(CAMPAIGN, "REPS"),
                         "the host and VM arms weight the 14-page corpus differently")


class MemoryCellSchedule(unittest.TestCase):
    """Matched sides stay adjacent in a reproducible, recorded cell schedule."""

    def test_memory_cells_are_interleaved_by_recorded_seed(self):
        sides = ["fcvm", "container"]
        schedule = corpus_mem.build_cell_schedule(
            sides, [1, 2, 4, 8], 2, seed=9182, url_count=14)
        self.assertEqual(schedule,
                         corpus_mem.build_cell_schedule(
                             sides, [1, 2, 4, 8], 2, seed=9182, url_count=14))
        expected = sorted((side, n, rep)
                          for n in (1, 2, 4, 8)
                          for rep in (1, 2)
                          for side in sides)
        self.assertEqual(sorted((side, n, rep) for side, n, rep, _urls in schedule),
                         expected)
        covered = set()
        for offset in range(0, len(schedule), len(sides)):
            pair = schedule[offset:offset + len(sides)]
            self.assertEqual({side for side, _n, _rep, _urls in pair}, set(sides))
            self.assertEqual(len({(n, rep) for _side, n, rep, _urls in pair}), 1,
                             f"matched sides were separated in {pair}")
            self.assertEqual(pair[0][3], pair[1][3],
                             f"matched sides rendered different pages in {pair}")
            covered.update(pair[0][3])
        self.assertEqual(covered, set(range(14)),
                         "the default N/repetition grid omits corpus members")

        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            source = handle.read()
        self.assertIn('"schedule_seed"', source,
                      "the seed needed to reproduce the cell order is not recorded")
        main = source[source.index("def main():"):]
        self.assertIn("schedule = build_cell_schedule(", main,
                      "main can bypass the interleaved schedule the test exercises")
        self.assertIn("for side_name, n, rep, url_indices in schedule:", main)
        self.assertRegex(main, r"run_cell\(\s*sides\[side_name\], args, n, rep, url_indices, out\)")


class SnapshotGenerationLease(unittest.TestCase):
    """Every measured clone must consume the generation recorded in run.json."""

    def test_generation_is_read_under_a_shared_lease_held_by_the_caller(self):
        with tempfile.TemporaryDirectory() as data_root:
            snapshots = os.path.join(data_root, "snapshots")
            os.makedirs(snapshots)
            lock_path = os.path.join(snapshots, "golden.lock")
            open(lock_path, "a").close()
            generation = {"generation_id": "generation-under-test"}
            with mock.patch.object(corpus_mem, "snapshot_generation",
                                   return_value=generation):
                with ExitStack() as resources:
                    actual = corpus_mem.snapshot_generation_under_lease(
                        resources, data_root, "golden")
                    probe = subprocess.run(
                        ["flock", "-x", "-n", lock_path, "true"],
                        capture_output=True, text=True, timeout=10)
                    self.assertNotEqual(
                        probe.returncode, 0,
                        "a creator could replace the recorded generation during the run",
                    )
                    self.assertIs(actual, generation)
                probe = subprocess.run(
                    ["flock", "-x", "-n", lock_path, "true"],
                    capture_output=True, text=True, timeout=10)
                self.assertEqual(probe.returncode, 0, probe.stderr)

    def test_main_uses_the_whole_run_resource_stack_for_the_snapshot_lease(self):
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            source = handle.read()
        self.assertIn("def main_with_resources(resources", source)
        body = source[source.index("def main_with_resources(resources"):]
        lease_match = re.search(
            r"snapshot_generation_under_lease\(\s*resources", body)
        lease = -1 if lease_match is None else lease_match.start()
        serve = body.find("fcvm_side.start_serve()")
        self.assertGreaterEqual(lease, 0)
        self.assertGreater(serve, lease,
                           "the serve can load a generation before the lease is held")


class RunScopedContainerCleanup(unittest.TestCase):
    """One run never names, waits for, or removes another run's containers."""

    def test_memory_container_names_include_the_full_run_id(self):
        args = SimpleNamespace()
        first = corpus_mem.ContainerSide(args, "a" * 32).prefix("host1r1")
        second = corpus_mem.ContainerSide(args, "b" * 32).prefix("host1r1")
        self.assertNotEqual(first, second)
        self.assertIn("a" * 32, first)
        self.assertIn("b" * 32, second)

    def test_teardown_does_not_wait_for_a_peer_runs_container(self):
        token = "c" * 32
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token=token), "a" * 32)
        owned = {"name": side.prefix("host1r1") + "0", "container_id": "ours"}
        calls = []

        def shell(cmd, *_args, **_kwargs):
            calls.append(cmd)
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, f"ours {token}\n", "")
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(1, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", shell):
            side.tear_down([owned])
        self.assertFalse(any(cmd[:3] == ["podman", "ps", "-a"] for cmd in calls))

    def test_teardown_that_cannot_identify_the_target_keeps_it_owned(self):
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token="c" * 32), "a" * 32)
        owned = {"name": side.prefix("host1r1") + "0"}
        side.owned.add(owned["name"])

        def shell(cmd, *_args, **_kwargs):
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            return Completed(125, "", "podman unavailable")

        with mock.patch.object(corpus_mem, "sh_bounded", shell):
            with self.assertRaises(RuntimeError):
                side.tear_down([owned])
        self.assertIn(owned["name"], side.owned)

    def test_outer_cleanup_reaps_only_its_run_after_a_signal(self):
        with open(EXTRA) as handle:
            source = handle.read()
        match = re.search(r'^cleanup\(\) \{\n(.*?)^\}', source,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, "corpus_extra cleanup function is gone")
        self.assertIn("cleanup_owned_containers", match.group(1),
                      "SIGTERM can leave this run's detached containers alive")
        cleanup = re.search(r'^cleanup_owned_containers\(\) \{\n.*?^\}', source,
                            re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(cleanup, "run-owned container cleanup is gone")
        owner = "a" * 32
        peer = "b" * 32
        token = "c" * 32
        peer_token = "d" * 32
        names = (
            f"cbmem-{owner}-host1r1-0",
            f"hostcdp-{owner}-free",
            f"cbmem-cpu-{owner}",
            f"cbmem-{peer}-host1r1-0",
            f"hostcdp-{peer}-free",
            f"cbmem-cpu-{peer}",
        )
        with tempfile.TemporaryDirectory() as tmp:
            removed = os.path.join(tmp, "removed")
            podman = os.path.join(tmp, "podman")
            with open(podman, "w") as handle:
                handle.write(
                    "#!/bin/sh\n"
                    "last=\nfor arg do last=$arg; done\n"
                    "case $1 in\n"
                    f"  ps) printf '%s\\n' {' '.join(repr(f'id{i} {name}') for i, name in enumerate(names))} ;;\n"
                    "  inspect) case \"$last\" in\n"
                    f"    id0) echo 'id0 {token}' ;; id1) echo 'id1 {token}' ;; id2) echo 'id2 {token}' ;;\n"
                    f"    *) echo \"$last {peer_token}\" ;; esac ;;\n"
                    "  rm) printf '%s\\n' \"$last\" >>\"$REMOVED\" ;;\n"
                    "  *) exit 64 ;;\n"
                    "esac\n")
            os.chmod(podman, 0o755)
            script = ("set -euo pipefail\n"
                      f"RUN_ID={owner}\nCONTAINER_OWNER_TOKEN={token}\n"
                      + cleanup.group(0) + "\n"
                      + "cleanup_owned_containers\n")
            env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                       REMOVED=removed)
            proc = subprocess.run(["bash", "-c", script], env=env,
                                  capture_output=True, text=True, timeout=60)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(removed) as handle:
                actual = set(handle.read().splitlines())
        self.assertEqual(actual, {"id0", "id1", "id2"},
                         "cleanup crossed the run ownership boundary")
        self.assertIn('--run-id "$RUN_ID"', source,
                      "the child and outer cleanup do not share an owner ID")
        self.assertIn('--container-owner-token "$CONTAINER_OWNER_TOKEN"', source,
                      "the child and outer cleanup do not share ownership proof")
        self.assertIn("stop_active_phase", match.group(1),
                      "cleanup can race a child that is still creating containers")
        self.assertLess(match.group(1).find("stop_active_phase"),
                        match.group(1).find("cleanup_owned_containers"),
                        "the child must stop before its owned containers are enumerated")
        self.assertIn('setsid "$@"', source,
                      "killing only the phase parent can leave its VMM children running")
        self.assertIn('kill -TERM -- "-$pid"', source)

    def test_reused_root_pid_is_never_signalled(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("owned_process", OWNED_PROCESS)
        self.assertIsNotNone(spec)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        sent = []
        identities = iter((111, 222))
        result = module.signal_if_identity(
            4242, 111, 15,
            read_identity=lambda _pid: next(identities),
            open_pidfd=lambda _pid: 9,
            send_signal=lambda fd, sig: sent.append((fd, sig)),
            close_pidfd=lambda _fd: None,
        )
        self.assertFalse(result)
        self.assertEqual(sent, [], "a replacement process received the cleanup signal")

    def test_corpus_serve_ownership_is_cleared_after_stop(self):
        with open(EXTRA) as handle:
            source = handle.read()
        function = re.search(r'^stop_corpus_serve\(\) \{\n.*?^\}', source,
                             re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(function)
        body = function.group(0)
        self.assertIn('owned_process.py" signal', body)
        self.assertIn('SERVE_PID=""', body)
        self.assertIn('SERVE_START_TIME=""', body)

    def test_a_phase_leader_cannot_leave_an_untracked_descendant(self):
        with open(EXTRA) as handle:
            source = handle.read()
        functions = []
        for name in ("stop_active_phase", "run_logged"):
            match = re.search(rf'^{name}\(\) \{{\n.*?^\}}', source,
                              re.MULTILINE | re.DOTALL)
            self.assertIsNotNone(match, f"{name} is gone")
            functions.append(match.group(0))
        with tempfile.TemporaryDirectory() as tmp:
            child_pid = os.path.join(tmp, "child.pid")
            log_path = os.path.join(tmp, "phase.log")
            script = (
                "set -uo pipefail\n"
                "say() { :; }\n"
                "ACTIVE_PHASE_PID=\n"
                + "\n".join(functions) + "\n"
                + "set +e\n"
                + f"CHILD_PID={child_pid!r} run_logged {log_path!r} "
                  "sh -c 'sleep 60 </dev/null >/dev/null 2>&1 & "
                  "echo $! >\"$CHILD_PID\"'\n"
                + "phase_rc=$?\n"
                + f"child=$(cat {child_pid!r})\n"
                + "if kill -0 \"$child\" 2>/dev/null; then "
                  "echo SURVIVED; kill -KILL \"$child\" 2>/dev/null || true; fi\n"
                + "exit \"$phase_rc\"\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=20)
        self.assertNotEqual(
            proc.returncode, 0,
            "run_logged reported success after its process group outlived the leader",
        )
        self.assertNotIn("SURVIVED", proc.stdout,
                         "the descendant escaped both phase accounting and cleanup")

    def test_a_failed_name_collision_never_makes_the_peer_owned(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="a" * 32)
        side = corpus_mem.ContainerSide(args, "b" * 32)
        removed = []

        def bounded(cmd, _timeout):
            if cmd[:3] == ["podman", "run", "-d"]:
                return Completed(125, "", "name is already in use")
            if cmd[:3] == ["podman", "rm", "-f"]:
                removed.append(cmd[-1])
                return Completed()
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, "peer-id peer-owner\n", "")
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            if cmd[:3] == ["podman", "ps", "-a"]:
                return Completed(0, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
            side.stop_all()
        self.assertEqual(removed, [],
                         "cleanup deleted a same-name container this run did not create")

    def test_partial_creation_is_cleaned_by_owner_label_and_exact_id(self):
        token = "a" * 32
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token=token)
        side = corpus_mem.ContainerSide(args, "b" * 32)
        removed = []
        name = side.prefix("host1r1") + "0"

        def bounded(cmd, _timeout):
            if cmd[:3] == ["podman", "run", "-d"]:
                return Completed(124, "", "timed out after create")
            if cmd[:3] == ["podman", "inspect", "--format"]:
                return Completed(0, f"{'c' * 64} {token}\n", "")
            if cmd[:3] == ["podman", "rm", "-f"]:
                removed.append(cmd[-1])
                return Completed()
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(1, "", "")
            return Completed()

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
            side.stop_all()
        self.assertEqual(removed, ["c" * 64])
        self.assertNotIn(name, side.owned)

    def test_shared_replay_ports_are_locked_before_dnsmasq_is_touched(self):
        with open(EXTRA) as handle:
            source = handle.read()
        lock = source.find("flock -n 9")
        dnsmasq = source.find("sudo systemctl stop dnsmasq")
        self.assertGreaterEqual(lock, 0, "the shared DNS/HTTP/HTTPS ports have no lease")
        self.assertGreaterEqual(dnsmasq, 0, "the dnsmasq handoff is gone")
        self.assertLess(lock, dnsmasq,
                        "the shared-port lease starts after host DNS is already changed")

    def test_shared_replay_ports_use_one_host_wide_lock(self):
        with open(EXTRA) as handle:
            source = handle.read()
        self.assertIn('CORPUS_EXTRA_LOCK="/run/lock/fcvm-corpus-extra.lock"', source,
                      "different UIDs can mutate the same host ports and dnsmasq concurrently")
        self.assertIn(
            'sudo -n install -d -o root -g root -m 0755 "$CORPUS_EXTRA_LOCK"',
            source,
            "the first caller can leave a lock inode another UID cannot open")
        self.assertIn('exec 9<"$CORPUS_EXTRA_LOCK"', source,
                      "opening a shared regular file with O_CREAT is denied across UIDs")

    def test_empty_phases_refuse_before_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "results")
            logs = os.path.join(tmp, "logs")
            env = dict(os.environ, PHASES="", RUN_ID="0" * 32,
                       RESULTS=results, LOGDIR=logs)
            result = subprocess.run(
                ["bash", EXTRA], env=env, capture_output=True, text=True,
                timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(os.path.exists(results))
            self.assertFalse(os.path.exists(logs))

    def test_host_control_name_is_derived_from_its_run_id(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        match = re.search(r'^CNAME="([^"]+)"', source, re.MULTILINE)
        self.assertIsNotNone(match)
        self.assertIn("$RUNID", match.group(1))
        self.assertNotIn("$$", match.group(1))

    def test_host_control_readiness_proves_its_container_owns_cdp(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        self.assertIn("container_owns_cdp", source)
        readiness = source[source.index("until "):source.index("log \"warm marker up")]
        self.assertIn("container_owns_cdp", readiness)

    def test_standalone_host_control_refuses_both_vmm_processes_fail_closed(self):
        with open(HOSTCDP) as handle:
            source = handle.read()
        self.assertIn("for process in fcvm firecracker", source)
        self.assertRegex(source, r'case "\$rc" in\s*0\)')
        self.assertRegex(source, r'\s1\)')

    def test_container_render_outputs_are_removed_before_sampling(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="c" * 32)
        side = corpus_mem.ContainerSide(args, "a" * 32)
        calls = []

        def shell(cmd, *_args, **_kwargs):
            calls.append(cmd)
            return Completed(0, "container-id\n", "")

        with mock.patch.object(corpus_mem, "sh", shell), \
             mock.patch.object(corpus_mem, "sh_bounded", shell):
            side.bring_up(1, "host1r1", [0])
        cleanup_calls = [cmd for cmd in calls
                         if cmd[:4] == ["podman", "exec", side.prefix("host1r1") + "0", "rm"]]
        self.assertTrue(cleanup_calls,
                        "render.py's JPEG and DOM remain charged to the container cgroup")

    def test_failed_podman_listing_cannot_report_cleanup_complete(self):
        side = corpus_mem.ContainerSide(
            SimpleNamespace(container_owner_token="c" * 32), "a" * 32)
        name = side.prefix("host1r1") + "0"
        side.owned.add(name)

        def bounded(cmd, _timeout):
            if cmd[:3] == ["podman", "container", "exists"]:
                return Completed(0, "", "")
            return Completed(125, "", "remove failed")

        with mock.patch.object(corpus_mem, "sh_bounded", bounded, create=True):
            with self.assertRaises(RuntimeError):
                side.tear_down([{"name": name}])
        self.assertIn(name, side.owned,
                      "failed cleanup discarded the only record of its live container")

    def test_failed_podman_run_keeps_the_name_owned_for_final_cleanup(self):
        args = SimpleNamespace(image="image", urls=["https://example.com/"],
                               container_owner_token="c" * 32)
        side = corpus_mem.ContainerSide(args, "a" * 32)
        name = side.prefix("host1r1") + "0"
        with mock.patch.object(
                corpus_mem, "sh_bounded",
                return_value=Completed(125, "", "runtime failed after create")):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "host1r1", [0])
        self.assertIn(name, side.owned)

    def test_failed_clone_readiness_keeps_the_process_owned_for_final_cleanup(self):
        proc = mock.Mock()
        proc.poll.return_value = None
        args = SimpleNamespace(
            results="/tmp", fcvm="fcvm", state_dir="/state", cdp_port=9222,
            urls=["https://example.com/"], tag="tag",
        )
        cg = mock.Mock()
        cg.leaf.return_value = "/cgroup/leaf"
        side = corpus_mem.FcvmSide(args, cg, "a" * 32)
        with mock.patch.object(corpus_mem, "spawn_in_cgroup", return_value=proc), \
             mock.patch.object(corpus_mem, "find_clone_state", return_value=None):
            with self.assertRaises(SystemExit):
                side.bring_up(1, "fcvm1r1", [0])
        self.assertIn("mem-" + "a" * 32 + "-fcvm1r1-0", side.owned)

    def test_unreadable_state_directory_cannot_prove_a_clone_is_gone(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = os.path.join(tmp, "missing-state")
            with self.assertRaises(corpus_mem.CloneStateReadError):
                corpus_mem.clone_gone(missing, "clone-a")

    def test_malformed_state_cannot_prove_a_clone_is_gone(self):
        with tempfile.TemporaryDirectory() as state_dir:
            with open(os.path.join(state_dir, "unknown.json"), "w") as handle:
                handle.write("{not-json\n")
            with self.assertRaises(corpus_mem.CloneStateReadError):
                corpus_mem.clone_gone(state_dir, "clone-a")

    def test_state_proof_failure_does_not_skip_later_clone_teardown(self):
        args = SimpleNamespace(state_dir="/state")
        cg = mock.Mock()
        side = corpus_mem.FcvmSide(args, cg, "a" * 32)
        clones = []
        for index in range(2):
            proc = mock.Mock()
            clone = {"name": f"clone-{index}", "leaf": f"leaf-{index}",
                     "proc": proc}
            clones.append(clone)
            side.owned[clone["name"]] = clone
        with mock.patch.object(
                corpus_mem, "clone_gone",
                side_effect=[corpus_mem.CloneStateReadError("cannot read state"), True]):
            with self.assertRaises(corpus_mem.CloneStateReadError):
                side.tear_down(clones)
        self.assertEqual(cg.rm.call_count, 2,
                         "one proof error skipped teardown of a later clone")
        for clone in clones:
            clone["proc"].terminate.assert_called_once()
            clone["proc"].wait.assert_called_once()

    def test_one_cleanup_failure_does_not_skip_the_other_resources(self):
        calls = []

        class Side:
            def __init__(self, label, *methods):
                self.label = label
                for method in methods:
                    setattr(self, method, self.stop)

            def stop(self):
                calls.append(self.label)
                raise RuntimeError(self.label)

        class Cgroup:
            def rm_all(self):
                calls.append("cgroup")
                raise RuntimeError("cgroup")

        class Output:
            def close(self):
                calls.append("output")
                raise RuntimeError("output")

        with self.assertRaises(RuntimeError):
            corpus_mem.cleanup_harness_resources(
                Side("container", "stop_all"),
                Side("fcvm", "stop_all", "stop_serve"),
                Cgroup(), Output())
        self.assertEqual(calls,
                         ["container", "fcvm", "fcvm", "cgroup", "output"])


class HostCdpProducer(unittest.TestCase):
    """The standalone host control publishes one owned, attributable run."""

    CONTAINER_ID = "c" * 64
    PEER_ID = "d" * 64
    IMAGE_ID = "sha256:" + "a" * 64

    def environment(self, tmp, mode="ok", **overrides):
        bindir = os.path.join(tmp, "bin")
        os.makedirs(bindir)
        state = os.path.join(tmp, "podman-state")
        removed = os.path.join(tmp, "removed")
        loadavg = os.path.join(tmp, "loadavg")
        with open(loadavg, "w") as handle:
            handle.write("0.1 0.2 0.3 1/1 1\n")

        podman = os.path.join(bindir, "podman")
        with open(podman, "w") as handle:
            handle.write(f'''#!/bin/bash
set -u
cmd="${{1:-}}"
[ "$#" -eq 0 ] || shift
case "$cmd" in
  run)
    name=
    owner=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift 2 ;;
        --label)
          case "$2" in
            io.fcvm.bench.owner=*) owner="${{2#*=}}" ;;
          esac
          shift 2
          ;;
        *) shift ;;
      esac
    done
    [ "${{PODMAN_MODE:-ok}}" != collision ] || exit 125
    printf '%s\n' "$name" >"$PODMAN_TEST_STATE.name"
    printf '%s\n' "$owner" >"$PODMAN_TEST_STATE.owner"
    printf '%s\n' '{self.CONTAINER_ID}'
    ;;
  inspect)
    format=
    target=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --format) format="$2"; shift 2 ;;
        --type) shift 2 ;;
        *) target="$1"; shift ;;
      esac
    done
    if [ "${{PODMAN_MODE:-ok}}" = collision ]; then
      printf '%s|%s\n' '{self.PEER_ID}' 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
      exit 0
    fi
    [ -e "$PODMAN_TEST_STATE.name" ] || exit 1
    case "$format" in
      *'.Image'*) printf '%s\n' '{self.IMAGE_ID}' ;;
      *'Config.Labels'*)
        printf '%s|%s\n' '{self.CONTAINER_ID}' "$(cat "$PODMAN_TEST_STATE.owner")"
        ;;
      *) exit 64 ;;
    esac
    ;;
  exec) exit 0 ;;
  rm)
    target="${{@: -1}}"
    printf '%s\n' "$target" >>"$PODMAN_REMOVED"
    exit 0
    ;;
  logs) exit 0 ;;
  *) exit 64 ;;
esac
''')
        os.chmod(podman, 0o755)

        pgrep = os.path.join(bindir, "pgrep")
        with open(pgrep, "w") as handle:
            handle.write("#!/bin/sh\nexit 1\n")
        os.chmod(pgrep, 0o755)

        python = os.path.join(bindir, "python3")
        with open(python, "w") as handle:
            handle.write(f'''#!/bin/bash
if [ "${{1:-}}" = "$HOSTCDP_DRIVER" ]; then
  [ -z "${{DRIVER_STARTED_FILE:-}}" ] || : >"$DRIVER_STARTED_FILE"
  if [ -n "${{DRIVER_WAIT_FILE:-}}" ]; then
    while [ ! -e "$DRIVER_WAIT_FILE" ]; do /bin/sleep 0.01; done
  fi
  case "${{DRIVER_LOAD_ACTION:-}}" in
    nonnumeric) printf '%s\n' 'not-a-load' >"$LOADAVG_FILE" ;;
    missing) rm -f -- "$LOADAVG_FILE" ;;
  esac
  case "${{DRIVER_RUNTIME_ACTION:-}}" in
    tamper) printf '%s\n' mutated >"$RUNTIME_PAYLOAD" ;;
  esac
  printf '%s\n' '{{"ok":true,"url":"https://example.com/","stages":{{"total_ms":1.0}},"nav":{{"load_ms":1.0}}}}'
  exit 0
fi
exec {sys.executable!r} "$@"
''')
        os.chmod(python, 0o755)

        if mode == "partial":
            real_timeout = shutil.which("timeout")
            timeout = os.path.join(bindir, "timeout")
            with open(timeout, "w") as handle:
                handle.write(f'''#!/bin/bash
duration="$1"
shift
if [ "${{1:-}}" = podman ] && [ "${{2:-}}" = run ]; then
  "$@" >/dev/null
  exit 124
fi
exec {real_timeout!r} "$duration" "$@"
''')
            os.chmod(timeout, 0o755)

        runtime = os.path.join(tmp, "runtime")
        os.mkdir(runtime)
        payload = os.path.join(runtime, "payload")
        with open(payload, "w") as handle:
            handle.write("sealed\n")
        payload_digest = hashlib.sha256(b"sealed\n").hexdigest()
        manifest = os.path.join(runtime, "REQBENCH_MANIFEST.sha256")
        with open(manifest, "w") as handle:
            handle.write(f"{payload_digest}  payload\n")
        with open(manifest, "rb") as handle:
            runtime_digest = hashlib.sha256(handle.read()).hexdigest()
        outer_manifest = os.path.join(runtime, "MANIFEST.sha256")
        with open(manifest, "rb") as source, open(outer_manifest, "wb") as target:
            target.write(source.read())
        revision = subprocess.check_output(
            ["git", "-C", os.path.dirname(os.path.dirname(HERE)),
             "rev-parse", "HEAD"], text=True).strip()

        env = dict(
            os.environ,
            PATH=bindir + os.pathsep + os.environ["PATH"],
            PODMAN_MODE=mode,
            PODMAN_TEST_STATE=state,
            PODMAN_REMOVED=removed,
            HOSTCDP_DRIVER=os.path.join(HERE, "cdpdrive.py"),
            LOADAVG_FILE=loadavg,
            RESULTS=os.path.join(tmp, "results"),
            RUNID="1" * 32,
            REPS="1",
            WARMUP="0",
            ALLOW_BUSY="1",
            SETTLE_WAIT_SECS="0",
            URL="https://example.com/",
            COMPARISON_LABEL="free",
            CPU_BUDGET="unlimited",
            SOURCE_REVISION=revision,
            REQBENCH_RUNTIME_MANIFEST=manifest,
            REQBENCH_RUNTIME_BUNDLE_SHA256=runtime_digest,
            CORPUS_EXTRA_RUNTIME_MANIFEST=outer_manifest,
            CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256=runtime_digest,
            RUNTIME_PAYLOAD=payload,
        )
        env.pop("CPUS", None)
        env.pop("CONTAINER_OWNER_TOKEN", None)
        env.update(overrides)
        return env, removed, state

    @staticmethod
    def run_host(env):
        return subprocess.run(
            ["bash", HOSTCDP], env=env, capture_output=True, text=True,
            timeout=30,
        )

    @staticmethod
    def removed_ids(path):
        if not os.path.exists(path):
            return []
        with open(path) as handle:
            return handle.read().splitlines()

    def test_a_name_collision_never_deletes_the_peer(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, _state = self.environment(tmp, mode="collision")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(
                self.removed_ids(removed), [],
                "a failed podman run made the pre-existing same-name container owned",
            )

    def test_partial_create_timeout_adopts_only_its_labeled_exact_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, removed, state = self.environment(tmp, mode="partial")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(self.removed_ids(removed), [self.CONTAINER_ID])
            with open(state + ".owner") as handle:
                token = handle.read().strip()
            self.assertRegex(token, r"^[0-9a-f]{32}$")

    def test_mid_run_nonnumeric_load_is_raw_evidence_not_a_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LOAD_ACTION="nonnumeric")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("REFUSING", proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
            self.assertEqual(len(rows), 1)
            self.assertIsNone(rows[0]["loadavg1"])
            self.assertEqual(rows[0]["loadavg1_raw"], "not-a-load")
            self.assertEqual(rows[0]["loadavg1_read_status"], 0)
            self.assertIs(rows[0]["measurement_valid"], False)

    def test_mid_run_missing_load_is_raw_evidence_not_a_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_LOAD_ACTION="missing")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("REFUSING", proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))
            with open(os.path.join(env["RESULTS"], "hostcdp.jsonl")) as handle:
                row = json.loads(handle.readline())
            self.assertIsNone(row["loadavg1"])
            self.assertNotEqual(row["loadavg1_read_status"], 0)
            self.assertIsInstance(row["loadavg1_raw"], str)
            self.assertIs(row["measurement_valid"], False)

    def test_run_metadata_binds_host_source_and_comparison_semantics(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, state = self.environment(tmp)
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            with open("/proc/sys/kernel/random/boot_id") as handle:
                boot_id = handle.read().strip()
            revision = subprocess.check_output(
                ["git", "-C", os.path.dirname(os.path.dirname(HERE)),
                 "rev-parse", "HEAD"], text=True).strip()
            with open(HOSTCDP, "rb") as handle:
                producer_hash = hashlib.sha256(handle.read()).hexdigest()
            import reqbench
            expected = {
                "host_boot_id": boot_id,
                "host_machine": os.uname().machine,
                "host_kernel": os.uname().release,
                "source_revision": revision,
                "runtime_bundle_sha256": env["REQBENCH_RUNTIME_BUNDLE_SHA256"],
                "corpus_extra_runtime_bundle_sha256":
                    env["CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256"],
                "harness_sha256": reqbench.harness_sha256(),
                "hostcdp_sha256": producer_hash,
                "driver": "cdpdrive.py",
                "network": "host (no VM, no DNAT)",
                "comparison_label": "free",
                "cpu_budget": "unlimited",
                "cpus": None,
            }
            for name, value in expected.items():
                self.assertEqual(run.get(name), value, name)
            self.assertRegex(run.get("container_owner_token", ""), r"^[0-9a-f]{32}$")
            with open(state + ".owner") as handle:
                self.assertEqual(handle.read().strip(), run["container_owner_token"])

    def test_runtime_manifest_drift_refuses_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, DRIVER_RUNTIME_ACTION="tamper")
            proc = self.run_host(env)
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("runtime", proc.stderr.lower())
            self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_vm_matched_cpu_budget_records_a_finite_json_number(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, _removed, _state = self.environment(
                tmp, CPU_BUDGET="vm-matched", CPUS="2.5")
            proc = self.run_host(env)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(os.path.join(env["RESULTS"], "run.json")) as handle:
                run = json.load(handle)
            self.assertEqual(run["cpu_budget"], "vm-matched")
            self.assertEqual(run["cpus"], 2.5)
            self.assertIsInstance(run["cpus"], float)

    def test_cpu_budget_and_comparison_label_are_explicit(self):
        cases = (
            ({"CPU_BUDGET": "unlimited", "CPUS": "2"}, "unlimited"),
            ({"CPU_BUDGET": "vm-matched", "CPUS": ""}, "vm-matched"),
            ({"CPU_BUDGET": "unknown"}, "CPU_BUDGET"),
            ({"COMPARISON_LABEL": ""}, "COMPARISON_LABEL"),
        )
        for overrides, expected in cases:
            with self.subTest(overrides=overrides), tempfile.TemporaryDirectory() as tmp:
                env, _removed, _state = self.environment(tmp, **overrides)
                proc = self.run_host(env)
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertIn(expected, proc.stderr)
                self.assertFalse(os.path.exists(os.path.join(env["RESULTS"], "summary.json")))

    def test_producer_and_resummarizer_serialize_on_one_permanent_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            started = os.path.join(tmp, "driver-started")
            release = os.path.join(tmp, "release-driver")
            env, _removed, _state = self.environment(
                tmp, DRIVER_STARTED_FILE=started, DRIVER_WAIT_FILE=release)
            producer = subprocess.Popen(
                ["bash", HOSTCDP], env=env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            resummarizer = None
            producer_output = ("", "")
            resummarizer_output = ("", "")
            opened_shared_lock = False
            try:
                deadline = time.monotonic() + 10
                while not os.path.exists(started) and time.monotonic() < deadline:
                    if producer.poll() is not None:
                        output = producer.communicate()
                        self.fail("producer exited before the held rep: " + "".join(output))
                    time.sleep(0.01)
                self.assertTrue(os.path.exists(started), "producer never reached the held rep")
                resummarizer = subprocess.Popen(
                    [sys.executable, os.path.join(HERE, "resummarize.py"), env["RESULTS"]],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                lock_path = os.path.realpath(os.path.join(env["RESULTS"], ".summary.lock"))
                deadline = time.monotonic() + 10
                while resummarizer.poll() is None and time.monotonic() < deadline:
                    fd_dir = f"/proc/{resummarizer.pid}/fd"
                    try:
                        targets = [os.path.realpath(os.path.join(fd_dir, fd))
                                   for fd in os.listdir(fd_dir)]
                    except FileNotFoundError:
                        break
                    if lock_path in targets:
                        opened_shared_lock = True
                        break
                    time.sleep(0.01)
                self.assertTrue(
                    opened_shared_lock,
                    "resummarize did not open the producer's permanent summary lock",
                )
                self.assertIsNone(
                    resummarizer.poll(),
                    "resummarize passed the producer while its records were incomplete",
                )
            finally:
                with open(release, "a"):
                    pass
                producer_output = producer.communicate(timeout=20)
                if resummarizer is not None:
                    resummarizer_output = resummarizer.communicate(timeout=20)
            self.assertEqual(producer.returncode, 0, "".join(producer_output))
            self.assertEqual(resummarizer.returncode, 0, "".join(resummarizer_output))
            self.assertTrue(os.path.exists(os.path.join(env["RESULTS"], ".summary.lock")))
            with open(os.path.join(env["RESULTS"], "summary.json")) as handle:
                self.assertEqual(json.load(handle)["n"], 1)

    def test_both_summary_writers_publish_by_atomic_replace(self):
        with open(HOSTCDP) as handle:
            producer = handle.read()
        with open(os.path.join(HERE, "resummarize.py")) as handle:
            resummarizer = handle.read()
        self.assertIn('"$RESULTS/.summary.lock"', producer)
        self.assertIn("os.replace(temporary, output_path)", producer)
        self.assertNotIn('open(sys.argv[3], "w")', producer)
        self.assertIn('os.path.join(d, ".summary.lock")', resummarizer)
        self.assertNotIn(".resummarize.lock", resummarizer)
        self.assertIn("write_json_atomic(summary_path, out)", resummarizer)


class CorpusExtraFailClosed(unittest.TestCase):
    """The shell driver cannot publish after a preflight or replay failure."""

    @classmethod
    def source(cls):
        with open(EXTRA) as handle:
            return handle.read()

    def test_unknown_or_empty_phases_are_validated(self):
        source = self.source()
        self.assertIn("validate_phases", source)
        self.assertLess(source.find("validate_phases"), source.find("claim_output_dirs"))

    def test_output_directories_are_claimed_exclusively(self):
        source = self.source()
        helper = re.search(r'^claim_output_dir\(\) \{\n.*?^\}', source,
                           re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(helper, "output ownership helper is gone")
        self.assertLess(source.find("claim_output_dirs"),
                        source.find("provenance.json"),
                        "a reused directory can be modified before it is refused")
        with tempfile.TemporaryDirectory() as tmp:
            existing = os.path.join(tmp, "existing")
            os.mkdir(existing)
            marker = os.path.join(existing, "prior-result")
            with open(marker, "w") as handle:
                handle.write("prior\n")
            script = ("set -euo pipefail\n" + helper.group(0) + "\n"
                      + f"claim_output_dir {existing!r} results\n")
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=10)
            self.assertNotEqual(proc.returncode, 0)
            with open(marker) as handle:
                self.assertEqual(handle.read(), "prior\n")

    def test_all_critical_external_dependencies_are_preflighted(self):
        source = self.source()
        tools = re.search(r'^for tool in (.*?); do$', source,
                          re.MULTILINE | re.DOTALL).group(1).replace("\\", "").split()
        for required in ("git", "sha256sum", "systemctl"):
            self.assertIn(required, tools)

    def test_outer_stray_preflight_distinguishes_no_match_from_error(self):
        source = self.source()
        self.assertIn("find_stray_vmms", source)
        self.assertRegex(source, r'case "\$rc" in\s*0\)')
        self.assertRegex(source, r'\s1\)')
        self.assertIn("pgrep", re.search(r'^for tool in .*?; do$', source,
                                         re.MULTILINE | re.DOTALL).group(0))

    def test_replay_readiness_is_bound_to_this_server(self):
        source = self.source()
        launch = source[source.index('SERVE_PIDFILE='):source.index('# Every corpus member')]
        self.assertIn('sudo kill -0 "$SERVE_PID"', launch)
        self.assertIn('[ "$answer" = "10.0.2.2" ]', launch)
        self.assertIn('[ "$code" = "200" ]', launch)

    def test_replay_nonzero_exit_prevents_success(self):
        source = self.source()
        final_records = source.rfind('say "records:')
        stop = source.rfind("stop_corpus_serve", 0, final_records)
        require = source.rfind("require_corpus_serve_clean", 0, final_records)
        self.assertGreater(stop, source.index('if [[ ",$PHASES,"'))
        self.assertGreater(require, stop)


class StrayPreflight(unittest.TestCase):
    """A preflight that could not run has not cleared the box.

    The refusal exists because a leftover fcvm or firecracker is charged to
    whatever this run then measures. `pgrep ... || true` collapsed every way
    pgrep can fail into the same empty string the clean case produces: exit 127
    (pgrep absent), 2 (bad pattern), 3 (fatal). Only exit 1 means "no match".
    """

    def patch(self, fn):
        real = corpus_mem.sh
        corpus_mem.sh = fn
        self.addCleanup(setattr, corpus_mem, "sh", real)

    def test_a_clean_box_reports_no_strays(self):
        self.patch(lambda *_a, **_k: Completed(1, "", ""))
        self.assertEqual(corpus_mem.stray_vmm_processes(), "")

    def test_a_dirty_box_reports_the_processes_it_found(self):
        self.patch(lambda *_a, **_k: Completed(0, "4242 firecracker --api-sock x\n", ""))
        self.assertIn("firecracker", corpus_mem.stray_vmm_processes())

    def assert_blocks(self):
        """SystemExit with a NON-ZERO code. `sys.exit(0)` is also a SystemExit,
        and a preflight that exits clean is the fail-open this refuses."""
        with self.assertRaises(SystemExit) as caught:
            corpus_mem.stray_vmm_processes()
        self.assertNotIn(caught.exception.code, (0, None),
                         "the preflight exited cleanly; the run would continue")

    def test_a_preflight_that_could_not_run_blocks_the_run(self):
        """exit 127 is "pgrep is not installed", not "the box is clean"."""
        self.patch(lambda *_a, **_k: Completed(127, "", "pgrep: command not found"))
        self.assert_blocks()

    def test_a_preflight_that_errored_blocks_the_run(self):
        self.patch(lambda *_a, **_k: Completed(2, "", "pgrep: syntax error"))
        self.assert_blocks()

    def test_a_preflight_that_cannot_be_spawned_blocks_the_run(self):
        def boom(*_a, **_k):
            raise FileNotFoundError("pgrep")
        self.patch(boom)
        self.assert_blocks()


class ZeroBasis(unittest.TestCase):
    """A basis of zero is a sample that could not see the process set.

    Every basis is a sum over the processes of live instances that have each
    rendered a page, so with the instance count satisfied none of them can be
    zero. report.py's single-node cgroup.procs read produced exactly this on
    the container side: pool_containers = N from `podman ps`, pool_procs = 0 and
    pool_pss_kb = 0 from an empty process set. cell_values reads those with
    `.get(key, 0)`, so a zero becomes a real number in summary.json and in the
    least-squares fit, and the run's own instance-count check passes.
    """

    def test_a_complete_container_sample_is_accepted(self):
        s = {"pool_containers": 4, "pool_procs": 52, "pool_cgroup_kb": 900_000,
             "pool_pss_kb": 700_000}
        self.assertEqual(corpus_mem.empty_bases(s, "host-container"), [])

    def test_a_container_sample_with_no_pss_is_refused(self):
        s = {"pool_containers": 4, "pool_procs": 0, "pool_cgroup_kb": 900_000,
             "pool_pss_kb": 0}
        self.assertEqual(sorted(corpus_mem.empty_bases(s, "host-container")),
                         ["pool_procs", "pool_pss_kb"])

    def test_a_clone_sample_with_no_pss_is_refused(self):
        s = {"clones": 4, "clone_procs": 0, "clone_cgroup_kb": 900_000,
             "clone_pss_kb": 0}
        self.assertEqual(sorted(corpus_mem.empty_bases(s, "fcvm-clone")),
                         ["clone_procs", "clone_pss_kb"])

    def test_a_missing_basis_is_refused_like_a_zero_one(self):
        """`.get(key, 0)` downstream cannot tell the two apart, so neither does this."""
        self.assertEqual(sorted(corpus_mem.empty_bases({"pool_containers": 1},
                                                       "host-container")),
                         ["pool_cgroup_kb", "pool_procs", "pool_pss_kb"])

    def test_a_complete_clone_sample_is_accepted(self):
        s = {"clones": 2, "clone_procs": 8, "clone_cgroup_kb": 500_000,
             "clone_pss_kb": 400_000}
        self.assertEqual(corpus_mem.empty_bases(s, "fcvm-clone"), [])

    def test_an_unreadable_cgroup_root_is_not_a_zero_measurement(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = os.path.join(tmp, "disappeared")
            with self.assertRaises(bench_report.CgroupReadError):
                bench_report.measure_cgroup_set(missing, "serve-")


class BoundedAttempt(unittest.TestCase):
    """A deadline checked after a hung subprocess is never checked again.

    The container readiness loops bound themselves with a 180 s deadline and
    then call `podman exec` with no timeout. One wedged container holds the
    harness there forever, and the deadline that was supposed to produce the
    diagnostic never gets evaluated. AGENTS.md: bound every attempt, not just
    the total.
    """

    def test_an_attempt_that_times_out_becomes_a_failed_attempt(self):
        def hang(cmd, **_k):
            raise subprocess.TimeoutExpired(cmd, 30)
        real = corpus_mem.sh
        corpus_mem.sh = hang
        self.addCleanup(setattr, corpus_mem, "sh", real)
        r = corpus_mem.sh_bounded(["podman", "exec", "x", "true"], 30)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("30", r.stderr)

    def test_an_attempt_that_answers_is_passed_through(self):
        real = corpus_mem.sh
        corpus_mem.sh = lambda *_a, **_k: Completed(0, "yes", "")
        self.addCleanup(setattr, corpus_mem, "sh", real)
        self.assertEqual(corpus_mem.sh_bounded(["true"], 30).stdout, "yes")

    @staticmethod
    def podman_exec_calls(src):
        """Every `podman exec` call site, with the function it went through."""
        out = []
        for m in re.finditer(r'(sh(?:_bounded)?)\(\[\s*"podman",\s*"exec"', src):
            depth = 0
            i = src.index("(", m.start())
            j = i
            while j < len(src):
                if src[j] == "(":
                    depth += 1
                elif src[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            out.append((m.group(1), src[i:j + 1]))
        return out

    def test_every_podman_exec_carries_a_bound(self):
        """A source-level pin: one unbounded `podman exec` reintroduces the hang.

        Either through sh_bounded, or through sh with an explicit timeout. A
        bare sh(["podman", "exec", ...]) is the shape that hangs.
        """
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            src = handle.read()
        calls = self.podman_exec_calls(src)
        self.assertTrue(calls, "the podman exec call sites are gone")
        for fn, call in calls:
            self.assertTrue(fn == "sh_bounded" or "timeout=" in call,
                            f"unbounded podman exec: {call!r}")

    def test_every_podman_lifecycle_call_carries_a_bound(self):
        with open(os.path.join(HERE, "corpus_mem.py")) as handle:
            src = handle.read()
        for operation in ("rm", "ps", "logs", "inspect"):
            for match in re.finditer(
                    rf'(sh(?:_bounded)?)\(\[\s*"podman",\s*"{operation}"', src):
                self.assertEqual(match.group(1), "sh_bounded",
                                 f"unbounded podman {operation} at byte {match.start()}")


class CgroupLifecycle(unittest.TestCase):
    """A stale or unremovable cgroup is contamination, not cleanup success."""

    def test_setup_refuses_an_existing_run_cgroup(self):
        cg = corpus_mem.CgroupSet("/sys/fs/cgroup/cbmem-existing.slice")
        with mock.patch("os.path.exists", return_value=True), \
             mock.patch.object(corpus_mem, "sh_bounded") as run:
            with self.assertRaises(SystemExit):
                cg.setup()
        run.assert_not_called()

    def test_failed_rmdir_is_reported_while_the_cgroup_still_exists(self):
        cg = corpus_mem.CgroupSet("/sys/fs/cgroup/cbmem-owned.slice")
        with mock.patch.object(
                corpus_mem, "sh_bounded",
                return_value=Completed(1, "", "Device or resource busy")), \
             mock.patch("os.path.isdir", return_value=True):
            with self.assertRaises(RuntimeError):
                cg.rm("leaf")


class RunOutputOwnership(unittest.TestCase):
    """A reused result path must not mix records from different runs."""

    def test_results_directory_can_only_be_claimed_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "memory-run")
            corpus_mem.claim_results_dir(results)
            marker = os.path.join(results, "prior-result")
            with open(marker, "w") as handle:
                handle.write("prior\n")
            with self.assertRaises(SystemExit):
                corpus_mem.claim_results_dir(results)
            with open(marker) as handle:
                self.assertEqual(handle.read(), "prior\n")


class SnapshotIdentity(unittest.TestCase):
    """A tag's existence does not identify the image or resolver it captured."""

    @staticmethod
    def generation(**overrides):
        record = {
            "image": "localhost/chromium-bench-req",
            "image_id": "sha256:" + "a" * 64,
            "guest_dns": "10.0.2.2",
            "dns_server": "10.0.2.2",
            "guest_env": [],
        }
        record.update(overrides)
        return record

    def test_matching_snapshot_identity_is_accepted(self):
        corpus_mem.validate_snapshot_for_benchmark(
            self.generation(), "localhost/chromium-bench-req",
            "sha256:" + "a" * 64, "10.0.2.2")

    def test_snapshot_of_another_image_is_refused(self):
        with self.assertRaises(SystemExit):
            corpus_mem.validate_snapshot_for_benchmark(
                self.generation(), "localhost/chromium-bench-req",
                "sha256:" + "b" * 64, "10.0.2.2")

    def test_snapshot_without_the_replay_resolver_is_refused(self):
        with self.assertRaises(SystemExit):
            corpus_mem.validate_snapshot_for_benchmark(
                self.generation(guest_dns=None, dns_server="127.0.0.53"),
                "localhost/chromium-bench-req", "sha256:" + "a" * 64,
                "10.0.2.2")


class ArgumentValidation(unittest.TestCase):
    """An empty measurement grid is not a successful run."""

    @staticmethod
    def args(**overrides):
        args = SimpleNamespace(
            urls=["https://example.com/"], ns=[1, 2, 4, 8], reps=2,
            settle=5.0, quiet_limit=1.0,
            quiet_wait=300.0, run_id="a" * 32,
            container_owner_token="b" * 32,
            source_revision="c" * 40,
            runtime_bundle_sha256="d" * 64,
            corpus_extra_runtime_bundle_sha256="e" * 64,
        )
        for key, value in overrides.items():
            setattr(args, key, value)
        return args

    def assert_refused(self, **overrides):
        with self.assertRaises(SystemExit) as caught:
            corpus_mem.validate_args(self.args(**overrides))
        self.assertNotIn(caught.exception.code, (0, None))

    def test_empty_or_nonpositive_grids_are_refused(self):
        for overrides in ({"ns": []}, {"ns": [0, 2]}, {"ns": [-1]},
                          {"reps": 0}, {"reps": -1}, {"urls": []}):
            with self.subTest(overrides=overrides):
                self.assert_refused(**overrides)

    def test_invalid_run_id_is_refused(self):
        for value in ("", "short", "A" * 32, "a/b", "has space", "a" * 100):
            with self.subTest(run_id=value):
                self.assert_refused(run_id=value)

    def test_valid_arguments_are_accepted(self):
        corpus_mem.validate_args(self.args())

    def test_unpaired_cpu_measurement_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = subprocess.run(
                [sys.executable, CORPUS_MEM, "--results", os.path.join(tmp, "out"),
                 "--tag", "tag", "--urls", "https://example.com/",
                 "--source-revision", "a" * 40,
                 "--runtime-bundle-sha256", "b" * 64,
                 "--corpus-extra-runtime-bundle-sha256", "c" * 64,
                 "--cputime-reps", "1"],
                capture_output=True, text=True, timeout=30,
            )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unrecognized arguments: --cputime-reps 1", proc.stderr)

    def test_invalid_container_owner_token_is_refused(self):
        for value in ("", "short", "A" * 32, "a/b", "has space", "a" * 100):
            with self.subTest(container_owner_token=value):
                self.assert_refused(container_owner_token=value)

    def test_invalid_runtime_identity_is_refused(self):
        fields = (
            ("source_revision", ("", "abc", "g" * 40, "a" * 41)),
            ("runtime_bundle_sha256", ("", "a" * 63, "g" * 64)),
            ("corpus_extra_runtime_bundle_sha256", ("", "a" * 65, "G" * 64)),
        )
        for field, values in fields:
            for value in values:
                with self.subTest(field=field, value=value):
                    self.assert_refused(**{field: value})

    def test_csv_parsing_rejects_empty_members(self):
        for value in ("", ",", "one,", ",one", "one,,two"):
            with self.subTest(value=value):
                with self.assertRaises(SystemExit):
                    corpus_mem.parse_csv(value, "--values")
        self.assertEqual(corpus_mem.parse_csv("one,two", "--values"),
                         ["one", "two"])


class CanonicalImageIdentity(unittest.TestCase):
    """Podman prints bare IDs while snapshot provenance stores sha256: IDs."""

    def test_bare_and_prefixed_ids_have_one_identity(self):
        digest = "a" * 64
        self.assertEqual(corpus_mem.canonical_image_id(digest), "sha256:" + digest)
        self.assertEqual(corpus_mem.canonical_image_id("sha256:" + digest),
                         "sha256:" + digest)

    def test_malformed_image_id_is_refused(self):
        for value in ("", "a" * 63, "sha256:" + "g" * 64):
            with self.subTest(value=value):
                with self.assertRaises(SystemExit):
                    corpus_mem.canonical_image_id(value)


class ExactHostListener(unittest.TestCase):
    """A generic host port can belong to another Chromium process."""

    def test_listener_probe_runs_inside_the_named_container(self):
        seen = []

        def bounded(cmd, timeout):
            seen.append((cmd, timeout))
            return Completed(0, "", "")

        with mock.patch.object(corpus_mem, "sh_bounded", bounded):
            self.assertTrue(corpus_mem.container_owns_tcp_listener("owned", 9222))
        self.assertEqual(seen[0][0][:3], ["podman", "exec", "owned"])
        self.assertIn("9222", seen[0][0])

    def test_listener_probe_rejects_an_unowned_port(self):
        with mock.patch.object(corpus_mem, "sh_bounded",
                               return_value=Completed(1, "", "not owned")):
            self.assertFalse(corpus_mem.container_owns_tcp_listener("owned", 9222))


class Resummarize(unittest.TestCase):
    """A recomputed host summary must describe one complete successful run.

    resummarize.py exists to restate a hostcdp run's p50 under the median
    convention reqanalyze publishes, so the ratio compare.py takes against the
    VM arm is between two numbers computed the same way. It overwrites the
    summary.json hostcdp.sh wrote.

    hostcdp.sh can write "failures": 0 because it exits 4 on the first failed
    rep, so a summary it reaches is a run with none. resummarize.py has no such
    process invariant: it is pointed at a directory. It must prove the declared
    record count, schedule, and successes, and remove an earlier summary when
    that proof fails.
    """

    @staticmethod
    def run_on(rows, meta=None, stale_summary=False):
        tmp = tempfile.mkdtemp()
        if meta is None:
            warmup = sum(r.get("warmup") is True for r in rows)
            urls = list(dict.fromkeys(r.get("url") for r in rows))
            meta = {
                "reps": len(rows) - warmup,
                "warmup": warmup,
                "total_reps": len(rows),
                "urls": urls,
                "url_count": len(urls),
            }
        if meta is not False:
            meta = dict(meta)
            meta.setdefault("run_id", "resummarize-fixture")
            with open(os.path.join(tmp, "run.json"), "w") as handle:
                json.dump(meta, handle)
            with open(os.path.join(tmp, "run.json"), "rb") as handle:
                run_json_sha256 = hashlib.sha256(handle.read()).hexdigest()
        else:
            run_json_sha256 = None
        with open(os.path.join(tmp, "hostcdp.jsonl"), "w") as handle:
            for r in rows:
                record = dict(r)
                if run_json_sha256 is not None:
                    record["run_json_sha256"] = run_json_sha256
                handle.write(json.dumps(record) + "\n")
        if stale_summary:
            with open(os.path.join(tmp, "summary.json"), "w") as handle:
                json.dump({"n": 999, "failures": 0, "passed": True}, handle)
        proc = subprocess.run([sys.executable, os.path.join(HERE, "resummarize.py"), tmp],
                              capture_output=True, text=True, timeout=60)
        return tmp, proc

    @staticmethod
    def rep(rep, ok=True, warmup=False, wall_ms=100.0, load=0.5):
        return {"rep": rep, "ok": ok, "warmup": warmup, "wall_ms": wall_ms,
                "loadavg1": load, "url": "https://example.com/", "driver": "{}"}

    def test_a_clean_run_is_summarised(self):
        rows = [self.rep(0, warmup=True, wall_ms=900.0)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 6)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["n"], 5)
        self.assertEqual(rec["failures"], 0)
        self.assertEqual(rec["p50_ms"], 103.0)

    def test_a_complete_legacy_run_is_summarised(self):
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 6)]
        meta = {"reps": 6, "warmup": 1,
                "urls": ["https://example.com/"], "url_count": 1}
        tmp, proc = self.run_on(rows, meta=meta)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["n"], 5)
        self.assertEqual(rec["p50_ms"], 103.0)

    def test_a_run_holding_a_failed_rep_is_refused(self):
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i)) for i in range(1, 5)] + \
               [self.rep(5, ok=False, wall_ms=30000.0)]
        tmp, proc = self.run_on(rows)
        self.assertNotEqual(proc.returncode, 0,
                            "a run with a failed rep was summarised; its timeout "
                            f"is now in the p95\n{proc.stdout}{proc.stderr}")
        # A refusal, not a traceback: the unfixed script died inside
        # statistics.median on the empty-run case, which is also a non-zero
        # exit and would satisfy a bare returncode check.
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("1 of 5 measured reps", proc.stderr, proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "a refused run still left a quotable summary.json")

    def test_the_contention_record_survives_the_overwrite(self):
        """hostcdp.sh writes loadavg1_measured; overwriting must not drop it.

        It is the field that answers "was the box busy while this was measured",
        and a summary.json that lost it cannot be checked against that question
        any more.
        """
        rows = [self.rep(0, warmup=True)] + \
               [self.rep(i, wall_ms=float(100 + i), load=0.2 * i) for i in range(1, 6)]
        tmp, proc = self.run_on(rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(tmp, "summary.json")) as handle:
            rec = json.load(handle)
        self.assertIsNotNone(rec.get("loadavg1_measured"),
                             "the recomputed summary dropped the contention record")
        self.assertEqual(rec["loadavg1_measured"]["n"], 5)

    def test_a_run_with_no_measured_reps_is_refused(self):
        """Refused, not crashed. The unfixed script raised StatisticsError from
        an empty median, which exits non-zero for a reason no reader can act on."""
        tmp, proc = self.run_on([self.rep(0, warmup=True)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_a_truncated_successful_prefix_is_refused(self):
        """Every row present can say ok=true while the run is still partial.

        The producer records the promised measured, warmup, and total counts in
        run.json. Four successful measured rows are not a completed five-rep
        run, so resummarizing them would turn interruption into a fast-looking
        result with failures=0.
        """
        rows = [self.rep(0, warmup=True)] + [self.rep(i) for i in range(1, 5)]
        meta = {"reps": 5, "warmup": 1, "total_reps": 6,
                "urls": ["https://example.com/"], "url_count": 1}
        tmp, proc = self.run_on(rows, meta=meta)
        self.assertNotEqual(proc.returncode, 0,
                            "a successful prefix was published as a completed run")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("total_reps", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))

    def test_a_refusal_removes_an_earlier_successful_summary(self):
        """A non-zero exit beside passed-looking summary.json fails open."""
        rows = [self.rep(0, warmup=True), self.rep(1),
                self.rep(2, ok=False, wall_ms=30000.0)]
        tmp, proc = self.run_on(rows, stale_summary=True)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")),
                         "the refused recomputation left the stale successful summary")

    def test_missing_run_metadata_is_refused(self):
        rows = [self.rep(0, warmup=True), self.rep(1)]
        tmp, proc = self.run_on(rows, meta=False)
        self.assertNotEqual(proc.returncode, 0,
                            "records with no declared count were summarized")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("run.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(tmp, "summary.json")))


class FrozenCopiesAreRecordsNotTests(unittest.TestCase):
    """The sealed harness copies under results/ are provenance, not code CI runs.

    A run freezes the scripts that produced it beside its records, and some of
    those are test files. CI's Bench Harness Tests job is
    `unittest discover -s bench/chromium -p 'test_*.py'`, so a frozen copy that
    got loaded would run a stale test against the current tree: green or red for
    reasons that have nothing to do with the commit under test. It does not
    happen today because results/ holds no package, and this says so out loud
    rather than leaving it to a property nobody wrote down.
    """

    def test_discovery_loads_nothing_from_results(self):
        import unittest as ut
        suite = ut.defaultTestLoader.discover(HERE, pattern="test_*.py")

        def modules(t):
            for item in t:
                if isinstance(item, ut.TestSuite):
                    yield from modules(item)
                else:
                    yield type(item).__module__

        for mod in sorted(set(modules(suite))):
            path = getattr(sys.modules.get(mod), "__file__", "") or ""
            self.assertNotIn(os.sep + "results" + os.sep, path,
                             f"CI would run the frozen copy {path}")


class ArchivedCorpusExtraDisposition(unittest.TestCase):
    """Legacy artifacts stay available, but no unprovable result is quotable."""

    RESULTS = os.path.join(HERE, "results")

    def marker(self, directory):
        path = os.path.join(self.RESULTS, directory, "WITHDRAWN")
        self.assertTrue(os.path.isfile(path), f"{path} is missing")
        with open(path) as handle:
            reason = handle.readline().strip()
        self.assertTrue(reason, f"{path} names no reason")

    def test_legacy_host_ratio_is_withdrawn_not_relabelled(self):
        directory = "corpusextra-hostcdp-20260830-172413"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory)
        self.assertFalse(os.path.exists(os.path.join(root, "comparison.json")),
                         "the unbound host ratio remains quotable")
        host = os.path.join(root, "hostcdp-free")
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        self.assertNotIn("run_id", meta,
                         "do not invent an identity the old producer never recorded")
        with open(os.path.join(host, "hostcdp.jsonl")) as handle:
            row = json.loads(next(handle))
        self.assertNotIn("run_json_sha256", row)
        self.assertNotIn("loadavg1", row)

    def test_failed_memory_attempt_is_permanently_withdrawn(self):
        directory = "corpusextra-memory-20260830-173915"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory, "memory")
        self.assertFalse(os.path.exists(os.path.join(root, "summary.json")))
        with open(os.path.join(root, "phase.log")) as handle:
            last = [line.strip() for line in handle if line.strip()][-1]
        self.assertIn(" BLOCKED:", last)

    def test_blocked_memory_and_cpu_design_is_permanently_withdrawn(self):
        directory = "corpusextra-memory-20260830-181830"
        self.marker(directory)
        root = os.path.join(self.RESULTS, directory, "memory")
        with open(os.path.join(root, "run.json")) as handle:
            meta = json.load(handle)
        self.assertNotIn("schedule", meta)
        self.assertNotIn("snapshot_generation", meta)
        with open(os.path.join(root, "samples.jsonl")) as handle:
            sides = [json.loads(line)["side"] for line in handle]
        switches = sum(left != right for left, right in zip(sides, sides[1:]))
        self.assertEqual(switches, 1,
                         "the invalidating blocked-side order changed; re-audit the archive")


class ComparePublicationGate(unittest.TestCase):
    """compare.py divides a VM p50 by a host p50 and writes comparison.json.

    The comparison must bind the raw VM bytes to the analysis that passed its
    publication gate, and must prove that every scheduled VM and host input is
    complete and compatible before it computes a ratio.
    """

    URL = "https://example.com/"
    IMAGE_ID = "sha256:" + "a" * 64
    CELL = {"cpu": 2, "memory_mib": 1024, "backend": "uffd", "uffd_mode": "minor",
            "snapshot": "cb-req-corpus", "image": "localhost/chromium-bench-req",
            "image_id": IMAGE_ID, "url": URL, "urls": [URL],
            "guest_dns": "10.0.2.2",
            "guest_env": [], "engine": "chromium", "cdp_port": 9222,
            "source_revision": "b" * 40, "harness_sha256": "c" * 64,
            "fcvm_sha256": "d", "runtime_bundle_sha256": "e" * 64,
            "host_boot_id": "00000000-0000-4000-8000-000000000001",
            "host_kernel_release": "k", "host_machine": "aarch64"}

    @staticmethod
    def artifact_identity(path):
        with open(path, "rb") as handle:
            raw = handle.read()
        return {"path": path, "realpath": os.path.realpath(path),
                "size": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}

    def make_run(self, publishable=True, passed=True, vm_rows=(), cell=None,
                 include_identity=True, warmup=0, meta_overrides=None):
        tmp = tempfile.mkdtemp()
        cell = dict(cell or self.CELL)
        measured = [dict(row) for row in vm_rows]
        template = dict(measured[0]) if measured else self.vm_rep(1.0)
        run_id = "r1"
        urls = list(cell.get("urls") or [cell["url"]])

        scheduled = []
        for rep in range(warmup):
            row = dict(template)
            row.update(rep=rep, warmup=True, url=urls[rep % len(urls)],
                       run_id=run_id,
                       record_id=f"{run_id}:cdp:{rep}:1")
            scheduled.append(row)
        for offset, original in enumerate(measured):
            rep = warmup + offset
            row = dict(original)
            row.update(rep=rep, warmup=False, url=urls[rep % len(urls)],
                       run_id=run_id,
                       record_id=f"{run_id}:cdp:{rep}:0")
            scheduled.append(row)

        records = os.path.join(tmp, "reqbench.jsonl")
        meta = {
            "kind": "meta", "run_id": run_id, "seed": 1,
            "arms": ["cdp"], "reps": len(measured), "warmup": warmup,
            "url": ",".join(urls), "urls": urls,
            "image": cell["image"], "image_id": cell["image_id"],
            "guest_dns": cell["guest_dns"], "guest_env": cell["guest_env"],
            "source_revision": cell["source_revision"],
            "harness_sha256": cell["harness_sha256"],
            "runtime_bundle_sha256": cell["runtime_bundle_sha256"],
            "host_boot_id": cell["host_boot_id"],
            "host_kernel_release": cell["host_kernel_release"],
            "host_machine": cell["host_machine"],
            "cpu": cell["cpu"],
        }
        if meta_overrides:
            meta.update(meta_overrides)
        with open(records, "w") as handle:
            handle.write(json.dumps(meta) + "\n")
            for row in scheduled:
                handle.write(json.dumps(row) + "\n")
        analysis = {"publishable": publishable, "gate": {"passed": passed},
                    "run_id": "r1", "cell": cell}
        if include_identity:
            analysis["analysis_identity"] = {
                "schema_version": 6,
                "inputs": [self.artifact_identity(records)],
            }
        with open(os.path.join(tmp, "analysis.json"), "w") as handle:
            json.dump(analysis, handle)
        return tmp

    def rewrite_vm_records(self, run, mutate):
        records = os.path.join(run, "reqbench.jsonl")
        with open(records) as handle:
            rows = [json.loads(line) for line in handle]
        mutate(rows)
        with open(records, "w") as handle:
            for row in rows:
                handle.write(json.dumps(row) + "\n")
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        analysis["analysis_identity"]["inputs"] = [self.artifact_identity(records)]
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)

    def make_two_arm_run(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])

        def add_noop(rows):
            meta, cdp_rows = rows[0], rows[1:]
            meta["arms"] = ["cdp", "noop"]
            schedule = []
            rng = random.Random(meta["seed"])
            for rep, cdp in enumerate(cdp_rows):
                order = list(meta["arms"])
                rng.shuffle(order)
                for arm in order:
                    if arm == "cdp":
                        schedule.append(cdp)
                    else:
                        schedule.append({
                            "arm": "noop", "rep": rep, "warmup": False,
                            "run_id": meta["run_id"],
                            "record_id": f"{meta['run_id']}:noop:{rep}:0",
                            "url": meta["urls"][rep % len(meta["urls"])],
                            "ok": True, "blocking_ms": 40.0,
                            "wall_ms": 200.0,
                        })
            rows[:] = [meta, *schedule]

        self.rewrite_vm_records(run, add_noop)
        return run

    @staticmethod
    def vm_rep(blocking_ms, ok=True, include_ok=True):
        rec = {"arm": "cdp", "warmup": False, "blocking_ms": blocking_ms,
               "wall_ms": blocking_ms, "url": ComparePublicationGate.URL,
               "render": {"ok": True, "url": ComparePublicationGate.URL,
                          "stages": {"total_ms": blocking_ms},
                          "nav": {"load_ms": blocking_ms}}}
        if include_ok:
            rec["ok"] = ok
        return rec

    @staticmethod
    def host_rep(rep, warmup, url=URL, ok=True, wall_ms=100.0,
                 complete_driver=True):
        driver = {"ok": True, "url": url,
                  "stages": {"total_ms": wall_ms},
                  "nav": {"load_ms": wall_ms}}
        if not complete_driver:
            driver = {"ok": True, "url": url, "stages": {},
                      "nav": {"load_ms": wall_ms}}
        return {"rep": rep, "ok": ok, "warmup": warmup,
                "wall_ms": wall_ms, "loadavg1": 0.2,
                "url": url, "driver": json.dumps(driver)}

    def make_host(self, rows, meta_overrides=None):
        tmp = tempfile.mkdtemp()
        warmup = sum(r["warmup"] is True for r in rows)
        urls = list(dict.fromkeys(r["url"] for r in rows))
        meta = {
            "run_id": os.path.basename(tmp),
            "image": self.CELL["image"], "image_id": self.IMAGE_ID.removeprefix("sha256:"),
            "reps": len(rows) - warmup, "warmup": warmup,
            "total_reps": len(rows), "url": ",".join(urls),
            "urls": urls, "url_count": len(urls), "cdp_port": 9222,
            "comparison_label": "host", "cpu_budget": "vm-matched",
            "cpus": 2, "driver": "cdpdrive.py",
            "network": "host (no VM, no DNAT)",
            "resolve_all_to": "127.0.0.1", "host_kernel": "k",
            "host_boot_id": self.CELL["host_boot_id"],
            "host_machine": self.CELL["host_machine"],
            "source_revision": self.CELL["source_revision"],
            "harness_sha256": self.CELL["harness_sha256"],
            "runtime_bundle_sha256": self.CELL["runtime_bundle_sha256"],
            "hostcdp_sha256": "f" * 64,
        }
        if meta_overrides:
            meta.update(meta_overrides)
        with open(os.path.join(tmp, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(tmp, rows)
        return tmp

    @staticmethod
    def bind_host_rows(host, rows=None):
        if rows is None:
            with open(os.path.join(host, "hostcdp.jsonl")) as handle:
                rows = [json.loads(line) for line in handle]
        with open(os.path.join(host, "run.json"), "rb") as handle:
            run_json_sha256 = hashlib.sha256(handle.read()).hexdigest()
        with open(os.path.join(host, "hostcdp.jsonl"), "w") as handle:
            for row in rows:
                record = dict(row)
                record["run_json_sha256"] = run_json_sha256
                handle.write(json.dumps(record) + "\n")

    def run_compare(self, run_dir, hosts=(), out=None, script=None):
        out = out or os.path.join(run_dir, "comparison.json")
        argv = [sys.executable, script or os.path.join(HERE, "compare.py"),
                "--vm-run", run_dir]
        for label, host_dir in hosts:
            argv.extend(["--host", f"{label}={host_dir}"])
        argv.extend(["--out", out])
        return subprocess.run(
            argv,
            capture_output=True, text=True, timeout=60), out

    def test_output_cannot_name_or_alias_any_input(self):
        for input_name in ("analysis", "reqbench", "host-run", "host-rows"):
            for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
                with self.subTest(input=input_name, alias=alias_kind):
                    run, host = self.valid_comparison()
                    inputs = {
                        "analysis": os.path.join(run, "analysis.json"),
                        "reqbench": os.path.join(run, "reqbench.jsonl"),
                        "host-run": os.path.join(host, "run.json"),
                        "host-rows": os.path.join(host, "hostcdp.jsonl"),
                    }
                    protected = inputs[input_name]
                    if alias_kind == "direct":
                        out = protected
                    elif alias_kind == "realpath":
                        alias_root = tempfile.mkdtemp()
                        os.symlink(os.path.dirname(protected),
                                   os.path.join(alias_root, "through-directory"))
                        out = os.path.join(alias_root, "through-directory",
                                           os.path.basename(protected))
                    else:
                        out = os.path.join(run, f"output-{input_name}-{alias_kind}")
                        if alias_kind == "symlink":
                            os.symlink(protected, out)
                        else:
                            os.link(protected, out)
                    with open(protected, "rb") as handle:
                        before = handle.read()
                    proc, _ = self.run_compare(run, [("host", host)], out=out)
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"--out accepted a {alias_kind} alias of {input_name}",
                    )
                    self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                    with open(protected, "rb") as handle:
                        self.assertEqual(
                            handle.read(), before,
                            f"--out destroyed or rewrote {input_name}",
                        )
                    if alias_kind in ("symlink", "hardlink"):
                        self.assertTrue(
                            os.path.lexists(out),
                            "the refused alias itself was unlinked",
                        )

    def test_output_cannot_alias_the_running_comparator(self):
        for alias_kind in ("direct", "realpath", "symlink", "hardlink"):
            with self.subTest(alias=alias_kind):
                run, host = self.valid_comparison()
                with tempfile.TemporaryDirectory() as tmp:
                    copied = os.path.join(tmp, "compare.py")
                    shutil.copyfile(os.path.join(HERE, "compare.py"), copied)
                    if alias_kind == "direct":
                        out = copied
                    elif alias_kind == "realpath":
                        os.mkdir(os.path.join(tmp, "unused"))
                        out = os.path.join(tmp, "unused", "..", "compare.py")
                    else:
                        out = os.path.join(tmp, f"output-{alias_kind}")
                        if alias_kind == "symlink":
                            os.symlink(copied, out)
                        else:
                            os.link(copied, out)
                    with open(copied, "rb") as handle:
                        before = handle.read()
                    proc, _ = self.run_compare(run, [("host", host)],
                                               out=out, script=copied)
                    self.assertNotEqual(
                        proc.returncode, 0,
                        f"the comparator accepted its {alias_kind} source alias",
                    )
                    self.assertIn("alias", proc.stderr.lower(), proc.stderr)
                    with open(copied, "rb") as handle:
                        self.assertEqual(handle.read(), before)
                    if alias_kind in ("symlink", "hardlink"):
                        self.assertTrue(os.path.lexists(out))

    def test_output_parent_cannot_be_swapped_onto_an_input_after_preflight(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as tmp:
            output_parent = os.path.join(tmp, "output")
            moved_parent = os.path.join(tmp, "moved-output")
            os.mkdir(output_parent)
            out = os.path.join(output_parent, "analysis.json")
            with open(out, "w") as handle:
                json.dump({"publishable": True, "stale": True}, handle)

            original = bench_compare.reject_output_alias

            def swap_parent_after_preflight(*args, **kwargs):
                protected_inputs = original(*args, **kwargs)
                os.rename(output_parent, moved_parent)
                os.symlink(run, output_parent)
                return protected_inputs

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=swap_parent_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            self.assertTrue(os.path.exists(protected),
                            "a parent swap let stale-output removal delete an input")
            with open(protected, "rb") as handle:
                self.assertEqual(handle.read(), before)

    def test_input_moved_onto_output_after_preflight_is_not_unlinked(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            original = bench_compare.reject_output_alias

            def move_input_after_preflight(*args, **kwargs):
                protected_inputs = original(*args, **kwargs)
                os.rename(protected, out)
                return protected_inputs

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=move_input_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            self.assertTrue(os.path.exists(out),
                            "stale-output removal unlinked the raced input")
            with open(out, "rb") as handle:
                self.assertEqual(handle.read(), before)

    def test_output_replaced_after_preflight_is_not_deleted(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            concurrent = os.path.join(output_parent, "concurrent.json")
            with open(out, "wb") as handle:
                handle.write(b"stale comparison\n")
            replacement = b"concurrent writer result\n"
            with open(concurrent, "wb") as handle:
                handle.write(replacement)
            original = bench_compare.reject_output_alias

            def replace_output_after_preflight(*args, **kwargs):
                preflight = original(*args, **kwargs)
                os.replace(concurrent, out)
                return preflight

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "reject_output_alias",
                    side_effect=replace_output_after_preflight), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(out, "rb") as handle:
                self.assertEqual(
                    handle.read(), replacement,
                    "stale-output removal deleted a concurrent writer's inode",
                )

    def test_output_parent_cannot_be_swapped_onto_an_input_before_publish(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as tmp:
            output_parent = os.path.join(tmp, "output")
            moved_parent = os.path.join(tmp, "moved-output")
            os.mkdir(output_parent)
            out = os.path.join(output_parent, "analysis.json")
            original = bench_compare.write_json_atomic

            def swap_parent_before_publish(*args, **kwargs):
                os.rename(output_parent, moved_parent)
                os.symlink(run, output_parent)
                return original(*args, **kwargs)

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "write_json_atomic",
                    side_effect=swap_parent_before_publish), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(protected, "rb") as handle:
                self.assertEqual(
                    handle.read(), before,
                    "a parent swap let atomic publication replace an input",
                )

    def test_input_moved_onto_output_before_publish_is_not_replaced(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        protected = os.path.join(run, "analysis.json")
        with open(protected, "rb") as handle:
            before = handle.read()
        with tempfile.TemporaryDirectory() as output_parent:
            out = os.path.join(output_parent, "comparison.json")
            original = bench_compare.write_json_atomic

            def move_input_before_publish(*args, **kwargs):
                os.rename(protected, out)
                return original(*args, **kwargs)

            argv = ["compare.py", "--vm-run", run, "--out", out]
            with mock.patch.object(
                    bench_compare, "write_json_atomic",
                    side_effect=move_input_before_publish), \
                    mock.patch.object(sys, "argv", argv):
                with self.assertRaises(bench_compare.Refusal):
                    bench_compare.main()

            with open(out, "rb") as handle:
                self.assertEqual(
                    handle.read(), before,
                    "atomic publication replaced the raced input",
                )

    def test_a_run_that_failed_its_gate_is_refused(self):
        run = self.make_run(passed=False, vm_rows=[self.vm_rep(700.0)])
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a run that did not pass its publication gate was quoted")
        self.assertFalse(os.path.exists(out))

    def test_a_refusal_removes_an_earlier_comparison(self):
        """A failed rerun beside an old comparison fails open."""
        run = self.make_run(passed=False, vm_rows=[self.vm_rep(700.0)])
        out = os.path.join(run, "comparison.json")
        with open(out, "w") as handle:
            json.dump({"publishable": True, "stale": True}, handle)
        proc, returned_out = self.run_compare(run)
        self.assertEqual(returned_out, out)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(out),
                         "the refused rerun left an earlier comparison quotable")

    def test_an_unreadable_nonalias_input_still_removes_an_earlier_comparison(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        out = os.path.join(run, "comparison.json")
        with open(out, "w") as handle:
            json.dump({"publishable": True, "stale": True}, handle)
        records = os.path.join(run, "reqbench.jsonl")
        os.unlink(records)
        os.symlink("reqbench.jsonl", records)
        proc, _ = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(
            os.path.exists(out),
            "alias preflight left an earlier comparison quotable when stat failed",
        )

    def test_an_unpublishable_run_is_refused(self):
        run = self.make_run(publishable=False, vm_rows=[self.vm_rep(700.0)])
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(os.path.exists(out))

    def test_a_passing_run_is_summarised(self):
        """The positive control: without it the two refusals could pass for
        any reason at all."""
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        proc, out = self.run_compare(run)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["vm"]["blocking_ms"]["p50"], 700.0)
        self.assertEqual(rec["vm"]["blocking_ms"]["n"], 3)

    def test_a_vm_record_that_does_not_say_it_succeeded_is_refused(self):
        rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        rows.append(self.vm_rep(30000.0, include_ok=False))
        run = self.make_run(vm_rows=rows)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a missing VM success silently reduced n")
        self.assertIn("successful", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_failed_vm_record_is_refused(self):
        rows = [self.vm_rep(600.0), self.vm_rep(700.0, ok=False),
                self.vm_rep(800.0)]
        run = self.make_run(vm_rows=rows)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a failed VM rep silently reduced n")
        self.assertIn("successful", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_meta_run_id_must_match_the_gated_analysis(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        analysis["run_id"] = "different-run"
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "analysis and raw records named different VM runs")
        self.assertIn("run_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_meta_must_be_the_first_record(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows.append(rows.pop(0)))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "records before VM metadata were accepted")
        self.assertIn("first", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_every_vm_row_must_name_the_meta_run_id(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(run_id="different-run"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a row from another VM run entered the medians")
        self.assertIn("run_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_declared_arm_schedule_must_be_complete(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)],
            meta_overrides={"arms": ["cdp", "noop"]},
        )
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a wholly missing declared VM arm was ignored")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_schedule_cannot_have_a_missing_rep(self):
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        self.rewrite_vm_records(run, lambda rows: rows.pop(2))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a gap in the VM schedule silently reduced n")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_schedule_cannot_duplicate_a_rep(self):
        run = self.make_run(vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
        self.rewrite_vm_records(run, lambda rows: rows[2].update(rep=0))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a duplicated VM rep silently replaced a scheduled rep")
        self.assertIn("schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_rep_must_be_an_integer_not_a_boolean(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(run, lambda rows: rows[1].update(rep=False))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "boolean False was accepted as VM rep zero")
        self.assertIn("invalid rep", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_warmup_flag_must_match_its_rep(self):
        run = self.make_run(
            vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        self.rewrite_vm_records(run, lambda rows: rows[-1].update(warmup=True))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a measured VM rep relabelled as warmup silently reduced n")
        self.assertIn("warmup", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_record_id_must_name_its_exact_schedule_cell(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(record_id="r1:cdp:0:0"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a VM row carried another schedule cell's identity")
        self.assertIn("record_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_url_must_match_the_declared_corpus_schedule(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[2].update(url="https://different.example/"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "a different page entered the VM corpus medians")
        self.assertIn("corpus schedule", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_vm_url_and_urls_metadata_must_name_the_same_corpus(self):
        run = self.make_run(vm_rows=[self.vm_rep(600.0), self.vm_rep(700.0)])
        self.rewrite_vm_records(
            run, lambda rows: rows[0].update(url="https://different.example/"))
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "contradictory VM corpus declarations were accepted")
        self.assertIn("corpus", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_every_vm_cdp_success_has_every_compared_metric(self):
        mutations = {
            "blocking_ms": lambda row: row.pop("blocking_ms"),
            "wall_ms": lambda row: row.pop("wall_ms"),
            "total_ms": lambda row: row["render"]["stages"].pop("total_ms"),
            "load_ms": lambda row: row["render"]["nav"].pop("load_ms"),
            "render success": lambda row: row["render"].update(ok=False),
        }
        for metric, mutate in mutations.items():
            with self.subTest(metric=metric):
                run = self.make_run(
                    vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)])
                self.rewrite_vm_records(run, lambda rows: mutate(rows[2]))
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a VM cdp row missing {metric} silently reduced its own n",
                )
                self.assertIn(metric.split()[0], proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_every_vm_noop_success_has_every_compared_metric(self):
        for metric in ("blocking_ms", "wall_ms"):
            with self.subTest(metric=metric):
                run = self.make_two_arm_run()

                def remove_metric(rows):
                    next(row for row in rows if row.get("arm") == "noop").pop(metric)

                self.rewrite_vm_records(run, remove_metric)
                proc, out = self.run_compare(run)
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"a VM noop row missing {metric} silently reduced its own n",
                )
                self.assertIn(metric, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_analysis_must_name_the_current_reqbench_bytes(self):
        rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        run = self.make_run(vm_rows=rows)
        path = os.path.join(run, "reqbench.jsonl")
        with open(path) as handle:
            before = handle.read()
        after = before.replace('"blocking_ms": 800.0',
                               '"blocking_ms": 900.0', 1)
        self.assertNotEqual(after, before)
        self.assertEqual(len(after), len(before),
                         "this test must isolate the content digest from size")
        with open(path, "w") as handle:
            handle.write(after)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0,
                            "compare used records other than the gated input bytes")
        self.assertIn("REFUSING", proc.stderr, proc.stderr)
        self.assertIn("analysis_identity.inputs", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_analysis_without_an_input_identity_is_refused(self):
        run = self.make_run(vm_rows=[self.vm_rep(700.0)], include_identity=False)
        proc, out = self.run_compare(run)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("analysis_identity.inputs", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_analysis_input_identity_is_portable_by_content(self):
        """Archived records move, so inode and absolute path are not identity."""
        run = self.make_run(vm_rows=[self.vm_rep(700.0)])
        analysis_path = os.path.join(run, "analysis.json")
        with open(analysis_path) as handle:
            analysis = json.load(handle)
        recorded = analysis["analysis_identity"]["inputs"][0]
        recorded.update(path="/recording-host/reqbench.jsonl",
                        realpath="/recording-host/reqbench.jsonl",
                        device=123, inode=456, mtime_ns=1, ctime_ns=1)
        with open(analysis_path, "w") as handle:
            json.dump(analysis, handle)
        proc, out = self.run_compare(run)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(os.path.exists(out))

    def valid_comparison(self, host_rows=None, meta_overrides=None, vm_rows=None,
                         vm_warmup=None):
        if vm_rows is None:
            vm_rows = [self.vm_rep(v) for v in (600.0, 700.0, 800.0)]
        if host_rows is None:
            host_rows = [self.host_rep(0, True)] + [
                self.host_rep(i, False, wall_ms=float(100 + i))
                for i in range(1, 4)
            ]
        if vm_warmup is None:
            vm_warmup = sum(row["warmup"] is True for row in host_rows)
        run = self.make_run(vm_rows=vm_rows, warmup=vm_warmup)
        host = self.make_host(host_rows, meta_overrides=meta_overrides)
        return run, host

    def test_a_complete_compatible_host_is_compared(self):
        run, host = self.valid_comparison()
        proc, out = self.run_compare(run, [("host", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["hosts"]["host"]["wall_ms"]["n"], 3)
        self.assertEqual(rec["hosts"]["host"]["driver_total_ms"]["n"], 3)
        identities = rec["input_identity"]
        self.assertEqual(
            identities["reqbench_jsonl"]["sha256"],
            self.artifact_identity(os.path.join(run, "reqbench.jsonl"))["sha256"],
        )
        self.assertEqual(
            identities["hosts"]["host"]["hostcdp_jsonl"]["sha256"],
            self.artifact_identity(os.path.join(host, "hostcdp.jsonl"))["sha256"],
        )

    def test_host_environment_and_producer_must_match_the_vm_arm(self):
        mutations = {
            "host_boot_id": ("00000000-0000-4000-8000-000000000099", "boot"),
            "host_machine": ("x86_64", "machine"),
            "host_kernel": ("totally-other-kernel", "kernel"),
            "source_revision": ("9" * 40, "source_revision"),
            "harness_sha256": ("9" * 64, "harness_sha256"),
            "runtime_bundle_sha256": ("9" * 64, "runtime_bundle_sha256"),
            "hostcdp_sha256": (None, "hostcdp_sha256"),
            "driver": ("different-driver.py", "driver"),
            "network": ("bridge", "network"),
        }
        for field, (value, message) in mutations.items():
            with self.subTest(field=field):
                run, host = self.valid_comparison(
                    meta_overrides={field: value})
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    f"host {field} was not part of compatibility",
                )
                self.assertIn(message, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_host_cpu_budget_and_output_label_are_explicit(self):
        mutations = (
            ({"comparison_label": "not-host"}, "label"),
            ({"cpu_budget": "unlimited", "cpus": 2}, "cpu_budget"),
            ({"cpu_budget": "vm-matched", "cpus": 3}, "cpus"),
            ({"cpu_budget": "vm-matched", "cpus": "2"}, "cpus"),
        )
        for overrides, message in mutations:
            with self.subTest(overrides=overrides):
                run, host = self.valid_comparison(meta_overrides=overrides)
                proc, out = self.run_compare(run, [("host", host)])
                self.assertNotEqual(
                    proc.returncode, 0,
                    "an ambiguous host CPU arm was labelled and ratioed",
                )
                self.assertIn(message, proc.stderr, proc.stderr)
                self.assertFalse(os.path.exists(out))

    def test_the_same_host_dataset_cannot_fill_two_comparison_arms(self):
        run, host = self.valid_comparison()
        proc, out = self.run_compare(
            run, [("host", host), ("second-label", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "one host recording filled two ratio arms")
        self.assertIn("same host dataset", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_arms_must_name_the_same_hostcdp_producer(self):
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, wall_ms=float(100 + i))
            for i in range(1, 4)
        ]
        run = self.make_run(
            vm_rows=[self.vm_rep(v) for v in (600.0, 700.0, 800.0)],
            warmup=1,
        )
        free = self.make_host(rows, meta_overrides={
            "comparison_label": "free", "cpu_budget": "unlimited",
            "cpus": None, "hostcdp_sha256": "f" * 64,
        })
        cpu2 = self.make_host(rows, meta_overrides={
            "comparison_label": "cpu2", "cpu_budget": "vm-matched",
            "cpus": 2, "hostcdp_sha256": "e" * 64,
        })
        proc, out = self.run_compare(
            run, [("free", free), ("cpu2", cpu2)])
        self.assertNotEqual(proc.returncode, 0,
                            "two host producers were presented as one comparison")
        self.assertIn("hostcdp_sha256", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_truncated_successful_host_prefix_is_refused(self):
        rows = [self.host_rep(0, True), self.host_rep(1, False),
                self.host_rep(2, False)]
        run, host = self.valid_comparison(host_rows=rows)
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        meta.update(reps=3, warmup=1, total_reps=4)
        with open(os.path.join(host, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "compare treated a successful prefix as a completed host arm")
        self.assertIn("total_reps", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_rows_must_belong_to_their_run_metadata(self):
        """Separate file hashes do not prevent a metadata/rows splice."""
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, wall_ms=float(100 + i))
            for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        other = self.make_host(
            rows, meta_overrides={"resolve_all_to": "203.0.113.9"})
        with open(os.path.join(other, "hostcdp.jsonl"), "rb") as handle:
            other_rows = handle.read()
        with open(os.path.join(host, "hostcdp.jsonl"), "wb") as handle:
            handle.write(other_rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "rows from another run were attributed to this run.json")
        self.assertIn("run.json", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_measured_count_must_match_the_vm_arm(self):
        host_rows = [self.host_rep(0, True), self.host_rep(1, False),
                     self.host_rep(2, False)]
        run, host = self.valid_comparison(host_rows=host_rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different host and VM sample counts were ratioed")
        self.assertIn("measured", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_warmup_count_must_match_the_vm_arm(self):
        run, host = self.valid_comparison(vm_warmup=2)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "host and VM runs with different warmup schedules were ratioed")
        self.assertIn("warmup", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_corpus_must_match_the_vm_arm(self):
        other = "https://different.example/"
        rows = [self.host_rep(0, True, url=other)] + [
            self.host_rep(i, False, url=other) for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different corpora were presented as one comparison")
        self.assertIn("corpus", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_image_identity_must_match_the_vm_arm(self):
        run, host = self.valid_comparison(meta_overrides={"image_id": "b" * 64})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "different container images were ratioed")
        self.assertIn("image_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_image_identity_must_be_present(self):
        run, host = self.valid_comparison(meta_overrides={"image_id": None})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("image_id", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_resolver_identity_must_match_the_vm_arm(self):
        run, host = self.valid_comparison(
            meta_overrides={"resolve_all_to": "203.0.113.9"})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "a live/other resolver host run was compared to the replay VM run")
        self.assertIn("resolver", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_host_resolver_identity_must_be_present_for_a_hostname_corpus(self):
        run, host = self.valid_comparison(meta_overrides={"resolve_all_to": None})
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("resolver", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))

    def test_a_legacy_total_count_is_interpreted_unambiguously(self):
        """Before total_reps existed, run.json's reps field meant total.

        The tracked 2026-08-30 host records use this documented schema:
        reps=230, warmup=28 means 202 measured. The compatibility rule is
        deterministic and must not reject those complete archived bytes.
        """
        run, host = self.valid_comparison()
        with open(os.path.join(host, "run.json")) as handle:
            meta = json.load(handle)
        del meta["total_reps"]
        meta["reps"] += meta["warmup"]
        with open(os.path.join(host, "run.json"), "w") as handle:
            json.dump(meta, handle)
        self.bind_host_rows(host)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out) as handle:
            rec = json.load(handle)
        self.assertEqual(rec["hosts"]["host"]["wall_ms"]["n"], 3)

    def test_every_host_success_must_have_complete_driver_metrics(self):
        rows = [self.host_rep(0, True)] + [
            self.host_rep(i, False, complete_driver=(i != 2)) for i in range(1, 4)
        ]
        run, host = self.valid_comparison(host_rows=rows)
        proc, out = self.run_compare(run, [("host", host)])
        self.assertNotEqual(proc.returncode, 0,
                            "a missing driver measurement silently reduced its own n")
        self.assertIn("total_ms", proc.stderr, proc.stderr)
        self.assertFalse(os.path.exists(out))


class ProvenanceNamesBytes(unittest.TestCase):
    """A provenance record that cites empty strings cites nothing.

    corpus_extra.sh builds provenance.json by interpolating command
    substitutions into echo lines. A failing sha256sum or podman inspect leaves
    the field empty, echo still exits 0 under `set -e`, and the only gate is a
    json.load -- which an object full of "" passes. The numbers then name no
    binary, no image and no script.
    """

    @staticmethod
    def block():
        with open(EXTRA) as handle:
            src = handle.read()
        m = re.search(r"(\{\n *echo \"\{\".*?\n\} > \"\$RESULTS/provenance\.json\"\n.*?\n)(?=\n)",
                      src, re.S)
        assert m, "the provenance block is gone"
        return m.group(1)

    def drive(self, bench_has_files):
        tmp = tempfile.mkdtemp()
        bench = os.path.join(tmp, "bench")
        results = os.path.join(tmp, "results")
        os.makedirs(bench)
        os.makedirs(os.path.join(tmp, "repo", "target", "release"))
        os.makedirs(results)
        for name in ("corpus_extra.sh", "corpus_mem.py", "hostcdp.sh",
                     "cdpdrive.py", "render.py", "corpus_serve.py", "report.py",
                     "reqbench.py", "reqbench.sh", "reqanalyze.py", "wddrive.py",
                     "owned_process.py", "corpus_campaign.sh", "fcvm"):
            if bench_has_files:
                with open(os.path.join(bench, name), "w") as handle:
                    handle.write(name)
        with open(os.path.join(tmp, "repo", "target", "release", "fcvm"), "w") as handle:
            handle.write("x")
        stub = os.path.join(tmp, "bin")
        os.makedirs(stub)
        with open(os.path.join(stub, "podman"), "w") as f:
            f.write('#!/bin/sh\necho sha256:deadbeef\n')
        with open(os.path.join(stub, "git"), "w") as f:
            f.write('#!/bin/sh\ncase "$*" in *rev-parse*) echo abc123 ;; *) : ;; esac\n')
        for name in ("podman", "git"):
            os.chmod(os.path.join(stub, name), 0o755)
        script = os.path.join(tmp, "run.sh")
        with open(script, "w") as f:
            f.write("set -euo pipefail\n"
                    f'export PATH="{stub}:$PATH"\n'
                    f'REPO="{tmp}/repo"\nBENCH="{bench}"\nRESULTS="{results}"\n'
                    'IMAGE=localhost/x\nTAG=t\nREPS=1\nWARMUP=0\n'
                    'SOURCE_REVISION=abc123\nSOURCE_GIT_DIRTY=\n'
                    'CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n'
                    'REQBENCH_RUNTIME_BUNDLE_SHA256=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n'
                    'RUNTIME_IMAGE=sha256:deadbeef\n'
                    + self.block() + "\necho ACCEPTED\n")
        return subprocess.run(["bash", script], capture_output=True, text=True, timeout=60)

    def test_a_complete_provenance_record_is_accepted(self):
        r = self.drive(bench_has_files=True)
        self.assertIn("ACCEPTED", r.stdout,
                      f"the positive control was refused\n{r.stdout}{r.stderr}")

    def test_a_provenance_record_naming_no_bytes_is_refused(self):
        r = self.drive(bench_has_files=False)
        self.assertNotIn("ACCEPTED", r.stdout,
                         "provenance.json passed its gate while naming no script bytes; "
                         f"the numbers would cite nothing\n{r.stdout}{r.stderr}")
        self.assertNotEqual(r.returncode, 0)


class CorpusExtraRuntimeBundle(unittest.TestCase):
    """The recorded scripts and binary are the immutable bytes that execute."""

    SOURCES = (
        "corpus_extra.sh", "corpus_mem.py", "hostcdp.sh", "cdpdrive.py",
        "render.py", "corpus_serve.py", "report.py", "reqbench.py",
        "reqbench.sh", "reqanalyze.py", "wddrive.py", "owned_process.py",
        "corpus_campaign.sh",
    )
    REQBENCH_SOURCES = (
        "fcvm", "fc-agent", "reqbench.sh", "reqbench.py", "reqanalyze.py",
        "cdpdrive.py", "render.py", "wddrive.py",
    )

    def test_staged_bundle_is_independent_of_later_repository_edits(self):
        with open(EXTRA) as handle:
            source = handle.read()
        match = re.search(r'^stage_runtime_bundle\(\) \{\n.*?^\}', source,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, "corpus-extra has no runtime staging function")
        with tempfile.TemporaryDirectory() as tmp:
            bench = os.path.join(tmp, "source")
            repo = os.path.join(tmp, "repo")
            results = os.path.join(tmp, "results")
            os.makedirs(bench)
            os.makedirs(results)
            os.makedirs(os.path.join(repo, "target", "release"))
            corpus = os.path.join(bench, "corpus-live")
            os.makedirs(corpus)
            with open(os.path.join(corpus, "page.html"), "w") as handle:
                handle.write("original corpus\n")
            for name in self.SOURCES:
                with open(os.path.join(bench, name), "w") as handle:
                    handle.write(f"original {name}\n")
            fcvm = os.path.join(repo, "target", "release", "fcvm")
            with open(fcvm, "w") as handle:
                handle.write("original fcvm\n")
            fc_agent = os.path.join(repo, "target", "release", "fc-agent")
            with open(fc_agent, "w") as handle:
                handle.write("original fc-agent\n")
            script = (
                "set -euo pipefail\n"
                f"SOURCE_BENCH={bench!r}\nREPO={repo!r}\nRESULTS={results!r}\n"
                + match.group(0) + "\n"
                + "stage_runtime_bundle\nprintf '%s\\n%s\\n' \"$BUNDLE_DIR\" \"$REQBENCH_BUNDLE_SHA256\"\n"
            )
            proc = subprocess.run(["bash", "-c", script], capture_output=True,
                                  text=True, timeout=60)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            bundle, reqbench_digest = proc.stdout.strip().splitlines()[-2:]
            with open(os.path.join(bench, "corpus_mem.py"), "w") as handle:
                handle.write("mutated after staging\n")
            with open(os.path.join(bundle, "corpus_mem.py")) as handle:
                self.assertEqual(handle.read(), "original corpus_mem.py\n")
            for name in self.SOURCES + ("fcvm", "fc-agent"):
                self.assertTrue(os.path.isfile(os.path.join(bundle, name)), name)
            with open(os.path.join(bundle, "REQBENCH_MANIFEST.sha256"), "rb") as handle:
                manifest_bytes = handle.read()
            self.assertEqual(hashlib.sha256(manifest_bytes).hexdigest(), reqbench_digest)
            self.assertEqual(
                [line.split()[-1].decode() for line in manifest_bytes.splitlines()],
                list(self.REQBENCH_SOURCES),
            )
            verified = subprocess.run(
                ["sha256sum", "--check", "--status", "MANIFEST.sha256"],
                cwd=bundle, capture_output=True, text=True, timeout=60)
            self.assertEqual(verified.returncode, 0, verified.stderr)

    def test_only_the_staged_bundle_drives_measured_phases(self):
        with open(EXTRA) as handle:
            source = handle.read()
        self.assertIn('bash "$bundle_dir/corpus_extra.sh"', source)
        self.assertIn('verify_runtime_bundle || cleanup_rc=1', source)
        for path in ("$BENCH/hostcdp.sh", "$BENCH/corpus_mem.py",
                     "$BENCH/corpus_serve.py", "$BENCH/fcvm"):
            self.assertIn(path, source)
        self.assertNotIn("CPUTIME_REPS", source)
        self.assertIn('--source-revision "$SOURCE_REVISION"', source)
        self.assertIn('--runtime-bundle-sha256 "$REQBENCH_RUNTIME_BUNDLE_SHA256"', source)
        self.assertIn(
            '--corpus-extra-runtime-bundle-sha256 "$CORPUS_EXTRA_RUNTIME_BUNDLE_SHA256"',
            source,
        )


if __name__ == "__main__":
    unittest.main()
