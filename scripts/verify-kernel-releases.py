#!/usr/bin/env python3
"""Check that every kernel Build Kernels publishes has its release asset.

usage: verify-kernel-releases.py [--list-legs] [--base-url URL]

`fcvm setup --kernel-profile <profile>` downloads
<base>/<kernel_repo>/releases/download/<tag>/<file>. Every automated consumer
passes --build-kernels, so a missing asset becomes a silent local build and the
404 goes unseen. This asks for each asset the client would download, using the
tag and file name scripts/kernel-release-identity.py derives, and names every
one that is missing.

Exit status: 0 when every asset is there, 1 when at least one is missing, 2 when
it could not check, which is never reported as missing or as present.
"""

import importlib.util
import pathlib
import sys
import time
import urllib.error
import urllib.request

# Importing the identity script must not leave a __pycache__ in the checkout.
sys.dont_write_bytecode = True

SCRIPTS = pathlib.Path(__file__).resolve().parent

# The (profile, config arch) legs kernels.yml builds and publishes.
# tests/test_default_kernel_release.rs fails when this list and the workflow
# disagree.
LEGS = [
    ("default", "arm64"),
    ("default", "amd64"),
    ("nested", "arm64"),
    ("nested", "amd64"),
    ("btrfs", "arm64"),
    ("btrfs", "amd64"),
]

# GitHub answers a release download with a redirect to the asset's storage.
FOUND = {200, 301, 302, 303, 307, 308}
ATTEMPTS = 2


def load_identity():
    spec = importlib.util.spec_from_file_location(
        "kernel_release_identity", SCRIPTS / "kernel-release-identity.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        # The redirect target is signed for GET, so a HEAD that followed it would
        # be refused. The redirect itself is the answer.
        return None


def probe(url):
    """'present', 'missing', or a string saying why it could not tell."""
    opener = urllib.request.build_opener(_NoRedirect)
    request = urllib.request.Request(url, method="HEAD")
    reason = "no attempt made"
    for attempt in range(ATTEMPTS):
        if attempt:
            time.sleep(1)
        try:
            with opener.open(request, timeout=30) as response:
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
        except (urllib.error.URLError, OSError) as error:
            reason = f"request failed: {error}"
            continue
        if status in FOUND:
            return "present"
        if status == 404:
            return "missing"
        reason = f"unexpected HTTP status {status}"
    return reason


def main(argv):
    arguments = argv[1:]
    if arguments == ["--list-legs"]:
        print("\n".join(f"{profile} {arch}" for profile, arch in LEGS))
        return 0
    base_url = "https://github.com"
    if len(arguments) == 2 and arguments[0] == "--base-url":
        base_url = arguments[1].rstrip("/")
    elif arguments:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2

    identity = load_identity()
    missing, unknown = [], []
    for profile, arch in LEGS:
        try:
            fields = identity.identity(profile, arch, check_runner=False)
        except (identity.IdentityError, OSError, ValueError) as error:
            print(f"UNKNOWN  {profile} {arch}: cannot derive its identity: {error}")
            unknown.append(f"{profile} {arch}")
            continue
        asset = f"{fields['tag']}/{fields['filename']}"
        url = f"{base_url}/{fields['repo']}/releases/download/{asset}"
        outcome = probe(url)
        if outcome == "present":
            print(f"present  {asset}")
        elif outcome == "missing":
            print(f"MISSING  {asset}")
            missing.append(asset)
        else:
            print(f"UNKNOWN  {asset}: {outcome}")
            unknown.append(asset)

    if unknown:
        print(
            f"ERROR: could not check {len(unknown)} of {len(LEGS)} assets, so this run "
            "proves nothing about them",
            file=sys.stderr,
        )
        return 2
    if missing:
        print(
            f"ERROR: {len(missing)} of {len(LEGS)} kernel release assets are missing; "
            "`fcvm setup` gets a 404 for each and builds locally or fails",
            file=sys.stderr,
        )
        return 1
    print(f"all {len(LEGS)} kernel release assets are published")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
