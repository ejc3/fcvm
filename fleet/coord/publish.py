#!/usr/bin/env python3
"""Publish an immutable coordinator image and invoke the image-only staging releaser."""
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path


def command(args, *, body=None, env=None, allow_missing=False):
    result = subprocess.run(args, input=body, capture_output=True, text=True, env=env)
    if result.returncode:
        if allow_missing and "ImageNotFoundException" in result.stderr:
            return None
        raise RuntimeError("command failed: " + args[0])
    return result.stdout


def main():
    commit, repo = os.environ["GITHUB_SHA"], os.environ["COORDINATOR_REPO"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("a full commit is required")
    if not re.fullmatch(r"[0-9]{12}\.dkr\.ecr\.us-east-2\.amazonaws\.com/fcvm-fleet/coordinator", repo):
        raise ValueError("unexpected coordinator repository")
    if os.environ["COORDINATOR_RELEASE"] != "fcvm-coordinator-release":
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
    payload = json.dumps({"commit": commit, "digest": images[0]["imageDigest"]})
    with tempfile.TemporaryDirectory(prefix="coordinator-release-") as tmp:
        output = Path(tmp) / "result.json"  # Only status, task ARN and image digest; no credentials.
        for attempt in range(11):
            meta = json.loads(command(["aws", "lambda", "invoke", "--region", "us-west-1",
                "--function-name", os.environ["COORDINATOR_RELEASE"], "--cli-binary-format", "raw-in-base64-out",
                "--payload", "file:///dev/stdin", "--cli-read-timeout", "910", str(output)], body=payload))
            if meta.get("FunctionError"):
                raise RuntimeError("release function failed")
            result = json.loads(output.read_text())
            status = result.get("status")
            if status == "busy" and attempt < 10:
                print("Terraform is running; retrying the staging release in 60 seconds", flush=True)
                time.sleep(60)
                continue
            if status not in ("deployed", "unchanged") or result.get("digest") != images[0]["imageDigest"]:
                raise RuntimeError("staging release did not complete")
            print(json.dumps({"status": status, "commit": commit, "digest": result["digest"]}))
            return


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never repeat subprocess output, request headers or a credential-bearing environment.
        print(json.dumps({"status": "failed", "errorType": type(error).__name__}))
        raise SystemExit(1)
