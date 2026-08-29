#!/usr/bin/env python3
"""Index the cells of one campaign into a single JSON file.

    campaign_summary.py --out PATH <run_dir>...

Each run directory holds reqanalyze's analysis.json (required; its stall_gate
must have been armed with --stall-max-ms and must have evaluated at least one
record, since an unarmed gate reports passed=true having evaluated nothing),
dns-evidence.json (required when the cell's guest_dns names a baked resolver,
optional otherwise; when present its verdict must be "clean", it must carry
the replay server's exit status 0, every file it cites must be present and
agree with the sha256 it recorded, each verify bracket must record the run's
resolver with the URL probes run under no proxy and every host and URL
answered through it, and every sample in dns-owner.log must name its
serve_pid as the owner of 127.0.0.1:53 with dnsmasq inactive and a load the
evidence accounts for) and diag/summary.json (optional). The index names
every file it was generated from with its sha256, and carries one
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
import re
import sys
import tempfile

VERIFY_STAGES = ("pre", "before-run", "after-run")
# One :53 owner sample, as corpus_campaign.sh's dns_owner_sample prints it.
# The load column is absent from logs written before the sampler carried it.
OWNER_SAMPLE = re.compile(
    r"^(?P<ts>\S+) owner_pid=(?P<owner>\S+) dnsmasq=(?P<dnsmasq>\S+)"
    r"(?: load1=(?P<load>\S+))?$"
)
# The sampler accepts a load only in this shape; dns_load_stats counts only
# these when it reports load_samples and load_max_1min.
LOAD_NUMBER = re.compile(r"^[0-9]+(\.[0-9]+)?$")
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


def is_sha256(value):
    return (
        isinstance(value, str)
        and len(value) == 64
        and all(c in "0123456789abcdef" for c in value)
    )


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


def check_owner_log(run_dir, evidence, owner_bytes):
    """Hold every sample in dns-owner.log to the rule the campaign applied at
    the verdict: the owner of 127.0.0.1:53 is serve_pid, dnsmasq is inactive,
    and the load column adds up to load_samples and load_max_1min. The
    evidence's own first_mismatch is a claim about these lines, not proof;
    a log rewritten under the same line count indexed clean on it."""
    serve_pid = evidence.get("serve_pid")
    if not positive_int(serve_pid):
        raise RunError(
            f"{run_dir}: dns-evidence.json is clean but records serve_pid="
            f"{serve_pid!r}; the samples have no server to be held to"
        )
    try:
        text = owner_bytes.decode("utf-8")
    except UnicodeDecodeError as error:
        raise RunError(f"{run_dir}: dns-owner.log is not UTF-8: {error}")
    loads = []
    for number, line in enumerate(text.splitlines(), 1):
        match = OWNER_SAMPLE.match(line)
        if match is None:
            raise RunError(f"{run_dir}: dns-owner.log line {number} is not a sample: {line!r}")
        if match["owner"] != str(serve_pid):
            raise RunError(
                f"{run_dir}: dns-owner.log line {number} names {match['owner']} as the "
                f"owner of 127.0.0.1:53, not corpus_serve {serve_pid}: {line!r}"
            )
        if match["dnsmasq"] != "inactive":
            raise RunError(
                f"{run_dir}: dns-owner.log line {number} records dnsmasq="
                f"{match['dnsmasq']}, not inactive: {line!r}"
            )
        if match["load"] is not None:
            if not LOAD_NUMBER.match(match["load"]):
                raise RunError(
                    f"{run_dir}: dns-owner.log line {number} carries a load1 that is "
                    f"not a number: {line!r}"
                )
            loads.append(float(match["load"]))
    load_samples = evidence.get("load_samples", 0)
    load_max = evidence.get("load_max_1min")
    if isinstance(load_samples, bool) or not isinstance(load_samples, int):
        raise RunError(f"{run_dir}: dns-evidence.json records load_samples={load_samples!r}")
    if isinstance(load_max, bool) or not (load_max is None or isinstance(load_max, (int, float))):
        raise RunError(f"{run_dir}: dns-evidence.json records load_max_1min={load_max!r}")
    found_max = max(loads) if loads else None
    if len(loads) != load_samples or found_max != load_max:
        raise RunError(
            f"{run_dir}: dns-owner.log carries {len(loads)} load sample(s) with maximum "
            f"{found_max} but dns-evidence.json records load_samples={load_samples}, "
            f"load_max_1min={load_max}"
        )


def check_verify_bracket(run_dir, name, verify, resolver):
    """Hold one HOP D bracket to what the campaign asserted when it ran it:
    the clone resolved through `resolver` (None takes the bracket's own, so
    the remaining brackets are held to the first), the URL probes ran with
    no proxy, and every corpus host and URL answered through that resolver.
    Returns the resolver the bracket names.

    passed=true is also what HOP D writes when it was given nothing to check,
    so it is not on its own a bracket."""
    if not isinstance(verify, dict):
        raise RunError(f"{run_dir}: {name} is not a JSON object")
    if verify.get("passed") is not True:
        raise RunError(f"{run_dir}: {name} does not record passed=true")
    dns_server = verify.get("dns_server")
    if not isinstance(dns_server, str) or not dns_server:
        raise RunError(
            f"{run_dir}: {name} records dns_server={dns_server!r}; the bracket "
            "names no resolver to have resolved through"
        )
    if resolver is not None and dns_server != resolver:
        raise RunError(
            f"{run_dir}: {name} records dns_server {dns_server!r}, not the "
            f"{resolver!r} this run was measured through"
        )
    if verify.get("proxies_disabled") is not True:
        raise RunError(
            f"{run_dir}: {name} records proxies_disabled="
            f"{verify.get('proxies_disabled')!r}; a URL probe that honoured the "
            "exec's proxy fetched the live site, not the replay server"
        )
    hosts = verify.get("hosts")
    if not isinstance(hosts, dict) or not hosts:
        raise RunError(
            f"{run_dir}: {name} records no resolved host; a bracket that "
            "checked nothing proves nothing"
        )
    for host, record in sorted(hosts.items()):
        if not isinstance(record, dict) or record.get("ok") is not True:
            raise RunError(f"{run_dir}: {name} records {host} as {record!r}, not resolved")
        if record.get("answer") != dns_server:
            raise RunError(
                f"{run_dir}: {name} records {host} answering "
                f"{record.get('answer')!r}, not the replay address {dns_server!r}"
            )
    urls = verify.get("urls")
    if not isinstance(urls, dict) or not urls:
        raise RunError(
            f"{run_dir}: {name} records no fetched URL; a bracket that "
            "checked nothing proves nothing"
        )
    for url, record in sorted(urls.items()):
        if not isinstance(record, dict) or record.get("ok") is not True:
            raise RunError(f"{run_dir}: {name} records {url} as {record!r}, not fetched")
        status = record.get("status")
        if isinstance(status, bool) or not isinstance(status, int) or not 200 <= status <= 399:
            raise RunError(
                f"{run_dir}: {name} records {url} returning {status!r}, not a 2xx or 3xx"
            )
        if not isinstance(record.get("proxy_env_ignored"), list):
            raise RunError(
                f"{run_dir}: {name} does not record which proxy variables the "
                f"{url} probe ignored, so nothing says the request left through none"
            )
    return dns_server


def check_evidence(run_dir, evidence, sources, guest_dns):
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
    check_owner_log(run_dir, evidence, owner_bytes)
    # The replay server's exit status, recorded by the campaign once the
    # server was gone: 0 is the shutdown sequence completing with both logs
    # closed; 1 is a log line it could not write after the response was
    # sent, so the logs it hashed are short. Only 0 is a complete log.
    serve_status = evidence.get("corpus_serve_exit_status")
    if isinstance(serve_status, bool) or not isinstance(serve_status, int) or serve_status != 0:
        raise RunError(
            f"{run_dir}: dns-evidence.json records corpus_serve exit status "
            f"{serve_status!r}, not 0; the replay logs it hashed may be short"
        )
    # The brackets: every stage the campaign runs, each present, holding the
    # bytes the verdict hashed, and asserting what the campaign asserted.
    # Basenames are resolved inside run_dir so a relocated run directory
    # still indexes; the evidence's own absolute paths are not trusted.
    # Evidence that pins no bracket is refused rather than read: it cannot
    # say whether the files beside it are the ones its verdict was formed on.
    verify_files = evidence.get("verify_files")
    if not isinstance(verify_files, list) or not all(isinstance(p, str) for p in verify_files):
        raise RunError(f"{run_dir}: dns-evidence.json has no verify_files list")
    recorded = evidence.get("verify_file_sha256")
    if not isinstance(recorded, dict):
        raise RunError(
            f"{run_dir}: dns-evidence.json has no verify_file_sha256 map; its "
            "brackets are unpinned, so an edited one cannot be told from the "
            "file the verdict read"
        )
    cited = {os.path.basename(p) for p in verify_files}
    resolver = guest_dns if isinstance(guest_dns, str) and guest_dns else None
    for stage in VERIFY_STAGES:
        name = f"verify-dns-{stage}.json"
        if name not in cited:
            raise RunError(f"{run_dir}: dns-evidence.json cites no {name} ({stage} bracket)")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        want = recorded.get(name)
        if not is_sha256(want):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name}")
        data, digest = sources.read_hashed(path)
        if digest != want:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the {want} "
                "dns-evidence.json recorded at the verdict"
            )
        resolver = check_verify_bracket(run_dir, name, parse_json(data, path), resolver)
    # The replay server's own logs, pinned by hash at the verdict.
    for field, name in REPLAY_LOGS.items():
        want = evidence.get(field)
        if not is_sha256(want):
            raise RunError(f"{run_dir}: dns-evidence.json has no sha256 for {name} ({field})")
        path = os.path.join(run_dir, name)
        if not os.path.isfile(path):
            raise RunError(f"{run_dir}: {name} cited by dns-evidence.json is missing")
        _data, digest = sources.read_hashed(path)
        if digest != want:
            raise RunError(
                f"{run_dir}: {name} sha256 {digest} does not match the "
                f"{want} dns-evidence.json recorded at the verdict"
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
        check_evidence(run_dir, evidence, sources, cell["guest_dns"])
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


def run_identity(run_dir):
    """The keys that name one run directory, so a second name for a run
    already listed can be recognised: its canonical path, and the directory
    identity the filesystem reports. A symlink alias shares the canonical
    path; a bind mount shares only (st_dev, st_ino). A path that cannot be
    stat'ed carries the first key alone and is refused by load_cell with its
    own message."""
    keys = [("path", os.path.realpath(run_dir))]
    try:
        stat = os.stat(run_dir)
    except OSError:
        return keys
    keys.append(("dir", (stat.st_dev, stat.st_ino)))
    return keys


def build_index(run_dirs):
    """Every cell or a list of refusals; never a partial index."""
    cells = []
    generated_from = []
    errors = []
    seen = {}
    for run_dir in run_dirs:
        keys = run_identity(run_dir)
        first = next((seen[key] for key in keys if key in seen), None)
        if first is not None:
            # Refused, not deduped: one run reached under two names is an
            # argument list the caller did not mean, and an index that
            # silently dropped the second would be one experiment counted
            # twice or a cell nobody notices is missing.
            if first == run_dir:
                errors.append(f"{run_dir}: listed more than once")
            else:
                errors.append(
                    f"{run_dir}: is the run directory {first} under a second name"
                )
            continue
        for key in keys:
            seen[key] = run_dir
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
