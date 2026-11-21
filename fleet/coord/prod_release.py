#!/usr/bin/env python3
"""Promote the set staging smoked together to the production fleet (.github/workflows/fleet-prod-*.yml).

The set is staging's coordinator image, its web commit and the host and tile image generation the
image release gate verified. Prod takes it only when one staging viewer smoke (both families) ran
against exactly that set: the smoke started after the last of the coordinator and web releases
finished, and after the gate verified the generation (or it is the gate's own verifying smoke),
and staging still runs all three. The release functions are ejc3/aws fcvm-fleet-prod-ci.tf's:
COORDINATOR_RELEASE (release.py, STACK=prod) and IMAGE_RELEASE (publish.py, ProdPublisher).

    prod_release.py enabled                 is the prod stack on, and is there anything new to look at
    prod_release.py decide                  the staging set, prod's current release, what changes
    prod_release.py hold --image BOOL       wait (up to HOLD_SECONDS) until prod may change
    prod_release.py image-promote RECEIPT   move prod's templates to a gate-verified receipt
    prod_release.py image-rollback RECEIPT  return prod's templates to their previous release
    prod_release.py record RELEASE          write /fcvm-fleet/prod/release
    prod_release.py current                 the release record, for a manual rollback

Each subcommand writes its results to $GITHUB_OUTPUT. Exit 3 means held: nothing changed.
Output carries commits, digests, generations, build ids and run ids: no host, address or account.
"""
import datetime
import importlib.util
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("publish", HERE / "publish.py")
publish = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(publish)

COMMIT = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
VERSION_ID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\Z")
HELD = publish.HELD

# The staging workflows, by file; each one's job that changes staging, and the step in it that does.
WORKFLOWS = {"coordinator": "fleet-coordinator-deploy.yml", "web": "fleet-web-deploy.yml",
             "gate": "fleet-release-gate.yml", "smoke": "fleet-viewer-smoke.yml"}
RELEASES = {"coordinator": ("deploy", "Publish and release the staging coordinator"),
            "web": ("Deploy fcvm-fleet-stage", "Deploy")}
VERIFY_JOB = "Verify or roll back"
VIEWERS = ("IPv4-only viewer", "IPv6 viewer")
RUNS = 30
# The gate writes its record inside its verify job; the two clocks are AWS's and GitHub's.
SKEW_SECONDS = 60
# One GitHub-hosted job runs at most 6 hours; the hold leaves the rest of it for the release.
HOLD_SECONDS = 350 * 60
POLL_SECONDS = 60


class Skip(Exception):
    """Nothing to promote, for a stated reason. Not a failure."""


def output(**values):
    """Write step outputs (JSON for anything but a string) and echo them."""
    lines = [f"{k}={v if isinstance(v, str) else json.dumps(v, sort_keys=True)}" for k, v in values.items()]
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as handle:
            handle.write("".join(line + "\n" for line in lines))
    for line in lines:
        print(line, flush=True)


def notice(text):
    print(f"::notice::{text}", flush=True)


def when(text):
    """A GitHub timestamp, as seconds since the epoch; None when absent."""
    if not text:
        return None
    return datetime.datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()


def api(path):
    """GET one GitHub REST resource of this repository."""
    request = urllib.request.Request(
        f"https://api.github.com/repos/{os.environ['GITHUB_REPOSITORY']}/{path}",
        headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"], "Accept": "application/vnd.github+json",
                 "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "fcvm-prod-release"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def runs(workflow):
    """The newest runs of a staging workflow on this repository's main, newest first."""
    repo = os.environ["GITHUB_REPOSITORY"]
    found = api(f"actions/workflows/{workflow}/runs?branch=main&per_page={RUNS}")["workflow_runs"]
    return [r for r in found if r.get("head_branch") == "main" and r.get("event") != "pull_request"
            and (r.get("head_repository") or {}).get("full_name") == repo and COMMIT.fullmatch(r.get("head_sha", ""))]


def jobs(run_id):
    return api(f"actions/runs/{int(run_id)}/jobs?filter=latest&per_page=100")["jobs"]


def job(listed, name):
    """A job by its name, also as a called workflow's job (`<caller job> / <name>`)."""
    found = [j for j in listed if j.get("name") == name or str(j.get("name", "")).endswith(" / " + name)]
    return found[0] if len(found) == 1 else None


def released(kind):
    """The commit staging runs for `kind` and when its release finished: of the listed runs (their
    latest attempts), the release step that succeeded last, since a rerun of an older run can deploy
    after a newer one. A release still running stops the promotion; a failed one left the previous."""
    job_name, step_name = RELEASES[kind]
    latest = None
    for run in runs(WORKFLOWS[kind]):
        found = job(jobs(run["id"]), job_name)
        if found is None or found.get("conclusion") == "skipped":
            continue
        if found.get("status") != "completed":
            raise Skip(f"a staging {kind} release is in progress")
        step = next((s for s in found.get("steps", []) if s.get("name") == step_name), None)
        # A failed wrangler deploy leaves the previous Worker version, and a failed coordinator release
        # restores the previous image (decide() checks staging's pointer against the commit found here).
        if step is None or step.get("conclusion") != "success":
            continue
        finished = when(step["completed_at"])
        if latest is None or finished > latest[1]:
            latest = (run["head_sha"], finished)
    if latest is None:
        raise Skip(f"no staging {kind} release among the last {RUNS} runs")
    return latest


def smokes(since):
    """Every staging viewer smoke whose IPv4 and IPv6 viewers both passed, in a run updated after `since`."""
    found = []
    for kind, workflow in WORKFLOWS.items():
        for run in runs(workflow):
            if run.get("status") != "completed" or (when(run.get("updated_at")) or 0) < since:
                continue
            listed = jobs(run["id"])
            v4, v6 = job(listed, VIEWERS[0]), job(listed, VIEWERS[1])
            if not (v4 and v6 and v4.get("conclusion") == v6.get("conclusion") == "success"):
                continue
            verify = job(listed, VERIFY_JOB) if kind == "gate" else None
            found.append({"run": int(run["id"]), "workflow": kind, "start": when(v4["started_at"]),
                          "end": when(v6["completed_at"]),
                          "verify": (when(verify["started_at"]), when(verify["completed_at"]))
                          if verify and verify.get("conclusion") == "success" else None})
    return found


def covers(smoke, floor, verified_at):
    """Whether `smoke` ran against the set: after both releases, and against the verified generation."""
    if smoke["start"] is None or smoke["end"] is None or smoke["start"] < floor:
        return False
    if smoke["start"] >= verified_at:
        return True
    verify = smoke["verify"]
    return bool(verify) and verify[0] - SKEW_SECONDS <= verified_at <= verify[1] + SKEW_SECONDS


def lambda_result(function_env, payload):
    result = publish.invoke(os.environ[function_env], payload)
    if not isinstance(result, dict):
        raise RuntimeError("release function gave no object")
    return result


def entry(release):
    """A release record entry without its own previous."""
    return {k: v for k, v in release.items() if k != "previous"}


def decide():
    """The staging set, prod's current release and what would change; Skip when nothing should."""
    candidate = lambda_result("COORDINATOR_RELEASE", {"action": "candidate"})
    status = lambda_result("IMAGE_RELEASE", {"action": "prod-status"})
    if candidate.get("status") != "ok" or status.get("status") != "ok":
        raise RuntimeError("a release function refused to describe the releases")
    if not status["stage_verified"]:
        raise Skip("staging runs a host and tile generation the image release gate has not verified")
    verified = status["verified"]
    coordinator, coordinator_at = released("coordinator")
    if coordinator not in candidate["stage"]["commits"]:
        raise Skip("staging's coordinator image is not its last successful staging release")
    web, web_at = released("web")
    floor = max(coordinator_at, web_at)
    covering = [s for s in smokes(floor) if covers(s, floor, status["verified_at"])]
    if not covering:
        raise Skip("no staging smoke has run against this set yet")
    smoke = max(covering, key=lambda s: s["end"])
    target = {"coordinator": {"commit": coordinator, "digest": candidate["stage"]["digest"]}, "web_commit": web,
              "generation": verified["generation"], "build_id": verified["build_id"], "smoke_run": smoke["run"]}

    record = candidate.get("release")
    prod_receipt = status["prod"]
    if record is not None:
        if record["coordinator"]["digest"] != candidate["prod"]["digest"] \
                or {"generation": record["generation"], "build_id": record["build_id"]} != prod_receipt:
            raise Skip("prod changed outside a promotion: its release record does not describe it")
        prior = entry(record)
    else:
        commits = candidate["prod"]["commits"]
        prior = {"coordinator": {"commit": commits[0] if len(commits) == 1 else None, "digest": candidate["prod"]["digest"]},
                 "web_commit": None, "worker_version": None, "generation": prod_receipt["generation"],
                 "build_id": prod_receipt["build_id"], "smoke_run": None}
    changes = {"image": {"generation": target["generation"], "build_id": target["build_id"]} != prod_receipt,
               "coordinator": target["coordinator"]["digest"] != prior["coordinator"]["digest"],
               "web": prior["web_commit"] != web}
    if not any(changes.values()):
        raise Skip("prod already runs the staging set")
    return target, prior, changes


def hold(image, deadline=None, clock=time.time, sleep=time.sleep):
    """Wait until prod may change: asleep when its images change, else no active viewer for 5 minutes.
    Returns whether it may; never changes anything."""
    deadline = deadline or clock() + HOLD_SECONDS
    while True:
        if image:
            status = lambda_result("IMAGE_RELEASE", {"action": "prod-status"})
            ok, reason = status.get("asleep") is True, status.get("reason")
        else:
            status = lambda_result("COORDINATOR_RELEASE", {"action": "quiet"})
            ok, reason = status.get("status") == "quiet", status.get("reason")
        if ok:
            return True
        if clock() >= deadline:
            notice(f"held: prod did not become free to change ({reason if isinstance(reason, str) else 'unknown'}); "
                   "nothing changed, and the next promotion run tries again")
            return False
        sleep(POLL_SECONDS)


def receipt_of(value):
    receipt = {"generation": value["generation"], "build_id": value["build_id"]}
    if type(receipt["generation"]) is not int or not isinstance(receipt["build_id"], str):
        raise ValueError("a receipt is a generation and a build id")
    return receipt


def image_promote(receipt):
    result = lambda_result("IMAGE_RELEASE", {"action": "promote-prod", "receipt": receipt})
    status = result.get("status")
    if status == "held":
        raise publish.Held("prod woke before its images moved")
    if status not in ("promoted", "unchanged") or receipt_of(result) != receipt:
        raise RuntimeError(f"the prod image promotion did not complete ({status})")
    return status


def image_rollback(receipt, want, deadline=None, clock=time.time, sleep=time.sleep):
    """Return prod's images from `receipt` to their previous release, which must be `want`; waits for
    prod to sleep (the templates change only then) until the deadline."""
    deadline = deadline or clock() + HOLD_SECONDS
    current = lambda_result("IMAGE_RELEASE", {"action": "prod-status"}).get("prod")
    if current == want:
        return "unchanged"  # the promotion never moved them
    if current != receipt:
        raise RuntimeError("prod's images are neither the promoted release nor the previous one")
    while True:
        result = lambda_result("IMAGE_RELEASE", {"action": "rollback-prod", "receipt": receipt})
        status = result.get("status")
        if status in ("rolled_back", "unchanged"):
            if receipt_of(result) != want:
                raise RuntimeError("prod's images returned to a release other than the previous one")
            return status
        if status != "held":
            raise RuntimeError(f"the prod image rollback did not complete ({status})")
        if clock() >= deadline:
            raise RuntimeError("prod did not sleep, so its images could not be rolled back")
        sleep(POLL_SECONDS)


def record(target, worker_version, previous):
    release = {**target, "worker_version": worker_version or None,
               "previous": previous if previous and (previous.get("coordinator") or {}).get("commit") else None}
    result = lambda_result("COORDINATOR_RELEASE", {"action": "record", "release": release})
    if result.get("status") not in ("recorded", "unchanged"):
        raise RuntimeError(f"the release record was not written ({result.get('status')})")
    return result["status"]


def enabled():
    """Whether to look for a promotion at all: not while the prod stack is off."""
    if os.environ.get("ENABLED") != "true":
        notice("the prod stack is off (repository variable FLEET_PROD_ENABLED is not true): nothing to promote, "
               "and staging is untouched")
        return False
    return True


def main(argv):
    action, rest = argv[0], argv[1:]
    try:
        if action == "enabled":
            output(run="true" if enabled() else "false")
        elif action == "decide":
            try:
                target, prior, changes = decide()
            except Skip as skip:
                notice(f"nothing promoted: {skip}")
                output(promote="false")
                return 0
            output(promote="true", target=target, prior=prior, image=str(changes["image"]).lower(),
                   coordinator=str(changes["coordinator"]).lower(), web=str(changes["web"]).lower())
        elif action == "hold":
            if rest[:1] != ["--image"] or rest[1:] not in (["true"], ["false"]):
                raise ValueError("hold --image true|false")
            output(free="true" if hold(rest[1] == "true") else "false")
        elif action == "image-promote":
            output(image=image_promote(receipt_of(json.loads(rest[0]))))
        elif action == "image-rollback":
            output(image=image_rollback(receipt_of(json.loads(rest[0])), receipt_of(json.loads(rest[1]))))
        elif action == "record":
            output(record=record(json.loads(rest[0]), rest[1] or None, json.loads(rest[2])))
        elif action == "current":
            release = lambda_result("COORDINATOR_RELEASE", {"action": "candidate"}).get("release")
            if not release or not release.get("previous"):
                raise RuntimeError("prod has no recorded previous release to roll back to")
            output(current=entry(release), previous=release["previous"])
        else:
            raise ValueError("unknown action")
    except publish.Held:
        print(json.dumps({"status": "held"}), flush=True)
        return HELD
    except Exception as error:
        # Fixed text and the error type only: never a response body or a credential.
        print(json.dumps({"status": "failed", "action": action, "errorType": type(error).__name__,
                          "error": str(error) if isinstance(error, (RuntimeError, ValueError)) and not isinstance(
                              error, publish.CommandFailed) else None,
                          **({"code": error.code} if isinstance(error, publish.CommandFailed) and error.code else {})}),
              flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
