#!/usr/bin/env python3
"""Tests for scripts/vmm-artifacts.py: the manifests it reads and the config it writes."""
import hashlib
import importlib.util
import json
import pathlib
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("vmm_artifacts", ROOT / "scripts" / "vmm-artifacts.py")
vmm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vmm)


def entry(name, sha256, commit="c" * 40, arch="x86_64", libc="libc-2.39"):
    return {"name": name, "commit": commit, "arch": arch, "libc": libc, "sha256": sha256}


class Record(unittest.TestCase):
    config = (ROOT / "rootfs-config.toml").read_text()

    def test_no_entries_leaves_the_config_as_it_is(self):
        self.assertEqual(vmm.record(self.config, {}), self.config)

    def test_entries_parse_back_and_rewriting_them_changes_nothing(self):
        entries = {"b.bin": entry("b.bin", "a" * 64), "a.bin": entry("a.bin", "b" * 64, arch="aarch64")}
        once = vmm.record(self.config, entries)
        self.assertEqual(vmm.record(once, entries), once)
        parsed = tomllib.loads(once)["vmm_artifacts"]
        self.assertEqual(parsed["repo"], "ejc3/fcvm")
        self.assertEqual(parsed["files"], [entries["a.bin"], entries["b.bin"]])
        # everything outside the table is as it was
        self.assertEqual(vmm.record(once, {}), self.config)
        self.assertEqual({k: v for k, v in tomllib.loads(once).items() if k != "vmm_artifacts"},
                         {k: v for k, v in tomllib.loads(self.config).items() if k != "vmm_artifacts"})


class Manifests(unittest.TestCase):
    def write(self, root, name, content, sha256=None):
        root.mkdir(parents=True, exist_ok=True)
        (root / name).write_bytes(content)
        sha256 = sha256 or hashlib.sha256(content).hexdigest()
        (root / "manifest.json").write_text(json.dumps([entry(name, sha256)]))
        return sha256

    def test_reads_every_leg_and_refuses_a_file_that_is_not_its_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            self.write(tmp / "leg-a", "firecracker-default-0123456789ab.bin", b"a")
            self.write(tmp / "leg-b", "cloud-hypervisor-0123456789ab.bin", b"b")
            self.assertEqual(sorted(vmm.manifests(tmp)), ["cloud-hypervisor-0123456789ab.bin", "firecracker-default-0123456789ab.bin"])
            self.write(tmp / "leg-c", "firecracker-nested-0123456789ab.bin", b"c", sha256="0" * 64)
            with self.assertRaises(SystemExit) as refused:
                vmm.manifests(tmp)
            self.assertIn("firecracker-nested-0123456789ab.bin", str(refused.exception))

    def test_refuses_an_empty_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                vmm.manifests(tmp)


if __name__ == "__main__":
    unittest.main()
