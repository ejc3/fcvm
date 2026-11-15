#!/usr/bin/env python3
"""The image release gate's client: .github/workflows/fleet-release-gate.yml runs it.

The gate is a Lambda in the control account (ejc3/aws fcvm-fleet-ami-pipeline.tf,
`fcvm_ami_release_gate`; publish.py `gate_handler`). Its status names the live release receipt
(`generation`, `build_id`) and the placement serving it: `current` (every live host and tile runs
the receipt's images), `settled` (every live host has been up long enough to serve), the number
of live hosts on the release's host image, and a `digest` of that placement. `verify` and
`rollback` take the receipt and the digest and the gate refuses either if the live receipt or the
placement has moved (ejc3/aws#862).

    release_gate.py status
    release_gate.py summary SMOKE.jsonl        (in each smoke family's job)
    release_gate.py decide --before STATUS --v4 SUMMARY --v6 SUMMARY

Only a verdict the release caused decides anything:
- both families ran every mode's probes (each reported its summary line), and
- the placement was ready before the probes and is identical after them: the same receipt, the
  same digest, current and settled.
Then a pass on both families verifies the receipt and a failure on either rolls it back.
Anything else (a family that did not report: setup, credentials, runner, an unreachable
coordinator; or a placement that is old, mixed, changing or unobservable) decides nothing, and a
later run smokes the release again.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile

FUNCTION = "fcvm-fleet-ami-release-gate"
MODES = ["wall", "phones", "windows", "minecraft", "potato"]


def invoke(payload, run=subprocess.run):
    if os.environ.get("RELEASE_GATE") != FUNCTION:
        raise SystemExit(f"release_gate: RELEASE_GATE must name {FUNCTION}")
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "response.json")
        result = run(["aws", "lambda", "invoke", "--function-name", FUNCTION, "--cli-binary-format", "raw-in-base64-out",
                      "--payload", json.dumps(payload, separators=(",", ":")), out], capture_output=True, text=True)
        if result.returncode != 0:
            raise SystemExit(f"release_gate: invoking {FUNCTION} failed (exit {result.returncode})")
        meta = json.loads(result.stdout or "{}")
        if meta.get("FunctionError"):
            raise SystemExit(f"release_gate: {FUNCTION} raised {meta['FunctionError']}")
        with open(out) as handle:
            return json.load(handle)


def outputs(values):
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as handle:
            for key, value in values.items():
                handle.write(f"{key}={value}\n")


def receipt(status):
    """The receipt a status names, or None when it names none."""
    if (isinstance(status, dict) and type(status.get("generation")) is int
            and isinstance(status.get("build_id"), str) and status["build_id"]):
        return {"generation": status["generation"], "build_id": status["build_id"]}
    return None


def placement_problem(status):
    """Why a status's placement cannot carry a verdict, or None when it can."""
    if receipt(status) is None:
        return "the status names no release receipt"
    p = status.get("placement")
    if not isinstance(p, dict):
        return "the status reports no placement"
    if p.get("current") is not True:
        return "a live host or tile runs an image other than the release's"
    if p.get("settled") is not True:
        return "a live host is still coming up"
    if type(p.get("release_hosts")) is not int or p["release_hosts"] < 1:
        return "no live host runs the release's host image"
    if not isinstance(p.get("digest"), str) or not p["digest"]:
        return "the placement has no digest"
    return None


def family_summary(raw):
    """A family's smoke summary, or None when the family did not report one."""
    try:
        value = json.loads(raw) if raw else None
    except ValueError:
        return None
    if (isinstance(value, dict) and type(value.get("probes")) is int and value["probes"] > 0
            and type(value.get("failed")) is int and 0 <= value["failed"] <= value["probes"]
            and value.get("modes") == MODES):
        return value
    return None


def decide(before, v4, v6, run):
    problem = placement_problem(before)
    if problem:
        return None, f"no verdict: before the probes {problem}"
    summaries = {"v4": family_summary(v4), "v6": family_summary(v6)}
    missing = [name for name, value in summaries.items() if value is None]
    if missing:
        return None, (f"no verdict: {' and '.join(missing)} reported no probe summary "
                      "(setup, credentials, runner or coordinator)")
    after = invoke({"action": "status"}, run)
    problem = placement_problem(after)
    if problem:
        return None, f"no verdict: after the probes {problem}"
    if receipt(after) != receipt(before) or after["placement"]["digest"] != before["placement"]["digest"]:
        return None, "no verdict: the release or its placement changed during the probes"
    failed = summaries["v4"]["failed"] + summaries["v6"]["failed"]
    action = "rollback" if failed else "verify"
    return {"action": action, "receipt": receipt(before), "placement": before["placement"]["digest"]}, None


def main(argv=None, run=subprocess.run):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("status")
    summary = sub.add_parser("summary")
    summary.add_argument("smoke")
    d = sub.add_parser("decide")
    d.add_argument("--before", required=True)
    d.add_argument("--v4", default="")
    d.add_argument("--v6", default="")
    args = parser.parse_args(argv)
    if args.command == "status":
        status = invoke({"action": "status"}, run)
        ready = status.get("verified") is False and placement_problem(status) is None
        print(json.dumps(status))
        outputs({"ready": "true" if ready else "false", "status": json.dumps(status, separators=(",", ":"))})
        return 0
    if args.command == "summary":
        last = ""
        try:
            with open(args.smoke) as handle:
                for line in handle:
                    try:
                        value = json.loads(line)
                    except ValueError:
                        continue
                    if isinstance(value, dict) and "probes" in value and "family" in value:
                        last = json.dumps(value, separators=(",", ":"))
        except OSError:
            last = ""
        outputs({"summary": last})
        return 0
    try:
        before = json.loads(args.before)
    except ValueError:
        before = None
    payload, why = decide(before, args.v4, args.v6, run)
    if payload is None:
        print(f"release_gate: {why}; a later run smokes the release again")
        return 0
    answer = invoke(payload, run)
    print(json.dumps(answer))
    if answer.get("status") == "verified":
        print(f"release_gate: verified generation {payload['receipt']['generation']} ({payload['receipt']['build_id']})")
        return 0
    if answer.get("status") == "rolled_back":
        # Moving the release pointer back is one thing; hosts already running the withdrawn image
        # are another, recovered by the fleet as they are replaced.
        print(f"release_gate: release pointer: rolled back to {json.dumps(answer.get('pointer', 'not reported'))}")
        print(f"release_gate: running hosts: {json.dumps(answer.get('hosts', 'not reported'))}")
        return 0
    raise SystemExit(f"release_gate: the gate refused {payload['action']} of generation "
                     f"{payload['receipt']['generation']}: {answer.get('reason', answer)}")


if __name__ == "__main__":
    sys.exit(main())
