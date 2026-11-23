#!/usr/bin/env python3
"""Select the lane image a stack's Lambda tiles run, so the image and its rollback stay in the repository.

    release.py select   --stack stage --digest sha256:<new>|--built --running sha256:<now>|none
    release.py rollback --stack prod --running sha256:<now>
    release.py show     --stack stage|prod
    release.py record   --digest sha256:<built>         the newest lane image built from main
    release.py publish  --archive lane.tar --commit C   CI: push rev-C (LAMBDA_TILE_REPO) and record it
    release.py copy     --digest sha256:<tested>        prod CI: copy staging's tested image into prod's

Each stack has its own lane repository in us-east-1 (ejc3/aws fcvm-fleet-lambda.tf): staging's
fcvm-fleet/lambda-tile and prod's fcvm-fleet/lambda-tile-prod (REPOSITORIES). Each lifecycle
policy expires every image outside the newest five unless the image carries one of its stack's
protected tags, `keep-<stack>-current`, `keep-<stack>-previous` or `keep-<stack>-next` (LIFECYCLES
below), and in staging's also `keep-built`. Those are the only mutable tags, a stack's tags move only
in its own repository, and this script is what moves them; a push names an image `rev-<commit>` or
any other tag outside `keep-`. Staging never reads or writes prod's repository. A promotion copies the
digest staging tested into prod's by digest, on prod CI's credentials (`copy`), tagged
`from-stage-<hex>`, and checks prod's repository holds that same digest before prod selects it.

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
STACKS = ("stage", "prod")
REPOSITORIES = {"stage": "fcvm-fleet/lambda-tile", "prod": "fcvm-fleet/lambda-tile-prod"}
# Each repository's mutable, never-expired tags (ejc3/aws fcvm-fleet-lambda.tf).
PROTECTED = {"stage": "keep-*", "prod": "keep-prod-*"}
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
# Kept with the bytes and media type that were pushed, so a tag put on them names the same digest.
MEDIA_TYPES = [
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.index.v1+json",
]

# Each repository's lifecycle policy, as ejc3/aws fcvm-fleet-lambda.tf sets it. An image a rule's
# tags select cannot be expired by a lower-priority rule, so rule 1 (which keeps up to 100 images,
# and at most four carry a protected tag) keeps every protected image; rule 2 counts every image
# and expires the ones outside the newest five that no protected tag holds.
LIFECYCLES = {stack: {"rules": [
    {"rulePriority": 1, "description": description,
     "selection": {"tagStatus": "tagged", "tagPatternList": [PROTECTED[stack]],
                   "countType": "imageCountMoreThan", "countNumber": 100},
     "action": {"type": "expire"}},
    {"rulePriority": 2, "description": "keep the newest 5 images",
     "selection": {"tagStatus": "any", "countType": "imageCountMoreThan", "countNumber": 5},
     "action": {"type": "expire"}},
]} for stack, description in (("stage", "never an image a stack runs or can roll back to"),
                               ("prod", "never an image prod runs or can roll back to"))}


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
    """A lane repository through the AWS CLI: read an image by tag or digest, put a tag on one."""

    def __init__(self, repository, region=REGION, registry=None):
        # registry: the repository's account, for a caller in another one (CI's role).
        self.repository, self.region, self.registry = repository, region, registry

    def url(self):
        if self.registry is None:
            raise ValueError(f"{self.repository}'s account is not known: set LAMBDA_TILE_REPO")
        return f"{self.registry}.dkr.ecr.{self.region}.amazonaws.com/{self.repository}"

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


def own(ecr, stack):
    """Refuse a stack's tags, or staging's builds, anywhere but that stack's own repository."""
    if stack not in STACKS:
        raise ValueError(f"unknown stack {stack!r}")
    if ecr.repository != REPOSITORIES[stack]:
        raise ValueError(f"{ecr.repository} is not {stack}'s lane repository")


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
    own(ecr, stack)
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
    own(ecr, stack)
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
    own(ecr, "stage")
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


REPOSITORY_URLS = {stack: re.compile(r"[0-9]{12}\.dkr\.ecr\." + re.escape(REGION) + r"\.amazonaws\.com/" + re.escape(name))
                   for stack, name in REPOSITORIES.items()}
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
    image ID), and after a push it must hold the digest podman pushed, or nothing is recorded. Staging's
    repository only: prod's receives images only by copy."""
    own(ecr, "stage")
    if not re.fullmatch(r"[0-9a-f]{40}", commit or ""):
        raise ValueError("a full commit is required")
    if not REPOSITORY_URLS["stage"].fullmatch(repository_url or ""):
        raise ValueError("not staging's lane repository")
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
    own(ecr, stack)
    found = {name: ecr.tagged(tag) for name, tag in (("current", current(stack)), ("previous", previous(stack)),
                                                       ("next", following(stack)))}
    return {name: image.digest if image else None for name, image in found.items()}


def reset(ecr, stack, before):
    """Put the stack's tags back to `before` (a snapshot taken before a selection whose release failed). Current
    first: the image the stack still runs held previous throughout the selection and holds current again before
    previous moves back to the old rollback target; next last, which unprotects the image the failed release
    selected. A tag that did not exist before is left where it is (nothing here deletes a tag)."""
    own(ecr, stack)
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
    own(ecr, "stage")
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
            # Every held commit is asked, as record() does: one this checkout lacks decides nothing alone.
            pairs, ahead, unknown = False, False, []
            for h in held:
                try:
                    if h == commit or ((is_ancestor(h, commit) or is_ancestor(commit, h)) and not changed(h, commit)):
                        pairs = True
                        break
                    ahead = ahead or not is_ancestor(h, commit)
                except UnknownCommit:
                    unknown.append(h)
            if pairs:
                return built.digest
            if not held:
                raise RuntimeError(f"no lane build pairs with {commit}: keep-built holds an image with no "
                                   "rev-<commit> tag")
            # keep-built moves only forward: a build of a later commit (or of another line) with other lane
            # inputs means no build will ever pair with this commit. Only a build behind it is worth waiting on.
            if ahead:
                raise RuntimeError(
                    f"no lane build pairs with {commit}: keep-built holds a newer lane build with other lane "
                    "inputs, and it only moves forward, so a rerun of this run cannot succeed. Release staging "
                    "with a workflow_dispatch of fleet-coordinator-deploy.yml at main's head")
            if unknown:
                raise RuntimeError(
                    f"no lane build pairs with {commit}: keep-built holds a build of {unknown[0]}, which this "
                    "checkout does not contain (main moved after this run started). A rerun fetches main again; "
                    "or release staging with a workflow_dispatch of fleet-coordinator-deploy.yml at main's head")
        if clock() >= deadline:
            if built is None:
                raise RuntimeError(f"no lane build is recorded (keep-built) for {commit}")
            raise RuntimeError(f"no lane build pairs with {commit}: this commit's lane build did not land in time")
        sleep(30)


def newest_build(ecr):
    own(ecr, "stage")
    built = ecr.tagged(BUILT)
    if built is None:
        raise LookupError("no lane image is recorded as built (keep-built): CI has not published one")
    return built.digest


def select(ecr, stack, new, running):
    with locked(stack):
        return move(ecr, stack, new, running)


def copied(digest_):
    """The tag a copy into prod's repository carries: immutable there, so a rerun finds it."""
    return "from-stage-" + digest(digest_).split(":", 1)[1]


def skopeo_copy(source, dest, digest_, tag):
    """Copy one image by digest between the lane repositories through the registry API, on the caller's
    credentials. --preserve-digests makes skopeo refuse any copy that would change the manifest, so the copy
    is the same digest or nothing."""
    with tempfile.TemporaryDirectory(prefix="lane-copy-") as tmp:
        auth = os.path.join(tmp, "auth.json")
        for registry in sorted({source.url().split("/", 1)[0], dest.url().split("/", 1)[0]}):
            password = quiet(["aws", "ecr", "get-login-password", "--region", REGION])
            quiet(["skopeo", "login", "--authfile", auth, "--username", "AWS", "--password-stdin", registry], body=password)
            password = None
        quiet(["skopeo", "copy", "--authfile", auth, "--preserve-digests", "--retry-times", "3",
               f"docker://{source.url()}@{digest_}", f"docker://{dest.url()}:{tag}"])


def copy(source, dest, digest_, transfer=None):
    """Prod CI: copy the digest staging tested from staging's repository into prod's, and check prod's
    repository holds that same digest under `from-stage-<hex>`. Moves no keep- tag: the release that follows
    selects it for prod. An image prod's repository already holds under that tag is not copied again."""
    own(source, "stage")
    own(dest, "prod")
    tag = copied(digest_)
    status = "present"
    # The tag decides, not the digest: an image prod's repository holds without it (the tag removed by hand while a
    # keep-prod- tag keeps the image) is copied again, so a rerun converges.
    landed = dest.tagged(tag)
    if landed is None:
        source.image(digest_)  # LookupError when staging's repository does not hold it
        (transfer or skopeo_copy)(source, dest, digest_, tag)
        status = "copied"
        landed = dest.tagged(tag)
    if landed is None or landed.digest != digest_ or dest.get("imageDigest=" + digest_) is None:
        raise RuntimeError(f"prod's lane repository holds {landed.digest if landed else 'nothing'} under {tag}, "
                           f"not the digest staging tested ({digest_})")
    return {"status": status, "digest": digest_, "tag": tag}


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


def show(ecr, stack):
    shown = {stack: tags(ecr, stack)}
    if stack == "stage":
        built = ecr.tagged(BUILT)
        shown[BUILT] = built.digest if built else None
    return shown


def arguments(argv=None):
    parser = argparse.ArgumentParser(description="Move the lane image's protected tags (module docstring).")
    sub = parser.add_subparsers(dest="action", required=True)
    s = sub.add_parser("select")
    s.add_argument("--stack", choices=STACKS, required=True)
    which = s.add_mutually_exclusive_group(required=True)
    which.add_argument("--digest")
    which.add_argument("--built", action="store_true", help="the newest lane image CI built (keep-built)")
    s.add_argument("--running", required=True, help="the digest the stack's coordinator runs now, or none")
    r = sub.add_parser("rollback")
    r.add_argument("--stack", choices=STACKS, required=True)
    r.add_argument("--running", required=True, help="the digest the stack's coordinator runs now")
    w = sub.add_parser("show")
    w.add_argument("--stack", choices=STACKS, required=True)
    c = sub.add_parser("record")
    c.add_argument("--digest", required=True)
    u = sub.add_parser("publish", help="CI: push the image built for --commit and record it")
    u.add_argument("--archive", required=True)
    u.add_argument("--commit", required=True)
    y = sub.add_parser("copy", help="prod CI: copy the digest staging tested into prod's repository (LAMBDA_TILE_REPO)")
    y.add_argument("--digest", required=True)
    return parser.parse_args(argv)


def stack_of(args):
    return {"publish": "stage", "record": "stage", "copy": "prod"}.get(args.action) or args.stack


def run(argv=None, ecr=None, source=None):
    args = arguments(argv)
    stack = stack_of(args)
    url = os.environ.get("LAMBDA_TILE_REPO", "")
    # LAMBDA_TILE_REPO names this stack's repository (its account, for CI's role in another one); CI's
    # publish and copy refuse to run without it.
    registry = url.split(".", 1)[0] if REPOSITORY_URLS[stack].fullmatch(url) else None
    if args.action in ("publish", "copy") and registry is None:
        raise ValueError(f"LAMBDA_TILE_REPO is not {stack}'s lane repository")
    if ecr is None:
        ecr = Ecr(REPOSITORIES[stack], registry=registry)
    if args.action == "select":
        new = newest_build(ecr) if args.built else digest(args.digest)
        result = select(ecr, args.stack, new, digest(args.running, none=True))
    elif args.action == "record":
        result = record(ecr, digest(args.digest))
    elif args.action == "publish":
        result = publish(ecr, args.archive, args.commit, url)
    elif args.action == "copy":
        # Staging's repository is in the same account as prod's.
        result = copy(source or Ecr(REPOSITORIES["stage"], registry=registry), ecr, digest(args.digest))
    elif args.action == "rollback":
        result = rollback(ecr, args.stack, digest(args.running))
    else:
        result = show(ecr, args.stack)
    print(json.dumps(result, sort_keys=True))
    return result


if __name__ == "__main__":
    run()
