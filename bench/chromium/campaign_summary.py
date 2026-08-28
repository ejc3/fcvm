#!/usr/bin/env python3
"""Index the cells of one campaign into a single JSON file.

    campaign_summary.py --out PATH <run_dir>...

Each run directory holds reqanalyze's analysis.json (required; its stall_gate
must have been armed with --stall-max-ms and must have evaluated at least one
record, since an unarmed gate reports passed=true having evaluated nothing),
dns-evidence.json (required when the cell's guest_dns names a baked resolver,
optional otherwise; when present its verdict must be "clean" and every file it
cites must be present and agree with it) and diag/summary.json (optional). The
index names every file it was generated from with its sha256, and carries one
cell per run: engine, cpu, memory_mib, guest_dns, the seal identity,
publishable, stall_gate_passed, dns_verdict, load_max_1min (the maximum 1-min
load the campaign's sampler saw during the measured run, when the evidence
records it), the headline median blocking_ms per arm with its CI, and the
diag summary when there is one.

The publication rule (REVIEW.md) is to quote only from sealed runs that passed
their gates and were never withdrawn, and publishable=true alone proves none
of that. The seal is the cell's identity, SEAL_FIELDS below: the runtime
bundle reqbench.sh sealed, the fcvm binary and harness sources hashed into
it, the source revision, and the image and snapshot generation measured. A
cell missing any of them is not a sealed run. A run is withdrawn by a file
named WITHDRAWN in its directory whose first line is the reason, or by
"withdrawn": true in its analysis.json; either refuses the run and the
refusal quotes the reason.

The index is written only when every run is sealed, publishable and not
withdrawn, every stall gate passed and every DNS verdict is clean. Otherwise
nothing is written, an index already at --out is removed, and the exit status
is 5, the same code reqanalyze uses for a refused run: an index that quietly
carried an unpublishable cell would be quoted by someone who only opened the
index. Inputs are only ever read, and each is read once so the hash names the
bytes that were parsed.
"""

import argparse
import hashlib
import json
import math
import os
import sys
import tempfile

VERIFY_STAGES = ("pre", "before-run", "after-run")
# The seal identity of one run, as reqbench.py stamps it into every record's
# meta and reqanalyze carries it into the cell (CELL_FIELDS).
SEAL_FIELDS = (
    "runtime_bundle_sha256",
    "fcvm_sha256",
    "harness_sha256",
    "source_revision",
    "image_id",
    "snapshot_generation_id",
    "snapshot_config_sha256",
)
WITHDRAWN_MARKER = "WITHDRAWN"
REPLAY_LOGS = {
    "corpus_dns_log_sha256": "corpus-dns.log",
    "corpus_access_log_sha256": "corpus-access.log",
}


class RunError(Exception):
    """One run directory cannot be indexed; the message says why."""


def reject_duplicate_keys(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            raise ValueError(f"duplicate JSON key {key!r}")
        seen[key] = value
    return seen


def reject_constant(name):
    raise ValueError(f"non-standard JSON numeric constant {name}")


def read_bytes(path):
    with open(path, "rb") as handle:
        return handle.read()


def parse_json(data, path):
    try:
        return json.loads(
            data.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_constant,
        )
    except (UnicodeDecodeError, ValueError) as error:
        raise RunError(f"{path}: cannot parse: {error}")


class Sources:
    """The files one cell was read from, each read once and hashed from those bytes."""

    def __init__(self):
        self.entries = []

    def read_json(self, path):
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{path}: cannot read: {error}")
        self.entries.append({"path": path, "sha256": hashlib.sha256(data).hexdigest()})
        return parse_json(data, path)

    def read_hashed(self, path):
        """Record a file the index does not parse; returns its sha256."""
        try:
            data = read_bytes(path)
        except OSError as error:
            raise RunError(f"{path}: cannot read: {error}")
        digest = hashlib.sha256(data).hexdigest()
        self.entries.append({"path": path, "sha256": digest})
        return data, digest


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


def positive_int(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def withdrawal_reason(marker):
    """The first line of a WITHDRAWN marker; a marker that cannot be read
    still withdraws the run."""
    try:
        with open(marker, "rb") as handle:
            first_line = handle.readline()
    except OSError as error:
        return f"(marker present but unreadable: {error})"
    reason = first_line.decode("utf-8", errors="replace").strip()
    return reason or "(no reason recorded in the marker)"


def check_evidence(run_dir, evidence, sources):
    """Hold a clean verdict to the files it cites; raise RunError otherwise."""
    if not isinstance(evidence, dict):
        # Valid JSON that is not an object ([] for one) has no verdict to
        # read; refusing it here keeps every .get() below on a dict.
        raise RunError(f"{run_dir}: dns-evidence.json is not a JSON object")
    verdict = evidence.get("verdict")
    if verdict != "clean":
        raise RunError(f"{run_dir}: dns-evidence.json verdict is {verdict!r}, not 'clean'")
    if not positive_int(evidence.get("samples")):
        raise RunError(
            f"{run_dir}: dns-evidence.json records samples={evidence.get('samples')!r}; "
            "a clean verdict needs at least one :53 owner sample"
        )
    if evidence.get("first_mismatch") is not None:
        raise RunError(
            f"{run_dir}: dns-evidence.json is clean but records a mismatching sample: "
            f"{evidence.get('first_mismatch')!r}"
        )
    if evidence.get("sampler_alive_at_stop") is not True:
        raise RunError(
            f"{run_dir}: dns-evidence.json does not show the :53 owner sampler "
            "alive until the measured run ended"
        )
    if evidence.get("dnsmasq_active_after_restore") is not False:
        raise RunError(f"{run_dir}: dns-evidence.json records dnsmasq active after the restores")
    if evidence.get("dnsmasq_state_after_restore") != "inactive":
        raise RunError(
            f"{run_dir}: dns-evidence.json records dnsmasq state "
            f"{evidence.get('dnsmasq_state_after_restore')!r} after the restores, not 'inactive'"
        )
    owner_log = os.path.join(run_dir, "dns-owner.log")
    if not os.path.isfile(owner_log):
        raise RunError(f"{run_dir}: dns-owner.log cited by dns-evidence.json is missing")
    owner_bytes, _digest = sources.read_hashed(owner_log)
    owner_lines = owner_bytes.count(b"\n")
    if owner_lines != evidence["samples"]:
        raise RunError(
            f"{run_dir}: dns-evidence.json records {evidence['samples']} samples but "
            f"dns-owner.log holds {owner_lines} lines"
        )
    # The brackets: every stage the campaign runs, each present and passed.
    # Basenames are resolved inside run_dir so a relocated run directory
    # still indexes; the evidence's own absolute paths are not trusted.
    verify_files = evidence.get("verify_files")
    if not isinstance(verify_files, list) or not all(isinstance(p, str) for p in verify_files):
        raise RunError(f"{run_dir}: dns-evidence.json has no verify_files list")
    cited = {os.path.basename(p) for p in verify_files}
    for stage in VERIFY_STAGES:
        name = f"verify-dns-{stage}.json"
        if name not in cited:
            raise RunError(f"{run_dir}: dns-evidence.json cites no {name} ({stage} bracket)")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        verify = sources.read_json(path)
        if not isinstance(verify, dict) or verify.get("passed") is not True:
            raise RunError(f"{run_dir}: {name} does not record passed=true")
    # The replay server's own logs, pinned by hash at the verdict.
    for field, name in REPLAY_LOGS.items():
        recorded = evidence.get(field)
        if (
            not isinstance(recorded, str)
            or len(recorded) != 64
            or any(c not in "0123456789abcdef" for c in recorded)
        ):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name} ({field})")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        _data, digest = sources.read_hashed(path)
        if digest != recorded:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the "
                f"{recorded} dns-evidence.json recorded at the verdict"
            )


def load_cell(run_dir):
    """Read one run directory into an index cell. Returns (cell, source entries)."""
    sources = Sources()
    marker = os.path.join(run_dir, WITHDRAWN_MARKER)
    if os.path.lexists(marker):
        raise RunError(f"{run_dir}: withdrawn: {withdrawal_reason(marker)}")
    analysis_path = os.path.join(run_dir, "analysis.json")
    if not os.path.isfile(analysis_path):
        raise RunError(f"{run_dir}: analysis.json is missing")
    analysis = sources.read_json(analysis_path)
    if not isinstance(analysis, dict):
        raise RunError(f"{analysis_path}: not a JSON object")

    if analysis.get("withdrawn", False) is not False:
        raise RunError(
            f"{run_dir}: withdrawn: analysis.json records "
            f"withdrawn={analysis.get('withdrawn')!r}"
        )
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
    seal = {}
    for field in SEAL_FIELDS:
        value = cell.get(field)
        if not isinstance(value, str) or not value.strip():
            raise RunError(
                f"{run_dir}: analysis.json cell has no {field}; a run without its "
                f"seal identity ({', '.join(SEAL_FIELDS)}) is not a sealed run"
            )
        seal[field] = value
    stall_gate = analysis.get("stall_gate")
    if not isinstance(stall_gate, dict) or not isinstance(stall_gate.get("passed"), bool):
        raise RunError(
            f"{run_dir}: analysis.json has no stall_gate verdict; re-run reqanalyze"
        )
    max_ms = stall_gate.get("max_ms")
    if (
        isinstance(max_ms, bool)
        or not isinstance(max_ms, (int, float))
        or not math.isfinite(max_ms)
        or max_ms <= 0
    ):
        # reqanalyze without --stall-max-ms writes passed=true, evaluated=0:
        # a gate that evaluated nothing has no pass to report.
        raise RunError(
            f"{run_dir}: stall_gate was not armed (max_ms is {max_ms!r}); "
            "re-run reqanalyze --stall-max-ms N"
        )
    if not positive_int(stall_gate.get("evaluated")):
        raise RunError(
            f"{run_dir}: stall_gate evaluated {stall_gate.get('evaluated')!r} "
            "record(s); a pass over nothing is not a pass"
        )
    if stall_gate["passed"] is not True:
        raise RunError(
            f"{run_dir}: stall_gate failed: {stall_gate.get('violations')}"
        )

    dns_verdict = None
    load_max_1min = None
    evidence_path = os.path.join(run_dir, "dns-evidence.json")
    if os.path.isfile(evidence_path):
        evidence = sources.read_json(evidence_path)
        check_evidence(run_dir, evidence, sources)
        dns_verdict = evidence["verdict"]
        # Reported, not gated: the run driver refused a busy box at the
        # start, and evidence from before the sampler carried the load
        # column has no value to copy.
        load_max_1min = evidence.get("load_max_1min")
    elif cell["guest_dns"] is not None:
        # A guest that resolved through a baked resolver is a campaign run;
        # only the bracket evidence says the resolver held for the whole
        # measured run.
        raise RunError(
            f"{run_dir}: guest_dns is {cell['guest_dns']!r} but there is no "
            "dns-evidence.json; a resolver run without its brackets is not indexed"
        )

    diag = None
    diag_path = os.path.join(run_dir, "diag", "summary.json")
    if os.path.isfile(diag_path):
        diag = sources.read_json(diag_path)

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
        "seal": seal,
        "publishable": True,
        "stall_gate_passed": True,
        "dns_verdict": dns_verdict,
        "load_max_1min": load_max_1min,
        "headline": headline,
        "diag": diag,
    }, sources.entries


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
        generated_from.extend(sources)
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
    aliases_input = False
    for entry in index["generated_from"]:
        if os.path.realpath(entry["path"]) == out_realpath:
            errors.append(f"--out {args.out} aliases input {entry['path']}")
            aliases_input = True
    if errors:
        print("REFUSED: no index written", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        # An index already at --out describes cells this refusal did not
        # accept; it must not outlive the refusal. Never when --out is one
        # of the inputs, which are only ever read.
        if not aliases_input and os.path.lexists(args.out):
            os.unlink(args.out)
            print(f"  removed stale index {args.out}", file=sys.stderr)
        return 5
    write_json_atomic(args.out, index)
    print(f"wrote {args.out}: {len(index['cells'])} cell(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main_with())
