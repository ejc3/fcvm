#!/usr/bin/env python3
"""Fail a job whose runner does not run a host kernel this repository builds.

usage: check-runner-kernel.py [--current-only]
       check-runner-kernel.py --print-expected [--machine MACHINE]

A runner boots the image scripts/build-ami.sh publishes, whose kernel is the host
kernel of [kernel_profiles.nested.<arch>.host_kernel] in rootfs-config.toml. That
kernel's release is `<kernel_version>-fcvm-<12 hex digits>`, the digits a hash of the
table's kernel_version and build_inputs (compute_host_kernel_sha in
src/setup/kernel.rs, which the_runner_kernel_check_names_the_host_kernel_setup_installs
compares with this script). A patch added to the host set therefore changes the release
this script expects, with no edit here.

Verdicts, each named in what the script prints:

  exit 0  the running kernel is the build this checkout's inputs give.
  exit 0  with a notice: it is the build main's inputs give, and this checkout gives
          another (it changes the host kernel inputs, or is behind main). Images are
          built from main only, so no runner can run this checkout's build.
  exit 0  with a warning: it is a build main gave at some moment in the last
          ROLLOUT_HOURS hours. After a host kernel change merges, the image with the new
          build has to be built, approved, copied and booted by replacement runners,
          and until then every runner runs the build before it.
  exit 1  any other kernel: a distribution kernel, or an fcvm build main stopped giving
          more than ROLLOUT_HOURS hours ago (a stale runner image).
  exit 2  the check could not evaluate: no readable config, a release or machine it
          cannot read, no git, or main's history could not be fetched.

--current-only accepts the first verdict alone, for a job whose results must not come
from a build that lacks a change (a benchmark).

The second and third verdicts need main's recent first-parent history, which a CI
checkout (one commit deep) does not hold. The script fetches it from `origin` into
refs/runner-kernel-check/main, only when the running kernel is an fcvm build other than
the one the checkout gives. In a shallow repository it deepens that one ref as far as
the window needs; in a full one it fetches without a depth, which would otherwise make
the repository shallow.

--release, --machine, --now and --root replace `uname -r`, `uname -m`, the clock and
the checkout, for the tests (scripts/test-check-runner-kernel.py) and for asking what a
given kernel would get. A workflow step passes none of them:
every_self_hosted_job_checks_the_runner_kernel fails if one does.
"""

import argparse
import datetime
import glob
import hashlib
import io
import os
import pathlib
import re
import subprocess
import sys
import tarfile
import tempfile
import time

try:
    import tomllib
except ImportError:  # Python before 3.11
    tomllib = None

ROOT = pathlib.Path(__file__).resolve().parent.parent

# The profile whose host kernel the runner image installs (scripts/build-ami.sh:
# `fcvm setup --kernel-profile nested ... --install-host-kernel`).
PROFILE = "nested"

# `uname -m`, and rootfs-config.toml's name for the same architecture.
CONFIG_ARCH = {"x86_64": "amd64", "aarch64": "arm64"}

# The only branch runner images are built from (build-runner-ami.yml's job condition).
IMAGE_BRANCH = "main"
HISTORY_REF = f"refs/runner-kernel-check/{IMAGE_BRANCH}"
IMAGE_WORKFLOW = ".github/workflows/build-runner-ami.yml"

# How long a build stays accepted after main stops giving it. A runner lives at most
# 13 h 30 min (the fleet's hard ceiling, ejc3/aws runner-autoscale.tf), and the image
# pipeline (Build Kernels, the approved Build Runner AMI run, the hourly copy to the
# runner region) took about 4 h 20 min from merge to the last replaced runner on 2026-11-25.
ROLLOUT_HOURS = 24

# Depths tried, in order, when main's history has to be fetched into a shallow
# repository. main had 35 first-parent commits in the 24 hours before this was written.
FETCH_DEPTHS = (1, 64, 512, 4096)

# What `uname -r` prints: three numbers, then nothing or a suffix.
RELEASE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)([-+].*)?")
# The release of a host kernel fcvm built (LOCALVERSION in src/setup/kernel.rs).
FCVM_BUILD = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+-fcvm-[0-9a-f]{12}")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")


class IdentityError(Exception):
    """The host kernel release of a tree cannot be computed."""


class Blocked(Exception):
    """The check cannot evaluate."""


def host_kernel_release(root, config_arch):
    """The release of the host kernel `root`'s inputs give for one architecture."""
    try:
        with open(root / "rootfs-config.toml", "rb") as handle:
            table = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise IdentityError(f"cannot read rootfs-config.toml: {error}") from error
    section = f"[kernel_profiles.{PROFILE}.{config_arch}.host_kernel]"
    for key in ("kernel_profiles", PROFILE, config_arch, "host_kernel"):
        table = table.get(key) if isinstance(table, dict) else None
    if not isinstance(table, dict):
        raise IdentityError(f"rootfs-config.toml has no {section}")
    version = table.get("kernel_version")
    if not (isinstance(version, str) and VERSION.fullmatch(version)):
        raise IdentityError(f"{section} kernel_version is {version!r}, not a kernel version")
    patterns = table.get("build_inputs")
    if not (
        isinstance(patterns, list) and patterns and all(isinstance(p, str) for p in patterns)
    ):
        raise IdentityError(f"{section} build_inputs is {patterns!r}, not a list of patterns")

    digest = hashlib.sha256(version.encode())
    for pattern in patterns:
        # The history path below materialises kernel/ and nothing else, and Python's glob
        # reads `**` differently from the Rust crate's.
        parts = pathlib.PurePosixPath(pattern).parts
        if parts[:1] != ("kernel",) or ".." in parts or "**" in pattern:
            raise IdentityError(
                f"{section} build_inputs pattern '{pattern}' is outside kernel/ or recursive, "
                "which this script does not mirror; teach it and its test first"
            )
        # include_hidden: the Rust glob crate matches a dot-prefixed file with `*`.
        matches = glob.glob(pattern, root_dir=root, include_hidden=True)
        if not matches:
            raise IdentityError(f"{section} build_inputs pattern '{pattern}' matched no files")
        # A *.vm.patch is applied to guest kernels only. Paths sort by component, as
        # Rust's PathBuf does.
        for path in sorted(pathlib.PurePath(m) for m in matches if not m.endswith(".vm.patch")):
            try:
                digest.update((root / path).read_bytes())
            except OSError as error:
                raise IdentityError(f"cannot read build input {path}: {error}") from error
    return f"{version}-fcvm-{digest.hexdigest()[:12]}"


def git(root, *args, binary=False):
    """Run git in `root` and return its output; any failure blocks the check."""
    command = ["git", "-C", str(root), *args]
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            timeout=300,
            env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
        )
    except FileNotFoundError as error:
        raise Blocked("git is not on PATH") from error
    except subprocess.TimeoutExpired as error:
        raise Blocked(f"`git {args[0]}` did not finish in 300 s") from error
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip().splitlines()
        raise Blocked(f"`git {' '.join(args)}` failed: {detail[-1] if detail else 'no output'}")
    return result.stdout if binary else result.stdout.decode().strip()


def release_at(root, commit, config_arch, cache):
    """The host kernel release a commit's inputs give, or None when they give none."""
    try:
        key = git(root, "rev-parse", f"{commit}:rootfs-config.toml", f"{commit}:kernel")
    except Blocked:
        return None  # a commit from before the config or the kernel directory existed
    if key not in cache:
        archive = git(root, "archive", "--format=tar", commit, "rootfs-config.toml", "kernel",
                      binary=True)
        with tempfile.TemporaryDirectory(prefix="runner-kernel-") as tree:
            with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                # The host patch sets link patches of other sets inside kernel/, which the
                # data filter keeps and a link out of the tree would not survive.
                tar.extractall(tree, filter="data")
            try:
                cache[key] = host_kernel_release(pathlib.Path(tree), config_arch)
            except IdentityError:
                cache[key] = None
    return cache[key]


def image_branch_commits(root, start):
    """main's first-parent commits as (hash, commit time), newest first, through the
    first one at or before `start`: every state main was in from `start` to now."""
    shallow = git(root, "rev-parse", "--is-shallow-repository") == "true"
    tip = None
    yielded = 0
    for depth in FETCH_DEPTHS if shallow else (None,):
        depth_args = [] if depth is None else [f"--depth={depth}"]
        git(root, "fetch", "--quiet", "--no-tags", *depth_args, "origin",
            f"+refs/heads/{IMAGE_BRANCH}:{HISTORY_REF}")
        if tip is None:
            # main may move between two fetches; the walk keeps the first tip it saw.
            tip = git(root, "rev-parse", "--verify", HISTORY_REF)
        commits = [
            (line.split()[0], int(line.split()[1]))
            for line in git(root, "log", "--first-parent", "--format=%H %ct", tip).splitlines()
        ]
        for commit, when in commits[yielded:]:
            yield commit, when
            yielded += 1
            if when <= start:
                return
        boundary = set()
        if shallow:
            shallow_file = pathlib.Path(git(root, "rev-parse", "--git-path", "shallow"))
            if not shallow_file.is_absolute():
                shallow_file = root / shallow_file
            try:
                boundary = set(shallow_file.read_text().split())
            except OSError as error:
                raise Blocked(f"cannot read {shallow_file}: {error}") from error
        if commits[-1][0] not in boundary:
            return  # the branch's first commit: its whole history is younger than the window
    raise Blocked(
        f"{IMAGE_BRANCH} has more than {FETCH_DEPTHS[-1]} commits in the last "
        f"{ROLLOUT_HOURS} hours, more history than this check fetches"
    )


def utc(when):
    return datetime.datetime.fromtimestamp(when, datetime.timezone.utc).strftime(
        "%Y-%m-%d %H:%M UTC"
    )


def image_runs():
    server = os.environ.get("GITHUB_SERVER_URL")
    repository = os.environ.get("GITHUB_REPOSITORY")
    if server and repository:
        return f"{server}/{repository}/actions/workflows/{pathlib.PurePath(IMAGE_WORKFLOW).name}"
    return IMAGE_WORKFLOW


def verdict(release, machine, root, now, current_only):
    """(exit status, lines to print). Raises Blocked when it cannot evaluate."""
    config_arch = CONFIG_ARCH.get(machine)
    if config_arch is None:
        raise Blocked(
            f"machine '{machine}' has no host kernel in rootfs-config.toml "
            f"(known: {', '.join(sorted(CONFIG_ARCH))})"
        )
    try:
        expected = host_kernel_release(root, config_arch)
    except IdentityError as error:
        raise Blocked(
            f"cannot compute the host kernel this checkout builds for {machine}: {error}"
        ) from error

    if release == expected:
        return 0, [f"runner kernel {release} is the host kernel this checkout builds for {machine}"]
    if not RELEASE.fullmatch(release):
        raise Blocked(f"'{release}' is not a kernel release as `uname -r` prints one")
    if not FCVM_BUILD.fullmatch(release):
        return 1, [
            f"::error title=Runner kernel::runner kernel {release} is not an fcvm host kernel",
            f"ERROR: runner kernel {release} is not a host kernel fcvm built: those are named",
            f"       <version>-fcvm-<12 hex digits>, and this checkout builds {expected} for",
            f"       {machine}. The runner did not boot the image {IMAGE_WORKFLOW}",
            "       publishes. A rerun on this runner fails the same way: replace the runner,",
            "       or fix the launcher's image selection.",
        ]
    if current_only:
        return 1, [
            f"::error title=Runner kernel::runner kernel {release} is not {expected}",
            f"ERROR: runner kernel {release} is an fcvm host kernel, and not the one this",
            f"       checkout builds for {machine} ({expected}). This job accepts no other",
            "       build (--current-only). Rerun it once the runners boot an image with",
            f"       {expected}: {image_runs()}",
        ]

    start = now - ROLLOUT_HOURS * 3600
    cache = {}
    newer = None
    given = []
    try:
        for commit, when in image_branch_commits(root, start):
            built = release_at(root, commit, config_arch, cache)
            if built == release and newer is None:
                return 0, [
                    f"::notice title=Runner kernel::runner kernel {release} is the host kernel "
                    f"{IMAGE_BRANCH} builds; this checkout builds {expected}, which no runner runs",
                    f"runner kernel {release} is the host kernel {IMAGE_BRANCH} builds for {machine}.",
                    f"This checkout builds {expected}: it changes the host kernel inputs, or is",
                    f"behind {IMAGE_BRANCH}. Runner images are built from {IMAGE_BRANCH} only, so this job",
                    f"runs on {release} and does not test this checkout's host kernel.",
                ]
            if built == release:
                changed, changed_at = newer
                deadline = changed_at + ROLLOUT_HOURS * 3600
                current = given[0] if given else expected
                return 0, [
                    f"::warning title=Runner kernel is the previous build::runner kernel {release} "
                    f"lacks a host kernel change {IMAGE_BRANCH} made at {utc(changed_at)}; accepted "
                    f"until {utc(deadline)}",
                    f"runner kernel {release} is a host kernel {IMAGE_BRANCH} built for {machine} until",
                    f"{utc(changed_at)} (commit {changed[:12]}). {IMAGE_BRANCH} now builds {current}.",
                    "Runners keep the previous build until an image with the new one is built,",
                    f"approved and booted, and this check accepts it for {ROLLOUT_HOURS} hours: it fails",
                    f"jobs on {release} from {utc(deadline)}.",
                    "A failure in this job may be what the host kernel change fixes.",
                    f"Image builds: {image_runs()}",
                ]
            if built is not None and built not in given:
                given.append(built)
            newer = (commit, when)
    except Blocked as error:
        raise Blocked(
            f"runner kernel {release} is not the build this checkout gives ({expected}), and "
            f"whether {IMAGE_BRANCH} gave it in the last {ROLLOUT_HOURS} hours is unknown: {error}"
        ) from error

    return 1, [
        f"::error title=Runner kernel::runner kernel {release} is a stale fcvm host kernel",
        f"ERROR: runner kernel {release} is an fcvm host kernel that neither this checkout",
        f"       ({expected}) nor {IMAGE_BRANCH} at any moment in the last {ROLLOUT_HOURS} hours",
        f"       ({', '.join(given) or 'no build'}) gives for {machine}. The runner image is stale:",
        "       it lacks every host kernel change since it was built. Check that the newest",
        f"       Build Runner AMI run for {IMAGE_BRANCH} was approved and succeeded, then that the",
        f"       runners were replaced: {image_runs()}",
    ]


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--current-only", action="store_true")
    parser.add_argument("--print-expected", action="store_true")
    parser.add_argument("--release")
    parser.add_argument("--machine")
    parser.add_argument("--now", type=int)
    parser.add_argument("--root", type=pathlib.Path, default=ROOT)
    args = parser.parse_args(argv[1:])

    try:
        if tomllib is None:
            raise Blocked(
                "this Python has no tomllib (3.11 or later is needed), so rootfs-config.toml "
                "cannot be read"
            )
        machine = os.uname().machine if args.machine is None else args.machine
        if args.print_expected:
            config_arch = CONFIG_ARCH.get(machine)
            if config_arch is None:
                raise Blocked(f"machine '{machine}' has no host kernel in rootfs-config.toml")
            try:
                print(host_kernel_release(args.root, config_arch))
            except IdentityError as error:
                raise Blocked(str(error)) from error
            return 0
        release = os.uname().release if args.release is None else args.release
        now = int(time.time()) if args.now is None else args.now
        status, lines = verdict(release, machine, args.root, now, args.current_only)
    except Blocked as error:
        print(f"::error title=Runner kernel check blocked::{error}", flush=True)
        print(f"BLOCKED: the runner kernel check cannot evaluate: {error}", file=sys.stderr)
        return 2
    # The annotation line goes to stdout, where the runner reads it; an error's detail
    # goes to stderr.
    print(lines[0], flush=True)
    for line in lines[1:]:
        print(line, file=sys.stderr if status else sys.stdout)
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv))
