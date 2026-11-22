#!/usr/bin/env python3
"""Select the lane image a stack's Lambda tiles run, so the image and its rollback stay in the repository.

    release.py select   --stack stage --digest sha256:<new>|--built --running sha256:<now>|none
    release.py promote  --running sha256:<now>|none     prod takes the digest staging selected
    release.py rollback --stack prod --running sha256:<now>
    release.py show
    release.py record   --digest sha256:<built>         the newest lane image built from main
    release.py publish  --archive lane.tar --commit C   CI: push rev-C (LAMBDA_TILE_REPO) and record it

The lane repository (fcvm-fleet/lambda-tile in us-east-1, ejc3/aws fcvm-fleet-lambda.tf) is
shared by both stacks. Its lifecycle policy expires every image outside the newest five unless the
image carries a protected tag, `keep-<stack>-current`, `keep-<stack>-previous` or `keep-<stack>-next`
(LIFECYCLE below), and `keep-built`. Those seven tags are the only mutable tags in the repository,
and this script is what moves them; a push names an image `rev-<commit>` or any other tag outside `keep-`.

A stack's coordinator runs one digest, its `--lambda-image <repository>@<digest>`, and every
function it creates runs that digest (the coordinator deletes its functions at start and at
shutdown). `--running` is that digest. A selection moves `keep-<stack>-next` to the new digest,
`keep-<stack>-previous` to the running one, then `keep-<stack>-current` to the new one (move), so
both carry a protected tag after every write, and after the coordinator is configured with the new
digest the one it replaced is the rollback target. A rollback selects `keep-<stack>-previous` and
keeps the digest it replaces as previous. An interrupted selection or rollback completes when run
again. Selections of one stack run one at a time: this script takes a per-stack lock and fails when
another holds it, and reads the tags back after moving them.

CI (.github/workflows/fleet-lambda-image.yml) builds the lane image on x86_64 when its inputs
change on main, pushes it as `rev-<commit>` (`publish`), and records the digest it pushed, which
moves `keep-built` to it unless keep-built already holds a newer commit's build. `select --built`
selects that digest, so nothing types one. `keep-built` is protected too, so the newest build
stays selectable however many images follow it.
"""
import argparse
import contextlib
import fnmatch
import fcntl
import json
import os
import re
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

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
# and at most seven carry a protected tag) keeps every protected image; rule 2 counts every image
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


BUILT = "keep-built"


def following(stack):
    """The image a selection is moving to: the third tag, so a swap never leaves an image unprotected."""
    return f"keep-{stack}-next"


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

    def __init__(self, repository=REPOSITORY, region=REGION, registry=None):
        # registry: the repository's account, for a caller in another one (CI's role).
        self.repository, self.region, self.registry = repository, region, registry

    def aws(self, args, body=None):
        account = [] if self.registry is None or body is not None else ["--registry-id", self.registry]
        result = subprocess.run(["aws", "ecr", *args, *account, "--region", self.region, "--output", "json"],
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

    def tags(self, digest):
        """Every tag the image carries."""
        out = self.aws(["describe-images", "--repository-name", self.repository, "--image-ids", "imageDigest=" + digest])
        return sorted(out["imageDetails"][0].get("imageTags", []))

    def put(self, tag, image):
        body = json.dumps({**({"registryId": self.registry} if self.registry else {}),
                           "repositoryName": self.repository, "imageTag": tag, "imageDigest": image.digest,
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
    """Make `new` the stack's current image and protect `running` as its previous one.

    Each write moves a tag off one image, so the order keeps every image that must stay protected
    holding a tag after every write: `next` takes the new image first, then `previous` the running
    one, then `current` the new one. A rollback (the running image holds `current`, the target
    `previous`) is a swap that two tags could not do without a moment where one image has none. An
    interrupted move is finished by running it again. Selecting the image the stack already runs
    leaves `previous`, its rollback target, where it is."""
    if stack not in STACKS:
        raise ValueError(f"unknown stack {stack!r}")
    image = ecr.image(new)
    ecr.put(following(stack), image)
    if running is not None and running != new:
        ecr.put(previous(stack), ecr.image(running))
    ecr.put(current(stack), image)
    now = tags(ecr, stack)
    moved = ecr.tagged(following(stack))
    if (now[current(stack)] != new or moved is None or moved.digest != new
            or (running not in (None, new) and now[previous(stack)] != running)):
        raise RuntimeError(f"{stack}'s tags changed during the selection: another selection ran")
    return now


def record(ecr, built, commit=None, is_ancestor=None):
    """Move keep-built to `built` and read it back. With `commit` (CI's publish), keep-built moves
    only forward in commit order: two main pushes' builds can finish out of order and a run can be
    rerun, so the image pushed last is not always the newest commit's. When keep-built holds the
    image of any commit that is not an ancestor of `commit` (an image can carry several rev- tags),
    it stays there ("kept").

    The lock is a flock in this host's temp directory and orders nothing across runners: CI's
    publishes are ordered by the workflow's concurrency group (fleet-lambda-image). A manual
    `record` must not run while a publish does."""
    with locked("built"):
        now = ecr.tagged(BUILT)
        if commit is not None and now is not None and now.digest != built:
            held = [t[len("rev-"):] for t in ecr.tags(now.digest) if re.fullmatch(r"rev-[0-9a-f]{40}", t)]
            # Every held commit is asked, so the tag order cannot matter: one known to be newer
            # keeps keep-built; only when none is, a commit git cannot find stops the record.
            unknown, behind = [], []
            for h in held:
                try:
                    if not is_ancestor(h, commit):
                        behind.append(h)
                except UnknownCommit:
                    unknown.append(h)
            if behind:
                return {"status": "kept", BUILT: now.digest}
            if unknown:
                raise RuntimeError(
                    f"keep-built holds {now.digest}, built from {unknown[0]}, which this checkout's history "
                    f"does not contain, so the builds cannot be ordered. If {built} should be the newest build, "
                    f"record it by hand: release.py record --digest {built}")
        ecr.put(BUILT, ecr.image(built))
        now = ecr.tagged(BUILT)
        if now is None or now.digest != built:
            raise RuntimeError("keep-built changed during the record: another record ran")
    return {"status": "recorded", BUILT: built}


class UnknownCommit(RuntimeError):
    def __init__(self, commit):
        super().__init__(f"git does not know {commit}")
        self.commit = commit


def git_is_ancestor(older, newer):
    """Whether `older` is an ancestor of `newer` (or the same commit). When merge-base cannot answer,
    it is UnknownCommit only when git can read the checked-out commit `newer` and not `older`; git
    that cannot read `newer` (outside a checkout, a corrupt repository) is an error with git's message,
    never a statement about the recorded commit."""
    result = subprocess.run(["git", "merge-base", "--is-ancestor", older, newer], capture_output=True, text=True)
    if result.returncode in (0, 1):
        return result.returncode == 0
    failure = RuntimeError(f"git merge-base failed ({result.returncode}): {result.stderr.strip()[:300]}")
    def present(commit):
        return subprocess.run(["git", "cat-file", "-e", commit + "^{commit}"], capture_output=True, text=True).returncode == 0
    if not present(newer):
        raise failure
    if not present(older):
        raise UnknownCommit(older)
    raise failure


REPOSITORY_URL = re.compile(r"[0-9]{12}\.dkr\.ecr\." + re.escape(REGION) + r"\.amazonaws\.com/" + re.escape(REPOSITORY))
LOCAL = "localhost/fcvm-lambda-tile"


def quiet(args, body=None, env=None):
    """Run a command whose output may name the registry: none of it is repeated."""
    result = subprocess.run(args, input=body, capture_output=True, text=True, env=env)
    if result.returncode:
        raise RuntimeError(f"{args[0]} {args[1]} failed")
    return result.stdout


def publish(ecr, archive, commit, repository_url, is_ancestor=git_is_ancestor):
    """CI: push the lane image CI built for `commit` as rev-<commit> and record its digest.

    The tag is immutable, so a rerun finds it. Whichever push put it there, the image under the tag
    must be the one this run built and checked (the same config digest, which podman calls the
    image ID), and after a push it must hold the digest podman pushed, or nothing is recorded."""
    if not re.fullmatch(r"[0-9a-f]{40}", commit or ""):
        raise ValueError("a full commit is required")
    if not REPOSITORY_URL.fullmatch(repository_url or ""):
        raise ValueError("not the lane repository")
    tag, local = "rev-" + commit, f"{LOCAL}:{commit}"
    quiet(["podman", "load", "-q", "-i", archive])
    built = "sha256:" + quiet(["podman", "image", "inspect", "--format", "{{.Id}}", local]).strip()
    found = ecr.tagged(tag)
    if found is None:
        with tempfile.TemporaryDirectory(prefix="lane-push-") as tmp:
            env = dict(os.environ, REGISTRY_AUTH_FILE=os.path.join(tmp, "auth.json"))
            password = quiet(["aws", "ecr", "get-login-password", "--region", REGION])
            registry = repository_url.split("/", 1)[0]
            quiet(["podman", "login", "--username", "AWS", "--password-stdin", registry], body=password, env=env)
            password = None
            digest_file = os.path.join(tmp, "digest")
            quiet(["podman", "push", "--digestfile", digest_file, local, f"docker://{repository_url}:{tag}"], env=env)
            with open(digest_file) as f:
                pushed = f.read().strip()
        found = ecr.tagged(tag)
        if found is None or found.digest != pushed:
            raise RuntimeError(f"{tag} does not hold the digest podman pushed")
    if json.loads(found.manifest).get("config", {}).get("digest") != built:
        # A full rerun after the build artifact expired rebuilds a different image; "Re-run failed
        # jobs" within the artifact's 7 days reuses the image this tag holds.
        raise RuntimeError(f"{tag} holds an image that is not the image this run built "
                           "(a rebuild? re-run the failed publish job, which reuses the run's image)")
    result = record(ecr, digest(found.digest), commit, is_ancestor)
    return {"tag": tag, "digest": found.digest, "status": result["status"]}


def snapshot(ecr, stack):
    """The stack's protected tags as they stand: what reset() puts back."""
    found = {name: ecr.tagged(tag) for name, tag in (("current", current(stack)), ("previous", previous(stack)),
                                                       ("next", following(stack)))}
    return {name: image.digest if image else None for name, image in found.items()}


def reset(ecr, stack, before):
    """Put the stack's tags back to `before` (a snapshot taken before a selection whose release failed). Current
    first: the image the stack still runs held previous throughout the selection and holds current again before
    previous moves back to the old rollback target; next last, which unprotects the image the failed release
    selected. A tag that did not exist before is left where it is (nothing here deletes a tag)."""
    with locked(stack):
        for name, tag in (("current", current(stack)), ("previous", previous(stack)), ("next", following(stack))):
            digest_ = before["current"] if name == "next" else before[name]
            if digest_ is not None:
                ecr.put(tag, ecr.image(digest_))


def lane_inputs(workflow):
    """The push paths of .github/workflows/fleet-lambda-image.yml: the lane image's inputs."""
    block = workflow.split("\n  push:\n", 1)[1].split("\n  pull_request:", 1)[0]
    return re.findall(r"^      - '([^']+)'", block, re.M)


def matches(path, patterns):
    return any(fnmatch.fnmatchcase(path, p.replace("**", "*")) for p in patterns)


def git_changed(older, newer, cwd=None):
    """Every path that differs between two commits: a rename lists both its paths (--no-renames), and a path
    git would quote comes back as it is (-z)."""
    result = subprocess.run(["git", "diff", "--no-renames", "-z", "--name-only", older, newer],
                            capture_output=True, cwd=cwd)
    if result.returncode:
        raise RuntimeError(f"git diff failed ({result.returncode}): {result.stderr.decode(errors='replace').strip()[:300]}")
    return [p for p in result.stdout.decode("utf-8", errors="surrogateescape").split("\0") if p]


def paired_build(ecr, commit, *, is_ancestor=None, changed=None, inputs=None, clock=time.monotonic,
                 sleep=time.sleep, timeout=1500):
    """The keep-built digest whose lane image is the one `commit` builds: built from `commit`, or from a commit
    on the same line (an ancestor or a descendant) with no lane input changed in between. keep-built moves
    only forward: when it holds a later build with other lane inputs, refuse at once; when it holds an earlier
    build, or none, wait (every 30 s, at most `timeout`) for this commit's lane build, then refuse. A coordinator
    never runs beside a lane image built from other lane inputs."""
    if changed is None:
        patterns = inputs if inputs is not None else lane_inputs(
            (Path(__file__).resolve().parents[2] / ".github/workflows/fleet-lambda-image.yml").read_text())
        changed = lambda a, b: any(matches(f, patterns) for f in git_changed(a, b))
    is_ancestor = is_ancestor or git_is_ancestor
    deadline = clock() + timeout
    while True:
        built = ecr.tagged(BUILT)
        if built is not None:
            held = [t[len("rev-"):] for t in ecr.tags(built.digest) if re.fullmatch(r"rev-[0-9a-f]{40}", t)]
            stranded = RuntimeError(
                f"no lane build pairs with {commit}: keep-built holds a newer lane build with other lane inputs, "
                "and it only moves forward, so a rerun of this run cannot succeed. Release staging with a "
                "workflow_dispatch of fleet-coordinator-deploy.yml at main's head")
            try:
                for h in held:
                    if h == commit or ((is_ancestor(h, commit) or is_ancestor(commit, h)) and not changed(h, commit)):
                        return built.digest
                behind = all(is_ancestor(h, commit) for h in held)
            except UnknownCommit:
                # keep-built's commit is not in this checkout: main moved past it after the run started.
                raise stranded from None
            # keep-built moves only forward: a build of a later commit (or of another line) with other lane
            # inputs means no build will ever pair with this commit. Only a build behind it is worth waiting on.
            if not held:
                raise RuntimeError(f"no lane build pairs with {commit}: keep-built holds an image with no "
                                   "rev-<commit> tag")
            if not behind:
                raise stranded
        if clock() >= deadline:
            if built is None:
                raise RuntimeError(f"no lane build is recorded (keep-built) for {commit}")
            raise RuntimeError(f"no lane build pairs with {commit}: this commit's lane build did not land in time")
        sleep(30)


def newest_build(ecr):
    built = ecr.tagged(BUILT)
    if built is None:
        raise LookupError("no lane image is recorded as built (keep-built): CI has not published one")
    return built.digest


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
        if target in (None, running):
            # A move that stopped after its `previous` write: `next` holds the image it was moving to.
            moved = ecr.tagged(following(stack))
            target = moved.digest if moved else None
        if target is None or target == running:
            raise LookupError(f"{stack} has no other image to roll back to")
        return move(ecr, stack, target, running)


def show(ecr):
    built = ecr.tagged(BUILT)
    return {**{stack: tags(ecr, stack) for stack in STACKS}, BUILT: built.digest if built else None}


def arguments(argv=None):
    parser = argparse.ArgumentParser(description="Move the lane image's protected tags (module docstring).")
    sub = parser.add_subparsers(dest="action", required=True)
    s = sub.add_parser("select")
    s.add_argument("--stack", choices=STACKS, required=True)
    which = s.add_mutually_exclusive_group(required=True)
    which.add_argument("--digest")
    which.add_argument("--built", action="store_true", help="the newest lane image CI built (keep-built)")
    s.add_argument("--running", required=True, help="the digest the stack's coordinator runs now, or none")
    p = sub.add_parser("promote")
    p.add_argument("--running", required=True, help="the digest prod's coordinator runs now, or none")
    r = sub.add_parser("rollback")
    r.add_argument("--stack", choices=STACKS, required=True)
    r.add_argument("--running", required=True, help="the digest the stack's coordinator runs now")
    sub.add_parser("show")
    c = sub.add_parser("record")
    c.add_argument("--digest", required=True)
    u = sub.add_parser("publish", help="CI: push the image built for --commit and record it")
    u.add_argument("--archive", required=True)
    u.add_argument("--commit", required=True)
    return parser.parse_args(argv)


def run(argv=None, ecr=None):
    args = arguments(argv)
    url = os.environ.get("LAMBDA_TILE_REPO", "")
    if ecr is None:
        ecr = Ecr(registry=url.split(".", 1)[0]) if args.action == "publish" and REPOSITORY_URL.fullmatch(url) else Ecr()
    if args.action == "select":
        new = newest_build(ecr) if args.built else digest(args.digest)
        result = select(ecr, args.stack, new, digest(args.running, none=True))
    elif args.action == "record":
        result = record(ecr, digest(args.digest))
    elif args.action == "publish":
        result = publish(ecr, args.archive, args.commit, url)
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
