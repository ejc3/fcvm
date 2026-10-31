#!/usr/bin/env python3
"""Offline image-recipe checks; all package, service and guard actions are fakes."""
import ast
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
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


class PublisherTests(unittest.TestCase):
    def image_branch(self, success):
        """Run the actual image branch with an AWS executable that cannot call AWS."""
        with tempfile.TemporaryDirectory(prefix="fleet-publish-test-") as temporary:
            directory = Path(temporary)
            log = directory / "calls.jsonl"
            binary = directory / "bin"
            binary.mkdir()
            fake = binary / "aws"
            fake.write_text('''#!/usr/bin/python3
import json,os,sys
args=sys.argv[1:]
if args[:1]==['--region']: args=args[2:]
with open(os.environ['PUBLISH_TEST_LOG'],'a') as output:output.write(json.dumps(args)+'\\n')
operation=tuple(args[:2])
if operation==('sts','get-caller-identity'): print('111122223333')
elif operation==('ssm','send-command'): print('cmd-test')
elif operation==('ssm','get-command-invocation'):
 query=args[args.index('--query')+1]
 print(os.environ['PUBLISH_TEST_STATUS'] if query=='Status' else 'guard publication proof')
elif operation==('ec2','create-image'): print('ami-0123456789abcdef0')
elif operation==('ec2','describe-images'): print('available')
else: sys.exit(91)
''')
            fake.chmod(0o755)
            result = subprocess.run(["bash", str(AMI / "build-ami.sh"), "image", "i-0123456789abcdef0", "example-guarded-image"],
                                    capture_output=True, text=True, timeout=15,
                                    env={**os.environ, "PATH": str(binary) + ":/usr/bin:/bin", "PUBLISH_TEST_LOG": str(log),
                                         "PUBLISH_TEST_STATUS": "Success" if success else "Failed"})
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            return result, calls

    def test_failed_remote_guard_never_creates_an_image(self):
        result, calls = self.image_branch(False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(any(args[:2] == ["ssm", "send-command"] for args in calls))
        self.assertFalse(any(args[:2] == ["ec2", "create-image"] for args in calls))

    def test_successful_remote_guard_tags_image_and_snapshot_with_same_hash(self):
        result, calls = self.image_branch(True)
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = next(index for index, args in enumerate(calls) if args[:2] == ["ssm", "send-command"])
        create = next(index for index, args in enumerate(calls) if args[:2] == ["ec2", "create-image"])
        self.assertLess(proof, create)
        command = json.loads(calls[proof][calls[proof].index("--parameters") + 1])["commands"][0]
        for kind, names in (("host", ("fcvm-fleet-agent.service", "fcvm-fleet-store.service")),
                            ("tile", ("fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"))):
            for name in names:
                self.assertIn(name + "=" + hashlib.sha256((AMI / kind / name).read_bytes()).hexdigest(), command)
        tags = calls[create][calls[create].index("--tag-specifications") + 1:]
        specifications = [value for value in tags if value.startswith("ResourceType=")]
        self.assertEqual(len(specifications), 2)
        for kind, value in zip(("image", "snapshot"), specifications):
            self.assertTrue(value.startswith("ResourceType=" + kind + ","))
            self.assertIn("{Key=GuardHelperSHA256,Value=" + HELPER_SHA + "}", value)

    def remote_proof(self, *, family="host", corrupt=False, healthy=True, enabled=True, missing=None, nft_change=None, unit_change=None, consumer_change=None):
        """Execute the actual transmitted verifier; every subprocess and helper path is fake."""
        source = (AMI / "build-ami.sh").read_text()
        match = re.search(r"(?ms)^# BEGIN IMAGE GUARD PROOF\n(.*?)^# END IMAGE GUARD PROOF$", source)
        self.assertIsNotNone(match, "publisher must include its remote guard verifier")
        tree = ast.parse(match[1])
        tree.body = [node for node in tree.body if not isinstance(node, (ast.Import, ast.ImportFrom))]
        ast.fix_missing_locations(tree)
        calls = []
        with tempfile.TemporaryDirectory() as temporary:
            helper = Path(temporary) / "installed-helper"
            helper.write_bytes((AMI / "common/cve-2026-98432-mitigation.py").read_bytes() + (b"\n" if corrupt else b""))
            unit = Path(temporary) / GUARD
            definition = next(node.value for node in ast.parse(helper.read_text()).body if isinstance(node, ast.Assign)
                              and any(isinstance(target, ast.Name) and target.id == "UNIT" for target in node.targets))
            unit_text = ast.literal_eval(definition)
            if unit_change == "simple": unit_text = unit_text.replace("Type=oneshot", "Type=simple")
            if unit_change == "condition": unit_text = unit_text.replace("[Unit]\n", "[Unit]\nConditionPathExists=/never\n")
            if unit_change == "post-command": unit_text = unit_text.replace("[Service]\n", "[Service]\nExecStartPost=/bin/true\n")
            unit.write_text(unit_text)
            consumers = {name: AMI / kind / name for kind, names in (
                ("host", ("fcvm-fleet-agent.service", "fcvm-fleet-store.service")),
                ("tile", ("fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"))) for name in names}
            files = {"/etc/systemd/system/" + GUARD: unit}
            for name, source in consumers.items():
                fragment = Path(temporary) / name
                fragment.write_bytes(source.read_bytes())
                files["/etc/systemd/system/" + name] = fragment
            dropins = {}
            if consumer_change:
                name, change = consumer_change
                if change == "fragment": files["/etc/systemd/system/" + name].write_text("[Service]\nExecStart=/bin/true\n")
                if change in ("command-dropin", "guard-only", "unknown-dropin"):
                    filename = "override.conf" if change == "unknown-dropin" else "10-ip-options-guard.conf"
                    canonical = "/etc/systemd/system/" + name + ".d/" + filename
                    dropin = Path(temporary) / (name + ".conf")
                    dropin.write_text("[Service]\nExecStartPre=/usr/sbin/nft delete table inet cve_2026_98432\n" if change == "command-dropin" else "[Unit]\nRequires=" + GUARD + "\nAfter=" + GUARD + "\n")
                    files[canonical] = dropin
                    dropins[name] = canonical
            loaded = ({"fcvm-fleet-agent.service", "fcvm-fleet-store.service"} if family == "host" else
                      {"fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"} if family == "tile" else
                      {"fcvm-fleet-agent.service"})

            def output(args, **kwargs):
                calls.append(args)
                if args == [str(helper), "--check"]:
                    return json.dumps({"status": "PASS" if healthy else "FAIL", "inet_prerouting": True,
                                       "inet_output": True, "bridge_prerouting": True})
                if args == ["systemctl", "is-enabled", GUARD]:
                    return "enabled\n" if enabled else "disabled\n"
                if args[:3] == ["systemctl", "show", GUARD] and "--property=ExecStart" in args:
                    command = f"{{ path={helper} ; argv[]={helper} --load ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"
                    settings = {"ExecStart": command, "DropInPaths": "", "FragmentPath": "/etc/systemd/system/" + GUARD,
                                "Type": "oneshot", "RemainAfterExit": "yes", "DefaultDependencies": "no", "NeedDaemonReload": "no"}
                    if unit_change == "noop": settings["ExecStart"] = "{ path=/bin/true ; argv[]=/bin/true ; ignore_errors=no ; pid=0 }"
                    if unit_change == "dropin": settings["DropInPaths"] = "/etc/systemd/system/" + GUARD + ".d/override.conf"
                    if unit_change == "fragment": settings["FragmentPath"] = "/run/systemd/system/" + GUARD
                    if unit_change == "simple": settings["Type"] = "simple"
                    if unit_change == "ignore-error": settings["ExecStart"] = command.replace("ignore_errors=no", "ignore_errors=yes")
                    if unit_change == "extra-command": settings["ExecStart"] = command + " ; { path=/bin/true ; argv[]=/bin/true ; ignore_errors=no ; pid=0 }"
                    if unit_change == "pending-reload": settings["NeedDaemonReload"] = "yes"
                    return "\n".join(key + "=" + value for key, value in settings.items())
                if args[:4] == ["nft", "-j", "list", "table"]:
                    # Actual nft 1.0.9 JSON: IPv4 payload access makes the explicit
                    # nfproto/ether-type clauses redundant, so nft omits those.
                    nft_family = args[4]
                    hooks = ("prerouting", "output") if nft_family == "inet" else ("prerouting",)
                    left = {"meta": {"key": "l4proto"}} if nft_family == "inet" else {"payload": {"protocol": "ip", "field": "protocol"}}
                    canonical = [{"match": {"op": "==", "left": left, "right": "tcp"}},
                                 {"match": {"op": ">", "left": {"payload": {"protocol": "ip", "field": "hdrlength"}}, "right": 5}},
                                 {"counter": {"packets": 7, "bytes": 308}}, {"drop": None}]
                    entries = [{"table": {"family": nft_family, "name": "cve_2026_98432", "handle": 20}}]
                    for hook in hooks:
                        change = nft_change[2] if nft_change and nft_change[:2] == (nft_family, hook) else None
                        expressions = copy.deepcopy(canonical)
                        if change == "udp": expressions[0]["match"]["right"] = "udp"
                        if change == "wrong-ihl": expressions[1]["match"]["right"] = 6
                        if change == "restrictive-port": expressions.insert(2, {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": 443}})
                        if change == "all-tcp": expressions.pop(1)
                        if change == "no-ipv4": expressions[1]["match"]["left"]["payload"]["protocol"] = "ip6"
                        entries.append({"chain": {"family": nft_family, "table": "cve_2026_98432", "name": hook,
                            "type": "filter", "hook": hook, "prio": -300 if change == "priority" else -310,
                            "policy": "drop" if change == "policy" else "accept"}})
                        entries.append({"rule": {"family": nft_family, "table": "cve_2026_98432", "chain": hook, "expr": expressions}})
                        if change == "extra-rule": entries.append(copy.deepcopy(entries[-1]))
                    return json.dumps({"nftables": entries})
                if args[:2] == ["systemctl", "show"] and "--property=LoadState" in args:
                    return "loaded\n" if args[2] in loaded else "not-found\n"
                if args[:2] == ["systemctl", "show"]:
                    name = args[2]
                    settings = {key: "unrelated.service" if key == missing else GUARD for key in ("Requires", "After")}
                    settings.update({"FragmentPath": "/etc/systemd/system/" + name,
                                     "DropInPaths": dropins.get(name, ""), "NeedDaemonReload": "yes" if consumer_change == (name, "pending-reload") else "no"})
                    return "\n".join(key + "=" + value for key, value in settings.items() if "--property=" + key in args)
                raise AssertionError("unexpected verifier command")

            def path(value):
                self.assertTrue(value == "/usr/local/sbin/cve-2026-98432-mitigation" or value in files)
                return helper if value.startswith("/usr/local/sbin/") else files[value]

            namespace = {"ast": ast, "hashlib": hashlib, "json": json, "re": re, "Path": path,
                         "sys": SimpleNamespace(argv=["-", HELPER_SHA] + [name + "=" + hashlib.sha256(source.read_bytes()).hexdigest() for name, source in consumers.items()]),
                         "subprocess": SimpleNamespace(check_output=output, DEVNULL=-3)}
            exec(compile(tree, "remote-image-guard-proof", "exec"), namespace)
        return calls

    def test_remote_verifier_accepts_only_complete_guarded_host_or_tile(self):
        for family in ("host", "tile"):
            with self.subTest(family=family):
                self.assertTrue(self.remote_proof(family=family))
        with self.assertRaises(SystemExit):
            self.remote_proof(family="partial")

    def test_remote_verifier_rejects_wrong_hash_failed_check_or_disabled_unit(self):
        for options in ({"corrupt": True}, {"healthy": False}, {"enabled": False}):
            with self.subTest(options=options), self.assertRaises(SystemExit):
                self.remote_proof(**options)

    def test_remote_verifier_requires_both_dependency_kinds(self):
        for key in ("Requires", "After"):
            with self.subTest(key=key), self.assertRaises(SystemExit):
                self.remote_proof(missing=key)

    def test_remote_verifier_rejects_ineffective_or_overbroad_packet_guards(self):
        for family, hook in (("inet", "prerouting"), ("inet", "output"), ("bridge", "prerouting")):
            for change in ("udp", "wrong-ihl", "restrictive-port", "all-tcp", "no-ipv4", "priority", "policy", "extra-rule"):
                with self.subTest(family=family, hook=hook, change=change), self.assertRaises(SystemExit):
                    self.remote_proof(nft_change=(family, hook, change))

    def test_remote_verifier_requires_canonical_effective_boot_guard(self):
        for change in ("noop", "dropin", "fragment", "simple", "ignore-error", "extra-command", "condition", "post-command", "pending-reload"):
            with self.subTest(change=change), self.assertRaises(SystemExit):
                self.remote_proof(unit_change=change)

    def test_remote_verifier_rejects_consumer_commands_or_untrusted_configuration(self):
        for family, names in (("host", ("fcvm-fleet-agent.service", "fcvm-fleet-store.service")),
                              ("tile", ("fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"))):
            for name in names:
                for change in ("command-dropin", "unknown-dropin", "fragment", "pending-reload"):
                    with self.subTest(unit=name, change=change), self.assertRaises(SystemExit):
                        self.remote_proof(family=family, consumer_change=(name, change))

    def test_remote_verifier_accepts_only_the_exact_known_guard_dependency_dropin(self):
        for family, names in (("host", ("fcvm-fleet-agent.service", "fcvm-fleet-store.service")),
                              ("tile", ("fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"))):
            for name in names:
                with self.subTest(unit=name):
                    self.assertTrue(self.remote_proof(family=family, consumer_change=(name, "guard-only")))


if __name__ == "__main__":
    unittest.main(verbosity=2)
