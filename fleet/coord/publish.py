#!/usr/bin/env python3
"""Publish an immutable coordinator image and invoke a stack's image-only releaser.

    publish.py                                  staging: push GITHUB_SHA's image, release it (with
                                                FLEET_LAMBDA_LANE=on, and the lane build paired with
                                                this commit)
    publish.py --stack prod --commit C --digest D [--force]
                                                prod: release an image staging already published
                                                (exit 3: held for viewers, nothing changed)

The prod releaser (ejc3/aws fcvm-fleet-prod-ci.tf) goes ahead only while the prod fleet has had no
active viewer for 5 minutes; `--force` skips that, for a rollback to an image prod ran before.
"""
import argparse
import functools
import importlib.util
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

TOKEN = re.compile(r"[A-Za-z0-9.:_-]{1,64}")
FUNCTIONS = {"stage": "fcvm-coordinator-release", "prod": "fcvm-coordinator-release-prod"}
HELD = 3


# The AWS CLI's "An error occurred (<code>) when calling ...": the code is a fixed token AWS defines;
# the rest of the line can name resources and is never repeated.
AWS_ERROR = re.compile(r"An error occurred \(([A-Za-z0-9]{1,64})\)")


class CommandFailed(RuntimeError):
    def __init__(self, name, returncode, code=None):
        super().__init__("command failed: " + name)
        self.returncode = returncode
        # The AWS error code the CLI reported, when it reported one.
        self.code = code


class Held(RuntimeError):
    """The prod releaser held the release: viewers, or no viewer data. Nothing changed."""


def command(args, *, body=None, env=None, allow_missing=False):
    result = subprocess.run(args, input=body, capture_output=True, text=True, env=env)
    if result.returncode:
        if allow_missing and "ImageNotFoundException" in result.stderr:
            return None
        found = AWS_ERROR.search(result.stderr or "")
        raise CommandFailed(args[0], result.returncode, found.group(1) if found else None)
    return result.stdout


def attribution(result):
    """The release function's failed step and AWS error code: the only fields of its result repeated here."""
    return {k: result[k] for k in ("step", "code") if isinstance(result.get(k), str) and TOKEN.fullmatch(result[k])}


def invoke(function, payload, *, attempts=11):
    """Invoke a release function and return its result. A call that ends without a result is
    retried: the function is serialized and idempotent, so a later invocation reports busy, then
    the finished release."""
    with tempfile.TemporaryDirectory(prefix="coordinator-release-") as tmp:
        output = Path(tmp) / "result.json"  # Only status, task ARN and image digest; no credentials.
        for attempt in range(attempts):
            try:
                meta = json.loads(command(["aws", "lambda", "invoke", "--region", "us-west-1",
                    "--function-name", function, "--cli-binary-format", "raw-in-base64-out",
                    "--payload", "file:///dev/stdin", "--cli-read-timeout", "910", str(output)],
                    body=json.dumps(payload)))
            except CommandFailed as error:
                # The call can end while the function keeps running (its connection was dropped, then the
                # CLI's own retries were throttled by the function's single reserved slot).
                # 252 (command syntax) and 253 (configuration): the CLI refused the call before
                # invoking anything, and the same call is refused again.
                if attempt == attempts - 1 or error.returncode in (252, 253):
                    raise
                code = f", {error.code}" if error.code else ""
                print(f"The release invocation ended without a result (aws exit {error.returncode}{code}); "
                      "retrying in 60 seconds", flush=True)
                time.sleep(60)
                continue
            if meta.get("FunctionError"):
                raise RuntimeError("release function failed")
            return json.loads(output.read_text())
    raise RuntimeError("release function gave no result")


@functools.cache
def lane_release():
    """fleet/lambda/release.py, which moves the lane image's protected tags (loaded once)."""
    spec = importlib.util.spec_from_file_location("lane_release", Path(__file__).resolve().parent.parent / "lambda" / "release.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lane_setting(name="FLEET_LAMBDA_LANE"):
    """FLEET_LAMBDA_LANE (staging's release) or FLEET_LAMBDA_LANE_PROD (prod's promotion decision): "on" carries
    the stack's lane image; unset, empty or "off" releases exactly as without a lane."""
    value = os.environ.get(name, "")
    if value not in ("", "off", "on"):
        raise ValueError(f"{name} is on, off or unset")
    return value == "on"


def lane_repository():
    lane = lane_release()
    url = os.environ.get("LAMBDA_TILE_REPO", "")
    if not lane.REPOSITORY_URL.fullmatch(url):
        raise ValueError("not the lane repository")
    return lane, lane.Ecr(registry=url.split(".", 1)[0])


def select_lane(stack, running, commit):
    """Select for the stack the lane build that pairs with `commit` (fleet/lambda/release.py paired_build: built
    from it, or from a commit on its line with the same lane inputs; it waits, bounded, for this commit's lane
    build), keeping `running`, the digest its coordinator runs, as the rollback target. Returns the selected
    digest and the stack's tags before the selection, which restore_lane puts back if the release fails."""
    lane, ecr = lane_repository()
    new = lane.paired_build(ecr, commit)
    before = lane.snapshot(ecr, stack)
    lane.select(ecr, stack, new, running)
    return new, before


def select_prod_lane(running, digest):
    """Select `digest` for prod (the lane image of the set prod takes, or of the set it returns to), keeping
    `running` as prod's previous. Returns it and prod's tags before the selection."""
    lane, ecr = lane_repository()
    before = lane.snapshot(ecr, "prod")
    lane.select(ecr, "prod", digest, running)
    return digest, before


def restore_lane(stack, before):
    lane, ecr = lane_repository()
    lane.reset(ecr, stack, before)


def lane_state(function):
    """What the stack's coordinator runs, from the release function's read-only `lane` action: the lane image
    digest (None before its first lane release) and the commit tags of its coordinator image."""
    result = invoke(function, {"action": "lane"})
    running, commits = result.get("lambda_digest", ""), result.get("commits")
    if (result.get("status") != "ok"
            or not (running is None or isinstance(running, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", running))
            or not isinstance(commits, list) or not commits
            or not all(isinstance(c, str) and re.fullmatch(r"[0-9a-f]{40}", c) for c in commits)):
        # No commit for the running image (it carries no commit tag): the release cannot be ordered.
        if attribution(result):
            print(json.dumps({"lane": attribution(result)}), flush=True)
        raise RuntimeError("the release function did not report what the coordinator runs")
    return running, commits


def lane_running(function):
    return lane_state(function)[0]


def commit_is_ancestor(older, newer):
    """Whether `older` is an ancestor of `newer`, a commit staging runs. `newer` must be on main: one this
    checkout's main lacks (a main commit pushed after the checkout) is fetched first, and one main does not
    contain after that (another branch's, which a full-history checkout can hold) stops the release."""
    def on_main(commit):
        return subprocess.run(["git", "merge-base", "--is-ancestor", commit, "origin/main"],
                              capture_output=True).returncode == 0
    if not on_main(newer):
        if subprocess.run(["git", "fetch", "--quiet", "origin", "main"], capture_output=True).returncode:
            raise RuntimeError(f"could not fetch main to order this release after {newer}, which staging runs")
        if not on_main(newer):
            raise RuntimeError(f"staging runs {newer}, which main does not contain: the release cannot be ordered")
    result = subprocess.run(["git", "merge-base", "--is-ancestor", older, newer], capture_output=True, text=True)
    if result.returncode not in (0, 1):
        raise RuntimeError(f"git merge-base failed ({result.returncode})")
    return result.returncode == 0


def release_with_lane(function, payload, digest, name, stack, selected, before):
    """Release with payload["lambda_digest"] = `selected`, the stack's tags having been moved from `before`. A
    release that fails leaves the coordinator on its old lane image, and the selection must say so (a promotion
    takes staging's selection; a rollback, prod's record). A call that ended without a result may still have
    released, so ask what runs; when nothing answers, leave the tags and raise the release's own error."""
    payload["lambda_digest"] = selected
    try:
        return release(function, payload, digest, name)
    except Exception as failure:
        try:
            now = lane_running(function)
        except Exception:
            print(json.dumps({"lane": "selection left: the running lane image is unknown"}), flush=True)
            raise failure from None
        if now != selected:
            restore_lane(stack, before)
        raise


def release(function, payload, digest, name):
    """Run one release to completion: busy (Terraform holds the lock) waits a minute and asks again."""
    for attempt in range(11):
        result = invoke(function, payload)
        status = result.get("status")
        if status == "busy" and attempt < 10:
            print(f"Terraform is running; retrying the {name} release in 60 seconds", flush=True)
            time.sleep(60)
            continue
        if status == "held":
            reason = result.get("reason") if isinstance(result.get("reason"), str) and TOKEN.fullmatch(result["reason"]) else None
            print(json.dumps({"status": "held", "reason": reason}), flush=True)
            raise Held("the release was held")
        if status not in ("deployed", "unchanged") or result.get("digest") != digest \
                or "lambda_digest" in payload and result.get("lambda_digest") != payload["lambda_digest"]:
            if attribution(result):
                print(json.dumps({"release": attribution(result)}), flush=True)
            raise RuntimeError(f"{name} release did not complete")
        return status
    raise RuntimeError(f"{name} release did not complete")


def main():
    lane = lane_setting()
    commit, repo = os.environ["GITHUB_SHA"], os.environ["COORDINATOR_REPO"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("a full commit is required")
    if not re.fullmatch(r"[0-9]{12}\.dkr\.ecr\.us-east-2\.amazonaws\.com/fcvm-fleet/coordinator", repo):
        raise ValueError("unexpected coordinator repository")
    if os.environ["COORDINATOR_RELEASE"] != FUNCTIONS["stage"]:
        raise ValueError("unexpected release function")
    describe = ["aws", "ecr", "describe-images", "--region", "us-east-2", "--registry-id", repo.split(".")[0],
                "--repository-name", "fcvm-fleet/coordinator", "--image-ids", "imageTag=" + commit, "--output", "json"]
    existing = command(describe, allow_missing=True)
    if existing is None:
        with tempfile.TemporaryDirectory(prefix="coordinator-docker-") as config:
            env = dict(os.environ, DOCKER_CONFIG=config)
            password = command(["aws", "ecr", "get-login-password", "--region", "us-east-2"])
            registry = repo.split("/")[0]
            try:
                command(["docker", "login", "--username", "AWS", "--password-stdin", registry], body=password, env=env)
                password = None
                command(["docker", "tag", "fcvm-fleet-coordinator:" + commit, repo + ":" + commit], env=env)
                command(["docker", "push", repo + ":" + commit], env=env)
            finally:
                command(["docker", "logout", registry], env=env)
        existing = command(describe)
    images = json.loads(existing)["imageDetails"]
    if len(images) != 1 or not re.fullmatch(r"sha256:[0-9a-f]{64}", images[0]["imageDigest"]):
        raise ValueError("unexpected published digest")
    digest = images[0]["imageDigest"]
    payload = {"commit": commit, "digest": digest}
    function = os.environ["COORDINATOR_RELEASE"]
    if not lane:
        status = release(function, payload, digest, "staging")
    else:
        # Releases are ordered here, under the deploy job's fleet-viewer-smoke group (one release at a time):
        # a commit that is a strict ancestor of one staging runs is not released over it.
        running, commits = lane_state(function)
        if any(c != commit and commit_is_ancestor(commit, c) for c in commits):
            # The order step skips such a release before this step runs; finding it here means staging moved
            # in between. Fail rather than conclude success for a release this run did not make.
            raise RuntimeError(f"staging runs a descendant of {commit}: this release was not made")
        # The function refuses a lane digest that keep-stage-current does not hold, so the selection
        # comes first; the running digest stays protected as keep-stage-previous.
        selected, before = select_lane("stage", running, commit)
        status = release_with_lane(function, payload, digest, "staging", "stage", selected, before)
    print(json.dumps({"status": status, "commit": commit, "digest": digest,
                      **({"lambda_digest": payload["lambda_digest"]} if lane else {})}))


def order():
    """The deploy workflow's order step, before any release call: whether this run's release is skipped
    because staging runs a descendant of its commit (invariant 1). Writes skip=true|false to $GITHUB_OUTPUT;
    the release step runs only on skip=false, so a skipped run's release step concludes "skipped", which
    prod's release discovery (prod_release.released) ignores. Lane off: skip=false, nothing asked."""
    commit = os.environ["GITHUB_SHA"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("a full commit is required")
    skip = False
    if lane_setting():
        if os.environ["COORDINATOR_RELEASE"] != FUNCTIONS["stage"]:
            raise ValueError("unexpected release function")
        _, commits = lane_state(os.environ["COORDINATOR_RELEASE"])
        skip = any(c != commit and commit_is_ancestor(commit, c) for c in commits)
    print(json.dumps({"order": "skip" if skip else "release", "commit": commit,
                      **({"reason": "staging-runs-a-descendant"} if skip else {})}))
    with open(os.environ["GITHUB_OUTPUT"], "a") as out:
        out.write(f"skip={'true' if skip else 'false'}\n")


def main_prod(commit, digest, force, lambda_digest=None):
    """Release an image staging already published to prod; nothing is pushed (the prod role cannot). With
    `lambda_digest` (the workflows pass it when the prod set carries one: prod_release.py decide, with
    FLEET_LAMBDA_LANE_PROD on), also select that lane image for prod and release it with the coordinator."""
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise ValueError("a full commit and image digest are required")
    if lambda_digest is not None and lambda_digest != "none" and not re.fullmatch(r"sha256:[0-9a-f]{64}", lambda_digest):
        raise ValueError("--lambda-digest is an image digest, or none")
    if os.environ["COORDINATOR_RELEASE"] != FUNCTIONS["prod"]:
        raise ValueError("unexpected release function")
    payload = {"commit": commit, "digest": digest, **({"force": True} if force else {})}
    if lambda_digest is None:
        status = release(FUNCTIONS["prod"], payload, digest, "prod")
    elif lambda_digest == "none":
        # A rollback to a set that ran no lane image: the function removes FLEET_LAMBDA_IMAGE. No tag moves (tags
        # only protect images; nothing runs one now).
        payload["lambda_digest"] = None
        status = release(FUNCTIONS["prod"], payload, digest, "prod")
    else:
        selected, before = select_prod_lane(lane_running(FUNCTIONS["prod"]), lambda_digest)
        status = release_with_lane(FUNCTIONS["prod"], payload, digest, "prod", "prod", selected, before)
    print(json.dumps({"status": status, "commit": commit, "digest": digest, "force": force,
                      **({"lambda_digest": payload.get("lambda_digest")} if lambda_digest else {})}))


def arguments(argv=None):
    parser = argparse.ArgumentParser(description="Publish and release a coordinator image (module docstring).")
    parser.add_argument("--stack", choices=sorted(FUNCTIONS), default="stage")
    parser.add_argument("--order", action="store_true", help="staging: only decide whether to skip the release")
    parser.add_argument("--commit")
    parser.add_argument("--digest")
    parser.add_argument("--force", action="store_true", help="prod rollback: skip the viewer hold")
    parser.add_argument("--lambda-digest", help="prod: the lane image of the set prod takes or returns to, or none "
                        "to return to running no lane image")
    args = parser.parse_args(argv)
    if args.stack == "stage" and (args.commit or args.digest or args.force or args.lambda_digest):
        parser.error("staging publishes GITHUB_SHA's image; --commit, --digest, --force and --lambda-digest are for prod")
    if args.stack == "prod" and not (args.commit and args.digest):
        parser.error("prod releases an image staging published: give --commit and --digest")
    return args


def run(argv=None):
    args = arguments(argv)
    try:
        if args.order:
            if args.stack != "stage":
                raise ValueError("--order is for the staging release")
            order()
        elif args.stack == "prod":
            main_prod(args.commit, args.digest, args.force, args.lambda_digest)
        else:
            main()
    except Held:
        raise SystemExit(HELD)
    except Exception as error:
        # Never repeat subprocess output, request headers or a credential-bearing environment.
        # The lane repository's commands (fleet/lambda/release.py) raise their own CommandFailed, with the same code.
        failed = isinstance(error, (CommandFailed, lane_release().CommandFailed))
        print(json.dumps({"status": "failed", "errorType": type(error).__name__,
                          **({"code": error.code} if failed and error.code else {})}))
        raise SystemExit(1)


if __name__ == "__main__":
    run()
