#!/usr/bin/env python3
"""Offline broker/SSM failure tests. No AWS SDK, credentials or paid instances."""
import hashlib
import concurrent.futures
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import pwd
import re
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import tarfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch


HERE = Path(__file__).resolve().parent


def module(name):
    spec = importlib.util.spec_from_file_location(name, HERE / (name + ".py"))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


pipeline = module("pipeline")
guest = module("pipeline_guest")
SOURCE = "1" * 40
IMAGE = "sha256:" + "2" * 64
RENDERS_RECORD = {"fcvm_commit": SOURCE, "kitesurf_commit": "9" * 40, "bundle_sha256": "a" * 64,
    "kernel_release": "7.3.3-fcvm-0123456789ab", "boot_params": "kvm-arm.mode=nested numa=off arm64.nv2",
    "hugepages": 34000}
CONFIG = {"build_id": "fleet-images:example", "source_commit": SOURCE, "capability": "c" * 64,
    "broker_arn": "arn:aws:lambda:us-west-1:111111111111:function:example:build",
    "bucket": "example-images", "final_store_bucket": "example-runtime", "final_store_region": "us-east-2",
    "region": "us-east-1", "asset_region": "us-west-1", "secret_region": "us-east-1",
    "tls_build_arn": "arn:aws:secretsmanager:us-east-1:111111111111:secret:example/build-a",
    "tls_test_arn": "arn:aws:secretsmanager:us-east-1:111111111111:secret:example/test-b",
    "document": "fcvm-fleet-ami-build"}


class Broker:
    def __init__(self):
        self.lock = threading.RLock()
        self.slots = {}
        self.candidates = {}
        self.calls = []
        self.deadline = 9100
        self.input_sha = "3" * 64
        self.source = SOURCE
        self.hide_cleanup_slot = False
        self.change_cleanup_owner = False
        self.existing = False
        self.ready_after = 0
        self.status_reads = 0
        self.midwait_change = None
        self.slow_final_cleanup = False
        self.final_cleanup_had_manifest = []
        self.track = "pair"

    def invoke(self, **arguments):
        request = json.loads(arguments["Payload"])
        with self.lock:
            self.calls.append(request)
            action, slot = request["action"], request.get("slot")
            assert request["build_id"] == CONFIG["build_id"]
            assert set(request) == {"action", "build_id", "capability"} | ({"slot"} if slot else set()) | ({"command_id"} if action == "command_status" else set())
            if request.get("capability") != CONFIG["capability"]:
                return {"Payload": io.BytesIO(b'{"status":"refused","message":"invalid capability"}')}
            if action == "launch":
                assert slot not in self.slots, "a failed slot is never replaced"
                if slot.endswith("-verify"):
                    assert self.slots[slot.replace("-verify", "-build")]["state"] == "terminated"
                self.slots[slot] = {"slot": slot, "instance_id": "i-" + f"{len(self.slots) + 1:017x}",
                    "state": "running", "deadline_unix": self.deadline, "boot": 1}
                self.slots[slot]["asset_prefix"] = "builders/arn:aws:ec2:us-east-1:111111111111:instance/" + self.slots[slot]["instance_id"] + "/"
                reply = self.slots[slot].copy()
            elif action in {"stop", "start", "reboot"}:
                if action == "stop":
                    self.slots[slot]["state"] = "stopped"
                else:
                    self.slots[slot]["state"] = "running"
                    self.slots[slot]["boot"] += 1
                reply = self.slots[slot].copy()
            elif action == "seal":
                assert slot.endswith("-build") and self.slots[slot]["state"] == "stopped"
                kind = slot.split("-")[0]
                image = "ami-" + {"host": "a", "tile": "b", "renders": "c"}[kind] * 17
                self.candidates[kind] = image
                self.slots[slot]["image_state"] = "available"
                reply = {"image_id": image, "state": "available"}
            elif action == "cleanup":
                final = any(name.endswith("-verify") for name in self.slots)
                if final:
                    self.final_cleanup_had_manifest.append(any(key.endswith("manifest.json") for key in self.worker.objects))
                for value in self.slots.values():
                    value["state"] = "shutting-down" if final and self.slow_final_cleanup else "terminated"
                reply = {"cleaned": True}
            elif action == "status":
                if slot:
                    reply = self.slots[slot].copy()
                else:
                    self.status_reads += 1
                    slots = {name: value.copy() for name, value in self.slots.items()}
                    if self.existing:
                        slots["host-build"] = {"state": "running"}
                    if self.hide_cleanup_slot and slots and all(value["state"] == "terminated" for value in slots.values()):
                        slots.pop(next(iter(slots)))
                    if self.change_cleanup_owner and slots and all(value["state"] == "terminated" for value in slots.values()):
                        slots[next(iter(slots))]["instance_id"] = "i-" + "f" * 17
                    reply = {"build_id": CONFIG["build_id"], "deadline_unix": self.deadline,
                        "input_sha256": self.input_sha, "source_commit": self.source,
                        "slots": slots, "candidates": self.candidates.copy(), "ready": self.status_reads > self.ready_after}
                    if self.midwait_change and self.status_reads == 2:
                        reply.update(self.midwait_change)
            elif action == "command_status":
                response = self.worker.responses[request["command_id"]]
                reply = {"status": response["Status"], "response_code": response["ResponseCode"],
                    "standard_output": response["StandardOutputContent"]}
            else:
                raise AssertionError(action)
            return {"Payload": io.BytesIO(json.dumps(reply).encode())}


class Worker:
    def __init__(self, broker):
        self.broker = broker
        broker.worker = self
        self.commands = []
        self.objects = {}
        self.responses = {}
        self.regions = []
        self.uncertain = False
        self.fail_phase = False
        self.bad_proof = None
        self.viewer_asks = []
        self.bad_renderer = False
        self.stale_boots = False
        self.invalidated = 0
        self.tls = {}
        self.artifact_fault = None
        self.renders_asks = []
        self.renders_drop = ()
        self.renders_kernel = RENDERS_RECORD["kernel_release"]
        self.renders_record = dict(RENDERS_RECORD)
        self.secret_writes = 0

    def invalidate(self):
        self.invalidated += 1

    def client(self, name, region=None):
        assert name in {"s3", "ssm", "secretsmanager"}, "the executor never mutates EC2 directly"
        self.regions.append((name, region))
        return self

    def put_object(self, **arguments):
        assert arguments["Bucket"] == CONFIG["bucket"]
        assert arguments["IfNoneMatch"] == "*"
        key = arguments["Key"]
        assert key.startswith("runs/" + hashlib.sha256(CONFIG["build_id"].encode()).hexdigest() + "/")
        assert key not in self.objects
        if key.endswith("manifest.json") and self.broker.track == "renders":
            assert set(self.broker.slots) == set(pipeline.RENDERS_SLOTS)
            assert self.broker.slots["renders-build"]["state"] == "terminated"
            assert sum("pipeline_guest.py verify-renders" in item["Parameters"]["commands"][0] for item in self.commands) == 3
        elif key.endswith("manifest.json"):
            assert set(self.broker.slots) == set(pipeline.SLOTS)
            assert all(self.broker.slots[kind + "-build"]["state"] == "terminated" for kind in ("host", "tile"))
            assert sum("pipeline_guest.py verify " in item["Parameters"]["commands"][0] for item in self.commands) == 6
        self.objects[key] = arguments["Body"]
        return {}

    def generate_presigned_url(self, operation, **arguments):
        assert operation == "get_object" and 0 < arguments["ExpiresIn"] <= 3600
        return "https://example.invalid/archive"

    def copy_object(self, **arguments):
        assert arguments["Bucket"] == CONFIG["bucket"]
        assert arguments["CopySource"]["Key"].startswith("builders/arn:aws:ec2:")
        assert arguments["Key"].startswith("runs/")
        version = arguments["CopySource"]["VersionId"]
        assert version and version != "null"
        if self.artifact_fault == "uncertain-copy":
            raise TimeoutError("copy delivery unknown")
        if self.artifact_fault == "different-source":
            return {"VersionId": "staged-version", "CopySourceVersionId": "another-version"}
        name = arguments["Key"].split("/")[-1]
        checksum = pipeline.base64.b64encode(bytes.fromhex("4" * 64 if name == "renderer.oci.tar.zst" else "7" * 64)).decode()
        assert arguments["ChecksumAlgorithm"] == "SHA256"
        return {"VersionId": "staged-" + version, "CopySourceVersionId": version,
            "CopyObjectResult": {"ChecksumSHA256": "wrong" if self.artifact_fault == "bad-checksum" else checksum}}

    def put_secret_value(self, **arguments):
        self.secret_writes += 1
        assert not self.broker.slots, "initialize isolated canaries before any paid launch"
        variant = "build" if arguments["SecretId"] == CONFIG["tls_build_arn"] else "test"
        assert arguments["SecretId"] == CONFIG["tls_" + variant + "_arn"]
        document = json.loads(arguments["SecretString"])
        assert set(document) == {"cert", "key", "scene_token"}
        self.tls[variant + "_cert_sha256"] = hashlib.sha256(pipeline.ssl.PEM_cert_to_DER_cert(document["cert"])).hexdigest()
        return {"VersionId": arguments["ClientRequestToken"]}

    def describe_instance_information(self, **arguments):
        return {"InstanceInformationList": [{"PingStatus": "Online"}]}

    def send_command(self, **arguments):
        assert len(arguments["InstanceIds"]) == 1
        assert arguments["DocumentName"] == "fcvm-fleet-ami-build"
        assert 0 < int(arguments["Parameters"]["executionTimeout"][0]) <= 9000
        with self.broker.lock:
            self.commands.append(arguments)
            if self.uncertain:
                raise TimeoutError("delivery unknown")
            instance = arguments["InstanceIds"][0]
            slot = next(name for name, value in self.broker.slots.items() if value["instance_id"] == instance)
            command = arguments["Parameters"]["commands"][0]
            value = None
            status = "Failed" if self.fail_phase and "/build.sh" in command and \
                (self.fail_phase is True or self.fail_phase in command) else "Success"
            if pipeline.GUEST in command:
                import shlex
                args = shlex.split(command.splitlines()[-1])
                action = args[2]
                count = 1 if self.stale_boots else self.broker.slots[slot]["boot"]
                identity = "00000000-0000-4000-8000-" + f"{count:012d}"
                if action == "boot":
                    value = {"boot_id": identity}
                elif action == "export-renderer":
                    value = {"sha256": "4" * 64, "image_id": IMAGE, "version_id": "renderer-v1", "size": 123}
                    if self.artifact_fault == "unversioned":
                        value["version_id"] = "null"
                elif action == "export-store":
                    names = [*(f"part-{index:02d}.tar.zst" for index in range(16)), "dirs.tar.zst", "parts", "sha256.json"]
                    value = {"objects": {name: {"sha256": "7" * 64, "version_id": "store-" + name, "size": 10} for name in names}}
                elif action == "export-diagnostics":
                    value = {"sha256": "8" * 64, "version_id": "diagnostics-v1", "size": 123,
                        "unexpected_log": "private-log-canary"}
                elif action == "renderer":
                    value = {"image_id": "sha256:" + "5" * 64 if self.bad_renderer else IMAGE}
                elif action == "verify":
                    value = {"boot_id": identity, "guard": True, "frames": True, "launch_path": True, "tls_rotation": True,
                        "sanitized": True, "phone_clone": slot.startswith("host-"),
                        "windows_sandbox": slot.startswith("host-"), "viewer_path": "viewer-path" in args[3:]}
                    self.viewer_asks.append((slot, "viewer-path" in args[3:]))
                    value.update(self.tls)
                    if self.bad_proof:
                        value[self.bad_proof] = False
                elif action == "verify-renders":
                    cold = "renders-agent" in args[3:]
                    self.renders_asks.append(tuple(args[3:]))
                    value = {"boot_id": identity, "guard": True, "sanitized": True, "launch_path": True,
                        "kernel_release": self.renders_kernel, "renders_host": True, "renders_host_problem_count": 0,
                        "renders_agent": cold, "renders_unit": "renders-unit" in args[3:]}
                    if cold:
                        value["renders_agent_failure_count"] = 0
                    if "renders-unit" in args[3:]:
                        value["renders_unit_failure_count"] = 0
                    if self.bad_proof in value:
                        value[self.bad_proof] = False
                        label = "problem" if self.bad_proof == "renders_host" else "failure"
                        value[self.bad_proof + "_" + label + "s"] = ["held:engines", "kill:running"]
                        value[self.bad_proof + "_" + label + "_count"] = 2
                    for key in self.renders_drop:
                        value.pop(key, None)
                elif action == "renders-record":
                    value = dict(self.renders_record)
                else:
                    value = {"ok": True}
            command_id = "00000000-0000-4000-8000-" + f"{len(self.commands):012d}"
            self.responses[command_id] = {"Status": status, "ResponseCode": 0 if status == "Success" else 1,
                "StandardOutputContent": json.dumps(value) if value is not None else "",
                "StandardErrorContent": "this must never reach the caller: secret"}
            return {"Command": {"CommandId": command_id}}

class PipelineTests(unittest.TestCase):
    def setUp(self):
        self.broker = Broker()
        self.worker = Worker(self.broker)
        self.now = 100
        self.driver = pipeline.Pipeline(self.broker, self.worker, CONFIG,
            root=HERE.parents[1], clock=lambda: self.now, sleep=self.advance)

    def advance(self, seconds):
        self.now += seconds

    def run_driver(self):
        with patch.object(pipeline, "input_digest", return_value=("6" * 64, b"immutable archive")):
            return self.driver.run()

    def no_manifest(self):
        self.assertFalse(any(name.endswith("manifest.json") for name in self.worker.objects))

    def test_pair_requires_fresh_hardware_three_boots_then_cleanup(self):
        result = self.run_driver()
        self.assertEqual(result["input_sha256"], "3" * 64)
        self.assertEqual(result["source_tree_sha256"], "6" * 64)
        self.assertEqual(len(result["store"]["objects"]), 19)
        self.assertEqual(result["store"]["destination"], {"bucket": "example-runtime", "region": "us-east-2",
            "prefix": "factory/" + "3" * 64 + "/store/"})
        self.assertTrue(all(item["version_id"].startswith("staged-") for item in result["store"]["objects"].values()))
        self.assertEqual(self.worker.invalidated, 4)
        self.assertNotEqual(result["tls"]["build_cert_sha256"], result["tls"]["test_cert_sha256"])
        self.assertEqual(set(self.broker.slots), set(pipeline.SLOTS))
        for kind in ("host", "tile"):
            proof = result["images"][kind]["validation"]
            self.assertEqual(len(set(proof["boot_ids"])), 3)
            self.assertTrue(proof["guard"] and proof["frames"] and proof["tls_rotation"])
        self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
        self.assertIn(("s3", "us-west-1"), self.worker.regions)
        self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 4)
        for command in self.worker.commands:
            slot = next(name for name, value in self.broker.slots.items()
                if value["instance_id"] == command["InstanceIds"][0])
            if slot.endswith("-verify"):
                self.assertNotIn("cat > /var/lib/fleet-ami/env", command["Parameters"]["commands"][0])
        for kind in ("host", "tile"):
            identifier = self.broker.slots[kind + "-build"]["instance_id"]
            commands = [entry["Parameters"]["commands"][0] for entry in self.worker.commands
                        if entry["InstanceIds"] == [identifier]]
            upgrade = next(index for index, text in enumerate(commands) if "--with-new-pkgs upgrade" in text)
            prep = next(index for index, text in enumerate(commands) if "/build.sh prep" in text)
            self.assertLess(upgrade, prep)

    def test_validated_manifest_survives_final_shutdown_past_the_compute_deadline(self):
        self.broker.slow_final_cleanup = True
        try:
            result = self.run_driver()
        except RuntimeError:
            result = None
        self.assertIsNotNone(result, "EC2 final shutdown latency discarded completed validation")
        self.assertIn(self.driver.prefix + "manifest.json", self.worker.objects)
        self.assertTrue(all(self.broker.final_cleanup_had_manifest))
        self.assertEqual({value["state"] for name, value in self.broker.slots.items() if name.endswith("-verify")}, {"shutting-down"})
        self.assertLess(self.now, self.broker.deadline)
        self.now = self.broker.deadline + 600
        self.assertEqual(len(self.worker.commands), len(self.worker.responses))
        self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 4)

    def test_failed_manifest_upload_still_requests_final_cleanup_without_success(self):
        put = self.worker.put_object
        def fail_manifest(**arguments):
            if arguments["Key"].endswith("manifest.json"):
                raise TimeoutError("manifest storage unavailable")
            return put(**arguments)
        with patch.object(self.worker, "put_object", side_effect=fail_manifest), self.assertRaises(TimeoutError):
            self.run_driver()
        self.no_manifest()
        self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
        self.assertEqual(self.broker.calls[-1]["action"], "cleanup")

    def test_isolated_sdk_bootstrap_precedes_every_guest_on_all_four_slots(self):
        self.run_driver()
        for slot, value in self.broker.slots.items():
            commands = [item["Parameters"]["commands"][0] for item in self.worker.commands
                        if item["InstanceIds"] == [value["instance_id"]]]
            unpack = next(index for index, text in enumerate(commands) if 'tar -xf "$t"' in text)
            bootstraps = [index for index, text in enumerate(commands) if "python3 -m venv /opt/fcvm-image-sdk" in text]
            self.assertTrue(bootstraps, "the disposable builder never installed the pinned SDK")
            bootstrap = bootstraps[0]
            first_guest = next(index for index, text in enumerate(commands) if pipeline.GUEST in text)
            self.assertLess(unpack, bootstrap)
            self.assertLess(bootstrap, first_guest)
            self.assertIn("--isolated install --no-input --only-binary=:all: --require-hashes", commands[bootstrap])
            self.assertIn(pipeline.REMOTE + "/fleet/ami/requirements-pipeline.txt", commands[bootstrap])
            self.assertLess(commands[bootstrap].index("cloud-init status --wait"), commands[bootstrap].index("apt-get"))
            for text in commands[first_guest:]:
                if pipeline.GUEST in text:
                    self.assertIn(pipeline.GUEST_PYTHON + " " + pipeline.GUEST, text)

    def test_reused_tile_traces_a_boot_without_the_inherited_prefetch_profile(self):
        events = []
        invoke, send = self.broker.invoke, self.worker.send_command
        def broker(**arguments):
            value = json.loads(arguments["Payload"])
            events.append((value["action"], value.get("slot"), None))
            return invoke(**arguments)
        def command(**arguments):
            slot = next(name for name, value in self.broker.slots.items()
                        if value["instance_id"] == arguments["InstanceIds"][0])
            events.append(("command", slot, arguments["Parameters"]["commands"][0]))
            return send(**arguments)
        with patch.object(self.broker, "invoke", side_effect=broker), \
             patch.object(self.worker, "send_command", side_effect=command):
            self.run_driver()
        recipe = (HERE / "tile/build.sh").read_text()
        unit = (HERE / "tile/fcvm-fleet-prefetch.service").read_text()
        self.assertIn("ConditionPathExists=/etc/fcvm-fleet/prefetch.list", unit)
        self.assertIn("ExecStartPost=/bin/sh -c ': > /run/fcvm-fleet-prefetched'", unit)
        self.assertIn("systemctl enable fcvm-fleet-prefetch.service", recipe)
        trace = recipe.split("\ntrace)\n", 1)[1].split("\n  ;;", 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "etc/fcvm-fleet"; config.mkdir(parents=True)
            profile = config / "prefetch.list"; profile.write_text("inherited profile\n")
            marker = root / "prefetched"; marker.touch()
            framed = root / "first-frame.log"; framed.write_text("ready\n")
            (root / "resident.py").write_text("from pathlib import Path\nimport sys\n"
                "Path(sys.argv[1]).write_text('fresh profile\\n')\n")
            prepared = rebooted = False
            for action, slot, text in events:
                if slot != "tile-build":
                    continue
                if action == "command":
                    if "/tile/build.sh prep" in text:
                        prepared = True
                    for line in text.splitlines():
                        if line == "rm -f /etc/fcvm-fleet/prefetch.list":
                            self.assertTrue(prepared)
                            subprocess.run(["bash", "-c", line.replace("/etc/fcvm-fleet/prefetch.list", str(profile))],
                                check=True, capture_output=True)
                if action == "reboot" and prepared:
                    # /run is fresh after the actual prep reboot. The unchanged
                    # unit runs only if its inherited profile still exists.
                    marker.unlink()
                    if profile.exists():
                        marker.touch()
                    rebooted = True
                    break
            self.assertTrue(rebooted)
            actual = trace.replace("/etc/fcvm-fleet", str(config)).replace(
                "/run/fcvm-fleet-prefetched", str(marker)).replace("/var/log/fcvm-fleet-first-frame.log", str(framed))
            result = subprocess.run(["bash", "-c", "set -eu\n" + actual],
                env={"PATH": "/usr/bin:/bin", "HERE": str(root)}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(profile.read_text(), "fresh profile\n")
            self.assertFalse(marker.exists())

    def test_actual_source_install_discards_deleted_recipe_files_without_following_a_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target, binary, archive = root / "source", root / "bin", root / "archive.tar"
            target.mkdir(); binary.mkdir()
            (target / "deleted.service").write_text("old recipe")
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode="w") as bundle:
                value = b"current source"
                entry = tarfile.TarInfo("current.txt"); entry.size = len(value)
                bundle.addfile(entry, io.BytesIO(value))
            archive.write_bytes(stream.getvalue())
            (binary / "curl").write_text("#!/usr/bin/python3\nimport shutil,sys\nshutil.copyfile("
                + repr(str(archive)) + ",sys.argv[sys.argv.index('-o')+1])\n")
            # Model the root-owned disposable builder; every other real shell/tar
            # operation runs against this private fixture directory as ourselves.
            (binary / "stat").write_text("#!/bin/sh\n[ \"$1\" = -c ] && [ \"$2\" = %u ] || exit 90\nprintf '0\\n'\n")
            for path in binary.iterdir(): path.chmod(0o700)
            self.driver.archive = archive.read_bytes()
            self.driver.input_sha = "3" * 64
            self.driver.deadline = 9100
            commands = []
            with patch.object(self.driver, "shell", side_effect=lambda slot, text, **kwargs: commands.append(text)):
                self.driver.install("host-build")
            actual = commands[0].replace(pipeline.REMOTE, str(target))
            environment = {"PATH": str(binary) + ":/usr/bin:/bin"}
            subprocess.run(["bash", "-c", "set -eu\n" + actual], check=True, env=environment, capture_output=True)
            self.assertFalse((target / "deleted.service").exists())
            self.assertEqual((target / "current.txt").read_text(), "current source")
            (target / "current.txt").unlink(); target.rmdir()
            external = root / "outside"; external.mkdir()
            (external / "preserve.txt").write_text("not the factory source")
            target.symlink_to(external, target_is_directory=True)
            result = subprocess.run(["bash", "-c", "set -eu\n" + actual], env=environment, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((external / "preserve.txt").read_text(), "not the factory source")

    def test_actual_installed_recipe_is_readable_but_not_writable_by_the_build_user(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root.chmod(0o755)  # The second identity must reach the fixture's source root.
            target, binary, archive = root / "source", root / "bin", root / "archive.tar"
            binary.mkdir()
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode="w") as bundle:
                value = b'test ! -w "$(dirname "$0")" && test ! -w "$0" && printf "recipe-readable\\n"\n'
                entry = tarfile.TarInfo("warm.sh"); entry.size = len(value); entry.mode = 0o644
                bundle.addfile(entry, io.BytesIO(value))
            archive.write_bytes(stream.getvalue())
            curl = binary / "curl"
            curl.write_text("#!/usr/bin/python3\nimport shutil,sys\nshutil.copyfile("
                + repr(str(archive)) + ",sys.argv[sys.argv.index('-o')+1])\n")
            curl.chmod(0o700)
            self.driver.archive = archive.read_bytes()
            self.driver.input_sha = "3" * 64
            self.driver.deadline = 9100
            commands = []
            with patch.object(self.driver, "shell", side_effect=lambda slot, text, **kwargs: commands.append(text)):
                self.driver.install("host-build")
            actual = commands[0].replace(pipeline.REMOTE, str(target))
            subprocess.run(["bash", "-c", "set -eu\n" + actual], check=True,
                env={"PATH": str(binary) + ":/usr/bin:/bin"}, capture_output=True)
            # SSM installs as root; a distinct Unix identity models the recipe's
            # sudo -u ubuntu warm-up without running any build or cloud command.
            result = subprocess.run(["sudo", "-n", "-u", "nobody", "--", "env", "-i",
                "PATH=/usr/bin:/bin", "bash", str(target / "warm.sh")], cwd="/",
                env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "recipe-readable\n")

    def test_uncertain_ssm_send_is_not_replayed_and_always_cleans_up(self):
        self.worker.uncertain = True
        with self.assertRaises(TimeoutError):
            self.run_driver()
        self.assertEqual(len(self.worker.commands), 1)
        self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 1)
        self.assertEqual(self.broker.slots["host-build"]["state"], "terminated")
        self.no_manifest()

    def test_host_prep_installs_fcvms_host_kernel_and_build_refuses_another(self):
        # Ubuntu's kernel lacks the bridge rcu_barrier removal (#1563): prep installs fcvm's
        # host kernel as GRUB's default before the reboot, and the build phase, which warms
        # every snapshot the image carries, runs only on it.
        source = (HERE / "host/build.sh").read_text()
        phases = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))
        prep, build = phases["prep"], phases["build"]
        self.assertIn('as_user "cd $SRC && make install-host-kernel"', prep)
        self.assertLess(prep.index("store.sh"), prep.index("make install-host-kernel"))
        self.assertLess(prep.index("update-grub"), prep.index("make install-host-kernel"))
        guard = build.split("\n")[1:6]
        self.assertTrue(any('case "$(uname -r)" in' in line for line in guard), guard)
        self.assertIn("*-fcvm-*)", build.split("make build")[0])

    def test_the_windows_image_builds_beside_the_rest_of_the_host_build(self):
        # fe5ccbfc's host build took 1 h 45 min, the Windows install 1066 s of it after the
        # media download, and the host image was sealed 3 h 15 min into the factory's fixed
        # 4 h, leaving the tile and the validation 45 min. The Windows image depends on
        # nothing else the phase builds, so it starts right after the checkout and the phase
        # waits for it only where it installs it.
        source = (HERE / "host/build.sh").read_text()
        build = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["build"]
        checkout = build.index("git checkout -q -f --detach $FCVM_COMMIT")
        image = build.index("bg_start fleet-ami-windows-image")
        first_build = build.index('as_user "cd $SRC && make build"')
        self.assertLess(checkout, image)
        self.assertLess(image, first_build, "the Windows image starts before fcvm's own build")
        self.assertIn('build-image.sh" /mnt/fcvm-btrfs/windows', build[image:build.index("\n", image)])
        env = build.index("/etc/fcvm-fleet/windows.env")
        self.assertLess(build.index("bg_wait fleet-ami-windows-image"), env)
        # The bridge stays in line: rootless podman's build runs outside a unit's cgroup, so
        # stopping a unit would not stop it (Codex on #1778).
        self.assertNotIn("bg_start fleet-ami-windows-bridge", build)
        self.assertLess(build.index("make -C fleet/windows bridge-image TAG=fleet"), env)
        self.assertLess(build.index("rm -rf \"$d\""), image, "an aging installation is removed before the build reuses it")

    def test_the_store_is_the_users_before_the_first_cargo_build_of_each_phase(self):
        # store.sh mounts the store as root's. A make build with the store still root's puts its
        # target beside the checkout (scripts/cargo-target-link.sh's fallback), and the next make
        # after setup-btrfs compiled fcvm and fc-agent again on the store: two full compiles of
        # each in every build phase, logged on 2026-11-07 (6 min 1 s and 1 min 0 s the second time).
        source = (HERE / "host/build.sh").read_text()
        phases = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))
        for name, first in (("prep", 'as_user "cd $SRC && make install-host-kernel"'),
                            ("build", 'as_user "cd $SRC && make build"')):
            with self.subTest(phase=name):
                phase = phases[name]
                setup = phase.index('as_user "cd $SRC && make setup-btrfs"')
                self.assertLess(phase.index("store_settled"), setup)
                self.assertLess(phase.index("git checkout -q -f --detach $FCVM_COMMIT"), setup)
                self.assertLess(setup, phase.index(first))
                if name == "build":
                    self.assertLess(setup, phase.index("bg_start fleet-ami-windows-image"))

    def test_the_bake_clears_untagged_images_before_it_packs_onto_the_root(self):
        # Every generation built from a base image inherits its untagged images and adds its own;
        # on 2026-11-13 they filled the 61 GB root during the pack (part 10 of 16). Both podman
        # stores are pruned, and the phone app's stale build images removed, before the pack.
        source = (HERE / "host/build.sh").read_text()
        bake = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["bake"]
        pack = bake.index('python3 "$HERE/store-pack.py"')
        for step in ("podman image prune -f", 'as_user "podman image prune -f',
                     "grep -vx 'localhost/fcvm-phone-app-build:$app' | xargs -r podman rmi"):
            with self.subTest(step=step):
                self.assertLess(bake.index(step), pack)
        self.assertIn('app=$(sha256sum "$SRC/fleet/phone-app/Containerfile" | cut -c1-16)', bake)

    def test_windows_env_says_the_bridge_refuses_still_only_when_the_tree_does(self):
        # The host bootstrap (ejc3/aws modules/fcvm-fleet-region) starts Windows on an image that is
        # not on its allowlist only when windows.env says the bridge refuses /still on the public
        # ports (ejc3/fcvm#1839). The build says so only when the tree the bridge image was built
        # from has both halves of that refusal, and the halves it looks for are the tree's own
        # lines (CI's bridge and renderer host tests exercise them).
        source = (HERE / "host/build.sh").read_text()
        build = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["build"]
        image = build.index('make -C fleet/windows bridge-image TAG=fleet')
        env = build.index("> /etc/fcvm-fleet/windows.env")
        check = build.index("still=\n", image)
        self.assertLess(check, env)
        block = build[check:env]
        override = 'bool allows_fleet_still() const override { return false; }'
        refusal = 'if (fleet() && !p_.allows_fleet_still()) return respond(c, 404, "Not Found", "");'
        self.assertIn(f"grep -qF '{override}' \"$SRC/fleet/windows/bridge/bridge.h\"", block)
        self.assertIn(f"grep -qF '{refusal}' \"$SRC/fleet/renderer/src/server.cpp\"", block)
        self.assertIn(override, (HERE.parents[1] / "fleet/windows/bridge/bridge.h").read_text())
        self.assertIn(refusal, (HERE.parents[1] / "fleet/renderer/src/server.cpp").read_text())
        self.assertIn("still='WINDOWS_BRIDGE_STILL=refused\\n'", block)
        self.assertIn("\\n'\"$still\" \"$WIN_IMAGE\"", block)

    def test_minecraft_env_says_the_join_limits_are_enforced_only_when_the_tree_has_them(self):
        # The host bootstrap's Java guard (ejc3/aws modules/fcvm-fleet-region) opens TCP 25565 on an
        # image off its list only when the image says its servers carry the join limits: the agent's
        # per-source FCVM-MC-JOIN chain (#1844, src/fleet/agent/joinguard.rs) and Pumpkin's player cap
        # and login timeout (fleet/minecraft/src/pumpkin.rs). The build says so only when the tree the
        # Minecraft image was built from has them, and the lines it looks for are the tree's own.
        source = (HERE / "host/build.sh").read_text()
        build = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["build"]
        image = build.index("podman image exists localhost/fcvm-minecraft:fleet")
        env = build.index("> /etc/fcvm-fleet/minecraft.env")
        check = build.index("limits=\n", image)
        self.assertLess(check, env)
        block = build[check:env]
        root = HERE.parents[1]
        for path, line in (("src/fleet/agent/joinguard.rs", '"--connlimit-above",'),
                           ("src/fleet/agent/joinguard.rs", '"--hashlimit-above",'),
                           ("fleet/minecraft/src/pumpkin.rs", "pub const MAX_PLAYERS: u32"),
                           ("fleet/minecraft/src/pumpkin.rs", "pub const LOGIN_TIMEOUT_SECS: u64")):
            with self.subTest(line=line):
                self.assertIn(f"grep -qF '{line}' \"$SRC/{path}\"", block)
                self.assertIn(line, (root / path).read_text())
        self.assertIn("limits='MINECRAFT_JOIN_LIMITS=enforced\\n'", block)
        self.assertIn("printf \"$limits\" > /etc/fcvm-fleet/minecraft.env", build)

    def test_the_bake_drops_build_only_state_from_the_root(self):
        # A host hydrates every root block it reads from the image's snapshot, and the image is
        # what every host boots: the compiler cache (4.1 GB on the cad0ddad image), the Rust
        # toolchain (2.9 GB) and the build's journal are build-only. The cache goes before the
        # pack, which needs the room; prep and build install the toolchain again when it is
        # missing.
        source = (HERE / "host/build.sh").read_text()
        phases = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))
        bake = phases["bake"]
        self.assertLess(bake.index('rm -rf "$UH/.cache/sccache"'), bake.index('python3 "$HERE/store-pack.py"'))
        self.assertIn('rm -rf "$UH/.cargo" "$UH/.rustup"', bake)
        self.assertIn("journalctl --vacuum-time=1s", bake)
        self.assertLess(bake.index('rm -rf "$UH/.cargo" "$UH/.rustup"'), bake.index("\n  root_has_room"))
        for phase in ("prep", "build"):
            with self.subTest(phase=phase):
                self.assertIn('[ -x "$UH/.cargo/bin/cargo" ] || as_user', phases[phase])

    def test_the_bake_fails_unless_the_root_keeps_room(self):
        # The image keeps its packed store on the root, which every host inherits; a gpubox from
        # the cad0ddad image had 416 MB free there and a 7.3.3 host kernel install failed. The
        # bake checks the root's free space last, once the store is packed and /tmp emptied, and
        # fails under ROOT_FREE_MIN_GIB (8).
        source = (HERE / "host/build.sh").read_text()
        bake = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["bake"]
        check = bake.index("\n  root_has_room")
        self.assertLess(bake.index('python3 "$HERE/store-pack.py"'), check)
        self.assertLess(bake.index("find /tmp /var/tmp -mindepth 1 -delete"), check)
        # fleet/ami/check.sh holds the /tmp cleanup right before the final sync; the check is a
        # read and comes after both.
        self.assertLess(bake.index("\n  sync\n"), check)
        fn = re.search(r"^root_has_room\(\) \{\n.*?^\}\n", source, re.S | re.M)
        self.assertIsNotNone(fn, "build.sh defines no root_has_room")
        with tempfile.TemporaryDirectory() as tmp:
            df = Path(tmp) / "df"

            def run(avail_bytes):
                df.write_text(f"#!/bin/sh\nprintf 'Avail\\n%s\\n' '{avail_bytes}'\n")
                df.chmod(0o755)
                return subprocess.run(["bash", "-c", fn.group(0) + "root_has_room"], capture_output=True, text=True,
                                      env={"PATH": f"{tmp}:/usr/bin:/bin"})

            low = run(416 * 1024 * 1024)
            self.assertNotEqual(low.returncode, 0)
            self.assertIn("416 MiB free", low.stdout)
            self.assertEqual(run(9 * 1024 ** 3).returncode, 0)
            self.assertNotEqual(run("").returncode, 0, "an unreadable df must fail, not pass")

    def test_the_minecraft_image_builds_beside_the_rest_into_roots_store(self):
        # In line it was the build phase's longest step (19 of 44 min on 2026-11-09). It starts
        # after the checkout, as root (a rootful build stays in its unit's cgroup), and the phase
        # waits for it where it used to build it, before the phones.
        source = (HERE / "host/build.sh").read_text()
        build = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["build"]
        start = build.index("bg_start fleet-ami-minecraft-image")
        self.assertLess(build.index("git checkout -q -f --detach $FCVM_COMMIT"), start)
        self.assertLess(start, build.index('as_user "cd $SRC && make build"'))
        unit = build[start:build.index("\n", build.index("\n", start) + 1)]
        self.assertIn("make minecraft-image MINECRAFT_IMAGE=localhost/fcvm-minecraft:fleet MINECRAFT_PODMAN='sudo podman'", unit)
        self.assertIn('sudo -u "$U"', unit, "make refuses to run as root")
        recipe = (HERE.parents[1] / "Makefile").read_text().split("\nminecraft-image:\n", 1)[1].split("\n\n", 1)[0]
        self.assertTrue(recipe.lstrip().startswith("$(MINECRAFT_PODMAN) build "), recipe)
        wait = build.index('bg_wait fleet-ami-minecraft-image "$L/minecraft-image.log"')
        self.assertLess(wait, build.index("make setup-phone"))
        self.assertLess(wait, build.index("podman image exists localhost/fcvm-minecraft:fleet"))
        # no second build, and no copy through the build user's store
        self.assertNotIn('as_user "cd $SRC && make minecraft-image', build)
        self.assertNotIn("minecraft.oci.tar", build)

    def test_background_jobs_are_named_units_stopped_on_every_way_out(self):
        # A failed phase must not leave a job writing to the checkout or the store while it is
        # retried (Codex on #1778): the jobs are transient systemd units of fixed names, the
        # EXIT trap stops them (their whole cgroup, escalating to SIGKILL), and a phase starts
        # by stopping any a phase that could not run its trap left behind.
        source = (HERE / "host/build.sh").read_text()
        preamble = source.split('\ncase "$PHASE" in\n', 1)[0]
        self.assertIn('BG_UNITS="fleet-ami-windows-image fleet-ami-minecraft-image"', preamble)
        trap = [line for line in preamble.splitlines() if line.startswith("trap ") and line.endswith(" EXIT")]
        self.assertEqual(len(trap), 1)
        self.assertIn("bg_stop;", trap[0])
        self.assertIn("\nbg_stop\n", preamble[preamble.index(trap[0]):])
        start = re.search(r"\nbg_start\(\) \{\n(.*?)\n\}\n", preamble, re.S).group(1)
        # A builder from a base image keeps the previous build's log, and the image directory is
        # read from the log's last matching line (Codex on #1778): the log is emptied first, and
        # both streams append (a truncate: stdout overwrote stderr's lines from offset 0).
        self.assertLess(start.index(': > "$log"'), start.index("systemd-run"))
        for prop in ("RemainAfterExit=yes", "TimeoutStopSec=30", "KillMode=control-group",
                     'StandardOutput="append:$log"', 'StandardError="append:$log"'):
            self.assertIn(prop, start)
        self.assertNotIn(" & ", re.sub(r"#.*", "", source), "no shell background jobs: only named units")

    def test_host_prep_runs_on_fcvms_host_kernel_too(self):
        # Revising a host image that already boots fcvm's host kernel runs prep on it, and that
        # kernel has neither Ubuntu's apparmor_restrict_unprivileged_userns nor Debian's
        # unprivileged_userns_clone: a plain `sysctl -p` exits 1 on the missing keys. Prep skips
        # keys the kernel lacks and checks what they are for, a user namespace the build user
        # can open.
        source = (HERE / "host/build.sh").read_text()
        prep = dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))["prep"]
        applies = [line.strip() for line in prep.splitlines() if re.match(r"\s*sysctl\b", line)]
        self.assertEqual(applies, ["sysctl -e -p /etc/sysctl.d/99-fcvm.conf"])
        self.assertLess(prep.index("sysctl -e -p"), prep.index('runuser -u "$U" -- unshare -U -r true'))

    def test_host_image_has_room_for_hundreds_of_routed_vms_neighbours(self):
        # The neighbour tables, IPv6 and IPv4, are shared by every network namespace: at the
        # kernel's 1024, 512 routed Minecraft servers overflowed the IPv6 one
        # (docs/fleet/minecraft.md). The agent refuses a fleet a limit has no room for, at the
        # entries a VM its constants name (src/fleet/agent/minecraft.rs), so the image's limits
        # are checked against those constants: room for 512 servers and 64 other VMs, twice over.
        source = (HERE / "host/build.sh").read_text()
        conf = re.search(r"cat > /etc/sysctl\.d/99-fcvm\.conf <<'X'\n(.*?)\nX\n", source, re.S).group(1)
        keys = dict(line.split("=", 1) for line in conf.splitlines() if "=" in line and not line.startswith("#"))
        agent = (HERE.parent.parent / "src/fleet/agent/minecraft.rs").read_text()
        for family, const in (("ipv6", "NEIGH_PER_VM"), ("ipv4", "NEIGH_PER_VM_V4")):
            per = int(re.search(rf"pub const {const}: u64 = (\d+);", agent).group(1))
            limits = [int(keys[f"net.{family}.neigh.default.gc_thresh{n}"]) for n in (1, 2, 3)]
            self.assertEqual(limits, sorted(limits), family)
            self.assertGreaterEqual(limits[2], 2 * (512 + 64) * per, family)
        # counted on a host: 16.5 IPv6 entries and 2.0 IPv4 entries a server
        self.assertGreaterEqual(int(re.search(r"pub const NEIGH_PER_VM: u64 = (\d+);", agent).group(1)), 20)
        self.assertGreaterEqual(int(re.search(r"pub const NEIGH_PER_VM_V4: u64 = (\d+);", agent).group(1)), 3)

    def test_the_agent_starts_after_the_sysctls_are_applied(self):
        # The agent reads the neighbour limits when it starts. /etc/sysctl.d is applied by
        # systemd-sysctl before sysinit.target, and a unit with systemd's default dependencies
        # is ordered after that target. A unit that opts out of them (as the store's does) is not.
        unit = (HERE / "host/fcvm-fleet-agent.service").read_text()
        settings = [line.split("=", 1)[0].strip() for line in unit.splitlines() if "=" in line and not line.lstrip().startswith("#")]
        self.assertNotIn("DefaultDependencies", settings)
        self.assertIn("DefaultDependencies=no", (HERE / "host/fcvm-fleet-store.service").read_text())

    def test_build_user_can_open_only_its_private_warm_log_under_the_actual_preamble(self):
        source = (HERE / "host/build.sh").read_text()
        preamble, _ = source.split('\ncase "$PHASE" in\n', 1)
        warm = [line.strip() for line in source.splitlines()
                if line.strip().startswith('install -o "$U"') and line.endswith('"$L/warm.log"')]
        self.assertEqual(len(warm), 1)
        user = pwd.getpwuid(os.getuid()) if os.getuid() != 0 else pwd.getpwnam("nobody")
        other = pwd.getpwnam("nobody" if user.pw_name != "nobody" else "daemon")
        # The privileged child touches only its own temporary directory. It runs
        # the actual preamble and warm-file install, never a recipe phase.
        fixture = r'''
import json, os, pathlib, stat, subprocess, sys, tempfile
value = json.load(sys.stdin)
uid, gid, other_uid, other_gid = (value[key] for key in ("uid", "gid", "other_uid", "other_gid"))
assert os.geteuid() == 0 and uid != 0 and other_uid not in (0, uid)
with tempfile.TemporaryDirectory(prefix="fleet-warm-log-permissions-") as directory:
    root = pathlib.Path(directory)
    root.chmod(0o755)
    state, logs = root / "state", root / "logs"
    state.mkdir(mode=0o700)
    environment = state / "env"
    environment.write_text("ROOT_ONLY=fixture\n")
    environment.chmod(0o600)
    preamble = "\n".join(line for line in value["preamble"].splitlines() if not line.lstrip().startswith("#"))
    assert preamble.count("/var/lib/fleet-ami") == preamble.count("/var/log/fleet-ami") == 1
    assert preamble.count("U=ubuntu;") == 1
    preamble = preamble.replace("/var/lib/fleet-ami", str(state)).replace("/var/log/fleet-ami", str(logs))
    preamble = preamble.replace("U=ubuntu;", "U=" + value["user"] + ";")
    # The CI identity's primary group need not have the same name as its user.
    preamble = preamble.replace('-g "$U"', '-g ' + str(gid))
    warm = value["warm"].replace('-g "$U"', '-g ' + str(gid))
    script = root / "fixture.sh"
    script.write_text("umask 077\n" + preamble + "\n" + warm + "\nexit 0\n")
    result = subprocess.run(["bash", str(script), "build"], env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    warm_path, root_log = logs / "warm.log", logs / "build.log"
    def child(code, selected_uid=uid, selected_gid=gid):
        return subprocess.run([sys.executable, "-I", "-c", code], user=selected_uid, group=selected_gid,
            extra_groups=[], cwd="/", env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=10)
    # First reproduce the real failure: the recipe's unprivileged user must open
    # the warm log, rather than only satisfying a mode-string assertion.
    opened = child("open(" + repr(str(warm_path)) + ", 'ab').write(b'warm fixture\\n')")
    assert opened.returncode == 0, opened.stderr
    denied = "import os\nfor path in " + repr([str(root_log), str(environment)]) + ":\n" + \
        " try: open(path, 'rb')\n except PermissionError: pass\n else: raise AssertionError('root file readable')\n" + \
        "try: os.listdir(" + repr(str(logs)) + ")\nexcept PermissionError: pass\nelse: raise AssertionError('root logs listable')\n"
    private = child(denied)
    assert private.returncode == 0, private.stderr
    unrelated = child("open(" + repr(str(warm_path)) + ", 'ab')", other_uid, other_gid)
    assert unrelated.returncode != 0 and "PermissionError" in unrelated.stderr
    assert stat.S_IMODE(logs.stat().st_mode) == 0o710 and logs.stat().st_uid == 0 and logs.stat().st_gid == gid
    assert stat.S_IMODE(warm_path.stat().st_mode) == 0o600 and warm_path.stat().st_uid == uid
    assert stat.S_IMODE(state.stat().st_mode) == 0o700 and stat.S_IMODE(environment.stat().st_mode) == 0o600
print("warm-log-permissions-pass")
'''
        result = subprocess.run(["sudo", "-n", "--", sys.executable, "-I", "-c", fixture],
            input=json.dumps({"preamble": preamble, "warm": warm[0], "user": user.pw_name,
                "uid": user.pw_uid, "gid": user.pw_gid, "other_uid": other.pw_uid, "other_gid": other.pw_gid}),
            cwd="/", env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "warm-log-permissions-pass")

    def test_parallel_validators_serialize_the_single_concurrency_broker(self):
        boundary = threading.Event()
        entered = threading.Event()
        class Throttled(Exception):
            response = {"Error": {"Code": "TooManyRequestsException"}}

        class ObservedLock:
            def __init__(self):
                self.lock = threading.Lock()
            def __enter__(self):
                if not self.lock.acquire(blocking=False):
                    boundary.set()
                    self.lock.acquire()
                return self
            def __exit__(self, *args):
                self.lock.release()

        class Broker:
            def __init__(self):
                self.active = threading.Lock()
                self.count = 0
                self.overlaps = 0
            def invoke(self, **arguments):
                if not self.active.acquire(blocking=False):
                    self.overlaps += 1
                    boundary.set()
                    raise Throttled()
                self.count += 1
                if self.count == 1:
                    entered.set()
                    if not boundary.wait(timeout=5):
                        self.active.release()
                        raise RuntimeError("the second validator never reached the broker boundary")
                owner = self
                class Payload:
                    def read(self, limit):
                        owner.active.release()
                        return b'{"ok":true}'
                return {"Payload": Payload()}

        broker = Broker()
        self.driver.broker = broker
        self.driver.call_lock = ObservedLock()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            first = executor.submit(self.driver.call, "status")
            self.assertTrue(entered.wait(timeout=5))
            second = executor.submit(self.driver.call, "status")
            results = []
            for future in (first, second):
                try:
                    results.append(future.result(timeout=5))
                except Exception as error:
                    results.append(type(error).__name__)
        self.assertEqual(results, [{"ok": True}, {"ok": True}])
        self.assertEqual(broker.overlaps, 0)
        self.assertEqual(broker.count, 2)

    def test_failed_validator_cancels_a_waiting_peer_and_fences_a_late_launch(self):
        for peer in ("waiting", "late-launch"):
            with self.subTest(peer=peer):
                self.setUp()
                waiting, cleaned = threading.Event(), threading.Event()
                invoke = self.broker.invoke

                def observed(**arguments):
                    request = json.loads(arguments["Payload"])
                    response = invoke(**arguments)
                    if request["action"] == "cleanup" and waiting.is_set():
                        cleaned.set()
                    return response

                def verify(kind, renderer):
                    if kind == "host":
                        self.assertTrue(waiting.wait(timeout=5), "the second validator never started")
                        raise RuntimeError("candidate validation failed")
                    waiting.set()
                    if peer == "late-launch":
                        self.assertTrue(cleaned.wait(timeout=5), "failure did not trigger cleanup")
                        self.driver.launch("tile-verify")
                    else:
                        self.driver.wait(lambda: False)

                def sleep(seconds):
                    if threading.current_thread() is not threading.main_thread():
                        self.assertTrue(cleaned.wait(timeout=5), "failure did not trigger cleanup")
                    self.advance(seconds)

                with patch.object(self.broker, "invoke", side_effect=observed), \
                     patch.object(self.driver, "verify", side_effect=verify), \
                     patch.object(self.driver, "sleep", side_effect=sleep):
                    with self.assertRaises(RuntimeError):
                        self.run_driver()
                self.assertLess(self.now, 200, "a failed peer waited out the paid build deadline")
                self.assertEqual([call["slot"] for call in self.broker.calls if call["action"] == "launch"],
                    ["host-build", "tile-build"], "a validator launched after failure cleanup")
                self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
                self.no_manifest()

    def test_cancelled_cleanup_still_waits_for_actual_termination_with_the_same_deadline(self):
        self.driver.deadline = self.broker.deadline
        with patch.object(self.driver, "install"):
            self.driver.launch("host-build")
        self.driver.cancelled = threading.Event()
        self.driver.cancelled.set()
        invoke = self.broker.invoke
        polls = []

        def delayed(**arguments):
            request = json.loads(arguments["Payload"])
            if request["action"] == "status":
                polls.append(request)
                if len(polls) >= 2:
                    self.broker.slots["host-build"]["state"] = "terminated"
            response = invoke(**arguments)
            if request["action"] == "cleanup":
                self.broker.slots["host-build"]["state"] = "shutting-down"
            return response

        with patch.object(self.broker, "invoke", side_effect=delayed):
            self.driver.cleanup()
        self.assertEqual(len(polls), 2)
        self.assertEqual(self.broker.slots["host-build"]["state"], "terminated")
        self.assertEqual(self.driver.deadline, self.broker.deadline)
        self.assertLess(self.now, self.driver.deadline)

    def test_only_definite_lambda_throttling_retries_within_the_fixed_limit(self):
        class Throttled(Exception):
            response = {"Error": {"Code": "TooManyRequestsException"}}
        calls = []
        def invoke(**arguments):
            calls.append(arguments)
            if len(calls) <= 2:
                raise Throttled()
            return {"Payload": io.BytesIO(b'{"ok":true}')}
        self.driver.broker = SimpleNamespace(invoke=invoke)
        self.assertEqual(self.driver.call("status"), {"ok": True})
        self.assertEqual(len(calls), 3)
        self.assertLessEqual(self.now, 160)

    def test_throttle_retries_stop_at_sixty_seconds_or_the_build_deadline(self):
        class Throttled(Exception):
            response = {"Error": {"Code": "TooManyRequestsException"}}
        for deadline in (0, 101):
            with self.subTest(deadline=deadline):
                self.setUp()
                calls = []
                def invoke(**arguments):
                    calls.append(arguments)
                    raise Throttled()
                self.driver.broker = SimpleNamespace(invoke=invoke)
                self.driver.deadline = deadline
                with self.assertRaises(Throttled):
                    self.driver.call("status")
                self.assertGreater(len(calls), 1)
                self.assertLessEqual(self.now, deadline or 160)

    def test_unknown_transport_and_executed_function_errors_are_never_replayed(self):
        for failure in ("transport", "function", "payload"):
            with self.subTest(failure=failure):
                self.setUp()
                calls = []
                def invoke(**arguments):
                    calls.append(arguments)
                    if failure == "transport":
                        raise TimeoutError("response lost after execution")
                    if failure == "function":
                        return {"FunctionError": "Unhandled", "Payload": io.BytesIO(b'{}')}
                    def read(limit):
                        raise ConnectionResetError("response body lost after execution")
                    return {"Payload": SimpleNamespace(read=read)}
                self.driver.broker = SimpleNamespace(invoke=invoke)
                with self.assertRaises((RuntimeError, TimeoutError, ConnectionResetError)):
                    self.driver.call("launch", "host-build")
                self.assertEqual(len(calls), 1)

    def test_failed_recipe_is_not_retried_or_published_and_logs_stay_private(self):
        self.worker.fail_phase = True
        with self.assertRaisesRegex(RuntimeError, "isolated builder command failed") as error:
            self.run_driver()
        self.assertNotIn("secret", str(error.exception))
        # both builders may launch before the failure stops the other; none is replaced
        launched = [call["slot"] for call in self.broker.calls if call["action"] == "launch"]
        self.assertEqual(len(launched), len(set(launched)))
        self.assertLessEqual(set(launched), {"host-build", "tile-build"})
        self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
        self.no_manifest()

    def test_a_build_side_stalled_past_the_deadline_cannot_hold_the_cleanup(self):
        # Codex on #1802: a host build stalled before its renderer export held the pair, and so
        # run()'s cleanup, past the deadline. The pair waits only until the deadline.
        stall = threading.Event()
        original = self.driver.build_host
        def stalled(renderer_ready=None):
            stall.wait(30)
            return original(renderer_ready)
        self.broker.deadline = self.now + 3600
        with patch.object(self.driver, "build_host", side_effect=stalled), \
                patch.object(self.driver, "remaining", side_effect=lambda cleanup=False: 1):
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "deadline"):
                self.driver.build_pair()
            self.assertLess(time.monotonic() - started, 10)
        stall.set()

    def test_a_failure_beside_a_stalled_side_is_the_one_raised(self):
        # Codex on #1802: the tile's prep failed while the host stalled, and the deadline error
        # hid the failure.
        stall = threading.Event()
        def stalled(renderer_ready=None):
            stall.wait(30)
        def broken():
            raise ValueError("tile prep failed")
        with patch.object(self.driver, "build_host", side_effect=stalled), \
                patch.object(self.driver, "build_tile_prep", side_effect=broken), \
                patch.object(self.driver, "remaining", side_effect=lambda cleanup=False: 1):
            with self.assertRaisesRegex(ValueError, "tile prep failed"):
                self.driver.build_pair()
        stall.set()

    def test_failed_phase_archives_metadata_before_cleanup_without_repeating_the_phase(self):
        self.worker.fail_phase = "host/build.sh build"
        original = self.driver.guest
        def guest_call(slot, action, *arguments):
            if action == "export-diagnostics":
                self.assertEqual(self.broker.slots[slot]["state"], "running")
                self.assertEqual(arguments, (CONFIG["bucket"], self.driver.builder_prefixes[slot],
                    CONFIG["asset_region"], "host", "build"))
            return original(slot, action, *arguments)
        output = io.StringIO()
        with patch.object(self.driver, "guest", side_effect=guest_call), contextlib.redirect_stdout(output):
            with self.assertRaisesRegex(RuntimeError, "isolated builder command failed"):
                self.run_driver()
        commands = [item["Parameters"]["commands"][0] for item in self.worker.commands]
        self.assertEqual(sum("host/build.sh build" in text for text in commands), 1)
        self.assertEqual(sum("export-diagnostics" in text for text in commands), 1)
        row = json.loads(output.getvalue())["phaseDiagnostics"]
        self.assertEqual(row, {"kind": "host", "phase": "build", "sha256": "8" * 64,
            "version_id": "diagnostics-v1", "size": 123})
        self.assertNotIn("private-log-canary", output.getvalue())
        self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
        self.no_manifest()

    def test_diagnostics_failure_preserves_the_original_phase_exception(self):
        error = getattr(pipeline, "BuilderCommandFailed", RuntimeError)("the isolated builder command failed")
        self.driver.builder_prefixes["host-build"] = "builders/arn:aws:ec2:us-east-1:111111111111:instance/i-" + "a" * 17 + "/"
        output = io.StringIO()
        with patch.object(self.driver, "shell", side_effect=error), \
             patch.object(self.driver, "guest", side_effect=RuntimeError("private-error-canary")) as export, \
             contextlib.redirect_stdout(output):
            with self.assertRaises(RuntimeError) as caught:
                self.driver.phase("host-build", "host", "build")
        self.assertIs(caught.exception, error)
        self.assertNotIn("private-error-canary", output.getvalue())
        self.assertEqual(json.loads(output.getvalue())["phaseDiagnostics"]["archived"], False)
        self.assertEqual(export.call_count, 1)

    def test_successful_or_uncertain_phase_never_exports_diagnostics(self):
        for error in (None, TimeoutError("unknown command delivery")):
            with self.subTest(uncertain=error is not None), patch.object(self.driver, "shell", side_effect=error), \
                 patch.object(self.driver, "guest") as export:
                if error:
                    with self.assertRaises(TimeoutError):
                        self.driver.phase("host-build", "host", "build")
                else:
                    self.driver.phase("host-build", "host", "build")
                export.assert_not_called()

    def test_actual_ssm_cancellation_statuses_never_resubmit_or_archive_an_uncertain_phase(self):
        for statuses, succeeds in (([("Cancelling", -1), ("Success", 0)], True),
                                   ([("Cancelled", -1)], False)):
            with self.subTest(statuses=statuses):
                self.setUp()
                instance = "i-" + "a" * 17
                self.broker.slots["host-build"] = {"instance_id": instance, "state": "running", "boot": 1}
                self.driver.instances["host-build"] = instance
                self.driver.builder_prefixes["host-build"] = "builders/arn:aws:ec2:us-east-1:111111111111:instance/" + instance + "/"
                self.driver.deadline = 9100
                pending, polls = list(statuses), []
                original = self.broker.invoke
                def invoke(**arguments):
                    request = json.loads(arguments["Payload"])
                    response = original(**arguments)
                    if request["action"] == "command_status":
                        polls.append(request["command_id"])
                        value = json.loads(response["Payload"].read())
                        if pending:
                            value["status"], value["response_code"] = pending.pop(0)
                        response["Payload"] = io.BytesIO(json.dumps(value).encode())
                    return response
                output, error = io.StringIO(), None
                with patch.object(self.broker, "invoke", side_effect=invoke), contextlib.redirect_stdout(output):
                    try:
                        self.driver.phase("host-build", "host", "build")
                    except Exception as caught:
                        error = caught
                if succeeds:
                    self.assertIsNone(error)
                    self.assertEqual(self.now, 110)
                else:
                    self.assertIsInstance(error, TimeoutError)
                    self.assertEqual(self.now, 100)
                self.assertEqual(len(self.worker.commands), 1)
                self.assertIn("host/build.sh build", self.worker.commands[0]["Parameters"]["commands"][0])
                self.assertEqual(len(polls), len(statuses))
                self.assertEqual(len(set(polls)), 1)
                self.assertFalse(any(call["action"] == "launch" for call in self.broker.calls))
                self.assertEqual(self.driver.deadline, 9100)
                self.assertEqual(output.getvalue(), "")

    def test_every_security_or_runtime_failure_prevents_pair_publication(self):
        for proof in ("guard", "frames", "launch_path", "tls_rotation", "sanitized", "phone_clone", "windows_sandbox", "viewer_path"):
            with self.subTest(proof=proof):
                self.setUp()
                self.worker.bad_proof = proof
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.no_manifest()
                self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))

    def test_the_host_viewer_path_runs_once_on_the_cold_boot(self):
        # Every mode over both families is the slowest proof (the Windows gold flow, every mode's
        # first launch): the host's cold boot runs it, its reboot and stop/start boots do not.
        result = self.run_driver()
        self.assertEqual({k: result["images"][k]["validation"]["viewer_path"] for k in ("host", "tile")},
                         {"host": True, "tile": False})
        self.assertEqual(sorted(self.worker.viewer_asks),
                         sorted([("host-verify", True), ("host-verify", False), ("host-verify", False),
                                 ("tile-verify", False), ("tile-verify", False), ("tile-verify", False)]))

    def test_renderer_mismatch_cannot_publish_a_mixed_pair(self):
        self.worker.bad_renderer = True
        with self.assertRaisesRegex(RuntimeError, "different images"):
            self.run_driver()
        self.no_manifest()

    def test_artifact_versions_are_immutable_and_an_uncertain_copy_is_not_replayed(self):
        for fault in ("unversioned", "different-source", "uncertain-copy", "bad-checksum"):
            with self.subTest(fault=fault):
                self.setUp()
                self.worker.artifact_fault = fault
                with self.assertRaises((RuntimeError, TimeoutError)):
                    self.run_driver()
                self.assertFalse(self.broker.candidates)
                self.no_manifest()
                self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 1)

    def test_cleanup_requires_all_known_instance_ids_and_actual_termination(self):
        for field in ("hide_cleanup_slot", "change_cleanup_owner"):
            with self.subTest(field=field):
                self.setUp()
                setattr(self.broker, field, True)
                with self.assertRaisesRegex(RuntimeError, "broker"):
                    self.run_driver()
                self.no_manifest()

    def test_existing_build_is_not_replayed_or_destroyed(self):
        self.broker.existing = True
        with self.assertRaisesRegex(RuntimeError, "replay"):
            self.run_driver()
        self.assertEqual([call["action"] for call in self.broker.calls], ["status"])
        self.no_manifest()

    def test_dispatch_fence_readiness_waits_without_credentials_uploads_or_launches(self):
        self.broker.ready_after = 2
        original = self.worker.client
        def client(*args, **kwargs):
            self.assertGreater(self.broker.status_reads, 2, "worker accessed before the active-build fence")
            return original(*args, **kwargs)
        self.worker.client = client
        self.run_driver()
        self.assertEqual([value["action"] for value in self.broker.calls[:3]], ["status"] * 3)
        self.assertEqual(self.broker.calls[3]["action"], "launch")

    def test_waiting_readiness_cannot_change_build_source_input_or_deadline(self):
        for change in ({"source_commit": "f" * 40}, {"input_sha256": "e" * 64},
                       {"deadline_unix": 9200}, {"build_id": "another-build"}):
            with self.subTest(change=change):
                self.setUp()
                self.broker.ready_after = 2
                self.broker.midwait_change = change
                with self.assertRaisesRegex(RuntimeError, "identity changed"):
                    self.run_driver()
                self.assertFalse(self.worker.regions)
                self.assertFalse(self.worker.objects)
                self.assertFalse(self.broker.slots)

    def test_unready_dispatch_stops_at_the_original_deadline_without_paid_work(self):
        self.broker.ready_after = 100
        self.broker.deadline = 102
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.run_driver()
        self.assertEqual(self.now, 102)
        self.assertFalse(self.worker.regions)
        self.assertFalse(self.broker.slots)

    def test_source_and_deadline_are_broker_bound_before_any_launch(self):
        for name, value in (("source", "f" * 40), ("deadline", 100 + 360 * 60 + 1), ("deadline", 100), ("input_sha", "bad")):
            with self.subTest(name=name, value=value):
                self.setUp()
                setattr(self.broker, name, value)
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.assertFalse(any(call["action"] == "launch" for call in self.broker.calls))
                self.assertFalse(self.worker.regions)
                self.assertFalse(self.worker.objects)
                self.no_manifest()

    def test_a_lease_past_the_fixed_bound_is_refused_before_any_launch(self):
        self.broker.deadline = self.now + 360 * 60 + 1
        with self.assertRaisesRegex(RuntimeError, "not bounded by the CodeBuild lease"):
            self.run_driver()
        self.assertFalse(any(call["action"] == "launch" for call in self.broker.calls))

    def test_total_lease_accepts_360_minutes_without_extending_individual_commands(self):
        # 240 minutes never reached a tile builder (2026-11-08): the host side alone took 3 h.
        for seconds in (150 * 60 + 1, 240 * 60 + 1, 360 * 60):
            with self.subTest(seconds=seconds):
                self.setUp()
                deadline = self.now + seconds
                self.broker.deadline = deadline
                self.broker.ready_after = 1
                try:
                    result = self.run_driver()
                except RuntimeError as error:
                    self.fail(f"valid {seconds}-second total lease was refused: {error}")
                self.assertEqual(result["deadline_unix"], deadline)
                self.assertEqual(self.driver.deadline, deadline)
                self.assertEqual(set(self.broker.slots), set(pipeline.SLOTS))
                self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
                timeouts = [int(command["Parameters"]["executionTimeout"][0]) for command in self.worker.commands]
                self.assertTrue(timeouts)
                self.assertEqual(max(timeouts), 9000)
                self.assertTrue(all(0 < timeout <= 9000 for timeout in timeouts))

    def test_unchanged_boot_identity_cannot_count_as_resume_validation(self):
        self.worker.stale_boots = True
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.run_driver()
        # the two builders run side by side; no validator launches without a resumed boot
        launched = [call["slot"] for call in self.broker.calls if call["action"] == "launch"]
        self.assertLessEqual(set(launched), {"host-build", "tile-build"})
        self.no_manifest()

    def test_the_tile_build_runs_beside_the_host_build(self):
        # Three factory builds in a row (2026-11-08: fe5ccbfc, fb6baada, 853eb034) sealed the host
        # image about 3 h into the 4 h deadline and never launched a tile builder. The tile's
        # launch, prep and reboot need nothing from the host: the host's build phase is held
        # here until the tile's prep has been sent, which a tile build that waits for the host
        # never does. The tile's image phase still follows the host's renderer export.
        tile_prep = threading.Event()
        original = self.worker.send_command
        def send(**arguments):
            command = arguments["Parameters"]["commands"][0]
            if "tile/build.sh prep" in command:
                tile_prep.set()
            if "host/build.sh build" in command:
                self.assertTrue(tile_prep.wait(10), "the host's build phase began with no tile prep beside it")
            return original(**arguments)
        with patch.object(self.worker, "send_command", side_effect=send):
            self.run_driver()
        commands = [entry["Parameters"]["commands"][0] for entry in self.worker.commands]
        at = lambda text: next(index for index, command in enumerate(commands) if text in command)
        self.assertLess(at("tile/build.sh prep"), at("host/build.sh build"))
        self.assertLess(at("export-renderer"), at("tile/build.sh image"))

    def test_broker_never_accepts_caller_commands_profiles_or_launch_parameters(self):
        self.driver.call("status")
        with self.assertRaises(RuntimeError):
            self.driver.call("arbitrary-command", "host-build")
        with self.assertRaises(RuntimeError):
            self.driver.call("launch", "host-build-replacement")

    def test_every_broker_operation_is_bound_to_the_current_build_capability(self):
        self.driver.config = {**CONFIG, "capability": "wrong"}
        with self.assertRaisesRegex(RuntimeError, "broker request failed") as error:
            self.driver.call("status")
        self.assertNotIn("wrong", str(error.exception))
        self.assertNotIn(CONFIG["capability"], str(error.exception))
        self.assertFalse(self.broker.slots)
        self.broker.calls.clear()
        self.driver.config = {key: value for key, value in CONFIG.items() if key != "capability"}
        with self.assertRaises(KeyError):
            self.driver.call("status")
        self.assertFalse(self.broker.calls)

    def test_worker_receives_only_broker_issued_prefix_credentials_without_assume_role(self):
        prefix = "runs/" + hashlib.sha256(CONFIG["build_id"].encode()).hexdigest() + "/"
        requests, sessions = [], []
        def issue(action):
            requests.append(action)
            return {"asset_prefix": prefix, "credentials": {"AccessKeyId": "test", "SecretAccessKey": "test",
                "SessionToken": "test", "Expiration": 3700}}
        def session(**kwargs):
            sessions.append(kwargs)
            return SimpleNamespace(client=lambda name, **kwargs: name)
        with patch.dict(sys.modules, {"boto3": SimpleNamespace(Session=session),
            "botocore.config": SimpleNamespace(Config=lambda **kwargs: kwargs)}):
            worker = pipeline.Worker(issue, "us-east-1", CONFIG["build_id"], clock=lambda: 100)
            self.assertEqual(worker.client("ssm"), "ssm")
            worker.client("s3", "us-west-1")
            self.assertEqual(requests, ["credentials"])
            worker.invalidate()
            worker.client("ssm")
            self.assertEqual(requests, ["credentials", "credentials"])
        self.assertEqual(len(sessions), 2)

    def test_worker_s3_downloads_use_the_requested_regional_sigv4_endpoint(self):
        prefix = "runs/" + hashlib.sha256(CONFIG["build_id"].encode()).hexdigest() + "/"
        def issue(action):
            self.assertEqual(action, "credentials")
            return {"asset_prefix": prefix, "credentials": {"AccessKeyId": "test", "SecretAccessKey": "test",
                "SessionToken": "test", "Expiration": 3700}}
        def client(name, **kwargs):
            return name, kwargs
        with patch.dict(sys.modules, {"boto3": SimpleNamespace(Session=lambda **kwargs: SimpleNamespace(client=client)),
            "botocore.config": SimpleNamespace(Config=lambda **kwargs: kwargs)}):
            worker = pipeline.Worker(issue, "us-east-1", CONFIG["build_id"], clock=lambda: 100)
            for region in (None, "us-east-2"):
                with self.subTest(region=region):
                    name, options = worker.client("s3", region)
                    selected = region or "us-east-1"
                    self.assertEqual(name, "s3")
                    self.assertEqual(options, {"region_name": selected,
                        "endpoint_url": f"https://s3.{selected}.amazonaws.com", "config": {
                            "retries": {"total_max_attempts": 1}, "connect_timeout": 10, "read_timeout": 60,
                            "signature_version": "s3v4", "s3": {"addressing_style": "virtual"}}})
            for service in ("ssm", "secretsmanager"):
                with self.subTest(service=service):
                    self.assertEqual(worker.client(service, "us-east-2"), (service, {
                        "region_name": "us-east-2", "config": {"retries": {"total_max_attempts": 1},
                            "connect_timeout": 10, "read_timeout": 60}}))

    def test_dirty_checkout_fails_and_all_tracked_source_changes_are_fingerprinted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", "-c", "user.name=Image test", "-c",
                    "user.email=image@example.invalid", *args], cwd=root, stderr=subprocess.DEVNULL).decode().strip()
            git("init", "-q")
            (root / "Cargo.toml").write_text("package = 'test'\n")
            (root / "README.md").write_text("first docs\n")
            git("add", ".")
            git("commit", "-qm", "first")
            first = git("rev-parse", "HEAD")
            original, original_archive = pipeline.input_digest(root, first)
            (root / "README.md").write_text("updated docs\n")
            with self.assertRaisesRegex(RuntimeError, "unchanged"):
                pipeline.input_digest(root, first)
            git("commit", "-qam", "docs")
            second = git("rev-parse", "HEAD")
            digest, archive = pipeline.input_digest(root, second)
            self.assertNotEqual(digest, original)
            self.assertNotEqual(archive, original_archive)
            (root / "Cargo.toml").write_text("package = 'changed'\n")
            git("commit", "-qam", "input")
            self.assertNotEqual(pipeline.input_digest(root, git("rev-parse", "HEAD"))[0], original)


class GuestTests(unittest.TestCase):
    def test_a_windows_image_too_old_to_publish_is_refused_before_it_boots(self):
        # The agent refuses sandboxes at 170 days from installation (or 7 days before the
        # evaluation expires) and warns at 150; build.sh installs again once an inherited image
        # is 30 days old. A host image may publish only an image at most 30 days old.
        import datetime as dt
        now = dt.datetime(2026, 11, 8, tzinfo=dt.timezone.utc)
        def manifest(age_days, runway_days=180):
            installed = now - dt.timedelta(days=age_days)
            return {"installed_at": installed.strftime("%Y-%m-%dT%H:%M:%SZ"),
                    "eval_expires": (installed + dt.timedelta(days=runway_days)).strftime("%Y-%m-%dT%H:%M:%S+00:00")}
        guest.windows_image_publishable(manifest(0), now)
        guest.windows_image_publishable(manifest(30), now)
        for bad, why in ((manifest(31), "31 days old"), (manifest(171), "171 days old"),
                         (manifest(2, runway_days=100), "expires too soon"), ({}, "no dates")):
            with self.assertRaises(RuntimeError, msg=why):
                guest.windows_image_publishable(bad, now)

    def test_actual_renderer_prestart_refreshes_stale_cdi_on_every_boot(self):
        unit = (HERE / 'tile/fcvm-fleet-renderer.service').read_text()
        commands = [line.split('=', 1)[1].lstrip('-') for line in unit.splitlines()
                    if line.startswith('ExecStartPre=') or line.startswith('ExecStart=')]
        with tempfile.TemporaryDirectory(prefix='renderer-cdi-') as directory:
            root = Path(directory)
            device, current, cdi, events = [root / name for name in ('uvm-tools', 'major', 'cdi.json', 'events')]
            device.touch()
            cdi.write_text(json.dumps({'major': 509}))
            vendor = root / 'vendor'
            vendor.write_text('#!' + sys.executable + ' -I\n' + r'''
import json, os, pathlib, sys
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
major = int((root / 'major').read_text())
if args[0] == 'cdi':
    assert args == ['cdi', 'generate', '--device-name-strategy=index',
        '--disable-hook=apply-cuda-memory-limits', '--output=' + str(root / 'cdi.json')]
    assert (root / 'uvm-tools').exists()
    (root / 'cdi.json').write_text(json.dumps({'major': major}))
    with (root / 'events').open('a') as stream: stream.write('generate:' + str(major) + '\n')
elif 'run' in args:
    assert '--device' in args and args[args.index('--device') + 1] == 'nvidia.com/gpu=all'
    if json.loads((root / 'cdi.json').read_text())['major'] != major:
        sys.exit('stale CDI: baked UVM major differs from current device')
    with (root / 'events').open('a') as stream: stream.write('run:' + str(major) + '\n')
else:
    assert args == ['--cgroup-manager=cgroupfs', 'rm', '-f', 'renderer']
''')
            vendor.chmod(0o700)
            for major in (234, 235):
                current.write_text(str(major))
                for command in commands:
                    actual = command.replace('/dev/nvidia-uvm-tools', str(device)).replace(
                        '/etc/cdi/nvidia.yaml', str(cdi)).replace('/etc/fcvm-fleet/scene', str(root / 'scene'))
                    actual = actual.replace('/usr/bin/nvidia-ctk', str(vendor)).replace('/usr/bin/podman', str(vendor))
                    self.assertNotIn('/dev/nvidia', actual)
                    self.assertNotIn('/etc/', actual)
                    result = subprocess.run(shlex.split(actual), env={'PATH': '/usr/bin:/bin', 'FIXTURE_ROOT': str(root)},
                        capture_output=True, text=True, timeout=5)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(json.loads(cdi.read_text())['major'], major)
            self.assertEqual(events.read_text().splitlines(), ['generate:234', 'run:234', 'generate:235', 'run:235'])

    def test_renderer_identity_normalizes_only_documented_sha256_formats(self):
        # Podman 4.9.3's image-inspect .Id is bare hex; .Digest is a different identity.
        digest = "a3" * 32
        for value in (digest, "sha256:" + digest):
            with self.subTest(value=value), patch.object(guest, "command", return_value=value) as inspect:
                try:
                    actual = guest.renderer()
                except RuntimeError as error:
                    self.fail("documented image ID rejected: " + str(error))
                self.assertEqual(actual, {"image_id": "sha256:" + digest})
                inspect.assert_called_once_with(["podman", "image", "inspect", "localhost/fcvm-renderer:fleet",
                                                 "--format", "{{.Id}}"])
        for value in ("", digest[:-1], digest + "a", digest.upper(), "g" * 64,
                      "sha256:sha256:" + digest, "sha512:" + digest, digest + "\n" + digest):
            with self.subTest(invalid=value), patch.object(guest, "command", return_value=value):
                with self.assertRaisesRegex(RuntimeError, "renderer image has no digest"):
                    guest.renderer()

    def test_renderer_export_binds_upload_receipt_to_canonical_image_identity(self):
        prefix = "builders/arn:aws:ec2:us-east-1:111111111111:instance/i-" + "a" * 17 + "/"
        data, image = b"private renderer archive fixture", "c4" * 32
        calls = []
        class S3:
            def put_object(self, **arguments):
                calls.append("put")
                self.data = arguments["Body"].read()
                self.arguments = arguments
                return {"VersionId": "renderer-version-one"}

            def head_object(self, **arguments):
                calls.append("head")
                self.head_arguments = arguments
                digest = hashlib.sha256(self.data).hexdigest()
                return {"VersionId": "renderer-version-one", "Metadata": {"sha256": digest},
                        "ContentLength": len(self.data), "ServerSideEncryption": "AES256",
                        "ChecksumSHA256": pipeline.base64.b64encode(bytes.fromhex(digest)).decode()}

        def inspect(arguments):
            self.assertEqual(calls, ["put", "head"])
            calls.append("inspect")
            self.assertEqual(arguments[-1], "{{.Id}}")
            return image

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "renderer.oci.tar.zst"
            path.write_bytes(data)
            client = S3()
            with patch.object(guest, "Path", return_value=path), \
                 patch.object(guest, "bucket_client", return_value=client) as selected, \
                 patch.object(guest, "command", side_effect=inspect):
                try:
                    result = guest.export_renderer("example-images", prefix, "us-east-2")
                except RuntimeError as error:
                    self.fail("uploaded renderer identity rejected: " + str(error))
            selected.assert_called_once_with("example-images", prefix, "us-east-2")
            self.assertEqual(calls, ["put", "head", "inspect"])
            self.assertEqual(client.data, data)
            self.assertEqual(client.arguments["Key"], prefix + "renderer.oci.tar.zst")
            self.assertEqual(client.head_arguments, {"Bucket": "example-images", "Key": prefix + "renderer.oci.tar.zst",
                                                   "VersionId": "renderer-version-one", "ChecksumMode": "ENABLED"})
            self.assertEqual(result, {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data),
                                      "version_id": "renderer-version-one", "image_id": "sha256:" + image})

    def ordering_commands(self, states, started=20):
        reads = []
        current = [0]
        def command(arguments, timeout=120):
            if "--property=ActiveState" in arguments:
                reads.append(arguments)
                state = states[min(len(reads) - 1, len(states) - 1)]
                current[0] = min(len(reads) - 1, len(states) - 1)
                return state + "\n" + state
            if arguments[:2] == ["systemctl", "is-active"]:
                return "active" if arguments[2] == guest.GUARD else states[current[0]]
            if "--property=ActiveEnterTimestampMonotonic" in arguments:
                return "10"
            if "--property=ExecMainStartTimestampMonotonic" in arguments:
                return str(started)
            raise AssertionError(arguments)
        return command, reads

    def test_boot_ordering_waits_for_restoring_consumers_before_proving_timestamps(self):
        for kind in ("host", "tile"):
            with self.subTest(kind=kind):
                command, reads = self.ordering_commands(["activating", "active"])
                with patch.object(guest, "command", side_effect=command), \
                     patch.object(guest.time, "sleep") as sleep:
                    try:
                        guest.ordering(kind)
                    except RuntimeError as error:
                        self.fail(str(error))
                self.assertEqual(len(reads), 2)
                sleep.assert_called_once()
                if kind == "host":
                    self.assertIn("fcvm-fleet-vfio.service", reads[0])
                    self.assertIn("fcvm-fleet-store.service", reads[0])

    def test_boot_readiness_rejects_failed_units_and_still_checks_guard_ordering(self):
        for states, started, message in ((["failed"], 20, "failed during boot"),
                                          (["active"], 5, "started before the guard")):
            with self.subTest(message=message):
                command, _ = self.ordering_commands(states, started)
                with patch.object(guest, "command", side_effect=command), \
                     patch.object(guest.time, "sleep") as sleep:
                    with self.assertRaisesRegex(RuntimeError, message):
                        guest.ordering("host")
                sleep.assert_not_called()

    def test_boot_readiness_stops_at_its_fixed_bound(self):
        command, reads = self.ordering_commands(["activating"])
        now = [0]
        def sleep(seconds):
            self.assertLessEqual(seconds, 1)
            now[0] += 900
        with patch.object(guest, "command", side_effect=command), \
             patch.object(guest.time, "monotonic", side_effect=lambda: now[0]), \
             patch.object(guest.time, "sleep", side_effect=sleep):
            with self.assertRaisesRegex(RuntimeError, "did not become active"):
                guest.ordering("host")
        self.assertEqual(now[0], 1800)
        self.assertEqual(len(reads), 2)

    def test_single_put_checks_the_server_checksum_even_when_metadata_claims_success(self):
        class S3:
            def __init__(self, corrupt):
                self.corrupt = corrupt

            def put_object(self, **arguments):
                data = arguments["Body"].read()
                self.sha = hashlib.sha256(data).hexdigest()
                self.size = len(data)
                self.checksum = pipeline.base64.b64encode(bytes.fromhex(self.sha)).decode()
                self.assertions = (arguments["ContentLength"] == self.size,
                    arguments["ChecksumAlgorithm"] == "SHA256", arguments["ChecksumSHA256"] == self.checksum,
                    arguments["ServerSideEncryption"] == "AES256")
                return {"VersionId": "immutable-v1"}

            def head_object(self, **arguments):
                assert arguments["VersionId"] == "immutable-v1" and arguments["ChecksumMode"] == "ENABLED"
                return {"VersionId": "immutable-v1", "Metadata": {"sha256": self.sha},
                    "ContentLength": self.size, "ServerSideEncryption": "AES256",
                    "ChecksumSHA256": "lying-server-checksum" if self.corrupt else self.checksum}

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "part.tar.zst"
            path.write_bytes(b"original archive bytes")
            good = S3(False)
            self.assertEqual(guest.upload(good, "bucket", "key", path)["version_id"], "immutable-v1")
            self.assertTrue(all(good.assertions))
            with self.assertRaisesRegex(RuntimeError, "server checksum"):
                guest.upload(S3(True), "bucket", "key", path)
            original_head = good.head_object
            def wrong_encryption(**arguments):
                return {**original_head(**arguments), "ServerSideEncryption": "aws:kms"}
            with patch.object(good, "head_object", side_effect=wrong_encryption):
                with self.assertRaisesRegex(RuntimeError, "encryption"):
                    guest.upload(good, "bucket", "key", path)

    def test_private_diagnostics_export_reads_only_the_fixed_bounded_tail(self):
        prefix = "builders/arn:aws:ec2:us-east-1:111111111111:instance/i-" + "a" * 17 + "/"
        class S3:
            def put_object(self, **arguments):
                self.arguments = arguments
                self.data = arguments["Body"].read()
                self.digest = hashlib.sha256(self.data).hexdigest()
                self.checksum = pipeline.base64.b64encode(bytes.fromhex(self.digest)).decode()
                return {"VersionId": "private-diagnostics-v1"}
            def head_object(self, **arguments):
                self.head = arguments
                return {"VersionId": "private-diagnostics-v1", "Metadata": {"sha256": self.digest},
                    "ContentLength": len(self.data), "ChecksumSHA256": self.checksum, "ServerSideEncryption": "AES256"}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "phase.log"
            data = b"older-private-log\n" + b"tail-private-canary\n" * 60000
            path.write_bytes(data)
            original_fstat = guest.os.fstat
            def root_file(descriptor):
                value = original_fstat(descriptor)
                return SimpleNamespace(st_mode=value.st_mode, st_uid=0, st_size=value.st_size)
            client, output = S3(), io.StringIO()
            self.assertEqual(guest.DIAGNOSTIC_LOG, Path("/var/log/fleet-image-phase.log"))
            with patch.object(guest, "DIAGNOSTIC_LOG", path), patch.object(guest.os, "fstat", side_effect=root_file), \
                 patch.object(guest, "bucket_client", return_value=client) as selected, contextlib.redirect_stdout(output):
                receipt = guest.export_diagnostics("example-images", prefix, "us-east-2", "host", "build")
            selected.assert_called_once_with("example-images", prefix, "us-east-2")
            self.assertEqual(client.data, data[-1024 * 1024:])
            self.assertEqual(client.arguments["Key"], prefix + "diagnostics/host-build.log")
            self.assertEqual(client.arguments["ServerSideEncryption"], "AES256")
            self.assertEqual(client.arguments["ChecksumSHA256"], client.checksum)
            self.assertEqual(client.head["VersionId"], "private-diagnostics-v1")
            self.assertEqual(receipt, {"sha256": client.digest, "size": 1024 * 1024, "version_id": "private-diagnostics-v1"})
            self.assertEqual(output.getvalue(), "")
            self.assertFalse(Path(client.arguments["Body"].name).exists())

    def test_diagnostics_rejects_nonregular_symlink_and_untrusted_destinations(self):
        prefix = "builders/arn:aws:ec2:us-east-1:111111111111:instance/i-" + "a" * 17 + "/"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "other.log"; target.write_text("must never be uploaded")
            link = root / "phase.log"; link.symlink_to(target)
            fifo = root / "fifo"; guest.os.mkfifo(fifo)
            for source in (link, fifo):
                with self.subTest(source=source.name), patch.object(guest, "DIAGNOSTIC_LOG", source), \
                     patch.object(guest, "bucket_client"), patch.object(guest, "upload") as upload:
                    with self.assertRaises((OSError, RuntimeError)):
                        guest.export_diagnostics("example-images", prefix, "us-east-2", "host", "build")
                    upload.assert_not_called()
        with patch.object(guest, "bucket_client") as client:
            for kind, phase in (("host", "../private"), ("tile", "build"), ("unknown", "prep")):
                with self.assertRaises(RuntimeError):
                    guest.export_diagnostics("example-images", prefix, "us-east-2", kind, phase)
            client.assert_not_called()
        with patch.dict(sys.modules, {"boto3": SimpleNamespace(client=lambda *args, **kwargs: self.fail("invalid destination reached SDK"))}):
            with self.assertRaises(RuntimeError):
                guest.export_diagnostics("example-images", "arbitrary/path/", "us-east-2", "host", "build")

    def test_store_requires_manifest_and_exact_seventeen_archives(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "parts").write_text("".join(f"part-{index:02d}\n" for index in range(16)))
            for name in [*(f"part-{index:02d}" for index in range(16)), "dirs"]:
                (root / (name + ".tar.zst")).write_bytes(b"archive")
            with patch.object(guest, "Path", return_value=root):
                self.assertEqual(len(guest.store_parts()[1]), 17)
                (root / "unknown.tar.zst").write_bytes(b"extra")
                with self.assertRaisesRegex(RuntimeError, "archive set"):
                    guest.store_parts()
                (root / "unknown.tar.zst").unlink()
                (root / "dirs.tar.zst").unlink()
                with self.assertRaisesRegex(RuntimeError, "archive set"):
                    guest.store_parts()

    def test_rotated_tls_identity_is_mandatory_before_any_video_proof(self):
        document = {"cert": "same", "key": "private", "scene_token": "same-token"}
        with patch.multiple(guest, sanitized=lambda: True, guard=lambda: True,
            ordering=lambda kind: None, launch_path_proof=lambda: True, renderer=lambda: {"image_id": IMAGE},
            secret=lambda arn, region: document, cert_hash=lambda cert: cert,
            host_proof=lambda *args: self.fail("unsafe identity reached the GPU proof")):
            with self.assertRaisesRegex(RuntimeError, "not distinct"):
                guest.verify("host", IMAGE, "a", "b", "region")

    def test_the_viewer_path_is_a_host_proof_and_reports_its_failing_checks(self):
        for proofs in (("viewer-path",), ("frames",)):
            with self.subTest(proofs=proofs), self.assertRaisesRegex(RuntimeError, "unknown validator proof"):
                guest.verify("tile" if proofs == ("viewer-path",) else "host", IMAGE, "a", "b", "region", *proofs)
        # A failing check reaches the record as the smoke's own fields, and a value that is not a
        # short plain string (a URL, here) is dropped rather than reported.
        lines = [{"mode": "windows", "tile": "win:1", "family": "v6", "ok": False, "step": "connect",
                  "error": "timed out"},
                 {"mode": "wall", "tile": "fcvm:1", "family": "v6", "ok": True},
                 {"mode": "minecraft", "tile": "mc:1", "family": "v4", "ok": False, "step": "connect",
                  "error": "wss://mc-1-abcdefgh-v4.example/view?admit=secret"}]
        ran = []
        class Done:
            def __init__(self, code): self.returncode = code
        def run(code, written):
            def go(argv, **kwargs):
                ran.append((argv, kwargs["env"].get("KEEP")))
                for family, items in written.items():
                    (Path(argv[2]) / f"smoke-{family}.jsonl").write_text("".join(json.dumps(i) + "\n" for i in items))
                return Done(code)
            return go
        real = tempfile.TemporaryDirectory
        for code, written, expected in (
                (1, {"v6": lines}, [{"mode": "windows", "tile": "win:1", "family": "v6", "step": "connect", "error": "timed out"},
                                    {"mode": "minecraft", "tile": "mc:1", "family": "v4", "step": "connect"}]),
                (0, {"v4": lines[1:2], "v6": lines[1:2]}, []),
                (2, {}, [{"step": "the viewer-path acceptance could not run"}])):
            with self.subTest(code=code), real() as directory, \
                    patch.object(guest.tempfile, "TemporaryDirectory", lambda dir=None: real(dir=directory)), \
                    patch.object(guest.subprocess, "run", run(code, written)):
                self.assertEqual(guest.viewer_proof(), expected)
        self.assertTrue(ran[0][0][1].endswith("fleet/tools/viewer-acceptance.sh"), ran)
        self.assertEqual({keep for _, keep in ran}, {"1"}, "the script keeps its results for the validator to read")
        one = [{"mode": "windows", "tile": "win:1", "family": "v6", "step": "connect", "error": "timed out"}]
        self.assertEqual(pipeline.viewer_summary(one), "windows win:1 v6 connect timed out")
        self.assertEqual(pipeline.viewer_summary(None), "no failing check reported")

    def test_frame_bound_accepts_real_binary_header_and_rejects_oversize(self):
        for wire, expected in ((b"\x82\x03abc", (2, b"abc")),
                               (b"\x82\x7f" + struct.pack("!Q", 1048577), None),
                               (b"\x82\x80", None), (b"\x02\x00", None)):
            with self.subTest(wire=wire):
                left, right = socket.socketpair()
                try:
                    left.sendall(wire)
                    if expected:
                        self.assertEqual(guest.ws_frame(right), expected)
                    else:
                        with self.assertRaises(RuntimeError):
                            guest.ws_frame(right)
                finally:
                    left.close()
                    right.close()


class LaunchPathTests(unittest.TestCase):
    # ejc3/aws#823: generation twelve's host booted, as the fleet launched it, without its user
    # data. On a first boot from the factory's image (g4dn.metal, 2026-11-14) ENA logged its
    # probe at 27.78 s and systemd started cloud-init-local at 25.96 s.
    DEVICE = "0000:00:05.0"

    def journal(self, probe, received=None):
        entries = [{"MESSAGE": "Linux version 6.18.60-fcvm", "_TRANSPORT": "kernel",
                    "_SOURCE_MONOTONIC_TIMESTAMP": "1000", "__MONOTONIC_TIMESTAMP": "4000000"}]
        nic = {"MESSAGE": "ena " + self.DEVICE + ": Elastic Network Adapter (ENA) found at mem fe800000, mac addr 0a:00:00:00:00:01",
               "_TRANSPORT": "kernel", "_SOURCE_MONOTONIC_TIMESTAMP": str(probe),
               "__MONOTONIC_TIMESTAMP": str(received if received is not None else probe)}
        rename = {"MESSAGE": "ena " + self.DEVICE + " ens5: renamed from eth0", "_TRANSPORT": "kernel",
                  "_SOURCE_MONOTONIC_TIMESTAMP": "40000000", "__MONOTONIC_TIMESTAMP": "40000000"}
        return entries + [nic, rename]

    def test_generation_twelves_boot_order_fails_even_when_it_won_the_race(self):
        self.assertFalse(guest.nic_bound_before_cloud_init(self.journal(27_784_460), "ena", self.DEVICE, 25_963_145))

    def test_a_nic_bound_before_cloud_init_passes(self):
        # ENA built in probes during the kernel's own boot, seconds before userspace; journald reads
        # the kernel's backlog later, so the kernel's own timestamp is the one compared
        self.assertTrue(guest.nic_bound_before_cloud_init(self.journal(2_904_118, received=11_000_000),
                                                          "ena", self.DEVICE, 10_412_771))

    def test_a_missing_probe_or_start_cannot_pass(self):
        with self.assertRaises(RuntimeError):
            guest.nic_bound_before_cloud_init(self.journal(2_000_000)[:1], "ena", self.DEVICE, 10_000_000)
        for start in (0, None):
            with self.subTest(start=start), self.assertRaises(RuntimeError):
                guest.nic_bound_before_cloud_init(self.journal(2_000_000), "ena", self.DEVICE, start)
        # another device's probe is not this NIC's
        with self.assertRaises(RuntimeError):
            guest.nic_bound_before_cloud_init(self.journal(2_000_000), "ena", "0000:00:06.0", 10_000_000)

    def test_the_start_comes_from_systemd_not_the_journal_line(self):
        # a generation-eleven first boot (2026-11-14) had no "Starting cloud-init-local" line in its
        # journal: the start time is systemd's own record of the unit
        source = Path(__file__).with_name("pipeline_guest.py").read_text()
        body = source.split("def launch_path_proof():", 1)[1].split("\ndef ", 1)[0]
        self.assertIn('"--property=InactiveExitTimestampMonotonic"', body)
        self.assertNotIn("Starting", body)

    def test_the_proof_runs_on_every_validator_boot(self):
        source = (Path(__file__).with_name("pipeline_guest.py")).read_text()
        body = source.split("def verify(kind, expected_image", 1)[1].split("\ndef ", 1)[0]
        self.assertLess(body.index("ordering(kind)"), body.index("launch_path_proof()"))
        self.assertIn('"launch_path": True', body)


RENDERS = HERE / "renders"


def renders_phases():
    source = (RENDERS / "build.sh").read_text()
    return source, dict(re.findall(r"\n(\w+)\)\n(.*?)\n  ;;", source, re.S))


def host_check():
    spec = importlib.util.spec_from_file_location("renders_host_check", RENDERS / "host-check.py")
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def unit_settings(path):
    """KEY -> [values] of a unit file's settings, comments left out."""
    found = {}
    for line in Path(path).read_text().splitlines():
        if "=" in line and not line.lstrip().startswith(("#", "[")):
            key, value = line.split("=", 1)
            found.setdefault(key.strip(), []).append(value.strip())
    return found


class RendersTrackTests(unittest.TestCase):
    """The renders image is a track of its own: its own build, broker slots, lease and manifest.
    It shares nothing with the host/tile pair, so a failure of either cannot publish or hold the
    other."""

    def setUp(self):
        self.broker = Broker()
        self.broker.track = "renders"
        self.worker = Worker(self.broker)
        self.now = 100
        self.driver = pipeline.Pipeline(self.broker, self.worker, {**CONFIG, "track": "renders"},
            root=HERE.parents[1], clock=lambda: self.now, sleep=self.advance)

    def advance(self, seconds):
        self.now += seconds

    def run_driver(self):
        with patch.object(pipeline, "input_digest", return_value=("6" * 64, b"immutable archive")):
            return self.driver.run()

    def commands(self, slot):
        identifier = self.broker.slots[slot]["instance_id"]
        return [entry["Parameters"]["commands"][0] for entry in self.worker.commands if entry["InstanceIds"] == [identifier]]

    def test_the_renders_track_builds_validates_and_records_one_image(self):
        result = self.run_driver()
        self.assertEqual(set(self.broker.slots), {"renders-build", "renders-verify"})
        self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 2)
        self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))
        self.assertEqual(result["track"], "renders")
        self.assertEqual(set(result["images"]), {"renders"})
        image = result["images"]["renders"]
        self.assertEqual(image["image_id"], "ami-" + "c" * 17)
        self.assertEqual(image["architecture"], "arm64")
        self.assertEqual(image["build"], RENDERS_RECORD)
        self.assertEqual(len(set(image["validation"]["boot_ids"])), 3)
        self.assertTrue(image["validation"]["renders_agent"])
        # its store goes to a prefix of its own, which its bake names in store.env
        self.assertEqual(result["store"]["destination"]["prefix"], "factory/" + "3" * 64 + "/renders/store/")
        self.assertEqual(len(result["store"]["objects"]), 19)
        env = next(text for text in self.commands("renders-build") if "cat > /var/lib/fleet-ami/env" in text)
        self.assertIn("export STORE_S3_PREFIX=factory/" + "3" * 64 + "/renders/store/", env)
        self.assertIn("export FCVM_COMMIT=" + SOURCE, env)
        # no renderer and no certificate: the track writes no TLS canary and exports no renderer
        self.assertNotIn("FLEET_TLS_SECRET", env)
        self.assertEqual(self.worker.secret_writes, 0)
        self.assertNotIn("tls", result)
        self.assertFalse(any("export-renderer" in text for text in self.commands("renders-build")))
        phases = [text for text in self.commands("renders-build") if "/renders/build.sh" in text]
        self.assertEqual([re.search(r"build\.sh (\w+)", text).group(1) for text in phases], ["prep", "build", "bake"])
        upgrade = next(i for i, text in enumerate(self.commands("renders-build")) if "--with-new-pkgs upgrade" in text)
        prep = next(i for i, text in enumerate(self.commands("renders-build")) if "/renders/build.sh prep" in text)
        self.assertLess(upgrade, prep)

    def test_the_agent_check_and_the_units_own_words_run_on_the_cold_boot_only(self):
        result = self.run_driver()
        self.assertEqual(self.worker.renders_asks, [("renders-agent", "renders-unit"), (), ()])
        self.assertTrue(result["images"]["renders"]["validation"]["renders_unit"])

    def test_a_pass_without_its_count_is_no_pass(self):
        # A verdict is the guest's raw one, with the number of failures behind it: a result that
        # says pass but carries no count (an older or damaged guest) does not publish.
        for drop in ("renders_host_problem_count", "renders_agent_failure_count", "renders_unit_failure_count"):
            with self.subTest(drop=drop):
                self.setUp()
                self.worker.renders_drop = (drop,)
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.assertFalse(any(name.endswith("manifest.json") for name in self.worker.objects))

    def test_a_failed_renders_proof_publishes_nothing_and_says_why(self):
        for proof, words in (("guard", "security"), ("sanitized", "security"), ("launch_path", "launch-path"),
                             ("renders_host", "2 .*held:engines"), ("renders_agent", "2 .*kill:running"),
                             ("renders_unit", "unit.*2 .*kill:running")):
            with self.subTest(proof=proof):
                self.setUp()
                self.worker.bad_proof = proof
                with self.assertRaisesRegex(RuntimeError, words):
                    self.run_driver()
                self.assertFalse(any(name.endswith("manifest.json") for name in self.worker.objects))
                self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))

    def test_the_validated_image_boots_the_kernel_its_build_installed(self):
        self.worker.renders_kernel = "7.3.3-fcvm-ba9876543210"
        with self.assertRaisesRegex(RuntimeError, "kernel"):
            self.run_driver()
        self.assertFalse(any(name.endswith("manifest.json") for name in self.worker.objects))

    def test_the_build_record_must_name_the_built_commit_and_the_reference_boot_line(self):
        for field, value in (("fcvm_commit", "f" * 40), ("kitesurf_commit", "main"), ("bundle_sha256", ""),
                             ("kernel_release", "7.3.3-generic"), ("boot_params", "kvm-arm.mode=nested"),
                             ("hugepages", 1)):
            with self.subTest(field=field):
                self.setUp()
                self.worker.renders_record[field] = value
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.assertFalse(self.broker.candidates, "an image was sealed from a build with a wrong record")

    def test_a_renders_build_refuses_the_pairs_slots_and_the_pair_refuses_its(self):
        with self.assertRaisesRegex(RuntimeError, "slot"):
            self.driver.call("launch", "host-build")
        pair = pipeline.Pipeline(self.broker, self.worker, CONFIG, root=HERE.parents[1],
                                 clock=lambda: self.now, sleep=self.advance)
        for slot in pipeline.RENDERS_SLOTS:
            with self.subTest(slot=slot), self.assertRaisesRegex(RuntimeError, "slot"):
                pair.call("launch", slot)

    def test_the_pair_is_the_default_track_and_an_unknown_track_is_refused(self):
        self.assertEqual(pipeline.Pipeline(self.broker, self.worker, CONFIG).track, "pair")
        self.assertEqual(pipeline.TRACKS, {"pair": pipeline.SLOTS, "renders": pipeline.RENDERS_SLOTS})
        with self.assertRaisesRegex(RuntimeError, "track"):
            pipeline.Pipeline(self.broker, self.worker, {**CONFIG, "track": "everything"})
        # The pair's CodeBuild project sets nothing new: without FLEET_AMI_TRACK it builds the pair.
        source = (HERE / "pipeline.py").read_text().split("\ndef main():", 1)[1]
        self.assertIn('config["track"] = os.environ.get("FLEET_AMI_TRACK", "pair")', source)


class RendersRecipeTests(unittest.TestCase):
    def test_the_boot_line_is_the_nested_arm64_profiles_and_the_references(self):
        import tomllib
        conf = host_check().read_conf((RENDERS / "renders.conf").read_text())
        profile = tomllib.loads((HERE.parents[1] / "rootfs-config.toml").read_text())["kernel_profiles"]["nested"]["arm64"]
        # make install-host-kernel writes the profile's boot_args to GRUB; the check expects these
        self.assertEqual(conf["RENDERS_BOOT_PARAMS"], profile["boot_args"])
        self.assertEqual(conf["RENDERS_BOOT_PARAMS"].split(), ["kvm-arm.mode=nested", "numa=off", "arm64.nv2"])
        self.assertEqual(conf["RENDERS_HUGEPAGES"], "34000")

    def test_prep_installs_fcvms_host_kernel_and_records_grubs_default(self):
        _, phases = renders_phases()
        prep = phases["prep"]
        self.assertIn('as_user "cd $SRC && make install-host-kernel"', prep)
        self.assertLess(prep.index("store_settled"), prep.index("make install-host-kernel"))
        self.assertLess(prep.index('as_user "cd $SRC && make setup-btrfs"'), prep.index("make install-host-kernel"))
        record = prep.index("/etc/fcvm-fleet/renders-kernel")
        self.assertLess(prep.index("make install-host-kernel"), record)
        self.assertIn("with Linux", prep[record - 300:record])

    def test_prep_loads_the_nic_driver_before_cloud_init_on_the_initrdless_boot(self):
        # ENA is a module of the arm64 host kernel (its config is the running Ubuntu kernel's, which
        # has it =m; kernel/host-arm64.conf does not pin it) and the image boots
        # Ubuntu's initrdless path, as the campaign host did. systemd-modules-load loads the driver,
        # which binds the present NIC inside the load, and cloud-init-local is ordered after it.
        _, phases = renders_phases()
        prep = phases["prep"]
        self.assertIn("echo ena > /etc/modules-load.d/fcvm-fleet-nic.conf", prep)
        self.assertIn("install -d /etc/systemd/system/cloud-init-local.service.d", prep)
        self.assertIn("printf '[Unit]\\nAfter=systemd-modules-load.service\\n' > "
                      "/etc/systemd/system/cloud-init-local.service.d/10-fcvm-fleet-nic.conf", prep)
        # the boot path stays Ubuntu's: no initramfs module list, no GRUB_FORCE_PARTUUID override
        self.assertNotIn("/etc/initramfs-tools/modules", prep)
        self.assertNotIn("GRUB_FORCE_PARTUUID", prep)
        self.assertNotIn("95-fcvm-fleet-initrd.cfg", prep)
        self.assertLess(prep.index("/etc/modules-load.d/fcvm-fleet-nic.conf"), prep.index("systemctl daemon-reload"))

    def test_the_build_runs_only_on_the_built_kernel_and_the_reference_boot_line(self):
        _, phases = renders_phases()
        build = phases["build"]
        boot = build.index('python3 "$HERE/host-check.py" boot')
        self.assertLess(boot, build.index('as_user "cd $SRC && make build"'))
        self.assertLess(boot, build.index("store_settled"))

    def test_kitesurf_is_built_at_its_pin_in_a_named_unit_beside_fcvms_build(self):
        source, phases = renders_phases()
        build = phases["build"]
        start = build.index("bg_start fleet-ami-kitesurf")
        self.assertLess(build.index("git checkout -q -f --detach $FCVM_COMMIT"), start)
        self.assertLess(start, build.index('as_user "cd $SRC && make build"'))
        unit = build[start:build.index("\n", build.index("\n", start) + 1)]
        self.assertIn("kitesurf-dist.sh", unit)
        # the flavor's own pin, renders.conf's, not the other benchmark arm's kitesurf_pins.sh
        self.assertIn('"$RENDERS_KITESURF_REV"', unit)
        wait = build.index('bg_wait fleet-ami-kitesurf "$L/kitesurf.log"')
        self.assertLess(wait, build.index('as_user "cd $SRC && make test-fleet-renders-agent'))
        code = "\n".join(line for line in source.splitlines() if not line.lstrip().startswith("#"))
        self.assertNotIn("kitesurf_pins.sh", code)
        self.assertIn('BG_UNITS="fleet-ami-kitesurf"', source)
        dist = (RENDERS / "kitesurf-dist.sh").read_text()
        # the repository's build script runs from a copy whose pins name the flavor's commit
        self.assertIn('bash "$TOOLS/kitesurf_build.sh"', dist)
        self.assertNotIn('bash "$SRC/bench/chromium/kitesurf_build.sh"', dist)
        self.assertIn('--rev "$REV"', dist)
        prepare = dist.index("wpt-engine.mjs prepare")
        bundle = dist.index("build //packages/engine:dist")
        self.assertLess(dist.index("kitesurf_build.sh"), prepare)
        self.assertLess(prepare, bundle)
        # Bazel's default mode, served from its own output directory (its assets are beside it)
        self.assertNotIn("-c opt", dist)
        self.assertNotIn("--compilation_mode", dist)
        self.assertIn('ln -s "$(readlink -f "$out")" "$DIST"', dist)
        self.assertIn("kitesurf_seal.py\" write", dist)

    def run_tools(self, rev):
        """kitesurf-dist.sh's tools(): copies of the build, pins and seal scripts with the pins'
        commit replaced, in a scratch directory."""
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "src/bench/chromium"
            src.mkdir(parents=True)
            for name in ("kitesurf_build.sh", "kitesurf_pins.sh", "kitesurf_seal.py"):
                (src / name).write_bytes((HERE.parents[1] / "bench/chromium" / name).read_bytes())
            program = ("KITESURF_DIST_LIB=1\n. " + shlex.quote(str(RENDERS / "kitesurf-dist.sh")) + "\n"
                       "tools " + shlex.quote(str(Path(tmp) / "src")) + " " + shlex.quote(rev) + " "
                       + shlex.quote(str(Path(tmp) / "tools")) + "\n")
            done = subprocess.run(["bash", "-c", program], capture_output=True, text=True, timeout=30,
                                  env={"PATH": "/usr/bin:/bin"})
            copies = {p.name: p.read_text() for p in (Path(tmp) / "tools").glob("*")} if done.returncode == 0 else {}
            original = (src / "kitesurf_pins.sh").read_text()
            return done, copies, original

    def test_kitesurf_is_built_from_copies_whose_pins_name_the_flavors_commit(self):
        rev = "0123456789abcdef0123456789abcdef01234567"
        done, copies, original = self.run_tools(rev)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(set(copies), {"kitesurf_build.sh", "kitesurf_pins.sh", "kitesurf_seal.py"})
        pins = copies["kitesurf_pins.sh"].splitlines()
        self.assertEqual([line for line in pins if line.startswith("KITESURF_REV=")], ["KITESURF_REV=" + rev])
        # everything else in the pins (node, pnpm, Bazelisk) is the repository's, and the
        # repository's own file is left as it was
        self.assertEqual([line for line in pins if not line.startswith("KITESURF_REV=")],
                         [line for line in original.splitlines() if not line.startswith("KITESURF_REV=")])
        self.assertEqual(original, (HERE.parents[1] / "bench/chromium/kitesurf_pins.sh").read_text())
        for bad in ("main", rev[:12], rev + "0"):
            with self.subTest(rev=bad):
                self.assertNotEqual(self.run_tools(bad)[0].returncode, 0)

    def test_the_flavor_runs_the_recorded_campaigns_configuration(self):
        # The page's reference figures and a live host come from one configuration: the image's
        # settings that the campaign's summary and host records carry are read from them here,
        # never typed. The levels, the viewport, the clones' vCPUs, memory and page size and the
        # engines' recycle threshold are the module's (its defaults, or the search's), not the
        # image's, and are not compared.
        summaries = sorted((HERE.parents[1] / "bench/chromium/results").glob("renders-*-summary.json"))
        self.assertEqual(len(summaries), 1, summaries)
        summary = json.loads(summaries[0].read_text())
        conf = host_check().read_conf((RENDERS / "renders.conf").read_text())
        self.assertEqual(conf["RENDERS_KITESURF_REV"], summary["kitesurf_commit"])
        held = next(r for r in summary["regimes"] if r["label"] == "held")
        fcvm, engines = held["sides"]["fcvm"]["setup"], held["sides"]["kitesurf"]["setup"]
        self.assertEqual(conf["RENDERS_MAX_FCVM"], fcvm["max_in_flight"])
        self.assertEqual(conf["RENDERS_MAX_ENGINES"], engines["engines"])
        self.assertEqual(conf["RENDERS_HUGEPAGES"], fcvm["pool_pages"])
        arguments = dict(word.split("=", 1) for word in held["arguments"].split())
        self.assertEqual((arguments["MAX_FCVM"], arguments["MAX_ENGINES"]),
                         (conf["RENDERS_MAX_FCVM"], conf["RENDERS_MAX_ENGINES"]))
        launch = re.search(r"\nRENDERS_TYPE=(\S+)\n", (HERE / "build-ami.sh").read_text()).group(1)
        self.assertEqual(launch, summary["instance_type"])
        # every run's host record names the boot line the image boots with
        records = sorted((summaries[0].parent / summary["campaign"]).glob("*/host-record.txt"))
        self.assertTrue(records)
        for record in records:
            lines = dict(line.split(": ", 1) for line in record.read_text().splitlines())
            with self.subTest(run=record.parent.name):
                self.assertEqual(lines["cmdline"], conf["RENDERS_BOOT_PARAMS"])
                self.assertEqual(lines["kitesurf"], conf["RENDERS_KITESURF_REV"])
        # the host kernel's version; its hash is the patches' and config fragment's at the
        # commit the image builds
        import tomllib
        host_kernel = tomllib.loads((HERE.parents[1] / "rootfs-config.toml").read_text())[
            "kernel_profiles"]["nested"]["arm64"]["host_kernel"]
        self.assertEqual(summary["kernel"].split("-fcvm-")[0], host_kernel["kernel_version"])

    def test_the_build_writes_the_agents_environment_and_runs_the_agent_check(self):
        _, phases = renders_phases()
        build = phases["build"]
        env = build.index('python3 "$HERE/host-check.py" agent-env > /etc/fcvm-fleet/renders.env')
        record = build.index('python3 "$HERE/host-check.py" record')
        check = build.index('as_user "cd $SRC && make test-fleet-renders-agent')
        self.assertLess(build.index('bg_wait fleet-ami-kitesurf'), record)
        self.assertLess(record, env)
        self.assertLess(env, check)
        line = build[build.rindex("\n", 0, check):build.index("\n", check)]
        for word in ("CASES=running", "FCVM=/usr/local/bin/fcvm", "TAG=$RENDERS_SNAPSHOT_TAG",
                     "KITESURF=$RENDERS_KITESURF", "DIST=$RENDERS_DIST",
                     "MAX_FCVM=$RENDERS_CHECK_MAX_FCVM", "MAX_ENGINES=$RENDERS_CHECK_MAX_ENGINES"):
            self.assertIn(word, line)
        self.assertLess(check, build.index('python3 "$HERE/host-check.py" host'))
        # the build's run asserts its own cases and that none failed, rather than printing them
        names = json.dumps(list(guest.AGENT_RUNNING_CASES), separators=(",", ":"))
        assertion = "jq -e '[.rows[].case] == " + names + " and .failed == []' /mnt/fcvm-btrfs/renders/build-check.agent-check.json"
        self.assertIn(assertion, build)
        self.assertLess(check, build.index(assertion))
        self.assertNotIn("jq -c '{cases, failed}'", build)

    def test_the_bake_packs_the_store_without_kitesurfs_build_only_state_and_checks_the_host(self):
        _, phases = renders_phases()
        bake = phases["bake"]
        pack = bake.index('python3 "$HERE/../host/store-pack.py"')
        call = bake[pack:bake.index("\n", bake.index("--exclude=./windows/src" if "windows" in bake else "kitesurf/bazelisk", pack))]
        for excluded in ("./vm-disks", "./state", "./cache", "./cargo-target*", "./renders",
                         "./kitesurf/downloads", "./kitesurf/npm-cache", "./kitesurf/pnpm-store",
                         "./kitesurf/cache", "./kitesurf/bazelisk"):
            self.assertIn("--exclude='" + excluded + "'" if "*" in excluded else "--exclude=" + excluded, call)
        for kept in ("./snapshots", "./kitesurf/bazel-root", "./kitesurf/src", "./image-cache"):
            self.assertNotIn("--exclude=" + kept, call)
        self.assertLess(pack, bake.index('python3 "$HERE/host-check.py" host'))
        self.assertLess(bake.index('python3 "$HERE/host-check.py" host'), bake.index("\n  root_has_room"))
        self.assertIn('STORE_S3_PREFIX=%s', bake)

    def test_the_recipe_reuses_the_gpu_hosts_store_unit_and_scripts(self):
        _, phases = renders_phases()
        for name in ("store.sh", "s3-cat.py", "store-pack.py", "fcvm-fleet-store.service"):
            self.assertFalse((RENDERS / name).exists(), name + " must be the host recipe's, not a copy")
        for phase in ("prep", "bake"):
            with self.subTest(phase=phase):
                self.assertIn('"$HERE/../host/store.sh" "$HERE/../host/s3-cat.py"', phases[phase])
                self.assertIn('"$HERE/../host/fcvm-fleet-store.service"', phases[phase])
        enable = [line for line in phases["prep"].splitlines() if line.strip().startswith("systemctl enable ")]
        self.assertEqual(len(enable), 1)
        for unit in ("fcvm-fleet-hugepages.service", "fcvm-fleet-store.service", "fcvm-fleet-renders-agent.service"):
            self.assertIn(unit, enable[0])

    def test_the_agents_unit_is_the_benchs_unit(self):
        unit = unit_settings(RENDERS / "fcvm-fleet-renders-agent.service")
        bench = (HERE.parents[1] / "bench/chromium/renders_bench.sh").read_text()
        props = re.search(r"\nunit_props\(\) \{\n(.*?)\n\}\n", bench, re.S).group(1)
        for key, value in re.findall(r"-p ([A-Za-z]+)=([^ \"\\]+)", props):
            if key == "User":
                continue  # the caller there; the build user here
            with self.subTest(key=key):
                self.assertEqual(unit.get(key), [value])
        conf = host_check().read_conf((RENDERS / "renders.conf").read_text())
        self.assertEqual(unit["User"], [conf["RENDERS_USER"]])
        # the default KillMode: with mixed, systemd kills the module's reaper with the agent
        self.assertNotIn("KillMode", unit)
        self.assertIn("XDG_RUNTIME_DIR=/run/user/" + conf["RENDERS_UID"], " ".join(unit["Environment"]))
        self.assertIn("HOME=/home/" + conf["RENDERS_USER"], " ".join(unit["Environment"]))
        self.assertEqual(unit["EnvironmentFile"], ["/etc/fcvm-fleet/agent.env", "/etc/fcvm-fleet/renders.env"])
        self.assertEqual(unit["ExecStart"],
                         ["/usr/local/bin/fcvm fleet agent $FLEET_AGENT_ARGS $RENDERS_AGENT_ARGS $RENDERS_FRAME_ARGS"])
        self.assertEqual(unit["ConditionPathExists"], ["/etc/fcvm-fleet/agent.env"])
        for key in ("Requires", "After"):
            words = " ".join(unit[key]).split()
            for name in ("cve-2026-98432-mitigation.service", "fcvm-fleet-hugepages.service", "fcvm-fleet-store.service"):
                self.assertIn(name, words, key)
        self.assertIn("user@" + conf["RENDERS_UID"] + ".service", " ".join(unit["After"]).split())

    def test_the_pool_is_reserved_early_in_boot_and_before_the_store(self):
        unit = unit_settings(RENDERS / "fcvm-fleet-hugepages.service")
        self.assertEqual(unit["DefaultDependencies"], ["no"])
        before = " ".join(unit["Before"]).split()
        self.assertIn("sysinit.target", before)
        self.assertIn("fcvm-fleet-store.service", before)
        self.assertNotIn("After", unit)
        self.assertEqual(unit["ExecStart"], ["/usr/local/lib/fcvm-fleet/hugepages.sh"])
        self.assertEqual(unit["WantedBy"], ["sysinit.target"])
        self.assertEqual(unit["Type"], ["oneshot"])
        self.assertEqual(unit["RemainAfterExit"], ["yes"])

    def run_pool(self, granted):
        """hugepages.sh against a stand-in for the kernel that grants at most `granted` pages."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "conf").write_text('RENDERS_HUGEPAGES=34000\n')
            (root / "nr").write_text("0\n")
            program = ("HUGEPAGES_LIB=1\n. " + shlex.quote(str(RENDERS / "hugepages.sh")) + "\n"
                       "pool_ask() { local n=$1; [ \"$n\" -le " + str(granted) + " ] || n=" + str(granted) + "; echo \"$n\" > \"$POOL\"; }\n"
                       "compact() { echo compacted >> " + shlex.quote(str(root / "compacted")) + "; }\n"
                       "main\n")
            result = subprocess.run(["bash", "-c", program], capture_output=True, text=True, timeout=30,
                env={"PATH": "/usr/bin:/bin", "RENDERS_CONF": str(root / "conf"), "POOL": str(root / "nr"),
                     "POOL_TRIES": "3"})
            compacted = (root / "compacted").read_text().count("compacted") if (root / "compacted").exists() else 0
            return result, compacted

    def test_the_pool_fails_its_unit_when_the_kernel_gives_fewer_pages(self):
        result, compacted = self.run_pool(34000)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(compacted, 0)
        result, compacted = self.run_pool(33999)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("33999 of 34000", result.stdout + result.stderr)
        self.assertEqual(compacted, 2, "it compacts memory and asks again before it gives up")

    def test_the_agent_can_bind_the_replays_ports_and_use_userfaultfd(self):
        # The module's replays listen on 80, 443 and 53 of loopback and its clones use
        # userfaultfd; the agent runs as the build user, as the recorded campaign's module did.
        _, phases = renders_phases()
        conf = re.search(r"cat > /etc/sysctl\.d/99-fcvm\.conf <<'X'\n(.*?)\nX\n", phases["prep"], re.S).group(1)
        keys = dict(line.split("=", 1) for line in conf.splitlines() if "=" in line and not line.startswith("#"))
        self.assertEqual(keys["net.ipv4.ip_unprivileged_port_start"], "0")
        self.assertEqual(keys["vm.unprivileged_userfaultfd"], "1")
        self.assertEqual(keys["kernel.unprivileged_userns_clone"], "1")
        self.assertEqual(keys["kernel.apparmor_restrict_unprivileged_userns"], "0")
        self.assertIn("sysctl -e -p /etc/sysctl.d/99-fcvm.conf", phases["prep"])
        self.assertIn('usermod -aG kvm "$U"', phases["prep"])


class RendersHostCheckTests(unittest.TestCase):
    PARAMS = "kvm-arm.mode=nested numa=off arm64.nv2"
    RELEASE = "7.3.3-fcvm-0123456789ab"
    LINE = "BOOT_IMAGE=/vmlinuz-7.3.3-fcvm-0123456789ab root=PARTUUID=00000000-01 ro console=tty1 console=ttyS0 " \
           "nvme_core.io_timeout=4294967295 kvm-arm.mode=nested numa=off arm64.nv2"

    BASE = "BOOT_IMAGE root ro console nvme_core.io_timeout panic"

    def setUp(self):
        self.check = host_check()

    def test_the_reference_boot_line_passes(self):
        self.assertEqual(self.check.boot_problems(self.RELEASE, self.RELEASE, self.LINE, self.PARAMS, self.BASE), [])

    def test_the_boot_line_holds_only_the_base_parameters_and_the_three(self):
        # an allowlist: anything outside the image's base parameters and RENDERS_BOOT_PARAMS fails,
        # named by its key alone (a value could be an id)
        for extra in ("kpti=0", "ssbd=force-off", "nospectre_v2", "idle=poll", "irqaffinity=0-3",
                      "kvm-arm.vgic_v4_enable=1", "arm64.nopauth", "init_on_alloc=0", "quiet"):
            with self.subTest(extra=extra):
                problems = self.check.boot_problems(self.RELEASE, self.RELEASE, self.LINE + " " + extra,
                                                    self.PARAMS, self.BASE)
                self.assertEqual(len(problems), 1, problems)
                self.assertIn(extra.split("=", 1)[0], problems[0])
                if "=" in extra:
                    self.assertNotIn(extra.split("=", 1)[1], problems[0])
        conf = self.conf()
        self.assertEqual(conf["RENDERS_BOOT_BASE_KEYS"].split(), self.BASE.split())

    def test_no_parameter_the_host_record_keeps_can_be_present_unrecorded(self):
        # bench/chromium/renders_host_record.py keeps these parameters in a run's record; on the
        # image's boot line each is one of RENDERS_BOOT_PARAMS or refused by the boot check
        spec = importlib.util.spec_from_file_location("renders_host_record",
                                                      HERE.parents[1] / "bench/chromium/renders_host_record.py")
        record = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(record)
        conf = self.conf()
        wanted = {token.split("=", 1)[0] for token in conf["RENDERS_BOOT_PARAMS"].split()}
        base = set(conf["RENDERS_BOOT_BASE_KEYS"].split())
        self.assertFalse(wanted & base)
        for key in record.KERNEL_PARAMETERS:
            with self.subTest(key=key):
                self.assertNotIn(key, base)
                if key not in wanted:
                    line = self.LINE + " " + key + "=1"
                    self.assertTrue(self.check.boot_problems(self.RELEASE, self.RELEASE, line,
                                                             conf["RENDERS_BOOT_PARAMS"], conf["RENDERS_BOOT_BASE_KEYS"]))

    def test_a_boot_line_that_differs_from_the_reference_fails(self):
        for line in (self.LINE.replace(" numa=off", ""), self.LINE.replace("numa=off", "numa=on"),
                     self.LINE + " numa=on", self.LINE.replace(" arm64.nv2", ""),
                     self.LINE.replace("kvm-arm.mode=nested", "kvm-arm.mode=nvhe"),
                     self.LINE + " hugepages=34000", self.LINE + " mitigations=off",
                     self.LINE + " transparent_hugepage=never"):
            with self.subTest(line=line[-40:]):
                self.assertTrue(self.check.boot_problems(self.RELEASE, self.RELEASE, line, self.PARAMS, self.BASE))

    BUILTIN = "kernel/drivers/net/ethernet/amazon/ena/ena.ko\n"
    AFTER = "systemd-remount-fs.service systemd-modules-load.service systemd-journald.socket"

    LOADER = {"LoadState": "loaded", "ActiveState": "active", "Result": "success"}

    def test_the_nic_driver_is_loaded_before_cloud_init_looks_for_a_nic(self):
        check = self.check.nic_load_problems
        self.assertEqual(check("ena", "# a comment\nena\n", True, self.AFTER, self.LOADER, ""), [])
        cases = (
            ({"modules_load": None}, "/etc/modules-load.d/fcvm-fleet-nic.conf"),
            ({"modules_load": "enax\n"}, "ena"),
            ({"modules_load": "# ena\n"}, "ena"),
            ({"dropin": False}, "10-fcvm-fleet-nic.conf"),
            ({"after": "systemd-remount-fs.service"}, "systemd-modules-load.service"),
            ({"after": None}, "is unknown"),
            # the loader masked, failed or not run: the files and the order are no load
            ({"loader": {**self.LOADER, "LoadState": "masked", "ActiveState": "inactive"}}, "LoadState=masked"),
            ({"loader": {**self.LOADER, "ActiveState": "failed", "Result": "exit-code"}}, "Result=exit-code"),
            ({"loader": {**self.LOADER, "ActiveState": "inactive"}}, "ActiveState=inactive"),
            ({"loader": {}}, "systemd-modules-load.service"),
            ({"loader": None}, "is unknown"),
        )
        texts = []
        for change, words in cases:
            with self.subTest(change=change):
                args = {"modules_load": "ena\n", "dropin": True, "after": self.AFTER, "loader": self.LOADER}
                args.update(change)
                problems = check("ena", args["modules_load"], args["dropin"], args["after"], args["loader"], "")
                self.assertTrue(problems, change)
                self.assertTrue(any(words in p for p in problems), problems)
                texts += problems
        # a driver built into the kernel binds during the kernel's own start
        self.assertEqual(check("ena", None, False, None, None, self.BUILTIN), [])
        # without a NIC driver to look for, nothing passes
        texts += check("", "ena\n", True, self.AFTER, self.LOADER, "")
        self.assertTrue(texts[-1])
        # every sentence reaches the validation record as itself, not as the fixed stand-in
        for text in texts:
            with self.subTest(text=text):
                self.assertTrue(guest.RENDERS_TEXT.fullmatch(text) and "//" not in text, text)

    def test_the_nic_driver_is_the_default_route_interfaces_or_none(self):
        route = ("Iface\tDestination\tGateway\tFlags\n"
                 "examplenic0\t00000000\t0102A8C0\t0003\n"
                 "examplenic1\t0000A8C0\t00000000\t0001\n")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "class/net/examplenic0").mkdir(parents=True)
            # an interface without a device/driver link has no driver
            self.assertEqual(self.check.nic_driver(route, root), "")
            (root / "bus/pci/drivers/examplenet").mkdir(parents=True)
            (root / "devices/example0").mkdir(parents=True)
            (root / "devices/example0/driver").symlink_to(root / "bus/pci/drivers/examplenet")
            (root / "class/net/examplenic0/device").symlink_to(root / "devices/example0")
            self.assertEqual(self.check.nic_driver(route, root), "examplenet")
            self.assertEqual(self.check.nic_driver(route.splitlines()[0] + "\n", root), "")
        problems = self.check.nic_load_problems("", "ena\n", True, self.AFTER, self.LOADER, "")
        self.assertTrue(problems and "no default route" in problems[0] and "no driver" in problems[0], problems)

    def test_a_systemctl_read_that_fails_is_none_never_text(self):
        calls = []
        def runner(code, out):
            def go(args):
                calls.append(args)
                return SimpleNamespace(returncode=code, stdout=out, stderr="")
            return go
        text = "LoadState=loaded\nActiveState=active\nResult=success\n"
        self.assertEqual(self.check.systemctl_show("systemd-modules-load.service", ("LoadState", "ActiveState", "Result"),
                                                   runner(0, text)), self.LOADER)
        self.assertEqual(calls[-1], ["systemctl", "show", "systemd-modules-load.service",
                                     "-p", "LoadState", "-p", "ActiveState", "-p", "Result"])
        for code in (1, 4):
            with self.subTest(code=code):
                self.assertIsNone(self.check.systemctl_show("systemd-modules-load.service", ("LoadState",),
                                                            runner(code, "LoadState=loaded\n")))

    def test_the_boot_check_holds_the_nic_order_and_not_grub(self):
        source = (RENDERS / "host-check.py").read_text()
        body = source.split("def check_boot(conf):", 1)[1].split("\ndef ", 1)[0]
        self.assertIn("nic_load_problems(", body)
        self.assertIn('systemctl_show("cloud-init-local.service", ("After",))', body)
        self.assertIn('systemctl_show("systemd-modules-load.service", ("LoadState", "ActiveState", "Result"))', body)
        self.assertNotIn('"systemctl"', body, "both reads go through systemctl_show")
        self.assertNotIn("grub", source.lower())
        self.assertNotIn("lsinitramfs", source)

    def test_a_kernel_that_is_not_the_built_one_fails(self):
        for release, expected in (("7.0.0-1009-aws", "7.0.0-1009-aws"), (self.RELEASE, "7.3.3-fcvm-ba9876543210"),
                                  (self.RELEASE, "")):
            with self.subTest(release=release, expected=expected):
                self.assertTrue(self.check.boot_problems(release, expected, self.LINE, self.PARAMS, self.BASE))

    def test_the_pool_is_exactly_the_reservation(self):
        self.assertEqual(self.check.pool_problems("34000\n", 34000), [])
        for text in ("33999\n", "68000\n", "", "lots"):
            with self.subTest(text=text):
                self.assertTrue(self.check.pool_problems(text, 34000))

    def test_kitesurfs_commit_is_renders_confs_and_a_full_commit(self):
        conf = self.conf()
        self.assertEqual(self.check.kitesurf_rev(conf), conf["RENDERS_KITESURF_REV"])
        for bad in ("", "main", "f964846e", "F" * 40):
            with self.subTest(rev=bad), self.assertRaises(ValueError):
                self.check.kitesurf_rev({**conf, "RENDERS_KITESURF_REV": bad})
        # the check reads the flavor's pin, never the other benchmark arm's pins file
        self.assertNotIn("kitesurf_pins.sh", (RENDERS / "host-check.py").read_text())

    def test_the_check_refuses_to_run_without_the_images_conf(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(self.check, "CONF", Path(tmp) / "renders.conf"):
            with self.assertRaises(SystemExit) as raised:
                self.check.main(["host-check.py", "boot"])
            self.assertIn("renders.conf", str(raised.exception))

    def test_the_conf_parser_takes_only_its_own_format(self):
        conf = self.check.read_conf('# a comment\n\nA=1\nB="two words"\n')
        self.assertEqual(conf, {"A": "1", "B": "two words"})
        for text in ("A = 1\n", "export A=1\n", "A=$(id)\n", 'A="unterminated\n', "A=1\nA=2\n", "a=1\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.check.read_conf(text)

    def conf(self):
        return self.check.read_conf((RENDERS / "renders.conf").read_text())

    def test_the_agent_arguments_are_the_modules_options_from_the_conf(self):
        conf = self.conf()
        urls = "https://example.com/,https://example.org/"
        words = self.check.agent_args(conf, RENDERS_RECORD, urls)
        self.assertEqual(words[:5], ["--renders", "--image", "unused", "--base-port", "1"])
        self.assertIn("--host-from-imds", words)
        module = [w.split("=", 1)[1] for w in words if w.startswith("--renders-arg=")]
        expected = {
            "--cpus-fcvm": conf["RENDERS_CPUS_FCVM"], "--cpus-engines": conf["RENDERS_CPUS_ENGINES"],
            "--cpus-harness": conf["RENDERS_CPUS_HARNESS"], "--max-fcvm": conf["RENDERS_MAX_FCVM"],
            "--max-engines": conf["RENDERS_MAX_ENGINES"], "--snapshot-tag": conf["RENDERS_SNAPSHOT_TAG"],
            "--kitesurf": conf["RENDERS_KITESURF"], "--kitesurf-dist": conf["RENDERS_DIST"],
            "--bench": conf["RENDERS_SRC"] + "/bench/chromium", "--corpus": conf["RENDERS_SRC"] + "/bench/chromium/corpus-live",
            "--urls": urls, "--out": conf["RENDERS_OUT"], "--run-dir": conf["RENDERS_RUN_DIR"],
            "--status-file": conf["RENDERS_OUT"] + ".status.json"}
        given = dict(option.split("=", 1) for option in module if not option.startswith("--setup="))
        self.assertEqual(given, expected)
        self.assertEqual(sorted(option for option in module if option.startswith("--setup=")),
                         ["--setup=fcvm.commit=" + SOURCE, "--setup=kitesurf.commit=" + "9" * 40])
        self.assertEqual(expected["--cpus-fcvm"], "0-39")
        self.assertEqual(expected["--cpus-engines"], "40-79")
        self.assertEqual(expected["--cpus-harness"], "80-95")
        self.assertEqual((expected["--max-fcvm"], expected["--max-engines"]), ("40", "48"))
        # systemd splits $RENDERS_AGENT_ARGS at whitespace; the benchmark's own options are refused
        # by the agent, so none is given
        self.assertTrue(all(not re.search(r"\s", w) for w in words))
        for refused in ("--levels", "--seconds", "--overlay-text", "--overlay-transform"):
            self.assertFalse(any(w.startswith("--renders-arg=" + refused) for w in words))

    def test_the_agent_environment_leaves_a_place_for_the_frame_servers_options(self):
        text = self.check.agent_env(self.conf(), RENDERS_RECORD, "https://example.com/")
        lines = [line for line in text.splitlines() if line and not line.startswith("#")]
        self.assertEqual([line.split("=", 1)[0] for line in lines], ["RENDERS_AGENT_ARGS", "RENDERS_FRAME_ARGS"])
        self.assertEqual(lines[1], "RENDERS_FRAME_ARGS=")
        self.assertTrue(lines[0].startswith('RENDERS_AGENT_ARGS="--renders ') and lines[0].endswith('"'))

    def test_the_page_list_is_the_campaigns(self):
        urls = self.check.campaign_urls(HERE.parents[1] / "bench/chromium/corpus_campaign.sh")
        bench = (HERE.parents[1] / "bench/chromium/renders_bench.sh").read_text()
        self.assertIn("corpus_campaign.sh", bench)
        self.assertTrue(urls.startswith("https://") and "," in urls and " " not in urls)

    def test_the_build_record_names_what_the_build_made(self):
        record = self.check.build_record(fcvm_commit=SOURCE, kitesurf_commit="9" * 40, index_js=b"bundle",
            release=self.RELEASE, conf=self.conf())
        self.assertEqual(record, {"fcvm_commit": SOURCE, "kitesurf_commit": "9" * 40,
            "bundle_sha256": hashlib.sha256(b"bundle").hexdigest(), "kernel_release": self.RELEASE,
            "boot_params": self.PARAMS, "hugepages": 34000})
        with self.assertRaises(ValueError):
            self.check.build_record(fcvm_commit="main", kitesurf_commit="9" * 40, index_js=b"x",
                                    release=self.RELEASE, conf=self.conf())


class RendersVerdictTests(unittest.TestCase):
    """A verdict comes from the raw result (exit status, raw problems or failed cases), never
    from the text the record keeps; the record holds one entry for each raw one."""

    PARAMS = "kvm-arm.mode=nested numa=off arm64.nv2"
    BASE = "BOOT_IMAGE root ro console nvme_core.io_timeout panic"
    RELEASE = "7.3.3-fcvm-0123456789ab"
    LINE = "BOOT_IMAGE=/vmlinuz root=LABEL=x ro console=ttyS0 kvm-arm.mode=nested numa=off arm64.nv2"

    def host_check_output(self, line):
        problems = host_check().boot_problems(self.RELEASE, self.RELEASE, line, self.PARAMS, self.BASE)
        self.assertTrue(problems)
        out = json.dumps({"ok": False, "problems": problems}) + "\n"
        return problems, SimpleNamespace(returncode=1, stdout=out, stderr="")

    def verify(self, **patches):
        base = dict(sanitized=lambda: True, guard=lambda: True, ordering=lambda kind: None,
                    launch_path_proof=lambda: True)
        with patch.multiple(guest, **{**base, **patches}):
            return guest.verify_renders()

    def test_a_wrong_boot_line_fails_the_boot_and_the_record_counts_its_problems(self):
        for line in (self.LINE.replace(" numa=off", ""), self.LINE + " mitigations=off"):
            with self.subTest(line=line[-30:]):
                problems, done = self.host_check_output(line)
                with patch.object(guest.subprocess, "run", return_value=done):
                    result = self.verify()
                self.assertIs(result["renders_host"], False)
                self.assertEqual(result["renders_host_problem_count"], len(problems))
                self.assertEqual(len(result["renders_host_problems"]), len(problems))
                self.assertTrue(any("numa" in p or "mitigations" in p for p in result["renders_host_problems"]))

    def test_a_check_that_says_ok_with_a_failing_status_or_problems_fails(self):
        for code, body in ((1, {"ok": True, "problems": []}), (0, {"ok": True, "problems": ["x=y"]}),
                           (0, {"ok": False, "problems": []}), (0, None)):
            with self.subTest(code=code, body=body):
                done = SimpleNamespace(returncode=code, stdout=json.dumps(body) + "\n" if body else "", stderr="")
                with patch.object(guest.subprocess, "run", return_value=done):
                    self.assertIs(self.verify()["renders_host"], False)

    def test_a_record_entry_the_sanitiser_drops_is_stood_for_not_lost(self):
        kept = guest.plain(["the boot line lacks numa=off", "https://example.invalid/x", "a" * 500])
        self.assertEqual(len(kept), 3)
        self.assertEqual(kept[0], "the boot line lacks numa=off")
        self.assertEqual(kept[1], guest.RENDERS_UNREADABLE)
        self.assertEqual(kept[2], guest.RENDERS_UNREADABLE)

    def agent_proof(self, code, record):
        """renders_agent_proof against a make that exits `code` and writes `record` (None: none)."""
        real = tempfile.TemporaryDirectory
        names = self.all_cases()
        with real() as directory:
            def run(argv, **kwargs):
                out = next(a.split("=", 1)[1] for a in argv if a.startswith("OUT="))
                if record is not None:
                    Path(out + ".agent-check.json").write_text(json.dumps(record))
                return SimpleNamespace(returncode=code)
            with patch.object(guest, "RENDERS_CONF", RENDERS / "renders.conf"), \
                 patch.object(guest, "agent_check_cases", lambda path: names), \
                 patch.object(guest.tempfile, "TemporaryDirectory", lambda dir=None: real(dir=directory)), \
                 patch.object(guest.os, "chown", lambda *args: None), \
                 patch.object(guest.subprocess, "run", side_effect=run):
                return guest.renders_agent_proof()

    def all_cases(self):
        return guest.agent_check_cases(HERE.parents[1] / "bench/chromium/renders_agent_check.py")

    def test_the_agent_check_passes_only_with_every_case_a_clean_exit_and_nothing_failed(self):
        names = self.all_cases()
        rows = [{"case": n, "ok": True} for n in names]
        self.assertEqual(self.agent_proof(0, {"rows": rows, "cases": len(rows), "failed": []}), (True, []))
        for code, record in ((1, {"rows": rows, "cases": len(rows), "failed": []}),
                             (0, {"rows": rows[:-1], "cases": len(rows) - 1, "failed": []}),
                             (0, None),
                             (0, {"rows": rows, "cases": len(rows), "failed": ["stop:serve#tail"]})):
            with self.subTest(code=code, record=None if record is None else record["failed"]):
                ok, failures = self.agent_proof(code, record)
                self.assertIs(ok, False)
                self.assertTrue(failures)
        # a failing case whose name the record cannot keep: still a failure, still one entry
        with patch.multiple(guest, sanitized=lambda: True, guard=lambda: True, ordering=lambda kind: None,
                            launch_path_proof=lambda: True, renders_host_proof=lambda: (True, []),
                            renders_agent_proof=lambda: (False, ["stop:serve#tail"]),
                            renders_unit_proof=lambda: (True, [])):
            result = guest.verify_renders("renders-agent", "renders-unit")
        self.assertIs(result["renders_agent"], False)
        self.assertEqual(result["renders_agent_failure_count"], 1)
        self.assertEqual(result["renders_agent_failures"], [guest.RENDERS_UNREADABLE])

    def test_every_case_is_the_checks_own_list(self):
        source = (HERE.parents[1] / "bench/chromium/renders_agent_check.py").read_text()
        running = re.findall(r'self\.run_case\("([a-z]+)"', source)
        names = self.all_cases()
        self.assertEqual(names[:len(running)], running)
        self.assertEqual(running, ["hello", "renders", "overlay", "stop"])
        self.assertIn("kill:running:TERM", names)
        self.assertIn("stop:engines-starting", names)
        # the defaults renders_bench.sh hands the check are the check's own
        bench = (HERE.parents[1] / "bench/chromium/renders_bench.sh").read_text()
        check = importlib.util.spec_from_file_location("rac", HERE.parents[1] / "bench/chromium/renders_agent_check.py")
        stages = re.search(r'agent-check\)\n\s+stages="\$\{1:-([^}]+)\}"', bench).group(1)
        hold = re.search(r'--hold-stages "\$\{HOLD_STAGES:-([^}]+)\}"', bench).group(1)
        self.assertEqual([n for n in names if n.startswith("kill:")],
                         [f"kill:{e.split(':')[0]}:{e.split(':')[1] if ':' in e else 'KILL'}" for e in stages.split(",")])
        self.assertEqual([n for n in names if n.startswith("stop:")], ["stop:" + h for h in hold.split(",")])


class RendersUnitProofTests(unittest.TestCase):
    """The cold boot starts the image's own unit words: the installed unit's [Service] settings
    and its ExecStart, with the image's renders.env and a validation agent.env that gives
    --stdio, under renders_agent_check.py's running case."""

    def test_the_command_is_the_units_own_words(self):
        unit = (RENDERS / "fcvm-fleet-renders-agent.service").read_text()
        argv = guest.unit_command(unit, "/tmp/validation-agent.env", "fcvm-fleet-renders-unit-check")
        self.assertEqual(argv[:6], ["systemd-run", "--unit", "fcvm-fleet-renders-unit-check", "--collect", "--quiet", "--pipe"])
        settings = unit_settings(RENDERS / "fcvm-fleet-renders-agent.service")
        props = [argv[i + 1] for i, a in enumerate(argv) if a == "-p"]
        for key, values in settings.items():
            if key in ("ExecStart", "Description", "Requires", "After", "Wants", "ConditionPathExists", "WantedBy"):
                continue
            for value in values:
                if value == "/etc/fcvm-fleet/agent.env":
                    value = "/tmp/validation-agent.env"
                self.assertIn(key + "=" + value, props)
        self.assertNotIn("EnvironmentFile=/etc/fcvm-fleet/agent.env", props)
        self.assertEqual(argv[-6:], settings["ExecStart"][0].split())

    def test_the_census_is_renders_bench_shs(self):
        conf = host_check().read_conf((RENDERS / "renders.conf").read_text())
        ours = guest.unit_census(conf, "fcvm-fleet-renders-unit-check")
        bench = (HERE.parents[1] / "bench/chromium/renders_bench.sh").read_text()
        fn = re.search(r"\ncensus_args\(\) \{.*?\n\}\n", bench, re.S).group(0)
        env = {"PATH": "/usr/bin:/bin", "UNIT": "fcvm-fleet-renders-unit-check",
               "STATUS": conf["RENDERS_OUT"] + ".status.json", "OUT": conf["RENDERS_OUT"],
               "RUN_DIR": conf["RENDERS_RUN_DIR"], "TAG": conf["RENDERS_SNAPSHOT_TAG"],
               "MAX_ENGINES": conf["RENDERS_MAX_ENGINES"], "FCVM_DATA_DIR": "/mnt/fcvm-btrfs"}
        done = subprocess.run(["bash", "-c", fn + "census_args"], capture_output=True, env=env, timeout=10)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(ours, done.stdout.decode().split("\0")[:-1])

    def test_the_proof_needs_the_unit_verified_and_every_running_case(self):
        names = ["hello", "renders", "overlay", "stop"]
        def run(verify_code, record):
            real = tempfile.TemporaryDirectory
            with real() as directory:
                def go(argv, **kwargs):
                    if argv[0] == "systemd-analyze":
                        return SimpleNamespace(returncode=verify_code, stdout="", stderr="")
                    out = argv[argv.index("--out") + 1]
                    if record is not None:
                        Path(out).write_text(json.dumps(record))
                    return SimpleNamespace(returncode=0 if record and not record["failed"] else 1)
                conf = RENDERS / "renders.conf"
                text = conf.read_text().replace("RENDERS_SRC=/home/ubuntu/fcvm", "RENDERS_SRC=" + str(HERE.parents[1]))
                local = Path(directory) / "renders.conf"
                local.write_text(text)
                with patch.object(guest, "RENDERS_CONF", local), \
                     patch.object(guest, "RENDERS_UNIT", RENDERS / "fcvm-fleet-renders-agent.service"), \
                     patch.object(guest.tempfile, "TemporaryDirectory", lambda dir=None: real(dir=directory)), \
                     patch.object(guest.subprocess, "run", side_effect=go):
                    return guest.renders_unit_proof()
        good = {"rows": [{"case": n, "ok": True} for n in names], "cases": 4, "failed": []}
        self.assertEqual(run(0, good), (True, []))
        for verify_code, record in ((1, good), (0, None), (0, {**good, "rows": good["rows"][:2], "cases": 2}),
                                    (0, {**good, "failed": ["renders"]})):
            with self.subTest(verify=verify_code, record=None if record is None else record["cases"]):
                ok, failures = run(verify_code, record)
                self.assertIs(ok, False)
                self.assertTrue(failures)


class RendersGuestTests(unittest.TestCase):
    def test_renders_verification_runs_the_agent_check_only_when_asked(self):
        asked = []
        with patch.multiple(guest, sanitized=lambda: True, guard=lambda: True, ordering=lambda kind: asked.append(kind),
                            launch_path_proof=lambda: True, renders_host_proof=lambda: (True, []),
                            renders_agent_proof=lambda: asked.append("agent") or (True, []),
                            renders_unit_proof=lambda: asked.append("unit") or (True, [])):
            cold = guest.verify_renders("renders-agent", "renders-unit")
            warm = guest.verify_renders()
        self.assertEqual(asked, ["renders", "agent", "unit", "renders"])
        for result in (cold, warm):
            self.assertTrue(result["guard"] and result["sanitized"] and result["launch_path"] and result["renders_host"])
            self.assertEqual(result["kernel_release"], os.uname().release)
        self.assertTrue(cold["renders_agent"] and cold["renders_unit"])
        self.assertEqual((cold["renders_agent_failure_count"], cold["renders_unit_failure_count"]), (0, 0))
        self.assertFalse(warm["renders_agent"] or warm["renders_unit"])
        with self.assertRaisesRegex(RuntimeError, "unknown validator proof"):
            guest.verify_renders("viewer-path")

    def test_renders_failures_reach_the_record_as_short_plain_strings(self):
        with patch.multiple(guest, sanitized=lambda: True, guard=lambda: True, ordering=lambda kind: None,
                            launch_path_proof=lambda: True,
                            renders_host_proof=lambda: (False, ["the pool holds 0 of 34000 2 MiB pages", "x" * 500]),
                            renders_agent_proof=lambda: (False, ["kill:running", "https://example.invalid/?token=secret"]),
                            renders_unit_proof=lambda: (True, [])):
            result = guest.verify_renders("renders-agent", "renders-unit")
        self.assertFalse(result["renders_host"])
        self.assertEqual(result["renders_host_problems"], ["the pool holds 0 of 34000 2 MiB pages", guest.RENDERS_UNREADABLE])
        self.assertFalse(result["renders_agent"])
        self.assertEqual(result["renders_agent_failures"], ["kill:running", guest.RENDERS_UNREADABLE])

    def test_the_agent_check_runs_as_the_build_user_through_make(self):
        source = (HERE / "pipeline_guest.py").read_text()
        body = source.split("def renders_agent_proof():", 1)[1].split("\ndef ", 1)[0]
        self.assertIn('"runuser", "-u", conf["RENDERS_USER"]', body)
        self.assertIn('"make", "-C", conf["RENDERS_SRC"], "test-fleet-renders-agent"', body)
        self.assertIn('"FCVM=/usr/local/bin/fcvm"', body)
        self.assertNotIn("CASES=", body, "the validation runs every case")

    def test_renders_ordering_waits_for_the_pool_and_the_store(self):
        reads = []
        def command(args, timeout=120):
            if "--property=ActiveState" in args:
                reads.append(args)
                return "\n".join("active" for _ in args[2:-2])
            if "--property=ActiveEnterTimestampMonotonic" in args:
                return "10"
            if args[:2] == ["systemctl", "is-active"]:
                return "active"
            return "20"
        with patch.object(guest, "command", side_effect=command):
            guest.ordering("renders")
        self.assertIn("fcvm-fleet-hugepages.service", reads[0])
        self.assertIn("fcvm-fleet-store.service", reads[0])
        self.assertNotIn("fcvm-fleet-renders-agent.service", reads[0], "a validator runs no agent")

    def test_renders_diagnostics_and_seal(self):
        prefix = "builders/arn:aws:ec2:us-west-2:111111111111:instance/i-" + "a" * 17 + "/"
        for phase in ("prep", "build", "bake"):
            with self.subTest(phase=phase), patch.object(guest, "bucket_client") as client, \
                    patch.object(guest.os, "open", side_effect=OSError("no log in this test")):
                with self.assertRaises(OSError):
                    guest.export_diagnostics("example-images", prefix, "us-west-2", "renders", phase)
                client.assert_called_once()
        with patch.object(guest, "bucket_client") as client, self.assertRaises(RuntimeError):
            guest.export_diagnostics("example-images", prefix, "us-west-2", "renders", "image")
        client.assert_not_called()
        steps = []
        with patch.multiple(guest, clean_identity=lambda: steps.append("clean"), sanitized=lambda: steps.append("sanitized"),
                            guard=lambda: steps.append("guard"), store_parts=lambda: steps.append("store"),
                            command=lambda args, timeout=120: steps.append(args[0])), \
                patch.object(guest.Path, "write_text", lambda self, text: steps.append("machine-id")):
            guest.seal("renders")
        self.assertEqual(steps[:4], ["clean", "sanitized", "guard", "store"])

    def test_the_build_record_is_read_and_checked_on_the_builder(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "renders-build.json"
            path.write_text(json.dumps(RENDERS_RECORD))
            with patch.object(guest, "RENDERS_RECORD", path), patch.object(guest, "RENDERS_CONF", RENDERS / "renders.conf"):
                self.assertEqual(guest.renders_record(), RENDERS_RECORD)
                path.write_text(json.dumps({**RENDERS_RECORD, "extra": "field"}))
                with self.assertRaises(RuntimeError):
                    guest.renders_record()


if __name__ == "__main__":
    unittest.main(verbosity=2)
