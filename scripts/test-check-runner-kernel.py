#!/usr/bin/env python3
"""Tests of scripts/check-runner-kernel.py, the first step of every self-hosted job
that starts VMs: which running kernels it accepts, which it fails, and that it blocks
when it cannot evaluate.

Each test builds a small repository (a rootfs-config.toml with the two host kernel
tables, and their patch sets under kernel/), commits to it at chosen times, clones it
one commit deep as a CI checkout is, and runs the script on the clone with the kernel
release, machine and clock given as arguments. Run with make test-runner-kernel.
"""

import datetime
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts/check-runner-kernel.py"

NOW = 1_800_000_000
HOUR = 3600

CONFIG = textwrap.dedent("""\
    [kernel_profiles.nested.arm64]
    kernel_version = "6.18.60"

    [kernel_profiles.nested.arm64.host_kernel]
    kernel_version = "7.3.3"
    build_inputs = [
        "kernel/patches-arm64-host/*.patch",
        "kernel/host-arm64.conf",
    ]

    [kernel_profiles.nested.amd64.host_kernel]
    kernel_version = "7.3.3"
    build_inputs = [
        "kernel/patches-x86-host/*.patch",
        "kernel/host-x86.conf",
    ]
""")

# A third x86 host patch, as 0003 of kernel/patches-x86-host is: nothing on arm64 reads it.
X86_ONLY = "kernel/patches-x86-host/0003-x86-only.patch"


def utc(when):
    return datetime.datetime.fromtimestamp(when, datetime.timezone.utc).strftime(
        "%Y-%m-%d %H:%M UTC"
    )


class Origin:
    """A repository whose `main` the tests commit to at chosen times."""

    def __init__(self, path):
        self.path = Path(path)
        self.path.mkdir()
        self.git("init", "--quiet", "--initial-branch=main")
        self.write("rootfs-config.toml", CONFIG)
        self.write("kernel/patches/0001-shared.patch", "shared 1\n")
        self.write("kernel/patches/0002-shared.patch", "shared 2\n")
        for name in ("patches-x86-host", "patches-arm64-host"):
            (self.path / "kernel" / name).mkdir()
            # The real host sets link the patches they share with other sets.
            for patch in ("0001-shared.patch", "0002-shared.patch"):
                (self.path / "kernel" / name / patch).symlink_to(f"../patches/{patch}")
        self.write("kernel/patches-arm64-host/nv2.patch", "arm64 only\n")
        self.write("kernel/host-x86.conf", "CONFIG_ENA_ETHERNET=y\n")
        self.write("kernel/host-arm64.conf", "CONFIG_WEXT_CORE=n\n")

    def git(self, *args, when=None):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
        if when is not None:
            env["GIT_AUTHOR_DATE"] = env["GIT_COMMITTER_DATE"] = f"@{when} +0000"
        return subprocess.run(
            ["git", "-C", str(self.path), "-c", "user.name=test", "-c", "user.email=test@example.com",
             "-c", "commit.gpgsign=false", *args],
            check=True, capture_output=True, text=True, env=env,
        ).stdout.strip()

    def write(self, name, text):
        path = self.path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, hours_ago, message="change", **files):
        """Commit `files` (path -> text) to the current branch, dated `hours_ago` before NOW."""
        for name, text in files.items():
            self.write(name, text)
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "-m", message,
                 when=NOW - int(hours_ago * HOUR))
        return self.git("rev-parse", "HEAD")

    def checkout(self, into, branch="main", depth=1):
        """A clone as actions/checkout leaves it: one commit deep, `origin` the remote."""
        depth_args = [] if depth is None else [f"--depth={depth}"]
        subprocess.run(
            ["git", "clone", "--quiet", *depth_args, "--branch", branch,
             f"file://{self.path}", str(into)],
            check=True, capture_output=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull},
        )
        return Path(into)


def check(root, release, machine="x86_64", now=NOW, extra=(), path=None):
    """Run the script; returns (exit status, stdout, stderr)."""
    env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
    env.pop("GITHUB_SERVER_URL", None)
    env.pop("GITHUB_REPOSITORY", None)
    if path is not None:
        env["PATH"] = path
    result = subprocess.run(
        [sys.executable, "-B", str(SCRIPT), "--root", str(root), "--release", release,
         "--machine", machine, "--now", str(now), *extra],
        capture_output=True, text=True, env=env,
    )
    return result.returncode, result.stdout, result.stderr


def expected(root, machine="x86_64"):
    result = subprocess.run(
        [sys.executable, "-B", str(SCRIPT), "--root", str(root), "--machine", machine,
         "--print-expected"],
        capture_output=True, text=True,
    )
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


class Case(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="runner-kernel-test-")
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.origin = Origin(self.tmp / "origin")
        # A PATH with no git, for the verdicts that must not need it.
        self.no_tools = self.tmp / "empty-bin"
        self.no_tools.mkdir()

    def rollout(self, changed_hours_ago, later_commits=1):
        """main as it is after a host kernel change: the first build, then the x86-only
        patch merged `changed_hours_ago` before NOW, then unrelated commits. Returns the
        release before the change, the release after, and a CI checkout of main."""
        self.origin.commit(72, "first build")
        before = expected(self.origin.path)
        self.origin.commit(changed_hours_ago, "x86 host patch", **{X86_ONLY: "pv kick\n"})
        after = expected(self.origin.path)
        for index in range(later_commits):
            self.origin.commit(changed_hours_ago * (later_commits - index) / (later_commits + 1),
                               "unrelated", README=f"{index}\n")
        return before, after, self.origin.checkout(self.tmp / "work")


class ExpectedBuild(Case):
    def test_the_release_is_the_version_and_a_hash_of_the_inputs_in_order(self):
        self.origin.commit(1)
        root = self.origin.path
        digest = hashlib.sha256(b"7.3.3")
        for name in ("patches-x86-host/0001-shared.patch", "patches-x86-host/0002-shared.patch",
                     "host-x86.conf"):
            digest.update((root / "kernel" / name).read_bytes())
        self.assertEqual(expected(root), "7.3.3-fcvm-" + digest.hexdigest()[:12])

    def test_the_build_this_checkout_gives_passes_without_git(self):
        self.origin.commit(1)
        for machine in ("x86_64", "aarch64"):
            release = expected(self.origin.path, machine)
            status, out, err = check(self.origin.path, release, machine, path=str(self.no_tools))
            self.assertEqual((status, err), (0, ""), out)
            self.assertEqual(
                out, f"runner kernel {release} is the host kernel this checkout builds for {machine}\n"
            )

    def test_every_input_changes_the_release(self):
        self.origin.commit(1)
        root = self.origin.path
        first = expected(root)
        seen = {first}
        changes = [
            ("kernel/patches-x86-host/0003-x86-only.patch", "new patch\n"),
            ("kernel/patches/0002-shared.patch", "a linked patch, edited\n"),
            ("kernel/host-x86.conf", "CONFIG_ENA_ETHERNET=m\n"),
            ("rootfs-config.toml", CONFIG.replace('"7.3.3"\nbuild_inputs = [\n    "kernel/patches-x86',
                                                  '"7.3.4"\nbuild_inputs = [\n    "kernel/patches-x86')),
        ]
        for name, text in changes:
            self.origin.write(name, text)
            release = expected(root)
            self.assertNotIn(release, seen, f"{name} did not change the release")
            seen.add(release)
        # The runner's kernel from before the changes is no longer the expected one.
        status, _, err = check(root, first, extra=["--current-only"])
        self.assertEqual(status, 1, err)

    def test_a_vm_patch_is_not_a_host_input(self):
        self.origin.commit(1)
        before = expected(self.origin.path)
        self.origin.write("kernel/patches-x86-host/mmfr4-override.vm.patch", "guest only\n")
        self.assertEqual(expected(self.origin.path), before)

    def test_an_x86_only_patch_leaves_the_arm64_build_unchanged(self):
        self.origin.commit(1)
        root = self.origin.path
        x86, arm64 = expected(root, "x86_64"), expected(root, "aarch64")
        self.origin.write(X86_ONLY, "pv kick\n")
        self.assertNotEqual(expected(root, "x86_64"), x86)
        self.assertEqual(expected(root, "aarch64"), arm64)
        # An arm64 runner on the build from before the x86 patch still passes, with no git.
        status, out, err = check(root, arm64, "aarch64", path=str(self.no_tools))
        self.assertEqual((status, err), (0, ""), out)
        self.origin.write("kernel/patches-arm64-host/nv2.patch", "arm64 only, edited\n")
        self.assertNotEqual(expected(root, "aarch64"), arm64)


class Rollout(Case):
    def test_the_previous_build_passes_with_a_warning_inside_the_window(self):
        before, after, work = self.rollout(changed_hours_ago=4)
        status, out, err = check(work, before)
        self.assertEqual((status, err), (0, ""), out)
        lines = out.splitlines()
        self.assertTrue(lines[0].startswith("::warning title=Runner kernel is the previous build::"),
                        lines[0])
        self.assertIn(f"runner kernel {before}", lines[0])
        self.assertIn(f"accepted until {utc(NOW + 20 * HOUR)}", lines[0])
        self.assertIn(f"main now builds {after}.", out)
        self.assertIn(f"until\n{utc(NOW - 4 * HOUR)}", out)

    def test_the_previous_build_fails_after_the_window(self):
        before, after, work = self.rollout(changed_hours_ago=4)
        # One second before the deadline it still passes; at the deadline it fails.
        self.assertEqual(check(work, before, now=NOW + 20 * HOUR - 1)[0], 0)
        status, out, err = check(work, before, now=NOW + 20 * HOUR)
        self.assertEqual(status, 1, out + err)
        self.assertTrue(out.startswith("::error title=Runner kernel::"), out)
        self.assertIn(f"runner kernel {before} is an fcvm host kernel that neither this checkout",
                      err)
        self.assertIn("The runner image is stale", err)
        self.assertIn(after, err)
        # The build the checkout gives still passes then.
        self.assertEqual(check(work, after, now=NOW + 20 * HOUR)[0], 0)

    def test_a_build_main_stopped_giving_before_the_window_fails(self):
        before, _, work = self.rollout(changed_hours_ago=25)
        status, _, err = check(work, before)
        self.assertEqual(status, 1, err)
        self.assertIn("The runner image is stale", err)

    def test_each_build_has_its_own_deadline(self):
        self.origin.commit(72, "first build")
        first = expected(self.origin.path)
        self.origin.commit(20, "patch 3", **{X86_ONLY: "pv kick\n"})
        second = expected(self.origin.path)
        self.origin.commit(2, "patch 4", **{"kernel/patches-x86-host/0004-next.patch": "next\n"})
        work = self.origin.checkout(self.tmp / "work")
        for release, deadline in ((first, NOW + 4 * HOUR), (second, NOW + 22 * HOUR)):
            status, out, _ = check(work, release)
            self.assertEqual(status, 0, release)
            self.assertIn(f"accepted until {utc(deadline)}", out)
        # Five hours on, the first build is past its deadline and the second is not.
        self.assertEqual(check(work, first, now=NOW + 5 * HOUR)[0], 1)
        self.assertEqual(check(work, second, now=NOW + 5 * HOUR)[0], 0)

    def test_the_window_starts_when_the_change_reached_main_not_when_it_was_written(self):
        self.origin.commit(72, "first build")
        before = expected(self.origin.path)
        self.origin.git("checkout", "--quiet", "-b", "patch")
        self.origin.commit(70, "x86 host patch, written three days ago", **{X86_ONLY: "pv kick\n"})
        self.origin.git("checkout", "--quiet", "main")
        self.origin.git("merge", "--quiet", "--no-ff", "-m", "merge the patch", "patch",
                        when=NOW - 3 * HOUR)
        work = self.origin.checkout(self.tmp / "work")
        status, out, err = check(work, before)
        self.assertEqual(status, 0, out + err)
        self.assertIn(f"accepted until {utc(NOW + 21 * HOUR)}", out)
        self.assertEqual(check(work, before, now=NOW + 21 * HOUR)[0], 1)

    def test_history_deeper_than_the_first_fetch_is_fetched(self):
        # 80 commits after the change: more than the 64 the second fetch asks for.
        before, _, work = self.rollout(changed_hours_ago=23, later_commits=80)
        status, out, err = check(work, before)
        self.assertEqual(status, 0, out + err)
        self.assertIn(f"accepted until {utc(NOW + 1 * HOUR)}", out)

    def test_more_history_than_the_check_fetches_blocks(self):
        before, _, work = self.rollout(changed_hours_ago=23, later_commits=5)
        spec = importlib.util.spec_from_file_location("check_runner_kernel", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.FETCH_DEPTHS = (1, 2)
        with self.assertRaisesRegex(module.Blocked, "more than 2 commits in the last 24 hours"):
            module.verdict(before, "x86_64", work, NOW, False)

    def test_a_full_clone_stays_a_full_clone(self):
        before, _, _ = self.rollout(changed_hours_ago=4)
        work = self.origin.checkout(self.tmp / "full", depth=None)
        self.assertEqual(check(work, before)[0], 0)
        shallow = subprocess.run(
            ["git", "-C", str(work), "rev-parse", "--is-shallow-repository"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        self.assertEqual(shallow, "false")

    def test_current_only_rejects_the_previous_build_without_git(self):
        before, after, work = self.rollout(changed_hours_ago=4)
        status, out, err = check(work, before, extra=["--current-only"], path=str(self.no_tools))
        self.assertEqual(status, 1, out + err)
        self.assertIn("This job accepts no other", err)
        self.assertIn(after, err)
        self.assertEqual(check(work, after, extra=["--current-only"])[0], 0)


class OtherCheckouts(Case):
    def test_a_checkout_that_changes_the_host_kernel_runs_on_mains_build(self):
        self.origin.commit(72, "first build")
        on_main = expected(self.origin.path)
        self.origin.commit(50, "unrelated", README="1\n")
        self.origin.git("checkout", "--quiet", "-b", "change")
        # Written 40 hours ago: an unmerged change has no deadline.
        self.origin.commit(40, "x86 host patch", **{X86_ONLY: "pv kick\n"})
        work = self.origin.checkout(self.tmp / "work", branch="change")
        status, out, err = check(work, on_main)
        self.assertEqual((status, err), (0, ""), out)
        self.assertTrue(out.startswith("::notice title=Runner kernel::"), out)
        self.assertIn(f"runner kernel {on_main} is the host kernel main builds for x86_64.", out)
        self.assertIn("does not test this checkout's host kernel", out)
        self.assertEqual(check(work, on_main, extra=["--current-only"])[0], 1)

    def test_a_checkout_behind_main_runs_on_mains_build(self):
        self.origin.commit(100, "first build")
        self.origin.git("branch", "old")
        self.origin.commit(90, "x86 host patch", **{X86_ONLY: "pv kick\n"})
        on_main = expected(self.origin.path)
        work = self.origin.checkout(self.tmp / "work", branch="old")
        self.assertNotEqual(expected(work), on_main)
        status, out, err = check(work, on_main)
        self.assertEqual((status, err), (0, ""), out)
        self.assertTrue(out.startswith("::notice title=Runner kernel::"), out)


class OtherKernels(Case):
    def test_a_kernel_fcvm_did_not_build_fails_and_history_is_never_asked(self):
        _, after, work = self.rollout(changed_hours_ago=4)
        for release in (
            "6.14.0-1018-aws",  # the stock kernel the x64 runners booted before their image
            "7.0.0-1017-aws",
            "6.8.0-1021-generic",
            "7.3.3",
            "7.3.3+",
            "7.3.3-fcvm-example",
            "7.3.3-fcvm-0123456789AB",
            "7.3.3-fcvm-0123456789abc",
            after + "-dirty",
        ):
            status, out, err = check(work, release, path=str(self.no_tools))
            self.assertEqual(status, 1, f"{release}: {out}{err}")
            self.assertIn(f"runner kernel {release} is not a host kernel fcvm built", err)
            self.assertIn(after, err)

    def test_a_release_that_cannot_be_read_blocks(self):
        _, _, work = self.rollout(changed_hours_ago=4)
        for release in ("", "unknown", "7.3", "7.3.3fcvm", "07.3.3-fcvm-0123456789ab", "7.3.3.1"):
            status, out, err = check(work, release)
            self.assertEqual(status, 2, f"'{release}': {out}{err}")
            self.assertIn(f"BLOCKED: the runner kernel check cannot evaluate: '{release}' is not a "
                          "kernel release", err)
            self.assertTrue(out.startswith("::error title=Runner kernel check blocked::"), out)

    def test_a_machine_with_no_host_kernel_blocks(self):
        self.origin.commit(1)
        status, _, err = check(self.origin.path, "7.3.3-fcvm-0123456789ab", machine="riscv64")
        self.assertEqual(status, 2, err)
        self.assertIn("machine 'riscv64' has no host kernel", err)


class CannotEvaluate(Case):
    def blocked(self, root, wanted, release="7.3.3-fcvm-0123456789ab", **kwargs):
        status, out, err = check(root, release, **kwargs)
        self.assertEqual(status, 2, out + err)
        self.assertIn("BLOCKED: the runner kernel check cannot evaluate:", err)
        self.assertRegex(err, wanted)

    def test_a_config_that_cannot_be_read_blocks(self):
        self.origin.commit(1)
        root = self.origin.path
        cases = {
            "cannot read rootfs-config.toml": lambda: (root / "rootfs-config.toml").unlink(),
            "cannot read rootfs-config.toml: Expected": lambda: self.origin.write(
                "rootfs-config.toml", "[kernel_profiles.nested\n"),
            r"has no \[kernel_profiles.nested.amd64.host_kernel\]": lambda: self.origin.write(
                "rootfs-config.toml", CONFIG.replace("nested.amd64.host_kernel", "other")),
            "kernel_version is None": lambda: self.origin.write(
                "rootfs-config.toml",
                CONFIG.replace('host_kernel]\nkernel_version = "7.3.3"\nbuild_inputs = [\n    "kernel/patches-x86',
                               'host_kernel]\nbuild_inputs = [\n    "kernel/patches-x86')),
            r"build_inputs is \[\]": lambda: self.origin.write(
                "rootfs-config.toml",
                CONFIG.replace('[\n    "kernel/patches-x86-host/*.patch",\n    "kernel/host-x86.conf",\n]', "[]")),
            "pattern 'kernel/host-x86.conf' matched no files": lambda: (
                root / "kernel/host-x86.conf").unlink(),
            "cannot read build input kernel/patches-x86-host/0002-shared.patch": lambda: (
                root / "kernel/patches/0002-shared.patch").unlink(),
            r"pattern '\.\./outside/\*\.patch' is outside kernel/": lambda: self.origin.write(
                "rootfs-config.toml", CONFIG.replace("kernel/patches-x86-host/*.patch",
                                                     "../outside/*.patch")),
        }
        for wanted, damage in cases.items():
            with self.subTest(wanted):
                self.origin.git("checkout", "--quiet", "--force", "HEAD", "--", ".")
                damage()
                self.blocked(root, "cannot compute the host kernel this checkout builds for "
                                   "x86_64: .*" + wanted)
                # Not even the release the undamaged tree gives passes.
                self.assertEqual(check(root, "7.3.3-fcvm-0123456789ab", extra=["--current-only"])[0], 2)

    def test_a_python_without_tomllib_blocks(self):
        self.origin.commit(1)
        program = (
            "import runpy, sys; sys.modules['tomllib'] = None; "
            f"sys.argv = [{str(SCRIPT)!r}, '--root', {str(self.origin.path)!r}, '--release', "
            "'7.3.3-fcvm-0123456789ab', '--machine', 'x86_64']; "
            f"runpy.run_path({str(SCRIPT)!r}, run_name='__main__')"
        )
        result = subprocess.run([sys.executable, "-B", "-c", program], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("this Python has no tomllib", result.stderr)

    def test_history_without_git_blocks(self):
        before, after, work = self.rollout(changed_hours_ago=4)
        self.blocked(work, f"runner kernel {before} is not the build this checkout gives "
                           rf"\({re.escape(after)}\), and whether main gave it in the last 24 hours "
                           "is unknown: git is not on PATH",
                     release=before, path=str(self.no_tools))

    def test_a_checkout_that_is_not_a_repository_blocks(self):
        self.origin.commit(1)
        plain = self.tmp / "plain"
        plain.mkdir()
        for name in ("rootfs-config.toml", "kernel"):
            subprocess.run(["cp", "-a", str(self.origin.path / name), str(plain)], check=True)
        self.blocked(plain, "is unknown: `git rev-parse --is-shallow-repository` failed")

    def test_a_fetch_that_fails_blocks(self):
        before, _, work = self.rollout(changed_hours_ago=4)
        subprocess.run(["git", "-C", str(work), "remote", "set-url", "origin",
                        f"file://{self.tmp}/no-such-repository"], check=True)
        self.blocked(work, "is unknown: `git fetch .*` failed", release=before)


class ThisRepository(unittest.TestCase):
    """What the script assumes about the repository it is checked out in."""

    def test_both_runner_architectures_have_a_release_and_it_passes(self):
        for machine in ("x86_64", "aarch64"):
            release = expected(ROOT, machine)
            self.assertRegex(release, r"^[0-9]+\.[0-9]+\.[0-9]+-fcvm-[0-9a-f]{12}$")
            status, out, err = check(ROOT, release, machine)
            self.assertEqual((status, err), (0, ""), out)

    def test_the_image_installs_the_profile_and_is_built_from_the_branch_the_script_names(self):
        spec = importlib.util.spec_from_file_location("check_runner_kernel", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        ami = (ROOT / "scripts/build-ami.sh").read_text()
        self.assertRegex(
            ami, rf"fcvm setup --kernel-profile {module.PROFILE}\b[^\n]*--install-host-kernel\n")
        workflow = (ROOT / module.IMAGE_WORKFLOW).read_text()
        self.assertIn(f"github.ref == 'refs/heads/{module.IMAGE_BRANCH}' &&", workflow)

    def test_the_committed_inputs_are_all_under_kernel(self):
        # The history path computes a release from `git archive <commit> rootfs-config.toml
        # kernel`; an input outside it, or a link out of it, would give another release there.
        inside = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--is-inside-work-tree"],
                                capture_output=True, text=True)
        if inside.returncode != 0:
            self.skipTest("not a git checkout")
        spec = importlib.util.spec_from_file_location("check_runner_kernel", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        dirty = subprocess.run(
            ["git", "-C", str(ROOT), "status", "--porcelain", "--", "rootfs-config.toml", "kernel"],
            capture_output=True, text=True, check=True,
        ).stdout
        for config_arch in ("amd64", "arm64"):
            archived = module.release_at(ROOT, "HEAD", config_arch, {})
            self.assertIsNotNone(archived, config_arch)
            if not dirty:
                self.assertEqual(archived, module.host_kernel_release(ROOT, config_arch))


if __name__ == "__main__":
    unittest.main()
