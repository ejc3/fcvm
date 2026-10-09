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
    def test_the_scale_output_is_handed_back_to_the_invoking_user(self):
        """RED ON 418b7683: reqscale.py runs under sudo and creates its output
        directory as root, so the campaign's analysis, which runs as the
        invoking user, could not write into it. reqbench.sh scale now hands
        the tree back once reqscale.py returns."""
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
            os.makedirs(out)  # what reqscale.py would have created, as root
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
        run = next(i for i, c in enumerate(calls) if "/reqscale.py" in c)
        handback = f"chown -R {os.getuid()}:{os.getgid()} -- {out}"
        self.assertIn(handback, calls[run + 1:],
                      f"the scale output was left to root: sudo calls {calls}")


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

class ScaleGraph(test_reqbench.MakefileBenchGraph):
    def test_scale_never_rebuilds(self):
        c = self.closure("bench-chromium-scale")
        for forbidden in ("build", "setup-default", "cargo-target-link"):
            self.assertNotIn(forbidden, c, f"bench-chromium-scale transitively reaches {forbidden}")


if __name__ == "__main__":
    unittest.main()
