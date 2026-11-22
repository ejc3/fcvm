#!/usr/bin/env python3
"""Select the lane image a stack's Lambda tiles run, so the image and its rollback stay in the repository.

    release.py select   --stack stage --digest sha256:<new> --running sha256:<now>|none
    release.py promote  --running sha256:<now>|none     prod takes the digest staging selected
    release.py rollback --stack prod --running sha256:<now>
    release.py show

The lane repository (fcvm-fleet/lambda-tile in us-east-1, ejc3/aws fcvm-fleet-lambda.tf) is
shared by both stacks. Its lifecycle policy expires every image outside the newest five unless the
image carries a protected tag, `keep-<stack>-current` or `keep-<stack>-previous` (LIFECYCLE below).
Those four tags are the only mutable tags in the repository, and this script is what moves them;
a push names an image `rev-<commit>` or any other tag outside `keep-`.

A stack's coordinator runs one digest, its `--lambda-image <repository>@<digest>`, and every
function it creates runs that digest (the coordinator deletes its functions at start and at
shutdown). `--running` is that digest. A selection moves `keep-<stack>-previous` to it, then
`keep-<stack>-current` to the new one, so both the running digest and the selected one carry a
protected tag at every moment, and after the coordinator is configured with the new digest the
one it replaced is the rollback target. A rollback selects `keep-<stack>-previous` and keeps the
digest it replaces as previous. Selections of one stack run one at a time: this script takes a
per-stack lock and fails when another holds it, and reads both tags back after moving them.
"""
import argparse
import contextlib
import fcntl
import json
import os
import re
import subprocess
import tempfile
from dataclasses import dataclass

REGION = "us-east-1"
REPOSITORY = "fcvm-fleet/lambda-tile"
STACKS = ("stage", "prod")
PROTECTED = "keep-*"
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
# Kept with the bytes and media type that were pushed, so a tag put on them names the same digest.
MEDIA_TYPES = [
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.index.v1+json",
]

# The repository's lifecycle policy, as ejc3/aws fcvm-fleet-lambda.tf sets it. An image a rule's
# tags select cannot be expired by a lower-priority rule, so rule 1 (which keeps up to 100 images,
# and at most four carry a protected tag) keeps every protected image; rule 2 counts every image
# and expires the ones outside the newest five that no protected tag holds.
LIFECYCLE = {"rules": [
    {"rulePriority": 1, "description": "never an image a stack runs or can roll back to",
     "selection": {"tagStatus": "tagged", "tagPatternList": [PROTECTED],
                   "countType": "imageCountMoreThan", "countNumber": 100},
     "action": {"type": "expire"}},
    {"rulePriority": 2, "description": "keep the newest 5 images",
     "selection": {"tagStatus": "any", "countType": "imageCountMoreThan", "countNumber": 5},
     "action": {"type": "expire"}},
]}


def current(stack):
    return f"keep-{stack}-current"


def previous(stack):
    return f"keep-{stack}-previous"


@dataclass(frozen=True)
class Image:
    digest: str
    manifest: str
    media_type: str


class CommandFailed(RuntimeError):
    def __init__(self, code):
        super().__init__("aws ecr failed" + (f" ({code})" if code else ""))
        self.code = code


AWS_ERROR = re.compile(r"An error occurred \(([A-Za-z0-9]{1,64})\)")


class Ecr:
    """The repository through the AWS CLI: read an image by tag or digest, put a tag on one."""

    def __init__(self, repository=REPOSITORY, region=REGION):
        self.repository, self.region = repository, region

    def aws(self, args, body=None):
        result = subprocess.run(["aws", "ecr", *args, "--region", self.region, "--output", "json"],
                                input=body, capture_output=True, text=True)
        if result.returncode:
            found = AWS_ERROR.search(result.stderr or "")
            raise CommandFailed(found.group(1) if found else None)
        return json.loads(result.stdout)

    def get(self, image_id):
        out = self.aws(["batch-get-image", "--repository-name", self.repository, "--image-ids", image_id,
                        "--accepted-media-types", *MEDIA_TYPES])
        if not out.get("images"):
            if all(f.get("failureCode") in ("ImageNotFound", "ImageTagDoesNotExist") for f in out.get("failures", [])):
                return None
            raise RuntimeError("batch-get-image failed: " + ",".join(str(f.get("failureCode")) for f in out["failures"]))
        image = out["images"][0]
        return Image(image["imageId"]["imageDigest"], image["imageManifest"], image["imageManifestMediaType"])

    def tagged(self, tag):
        return self.get("imageTag=" + tag)

    def image(self, digest):
        found = self.get("imageDigest=" + digest)
        if found is None:
            raise LookupError(f"no image {digest} in {self.repository}")
        return found

    def put(self, tag, image):
        body = json.dumps({"repositoryName": self.repository, "imageTag": tag, "imageDigest": image.digest,
                           "imageManifest": image.manifest, "imageManifestMediaType": image.media_type})
        try:
            out = self.aws(["put-image", "--cli-input-json", "file:///dev/stdin"], body=body)
        except CommandFailed as error:
            # The tag is on this image already.
            if error.code == "ImageAlreadyExistsException":
                return
            raise
        if out["image"]["imageId"]["imageDigest"] != image.digest:
            raise RuntimeError(f"{tag} landed on another digest")


def digest(value, *, none=False):
    if none and value == "none":
        return None
    if not DIGEST.fullmatch(value or ""):
        raise ValueError(f"not an image digest: {value!r}")
    return value


@contextlib.contextmanager
def locked(*stacks):
    """Hold each stack's selection lock, in a fixed order; fail at once if another selection holds one."""
    with contextlib.ExitStack() as held:
        for stack in sorted(stacks):
            f = held.enter_context(open(os.path.join(tempfile.gettempdir(), f"fcvm-lambda-tile-{stack}.lock"), "a"))
            try:
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise RuntimeError(f"another selection for {stack} is running") from None
        yield


def tags(ecr, stack):
    return {name: (found.digest if found else None)
            for name in (current(stack), previous(stack)) for found in [ecr.tagged(name)]}


def move(ecr, stack, new, running):
    """Protect `running` as the stack's previous image, then make `new` its current one."""
    if stack not in STACKS:
        raise ValueError(f"unknown stack {stack!r}")
    image = ecr.image(new)
    if running is not None:
        ecr.put(previous(stack), ecr.image(running))
    ecr.put(current(stack), image)
    now = tags(ecr, stack)
    if now[current(stack)] != new or (running is not None and now[previous(stack)] != running):
        raise RuntimeError(f"{stack}'s tags changed during the selection: another selection ran")
    return now


def select(ecr, stack, new, running):
    with locked(stack):
        return move(ecr, stack, new, running)


def promote(ecr, running):
    with locked("stage", "prod"):
        staged = ecr.tagged(current("stage"))
        if staged is None:
            raise LookupError("staging has selected no lane image")
        return move(ecr, "prod", staged.digest, running)


def rollback(ecr, stack, running):
    with locked(stack):
        now = tags(ecr, stack)
        if now[current(stack)] != running:
            raise RuntimeError(f"{stack} does not run its selected image: roll back by selecting the one it should run")
        target = now[previous(stack)]
        if target is None or target == running:
            raise LookupError(f"{stack} has no other image to roll back to")
        return move(ecr, stack, target, running)


def show(ecr):
    return {stack: tags(ecr, stack) for stack in STACKS}


def arguments(argv=None):
    parser = argparse.ArgumentParser(description="Move the lane image's protected tags (module docstring).")
    sub = parser.add_subparsers(dest="action", required=True)
    s = sub.add_parser("select")
    s.add_argument("--stack", choices=STACKS, required=True)
    s.add_argument("--digest", required=True)
    s.add_argument("--running", required=True, help="the digest the stack's coordinator runs now, or none")
    p = sub.add_parser("promote")
    p.add_argument("--running", required=True, help="the digest prod's coordinator runs now, or none")
    r = sub.add_parser("rollback")
    r.add_argument("--stack", choices=STACKS, required=True)
    r.add_argument("--running", required=True, help="the digest the stack's coordinator runs now")
    sub.add_parser("show")
    return parser.parse_args(argv)


def run(argv=None, ecr=None):
    args = arguments(argv)
    ecr = ecr or Ecr()
    if args.action == "select":
        result = select(ecr, args.stack, digest(args.digest), digest(args.running, none=True))
    elif args.action == "promote":
        result = promote(ecr, digest(args.running, none=True))
    elif args.action == "rollback":
        result = rollback(ecr, args.stack, digest(args.running))
    else:
        result = show(ecr)
    print(json.dumps(result, sort_keys=True))
    return result


if __name__ == "__main__":
    run()
