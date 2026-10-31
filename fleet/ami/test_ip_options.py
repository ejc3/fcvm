#!/usr/bin/env python3
"""Offline image-recipe checks; all package, service and guard actions are fakes."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

AMI = Path(__file__).resolve().parent
ROOT = AMI.parent.parent
GUARD = "cve-2026-98432-mitigation.service"
HELPER_SHA = "3676cd1defac9288c6ca650ece15f81f8e32bc8781b9f78a8b3e7e83fa9f5b18"
INSTALL_CALL = 'bash "$HERE/../common/install-ip-options.sh"'


class GuardContractTests(unittest.TestCase):
    def test_public_units_require_successful_guard_before_start(self):
        for relative in ("host/fcvm-fleet-agent.service", "host/fcvm-fleet-store.service",
                         "tile/fcvm-fleet-renderer.service", "tile/fcvm-fleet-nginx.service"):
            with self.subTest(unit=relative):
                unit = (AMI / relative).read_text().split("[Unit]\n", 1)[1].split("[Service]", 1)[0]
                for key in ("Requires", "After"):
                    words = [word for line in unit.splitlines() if line.startswith(key + "=")
                             for word in line.split("=", 1)[1].split()]
                    self.assertIn(GUARD, words, key + " must name the guard in executable unit settings")

    def test_both_recipes_apply_guard_before_enabling_units(self):
        host = (AMI / "host/build.sh").read_text()
        prep = host.split("\nprep)\n", 1)[1].split("\nbuild)\n", 1)[0]
        self.assertIn(INSTALL_CALL, prep)
        self.assertLess(prep.index(INSTALL_CALL), prep.index("systemctl enable"))
        bake = host.split("\nbake)\n", 1)[1]
        self.assertIn(INSTALL_CALL, bake, "BASE_AMI revisions must install the guard too")
        body = re.search(r"(?ms)^install_units\(\) \{\n(.*?)^\}", (AMI / "tile/build.sh").read_text())[1]
        self.assertIn(INSTALL_CALL, body)
        self.assertLess(body.index(INSTALL_CALL), body.index("systemctl enable"))

    def test_bundled_helper_is_the_reviewed_implementation(self):
        self.assertEqual(hashlib.sha256((AMI / "common/cve-2026-98432-mitigation.py").read_bytes()).hexdigest(), HELPER_SHA)

    def test_regressions_run_in_the_existing_fleet_ci_job(self):
        job = (ROOT / ".github/workflows/ci.yml").read_text().split("\n  fleet-renderer:\n", 1)[1]
        self.assertIn("run: python3 -B fleet/ami/test_ip_options.py", job)


class InstallerTests(unittest.TestCase):
    def exercise(self, *, corrupt=False, fail=False, tile=False):
        with tempfile.TemporaryDirectory(prefix="fleet guard test ") as temporary:
            directory = Path(temporary)
            common, binary, tile_dir = directory / "common", directory / "bin", directory / "tile"
            for path in (common, binary, tile_dir):
                path.mkdir()
            for source in (AMI / "common").iterdir():
                if source.is_file():
                    shutil.copyfile(source, common / source.name)
            if corrupt:
                with (common / "cve-2026-98432-mitigation.py").open("ab") as output:
                    output.write(b"\n")
            log = directory / "calls"
            # These commands cannot reach root paths, package managers, nft or systemd.
            # sha256sum and dirname are the only real external tools used by the installer.
            for name in ("nft", "apt-get", "python3", "install", "gcc", "rm", "ln", "systemctl"):
                content = '#!/bin/bash\nprintf "%s:%s\\n" "${0##*/}" "$*" >> "$GUARD_TEST_LOG"\n'
                if name == "python3":
                    content += '[ "${GUARD_TEST_APPLY_FAIL:-0}" = 0 ] || exit 31\n'
                path = binary / name
                path.write_text(content)
                path.chmod(0o755)
            if tile:
                source = (AMI / "tile/build.sh").read_text()
                definition = re.search(r"(?ms)^install_units\(\) \{\n.*?^\}", source)[0]
                # All mutating commands of this actual function are the PATH fakes above.
                program = 'set -euo pipefail\nHERE="$GUARD_TEST_TILE"\n' + definition + '\ninstall_units\n'
                command = ["bash", "-c", program]
            else:
                command = ["bash", str(common / "install-ip-options.sh")]
            result = subprocess.run(command, capture_output=True, text=True, timeout=15,
                                    env={**os.environ, "PATH": f"{binary}:/usr/bin:/bin",
                                         "GUARD_TEST_LOG": str(log), "GUARD_TEST_TILE": str(tile_dir),
                                         "GUARD_TEST_APPLY_FAIL": "1" if fail else "0"})
            return result, log.read_text().splitlines() if log.exists() else []

    def test_changed_helper_fails_before_any_privileged_action(self):
        result, calls = self.exercise(corrupt=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])

    def test_reviewed_helper_is_applied_without_reinstalling_existing_tools(self):
        result, calls = self.exercise()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertTrue(calls[0].startswith("python3:"))
        self.assertTrue(calls[0].endswith("/cve-2026-98432-mitigation.py --apply"))

    def test_guard_failure_prevents_tile_unit_enable(self):
        result, calls = self.exercise(fail=True, tile=True)
        self.assertEqual(result.returncode, 31)
        self.assertEqual(len(calls), 1, calls)
        self.assertTrue(calls[0].startswith("python3:"))

    def test_tile_enable_follows_successful_guard(self):
        result, calls = self.exercise(tile=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(calls[0].startswith("python3:"), calls)
        self.assertTrue(any(line.startswith("systemctl:enable ") for line in calls))


if __name__ == "__main__":
    unittest.main(verbosity=2)
