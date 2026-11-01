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
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import tarfile
import threading
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
                image = "ami-" + ("a" if kind == "host" else "b") * 17
                self.candidates[kind] = image
                self.slots[slot]["image_state"] = "available"
                reply = {"image_id": image, "state": "available"}
            elif action == "cleanup":
                for value in self.slots.values():
                    value["state"] = "terminated"
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
        self.bad_renderer = False
        self.stale_boots = False
        self.invalidated = 0
        self.tls = {}
        self.artifact_fault = None

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
        if key.endswith("manifest.json"):
            assert all(value["state"] == "terminated" for value in self.broker.slots.values())
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
                    value = {"boot_id": identity, "guard": True, "frames": True, "tls_rotation": True,
                        "sanitized": True, "phone_clone": slot.startswith("host-")}
                    value.update(self.tls)
                    if self.bad_proof:
                        value[self.bad_proof] = False
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
        self.assertEqual(sum(call["action"] == "launch" for call in self.broker.calls), 1)
        self.no_manifest()

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
        for proof in ("guard", "frames", "tls_rotation", "sanitized", "phone_clone"):
            with self.subTest(proof=proof):
                self.setUp()
                self.worker.bad_proof = proof
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.no_manifest()
                self.assertTrue(all(value["state"] == "terminated" for value in self.broker.slots.values()))

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
        for name, value in (("source", "f" * 40), ("deadline", 14501), ("deadline", 100), ("input_sha", "bad")):
            with self.subTest(name=name, value=value):
                self.setUp()
                setattr(self.broker, name, value)
                with self.assertRaises(RuntimeError):
                    self.run_driver()
                self.assertFalse(any(call["action"] == "launch" for call in self.broker.calls))
                self.assertFalse(self.worker.regions)
                self.assertFalse(self.worker.objects)
                self.no_manifest()

    def test_total_lease_accepts_240_minutes_without_extending_individual_commands(self):
        for seconds in (150 * 60 + 1, 240 * 60):
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
        self.assertLessEqual(sum(call["action"] == "launch" for call in self.broker.calls), 1)
        self.no_manifest()

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
            ordering=lambda kind: None, renderer=lambda: {"image_id": IMAGE},
            secret=lambda arn, region: document, cert_hash=lambda cert: cert,
            host_proof=lambda *args: self.fail("unsafe identity reached the GPU proof")):
            with self.assertRaisesRegex(RuntimeError, "not distinct"):
                guest.verify("host", IMAGE, "a", "b", "region")

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
