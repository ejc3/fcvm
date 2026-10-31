#!/usr/bin/env python3
"""Request an immutable main build. AWS owns batching, deadlines and promotion."""
import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request

SOURCE_PATHS = ("Cargo.toml", "Cargo.lock", "build.rs", "Makefile", "rootfs-config.toml",
                "src", "fc-agent", "fleet", "kernel", "scripts", "assets")
CRITICAL_LABELS = {"critical", "security:critical", "priority:critical", "priority:p0", "p0"}
STATUSES = {"queued", "building", "preempting", "awaiting-publish", "promoted",
            "failed", "superseded", "disabled"}


def need(condition, message):
    if not condition:
        raise ValueError(message)


def tree_digest(sha, run=subprocess.check_output):
    need(re.fullmatch(r"[0-9a-f]{40}", sha), "Expected an immutable source commit")
    records = run(["git", "ls-tree", "-rz", sha, "--", *SOURCE_PATHS], timeout=30)
    need(records, "Source tree has no image inputs")
    return hashlib.sha256(b"fcvm-fleet-ami-source-v1\0" + records).hexdigest()


def priority_for_prs(prs):
    need(isinstance(prs, list) and len(prs) <= 100, "Invalid associated pull requests")
    for pr in prs:
        need(isinstance(pr, dict) and isinstance(pr.get("labels", []), list), "Invalid pull request labels")
        if any(isinstance(label, dict) and str(label.get("name", "")).lower() in CRITICAL_LABELS
               for label in pr.get("labels", [])):
            return "critical"
    return "normal"


def associated_priority(sha, token):
    # A failed label lookup must not silently delay a critical security fix.
    request = urllib.request.Request(
        "https://api.github.com/repos/ejc3/fcvm/commits/" + sha + "/pulls?per_page=100",
        headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
                 "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "fcvm-image-request"})
    with urllib.request.urlopen(request, timeout=20) as response:
        body = response.read(1024 * 1024 + 1)
        need(len(body) <= 1024 * 1024, "Associated pull requests response is too large")
    return priority_for_prs(json.loads(body))


def request_digest(content, priority, run_id, now):
    need(re.fullmatch(r"[0-9a-f]{64}", content), "Invalid source input digest")
    need(priority in ("normal", "critical"), "Invalid image build priority")
    # Routine source-identical builds share a weekly refresh key. A deliberate
    # critical request gets a new key even when only upstream packages changed.
    if priority == "critical":
        need(re.fullmatch(r"[0-9]{1,24}", run_id), "Critical requests require a GitHub run identity")
        refresh = "critical:" + run_id
    else:
        calendar = now.isocalendar()
        refresh = "week:" + str(calendar.year) + ":" + str(calendar.week)
    return hashlib.sha256(("fcvm-fleet-ami-request-v1\0" + content + "\0" + refresh).encode()).hexdigest()


def invoke(function, payload, run=subprocess.run):
    need(re.fullmatch(r"arn:aws:lambda:us-west-1:[0-9]{12}:function:[A-Za-z0-9_-]+:request", function),
         "Expected the fixed request alias")
    with tempfile.TemporaryDirectory(prefix="fcvm-image-request-") as directory:
        target = Path(directory) / "response.json"
        command = ["aws", "lambda", "invoke", "--region", "us-west-1", "--function-name", function,
                   "--cli-binary-format", "raw-in-base64-out", "--payload", json.dumps(payload),
                   "--output", "json", "--no-cli-pager", str(target)]
        environment = {key: value for key, value in os.environ.items() if not key.startswith("AWS_ENDPOINT_URL")}
        # Admission is idempotent by input key. Bounded CLI retries can safely
        # handle the serialized request alias without replaying an EC2 launch.
        environment.update(AWS_RETRY_MODE="standard", AWS_MAX_ATTEMPTS="6", AWS_PAGER="")
        result = run(command, check=True, capture_output=True, text=True, timeout=120, env=environment)
        transport = json.loads(result.stdout)
        need(transport.get("StatusCode") == 200 and not transport.get("FunctionError"), "Image request failed")
        need(target.stat().st_size <= 65536, "Image response is too large")
        response = json.loads(target.read_text())
    need(isinstance(response, dict) and response.get("status") in STATUSES, "Invalid image request status")
    need(response.get("request_id") == payload["input_sha256"], "Image request identity changed")
    need(response.get("input_sha256") == payload["input_sha256"], "Image inputs changed")
    # A docs-only main revision can share already queued/built immutable inputs.
    # Preserve the actual build commit instead of misreporting it as the caller's.
    need(re.fullmatch(r"[0-9a-f]{40}", response.get("source_sha", "")), "Invalid actual build source")
    need(response.get("status") not in ("failed", "disabled"), "Image pipeline is not accepting builds")
    return response


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--priority", choices=("auto", "normal", "critical"), required=True)
    args = parser.parse_args()
    need(os.environ.get("GITHUB_REPOSITORY") == "ejc3/fcvm", "Unexpected repository")
    need(re.fullmatch(r"[0-9a-f]{40}", args.sha), "Invalid source commit")
    priority = associated_priority(args.sha, os.environ["GH_TOKEN"]) if args.priority == "auto" else args.priority
    source = tree_digest(args.sha)
    key = request_digest(source, priority, os.environ["GITHUB_RUN_ID"], dt.datetime.now(dt.timezone.utc))
    payload = {"action": "request", "source_sha": args.sha, "input_sha256": key, "priority": priority}
    result = invoke(os.environ["AMI_PIPELINE_DISPATCH"], payload)
    # A queued request is not a published image. The fixed AWS executor reports
    # final validation/promotion independently and retains the previous pair on failure.
    print(json.dumps({"request_id": key, "status": result["status"], "requested_source_sha": args.sha,
                      "build_source_sha": result["source_sha"],
                      "source_tree_sha256": source, "priority": priority}, sort_keys=True))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as stream:
            stream.write("Fleet image request `" + key + "`: **" + result["status"] + "** (" + priority + ").\n\n")
            stream.write("Publication follows the AWS build and boot-validation checks; this request alone does not publish an image.\n")


if __name__ == "__main__":
    main()
