#!/usr/bin/env python3
"""The image release gate's client: .github/workflows/fleet-release-gate.yml runs it.

The gate is a Lambda in the control account (ejc3/aws fcvm-fleet-ami-pipeline.tf,
`fcvm_ami_release_gate`; publish.py `gate_handler`). Its status names the live release receipt
(`generation`, `build_id`) and the placement serving it: `current` (every live host and tile runs
the receipt's images), `settled` (every live host has been up long enough to serve),
`release_hosts` and `release_tiles` (the instance ids of the live hosts on the release's host
image and of the live EC2 tiles on its tile image), and a `digest` of that placement.
`changed_kinds` lists which image kinds differ from the previous release. `verify` and `rollback` take the receipt and the digest and the gate refuses either if the live receipt or
the placement has moved (ejc3/aws#862). Instance ids are never printed or handed on: `status`
replaces them by digests (attribution_digest), the form the smoke records them in.

    release_gate.py status
    release_gate.py summary SMOKE.jsonl        (in each smoke family's job)
    release_gate.py decide --before STATUS --v4 SUMMARY --v6 SUMMARY

Only a verdict the release caused decides anything:
- both families ran every mode's probes (each reported its summary line),
- every probe is attributed: the smoke recorded, for each, the hosts or EC2 tile instances /live
  said served it, every host is one of the after-status's release hosts and every tile instance
  one of its release tiles, and
- the placement was ready before the probes and is identical after them: the same receipt, the
  same digest, current and settled.
All passes verify the receipt. An attributed, positive application failure rolls it back
only if that probe served a changed image and neither family has an inconclusive probe.
Failure on an unchanged image alone does not verify or roll back the receipt. A socket, DNS, TLS, authentication,
setup, decoder, timeout or coordinator failure still fails the smoke, but it does not prove
the release failed. Missing or ambiguous evidence decides nothing, and a later run retries.
The rollback includes the failing family's hashed host and tile attribution so the gate can
validate it again against the live placement under the publication lock.
"""
import argparse
import hashlib
import json
import re
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


DIGEST = re.compile(r"[0-9a-f]{64}")


def attribution_digest(instance):
    """An instance id as the smoke and the status output name it (fleet/coord/stage-smoke.py
    computes the same one; fleet/coord/test_release.py pins the two together)."""
    return hashlib.sha256(b"fcvm-release-gate\0" + instance.encode()).hexdigest()


def redact(status):
    """The status with its release instance ids replaced by their digests."""
    value = json.loads(json.dumps(status))
    placement = value.get("placement") if isinstance(value, dict) else None
    if isinstance(placement, dict):
        for key in ("release_hosts", "release_tiles"):
            if isinstance(placement.get(key), list):
                placement[key] = [attribution_digest(i) if isinstance(i, str) else None for i in placement[key]]
    return value


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
    for key in ("release_hosts", "release_tiles"):
        if not isinstance(p.get(key), list) or not all(isinstance(i, str) and DIGEST.fullmatch(i) for i in p[key]):
            return f"the placement does not list its {key}"
    if not p["release_hosts"]:
        return "no live host runs the release's host image"
    if not isinstance(p.get("digest"), str) or not p["digest"]:
        return "the placement has no digest"
    kinds = p.get("changed_kinds")
    if (not isinstance(kinds, list) or any(kind not in ("host", "tile") for kind in kinds)
            or len(set(kinds)) != len(kinds)):
        return "the placement does not identify its changed image kinds"
    return None


def family_summary(raw, family):
    """A family's smoke summary, or None when that family did not report one."""
    try:
        value = json.loads(raw) if raw else None
    except ValueError:
        return None
    if (isinstance(value, dict) and value.get("family") == family
            and type(value.get("probes")) is int and value["probes"] > 0
            and type(value.get("failed")) is int and 0 <= value["failed"] <= value["probes"]
            and value.get("modes") == MODES):
        return value
    return None


def unattributed(summary, placement):
    """Why a family's probes cannot be laid on the release, or None when each one can: every probe
    names a host or tile instance, every host is a release host and every tile instance a release
    tile. A probe that ran on something else, or that /live named nothing for, is no evidence
    about the release, whatever it found."""
    entries = summary.get("attribution")
    if not isinstance(entries, list) or len(entries) != summary["probes"]:
        return "does not attribute every probe"
    hosts, tiles = set(placement["release_hosts"]), set(placement["release_tiles"])
    modes = set()
    failed = 0
    for entry in entries:
        named = (entry.get("hosts"), entry.get("instances")) if isinstance(entry, dict) else (None, None)
        if not all(isinstance(n, list) and all(isinstance(i, str) and DIGEST.fullmatch(i) for i in n) for n in named):
            return "has a probe without host attribution"
        if not named[0] and not named[1]:
            return "has a probe /live named no host or tile instance for"
        if not set(named[0]) <= hosts or not set(named[1]) <= tiles:
            return "has a probe served by a host or tile instance not on the release's images"
        mode, verdict, ok = entry.get("mode"), entry.get("verdict"), entry.get("ok")
        if mode not in (*MODES, "aggregate"):
            return "has a probe for an unknown mode"
        if verdict not in ("pass", "image_failure", "inconclusive") or ok is not (verdict == "pass"):
            return "has a probe without a valid verdict"
        if verdict == "image_failure" and (mode in ("aggregate", "potato")
                or len(named[0]) + len(named[1]) != 1 or (named[1] and mode != "wall")):
            return "has an image failure without one direct host or tile instance"
        modes.add(mode)
        failed += not ok
    if not set(MODES) <= modes:
        return "has no probe for at least one required mode"
    if failed != summary["failed"]:
        return "has a failure count inconsistent with its probes"
    return None


def agent_snapshot(raw):
    """The coordinator's registered agents after the probes (fleet/coord/stage-smoke.py --agents):
    {"agents": [digests], "observed_at": ms}, or None when there is none to read."""
    try:
        value = json.loads(raw) if raw else None
    except ValueError:
        return None
    if (isinstance(value, dict) and set(value) == {"agents", "observed_at"}
            and isinstance(value["agents"], list)
            and all(isinstance(a, str) and DIGEST.fullmatch(a) for a in value["agents"])
            and type(value["observed_at"]) is int and value["observed_at"] > 0):
        return value
    return None


def decide(before, v4, v6, run, agents=""):
    problem = placement_problem(before)
    if problem:
        return None, f"no verdict: before the probes {problem}"
    summaries = {"v4": family_summary(v4, "v4"), "v6": family_summary(v6, "v6")}
    missing = [name for name, value in summaries.items() if value is None]
    snapshot = agent_snapshot(agents)
    if missing and snapshot is None:
        return None, (f"no verdict: {' and '.join(missing)} reported no probe summary "
                      "(setup, credentials, runner or coordinator)")
    after = redact(invoke({"action": "status"}, run))
    problem = placement_problem(after)
    if problem:
        return None, f"no verdict: after the probes {problem}"
    changed = set(after["placement"]["changed_kinds"])
    if (receipt(after) != receipt(before) or after["placement"]["digest"] != before["placement"]["digest"]
            or changed != set(before["placement"]["changed_kinds"])):
        return None, "no verdict: the release or its placement changed during the probes"
    # A host image that boots but never starts its agent leaves the smoke no tile to probe. The
    # coordinator then lists none of the release's hosts among its agents, and that, on a settled
    # placement that changed the host image, is the release's failure. The gate (ejc3/aws
    # publish.py) checks the hosts are exactly the live release hosts and have been up long
    # enough that an agent would have registered.
    hosts = after["placement"]["release_hosts"]
    if snapshot is not None and not set(hosts) & set(snapshot["agents"]):
        if "host" not in changed:
            return None, "no verdict: no release host has an agent, but this release did not change the host image"
        return {"action": "rollback", "receipt": receipt(before), "placement": before["placement"]["digest"],
                "evidence": {"kind": "no_agent", "hosts": sorted(hosts),
                             "observed_at": snapshot["observed_at"]}}, None
    if missing:
        return None, (f"no verdict: {' and '.join(missing)} reported no probe summary "
                      "(setup, credentials, runner or coordinator)")
    for name, summary in summaries.items():
        problem = unattributed(summary, after["placement"])
        if problem:
            return None, f"no verdict: {name} {problem}"
    evidence = None
    saw_failure = False
    for family, summary in summaries.items():
        for entry in summary["attribution"]:
            if entry["verdict"] == "inconclusive":
                return None, f"no verdict: {family} has an inconclusive probe"
            if entry["verdict"] != "image_failure":
                continue
            saw_failure = True
            # Each positive failure names one direct instance. An unchanged image does
            # not prove why this release fails and cannot mask an eligible proof later.
            kind = "host" if entry["hosts"] else "tile"
            if evidence is None and kind in changed:
                evidence = {"kind": "image_failure", "family": family,
                            "hosts": entry["hosts"], "instances": entry["instances"]}
    if saw_failure and evidence is None:
        return None, "no verdict: the failed probes served only unchanged images"
    payload = {"action": "rollback" if evidence else "verify", "receipt": receipt(before),
               "placement": before["placement"]["digest"]}
    if evidence:
        payload["evidence"] = evidence
    return payload, None


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
    d.add_argument("--agents", default="")
    args = parser.parse_args(argv)
    if args.command == "status":
        status = redact(invoke({"action": "status"}, run))
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
    payload, why = decide(before, args.v4, args.v6, run, args.agents)
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
