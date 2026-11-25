#!/usr/bin/env python3
"""One summary of a pages-mode campaign: the regimes of one boot of one host.

    renders_campaign.py DIR --name NAME --regime LABEL=RUN[,skip=S][,last=S] ... [--out FILE]

DIR holds one directory per run of `make fleet-renders-bench` (renders.jsonl, samples.jsonl,
summary.json, teardown.json) with a `host-record.txt` beside them, written when the run
ended: lines `kernel:`, `source:` (the fcvm commit), `kitesurf:` (its commit), `host:` (the
instance type, then `boot:` and a hash of the boot id), `arguments:`, `ended:`.

A regime is a run scored over a window (renders_analyze.py: from `skip` seconds after the
first sample, or the `last` seconds). The summary carries, per regime, each side's figures,
the harness's, the levels the search moved through, and the sha256 and size of the run's
files, so a figure can be traced to its record.

Nothing is written unless every run agrees on the kernel, the fcvm commit, Kitesurf's commit,
the instance type and the boot, the kernel is fcvm's own host kernel, each run's summary
names that kernel, and each run's teardown left nothing.
"""

import argparse
import hashlib
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_analyze  # noqa: E402

FCVM_KERNEL = re.compile(r"^\d+\.\d+\.\d+-fcvm-[0-9a-f]{12}$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
RECORD_FILES = ("renders.jsonl", "samples.jsonl", "summary.json", "teardown.json", "host-record.txt")
SAME = ("kernel", "source", "kitesurf", "instance_type", "boot")


class Refused(Exception):
    """The records cannot stand as one campaign."""


def host_record(path):
    fields = {}
    with open(path) as handle:
        for line in handle:
            key, sep, value = line.partition(":")
            if sep:
                fields[key.strip()] = value.strip()
    host = fields.get("host", "")
    found = re.fullmatch(r"(\S+)\s+boot:\s+([0-9a-f]{16})", host)
    if not found:
        raise Refused(f"{path}: no `host: TYPE  boot: HASH` line")
    record = {"kernel": fields.get("kernel", ""), "source": fields.get("source", ""),
              "kitesurf": fields.get("kitesurf", ""), "instance_type": found.group(1),
              "boot": found.group(2), "arguments": fields.get("arguments", ""),
              "ended": fields.get("ended", "")}
    if not FCVM_KERNEL.match(record["kernel"]):
        raise Refused(f"{path}: kernel {record['kernel']!r} is not fcvm's host kernel")
    for key in ("source", "kitesurf"):
        if not COMMIT.match(record[key]):
            raise Refused(f"{path}: {key} {record[key]!r} is not a commit")
    return record


def level_changes(samples, side):
    """[[seconds from the first sample, level], ...]: each change of the side's level."""
    out, last = [], None
    start = samples[0]["at"]
    for sample in samples:
        level = sample[side]["level"]
        if level != last:
            out.append([round((sample["at"] - start) / 1000), level])
            last = level
    return out


def file_record(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return {"sha256": digest.hexdigest(), "size": os.path.getsize(path)}


def regime(directory, label, run, skip_s, last_s):
    out = os.path.join(directory, run)
    host = host_record(os.path.join(out, "host-record.txt"))
    with open(os.path.join(out, "summary.json")) as handle:
        summary = json.load(handle)
    with open(os.path.join(out, "teardown.json")) as handle:
        teardown = json.load(handle)
    module = summary.get("module") or {}
    if module.get("kernel") != host["kernel"]:
        raise Refused(f"{run}: summary.json names kernel {module.get('kernel')!r}, the host record {host['kernel']!r}")
    if teardown.get("cgroups_left") or teardown.get("resources_left") or teardown.get("run_dir_left"):
        raise Refused(f"{run}: the teardown left something: {json.dumps(teardown, sort_keys=True)[:200]}")
    figures = renders_analyze.analyze(out, skip_s, last_s)
    samples = renders_analyze.rows(os.path.join(out, "samples.jsonl"))
    sides = {}
    for side, values in figures["sides"].items():
        search = ((summary.get("sides") or {}).get(side) or {}).get("search") or {}
        sides[side] = dict(values)
        sides[side]["setup"] = ((summary.get("sides") or {}).get(side) or {}).get("setup") or {}
        sides[side]["limit"] = search.get("limit")
        sides[side]["level_changes"] = level_changes(samples, side)
    return host, {
        "label": label, "run": run, "arguments": host["arguments"], "ended": host["ended"],
        "window": {"seconds": figures["window_s"], "skip_s": skip_s, "last_s": last_s},
        "fixed_levels": module.get("fixed_levels"),
        "engine_recycles": figures["recycles"], "engine_restarts": module.get("engine_restarts"),
        "sides": sides, "harness": figures["harness"],
        "files": {name: file_record(os.path.join(out, name)) for name in RECORD_FILES},
    }


def summarize(directory, name, regimes):
    """`regimes`: (label, run, skip_s, last_s). Returns the summary; raises Refused."""
    if not regimes:
        raise Refused("no regime named")
    shared, rows = None, []
    for label, run, skip_s, last_s in regimes:
        host, row = regime(directory, label, run, skip_s, last_s)
        mine = {key: host[key] for key in SAME}
        if shared is None:
            shared = mine
        elif mine != shared:
            differs = sorted(key for key in SAME if mine[key] != shared[key])
            raise Refused(f"{run}: not the campaign's {', '.join(differs)}: "
                          f"{[mine[k] for k in differs]} against {[shared[k] for k in differs]}")
        rows.append(row)
    labels = [row["label"] for row in rows]
    if len(set(labels)) != len(labels):
        raise Refused(f"a label is used twice: {labels}")
    return {"schema": 1, "campaign": name, "generated_by": "bench/chromium/renders_campaign.py",
            "kernel": shared["kernel"], "source_revision": shared["source"],
            "kitesurf_commit": shared["kitesurf"], "instance_type": shared["instance_type"],
            "boot": shared["boot"], "regimes": rows}


def parse_regime(text):
    label, sep, rest = text.partition("=")
    parts = rest.split(",")
    if not sep or not label or not parts[0]:
        raise argparse.ArgumentTypeError(f"{text!r}: want LABEL=RUN[,skip=S][,last=S]")
    skip_s, last_s = 30.0, 0.0
    for option in parts[1:]:
        key, _, value = option.partition("=")
        try:
            if key == "skip":
                skip_s = float(value)
            elif key == "last":
                last_s = float(value)
            else:
                raise ValueError(key)
        except ValueError:
            raise argparse.ArgumentTypeError(f"{text!r}: want LABEL=RUN[,skip=S][,last=S]") from None
    return label, parts[0], skip_s, last_s


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("directory")
    p.add_argument("--name", required=True)
    p.add_argument("--regime", action="append", type=parse_regime, default=[], metavar="LABEL=RUN[,skip=S][,last=S]")
    p.add_argument("--out", default="")
    opts = p.parse_args(argv)
    try:
        summary = summarize(opts.directory, opts.name, opts.regime)
    except Refused as refused:
        print(f"REFUSED: {refused}", file=sys.stderr)
        return 2
    text = json.dumps(summary, indent=1, sort_keys=True) + "\n"
    if opts.out:
        with open(opts.out, "w") as handle:
            handle.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
