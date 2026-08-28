#!/usr/bin/env python3
"""Index the cells of one campaign into a single JSON file.

    campaign_summary.py --out PATH <run_dir>...

Each run directory holds reqanalyze's analysis.json (required),
dns-evidence.json (optional; when present its verdict must be "clean") and
diag/summary.json (optional). The index names every file it was generated
from with its sha256, and carries one cell per run: engine, cpu, memory_mib,
guest_dns, publishable, stall_gate_passed, dns_verdict, the headline median
blocking_ms per arm with its CI, and the diag summary when there is one.

The index is written only when every run is publishable, every stall gate
passed and every DNS verdict is clean. Otherwise nothing is written and the
exit status is 5, the same code reqanalyze uses for a refused run: an index
that quietly carried an unpublishable cell would be quoted by someone who
only opened the index. Inputs are only ever read.
"""

import argparse
import hashlib
import json
import os
import sys
import tempfile


class RunError(Exception):
    """One run directory cannot be indexed; the message says why."""


def reject_duplicate_keys(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            raise ValueError(f"duplicate JSON key {key!r}")
        seen[key] = value
    return seen


def read_json(path):
    try:
        with open(path) as handle:
            return json.load(handle, object_pairs_hook=reject_duplicate_keys)
    except (OSError, ValueError) as error:
        raise RunError(f"{path}: cannot read: {error}")


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json_atomic(path, value):
    directory = os.path.dirname(os.path.abspath(path))
    fd, temp_path = tempfile.mkstemp(prefix=".campaign-summary.", dir=directory)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle, indent=2, sort_keys=False)
            handle.write("\n")
        os.replace(temp_path, path)
    except BaseException:
        os.unlink(temp_path)
        raise


def load_cell(run_dir):
    """Read one run directory into an index cell. Returns (cell, source paths)."""
    analysis_path = os.path.join(run_dir, "analysis.json")
    if not os.path.isfile(analysis_path):
        raise RunError(f"{run_dir}: analysis.json is missing")
    analysis = read_json(analysis_path)
    if not isinstance(analysis, dict):
        raise RunError(f"{analysis_path}: not a JSON object")
    sources = [analysis_path]

    if analysis.get("publishable") is not True:
        reasons = (analysis.get("gate") or {}).get("reasons") or []
        raise RunError(f"{run_dir}: analysis.json is not publishable: {reasons}")
    cell = analysis.get("cell")
    if not isinstance(cell, dict):
        raise RunError(
            f"{run_dir}: analysis.json has no top-level cell; the index takes one "
            "run per cell, not a pooled multi-backend analysis"
        )
    for field in ("engine", "cpu", "memory_mib", "guest_dns"):
        if field not in cell:
            raise RunError(
                f"{run_dir}: analysis.json cell has no {field}; re-run reqanalyze"
            )
    stall_gate = analysis.get("stall_gate")
    if not isinstance(stall_gate, dict) or not isinstance(stall_gate.get("passed"), bool):
        raise RunError(
            f"{run_dir}: analysis.json has no stall_gate verdict; re-run reqanalyze"
        )
    if stall_gate["passed"] is not True:
        raise RunError(
            f"{run_dir}: stall_gate failed: {stall_gate.get('violations')}"
        )

    dns_verdict = None
    evidence_path = os.path.join(run_dir, "dns-evidence.json")
    if os.path.isfile(evidence_path):
        evidence = read_json(evidence_path)
        sources.append(evidence_path)
        dns_verdict = evidence.get("verdict") if isinstance(evidence, dict) else None
        if dns_verdict != "clean":
            raise RunError(
                f"{run_dir}: dns-evidence.json verdict is {dns_verdict!r}, not 'clean'"
            )

    diag = None
    diag_path = os.path.join(run_dir, "diag", "summary.json")
    if os.path.isfile(diag_path):
        diag = read_json(diag_path)
        sources.append(diag_path)

    arms = analysis.get("arms")
    if not isinstance(arms, dict) or not arms:
        raise RunError(f"{run_dir}: analysis.json has no arms")
    headline = {}
    for arm, data in arms.items():
        blocking = data.get("blocking_ms") if isinstance(data, dict) else None
        median = blocking.get("median") if isinstance(blocking, dict) else None
        if isinstance(median, bool) or not isinstance(median, (int, float)):
            raise RunError(f"{run_dir}: arm {arm} has no blocking_ms median")
        headline[arm] = {
            "blocking_ms": median,
            "blocking_ms_ci": [blocking.get("lo"), blocking.get("hi")],
            "n": blocking.get("n"),
        }

    return {
        "run_dir": run_dir,
        "run_id": analysis.get("run_id"),
        "engine": cell["engine"],
        "cpu": cell["cpu"],
        "memory_mib": cell["memory_mib"],
        "guest_dns": cell["guest_dns"],
        "backend": cell.get("backend"),
        "uffd_mode": cell.get("uffd_mode"),
        "publishable": True,
        "stall_gate_passed": True,
        "dns_verdict": dns_verdict,
        "headline": headline,
        "diag": diag,
    }, sources


def build_index(run_dirs):
    """Every cell or a list of refusals; never a partial index."""
    cells = []
    generated_from = []
    errors = []
    seen = set()
    for run_dir in run_dirs:
        if run_dir in seen:
            errors.append(f"{run_dir}: listed more than once")
            continue
        seen.add(run_dir)
        try:
            cell, sources = load_cell(run_dir)
        except RunError as error:
            errors.append(str(error))
            continue
        cells.append(cell)
        generated_from.extend(
            {"path": path, "sha256": sha256_file(path)} for path in sources
        )
    return {"generated_from": generated_from, "cells": cells}, errors


def main_with(argv=None):
    parser = argparse.ArgumentParser(
        description="Index the publishable cells of one campaign into one JSON file.",
    )
    parser.add_argument("--out", required=True, help="index path to write")
    parser.add_argument("run_dir", nargs="+", help="run directories holding analysis.json")
    args = parser.parse_args(argv)

    run_dirs = [os.path.normpath(run_dir) for run_dir in args.run_dir]
    index, errors = build_index(run_dirs)
    out_realpath = os.path.realpath(args.out)
    for entry in index["generated_from"]:
        if os.path.realpath(entry["path"]) == out_realpath:
            errors.append(f"--out {args.out} aliases input {entry['path']}")
    if errors:
        print("REFUSED: no index written", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 5
    write_json_atomic(args.out, index)
    print(f"wrote {args.out}: {len(index['cells'])} cell(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main_with())
