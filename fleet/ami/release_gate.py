#!/usr/bin/env python3
"""The image release gate's client: .github/workflows/fleet-release-gate.yml runs it.

The gate is a Lambda in the control account (ejc3/aws fcvm-fleet-ami-pipeline.tf,
`fcvm_ami_release_gate`; publish.py `gate_handler`). It answers three requests about the live
fleet image release: `status` (its generation and whether the gate verified it), `verify N` and
`rollback N`, which moves staging back to the live receipt's own previous release unless N was
verified. The workflow asks for the status, runs the staging viewer smoke on an unverified
generation, and reports the smoke's result here:

    release_gate.py status
    release_gate.py decide --generation N --smoke success|failure|cancelled|skipped

A smoke that passed on both families verifies the generation; one that failed on either rolls it
back. A cancelled or skipped smoke decides nothing, and the next run smokes the generation again.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile

FUNCTION = "fcvm-fleet-ami-release-gate"


def request(smoke, generation):
    """The gate request for a smoke result, or None when the smoke decided nothing."""
    if smoke == "success":
        return {"action": "verify", "generation": generation}
    if smoke == "failure":
        return {"action": "rollback", "generation": generation}
    return None


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


def main(argv=None, run=subprocess.run):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("status")
    decide = sub.add_parser("decide")
    decide.add_argument("--generation", type=int, required=True)
    decide.add_argument("--smoke", required=True, choices=["success", "failure", "cancelled", "skipped"])
    args = parser.parse_args(argv)
    if args.command == "status":
        status = invoke({"action": "status"}, run)
        if type(status.get("generation")) is not int or type(status.get("verified")) is not bool:
            raise SystemExit(f"release_gate: unexpected status {status}")
        print(json.dumps(status))
        outputs({"generation": status["generation"], "verified": "true" if status["verified"] else "false"})
        return 0
    payload = request(args.smoke, args.generation)
    if payload is None:
        print(f"release_gate: the smoke was {args.smoke}; generation {args.generation} stays unverified")
        return 0
    answer = invoke(payload, run)
    print(json.dumps(answer))
    if answer.get("status") not in {"verified", "rolled_back"}:
        raise SystemExit(f"release_gate: the gate refused {payload['action']} of generation {args.generation}: "
                         f"{answer.get('reason', answer)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
