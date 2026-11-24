#!/usr/bin/env python3
"""Exercise the real AMI build orchestration with AWS/build work fully mocked.

Run with make test-ci-security. No AWS credentials or Rust build
are needed. The fixture fails closed on every unexpected AWS operation.
"""

import json
import os
from pathlib import Path
import re
import shlex
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
    # the builder, or the boot test launched from the image the build created
    boot_test = args[args.index("--image-id") + 1] == "ami-1234567890abcdef0"
    print("i-2234567890abcdef0" if boot_test else "i-1234567890abcdef0")
elif operation == "describe-tags":
    if "Name=key,Values=KernelVersion" not in args:
        print(os.environ.get("AMI_TEST_BUILD_STATUS", "boot-ok"))
    elif "Name=resource-id,Values=i-2234567890abcdef0" in args:
        print(os.environ.get("AMI_TEST_KERNEL", "7.3.3-fcvm-0123456789ab"))
    else:
        print(os.environ.get("AMI_TEST_BUILDER_KERNEL", "7.3.3-fcvm-0123456789ab"))
elif operation == "create-image":
    print("ami-1234567890abcdef0")
elif operation == "describe-images":
    if "Name=tag:Purpose,Values=github-runner-candidate" in args:
        print(os.environ.get("AMI_TEST_STALE", ""))
    else:
        print(os.environ.get("AMI_TEST_IMAGE_STATE", "available"))
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
            ["bash", "-c", 'set -euo pipefail\nREGION=us-west-1\nEC2_ARCH=x86_64\naws() { printf "%s\\n" "$@"; }\n' + function + '\ncheck_existing_ami test-build-hash'],
            text=True, capture_output=True, check=True,
        )
        filters = result.stdout.splitlines()
        self.assertIn("Name=state,Values=available", filters)
        # Only a published image of this architecture is a cache hit: a candidate, or one
        # that failed its boot test, has the same BuildHash, and so could the other image.
        self.assertIn("Name=tag:Purpose,Values=github-runner", filters)
        self.assertIn("Name=architecture,Values=x86_64", filters)

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
            harness.write_text(script + 'SCRIPT_DIR=' + shlex.quote(str(Path(__file__).resolve().parent)) + r'''
sleep() { :; }
git() { printf '%040d\n' 0; }
compute_pinned_hash() { echo test-build-hash; }
check_existing_ami() { echo None; }
verify_source_commit_fetchable() { return "${AMI_TEST_UNFETCHABLE:-0}"; }
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
            outputs = directory / "outputs"
            result.outputs = outputs.read_text() if outputs.exists() else ""
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
        # the builder, then the boot test of the candidate image
        self.assertEqual(len(launches), 2)
        for launch in launches:
            self.assert_secure_launch(launch)

    def test_on_demand_fallback_keeps_same_boundary(self):
        result, calls = self.run_build(AMI_TEST_SPOT_FAIL="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        launches = [call for call in calls if call[1] == "run-instances"]
        self.assertEqual(len(launches), 4)
        for launch in launches:
            self.assert_secure_launch(launch)

    @staticmethod
    def flag(call, name):
        return call[call.index(name) + 1]

    def purpose_tags(self, calls):
        """The Purpose values written to the image after its creation, in order."""
        return [self.flag(call, "--tags") for call in calls
                if call[1] == "create-tags" and "ami-1234567890abcdef0" in call]

    def outputs(self, result):
        return dict(line.split("=", 1) for line in result.outputs.splitlines())

    def test_image_is_created_as_a_candidate_no_launcher_selects(self):
        result, calls = self.run_build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        create = next(call for call in calls if call[1] == "create-image")
        tags = {item["ResourceType"]: item["Tags"] for item in json.loads(self.flag(create, "--tag-specifications"))}
        self.assertEqual(set(tags), {"image", "snapshot"})
        self.assertIn({"Key": "Purpose", "Value": "github-runner-candidate"}, tags["image"])
        self.assertNotIn({"Key": "Purpose", "Value": "github-runner"}, tags["image"])
        self.assertIn({"Key": "Purpose", "Value": "github-runner"}, tags["snapshot"])

    def test_candidate_is_published_only_after_it_boots_the_fixed_kernel(self):
        for arch, metal in [("arm64", "c7gd.metal"), ("x64", "c5d.metal")]:
            with self.subTest(arch=arch):
                result, calls = self.run_build(AMI_ARCH=arch)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                operations = [call[1] for call in calls]
                builder, boot_test = [call for call in calls if call[1] == "run-instances"]
                self.assertEqual(self.flag(builder, "--image-id"), "ami-00000000000000000")
                # the boot test launches the candidate itself, on bare metal as the runners are
                self.assertEqual(self.flag(boot_test, "--image-id"), "ami-1234567890abcdef0")
                self.assertEqual(self.flag(boot_test, "--instance-type"), metal)
                self.assertEqual(self.purpose_tags(calls), ["Key=Purpose,Value=github-runner"])
                publish = max(i for i, call in enumerate(calls) if call[1] == "create-tags")
                self.assertLess(operations.index("create-image"), calls.index(boot_test))
                self.assertLess(calls.index(boot_test), publish)
                self.assertTrue(any(c[1] == "describe-tags" and "Name=key,Values=BuildStatus" in c
                                    for c in calls[calls.index(boot_test):publish]))
                outputs = self.outputs(result)
                self.assertEqual(outputs["ami_id"], "ami-1234567890abcdef0")
                self.assertEqual(outputs["boot_kernel"], "7.3.3-fcvm-0123456789ab")
                # both instances are terminated when the run ends
                terminated = [call for call in calls if call[1] == "terminate-instances"]
                self.assertEqual(len(terminated), 1)

    def test_a_candidate_that_fails_its_boot_test_is_never_published(self):
        cases = {
            "the instance reports the check failed": {"AMI_TEST_BUILD_STATUS": "boot-failed"},
            "the instance never reports": {"AMI_TEST_BUILD_STATUS": "starting"},
            "the instance reports no kernel": {"AMI_TEST_KERNEL": "None"},
            # boot-ok from the instance is not enough: it must be running the release the
            # builder resolved from the image's grub.cfg
            "it booted the base image's kernel": {"AMI_TEST_KERNEL": "7.0.0-1013-aws"},
            "it booted the stock kernel the old image had": {"AMI_TEST_KERNEL": "6.14.0-1018-aws"},
            "it booted another kernel that has the fix": {"AMI_TEST_KERNEL": "7.3.3-fcvm-ba9876543210"},
        }
        for name, settings in cases.items():
            with self.subTest(name):
                result, calls = self.run_build(**settings)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.purpose_tags(calls), ["Key=Purpose,Value=github-runner-failed"])
                self.assertNotIn("ami_id", self.outputs(result))

    def test_no_image_is_created_unless_the_builder_reported_a_fixed_kernel(self):
        # KernelVersion on the builder is the release it read from the image's grub.cfg. A
        # builder that reported none, or one without the fix, must not become an image.
        for reported in ["None", "unknown", "nested", "7.0.0-1013-aws"]:
            with self.subTest(reported=reported):
                result, calls = self.run_build(AMI_TEST_BUILDER_KERNEL=reported, AMI_TEST_KERNEL=reported)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(any(call[1] == "create-image" for call in calls))
                self.assertNotIn("ami_id", self.outputs(result))

    def test_candidates_of_earlier_runs_are_retired_before_anything_is_built(self):
        stale = ["ami-0aaaaaaaaaaaaaaa1", "ami-0bbbbbbbbbbbbbbb2"]
        for arch, ec2_arch in [("arm64", "arm64"), ("x64", "x86_64")]:
            with self.subTest(arch=arch):
                result, calls = self.run_build(AMI_ARCH=arch, AMI_TEST_STALE="\t".join(stale))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                listing = next(i for i, call in enumerate(calls)
                               if "Name=tag:Purpose,Values=github-runner-candidate" in call)
                # only this architecture's: the other leg's candidate may be under test right now
                self.assertIn(f"Name=architecture,Values={ec2_arch}", calls[listing])
                retired = {i: call for i, call in enumerate(calls)
                           if call[1] == "create-tags" and call[call.index("--resources") + 1] in stale}
                self.assertEqual([call[call.index("--resources") + 1] for call in retired.values()], stale)
                for call in retired.values():
                    self.assertEqual(self.flag(call, "--tags"), "Key=Purpose,Value=github-runner-failed")
                first_launch = next(i for i, call in enumerate(calls) if call[1] == "run-instances")
                self.assertLess(listing, min(retired))
                self.assertLess(max(retired), first_launch)

    def test_an_unfetchable_source_commit_is_refused_before_any_aws_call(self):
        # The refusal needs nothing from AWS, so it comes before the run's first write: an
        # earlier candidate is not retired by a run that was never going to build.
        result, calls = self.run_build(AMI_TEST_UNFETCHABLE="1", AMI_TEST_STALE="ami-0aaaaaaaaaaaaaaa1")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, [])

    def test_a_candidate_listing_that_cannot_be_trusted_stops_the_leg(self):
        result, calls = self.run_build(AMI_TEST_STALE="not-an-image-id")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any(call[1] in ("run-instances", "create-tags") for call in calls))

    def test_user_data_never_traces_the_metadata_token(self):
        # Under `set -x` a command that expands $TOKEN is written to the instance's log and
        # console. Both scripts fetch the token, use it once, and only then turn tracing on.
        script = Path(__file__).with_name("build-ami.sh").read_text()
        build = script[script.index("\ncreate_user_data() {"):script.index("\nUSERDATA\n}")]
        boot = script[script.index("\ncreate_boot_test_user_data() {"):script.index("\nBOOTTEST\n}")]
        for name, user_data in [("build", build), ("boot test", boot)]:
            with self.subTest(name):
                lines = user_data.splitlines()
                tracing = [i for i, line in enumerate(lines)
                           if re.match(r"\s*set\s+-\w*x|\s*set\s+-o\s+xtrace", line)]
                token = [i for i, line in enumerate(lines) if "TOKEN" in line]
                self.assertTrue(tracing and token)
                self.assertLess(max(token), min(tracing))
                self.assertEqual(lines[max(token)].strip(), "unset TOKEN")

    def test_an_image_that_is_not_available_fails_and_is_never_booted_or_published(self):
        for state in ["pending", "failed"]:
            with self.subTest(state=state):
                result, calls = self.run_build(AMI_TEST_IMAGE_STATE=state)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(len([call for call in calls if call[1] == "run-instances"]), 1)
                self.assertEqual(self.purpose_tags(calls), ["Key=Purpose,Value=github-runner-failed"])
                self.assertNotIn("ami_id", self.outputs(result))

    def test_orphan_cleanup_sees_only_builders_of_its_own_architecture(self):
        # The two legs run side by side: without the filter each terminates the other's builder.
        for arch, ec2_arch in [("arm64", "arm64"), ("x64", "x86_64")]:
            with self.subTest(arch=arch):
                result, calls = self.run_build(AMI_ARCH=arch)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                orphans = [call for call in calls if call[1] == "describe-instances"]
                self.assertEqual(len(orphans), 1)
                self.assertIn("Name=tag:Name,Values=ami-builder-temp", orphans[0])
                self.assertIn(f"Name=architecture,Values={ec2_arch}", orphans[0])

    def test_boot_test_reports_the_running_kernel_checked_by_the_baked_script(self):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("create_boot_test_user_data() {"):].split("\nBOOTTEST\n}", 1)[0] + "\nBOOTTEST\n}"
        user_data = subprocess.run(["bash", "-c", function + "\ncreate_boot_test_user_data"],
                                   text=True, capture_output=True, check=True).stdout
        baked = "/usr/local/sbin/check-bridge-teardown-kernel.sh"
        # no argument: the kernel the instance is running
        self.assertIn(f"if {baked}; then\n  STATUS=boot-ok\nfi\n", user_data)
        self.assertIn("STATUS=boot-failed\n", user_data.split(f"if {baked}", 1)[0])
        self.assertIn('"Key=KernelVersion,Value=$(uname -r)" "Key=BuildStatus,Value=$STATUS"', user_data)
        # the builder puts that script in the image, after checking the kernel GRUB will boot
        build = script[script.index("\ncreate_user_data() {"):script.index("\nUSERDATA\n}")]
        steps = ["--install-host-kernel\n", "\nupdate-grub\n", "\nBOOT_KERNEL=$(./scripts/grub-boot-kernel.sh)\n",
                 '\n./scripts/check-bridge-teardown-kernel.sh "$BOOT_KERNEL"\n',
                 '\naws ec2 create-tags --resources $INSTANCE_ID --tags "Key=KernelVersion,Value=$BOOT_KERNEL" --region us-west-1\n',
                 f"\ninstall -m 0755 scripts/check-bridge-teardown-kernel.sh {baked}\n"]
        positions = [build.index(step) for step in steps]
        self.assertEqual(positions, sorted(positions))

    def test_ambiguous_or_missing_network_fails_before_launch(self):
        for subnets in ["None", "", "subnet-1234567890abcdef0\tsubnet-2234567890abcdef0"]:
            with self.subTest(subnets=subnets):
                result, calls = self.run_build(AMI_TEST_SUBNETS=subnets)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(call[1] == "run-instances" for call in calls))


# The shape grub-mkconfig writes on Ubuntu: a header that sets the default, functions, one
# top-level entry and a submenu holding every installed kernel. Each entry carries a second
# `linux` line for the boot without an initrd.
GRUB_CFG = """\
if [ "${next_entry}" ] ; then
   set default="${next_entry}"
   set next_entry=
   save_env next_entry
else
   set default="@DEFAULT@"
fi
function recordfail {
  set recordfail=1
}
menuentry 'Ubuntu' --class ubuntu $menuentry_id_option 'gnulinux-simple-0000' {
	recordfail
	if [ "${initrdfail}" = 1 ]; then
		linux	/boot/vmlinuz-7.3.3-fcvm-new root=PARTUUID=0000 ro
		initrd	/boot/initrd.img-7.3.3-fcvm-new
	else
		linux	/boot/vmlinuz-7.3.3-fcvm-new root=PARTUUID=0000 ro panic=-1
	fi
}
submenu 'Advanced options for Ubuntu' $menuentry_id_option 'gnulinux-advanced-0000' {
	menuentry 'Ubuntu, with Linux 7.3.3-fcvm-new' --class ubuntu $menuentry_id_option 'gnulinux-7.3.3-fcvm-new-advanced-0000' {
		recordfail
		linux	/boot/vmlinuz-7.3.3-fcvm-new root=PARTUUID=0000 ro
		initrd	/boot/initrd.img-7.3.3-fcvm-new
	}
	menuentry 'Ubuntu, with Linux 7.3.3-fcvm-new (recovery mode)' --class ubuntu $menuentry_id_option 'gnulinux-7.3.3-fcvm-new-recovery-0000' {
		linux	/boot/vmlinuz-7.3.3-fcvm-new root=PARTUUID=0000 ro recovery
		initrd	/boot/initrd.img-7.3.3-fcvm-new
	}
	menuentry 'Ubuntu, with Linux 7.0.0-1013-aws' --class ubuntu $menuentry_id_option 'gnulinux-7.0.0-1013-aws-advanced-0000' {
		linux	/boot/vmlinuz-7.0.0-1013-aws root=PARTUUID=0000 ro
		initrd	/boot/initrd.img-7.0.0-1013-aws
	}
	menuentry 'Ubuntu, with Linux 6.8.0-no-initrd' --class ubuntu $menuentry_id_option 'gnulinux-6.8.0-no-initrd-advanced-0000' {
		linux	/boot/vmlinuz-6.8.0-no-initrd root=PARTUUID=0000 ro
	}
}
"""
ADVANCED = "Advanced options for Ubuntu>Ubuntu, with Linux "


class GrubBootKernelTest(unittest.TestCase):
    """scripts/grub-boot-kernel.sh: the release the image will boot comes from the generated
    grub.cfg, and the image build fails when that cannot be read off it."""

    def resolve(self, default, grubenv="", missing=()):
        with tempfile.TemporaryDirectory(prefix="grub-boot-kernel-test-") as temporary:
            directory = Path(temporary)
            boot = directory / "boot"
            boot.mkdir()
            for name in ["vmlinuz-7.3.3-fcvm-new", "initrd.img-7.3.3-fcvm-new", "vmlinuz-7.0.0-1013-aws",
                         "initrd.img-7.0.0-1013-aws", "vmlinuz-6.8.0-no-initrd"]:
                if name not in missing:
                    (boot / name).write_text("")
            config = directory / "grub.cfg"
            config.write_text(GRUB_CFG.replace("@DEFAULT@", default))
            editenv = directory / "grub-editenv"
            editenv.write_text("#!/bin/sh\ncat <<'ENV'\n" + grubenv + "\nENV\n")
            editenv.chmod(0o755)
            result = subprocess.run(
                ["bash", str(Path(__file__).with_name("grub-boot-kernel.sh"))],
                text=True, capture_output=True, timeout=10,
                env={**os.environ, "PATH": f"{directory}:{os.environ['PATH']}",
                     "GRUB_CFG": str(config), "BOOT_DIR": str(boot)},
            )
            return result.returncode, result.stdout.strip(), result.stderr

    def test_the_default_entry_names_the_kernel(self):
        stale = "saved_entry=" + ADVANCED + "7.0.0-1013-aws"
        for default, grubenv, release in [
            (ADVANCED + "7.3.3-fcvm-new", "", "7.3.3-fcvm-new"),
            # a saved entry is not the default unless grub.cfg says so
            (ADVANCED + "7.3.3-fcvm-new", stale, "7.3.3-fcvm-new"),
            ("${saved_entry}", stale, "7.0.0-1013-aws"),
            # what /etc/default/grub says does not matter: a grub.d override that leaves the
            # old kernel the default shows up here, and the kernel check then refuses it
            (ADVANCED + "7.0.0-1013-aws", "", "7.0.0-1013-aws"),
            ("0", "", "7.3.3-fcvm-new"),
            ("1>2", "", "7.0.0-1013-aws"),
            ("gnulinux-advanced-0000>gnulinux-7.0.0-1013-aws-advanced-0000", "", "7.0.0-1013-aws"),
        ]:
            with self.subTest(default=default, grubenv=grubenv):
                status, release_out, error = self.resolve(default, grubenv)
                self.assertEqual((status, release_out), (0, release), error)

    def test_an_entry_that_cannot_boot_or_cannot_be_resolved_fails(self):
        new = ADVANCED + "7.3.3-fcvm-new"
        for name, arguments, message in [
            ("kernel file missing", dict(default=new, missing=["vmlinuz-7.3.3-fcvm-new"]), "vmlinuz-7.3.3-fcvm-new, which is not in"),
            ("initrd file missing", dict(default=new, missing=["initrd.img-7.3.3-fcvm-new"]), "initrd.img-7.3.3-fcvm-new, which is not in"),
            ("entry without an initrd", dict(default=ADVANCED + "6.8.0-no-initrd"), "has no initrd line"),
            ("entry not in the menu", dict(default=ADVANCED + "9.9.9-absent"), "has no menu entry"),
            ("a submenu is not an entry", dict(default="1"), "has no menu entry"),
            ("index past the submenu", dict(default="1>9"), "has no menu entry"),
            ("saved default with nothing saved", dict(default="${saved_entry}"), "grubenv has none"),
            ("one-time next entry", dict(default=new, grubenv="next_entry=" + ADVANCED + "7.0.0-1013-aws"), "next_entry="),
        ]:
            with self.subTest(name):
                status, release, error = self.resolve(**arguments)
                self.assertEqual((status, release), (1, ""), error)
                self.assertIn(message, error)


if __name__ == "__main__":
    unittest.main(verbosity=2)
