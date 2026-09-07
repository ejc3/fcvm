#!/usr/bin/env python3
"""Exercise the real AMI build orchestration with AWS/build work fully mocked.

Run with make test-ci-security. No AWS credentials or Rust build
are needed. The fixture fails closed on every unexpected AWS operation.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


MOCK_AWS = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["AMI_TEST_CALLS"], "a") as log:
    log.write(json.dumps(args) + "\n")
operation = args[1]
if args[0] != "ec2":
    raise SystemExit("unexpected service: " + str(args))
if operation == "describe-instances":
    print("")
elif operation == "describe-subnets":
    print(os.environ.get("AMI_TEST_SUBNETS", "subnet-1234567890abcdef0"))
elif operation == "describe-security-groups":
    print("sg-1234567890abcdef0")
elif operation == "run-instances":
    if "--instance-market-options" in args and os.environ.get("AMI_TEST_SPOT_FAIL"):
        raise SystemExit("Error: no Spot capacity")
    print("i-1234567890abcdef0")
elif operation == "describe-tags":
    print(os.environ.get("AMI_TEST_BUILD_STATUS", "7.0.14-fcvm-test"))
elif operation == "create-image":
    print("ami-1234567890abcdef0")
elif operation == "describe-images":
    print("available")
elif operation in {"create-tags", "terminate-instances"}:
    print("{}")
else:
    raise SystemExit("unexpected AWS operation: " + str(args))
'''


class AmiSecurityTest(unittest.TestCase):
    def test_progress_uses_status_tags_not_unscoped_ssm_output(self):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("wait_for_build() {"):].split("\n}", 1)[0] + "\n}"
        for status in ['building', 'failed', 'complete']:
            with self.subTest(status=status), tempfile.TemporaryDirectory(prefix='ami-progress-test-') as temporary:
                directory = Path(temporary)
                aws = directory / 'aws'
                aws.write_text(MOCK_AWS)
                aws.chmod(0o755)
                calls = directory / 'calls.jsonl'
                harness = 'set -euo pipefail\nREGION=us-west-1\nsleep() { :; }\n' + function
                result = subprocess.run(
                    ['bash', '-c', harness + '\nwait_for_build i-1234567890abcdef0 2'],
                    text=True, capture_output=True, timeout=10,
                    env={**os.environ, 'PATH': f"{directory}:{os.environ['PATH']}",
                         'AMI_TEST_CALLS': str(calls), 'AMI_TEST_BUILD_STATUS': status},
                )
                recorded = [json.loads(line) for line in calls.read_text().splitlines()]
                self.assertTrue(all(call[:2] == ['ec2', 'describe-tags'] for call in recorded), recorded)
                self.assertEqual(result.returncode, 0 if status == 'complete' else 1, result.stdout + result.stderr)

    def test_cache_never_returns_pending_or_failed_images(self):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("check_existing_ami() {"):].split("\n}", 1)[0] + "\n}"
        result = subprocess.run(
            ["bash", "-c", 'set -euo pipefail\nREGION=us-west-1\naws() { printf "%s\\n" "$@"; }\n' + function + '\ncheck_existing_ami test-build-hash'],
            text=True, capture_output=True, check=True,
        )
        self.assertIn("Name=state,Values=available", result.stdout.splitlines())

    def run_build(self, **settings):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        self.assertTrue(script.rstrip().endswith('main "$@"'))
        script = script.rstrip().removesuffix('main "$@"')
        with tempfile.TemporaryDirectory(prefix="ami-security-test-") as temporary:
            directory = Path(temporary)
            aws = directory / "aws"
            aws.write_text(MOCK_AWS)
            aws.chmod(0o755)
            calls = directory / "calls.jsonl"
            harness = directory / "harness.sh"
            harness.write_text(script + r'''
git() { printf '%040d\n' 0; }
compute_pinned_hash() { echo test-build-hash; }
check_existing_ami() { echo None; }
verify_source_commit_fetchable() { return 0; }
get_base_ami() { echo ami-00000000000000000; }
create_user_data() { echo '#!/bin/bash'; }
wait_for_build() { return 0; }
main
''')
            result = subprocess.run(
                ["bash", str(harness)], text=True, capture_output=True, timeout=10,
                env={**os.environ, "PATH": f"{directory}:{os.environ['PATH']}",
                     "AWS_EC2_METADATA_DISABLED": "true", "AMI_TEST_CALLS": str(calls),
                     "TMPDIR": str(directory), "GITHUB_OUTPUT": str(directory / "outputs"),
                     **settings},
            )
            recorded = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            return result, recorded

    def assert_secure_launch(self, args):
        def value(flag):
            self.assertIn(flag, args)
            return args[args.index(flag) + 1]

        self.assertEqual(value("--iam-instance-profile"), "Name=ami-builder-profile")
        self.assertEqual(value("--subnet-id"), "subnet-1234567890abcdef0")
        self.assertEqual(value("--security-group-ids"), "sg-1234567890abcdef0")
        self.assertIn("HttpTokens=required", value("--metadata-options"))
        self.assertIn("HttpPutResponseHopLimit=1", value("--metadata-options"))
        for mapping in json.loads(value("--block-device-mappings")):
            self.assertTrue(mapping["Ebs"]["Encrypted"])
        tags = json.loads(value("--tag-specifications"))
        self.assertEqual({item["ResourceType"] for item in tags}, {"instance", "volume", "network-interface"})
        for item in tags:
            self.assertIn({"Key": "Name", "Value": "ami-builder-temp"}, item["Tags"])

    def test_spot_launch_uses_only_builder_authority(self):
        result, calls = self.run_build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        launches = [call for call in calls if call[1] == "run-instances"]
        self.assertEqual(len(launches), 1)
        self.assert_secure_launch(launches[0])

    def test_on_demand_fallback_keeps_same_boundary(self):
        result, calls = self.run_build(AMI_TEST_SPOT_FAIL="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        launches = [call for call in calls if call[1] == "run-instances"]
        self.assertEqual(len(launches), 2)
        for launch in launches:
            self.assert_secure_launch(launch)

    def test_image_and_snapshots_are_scoped_at_creation(self):
        result, calls = self.run_build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        create = next(call for call in calls if call[1] == "create-image")
        self.assertIn("--tag-specifications", create)
        tags = json.loads(create[create.index("--tag-specifications") + 1])
        self.assertEqual({item["ResourceType"] for item in tags}, {"image", "snapshot"})
        for item in tags:
            self.assertIn({"Key": "Purpose", "Value": "github-runner"}, item["Tags"])
        self.assertFalse(any(call[1] == "create-tags" for call in calls))

    def test_ambiguous_or_missing_network_fails_before_launch(self):
        for subnets in ["None", "", "subnet-1234567890abcdef0\tsubnet-2234567890abcdef0"]:
            with self.subTest(subnets=subnets):
                result, calls = self.run_build(AMI_TEST_SUBNETS=subnets)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(call[1] == "run-instances" for call in calls))


if __name__ == "__main__":
    unittest.main(verbosity=2)
