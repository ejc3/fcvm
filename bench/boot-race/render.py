#!/usr/bin/env python3
"""Render the boot-race page from one campaign's records.

Reads <results>/samples.jsonl, <results>/lambda-reports.json and <results>/setup.json (all written on the campaign
box), and writes docs/boot-race.html from page.html with the campaign's data inlined. The data carries no instance,
image, task or request ids and no addresses: only lane, round, the sample's recorded start time, its duration, and the
identity checks the page reports (did the environment change, was there an Init Duration).

    python3 bench/boot-race/render.py <results dir> [--out docs/boot-race.html]
"""

import argparse
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
        "name": "Lambda, cold every time",
        "short": "Lambda cold",
        "start": "HTTPS GET to the function URL; the previous request's runtime exited",
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

FORBIDDEN = [
    re.compile(r"\b(?:i|eni|vol|snap|sg|subnet|lt)-[0-9a-f]{8,17}\b"),
    re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"),
    re.compile(r"\b[0-9a-f]{32}\b"),
    re.compile(r"lambda-url|amazonaws\.com"),
    re.compile(r"\b[0-9a-f]{1,4}(?::[0-9a-f]{0,4}){2,7}\b", re.I),
    re.compile(r"\bami-[0-9a-f]{8,17}\b"),
    re.compile(r"\b\d{12}\b"),
    re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    re.compile(r"arn:aws:"),
]


SCRUB = [
    (re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"), "<address>"),
    (re.compile(r"\b(?:i|eni|vol|snap|sg|subnet|lt|ami)-[0-9a-f]{8,17}\b"), "<id>"),
    (re.compile(r"arn:aws:\S+"), "<arn>"),
    (re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"), "<id>"),
    (re.compile(r"https?://\S+"), "<url>"),
    (re.compile(r"\b\d{12}\b"), "<account>"),
]


def scrub(text):
    for rx, sub in SCRUB:
        text = rx.sub(sub, text)
    return text[:200]


def median(xs):
    return statistics.median(xs)


def p95(xs):
    """Nearest-rank 95th percentile: the smallest sample at or above 95% of the samples."""
    s = sorted(xs)
    return s[max(0, math.ceil(0.95 * len(s)) - 1)]


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

    lanes = []
    for lid, meta in LANES.items():
        mine = [r for r in recs if r["lane"] == lid]
        if not mine:
            continue
        ok = [r for r in mine if r.get("ok")]
        samples = []
        raw = []
        seen_proc, seen_boot = set(), set()
        checks = {"new_proc": 0, "new_boot": 0, "same_proc_as_previous": 0, "init_duration": 0, "report": 0}
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
                "min_ms": round(min(ms), 1) if ms else None,
                "max_ms": round(max(ms), 1) if ms else None,
                "checks": checks,
                "samples": samples,
            }
        )
    lanes.sort(key=lambda l: (l["median_ms"] is None, l["median_ms"] or 0))
    starts = [s["t"] for l in lanes for s in l["samples"]] + [f["t"] for l in lanes for f in l["failed"] if f["t"]]
    data = {
        "setup": setup,
        "first_sample_unix_ms": min(starts) if starts else None,
        "last_sample_unix_ms": max(starts) if starts else None,
        "rounds": len({r["round"] for r in recs}),
        "lanes": lanes,
    }
    return data


def check_public(text):
    for rx in FORBIDDEN:
        m = rx.search(text)
        if m:
            raise SystemExit(f"refusing to write: the page data holds {m.group(0)!r} ({rx.pattern})")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("--out", default=os.path.join(HERE, "..", "..", "docs", "boot-race.html"))
    a = ap.parse_args()
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
        print(f"{l['id']:14} n={l['n']:3} fail={l['failures']} median={l['median_ms']} p95={l['p95_ms']}", file=sys.stderr)


if __name__ == "__main__":
    main()
