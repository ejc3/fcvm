"""The scale run measures with the runtime that created its golden, as the serial run does."""
import contextlib
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqbench  # noqa: E402
import reqscale  # noqa: E402
import reqscale_analyze  # noqa: E402
import test_reqbench  # noqa: E402
from test_reqscale import CompleteAnalyzerFixture  # noqa: E402

GEN = "11111111-1111-4111-8111-111111111111"
KEY = "a" * 64


def write_golden(data_root, tag, creator):
    snap = os.path.join(data_root, "snapshots", tag)
    os.makedirs(snap)
    paths = {}
    for field, name in (("memory_path", "memory.bin"), ("vmstate_path", "vmstate.bin"),
                        ("disk_path", "disk.raw")):
        paths[field] = os.path.join(snap, name)
        with open(paths[field], "wb") as f:
            f.write(field.encode())
    config = {
        "name": tag, "vm_id": "vm-source", "generation_id": GEN,
        "created_at": "2026-10-08T00:00:00Z", **paths,
        "metadata": {
            "image": "localhost/chromium-bench-req",
            "image_disk_path": f"/image-cache/{KEY}.storage-v2.img",
            "vcpu": 2, "memory_mib": 1024, "network_mode": "rootless",
            "port_mappings": [{"host_ip": None, "host_port": 9222,
                               "guest_port": 9222, "proto": "tcp"}],
        },
    }
    raw = (json.dumps(config, sort_keys=True) + "\n").encode()
    with open(os.path.join(snap, "config.json"), "wb") as f:
        f.write(raw)
    provenance = {
        "snapshot_generation_id": GEN,
        "snapshot_config_sha256": hashlib.sha256(raw).hexdigest(),
        "snapshot_created_at": config["created_at"], "snapshot_vm_id": "vm-source",
        "image": "localhost/chromium-bench-req", "image_id": "sha256:" + "b" * 64,
        "image_digest": "sha256:" + KEY, "image_cache_key": KEY,
        "guest_dns": None, "guest_env": [], **creator,
    }
    with open(os.path.join(snap, "reqbench-provenance.json"), "w") as f:
        json.dump(provenance, f)


class Reached(Exception):
    pass


class ScaleRefusesAGoldenFromAnotherRuntime(unittest.TestCase):
    """reqscale.main() must refuse, before measuring, a golden whose recorded
    creator runtime is not the staged runtime it executes from."""

    def _run(self, d, creator_overrides):
        bundle = os.path.join(d, "bundle")
        os.makedirs(bundle)
        with open(os.path.join(bundle, "fcvm"), "wb") as f:
            f.write(b"#!/bin/sh\nexit 0\n")
        with open(os.path.join(bundle, "MANIFEST.sha256"), "w") as f:
            f.write("fixture manifest\n")
        revision = "e" * 40
        creator = {
            "creator_fcvm_sha256": reqbench.sha256_file(os.path.join(bundle, "fcvm")),
            "creator_runtime_bundle_sha256": reqbench.sha256_file(
                os.path.join(bundle, "MANIFEST.sha256")),
            "source_revision": revision,
        }
        creator.update(creator_overrides)
        data_root = os.path.join(d, "data")
        write_golden(data_root, "golden", creator)
        argv = [
            "reqscale.py", "--snapshot-tag", "golden", "--url", "http://127.0.0.1/x",
            "--rates", "0.8", "--bursts", "5", "--seed", "776",
            "--max-offered-rps-error-pct", "1", "--min-departure-ratio", "0.95",
            "--max-score-end-backlog", "2", "--max-p95-launch-lag-ms", "20",
            "--max-control-median-drift-pct", "10", "--out-dir", os.path.join(d, "out"),
            "--data-root", data_root, "--control-chromium", sys.executable,
            "--control-tmp-root", d, "--cgroup-root", os.path.join(d, "cgroup"),
        ]
        env = {"REQBENCH_RUNTIME_BUNDLE": bundle, "REQBENCH_SOURCE_REVISION": revision,
               "REQBENCH_SOURCE_REPO": d}
        err = io.StringIO()
        reached = False
        with mock.patch.object(sys, "argv", argv), \
                mock.patch.dict(os.environ, env), \
                mock.patch.object(reqscale.os, "geteuid", return_value=0), \
                mock.patch.object(reqscale, "HERE", bundle), \
                mock.patch.object(reqbench, "HERE", bundle), \
                mock.patch.object(reqscale, "collect_provenance", side_effect=Reached), \
                mock.patch.object(reqscale, "execute", side_effect=Reached), \
                contextlib.redirect_stderr(err):
            os.environ.pop("FCVM_FORCE_UFFD", None)
            try:
                rc = reqscale.main()
            except Reached:
                reached, rc = True, None
        return reached, rc, err.getvalue()

    def test_a_golden_created_by_another_runtime_is_refused_before_measuring(self):
        with tempfile.TemporaryDirectory() as d:
            reached, _rc, err = self._run(d, {})
            self.assertTrue(reached, f"the golden's own runtime was refused: {err}")
        for field, value in (("creator_fcvm_sha256", "1" * 64),
                             ("creator_runtime_bundle_sha256", "2" * 64),
                             ("source_revision", "3" * 40)):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as d:
                reached, rc, err = self._run(d, {field: value})
                self.assertFalse(reached, f"measured although the golden records {field}={value}")
                self.assertEqual(rc, 4, err)
                self.assertIn(f"was created with {field}='{value}'", err)


class AnalyzerBindsTheRunToItsGoldensRuntime(unittest.TestCase):
    @staticmethod
    def _bind(provenance):
        # what build_run writes once the fix lands
        provenance["runtime_bundle_sha256"] = "5" * 64
        provenance["golden_creator"] = {
            "creator_fcvm_sha256": provenance["fcvm_sha256"],
            "creator_runtime_bundle_sha256": "5" * 64,
            "source_revision": provenance["source_revision"],
        }

    def test_a_run_not_bound_to_its_goldens_runtime_is_refused(self):
        cases = {
            "unbound": "provenance fields are incomplete or unknown",
            "creator_fcvm_sha256": "creator_fcvm_sha256",
            "creator_runtime_bundle_sha256": "creator_runtime_bundle_sha256",
            "source_revision": "source_revision",
        }
        for case, expected in cases.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as d:
                CompleteAnalyzerFixture.build_run(d)
                path = os.path.join(d, "provenance.json")
                with open(path) as f:
                    provenance = json.load(f)
                if "golden_creator" not in provenance:
                    self._bind(provenance)
                if case == "unbound":
                    del provenance["golden_creator"], provenance["runtime_bundle_sha256"]
                else:
                    width = 40 if case == "source_revision" else 64
                    provenance["golden_creator"][case] = "a" * width
                with open(path, "w") as f:
                    json.dump(provenance, f, sort_keys=True)
                with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, expected):
                    reqscale_analyze.analyze(d)


class ScaleShell(unittest.TestCase):
    def test_scale_runs_reqscale_from_the_staged_bundle(self):
        with tempfile.TemporaryDirectory() as d:
            binx = os.path.join(d, "bin")
            os.makedirs(binx)
            for name, body in (("fcvm", "#!/bin/bash\nexit 0\n"),
                               ("fc-agent", "#!/bin/bash\nexit 0\n"),
                               ("bin/sudo", '#!/bin/bash\nprintf "%s\\n" "$@" > "$SUDO_ARGV"\n')):
                with open(os.path.join(d, name), "w") as f:
                    f.write(body)
                os.chmod(os.path.join(d, name), 0o755)
            os.makedirs(os.path.join(d, "state"))
            env = dict(os.environ, PATH=binx + os.pathsep + os.environ["PATH"],
                       RESULTS=os.path.join(d, "results"), STATE_DIR=os.path.join(d, "state"),
                       RUNID="0" * 32, FCVM=os.path.join(d, "fcvm"),
                       FC_AGENT=os.path.join(d, "fc-agent"), TAG="cb-scale",
                       SUDO_ARGV=os.path.join(d, "argv"))
            result = subprocess.run(
                [os.path.join(HERE, "reqbench.sh"), "scale", "--url", "http://127.0.0.1/x"],
                env=env, capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(env["SUDO_ARGV"]) as f:
                argv = f.read().splitlines()
            script = next(a for a in argv if a.endswith("/reqscale.py"))
            bundle = os.path.dirname(script)
            self.assertEqual(os.path.dirname(bundle), os.path.join(d, "results", "runtime"))
            with open(os.path.join(bundle, "MANIFEST.sha256")) as f:
                sealed = {line.split()[1] for line in f}
            self.assertLessEqual({"fcvm", "reqscale.py", "reqscale_analyze.py", "guardexec.py",
                                  "guardsupervise.py", "faulttrace.bt"}, sealed)
            self.assertIn(f"REQBENCH_RUNTIME_BUNDLE={bundle}", argv)
            self.assertEqual(argv[argv.index("--snapshot-tag") + 1], "cb-scale")
            self.assertEqual(argv[argv.index("--url") + 1], "http://127.0.0.1/x")



class ScaleOutputOwnership(unittest.TestCase):
    def test_a_pre_existing_output_directory_is_not_handed_back(self):
        """RED ON f9f2fd96: reqbench.sh scale changed ownership of --out-dir
        after reqscale.py returned, even when reqscale.py had refused it because
        it already existed, so a mistyped SCALE_OUT could hand an existing
        root-owned tree to the invoking user. Only reqscale.py, which knows
        whether it created the directory, hands it back."""
        with tempfile.TemporaryDirectory() as d:
            binx = os.path.join(d, "bin")
            os.makedirs(binx)
            for name, body in (("fcvm", "#!/bin/bash\nexit 0\n"),
                               ("fc-agent", "#!/bin/bash\nexit 0\n"),
                               ("bin/sudo", '#!/bin/bash\nprintf "%s\\n" "$*" >> "$SUDO_LOG"\n')):
                with open(os.path.join(d, name), "w") as f:
                    f.write(body)
                os.chmod(os.path.join(d, name), 0o755)
            os.makedirs(os.path.join(d, "state"))
            out = os.path.join(d, "results", "scale")
            os.makedirs(out)  # already there before this run: reqscale.py refuses it
            env = dict(os.environ, PATH=binx + os.pathsep + os.environ["PATH"],
                       RESULTS=os.path.join(d, "results"), STATE_DIR=os.path.join(d, "state"),
                       RUNID="0" * 32, FCVM=os.path.join(d, "fcvm"),
                       FC_AGENT=os.path.join(d, "fc-agent"), TAG="cb-scale",
                       SUDO_LOG=os.path.join(d, "sudo.log"))
            result = subprocess.run(
                [os.path.join(HERE, "reqbench.sh"), "scale", "--url", "http://127.0.0.1/x",
                 "--out-dir", out],
                env=env, capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(env["SUDO_LOG"]) as f:
                calls = f.read().splitlines()
        self.assertTrue(any("/reqscale.py" in c for c in calls), calls)
        self.assertEqual([c for c in calls if c.startswith("chown")], [],
                         f"reqbench.sh changed ownership of an existing directory: {calls}")


class HandBackToInvoker(unittest.TestCase):
    def _record(self, uid_env=True, euid=0):
        seen = []
        patches = [mock.patch.object(reqscale.os, "geteuid", return_value=euid),
                   mock.patch.object(reqscale.os, "chown",
                                     side_effect=lambda name, u, g, **kw: seen.append((name, u, g))),
                   mock.patch.object(reqscale.os, "fchown",
                                     side_effect=lambda fd, u, g: seen.append(("<pinned>", u, g)))]
        if uid_env:
            patches.append(mock.patch.dict(os.environ, {"SUDO_UID": "1234", "SUDO_GID": "567"}))
        return seen, patches

    def test_the_handback_follows_the_pinned_directory_not_its_path(self):
        """RED ON a1e0e4bd: the handback walked the run directory by path when
        reqscale.py exited, so a process running as the invoking user could
        rename the directory during the run and leave a symlink in its place,
        and root would change ownership of whatever tree it named. The
        directory is pinned by a descriptor right after mkdir, and the
        handback walks that descriptor without following a symlink."""
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "out")
            os.makedirs(os.path.join(root, "logs"))
            open(os.path.join(root, "logs", "a.json"), "w").close()
            victim = os.path.join(d, "victim")
            os.makedirs(victim)
            open(os.path.join(victim, "secret"), "w").close()
            fd = reqscale.pin_run_directory(root)
            try:
                os.rename(root, root + ".moved")
                os.symlink(victim, root)
                seen, patches = self._record()
                with contextlib.ExitStack() as stack:
                    for patch in patches:
                        stack.enter_context(patch)
                    reqscale.hand_back_to_invoker(fd)
            finally:
                os.close(fd)
        names = sorted(name for name, _u, _g in seen)
        self.assertEqual(names, sorted(["<pinned>", "a.json", "logs"]), names)
        self.assertTrue(all((u, g) == (1234, 567) for _n, u, g in seen), seen)

    def test_a_run_that_is_not_root_changes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            fd = reqscale.pin_run_directory(d)
            try:
                seen, patches = self._record(euid=1000)
                with contextlib.ExitStack() as stack:
                    for patch in patches:
                        stack.enter_context(patch)
                    reqscale.hand_back_to_invoker(fd)
            finally:
                os.close(fd)
        self.assertEqual(seen, [], "a run that is not root changed ownership")

    def test_execute_pins_then_hands_back_before_it_returns(self):
        """RED ON a1e0e4bd: the handback ran only from atexit, so a failed
        chown could not change the exit status of a run that left its output
        root-owned. execute() now hands back explicitly before it returns,
        where an OSError reaches main() and exits 4, and atexit is the
        fallback for an early exit."""
        with open(os.path.join(HERE, "reqscale.py")) as f:
            source = f.read()
        body = source[source.index("def execute(args, schedule: dict, provenance: dict) -> int:"):]
        body = body[:body.index("\ndef ", 10)]
        self.assertEqual([line.strip() for line in body.splitlines()[1:4]],
                         ["os.mkdir(args.out_dir)",
                          "run_dir_fd = pin_run_directory(args.out_dir)",
                          "atexit.register(hand_back_to_invoker, run_dir_fd)"])
        status = body.index('write_json_exclusive(os.path.join(args.out_dir, "status.json"), status)')
        self.assertIn("hand_back_to_invoker(run_dir_fd)", body[status:body.index("if failures:", status)],
                      "execute() returns without handing back its run directory")


class UffdServeRecordsPrefetch(unittest.TestCase):
    def test_the_prefetch_setting_is_required_and_published(self):
        """RED ON a5d0159f: uffd-serve.json recorded only the memory mode, and
        the analyzer neither required nor published the working-set prefetch
        setting, so an on run and an off run read as the same UFFD experiment."""
        with tempfile.TemporaryDirectory() as d:
            CompleteAnalyzerFixture.build_run(d)
            analysis = reqscale_analyze.analyze(d)
        self.assertEqual(analysis.get("uffd"), {"mode": "copy", "prefetch": "on"})
        for case, value in (("missing", None), ("invalid", "maybe")):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as d:
                CompleteAnalyzerFixture.build_run(d)
                path = os.path.join(d, "uffd-serve.json")
                with open(path) as f:
                    serve = json.load(f)
                if value is None:
                    serve.pop("uffd_prefetch", None)
                else:
                    serve["uffd_prefetch"] = value
                with open(path, "w") as f:
                    json.dump(serve, f, sort_keys=True)
                with self.assertRaisesRegex(reqscale_analyze.AnalysisInvalid, "UFFD serve"):
                    reqscale_analyze.analyze(d)

    def test_the_serve_record_carries_the_configured_prefetch(self):
        with open(os.path.join(HERE, "reqscale.py")) as f:
            source = f.read()
        record = source[source.index('"kind": "uffd-serve",'):]
        record = record[:record.index("}")]
        self.assertIn('"uffd_prefetch": getattr(self.args, "uffd_prefetch", "on"),', record)


class ResolverEvidenceGate(unittest.TestCase):
    RUN_ID = "0" * 32

    def test_several_ip_literal_urls_need_no_resolver_evidence(self):
        """RED ON 25906ca7: the gate called any run with more than one URL a
        corpus run, so a standalone run over IP-literal or local URLs, which
        resolve nothing, was refused for lacking the campaign's DNS evidence."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "scale")
            os.mkdir(run_dir)
            gate = reqscale_analyze.corpus_dns_gate(
                run_dir, {"run_id": self.RUN_ID,
                          "urls": ["http://127.0.0.1/a", "http://localhost/b"]},
                {"host_control": {"resolve_all_to": None}})
        self.assertIsNone(gate, gate)

    def test_a_hostname_url_needs_resolver_evidence_even_alone(self):
        """RED ON 25906ca7: one hostname URL was not a corpus run, so it
        published with no record of which resolver answered it."""
        with tempfile.TemporaryDirectory() as d:
            run_dir = os.path.join(d, "scale")
            os.mkdir(run_dir)
            gate = reqscale_analyze.corpus_dns_gate(
                run_dir, {"run_id": self.RUN_ID, "urls": ["https://example.com/"]},
                {"host_control": {"resolve_all_to": None}})
        self.assertIn("without the campaign's DNS evidence", gate or "the gate passed it")

class ScaleGraph(test_reqbench.MakefileBenchGraph):
    def test_scale_never_rebuilds(self):
        c = self.closure("bench-chromium-scale")
        for forbidden in ("build", "setup-default", "cargo-target-link"):
            self.assertNotIn(forbidden, c, f"bench-chromium-scale transitively reaches {forbidden}")


if __name__ == "__main__":
    unittest.main()
