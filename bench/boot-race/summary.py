#!/usr/bin/env python3
"""Write the boot-race summary the fleet web page (fleet/web, /boot-race) renders, from the published page.

The numbers are the published page's own data, which render.py built from the campaign's records; this script
copies them and adds what the web page states about the campaign: its id and records commit (from the records
manifest), its UTC window, and each lane's configuration (from the setup rows). It refuses a summary in which a lane
has no configuration, so the web page cannot show a number without saying what was measured.

    python3 bench/boot-race/summary.py --page docs/boot-race.html \
        --manifest bench/boot-race/results/records-<campaign>.json --out fleet/web/app/boot-race/summary.json
"""

import argparse
import datetime
import json
import re
import sys

PAGE_URL = "https://ejc3.github.io/fcvm/boot-race.html"

# The setup row that describes each lane, by lane id. The Lambda cold lane's row has carried three names.
CONFIG_ROWS = {
    "fcvm-file": ("fcvm VM",),
    "fcvm-uffd": ("fcvm VM",),
    "ec2-launch": ("EC2 lanes",),
    "ec2-start": ("EC2 lanes",),
    "ec2-hibernate": ("EC2 lanes",),
    "ecs-fargate": ("ECS on Fargate",),
    "ecs-ec2": ("ECS on EC2",),
    "lambda-cold": ("Lambda, cold start", "Lambda, new runtime every request", "Lambda, cold"),
    "lambda-warm": ("Lambda, warm",),
}


def page_data(html):
    m = re.search(r'<script id="data" type="application/json">(.*?)</script>', html, re.S)
    if not m:
        raise SystemExit("the page holds no data element")
    data = json.loads(m.group(1).replace("<\\/", "</"))
    if not data or not data.get("lanes"):
        raise SystemExit("the page's data holds no lanes")
    return data


def check_text(template, lane):
    c = lane["checks"]
    return (template.replace("{n}", str(lane["n"])).replace("{new_proc}", str(c["new_proc"]))
            .replace("{new_boot}", str(c["new_boot"])).replace("{same_proc}", str(c["same_proc_as_previous"]))
            .replace("{same_boot}", str(c.get("same_boot_as_previous", 0)))
            .replace("{n_minus_1}", str(max(0, lane["n"] - 1))).replace("{init}", str(c["init_duration"]))
            .replace("{report}", str(c["report"])))


def iso(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def summarize(data, manifest):
    rows = dict(data["setup"]["rows"])
    checks = dict(data["setup"].get("checks", []))
    lanes = []
    for lane in data["lanes"]:
        label = next((r for r in CONFIG_ROWS.get(lane["id"], ()) if r in rows), None)
        if label is None:
            raise SystemExit(f"lane {lane['id']} has no configuration row in the page's setup")
        lanes.append({
            "id": lane["id"], "name": lane["name"], "start": lane["start"], "config": rows[label],
            "n": lane["n"], "failures": lane["failures"],
            **{k: lane[k] for k in ("median_ms", "p95_ms", "p99_ms", "min_ms", "max_ms")},
            "check": check_text(checks[lane["id"]], lane) if lane["id"] in checks else "",
        })
    fcvm = re.search(r"commit ([0-9a-f]{7,40})", rows.get("fcvm", ""))
    if not fcvm:
        raise SystemExit("the page's setup names no fcvm commit")
    campaign, commit = manifest["campaign"], manifest["records_commit"]
    return {
        "recorded": True,
        "campaign": campaign,
        "window_utc": {"first_sample": iso(data["first_sample_unix_ms"]), "last_sample": iso(data["last_sample_unix_ms"])},
        "rounds": data["rounds"],
        "fcvm_commit": fcvm.group(1),
        "region": data["setup"]["region"],
        "campaign_host": rows["Campaign host"],
        "host_load": rows.get("Campaign host load", ""),
        "records": {
            "repository": manifest["repository"],
            "commit": commit,
            "url": f"https://github.com/{manifest['repository']}/tree/{commit}/bench/boot-race/results/{campaign}",
        },
        "page_url": PAGE_URL,
        "lanes": lanes,
        "not_run": [{"id": l["id"], "name": l["name"], "why": l["why"]} for l in data.get("not_run", [])],
    }


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--page", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args(argv)
    with open(a.page) as f:
        data = page_data(f.read())
    with open(a.manifest) as f:
        manifest = json.load(f)
    summary = summarize(data, manifest)
    with open(a.out, "w") as f:
        json.dump(summary, f, indent=1)
        f.write("\n")
    print(f"{a.out}: {summary['campaign']}, {len(summary['lanes'])} lanes", file=sys.stderr)


if __name__ == "__main__":
    main()
