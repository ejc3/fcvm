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


def lane_setting():
    """FLEET_LAMBDA_LANE: "on" releases the staging coordinator with the lane image staging selects; unset, empty
    or "off" releases exactly as without a lane."""
    value = os.environ.get("FLEET_LAMBDA_LANE", "")
    if value not in ("", "off", "on"):
        raise ValueError("FLEET_LAMBDA_LANE is on, off or unset")
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


def restore_lane(stack, before):
    lane, ecr = lane_repository()
    lane.reset(ecr, stack, before)


def lane_running(function):
    """The lane image digest the stack's coordinator runs (None before its first lane release), from the
    release function, which reads it from the stack's lane pointer."""
    result = invoke(function, {"action": "lane"})
    running = result.get("lambda_digest", "")
    if result.get("status") != "ok" or not (running is None or isinstance(running, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", running)):
        if attribution(result):
            print(json.dumps({"lane": attribution(result)}), flush=True)
        raise RuntimeError("the release function did not report the running lane image")
    return running


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
        # The function refuses a lane digest that keep-stage-current does not hold, so the selection
        # comes first; the running digest stays protected as keep-stage-previous.
        payload["lambda_digest"], before = select_lane("stage", lane_running(function), commit)
        try:
            status = release(function, payload, digest, "staging")
        except Exception as failure:
            # A release that failed leaves the coordinator on its old lane image, and the selection must say
            # so (a promotion takes staging's selection). A call that ended without a result may still have
            # released, so ask what runs; when nothing answers, leave the tags and report the release error.
            try:
                now = lane_running(function)
            except Exception:
                print(json.dumps({"lane": "selection left: the running lane image is unknown"}), flush=True)
                raise failure from None
            if now != payload["lambda_digest"]:
                restore_lane("stage", before)
            raise
    print(json.dumps({"status": status, "commit": commit, "digest": digest,
                      **({"lambda_digest": payload["lambda_digest"]} if lane else {})}))


def main_prod(commit, digest, force):
    """Release an image staging already published to prod; nothing is pushed (the prod role cannot)."""
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise ValueError("a full commit and image digest are required")
    if os.environ["COORDINATOR_RELEASE"] != FUNCTIONS["prod"]:
        raise ValueError("unexpected release function")
    payload = {"commit": commit, "digest": digest, **({"force": True} if force else {})}
    status = release(FUNCTIONS["prod"], payload, digest, "prod")
    print(json.dumps({"status": status, "commit": commit, "digest": digest, "force": force}))


def arguments(argv=None):
    parser = argparse.ArgumentParser(description="Publish and release a coordinator image (module docstring).")
    parser.add_argument("--stack", choices=sorted(FUNCTIONS), default="stage")
    parser.add_argument("--commit")
    parser.add_argument("--digest")
    parser.add_argument("--force", action="store_true", help="prod rollback: skip the viewer hold")
    args = parser.parse_args(argv)
    if args.stack == "stage" and (args.commit or args.digest or args.force):
        parser.error("staging publishes GITHUB_SHA's image; --commit, --digest and --force are for prod")
    if args.stack == "prod" and not (args.commit and args.digest):
        parser.error("prod releases an image staging published: give --commit and --digest")
    return args


def run(argv=None):
    args = arguments(argv)
    try:
        if args.stack == "prod":
            main_prod(args.commit, args.digest, args.force)
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
