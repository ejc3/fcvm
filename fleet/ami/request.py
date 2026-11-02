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

CRITICAL_LABELS = {"critical", "security:critical", "priority:critical", "priority:p0", "p0"}
STATUSES = {"queued", "building", "preempting", "awaiting-publish", "promoted",
            "failed", "superseded", "disabled"}


def need(condition, message):
    if not condition:
        raise ValueError(message)


class Refused(ValueError):
    """The factory refused the request and said why (its message is a fixed string)."""


def tree_digest(sha, run=subprocess.check_output):
    need(re.fullmatch(r"[0-9a-f]{40}", sha), "Expected an immutable source commit")
    records = run(["git", "ls-tree", "-rz", sha, "--"], timeout=30)
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


def github_json(path, token):
    request = urllib.request.Request(
        "https://api.github.com/repos/ejc3/fcvm/" + path,
        headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
                 "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "fcvm-image-request"})
    with urllib.request.urlopen(request, timeout=20) as response:
        body = response.read(1024 * 1024 + 1)
        need(len(body) <= 1024 * 1024, "GitHub response is too large")
    return json.loads(body)


def associated_priority(sha, token, fetch=github_json):
    # A failed label lookup must not silently delay a critical security fix.
    return priority_for_prs(fetch("commits/" + sha + "/pulls?per_page=100", token))


def not_requestable(sha, token, fetch=github_json):
    """Why this revision cannot enter the factory now, or None. The factory takes only the
    current main revision with successful CI. A CI run that completes after the next merge
    asks for a revision main has moved past, and the weekly schedule can fire while main's
    newest CI is still running; in both cases the newer revision's own CI completion asks
    for its image, so this request is not needed."""
    head = fetch("git/ref/heads/main", token).get("object", {}).get("sha")
    need(isinstance(head, str) and re.fullmatch(r"[0-9a-f]{40}", head), "Invalid main revision")
    if head != sha:
        return "main has moved on to " + head + ", whose CI completion requests its image"
    runs = fetch("actions/workflows/ci.yml/runs?branch=main&head_sha=" + sha + "&per_page=20", token)
    runs = runs.get("workflow_runs")
    need(isinstance(runs, list), "Invalid CI runs")
    if any(r.get("conclusion") == "success" for r in runs):
        return None
    if any(r.get("status") in ("queued", "in_progress", "waiting", "pending", "requested") for r in runs):
        return "main CI for " + sha + " is still running; its completion requests this image"
    return None


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
    if isinstance(response, dict) and response.get("status") == "refused":
        message = response.get("message")
        need(isinstance(message, str) and len(message) <= 200, "Invalid image request refusal")
        raise Refused("The image factory refused the request: " + message)
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
    skip = not_requestable(args.sha, os.environ["GH_TOKEN"])
    if skip:
        print(json.dumps({"requested_source_sha": args.sha, "skipped": skip}, sort_keys=True))
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a") as stream:
                stream.write("Fleet image request skipped: " + skip + ".\n")
        return
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
