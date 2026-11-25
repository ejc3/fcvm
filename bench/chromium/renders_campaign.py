#!/usr/bin/env python3
"""One summary of a pages-mode campaign: the regimes of one boot of one host.

    renders_campaign.py DIR --name NAME --regime LABEL=RUN[,skip=S][,last=S] ... [--out FILE]

DIR holds one directory per run of `make fleet-renders-bench` (renders.jsonl, samples.jsonl,
summary.json, teardown.json) with a `host-record.txt` beside them, written when the run
ended (renders_host_record.py): lines `kernel:`, `cmdline:` (the kernel parameters that
writer keeps, and no other), `source:` (the fcvm commit), `kitesurf:` (its commit), `host:`
(the instance type, then `boot:` and a hash of the boot id), `arguments:`, `ended:`, each
once.

A regime is a run scored over a window (renders_analyze.py: from `skip` seconds after the
first sample, or the `last` seconds). The summary carries, per regime, each side's figures,
the harness's, the levels the search moved through, and the sha256 and size of the run's
files, so a figure can be traced to its record.

Nothing is written unless every run agrees on the kernel, the fcvm commit, Kitesurf's commit,
the instance type and the boot, the kernel is fcvm's own host kernel, each run's summary
names that kernel, and each run's teardown says it left nothing. A host record that holds
anything its writer would not write, a run that is not a directory of DIR, or a window
without samples is refused the same way.
"""

import argparse
import hashlib
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_analyze  # noqa: E402
import renders_host_record  # noqa: E402

FCVM_KERNEL = re.compile(r"^\d+\.\d+\.\d+-fcvm-[0-9a-f]{12}$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
ENDED = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
HOST_FIELDS = ("kernel", "cmdline", "source", "kitesurf", "host", "arguments", "ended")
RECORD_FILES = ("renders.jsonl", "samples.jsonl", "summary.json", "teardown.json", "host-record.txt")
SAME = ("kernel", "source", "kitesurf", "instance_type", "boot")


class Refused(Exception):
    """The records cannot stand as one campaign."""


def host_record(path):
    fields = {}
    try:
        with open(path) as handle:
            lines = handle.read().splitlines()
    except OSError as error:
        raise Refused(f"{path}: {error.strerror}") from None
    for line in lines:
        key, sep, value = line.partition(":")
        if not sep or key not in HOST_FIELDS:
            raise Refused(f"{path}: {line[:40]!r} is a line that is not one of the record's")
        if key in fields:
            raise Refused(f"{path}: says `{key}` twice")
        fields[key] = value.strip()
    for key in HOST_FIELDS:
        if key not in fields:
            raise Refused(f"{path}: no `{key}` line" if key != "host" else f"{path}: no `host: TYPE  boot: HASH` line")
    found = re.fullmatch(r"(\S+)\s+boot:\s+([0-9a-f]{16})", fields["host"])
    if not found:
        raise Refused(f"{path}: no `host: TYPE  boot: HASH` line")
    # Only what the record's writer writes: listed kernel parameters, plain arguments, an instance type.
    if fields["cmdline"] != renders_host_record.kept_parameters(fields["cmdline"]):
        raise Refused(f"{path}: the kernel command line holds more than the recorded parameters")
    try:
        if fields["arguments"] != " ".join(renders_host_record.argument(t) for t in fields["arguments"].split()):
            raise ValueError("a path")
    except ValueError:
        raise Refused(f"{path}: `arguments` holds more than the launcher's variables with values of their shape") from None
    if not renders_host_record.INSTANCE_TYPE.fullmatch(found.group(1)):
        raise Refused(f"{path}: {found.group(1)!r} is not an instance type")
    if not ENDED.fullmatch(fields["ended"]):
        raise Refused(f"{path}: `ended` is {fields['ended']!r}, not a UTC time to the second")
    record = {"kernel": fields["kernel"], "source": fields["source"],
              "kitesurf": fields["kitesurf"], "instance_type": found.group(1),
              "boot": found.group(2), "arguments": fields["arguments"],
              "ended": fields["ended"]}
    if not FCVM_KERNEL.fullmatch(record["kernel"]):
        raise Refused(f"{path}: kernel {record['kernel']!r} is not fcvm's host kernel")
    for key in ("source", "kitesurf"):
        if not COMMIT.fullmatch(record[key]):
            raise Refused(f"{path}: {key} {record[key]!r} is not a commit")
    return record


def read_json(path):
    try:
        with open(path) as handle:
            return json.load(handle)
    except OSError as error:
        raise Refused(f"{path}: {error.strerror}") from None
    except ValueError as error:
        raise Refused(f"{path}: not JSON ({error})") from None


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
    if run in ("", ".", "..") or os.path.basename(run) != run:
        raise Refused(f"{run!r}: a run names a directory of the campaign, not a path")
    if skip_s < 0 or last_s < 0:
        raise Refused(f"{run}: a window's seconds are not negative")
    out = os.path.join(directory, run)
    if not os.path.isdir(out):
        raise Refused(f"{run}: absent from {directory}")
    host = host_record(os.path.join(out, "host-record.txt"))
    summary = read_json(os.path.join(out, "summary.json"))
    teardown = read_json(os.path.join(out, "teardown.json"))
    if not isinstance(summary, dict):
        raise Refused(f"{run}: summary.json is not an object")
    module = summary.get("module") or {}
    if module.get("kernel") != host["kernel"]:
        raise Refused(f"{run}: summary.json names kernel {module.get('kernel')!r}, the host record {host['kernel']!r}")
    said = json.dumps(teardown, sort_keys=True)[:200]
    if not isinstance(teardown, dict) or type(teardown.get("cgroups_left")) is not list \
            or type(teardown.get("resources_left")) is not int or type(teardown.get("run_dir_left")) is not bool:
        raise Refused(f"{run}: teardown.json does not say nothing was left: {said}")
    if teardown["cgroups_left"] or teardown["resources_left"] or teardown["run_dir_left"]:
        raise Refused(f"{run}: the teardown left something: {said}")
    try:
        figures = renders_analyze.analyze(out, skip_s, last_s)
        samples = renders_analyze.rows(os.path.join(out, "samples.jsonl"))
    except SystemExit as stopped:  # renders_analyze's own refusals: an empty window
        raise Refused(str(stopped)) from None
    except OSError as error:
        raise Refused(f"{run}: {error.filename}: {error.strerror}") from None
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
