#!/usr/bin/env python3
"""Classify a completed CI failure without executing the failed revision.

The classifier is deliberately narrow. A failed job is infrastructure only
when GitHub's step metadata contains no genuine failure and either:

* the job log contains GitHub's runner-shutdown sentinel, or
* the completed job has the silent runner-loss shape (one still-running step,
  only pending steps after it) and Azure reports that the log blob is missing.

The CI Summary job is derivative when its only failed step is the gate that
reports a failed dependency. It does not turn an otherwise infrastructure-only
run into a mixed failure. Every unknown shape is non-infrastructure.
"""

from __future__ import annotations

import argparse
import datetime
import email.utils
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


RUNNER_SHUTDOWN_SENTINEL = (
    "The runner has received a shutdown signal. This can happen when the runner "
    "service is stopped, or a manually started runner is canceled."
)
DERIVATIVE_JOB = "Summary"
DERIVATIVE_STEP = "Fail if any gating job did not succeed"
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
GH_ESCAPE_REFUSAL = (
    "the response contains terminal escape sequences; pass --allow-escape-sequences"
)


class InputError(RuntimeError):
    """The GitHub response or fixture did not satisfy the input contract."""


def _run_gh(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["gh", *args],
        check=False,
        capture_output=True,
        text=True,
    )


def _positive_int(value: Any, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise InputError(f"{field} must be a positive integer")
    return value


def _validate_jobs(jobs: Any) -> list[dict[str, Any]]:
    if not isinstance(jobs, list):
        raise InputError("jobs must be an array")

    validated: list[dict[str, Any]] = []
    ids: set[int] = set()
    for job in jobs:
        if not isinstance(job, dict):
            raise InputError("every job must be an object")
        job_id = _positive_int(job.get("id"), "job.id")
        if job_id in ids:
            raise InputError(f"duplicate job id {job_id}")
        ids.add(job_id)
        if not isinstance(job.get("name"), str):
            raise InputError(f"job {job_id} has no string name")
        if not isinstance(job.get("status"), str):
            raise InputError(f"job {job_id} has no string status")
        conclusion = job.get("conclusion")
        if conclusion is not None and not isinstance(conclusion, str):
            raise InputError(f"job {job_id} has an invalid conclusion")
        completed_at = job.get("completed_at")
        if completed_at is not None and not isinstance(completed_at, str):
            raise InputError(f"job {job_id} has an invalid completed_at")
        steps = job.get("steps")
        if not isinstance(steps, list):
            raise InputError(f"job {job_id} has no steps array")
        for step in steps:
            if not isinstance(step, dict):
                raise InputError(f"job {job_id} contains a non-object step")
            if not isinstance(step.get("name"), str):
                raise InputError(f"job {job_id} contains a step with no string name")
            if not isinstance(step.get("status"), str):
                raise InputError(f"job {job_id} contains a step with no string status")
            step_conclusion = step.get("conclusion")
            if step_conclusion is not None and not isinstance(step_conclusion, str):
                raise InputError(f"job {job_id} contains a step with an invalid conclusion")
        validated.append(job)
    return validated


def _validate_logs(logs: Any) -> dict[str, dict[str, Any]]:
    if not isinstance(logs, dict):
        raise InputError("logs must be an object keyed by job id")
    validated: dict[str, dict[str, Any]] = {}
    for job_id, evidence in logs.items():
        if not isinstance(job_id, str) or not job_id.isdecimal():
            raise InputError("every logs key must be a decimal job id")
        if not isinstance(evidence, dict):
            raise InputError(f"log evidence for job {job_id} must be an object")
        status = evidence.get("status")
        if status not in {"available", "missing_blob", "unavailable"}:
            raise InputError(f"log evidence for job {job_id} has invalid status")
        if status == "available" and not isinstance(evidence.get("text"), str):
            raise InputError(f"available log evidence for job {job_id} needs text")
        last_modified = evidence.get("last_modified")
        if last_modified is not None and not isinstance(last_modified, str):
            raise InputError(
                f"log evidence for job {job_id} has an invalid last_modified"
            )
        if status == "missing_blob" and (
            evidence.get("http_status") != 404
            or evidence.get("error_code") != "BlobNotFound"
        ):
            raise InputError(
                f"missing log evidence for job {job_id} must be 404 BlobNotFound"
            )
        validated[job_id] = evidence
    return validated


def load_fixture(path: Path) -> tuple[int, list[dict[str, Any]], dict[str, dict[str, Any]]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise InputError(f"cannot load fixture {path}: {error}") from error
    if not isinstance(payload, dict):
        raise InputError("fixture root must be an object")
    return (
        _positive_int(payload.get("run_attempt"), "run_attempt"),
        _validate_jobs(payload.get("jobs")),
        _validate_logs(payload.get("logs")),
    )


def _fetch_jobs(repository: str, run_id: int, run_attempt: int) -> list[dict[str, Any]]:
    jobs: list[dict[str, Any]] = []
    page = 1
    total_count: int | None = None
    while total_count is None or len(jobs) < total_count:
        endpoint = (
            f"repos/{repository}/actions/runs/{run_id}/attempts/{run_attempt}/jobs"
            f"?per_page=100&page={page}"
        )
        result = _run_gh(["api", endpoint])
        if result.returncode != 0:
            raise InputError(
                f"GitHub jobs API failed for run {run_id} attempt {run_attempt}"
            )
        try:
            payload = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise InputError("GitHub jobs API returned invalid JSON") from error
        if not isinstance(payload, dict):
            raise InputError("GitHub jobs API response is not an object")
        count = payload.get("total_count")
        page_jobs = payload.get("jobs")
        if isinstance(count, bool) or not isinstance(count, int) or count < 0:
            raise InputError("GitHub jobs API returned an invalid total_count")
        if total_count is None:
            total_count = count
        elif total_count != count:
            raise InputError("GitHub jobs API total_count changed during pagination")
        if not isinstance(page_jobs, list):
            raise InputError("GitHub jobs API returned no jobs array")
        if not page_jobs and len(jobs) < total_count:
            raise InputError("GitHub jobs API pagination ended before total_count")
        jobs.extend(page_jobs)
        page += 1
    if len(jobs) != total_count:
        raise InputError("GitHub jobs API returned more jobs than total_count")
    return _validate_jobs(jobs)


def _last_modified_header(response: str) -> str | None:
    """The Last-Modified value from a `gh api --include` response, if present.

    Headers end at the first blank line; a log body can contain anything,
    including a line that looks like a header, so parsing must stop there.
    """
    for line in response.splitlines():
        if not line.strip():
            return None
        name, separator, value = line.partition(":")
        if separator and name.strip().lower() == "last-modified":
            return value.strip()
    return None


def _fetch_log(repository: str, job_id: int) -> dict[str, Any]:
    endpoint = f"repos/{repository}/actions/jobs/{job_id}/logs"
    # gh <= 2.96 has no --allow-escape-sequences option and emits captured API
    # output directly. gh >= 2.97 first refuses a response containing terminal
    # escapes and tells the caller to opt in. Start with the command every version
    # accepts, then use the newer option only after this exact CLI has advertised
    # it. The log stays in a subprocess pipe and is never rendered to a terminal.
    # Any other failure, including a changed refusal shape or a failed retry,
    # remains unavailable and therefore classifies fail-closed.
    result = _run_gh(["api", "--include", endpoint])
    if result.returncode != 0 and GH_ESCAPE_REFUSAL in result.stderr:
        result = _run_gh(
            ["api", "--include", "--allow-escape-sequences", endpoint]
        )
    if result.returncode == 0:
        evidence: dict[str, Any] = {"status": "available", "text": result.stdout}
        # `--include` already returns the response headers, so the blob's write
        # time costs no extra request. Absent or malformed, the field is simply
        # omitted and the stale-gap witness cannot fire.
        last_modified = _last_modified_header(result.stdout)
        if last_modified is not None:
            evidence["last_modified"] = last_modified
        return evidence

    response = f"{result.stdout}\n{result.stderr}"
    if "gh: HTTP 404" in response and "BlobNotFound" in response:
        return {
            "status": "missing_blob",
            "http_status": 404,
            "error_code": "BlobNotFound",
        }
    # Authentication, rate limits, 5xx responses, and every other error are not
    # evidence of runner loss. Preserve the conservative result without copying
    # possibly sensitive API output into the classifier report.
    return {"status": "unavailable"}


def fetch_live(
    repository: str, run_id: int, run_attempt: int
) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]]]:
    if not REPOSITORY_RE.fullmatch(repository):
        raise InputError("repository must be OWNER/REPO")
    jobs = _fetch_jobs(repository, run_id, run_attempt)
    logs: dict[str, dict[str, Any]] = {}
    for job in jobs:
        if job.get("conclusion") != "failure" or _is_derivative(job):
            continue
        if _has_genuine_step_failure(job):
            continue
        job_id = _positive_int(job.get("id"), "job.id")
        logs[str(job_id)] = _fetch_log(repository, job_id)
    return jobs, logs


def _failed_steps(job: dict[str, Any]) -> list[dict[str, Any]]:
    return [step for step in job["steps"] if step.get("conclusion") == "failure"]


def _has_genuine_step_failure(job: dict[str, Any]) -> bool:
    # Only success/skipped/cancelled/null can participate in a runner-loss
    # shape. Unknown terminal conclusions fail closed as genuine.
    allowed = {None, "success", "skipped", "cancelled"}
    return any(step.get("conclusion") not in allowed for step in job["steps"])


def _is_derivative(job: dict[str, Any]) -> bool:
    failed = _failed_steps(job)
    return (
        job.get("name") == DERIVATIVE_JOB
        and len(failed) == 1
        and failed[0].get("name") == DERIVATIVE_STEP
        and not any(
            step.get("conclusion") not in {None, "success", "skipped", "failure"}
            for step in job["steps"]
        )
    )


def _has_silent_loss_shape(job: dict[str, Any]) -> bool:
    if job.get("status") != "completed" or job.get("conclusion") != "failure":
        return False
    steps = job["steps"]
    active = [
        index
        for index, step in enumerate(steps)
        if step.get("status") == "in_progress" and step.get("conclusion") is None
    ]
    if len(active) != 1:
        return False
    index = active[0]
    if index == 0 or index == len(steps) - 1:
        return False
    before = steps[:index]
    after = steps[index + 1 :]
    return all(
        step.get("status") == "completed"
        and step.get("conclusion") in {"success", "skipped"}
        for step in before
    ) and all(
        step.get("status") == "pending" and step.get("conclusion") is None
        for step in after
    )


# A log blob that stopped growing this long before GitHub concluded the job is
# a runner that died, not a job that ran. Measured on the live API the two
# populations do not overlap: runner-concluded jobs write their blob within
# 0-2s of completed_at (n=66), agent-death jobs show 569-1149s (n=6). 120s sits
# 60x above the largest healthy gap and 4.7x below the smallest observed loss.
STALE_LOG_SECONDS = 120


def _blob_stale_seconds(
    job: dict[str, Any], evidence: dict[str, Any]
) -> float | None:
    """Seconds between the log blob's last write and the job's conclusion.

    Returns None whenever the answer is not knowable: a missing or unparseable
    timestamp on either side, or a negative gap. Every such case falls through
    to `unknown`, so an unreadable header can never manufacture an
    infrastructure verdict.
    """
    completed_at = job.get("completed_at")
    last_modified = evidence.get("last_modified")
    if not isinstance(completed_at, str) or not isinstance(last_modified, str):
        return None
    try:
        concluded = datetime.datetime.fromisoformat(
            completed_at.replace("Z", "+00:00")
        )
        written = email.utils.parsedate_to_datetime(last_modified)
    except (ValueError, TypeError):
        return None
    if concluded.tzinfo is None or written.tzinfo is None:
        return None
    gap = (concluded - written).total_seconds()
    return gap if gap >= 0 else None


def _classify_root_job(
    job: dict[str, Any], evidence: dict[str, Any] | None
) -> tuple[str, str]:
    if _has_genuine_step_failure(job):
        return "genuine", "step metadata records a genuine failure"
    if job.get("status") != "completed" or job.get("conclusion") != "failure":
        return "unknown", "job is not a completed failure"

    evidence = evidence or {"status": "unavailable"}
    if (
        evidence.get("status") == "available"
        and RUNNER_SHUTDOWN_SENTINEL in evidence.get("text", "")
    ):
        return "infrastructure_explicit", "GitHub runner shutdown sentinel"
    if evidence.get("status") == "missing_blob" and _has_silent_loss_shape(job):
        return "infrastructure_silent", "null-step runner loss with missing log blob"
    if evidence.get("status") == "available" and _has_silent_loss_shape(job):
        # The blob survived but stopped mid-job. Same event as the missing-blob
        # case; GitHub simply kept what the dying agent had already flushed.
        # The null-step shape stays REQUIRED: the gap is a second witness, never
        # the only one.
        stale_by = _blob_stale_seconds(job, evidence)
        if stale_by is not None and stale_by >= STALE_LOG_SECONDS:
            return (
                "infrastructure_silent",
                f"null-step runner loss with log blob stale by {stale_by:.0f}s",
            )
    return "unknown", "no exclusive runner-loss evidence"


def classify(
    run_attempt: int,
    jobs: list[dict[str, Any]],
    logs: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    run_attempt = _positive_int(run_attempt, "run_attempt")
    jobs = _validate_jobs(jobs)
    logs = _validate_logs(logs)
    failed = [job for job in jobs if job.get("conclusion") == "failure"]

    results: list[dict[str, Any]] = []
    roots: list[str] = []
    for job in failed:
        job_id = _positive_int(job.get("id"), "job.id")
        if _is_derivative(job):
            kind, reason = "derivative", "Summary reports a failed gating dependency"
        else:
            kind, reason = _classify_root_job(job, logs.get(str(job_id)))
            roots.append(kind)
        results.append(
            {
                "id": job_id,
                "name": job["name"],
                "kind": kind,
                "reason": reason,
            }
        )

    infrastructure = bool(roots) and all(kind.startswith("infrastructure_") for kind in roots)
    return {
        "run_attempt": run_attempt,
        "classification": "infrastructure" if infrastructure else "not_infrastructure",
        "rerun_failed_jobs": infrastructure and run_attempt == 1,
        "jobs": results,
    }


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--repository")
    parser.add_argument("--run-id", type=int)
    parser.add_argument("--run-attempt", type=int)
    args = parser.parse_args(argv)
    if args.fixture is not None:
        if any(value is not None for value in (args.repository, args.run_id, args.run_attempt)):
            parser.error("--fixture cannot be combined with live-run arguments")
    elif any(value is None for value in (args.repository, args.run_id, args.run_attempt)):
        parser.error("live classification requires --repository, --run-id, and --run-attempt")
    return args


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.fixture is not None:
            run_attempt, jobs, logs = load_fixture(args.fixture)
        else:
            run_attempt = _positive_int(args.run_attempt, "run_attempt")
            run_id = _positive_int(args.run_id, "run_id")
            jobs, logs = fetch_live(args.repository, run_id, run_attempt)
        result = classify(run_attempt, jobs, logs)
    except InputError as error:
        print(f"classification failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
