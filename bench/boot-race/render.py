#!/usr/bin/env python3
"""Render the boot-race page from one campaign's records.

Reads <results>/samples.jsonl, <results>/lambda-reports.json and <results>/setup.json (all written on the campaign
box), and writes docs/boot-race.html from page.html with the campaign's data inlined. The data carries no instance,
image, task or request ids and no addresses: only lane, round, the sample's recorded start time, its duration, and the
identity checks the page reports (did the environment change, was there an Init Duration).

    python3 bench/boot-race/render.py <results dir> [--out docs/boot-race.html]
"""

import argparse
import datetime
import json
import math
import os
import re
import statistics
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Lane order on the page and what each one is. `start` names the event t0 is taken at; the end event is the same for
# every lane: a GET from the campaign host has read a complete 200 answer from the workload.
LANES = {
    "fcvm-uffd": {
        "name": "fcvm restore, UFFD",
        "short": "fcvm UFFD",
        "start": "fcvm snapshot run --pid <serve> (memory paged in by a running fcvm snapshot serve)",
    },
    "fcvm-file": {
        "name": "fcvm restore, memory file",
        "short": "fcvm file",
        "start": "fcvm snapshot run --snapshot <tag> (Firecracker maps the memory file; the wall's path)",
    },
    "lambda-warm": {
        "name": "Lambda, warm",
        "short": "Lambda warm",
        "start": "HTTPS GET to the function URL, environment already running",
    },
    "ecs-ec2": {
        "name": "ECS on EC2, warm host",
        "short": "ECS on EC2",
        "start": "RunTask on a running container instance, image already pulled",
    },
    "lambda-cold": {
        "name": "Lambda, cold start",
        "short": "Lambda cold",
        "start": "HTTPS GET to the function URL, after an untimed code upload that retires the function's "
                 "execution environments",
    },
    "ec2-hibernate": {
        "name": "EC2 hibernate resume",
        "short": "EC2 hibernate",
        "start": "StartInstances of an instance stopped with Hibernate=true",
    },
    "ec2-start": {
        "name": "EC2 start of a stopped instance",
        "short": "EC2 start",
        "start": "StartInstances of a stopped instance",
    },
    "ecs-fargate": {
        "name": "ECS on Fargate",
        "short": "Fargate",
        "start": "RunTask, launch type FARGATE",
    },
    "ec2-launch": {
        "name": "EC2 launch from an AMI",
        "short": "EC2 launch",
        "start": "RunInstances from an AMI with the workload installed",
    },
}

# What the page says each lane's identity evidence shows, filled from the records ({n}, {new_proc}, ...).
CHECKS = [
    ["fcvm-uffd", "{n} samples answered from {new_proc} workload process and {new_boot} kernel boot: every restore "
                  "resumed the snapshot's own process (race.py refuses any other)."],
    ["fcvm-file", "{n} samples answered from {new_proc} workload process and {new_boot} kernel boot: every restore "
                  "resumed the snapshot's own process (race.py refuses any other)."],
    ["lambda-warm", "{n} samples answered from {new_proc} runtime process; {init} of {report} REPORT lines carried "
                    "an Init Duration. race.py refuses a warm sample from a new process."],
    ["lambda-cold", "{n} samples, each from a new runtime process; {init} of {report} REPORT lines carried an Init "
                    "Duration. Before each sample race.py uploads new code so Lambda starts a new "
                    "execution environment, and it refuses a sample whose REPORT line has no Init Duration."],
    ["ecs-ec2", "{n} samples answered from {new_proc} container processes on {new_boot} kernel boot (the one "
                "container instance)."],
    ["ecs-fargate", "{n} samples answered from {new_proc} processes on {new_boot} kernel boots: one per task."],
    ["ec2-hibernate", "{n} samples, {same_proc} of them from the same process that answered before the instance "
                      "hibernated, on {new_boot} kernel boot: every start resumed the hibernated kernel. race.py "
                      "refuses a resume that booted."],
    ["ec2-start", "{n} samples answered from {new_proc} processes and {new_boot} kernel boots: every start booted."],
    ["ec2-launch", "{n} samples answered from {new_proc} processes and {new_boot} kernel boots: one per instance."],
]

# Lanes the driver has but a campaign may not run, with the reason the page gives, and the setup row that describes
# each one (dropped when the lane did not run).
NOT_RUN = {
    "ecs-fargate": "The campaign's AWS configuration (ejc3/aws boot-race.tf) keeps this lane off. A Fargate task "
                   "without a public IP pulls its image through ECR interface endpoints, which bill by the hour and "
                   "change ECR DNS for the whole VPC, and they have no reviewed teardown yet.",
}
SETUP_ROW_LANE = {"ECS on Fargate": "ecs-fargate", "Fargate platform": "ecs-fargate"}
SETUP_ROW_NAME = {"Lambda, cold": "Lambda, cold start"}

FORBIDDEN = [
    re.compile(r"\b(?:i|eni|vol|snap|sg|subnet|lt)-[0-9a-f]{8,17}\b"),
    re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"),
    re.compile(r"\b[0-9a-f]{32}\b"),
    re.compile(r"lambda-url|amazonaws\.com"),
    re.compile(r"\b[0-9a-f]{1,4}(?::[0-9a-f]{0,4}){2,7}\b", re.I),
    re.compile(r"\bami-[0-9a-f]{8,17}\b"),
    re.compile(r"(?<![\w-])(?<!\d\.)\d{12}(?![\w-])(?!\.\d)"),
    re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    re.compile(r"arn:aws:"),
]


SCRUB = [
    (re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"), "<address>"),
    (re.compile(r"\b(?:i|eni|vol|snap|sg|subnet|lt|ami)-[0-9a-f]{8,17}\b"), "<id>"),
    (re.compile(r"arn:aws:\S+"), "<arn>"),
    (re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"), "<id>"),
    (re.compile(r"https?://\S+"), "<url>"),
    (re.compile(r"(?<![\w-])(?<!\d\.)\d{12}(?![\w-])(?!\.\d)"), "<account>"),
]


def cold_evidence_missing(rec):
    """A lambda-cold record the page may not call a cold start: one taken without a code upload before it
    or without an Init Duration after it (records from a driver that did neither, or a resumed campaign's)."""
    if rec.get("lane") != "lambda-cold" or not rec.get("ok"):
        return None
    if not rec.get("generation"):
        return "no code upload before the sample: not shown to be a new execution environment"
    if rec.get("init_duration_ms") is None:
        return "no Init Duration in the invocation's REPORT line: not a cold start"
    return None


def scrub(text):
    for rx, sub in SCRUB:
        text = rx.sub(sub, text)
    return text[:200]


def median(xs):
    return statistics.median(xs)


def nearest_rank(xs, q):
    """Nearest-rank percentile: the smallest sample at or above the fraction q of the samples."""
    s = sorted(xs)
    return s[max(0, math.ceil(q * len(s)) - 1)]


def p95(xs):
    return nearest_rank(xs, 0.95)


def p99(xs):
    return nearest_rank(xs, 0.99)


def build(results):
    with open(os.path.join(results, "setup.json")) as f:
        setup = json.load(f)
    reports = {}
    rp = os.path.join(results, "lambda-reports.json")
    if os.path.exists(rp):
        with open(rp) as f:
            reports = json.load(f)
    recs = []
    with open(os.path.join(results, "samples.jsonl")) as f:
        for line in f:
            if line.strip():
                recs.append(json.loads(line))
    recs.sort(key=lambda r: r.get("t_start_unix_ms") or r.get("t_failed_unix_ms") or 0)
    for r in recs:
        refused = cold_evidence_missing(r)
        if refused:
            r.update(ok=False, error=f"IdentityCheck: {refused}")

    lanes = []
    for lid, meta in LANES.items():
        mine = [r for r in recs if r["lane"] == lid]
        if not mine:
            continue
        ok = [r for r in mine if r.get("ok")]
        samples = []
        raw = []
        seen_proc, seen_boot = set(), set()
        checks = {"new_proc": 0, "new_boot": 0, "same_proc_as_previous": 0, "same_boot_as_previous": 0,
                  "init_duration": 0, "report": 0}
        for i, r in enumerate(ok, 1):
            a = r["answer"]
            s = {"i": i, "round": r["round"], "t": r["t_start_unix_ms"], "ms": round(r["ms"], 1)}
            raw.append(r["ms"])
            if a["proc_id"] not in seen_proc:
                checks["new_proc"] += 1
            if a["boot_id"] not in seen_boot:
                checks["new_boot"] += 1
            seen_proc.add(a["proc_id"])
            seen_boot.add(a["boot_id"])
            if r.get("same_proc_as_previous"):
                checks["same_proc_as_previous"] += 1
            if r.get("same_boot_as_previous"):
                checks["same_boot_as_previous"] += 1
            if "api_ms" in r:
                s["api_ms"] = round(r["api_ms"], 1)
            rep = reports.get(r.get("request_id") or "")
            if rep:
                checks["report"] += 1
                if rep.get("init_duration"):
                    checks["init_duration"] += 1
                    s["init_ms"] = float(rep["init_duration"].split()[0])
            samples.append(s)
        ms = raw
        failed = [
            {"round": r["round"], "t": r.get("t_attempt_unix_ms") or r.get("t_failed_unix_ms"), "error": scrub(r.get("error", ""))}
            for r in mine if not r.get("ok")
        ]
        lanes.append(
            {
                "id": lid,
                **meta,
                "n": len(ok),
                "failures": len(mine) - len(ok),
                "failed": failed,
                "median_ms": round(median(ms), 1) if ms else None,
                "p95_ms": round(p95(ms), 1) if ms else None,
                "p99_ms": round(p99(ms), 1) if ms else None,
                "min_ms": round(min(ms), 1) if ms else None,
                "max_ms": round(max(ms), 1) if ms else None,
                "checks": checks,
                "samples": samples,
            }
        )
    lanes.sort(key=lambda l: (l["median_ms"] is None, l["median_ms"] or 0))
    starts = [s["t"] for l in lanes for s in l["samples"]] + [f["t"] for l in lanes for f in l["failed"] if f["t"]]
    setup = dict(setup)
    setup["checks"] = CHECKS
    days = sorted({datetime.datetime.fromtimestamp(t / 1000, datetime.timezone.utc).strftime("%Y-%m-%d")
                   for t in starts})
    loads = [r["load1"] for r in recs if "load1" in r]
    fargate = sorted({r["platform_version"] for r in recs if r.get("platform_version")})
    derived = [["Date (UTC)", " to ".join(days[:1] + days[1:][-1:])]]
    derived.append(["Rounds", f"{len({r['round'] for r in recs})}, lane order rotated by one lane per round"])
    if fargate:
        derived.append(["Fargate platform", ", ".join(fargate)])
    if loads:
        derived.append(["Campaign host load", f"1-minute load average {min(loads):.2f} to {max(loads):.2f} "
                        f"when each lane attempt began (before any wait for the instance to be ready)"])
    run = {r["lane"] for r in recs}
    setup["rows"] = [[SETUP_ROW_NAME.get(row[0], row[0]), row[1]] for row in derived + list(setup.get("rows", []))
                     if row[0] not in SETUP_ROW_LANE or SETUP_ROW_LANE[row[0]] in run]
    not_run = [{"id": lid, "name": LANES[lid]["name"], "why": NOT_RUN.get(lid, "Not run in this campaign.")}
               for lid in LANES if lid not in run]
    data = {
        "not_run": not_run,
        "setup": setup,
        "first_sample_unix_ms": min(starts) if starts else None,
        "last_sample_unix_ms": max(starts) if starts else None,
        "rounds": len({r["round"] for r in recs}),
        "lanes": lanes,
    }
    return data


# The campaign's raw records go to the public records repository: random per-process and per-request ids may stay,
# addresses and resource or account ids may not.
FORBIDDEN_IN_RECORDS = [FORBIDDEN[i] for i in (0, 3, 4, 5, 6, 7, 8)]


def public_records(results, out):
    """Write the campaign's samples and REPORT lines for the records repository, errors scrubbed, and check them."""
    os.makedirs(out, exist_ok=True)
    lines = []
    with open(os.path.join(results, "samples.jsonl")) as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                if "error" in r:
                    r["error"] = scrub(r["error"])
                lines.append(json.dumps(r, sort_keys=True))
    files = {"samples.jsonl": "\n".join(lines) + "\n"}
    rp = os.path.join(results, "lambda-reports.json")
    if os.path.exists(rp):
        with open(rp) as f:
            files["lambda-reports.json"] = json.dumps(json.load(f), indent=1, sort_keys=True) + "\n"
    with open(os.path.join(results, "setup.json")) as f:
        files["setup.json"] = json.dumps(json.load(f), indent=1) + "\n"
    for name, text in files.items():
        check_public(text, FORBIDDEN_IN_RECORDS)
        with open(os.path.join(out, name), "w") as f:
            f.write(text)
    return sorted(files)


def check_public(text, patterns=None):
    for rx in (FORBIDDEN if patterns is None else patterns):
        m = rx.search(text)
        if m:
            raise SystemExit(f"refusing to write: the page data holds {m.group(0)!r} ({rx.pattern})")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("--out", default=os.path.join(HERE, "..", "..", "docs", "boot-race.html"))
    ap.add_argument("--records-out", help="also write the scrubbed raw records here (bench/boot-race/results/<campaign>)")
    a = ap.parse_args()
    if a.records_out:
        print("records:", public_records(a.results, a.records_out), file=sys.stderr)
    data = build(a.results)
    blob = json.dumps(data, separators=(",", ":"))
    check_public(blob)
    with open(os.path.join(HERE, "page.html")) as f:
        page = f.read()
    marker = "/*BOOT_RACE_DATA*/null"
    if page.count(marker) != 1:
        raise SystemExit("page.html must hold the data marker exactly once")
    out = page.replace(marker, blob.replace("</", "<\\/"))
    with open(a.out, "w") as f:
        f.write(out)
    for l in data["lanes"]:
        print(f"{l['id']:14} n={l['n']:3} fail={l['failures']} median={l['median_ms']} p95={l['p95_ms']} p99={l['p99_ms']}", file=sys.stderr)


if __name__ == "__main__":
    main()
