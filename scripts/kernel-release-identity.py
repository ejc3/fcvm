#!/usr/bin/env python3
"""Print the release identity of one guest kernel leg.

usage: kernel-release-identity.py <profile> <config_arch>

Build Kernels publishes each kernel under the tag and file name that
`fcvm setup` later downloads, so this derives them the way the client does in
src/setup/kernel.rs: compute_profile_kernel_sha_at_root for the SHA,
custom_kernel_release_tag and custom_kernel_filename for the names.
tests/test_default_kernel_release.rs runs both sides over every leg the
workflow builds, and over fixtures for the cases noted below.

On success it prints version=, arch=, sha=, tag= and filename= lines, the
format $GITHUB_OUTPUT takes. On any error it prints nothing to stdout and
exits non-zero, so a step cannot publish part of an identity.
"""

import glob
import hashlib
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

# rootfs-config.toml's architecture names, and what `uname -m` and Rust's
# std::env::consts::ARCH call the same machines.
RUNTIME_ARCH = {"arm64": "aarch64", "amd64": "x86_64"}

# The profile name and kernel version become a git tag, a file name and
# $GITHUB_OUTPUT lines.
NAME = re.compile(r"[A-Za-z0-9._-]+")


class IdentityError(Exception):
    pass


def build_inputs_sha(patterns):
    """The first 12 hex digits of the SHA-256 over a table's build inputs."""
    if not isinstance(patterns, list) or not all(isinstance(p, str) for p in patterns):
        raise IdentityError(f"build_inputs must be a list of strings, got {patterns!r}")
    if not patterns:
        # The client's constant for a table with no build_inputs. Hashing
        # nothing would give e3b0c44298fc.
        return "000000000000"
    digest = hashlib.sha256()
    hashed = 0
    for pattern in patterns:
        if "**" in pattern:
            raise IdentityError(
                f"build_inputs pattern '{pattern}' is recursive, which this script does not "
                "mirror; teach it and its test the client's behaviour first"
            )
        # include_hidden: the Rust glob crate matches a dot-prefixed file with
        # `*`, and Python's glob skips it unless asked.
        matches = glob.glob(pattern, root_dir=ROOT, include_hidden=True)
        if not matches:
            raise IdentityError(f"build_inputs pattern '{pattern}' matched no files")
        # A *.disabled file counts as a match and stays out of the hash, so a
        # patch can be switched off without the pattern failing. Patterns keep
        # their listed order; within one, paths sort by component, as Rust's
        # PathBuf does, not as strings.
        kept = sorted(
            pathlib.PurePath(match) for match in matches if not match.endswith(".disabled")
        )
        for path in kept:
            data = (ROOT / path).read_bytes()
            digest.update(data)
            hashed += len(data)
    if hashed == 0:
        raise IdentityError(f"build_inputs {patterns} matched nothing to hash")
    return digest.hexdigest()[:12]


def identity(profile_name, config_arch):
    runtime_arch = RUNTIME_ARCH.get(config_arch)
    if runtime_arch is None:
        raise IdentityError(
            f"unsupported config arch '{config_arch}', expected one of {sorted(RUNTIME_ARCH)}"
        )
    if not NAME.fullmatch(profile_name):
        raise IdentityError(f"profile name '{profile_name}' cannot be part of a release tag")

    with open(ROOT / "rootfs-config.toml", "rb") as handle:
        config = tomllib.load(handle)
    table = config.get("kernel_profiles", {}).get(profile_name, {}).get(config_arch)
    if not isinstance(table, dict):
        raise IdentityError(
            f"rootfs-config.toml has no [kernel_profiles.{profile_name}.{config_arch}]"
        )

    # Only a source release has an artifact to publish. The client downloads an
    # archive for a table with kernel_url, and a table without kernel_version
    # and kernel_repo inherits the default kernel.
    if "kernel_url" in table:
        raise IdentityError(
            f"{profile_name}.{config_arch} is URL-based; there is nothing to publish"
        )
    version = table.get("kernel_version", "")
    repo = table.get("kernel_repo", "")
    if not (isinstance(version, str) and version and isinstance(repo, str) and repo):
        raise IdentityError(
            f"{profile_name}.{config_arch} names no kernel_version and kernel_repo, so it "
            "inherits the default kernel; there is nothing to publish"
        )
    if not NAME.fullmatch(version):
        raise IdentityError(f"kernel_version '{version}' cannot be part of a release tag")

    sha = build_inputs_sha(table.get("build_inputs", []))
    manifest = table.get("kernel_sha")
    if manifest is not None:
        if not (isinstance(manifest, str) and re.fullmatch(r"[0-9a-f]{12}", manifest)):
            raise IdentityError(
                f"kernel_sha must be exactly 12 lowercase hexadecimal characters, got '{manifest}'"
            )
        if manifest != sha:
            raise IdentityError(
                f"kernel_sha '{manifest}' does not match build_inputs hash '{sha}'; update the "
                "manifest"
            )

    # The client names the artifact after the machine it runs on, so a leg that
    # lands on a runner of the other architecture would build and publish the
    # wrong kernel under this name.
    machine = subprocess.run(
        ["uname", "-m"], check=True, capture_output=True, text=True
    ).stdout.strip()
    if machine != runtime_arch:
        raise IdentityError(f"{config_arch} leg ran on {machine}, expected {runtime_arch}")

    return {
        "version": version,
        "arch": runtime_arch,
        "sha": sha,
        "tag": f"kernel-{profile_name}-{version}-{runtime_arch}-{sha}",
        "filename": f"vmlinux-{profile_name}-{version}-{runtime_arch}-{sha}.bin",
    }


def main(argv):
    if len(argv) != 3:
        print(f"usage: {pathlib.Path(argv[0]).name} <profile> <config_arch>", file=sys.stderr)
        return 2
    try:
        fields = identity(argv[1], argv[2])
    except (
        IdentityError,
        OSError,
        subprocess.CalledProcessError,
        tomllib.TOMLDecodeError,
    ) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("\n".join(f"{key}={value}" for key, value in fields.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
