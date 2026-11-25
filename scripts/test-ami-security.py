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
import shutil
import sys
import subprocess
import tempfile
import time
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
    print("i-2234567890abcdef" if boot_test else "i-1234567890abcdef")
elif operation == "describe-tags":
    if "Name=key,Values=KernelVersion" not in args:
        print(os.environ.get("AMI_TEST_BUILD_STATUS", "boot-ok"))
    elif "Name=resource-id,Values=i-2234567890abcdef" in args:
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
elif operation == "get-console-output":
    if "AMI_TEST_CONSOLE" not in os.environ:
        raise SystemExit("UnauthorizedOperation")
    print(os.environ["AMI_TEST_CONSOLE"])
elif operation in {"create-tags", "terminate-instances"}:
    print("{}")
else:
    raise SystemExit("unexpected AWS operation: " + str(args))
'''


class AmiSecurityTest(unittest.TestCase):
    def test_progress_uses_status_tags_not_unscoped_ssm_output(self):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("print_builder_console() {"):script.index("\n# User data of the boot test")]
        for status in ['building', 'failed', 'complete']:
            with self.subTest(status=status), tempfile.TemporaryDirectory(prefix='ami-progress-test-') as temporary:
                directory = Path(temporary)
                aws = directory / 'aws'
                aws.write_text(MOCK_AWS)
                aws.chmod(0o755)
                calls = directory / 'calls.jsonl'
                harness = 'set -euo pipefail\nREGION=us-west-1\nsleep() { :; }\n' + function
                result = subprocess.run(
                    ['bash', '-c', harness + '\nwait_for_build i-1234567890abcdef 2'],
                    text=True, capture_output=True, timeout=10,
                    env={**os.environ, 'PATH': f"{directory}:{os.environ['PATH']}",
                         'AMI_TEST_CALLS': str(calls), 'AMI_TEST_BUILD_STATUS': status},
                )
                recorded = [json.loads(line) for line in calls.read_text().splitlines()]
                # tags, and one read of the builder's console when the wait ends without a
                # finished build (a failure, or here two polls of "building"): never SSM
                console = [call for call in recorded if call[:2] == ['ec2', 'get-console-output']]
                self.assertEqual(len(console), 0 if status == 'complete' else 1, recorded)
                self.assertTrue(all(call[:2] == ['ec2', 'describe-tags'] for call in recorded
                                    if call not in console), recorded)
                self.assertEqual(result.returncode, 0 if status == 'complete' else 1, result.stdout + result.stderr)

    # The shape of an Ubuntu console at first boot: kernel lines, systemd lines, cloud-init's
    # network and key tables, then the user data's own output through cloud-init.
    CONSOLE = """\
[    9.100000] cloud-init[1469]: ci-info: | ens5 | True | 192.0.2.42 | 255.255.255.0 | global | 0a:00:00:00:00:01 |
[   13.210095] cloud-init[1469]: + apt-get update
         Starting apt-news.service - Update APT News...
         Starting esm-cache.service - Update the local ESM caches...
ip-192-0-2-42 login: [   26.214015] cloud-init[1469]: Fetched 38.5 MB in 5s (8236 kB/s)
[   27.139387] cloud-init[1469]: + apt-get install -y unzip curl
[   27.145225] cloud-init[1469]: E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 2020 (apt)
[   27.150000] cloud-init[1469]: + echo DRYRUN --resources i-0123456789abcdef0 from ip-192-0-2-42.us-west-1.compute.internal at 192.0.2.42
[   27.160000] cloud-init[1469]: ::error::printed by a program on the builder
[   27.170000] cloud-init[1469]:   ::add-mask::also after leading spaces
[   27.180000] cloud-init[1469]: An error occurred (UnauthorizedOperation): User: arn:aws:sts::123456789012:assumed-role/example-role/i-0123456789abcdef0 is not authorized
[   27.190000] cloud-init[1469]: reached ec2-192-0-2-42.us-west-1.compute.amazonaws.com over 2001:db8::42 and 2001:db8:0:0:0:0:0:42, account 123456789012.
<14>Nov 24 19:15:06 cloud-init: 256 SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA root@ip-192-0-2-42 (ED25519)
[   27.200000] cloud-init[1469]: ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexamplehostkeyexamplehostkeyexamplehost root@ip-192-0-2-42
[   27.215462] cloud-init[1469]: Cloud-init v. 26.2 finished at Tue, 24 Nov 2026 19:15:06 +0000. Up 27.21 seconds
"""

    def test_a_silent_builders_console_reaches_the_job_log_without_host_details(self):
        for status in ['starting', 'failed:line 62:apt_get install']:
            with self.subTest(status=status):
                result, _ = self.wait_for_build(status, 120, console=self.CONSOLE)
                self.assertEqual(result.returncode, 1)
                printed = result.stdout.split("The builder's last cloud-init lines, from its console:\n", 1)
                self.assertEqual(len(printed), 2, result.stdout)
                self.assertTrue(all(line.startswith("builder| ") for line in printed[1].splitlines()))
                lines = [line[len("builder| "):] for line in printed[1].splitlines()]
                self.assertEqual(lines, [
                    "+ apt-get update",
                    "Fetched 38.5 MB in 5s (8236 kB/s)",
                    "+ apt-get install -y unzip curl",
                    "E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 2020 (apt)",
                    "+ echo DRYRUN --resources <instance> from <host> at <address>",
                    "::error::printed by a program on the builder",
                    "  ::add-mask::also after leading spaces",
                    "An error occurred (UnauthorizedOperation): User: <arn> is not authorized",
                    "reached <host> over <address> and <address>, account <account>.",
                    "Cloud-init v. 26.2 finished at Tue, 24 Nov 2026 19:15:06 +0000. Up 27.21 seconds",
                ])
                # No line reaches the Actions runner as a workflow command: it reads one from a
                # line whose first non-blank characters are `::`.
                self.assertFalse([line for line in result.stdout.splitlines() if line.lstrip().startswith("::")])
        # a console this role may not read changes nothing but one line
        result, polls = self.wait_for_build('starting', 120)
        self.assertEqual((result.returncode, polls), (1, 40))
        self.assertIn("(the builder's console could not be read)", result.stdout)

    def wait_for_build(self, status, polls, console=None):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("print_builder_console() {"):script.index("\n# User data of the boot test")]
        with tempfile.TemporaryDirectory(prefix='ami-progress-test-') as temporary:
            directory = Path(temporary)
            aws = directory / 'aws'
            aws.write_text(MOCK_AWS)
            aws.chmod(0o755)
            calls = directory / 'calls.jsonl'
            harness = 'set -euo pipefail\nREGION=us-west-1\nsleep() { :; }\n' + function
            result = subprocess.run(
                ['bash', '-c', harness + f'\nwait_for_build i-1234567890abcdef0 {polls}'],
                text=True, capture_output=True, timeout=20,
                env={**os.environ, 'PATH': f"{directory}:{os.environ['PATH']}",
                     'AMI_TEST_CALLS': str(calls), 'AMI_TEST_BUILD_STATUS': status,
                     **({'AMI_TEST_CONSOLE': console} if console is not None else {})},
            )
            recorded = [json.loads(line) for line in calls.read_text().splitlines()]
            return result, sum(call[1] == 'describe-tags' for call in recorded)

    def test_a_builder_that_never_reports_fails_after_twenty_minutes_not_an_hour(self):
        # A builder whose user data stops before the AWS CLI is installed can tag nothing and
        # stays "starting". On 2026-11-24 the x64 leg waited 44 minutes on one, and would have
        # waited the full hour. A builder that is building is still given the hour.
        result, polls = self.wait_for_build('starting', 120)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(polls, 40)
        self.assertIn('the builder wrote no tag in 40 polls', result.stdout)
        result, polls = self.wait_for_build('building', 120)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(polls, 120)
        self.assertIn('Build timeout!', result.stdout)

    def test_the_builders_reason_reaches_the_job_log(self):
        reason = 'failed:line 62:apt_get install -y build-essential'
        result, polls = self.wait_for_build(reason, 120)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(polls, 1)
        self.assertIn('Build failed on the builder: ' + reason, result.stdout)

    def user_data(self):
        script = Path(__file__).with_name("build-ami.sh").read_text()
        return script[script.index("\ncreate_user_data() {"):script.index("\nUSERDATA\n}")]

    def run_user_data_block(self, start, end, tail, **settings):
        """Run one block of the builder's user data, from the line `start` through the line
        `end`, with stand-in commands on PATH, then `tail`."""
        user_data = self.user_data()
        block = user_data[user_data.index(start):user_data.index(end) + len(end)]
        with tempfile.TemporaryDirectory(prefix="ami-user-data-test-") as temporary:
            directory = Path(temporary)
            calls = directory / "calls.jsonl"
            for name, body in settings.pop("commands", {}).items():
                (directory / name).write_text(body)
                (directory / name).chmod(0o755)
            # Only the stand-ins and the few tools the block calls are on PATH, so a block
            # that reaches for `aws` finds the stand-in or nothing, never a real CLI.
            for tool in ["tr", "cut"]:
                (directory / tool).symlink_to(shutil.which(tool))
            result = subprocess.run(
                [shutil.which("bash"), "-c", "set -Eeuo pipefail\nsleep() { :; }\n" + block + "\n" + tail],
                text=True, capture_output=True, timeout=20,
                env={"PATH": str(directory), "CALLS": str(calls), **settings},
            )
            recorded = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            return result, recorded

    RECORD = '#!' + sys.executable + '\nimport json, os, sys\nopen(os.environ["CALLS"], "a").write(json.dumps(sys.argv[1:]) + "\\n")\n'

    def test_a_failed_step_puts_its_line_and_command_in_the_build_status_tag(self):
        start, end = "INSTANCE_ID=unknown\n", "trap 'tag_failed \"$LINENO\" \"$BASH_COMMAND\"' ERR\n"
        tail = 'INSTANCE_ID=i-1234567890abcdef0\nfalse "one, two" \'three\'\necho not reached\n'
        result, calls = self.run_user_data_block(start, end, tail, commands={"aws": self.RECORD})
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("not reached", result.stdout)
        self.assertEqual(len(calls), 1)
        call = calls[0]
        self.assertEqual(call[:4], ["ec2", "create-tags", "--resources", "i-1234567890abcdef0"])
        tags = json.loads(call[call.index("--tags") + 1])
        self.assertEqual([tag["Key"] for tag in tags], ["BuildStatus"])
        # the status starts with "failed", names the line and the command, and holds only
        # characters a tag value takes
        self.assertRegex(tags[0]["Value"], r"^failed:line \d+:false _one_ two_ _three_$")
        self.assertRegex(tags[0]["Value"], r"^[A-Za-z0-9 _.:/=+@-]{1,200}$")
        # before the AWS CLI exists there is nothing to tag with: the step still stops the script
        result, calls = self.run_user_data_block(start, end, tail)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("BUILD FAILED at line", result.stdout)
        self.assertEqual(calls, [])

    def test_the_lock_setting_covers_the_whole_build_and_leaves_the_image(self):
        lines = self.user_data().splitlines()
        conf = "/etc/apt/apt.conf.d/99-fcvm-builder-lock-wait"
        where = lambda text: [i for i, line in enumerate(lines) if text in line and not line.lstrip().startswith("#")]
        written, removed = where(f"> {conf}"), where(f"rm -f {conf}")
        self.assertEqual((len(written), len(removed)), (1, 1))
        self.assertEqual(lines[written[0]], f"echo 'DPkg::Lock::Timeout \"120\";' > {conf}")
        apt_calls = where("apt_get ") + where("setup_22.x") + where("installdependencies.sh")
        self.assertLess(written[0], min(apt_calls))
        self.assertGreater(removed[0], max(apt_calls))
        # the image is taken after the "complete" tag: the setting is gone by then
        complete = where("Key=BuildStatus,Value=complete")
        self.assertEqual(len(complete), 1)
        self.assertLess(removed[0], complete[0])

    def test_the_kernel_install_waits_for_the_dpkg_lock(self):
        user_data = self.user_data()
        begin = user_data.index("wait_for_dpkg_lock() {")
        helper = user_data[begin:user_data.index("\nPY\n}\n", begin) + len("\nPY\n}\n")]
        # it runs right before the step that calls dpkg directly
        self.assertIn("\nwait_for_dpkg_lock\n./target/release/fcvm setup --kernel-profile nested "
                      "--build-kernels --install-host-kernel\n", user_data)
        hold = ("import fcntl, sys, time\nlock = open(sys.argv[1], 'a')\nfcntl.lockf(lock, fcntl.LOCK_EX)\n"
                "print('held', flush=True)\ntime.sleep(float(sys.argv[2]))\n")
        for held_for, wait, status in [(0, 5, 0), (3, 30, 0), (30, 3, 1)]:
            with self.subTest(held_for=held_for, wait=wait), \
                    tempfile.TemporaryDirectory(prefix="ami-dpkg-lock-test-") as temporary:
                lock = Path(temporary) / "lock-frontend"
                holder = subprocess.Popen([sys.executable, "-c", hold, str(lock), str(held_for)],
                                          stdout=subprocess.PIPE, text=True)
                try:
                    self.assertEqual(holder.stdout.readline().strip(), "held")
                    if held_for == 0:
                        holder.wait(timeout=10)
                    started = time.monotonic()
                    result = subprocess.run(
                        ["bash", "-c", helper + f"\nwait_for_dpkg_lock {shlex.quote(str(lock))} {wait}\n"],
                        text=True, capture_output=True, timeout=60)
                    waited = time.monotonic() - started
                finally:
                    holder.kill()
                    holder.wait()
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                if held_for == 3:
                    self.assertGreaterEqual(waited, 2)
                if status == 1:
                    self.assertIn("still held", result.stderr)

    def test_the_failure_handler_is_installed_before_anything_can_fail(self):
        lines = self.user_data().splitlines()
        trap = next(i for i, line in enumerate(lines) if line.startswith("trap 'tag_failed"))
        first = {name: next(i for i, line in enumerate(lines) if pattern in line) for name, pattern in
                 [("token", "TOKEN=$(curl"), ("apt", "apt_get -o APT::Update::Error-Mode=any update"),
                  ("cli", "awscli-exe-linux-")]}
        for name, position in first.items():
            self.assertLess(trap, position, name)
        self.assertEqual(sum(line.startswith("trap ") for line in lines), 1)
        self.assertIn("set -Eeuo pipefail", lines)

    def test_every_apt_call_waits_for_the_lock_and_is_retried(self):
        user_data = self.user_data()
        helper = user_data[user_data.index("apt_get() {"):user_data.index("\n}\n", user_data.index("apt_get() {"))]
        self.assertIn('apt-get -o DPkg::Lock::Timeout=120 -o Acquire::Retries=5 "$@"', helper)
        outside = user_data.replace(helper, "")
        bare = [line for line in outside.splitlines()
                if re.search(r"(^|[\s;&|(])apt-get\s", line) and not line.lstrip().startswith("#")]
        self.assertEqual(bare, [])
        self.assertGreaterEqual(len(re.findall(r"(?m)^apt_get ", outside)), 4)

    def test_the_apt_helper_runs_a_failed_call_again(self):
        flaky = ('#!/bin/bash\necho "$*" >> "$CALLS.args"\nn=$(wc -l < "$CALLS.args")\n'
                 '[ "$n" -gt "${FAILURES:-0}" ]\n')
        for failures, status, attempts in [(0, 0, 1), (2, 0, 3), (99, 1, 3)]:
            with self.subTest(failures=failures):
                with tempfile.TemporaryDirectory(prefix="ami-apt-test-") as temporary:
                    directory = Path(temporary)
                    (directory / "apt-get").write_text(flaky)
                    (directory / "apt-get").chmod(0o755)
                    user_data = self.user_data()
                    begin = user_data.index("apt_get() {")
                    helper = user_data[begin:user_data.index("\n}\n", begin) + 3]
                    result = subprocess.run(
                        ["bash", "-c", "set -Eeuo pipefail\nsleep() { :; }\n"
                         'tag_failed() { echo "TAG line $1: $2"; exit 1; }\n' + helper +
                         "\n\napt_get install -y unzip curl\n"],
                        text=True, capture_output=True, timeout=20,
                        env={"PATH": f"{directory}:/usr/bin:/bin", "CALLS": str(directory / "calls"),
                             "FAILURES": str(failures)},
                    )
                    self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                    # a call that never succeeds reports itself and the line that made it
                    reported = "TAG line " in result.stdout
                    self.assertEqual(reported, status == 1)
                    if reported:
                        self.assertRegex(result.stdout, r"(?m)^TAG line \d+: apt-get install -y unzip curl$")
                    args = (directory / "calls.args").read_text().splitlines()
                    self.assertEqual(len(args), attempts)
                    self.assertEqual(set(args), {"-o DPkg::Lock::Timeout=120 -o Acquire::Retries=5 install -y unzip curl"})

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
        self.assertIn(f"if {baked} && nic_order_ok; then\n  STATUS=boot-ok\nfi\n", user_data)
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

    def test_the_arm64_image_loads_the_nic_driver_before_cloud_init_local(self):
        # #2174: the arm64 host kernel keeps ENA a module and the image boots without an
        # initramfs, so without these two files the NIC binds before cloud-init-local only by
        # winning a race, and a runner that loses it runs no user data and never registers.
        script = Path(__file__).with_name("build-ami.sh").read_text()
        build = script[script.index("\ncreate_user_data() {"):script.index("\nUSERDATA\n}")]
        block = ('if [ "$RUNNER_ARCH" = arm64 ]; then\n'
                 "  echo ena > /etc/modules-load.d/fcvm-runner-nic.conf\n"
                 "  install -d /etc/systemd/system/cloud-init-local.service.d\n"
                 "  printf '[Unit]\\nAfter=systemd-modules-load.service\\n' \\\n"
                 "    > /etc/systemd/system/cloud-init-local.service.d/10-fcvm-runner-nic.conf\n"
                 "fi\n")
        self.assertIn(block, build)
        # after the architecture is known, and in the image before it reports done
        self.assertLess(build.index("aarch64) RUNNER_ARCH=arm64 ;;"), build.index(block))
        self.assertLess(build.index(block), build.index("Key=BuildStatus,Value=complete"))

    def test_the_boot_test_requires_cloud_init_local_ordered_after_the_module_loader(self):
        # The candidate is published only when systemd's resolved order of the booted image
        # holds (#2174). An order that cannot be read is a failed boot test, not a pass.
        script = Path(__file__).with_name("build-ami.sh").read_text()
        function = script[script.index("create_boot_test_user_data() {"):].split("\nBOOTTEST\n}", 1)[0] + "\nBOOTTEST\n}"
        user_data = subprocess.run(["bash", "-c", function + "\ncreate_boot_test_user_data"],
                                   text=True, capture_output=True, check=True).stdout
        check = user_data[user_data.index("nic_order_ok() {"):user_data.index("\nSTATUS=boot-failed\n")]
        loader = "systemd-modules-load.service"
        # the loader's file is named where the function reads it
        self.assertIn("\nNIC_MODULES_LOAD=/etc/modules-load.d/fcvm-runner-nic.conf\n", user_data.split("nic_order_ok() {")[0])
        # machine, the resolved After, systemctl show's status, whether the loader is active,
        # the loader's file (None: absent)
        cases = [("aarch64", f"systemd-remount-fs.service {loader} system.slice", 0, 0, "ena\n", True),
                 ("aarch64", "systemd-remount-fs.service system.slice", 0, 0, "ena\n", False),
                 ("aarch64", f"not-{loader} system.slice", 0, 0, "ena\n", False),
                 ("aarch64", "", 0, 0, "ena\n", False),
                 ("aarch64", loader, 1, 0, "ena\n", False),
                 # ordered after a loader that failed or never ran: the module was not loaded there
                 ("aarch64", loader, 0, 3, "ena\n", False),
                 # an active loader that was not told to load the driver
                 ("aarch64", loader, 0, 0, None, False),
                 ("aarch64", loader, 0, 0, "enax\n", False),
                 # a long After with the loader first: the answer does not depend on a reader
                 # that stops at the first match while the list is still being written
                 ("aarch64", loader + " example.service" * 20000, 0, 0, "ena\n", True),
                 ("x86_64", "systemd-remount-fs.service system.slice", 0, 3, None, True)]
        for machine, after, status, inactive, conf, want in cases:
            with self.subTest(machine=machine, after=after[:80], status=status, inactive=inactive, conf=conf), \
                    tempfile.TemporaryDirectory() as tmp:
                if conf is not None:
                    Path(tmp, "nic.conf").write_text(conf)
                for name, body in (("uname", f'[ "$*" = -m ] && echo {machine}'),
                                   ("systemctl", f'[ "$*" = "is-active --quiet {loader}" ] && exit {inactive}\n'
                                                 f'[ "$*" = "show cloud-init-local.service -p After --value" ] || exit 9\n'
                                                 f"echo '{after}'\nexit {status}")):
                    tool = Path(tmp, name)
                    tool.write_text("#!/bin/bash\n" + body + "\n")
                    tool.chmod(0o755)
                ran = subprocess.run(["bash", "-c", f"set -uo pipefail\nNIC_MODULES_LOAD={tmp}/nic.conf\n" + check + "\nnic_order_ok"],
                                     env={"PATH": f"{tmp}:/usr/bin:/bin"}, text=True, capture_output=True)
                self.assertEqual(ran.returncode == 0, want, ran.stderr)

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
