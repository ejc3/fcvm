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
import contextlib
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import types
import unittest
from unittest import mock

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

    def git(self, *args, when=None, authored=None):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
        if when is not None:
            env["GIT_COMMITTER_DATE"] = f"@{when} +0000"
            env["GIT_AUTHOR_DATE"] = f"@{when if authored is None else authored} +0000"
        return subprocess.run(
            ["git", "-C", str(self.path), "-c", "user.name=test", "-c", "user.email=test@example.com",
             "-c", "commit.gpgsign=false", *args],
            check=True, capture_output=True, text=True, env=env,
        ).stdout.strip()

    def write(self, name, text):
        path = self.path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, hours_ago, message="change", authored_hours_ago=None, **files):
        """Commit `files` (path -> text) to the current branch, committed `hours_ago` before
        NOW and authored then too, or `authored_hours_ago` before NOW."""
        for name, text in files.items():
            self.write(name, text)
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "-m", message,
                 when=NOW - int(hours_ago * HOUR),
                 authored=None if authored_hours_ago is None else NOW - int(authored_hours_ago * HOUR))
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


def load_script():
    """The script as a module, for the cases that replace one of its parts."""
    spec = importlib.util.spec_from_file_location("check_runner_kernel", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fake_git(directory, case_arms):
    """A `git` in `directory` that runs `case_arms` (the arms of a shell `case` on the git
    subcommand) and hands every other command to the real git. Returns a PATH with it first."""
    directory.mkdir()
    script = directory / "git"
    script.write_text(
        f'#!/bin/bash\nreal={shutil.which("git")}\nfor argument in "$@"; do\n  case "$argument" in\n'
        f'{textwrap.indent(textwrap.dedent(case_arms).strip(), "    ")}\n  esac\ndone\nexec "$real" "$@"\n'
    )
    script.chmod(0o755)
    return f"{directory}:{os.environ['PATH']}"


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

    def test_a_rebased_change_counts_from_when_it_was_committed_to_main(self):
        # A rebase or squash merge: one commit on main, authored three days ago, committed
        # 3 hours ago.
        self.origin.commit(72, "first build")
        before = expected(self.origin.path)
        self.origin.commit(3, "x86 host patch, rebased onto main", authored_hours_ago=70,
                           **{X86_ONLY: "pv kick\n"})
        work = self.origin.checkout(self.tmp / "work")
        status, out, err = check(work, before)
        self.assertEqual(status, 0, out + err)
        self.assertIn(f"accepted until {utc(NOW + 21 * HOUR)}", out)
        self.assertEqual(check(work, before, now=NOW + 21 * HOUR)[0], 1)

    def test_the_fetch_writes_its_own_ref_and_no_other(self):
        self.origin.commit(72, "first build")
        before = expected(self.origin.path)
        self.origin.commit(4, "x86 host patch", **{X86_ONLY: "pv kick\n"})
        work = self.origin.checkout(self.tmp / "work")
        # main moves and gains a tag after the checkout was made, as it does under a job.
        self.origin.commit(1, "unrelated", README="later\n")
        self.origin.git("tag", "v-later")

        def refs():
            listed = subprocess.run(
                ["git", "-C", str(work), "for-each-ref", "--format=%(refname) %(objectname)"],
                capture_output=True, text=True, check=True,
            ).stdout.split("\n")
            head = subprocess.run(["git", "-C", str(work), "rev-parse", "HEAD"],
                                  capture_output=True, text=True, check=True).stdout.strip()
            return {line.split()[0]: line.split()[1] for line in listed if line} | {"HEAD": head}

        at_checkout = refs()
        self.assertIn("refs/remotes/origin/main", at_checkout)
        # A FETCH_HEAD the caller has not read yet.
        fetch_head = work / ".git/FETCH_HEAD"
        fetch_head.write_text("0123456789abcdef0123456789abcdef01234567\t\tbranch 'topic' of example\n")
        self.assertEqual(check(work, before)[0], 0)
        self.assertEqual(fetch_head.read_text(),
                         "0123456789abcdef0123456789abcdef01234567\t\tbranch 'topic' of example\n")
        after = refs()
        tip = self.origin.git("rev-parse", "main")
        self.assertEqual(after.pop("refs/runner-kernel-check/main"), tip)
        self.assertEqual(after, at_checkout)
        self.assertNotEqual(at_checkout["refs/remotes/origin/main"], tip)

    def test_history_deeper_than_the_first_fetch_is_fetched(self):
        # 80 commits after the change: more than the 64 the second fetch asks for.
        before, _, work = self.rollout(changed_hours_ago=23, later_commits=80)
        status, out, err = check(work, before)
        self.assertEqual(status, 0, out + err)
        self.assertIn(f"accepted until {utc(NOW + 1 * HOUR)}", out)

    def test_more_history_than_the_check_fetches_blocks(self):
        before, _, work = self.rollout(changed_hours_ago=23, later_commits=5)
        module = load_script()
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
        self.assertIn("is not tested by this job", out.splitlines()[0])
        self.assertIn("`make install-host-kernel`", out)
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


    def test_a_failed_git_command_keeps_its_last_lines(self):
        # git's reason is often on the lines before its last one.
        before, _, work = self.rollout(changed_hours_ago=4)
        path = fake_git(self.tmp / "fake-bin", """
            fetch) printf 'one\\ntwo\\nerror: RPC failed; HTTP 503\\n\\nfatal: the remote end hung up\\nfatal: early EOF\\n' >&2; exit 128 ;;
        """)
        self.blocked(work, r"is unknown: `git fetch .*` failed: two \| error: RPC failed; HTTP 503 \| "
                           r"fatal: the remote end hung up \| fatal: early EOF$",
                     release=before, path=path)

    def test_a_history_commit_git_cannot_read_blocks(self):
        # The stale-image verdict blames the runner image. A commit git could not list must
        # not count as "this commit gives no build" on the way to it.
        before, after, work = self.rollout(changed_hours_ago=4)
        path = fake_git(self.tmp / "fake-bin", """
            ls-tree) echo 'fatal: unable to read tree' >&2; exit 128 ;;
        """)
        self.blocked(work, f"runner kernel {before} is not the build this checkout gives "
                           rf"\({re.escape(after)}\), and whether main gave it in the last 24 hours "
                           "is unknown: `git ls-tree .*` failed: fatal: unable to read tree",
                     release=before, path=path)

    def test_a_history_commit_whose_inputs_cannot_be_read_blocks(self):
        # main's tip has a config that does not parse, and its parent gives the running
        # kernel. What main builds now cannot be computed, so the kernel is neither "the
        # previous build" nor "a stale image": the check cannot say.
        self.origin.commit(72, "first build")
        on_parent = expected(self.origin.path)
        self.origin.git("checkout", "--quiet", "-b", "change")
        self.origin.commit(40, "a host patch of the pull request's own", **{X86_ONLY: "other\n"})
        self.origin.git("checkout", "--quiet", "main")
        broken = self.origin.commit(2, "a config that does not parse",
                                    **{"rootfs-config.toml": "[kernel_profiles.nested\n"})
        wanted = (f"is unknown: the host kernel main gave at commit {broken[:12]} cannot be "
                  "computed: cannot read rootfs-config.toml")
        work = self.origin.checkout(self.tmp / "work", branch="change")
        self.blocked(work, wanted, release=on_parent)
        # Deeper in the window: main's tip is readable again, and the unreadable commit is
        # between it and the commit that gave the running kernel.
        self.origin.commit(1, "the config again", **{"rootfs-config.toml": CONFIG,
                                                     "kernel/host-x86.conf": "CONFIG_ENA_ETHERNET=m\n"})
        tip = self.origin.checkout(self.tmp / "tip")
        self.blocked(tip, wanted, release=on_parent)
        # The build the readable tip gives is still judged without its history.
        self.assertEqual(check(tip, expected(tip))[0], 0)

    def test_a_history_commit_without_the_host_table_gives_no_build(self):
        # A readable config with no host kernel for this architecture is "no build", not an
        # error: the walk goes on to the commit before it.
        self.origin.commit(72, "first build")
        before = expected(self.origin.path)
        self.origin.commit(5, "no x86 host kernel",
                           **{"rootfs-config.toml": CONFIG.replace("nested.amd64.host_kernel", "other")})
        self.origin.commit(3, "the table again, with a patch",
                           **{"rootfs-config.toml": CONFIG, X86_ONLY: "pv kick\n"})
        work = self.origin.checkout(self.tmp / "work")
        status, out, err = check(work, before)
        self.assertEqual(status, 0, out + err)
        self.assertIn(f"accepted until {utc(NOW + 19 * HOUR)}", out)

    def test_a_history_commit_from_before_the_config_gives_no_build(self):
        # main's first commit has no config: it is walked, gives no build, and is no error.
        self.origin.write("README", "first\n")
        self.origin.git("add", "README")
        self.origin.git("commit", "--quiet", "-m", "before the config", when=NOW - 10 * HOUR)
        self.origin.commit(8, "the config and the host sets")
        work = self.origin.checkout(self.tmp / "work")
        status, _, err = check(work, "7.3.3-fcvm-0123456789ab")
        self.assertEqual(status, 1, err)
        self.assertIn("The runner image is stale", err)
        # Deepening reached the first commit, so the checkout is a full repository now, and
        # a second run reads it as one.
        self.assertFalse((work / ".git/shallow").exists())
        self.assertEqual(check(work, "7.3.3-fcvm-0123456789ab")[0], 1)

    def no_process(self, pattern):
        """Fail unless pgrep ran and found no process whose command line is `pattern`."""
        found = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True, check=False)
        self.assertEqual(found.returncode, 1,
                         f"pgrep -f {pattern!r}: status {found.returncode}, found "
                         f"{found.stdout.split()}, stderr {found.stderr!r}")

    def test_the_process_probe_fails_when_it_cannot_run(self):
        # pgrep prints nothing both when it finds nothing (status 1) and when it fails
        # (status 2, here a pattern that is not a regular expression).
        with self.assertRaisesRegex(AssertionError, "status 2"):
            self.no_process("(")

    def test_a_fetch_that_hangs_is_stopped_with_sigterm_first(self):
        # git removes the lock files it holds when it gets SIGTERM, and not when it gets
        # SIGKILL. The stand-in does the same, so a script that went straight to SIGKILL
        # would leave the lock.
        before, _, work = self.rollout(changed_hours_ago=4)
        lock = work / ".git/shallow.lock"
        nap = f"60.{os.getpid()}"  # a duration no other process on the host sleeps for
        path = fake_git(self.tmp / "fake-bin", f"""
            fetch) trap 'rm -f {lock}; exit 143' TERM; touch {lock}; sleep {nap} & wait ;;
        """)
        module = load_script()
        module.GIT_TIMEOUT = 1
        started = time.monotonic()
        with mock.patch.dict(os.environ, {"PATH": path}):
            with self.assertRaisesRegex(module.Blocked,
                                        "`git fetch` did not finish in 1 s and was stopped$"):
                module.verdict(before, "x86_64", work, NOW, False)
        self.assertLess(time.monotonic() - started, 30)
        self.assertFalse(lock.exists(), "the stopped fetch kept its lock: it was not sent SIGTERM")
        # The fetch's children are gone too: the signal went to its process group.
        self.no_process(f"^sleep {nap}$")

    def test_a_fetch_that_ignores_sigterm_is_killed_and_no_lock_is_deleted(self):
        before, _, work = self.rollout(changed_hours_ago=4)
        lock = work / ".git/shallow.lock"
        nap = f"61.{os.getpid()}"
        # The stand-in does not start the sleep in the background: bash runs a trap only
        # between commands, so SIGTERM waits behind the sleep and only SIGKILL ends it.
        path = fake_git(self.tmp / "fake-bin", f"""
            fetch) trap '' TERM; touch {lock}; sleep {nap} ;;
        """)
        module = load_script()
        module.GIT_TIMEOUT = 1
        module.GIT_KILL_GRACE = 1
        with mock.patch.dict(os.environ, {"PATH": path}):
            with self.assertRaisesRegex(module.Blocked, "was killed, and may have left a lock file"):
                module.verdict(before, "x86_64", work, NOW, False)
        self.no_process(f"^sleep {nap}$")
        # Whose lock that is cannot be known from the file, so it is left where it is.
        self.assertTrue(lock.exists())

    def test_a_python_without_the_tar_filter_blocks(self):
        before, _, work = self.rollout(changed_hours_ago=4)
        module = load_script()
        module.tarfile = types.SimpleNamespace(open=module.tarfile.open)
        with self.assertRaisesRegex(module.Blocked, "tarfile has no extraction filter"):
            module.verdict(before, "x86_64", work, NOW, False)

    def test_an_error_the_script_does_not_expect_blocks(self):
        self.origin.commit(1)
        module = load_script()

        def broken(*_):
            raise RuntimeError("not a verdict")

        module.verdict = broken
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = module.main(["check-runner-kernel.py", "--root", str(self.origin.path),
                                  "--release", "7.3.3-fcvm-0123456789ab", "--machine", "x86_64"])
        self.assertEqual(status, 2)
        self.assertEqual(out.getvalue(), "::error title=Runner kernel check blocked::unexpected "
                                         "RuntimeError: not a verdict\n")
        self.assertIn("BLOCKED: the runner kernel check cannot evaluate: unexpected RuntimeError",
                      err.getvalue())
        self.assertIn("Traceback", err.getvalue())


class ThisRepository(unittest.TestCase):
    """What the script assumes about the repository it is checked out in."""

    def test_both_runner_architectures_have_a_release_and_it_passes(self):
        for machine in ("x86_64", "aarch64"):
            release = expected(ROOT, machine)
            self.assertRegex(release, r"^[0-9]+\.[0-9]+\.[0-9]+-fcvm-[0-9a-f]{12}$")
            status, out, err = check(ROOT, release, machine)
            self.assertEqual((status, err), (0, ""), out)

    def test_the_image_installs_the_profile_and_is_built_from_the_branch_the_script_names(self):
        module = load_script()
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
        module = load_script()
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
