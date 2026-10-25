#!/usr/bin/env python3
"""Read fcvm's restored record out of every clone log of a finished reqbench run.

  python3 restore_ack.py <run dir> [<out>]

Runs recorded before reqbench.py stored `restore_ack` in each request record
keep the same information only in the per-request clone logs, which are not
committed. This reads each record's log (beside reqbench.jsonl, by its
basename) with reqbench.restore_ack, the function the harness now calls, and
writes <run dir>/restore-ack.jsonl: a meta line binding the output to
reqbench.jsonl by sha256, then one line per request record with the log's
sha256 and what restore_ack read. A record whose log is missing stops the
extraction and nothing is written, so a partial file cannot pass for a whole
one.
"""
import hashlib
import json
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqbench  # noqa: E402


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def extract(run_dir):
    """The lines of restore-ack.jsonl for run_dir, or raise ValueError."""
    source = os.path.join(run_dir, "reqbench.jsonl")
    with open(source) as handle:
        rows = [json.loads(line) for line in handle if line.strip()]
    if not rows or rows[0].get("kind") != "meta":
        raise ValueError(f"{source}: the first line is not a meta line")
    lines = [{"kind": "meta", "source": "reqbench.jsonl", "source_sha256": sha256_of(source),
              "run_id": rows[0].get("run_id")}]
    for row in rows[1:]:
        log = row.get("log")
        if not isinstance(log, str) or not log:
            raise ValueError(f"record {row.get('record_id')!r} names no log")
        path = os.path.join(run_dir, os.path.basename(log))
        if not os.path.isfile(path):
            raise ValueError(f"record {row.get('record_id')!r}: {path} is missing")
        lines.append({"record_id": row.get("record_id"), "arm": row.get("arm"),
                      "rep": row.get("rep"), "warmup": row.get("warmup"),
                      "log": os.path.basename(log), "log_sha256": sha256_of(path),
                      "restore_ack": reqbench.restore_ack(path)})
    return lines


def main(argv):
    if len(argv) not in (2, 3):
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    run_dir = argv[1]
    out = argv[2] if len(argv) == 3 else os.path.join(run_dir, "restore-ack.jsonl")
    try:
        lines = extract(run_dir)
    except (OSError, ValueError) as error:
        print(f"restore_ack: {error}; nothing written", file=sys.stderr)
        return 2
    # A temporary file of this invocation's own, so two extractions into one
    # run directory cannot write into each other's file before the rename.
    fd, tmp = tempfile.mkstemp(prefix=os.path.basename(out) + ".", suffix=".tmp",
                               dir=os.path.dirname(os.path.abspath(out)))
    try:
        with os.fdopen(fd, "w") as handle:
            for line in lines:
                handle.write(json.dumps(line, sort_keys=True) + "\n")
        os.replace(tmp, out)
    except BaseException:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise
    errors = sum(1 for line in lines[1:] if "error" in line["restore_ack"])
    print(f"restore_ack: {len(lines) - 1} records, {errors} without a restored record -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
