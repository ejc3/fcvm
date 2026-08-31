#!/usr/bin/env python3
"""Set the VM arm and the host-container arms side by side, per URL and overall.

Reads only records: the campaign's reqbench.jsonl + analysis.json (the VM arm,
already through its publication gate) and hostcdp.jsonl + run.json +
complete.json (the host arms). A corpus-extra host also requires the parent
campaign-complete.json. The VM bytes and published cell must match the raw
metadata, every host row must name the exact run.json bytes beside it, and the
completion records must commit every consumed artifact. Prints two tables and
the ratios, and writes comparison.json.

Two quantities are compared, never mixed:
  caller-visible   VM blocking_ms (spawn -> image in hand) against host wall_ms
                   (the driver invocation). The host side carries a python
                   interpreter start per rep that the VM side does not, because
                   reqbench imports cdpdrive and calls drive() in-process.
  driver total     cdpdrive's own total_ms on both sides: the same code, timed
                   the same way, with no interpreter start and no clone
                   lifecycle in either.
"""
import argparse
from collections import Counter
import ctypes
import fcntl
import hashlib
import ipaddress
import json
import math
import os
import random
import secrets
import stat
import statistics
import sys
import tempfile
import time
from urllib.parse import urlsplit

import campaign_summary


class Refusal(ValueError):
    """The inputs cannot support the comparison they were asked to make."""


def artifact_identity(artifact):
    return {key: value for key, value in artifact.items() if key != "text"}


def read_artifact(path):
    """Read one stable byte view and name the exact bytes returned."""
    try:
        with open(path, "rb") as handle:
            before = os.fstat(handle.fileno())
            raw = handle.read()
            after = os.fstat(handle.fileno())
    except OSError as error:
        raise Refusal(f"cannot read {path}: {error}") from error
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    if any(getattr(before, field) != getattr(after, field) for field in fields):
        raise Refusal(f"{path} changed while it was being read")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise Refusal(f"{path} is not UTF-8: {error}") from error
    return {
        "path": path,
        "realpath": os.path.realpath(path),
        "device": before.st_dev,
        "inode": before.st_ino,
        "size": len(raw),
        "mtime_ns": before.st_mtime_ns,
        "ctime_ns": before.st_ctime_ns,
        "sha256": hashlib.sha256(raw).hexdigest(),
        "text": text,
    }


def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def parse_json(text, label):
    try:
        value = json.loads(text, object_pairs_hook=reject_duplicate_keys)
    except ValueError as error:
        raise Refusal(f"{label} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise Refusal(f"{label} is not a JSON object")
    return value


def parse_jsonl(text, label):
    rows = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            raise Refusal(f"{label}:{line_number} is blank")
        rows.append(parse_json(line, f"{label}:{line_number}"))
    if not rows:
        raise Refusal(f"{label} has no records")
    return rows


def load_vm_publication(run_dir):
    """Validate one VM run and bind the exact bytes used by that verdict."""
    try:
        _cell, sources = campaign_summary.load_cell(run_dir)
    except campaign_summary.RunError as error:
        raise Refusal(f"VM publication contract: {error}") from error
    reject_vm_withdrawn(run_dir)

    artifacts = []
    for source in sources:
        artifact = read_artifact(source["path"])
        if artifact["sha256"] != source["sha256"]:
            raise Refusal(
                f"VM publication input {source['path']} changed while its "
                "publication contract was being validated"
            )
        artifacts.append(artifact)

    analysis_path = os.path.abspath(os.path.join(run_dir, "analysis.json"))
    analysis_artifacts = [
        artifact for artifact in artifacts
        if os.path.abspath(artifact["path"]) == analysis_path
    ]
    if len(analysis_artifacts) != 1:
        raise Refusal(
            "VM publication contract did not consume exactly one analysis.json"
        )
    analysis_artifact = analysis_artifacts[0]
    analysis = parse_json(analysis_artifact["text"], analysis_artifact["path"])
    return analysis, analysis_artifact, [
        artifact_identity(artifact) for artifact in artifacts
    ]


def open_output_target(path):
    absolute = os.path.abspath(path)
    directory, name = os.path.split(absolute)
    if not name:
        raise Refusal(f"output path {path!r} names no file")
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0)
    directory_fd = None
    try:
        directory_fd = os.open(directory, flags)
        directory_stat = os.fstat(directory_fd)
    except OSError as error:
        if directory_fd is not None:
            os.close(directory_fd)
        raise Refusal(f"cannot open output directory {directory}: {error}") from error
    return {
        "path": path,
        "absolute": absolute,
        "directory": directory,
        "name": name,
        "directory_fd": directory_fd,
        "directory_stat": directory_stat,
    }


def ensure_output_directory(target):
    try:
        current = os.stat(target["directory"])
    except OSError as error:
        raise Refusal(
            f"output directory {target['directory']} cannot be rechecked: {error}"
        ) from error
    if not os.path.samestat(current, target["directory_stat"]):
        raise Refusal(f"output directory {target['directory']} changed during comparison")


def validate_output_lock(target, lock_fd):
    """Prove the held fd is the one permanent regular lock directory entry."""
    ensure_output_directory(target)
    lock_name = f"{target['name']}.lock"
    try:
        opened = os.fstat(lock_fd)
        entry = os.stat(
            lock_name,
            dir_fd=target["directory_fd"],
            follow_symlinks=False,
        )
    except OSError as error:
        raise Refusal(
            f"cannot validate output lock {target['path']}.lock: {error}"
        ) from error
    if not stat.S_ISREG(opened.st_mode) or opened.st_nlink != 1:
        raise Refusal(
            f"output lock {target['path']}.lock is not a single-link regular file"
        )
    if (
        not stat.S_ISREG(entry.st_mode)
        or entry.st_nlink != 1
        or not os.path.samestat(opened, entry)
    ):
        raise Refusal(
            f"output lock {target['path']}.lock changed directory identity"
        )
    ensure_output_directory(target)


def open_output_lock(target):
    """Open, without following links, the permanent lock beside the output."""
    ensure_output_directory(target)
    lock_name = f"{target['name']}.lock"
    flags = (
        os.O_RDWR
        | os.O_CREAT
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    try:
        lock_fd = os.open(
            lock_name,
            flags,
            0o666,
            dir_fd=target["directory_fd"],
        )
    except OSError as error:
        raise Refusal(
            f"cannot open output lock {target['path']}.lock: {error}"
        ) from error
    try:
        validate_output_lock(target, lock_fd)
    except BaseException:
        os.close(lock_fd)
        raise
    return lock_fd


def acquire_output_lock(target, lock_fd, wait_seconds=5.0):
    """Acquire one permanent lock within a bounded monotonic deadline."""
    deadline = time.monotonic() + wait_seconds
    while True:
        validate_output_lock(target, lock_fd)
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError as error:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise Refusal(
                    f"output lock {target['path']}.lock remained held for "
                    f"{wait_seconds:g}s"
                ) from error
            time.sleep(min(0.01, remaining))
        except OSError as error:
            raise Refusal(
                f"cannot acquire output lock {target['path']}.lock: {error}"
            ) from error
    validate_output_lock(target, lock_fd)


def rename_noreplace(directory_fd, source, destination):
    """Rename one directory entry without overwriting a concurrent creator."""
    try:
        renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as error:
        raise Refusal("renameat2 is required for race-free output handling") from error
    renameat2.argtypes = (
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    )
    renameat2.restype = ctypes.c_int
    result = renameat2(
        directory_fd,
        os.fsencode(source),
        directory_fd,
        os.fsencode(destination),
        1,
    )
    if result != 0:
        error_number = ctypes.get_errno()
        raise OSError(error_number, os.strerror(error_number), source)


def write_json_atomic(path, value, output_target=None, before_publish=None):
    if output_target is not None:
        return write_json_atomic_at(output_target, value, before_publish)
    directory = os.path.dirname(os.path.abspath(path)) or "."
    fd, temporary = tempfile.mkstemp(prefix=".compare-", dir=directory)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle, indent=1, allow_nan=False)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        if before_publish is not None:
            before_publish()
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def write_json_atomic_at(target, value, before_publish=None):
    ensure_output_directory(target)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0)
    while True:
        temporary = f".compare-{os.getpid()}-{secrets.token_hex(8)}"
        try:
            fd = os.open(
                temporary, flags, 0o600, dir_fd=target["directory_fd"]
            )
            break
        except FileExistsError:
            continue
        except OSError as error:
            raise Refusal(
                f"cannot create temporary output in {target['directory']}: {error}"
            ) from error
    linked = False
    temporary_exists = True
    try:
        try:
            handle = os.fdopen(fd, "w")
        except BaseException:
            os.close(fd)
            raise
        with handle:
            json.dump(value, handle, indent=1, allow_nan=False)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
            written_stat = os.fstat(handle.fileno())
        ensure_output_directory(target)
        if before_publish is not None:
            before_publish()
        try:
            os.link(
                temporary,
                target["name"],
                src_dir_fd=target["directory_fd"],
                dst_dir_fd=target["directory_fd"],
                follow_symlinks=False,
            )
        except FileExistsError as error:
            raise Refusal(
                f"output {target['path']} appeared during comparison; refusing "
                "to replace it"
            ) from error
        linked = True
        os.unlink(temporary, dir_fd=target["directory_fd"])
        temporary_exists = False
        current = os.stat(
            target["name"],
            dir_fd=target["directory_fd"],
            follow_symlinks=False,
        )
        if not os.path.samestat(current, written_stat):
            raise Refusal(f"output {target['path']} changed during publication")
        ensure_output_directory(target)
    except BaseException:
        if linked:
            try:
                current = os.stat(
                    target["name"],
                    dir_fd=target["directory_fd"],
                    follow_symlinks=False,
                )
                if os.path.samestat(current, written_stat):
                    os.unlink(target["name"], dir_fd=target["directory_fd"])
            except FileNotFoundError:
                pass
        if temporary_exists:
            try:
                os.unlink(temporary, dir_fd=target["directory_fd"])
            except FileNotFoundError:
                pass
        raise


def integer_field(record, key, label, minimum=0):
    value = record.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise Refusal(f"{label} has invalid {key}={value!r}")
    return value


def finite_nonnegative(value):
    return (
        not isinstance(value, bool)
        and isinstance(value, (int, float))
        and math.isfinite(value)
        and value >= 0
    )


def finite_positive(value):
    return finite_nonnegative(value) and value > 0


def required_string(record, key, label):
    value = record.get(key)
    if not isinstance(value, str) or not value:
        raise Refusal(f"{label} has invalid {key}={value!r}")
    return value


def sha256_field(record, key, label):
    value = required_string(record, key, label)
    if len(value) != 64 or any(character not in "0123456789abcdef" for character in value):
        raise Refusal(f"{label} has invalid {key}={value!r}")
    return value


def declared_urls(record, label):
    urls = record.get("urls")
    if urls is None:
        value = record.get("url")
        if not isinstance(value, str):
            raise Refusal(f"{label} names no URL corpus")
        urls = [part.strip() for part in value.split(",") if part.strip()]
    if (
        not isinstance(urls, list)
        or not urls
        or any(not isinstance(url, str) or not url for url in urls)
    ):
        raise Refusal(f"{label} has no valid ordered URL corpus")
    value = record.get("url")
    if value is not None:
        if not isinstance(value, str):
            raise Refusal(f"{label} has invalid url={value!r}")
        split_urls = [part.strip() for part in value.split(",") if part.strip()]
        if split_urls != urls:
            raise Refusal(
                f"{label} has contradictory corpus declarations: "
                f"url={split_urls!r} urls={urls!r}"
            )
    return urls


def bind_analysis_input(analysis, current):
    identity = analysis.get("analysis_identity")
    inputs = identity.get("inputs") if isinstance(identity, dict) else None
    if not isinstance(inputs, list) or len(inputs) != 1:
        raise Refusal(
            "analysis.json analysis_identity.inputs must identify exactly the "
            "reqbench.jsonl this comparison consumes"
        )
    recorded = inputs[0]
    if not isinstance(recorded, dict):
        raise Refusal("analysis.json analysis_identity.inputs[0] is not an object")
    digest = recorded.get("sha256")
    size = recorded.get("size")
    if (
        not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdefABCDEF" for character in digest)
        or isinstance(size, bool)
        or not isinstance(size, int)
        or size < 0
    ):
        raise Refusal(
            "analysis.json analysis_identity.inputs[0] has no valid size and sha256"
        )
    if digest.lower() != current["sha256"] or size != current["size"]:
        raise Refusal(
            "current reqbench.jsonl does not match analysis_identity.inputs "
            f"(current size={current['size']} sha256={current['sha256']}, "
            f"recorded size={size} sha256={digest})"
        )


def pct(v, p):
    """p50 is statistics.median, the convention reqanalyze uses for every
    published median (median_ci), so a ratio taken here is between two numbers
    computed the same way. Other percentiles are nearest-rank."""
    v = sorted(v)
    if not v:
        return None
    if p == 50:
        return statistics.median(v)
    return v[max(0, -(-p * len(v) // 100) - 1)]


def vm_schedule(meta, label):
    measured = integer_field(meta, "reps", label, minimum=1)
    warmup = integer_field(meta, "warmup", label)
    seed = meta.get("seed")
    if isinstance(seed, bool) or not isinstance(seed, int):
        raise Refusal(f"{label} has invalid seed={seed!r}")
    arms = meta.get("arms")
    if (
        not isinstance(arms, list)
        or not arms
        or any(not isinstance(arm, str) or not arm for arm in arms)
        or len(set(arms)) != len(arms)
    ):
        raise Refusal(f"{label} has no valid unique arm schedule")
    urls = declared_urls(meta, label)
    rng = random.Random(seed)
    schedule = []
    for rep in range(warmup + measured):
        order = list(arms)
        rng.shuffle(order)
        schedule.extend((rep, arm, rep < warmup) for arm in order)
    return schedule, urls


def validate_vm_metric(record, key, label):
    value = record.get(key)
    if not finite_nonnegative(value):
        raise Refusal(f"{label} has invalid {key}={value!r}")
    return value


def load_vm(run_dir, analysis):
    artifact = read_artifact(os.path.join(run_dir, "reqbench.jsonl"))
    bind_analysis_input(analysis, artifact)
    rows = parse_jsonl(artifact["text"], artifact["path"])
    if rows[0].get("kind") != "meta":
        raise Refusal(f"{artifact['path']} first record is not VM metadata")
    metas = [row for row in rows if row.get("kind") == "meta"]
    if len(metas) != 1:
        raise Refusal(
            f"{artifact['path']} has {len(metas)} meta records; exactly one is required"
        )
    meta = metas[0]
    run_id = required_string(meta, "run_id", "VM reqbench metadata")
    if analysis.get("run_id") != run_id:
        raise Refusal(
            f"analysis.json run_id={analysis.get('run_id')!r} does not match "
            f"VM reqbench metadata run_id={run_id!r}"
        )
    schedule, urls = vm_schedule(meta, "VM reqbench metadata")
    records = rows[1:]
    if len(records) != len(schedule):
        raise Refusal(
            f"VM reqbench schedule declares {len(schedule)} arm/rep records but "
            f"{artifact['path']} contains {len(records)}"
        )

    measured = []
    for ordinal, (record, expected) in enumerate(zip(records, schedule), 1):
        rep, arm, is_warmup = expected
        label = f"{artifact['path']}:{ordinal + 1}"
        if record.get("run_id") != run_id:
            raise Refusal(
                f"{label} run_id={record.get('run_id')!r} does not match "
                f"VM reqbench metadata run_id={run_id!r}"
            )
        actual_rep = integer_field(record, "rep", label)
        if record.get("arm") != arm or actual_rep != rep:
            raise Refusal(
                f"{label} does not match the declared VM schedule: "
                f"got arm={record.get('arm')!r} rep={actual_rep!r}, "
                f"expected arm={arm!r} rep={rep}"
            )
        if record.get("warmup") is not is_warmup:
            raise Refusal(
                f"{label} has warmup={record.get('warmup')!r}; expected {is_warmup}"
            )
        expected_record_id = f"{run_id}:{arm}:{rep}:{int(is_warmup)}"
        if record.get("record_id") != expected_record_id:
            raise Refusal(
                f"{label} has record_id={record.get('record_id')!r}; "
                f"expected {expected_record_id!r}"
            )
        expected_url = urls[rep % len(urls)]
        if record.get("url") != expected_url:
            raise Refusal(
                f"{label} URL {record.get('url')!r} does not match corpus schedule "
                f"{expected_url!r}"
            )
        if record.get("ok") is not True:
            raise Refusal(f"{label} is not a successful VM rep")
        if is_warmup:
            continue

        if arm in ("cdp", "noop"):
            validate_vm_metric(record, "blocking_ms", label)
            validate_vm_metric(record, "wall_ms", label)
        if arm == "cdp":
            render = record.get("render")
            if not isinstance(render, dict):
                raise Refusal(f"{label} has no render object")
            if render.get("ok") is not True:
                raise Refusal(f"{label} render success is not true")
            if render.get("url") != expected_url:
                raise Refusal(f"{label} render URL does not match its VM record")
            stages = render.get("stages")
            nav = render.get("nav")
            if not isinstance(stages, dict):
                raise Refusal(f"{label} render has no stages object")
            if not isinstance(nav, dict):
                raise Refusal(f"{label} render has no nav object")
            validate_vm_metric(stages, "total_ms", f"{label} render stages")
            validate_vm_metric(nav, "load_ms", f"{label} render nav")
        measured.append(record)
    return meta, measured, artifact_identity(artifact)


def driver_total(rec):
    st = (rec.get("render") or {}).get("stages") or rec.get("stages") or {}
    return st.get("total_ms")


def nav_load(rec):
    nav = (rec.get("render") or {}).get("nav") or rec.get("nav") or {}
    return nav.get("load_ms")


def host_counts(meta, label):
    reps = integer_field(meta, "reps", label, minimum=1)
    warmup = integer_field(meta, "warmup", label)
    if "total_reps" in meta:
        total = integer_field(meta, "total_reps", label, minimum=1)
        measured = reps
        if total != warmup + measured:
            raise Refusal(
                f"{label} total_reps={total} does not equal "
                f"warmup={warmup} + measured reps={measured}"
            )
        convention = "measured-plus-warmup"
    else:
        # Documented pre-total_reps schema: reps was the total attempt count.
        total = reps
        measured = total - warmup
        if measured < 1:
            raise Refusal(
                f"{label} legacy reps={total} and warmup={warmup} leave no measured reps"
            )
        convention = "legacy-total"
    return {"measured": measured, "warmup": warmup, "total": total,
            "convention": convention}


def reject_withdrawn(directory):
    absolute = os.path.realpath(directory)
    for owner in (absolute, os.path.dirname(absolute)):
        marker = os.path.join(owner, "WITHDRAWN")
        try:
            os.lstat(marker)
        except FileNotFoundError:
            continue
        except OSError as error:
            raise Refusal(f"cannot inspect withdrawal marker {marker}: {error}") from error
        raise Refusal(f"host input is WITHDRAWN by {marker}")


def reject_vm_withdrawn(run_dir):
    marker = os.path.join(run_dir, campaign_summary.WITHDRAWN_MARKER)
    try:
        os.lstat(marker)
    except FileNotFoundError:
        return
    except OSError as error:
        raise Refusal(
            f"cannot inspect VM withdrawal marker {marker}: {error}"
        ) from error
    reason = campaign_summary.withdrawal_reason(marker)
    raise Refusal(f"VM input is WITHDRAWN by {marker}: {reason}")


def completion_identity(record, label):
    if not isinstance(record, dict) or set(record) != {"size", "sha256"}:
        raise Refusal(f"{label} must contain exactly size and sha256")
    size = record.get("size")
    digest = record.get("sha256")
    if isinstance(size, bool) or not isinstance(size, int) or size < 0:
        raise Refusal(f"{label} has invalid size={size!r}")
    if (
        not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
    ):
        raise Refusal(f"{label} has invalid sha256={digest!r}")
    return {"size": size, "sha256": digest}


def validate_host_completion(directory, run_id, artifacts):
    path = os.path.join(directory, "complete.json")
    completion_artifact = read_artifact(path)
    completion = parse_json(completion_artifact["text"], path)
    if set(completion) != {"schema_version", "run_id", "artifacts"}:
        raise Refusal(
            f"{path} must contain exactly schema_version, run_id, and artifacts"
        )
    version = completion.get("schema_version")
    if isinstance(version, bool) or not isinstance(version, int) or version != 1:
        raise Refusal(f"{path} has unsupported schema_version={version!r}")
    if completion.get("run_id") != run_id:
        raise Refusal(
            f"{path} run_id={completion.get('run_id')!r} does not match "
            f"run.json run_id={run_id!r}"
        )
    declared = completion.get("artifacts")
    expected_names = {"run.json", "hostcdp.jsonl"}
    if not isinstance(declared, dict) or set(declared) != expected_names:
        raise Refusal(
            f"{path} artifacts must identify exactly run.json and hostcdp.jsonl"
        )
    for name in sorted(expected_names):
        recorded = completion_identity(
            declared[name], f"{path} artifacts[{name!r}]"
        )
        current = artifacts[name]
        if (
            recorded["size"] != current["size"]
            or recorded["sha256"] != current["sha256"]
        ):
            raise Refusal(
                f"{path} does not bind the current {name}: current "
                f"size={current['size']} sha256={current['sha256']}, recorded "
                f"size={recorded['size']} sha256={recorded['sha256']}"
            )
    return artifact_identity(completion_artifact)


def campaign_arm(path, label):
    if not isinstance(path, str):
        raise Refusal(f"{label} has invalid path={path!r}")
    parts = path.split("/")
    if len(parts) != 2 or parts[1] != "complete.json":
        raise Refusal(f"{label} has non-canonical path={path!r}")
    prefix = "hostcdp-"
    directory = parts[0]
    arm = directory[len(prefix):] if directory.startswith(prefix) else ""
    if (
        not arm
        or len(arm) > 63
        or not arm[0].isascii()
        or not arm[0].isalnum()
        or any(
            not character.isascii()
            or not (character.isalnum() or character in "-_.")
            for character in arm[1:]
        )
        or path != f"hostcdp-{arm}/complete.json"
    ):
        raise Refusal(f"{label} has non-canonical path={path!r}")
    return arm


def validate_campaign_completion(directory, meta, child_completion):
    meta_path = os.path.join(directory, "run.json")
    key = "corpus_extra_runtime_bundle_sha256"
    if key not in meta:
        raise Refusal(f"{meta_path} has no {key}")
    runtime_sha256 = meta.get(key)
    if runtime_sha256 is None:
        return None, None
    runtime_sha256 = sha256_field(meta, key, meta_path)

    host_directory = os.path.realpath(directory)
    campaign_directory = os.path.dirname(host_directory)
    child_path = f"{os.path.basename(host_directory)}/complete.json"
    arm = campaign_arm(child_path, f"host directory {host_directory}")
    if meta.get("comparison_label") != arm:
        raise Refusal(
            f"{meta_path} comparison_label={meta.get('comparison_label')!r} "
            f"does not match campaign child arm={arm!r}"
        )

    path = os.path.join(campaign_directory, "campaign-complete.json")
    artifact = read_artifact(path)
    completion = parse_json(artifact["text"], path)
    expected_keys = {
        "schema_version",
        "run_id",
        "runtime_bundle_sha256",
        "phases",
        "host_completes",
        "memory_complete",
    }
    if set(completion) != expected_keys:
        raise Refusal(
            f"{path} must contain exactly schema_version, run_id, "
            "runtime_bundle_sha256, phases, host_completes, and "
            "memory_complete; older records do not bind memory completion"
        )
    version = completion.get("schema_version")
    if isinstance(version, bool) or not isinstance(version, int) or version != 2:
        raise Refusal(f"{path} has unsupported schema_version={version!r}")
    campaign_run_id = completion.get("run_id")
    if (
        not isinstance(campaign_run_id, str)
        or len(campaign_run_id) != 32
        or any(
            character not in "0123456789abcdef"
            for character in campaign_run_id
        )
    ):
        raise Refusal(f"{path} has invalid run_id={campaign_run_id!r}")
    completion_runtime = sha256_field(
        completion, "runtime_bundle_sha256", path
    )
    if completion_runtime != runtime_sha256:
        raise Refusal(
            f"{path} runtime_bundle_sha256={completion_runtime!r} does not "
            f"match run.json {key}={runtime_sha256!r}"
        )
    expected_child_run_id = f"{campaign_run_id}-{arm}"
    if meta.get("run_id") != expected_child_run_id:
        raise Refusal(
            f"{path} run_id={campaign_run_id!r} and child arm={arm!r} "
            f"require run.json run_id={expected_child_run_id!r}, got "
            f"{meta.get('run_id')!r}"
        )

    phases = completion.get("phases")
    if (
        not isinstance(phases, list)
        or not phases
        or any(phase not in {"hostcdp", "memory"} for phase in phases)
        or len(phases) != len(set(phases))
    ):
        raise Refusal(f"{path} has invalid phases={phases!r}")
    if "hostcdp" not in phases:
        raise Refusal(f"{path} does not declare the hostcdp phase")

    children = completion.get("host_completes")
    if not isinstance(children, list) or not children:
        raise Refusal(f"{path} host_completes must be a nonempty array")
    paths = []
    selected = None
    for ordinal, record in enumerate(children):
        label = f"{path} host_completes[{ordinal}]"
        if not isinstance(record, dict) or set(record) != {
            "path", "size", "sha256"
        }:
            raise Refusal(
                f"{label} must contain exactly path, size, and sha256"
            )
        recorded_path = record.get("path")
        campaign_arm(recorded_path, label)
        identity = completion_identity(
            {"size": record.get("size"), "sha256": record.get("sha256")},
            label,
        )
        paths.append(recorded_path)
        if recorded_path == child_path:
            selected = identity
    if paths != sorted(paths):
        raise Refusal(f"{path} host_completes is not sorted by path")
    if len(paths) != len(set(paths)):
        raise Refusal(f"{path} host_completes contains duplicate paths")
    if selected is None:
        raise Refusal(f"{path} does not commit selected child {child_path}")
    if (
        selected["size"] != child_completion["size"]
        or selected["sha256"] != child_completion["sha256"]
    ):
        raise Refusal(
            f"{path} does not bind the current {child_path}: current "
            f"size={child_completion['size']} "
            f"sha256={child_completion['sha256']}, recorded "
            f"size={selected['size']} sha256={selected['sha256']}"
        )

    memory_record = completion.get("memory_complete")
    memory_identity = None
    if "memory" in phases:
        label = f"{path} memory_complete"
        if not isinstance(memory_record, dict) or set(memory_record) != {
            "path", "size", "sha256"
        }:
            raise Refusal(
                f"{label} must contain exactly path, size, and sha256"
            )
        if memory_record.get("path") != "memory/complete.json":
            raise Refusal(
                f"{label} has invalid path={memory_record.get('path')!r}"
            )
        expected_memory = completion_identity(
            {
                "size": memory_record.get("size"),
                "sha256": memory_record.get("sha256"),
            },
            label,
        )
        memory_artifact = read_artifact(
            os.path.join(campaign_directory, "memory", "complete.json")
        )
        memory_identity = artifact_identity(memory_artifact)
        if (
            expected_memory["size"] != memory_identity["size"]
            or expected_memory["sha256"] != memory_identity["sha256"]
        ):
            raise Refusal(
                f"{path} does not bind the current memory/complete.json: current "
                f"size={memory_identity['size']} "
                f"sha256={memory_identity['sha256']}, recorded "
                f"size={expected_memory['size']} "
                f"sha256={expected_memory['sha256']}"
            )
    elif memory_record is not None:
        raise Refusal(
            f"{path} binds memory_complete without declaring the memory phase"
        )
    return artifact_identity(artifact), memory_identity


def revalidate_artifact_identity(label, expected):
    """Refuse if one captured artifact no longer names the same exact bytes."""
    current = artifact_identity(read_artifact(expected["path"]))
    if current != expected:
        raise Refusal(
            f"captured input {label} changed before publication: "
            f"{expected['path']}"
        )


def revalidate_host_inputs(directory, identities):
    """Refuse if any captured authorization/input bytes changed."""
    reject_withdrawn(directory)
    for name, expected in sorted(identities.items()):
        revalidate_artifact_identity(f"host {name}", expected)
    reject_withdrawn(directory)


def load_host_dataset(directory, require_driver=False):
    reject_withdrawn(directory)
    meta_artifact = read_artifact(os.path.join(directory, "run.json"))
    rows_artifact = read_artifact(os.path.join(directory, "hostcdp.jsonl"))
    meta = parse_json(meta_artifact["text"], meta_artifact["path"])
    records = parse_jsonl(rows_artifact["text"], rows_artifact["path"])
    run_id = meta.get("run_id")
    if not isinstance(run_id, str) or not run_id:
        raise Refusal(f"{meta_artifact['path']} names no host run_id")
    bound_artifacts = {
        "run.json": meta_artifact,
        "hostcdp.jsonl": rows_artifact,
    }
    completion_identity_record = validate_host_completion(
        directory, run_id, bound_artifacts
    )
    (
        campaign_completion_identity,
        memory_completion_identity,
    ) = validate_campaign_completion(directory, meta, completion_identity_record)
    reject_withdrawn(directory)
    counts = host_counts(meta, meta_artifact["path"])
    urls = declared_urls(meta, meta_artifact["path"])
    if integer_field(meta, "url_count", meta_artifact["path"], minimum=1) != len(urls):
        raise Refusal(
            f"{meta_artifact['path']} url_count does not match its ordered corpus"
        )
    if len(records) != counts["total"]:
        raise Refusal(
            f"{rows_artifact['path']} has {len(records)} rows but run.json "
            f"declares total_reps={counts['total']}"
        )
    failed_measured = [
        record for record in records
        if record.get("warmup") is False and record.get("ok") is not True
    ]
    if failed_measured:
        first = failed_measured[0]
        raise Refusal(
            f"{len(failed_measured)} of {counts['measured']} measured reps in "
            f"{directory} failed; first: rep {first.get('rep')} {first.get('url')}"
        )

    measured_rows = []
    for ordinal, record in enumerate(records):
        label = f"{rows_artifact['path']}:{ordinal + 1}"
        if record.get("run_json_sha256") != meta_artifact["sha256"]:
            raise Refusal(
                f"{label} is not bound to this run.json: "
                f"run_json_sha256={record.get('run_json_sha256')!r}, "
                f"expected {meta_artifact['sha256']}"
            )
        rep = integer_field(record, "rep", label)
        if rep != ordinal:
            raise Refusal(f"{label} has rep={rep}; expected {ordinal}")
        expected_warmup = rep < counts["warmup"]
        if record.get("warmup") is not expected_warmup:
            raise Refusal(
                f"{label} has warmup={record.get('warmup')!r}; expected {expected_warmup}"
            )
        if record.get("ok") is not True:
            raise Refusal(f"{label} is not a successful host rep")
        if record.get("measurement_valid") is not True:
            raise Refusal(
                f"{label} has measurement_valid="
                f"{record.get('measurement_valid')!r}; expected true"
            )
        load_status = integer_field(record, "loadavg1_read_status", label)
        if load_status != 0:
            raise Refusal(
                f"{label} has loadavg1_read_status={load_status}; expected 0"
            )
        expected_url = urls[rep % len(urls)]
        if record.get("url") != expected_url:
            raise Refusal(
                f"{label} URL {record.get('url')!r} does not match corpus schedule "
                f"{expected_url!r}"
            )
        if not finite_nonnegative(record.get("wall_ms")):
            raise Refusal(f"{label} has invalid wall_ms={record.get('wall_ms')!r}")
        if not finite_nonnegative(record.get("loadavg1")):
            raise Refusal(f"{label} has invalid loadavg1={record.get('loadavg1')!r}")

        transformed = {"url": expected_url, "wall_ms": record["wall_ms"]}
        if require_driver:
            driver_text = record.get("driver")
            if not isinstance(driver_text, str):
                raise Refusal(f"{label} has no driver JSON")
            driver = parse_json(driver_text, f"{label} driver")
            stages = driver.get("stages")
            nav = driver.get("nav")
            if not isinstance(stages, dict):
                raise Refusal(f"{label} driver has no stages object")
            if not isinstance(nav, dict):
                raise Refusal(f"{label} driver has no nav object")
            total_ms = stages.get("total_ms")
            load_ms = nav.get("load_ms")
            if driver.get("ok") is not True:
                raise Refusal(f"{label} driver does not say it succeeded")
            if driver.get("url") != expected_url:
                raise Refusal(f"{label} driver URL does not match its host record")
            if not finite_nonnegative(total_ms):
                raise Refusal(f"{label} driver has invalid total_ms={total_ms!r}")
            if not finite_nonnegative(load_ms):
                raise Refusal(f"{label} driver has invalid load_ms={load_ms!r}")
            transformed.update(total_ms=total_ms, load_ms=load_ms)
        if not expected_warmup:
            measured_rows.append(transformed)

    if len(measured_rows) != counts["measured"]:
        raise Refusal(
            f"{rows_artifact['path']} has {len(measured_rows)} measured rows but "
            f"run.json declares {counts['measured']}"
        )
    identities = {
        "run_json": artifact_identity(meta_artifact),
        "hostcdp_jsonl": artifact_identity(rows_artifact),
        "complete_json": completion_identity_record,
    }
    if campaign_completion_identity is not None:
        identities["campaign_complete_json"] = campaign_completion_identity
    if memory_completion_identity is not None:
        identities["memory_complete_json"] = memory_completion_identity
    revalidate_host_inputs(directory, identities)
    return meta, records, measured_rows, counts, identities


def normalize_image_id(value, label):
    if not isinstance(value, str) or not value:
        raise Refusal(f"{label} names no image_id")
    normalized = value.lower()
    if normalized.startswith("sha256:"):
        normalized = normalized[7:]
    if len(normalized) != 64 or any(c not in "0123456789abcdef" for c in normalized):
        raise Refusal(f"{label} has invalid image_id={value!r}")
    return normalized


PUBLISHED_CELL_FIELDS = (
    "cpu",
    "memory_mib",
    "backend",
    "uffd_mode",
    "snapshot",
    "image_id",
    "source_revision",
    "fcvm_sha256",
    "runtime_bundle_sha256",
    "host_kernel_release",
    "host_machine",
)


def exact_string_field(record, key, label):
    value = record.get(key)
    if not isinstance(value, str) or not value or value != value.strip():
        raise Refusal(f"{label} has invalid {key}={value!r}")
    return value


def lowercase_hex_field(record, key, label, length):
    value = exact_string_field(record, key, label)
    if len(value) != length or any(
        character not in "0123456789abcdef" for character in value
    ):
        raise Refusal(f"{label} has invalid {key}={record.get(key)!r}")
    return value


def source_revision_field(record, label):
    value = exact_string_field(record, "source_revision", label)
    if len(value) not in (40, 64) or any(
        character not in "0123456789abcdef" for character in value
    ):
        raise Refusal(
            f"{label} has invalid source_revision="
            f"{record.get('source_revision')!r}"
        )
    return value


def snapshot_name_field(record, label):
    value = exact_string_field(record, "snapshot", label)
    if (
        len(value) > 128
        or value in (".", "..")
        or any(
            not character.isascii()
            or not (character.isalnum() or character in "-_.")
            for character in value
        )
    ):
        raise Refusal(f"{label} has invalid snapshot={value!r}")
    return value


def validated_published_cell(record, label):
    if not isinstance(record, dict):
        raise Refusal(f"{label} cell is not an object")
    cell = {
        "cpu": integer_field(record, "cpu", label, minimum=1),
        "memory_mib": integer_field(record, "memory_mib", label, minimum=1),
        "backend": exact_string_field(record, "backend", label),
        "uffd_mode": exact_string_field(record, "uffd_mode", label),
        "snapshot": snapshot_name_field(record, label),
        "image_id": "sha256:" + normalize_image_id(
            record.get("image_id"), label
        ),
        "source_revision": source_revision_field(record, label),
        "fcvm_sha256": lowercase_hex_field(
            record, "fcvm_sha256", label, 64
        ),
        "runtime_bundle_sha256": lowercase_hex_field(
            record, "runtime_bundle_sha256", label, 64
        ),
        "host_kernel_release": exact_string_field(
            record, "host_kernel_release", label
        ),
        "host_machine": exact_string_field(record, "host_machine", label),
    }
    if cell["backend"] not in ("file", "uffd"):
        raise Refusal(f"{label} has invalid backend={cell['backend']!r}")
    if cell["uffd_mode"] not in ("file", "copy", "minor"):
        raise Refusal(f"{label} has invalid uffd_mode={cell['uffd_mode']!r}")
    valid_pair = (
        cell["backend"] == "file" and cell["uffd_mode"] == "file"
    ) or (
        cell["backend"] == "uffd" and cell["uffd_mode"] in ("copy", "minor")
    )
    if not valid_pair:
        raise Refusal(
            f"{label} has inconsistent backend={cell['backend']!r} and "
            f"uffd_mode={cell['uffd_mode']!r}"
        )
    return cell


def bind_analysis_cell(analysis, vm_meta):
    analysis_cell = validated_published_cell(
        analysis.get("cell"), "analysis.json"
    )
    raw_cell = validated_published_cell(vm_meta, "VM reqbench metadata")
    for field in PUBLISHED_CELL_FIELDS:
        if analysis_cell[field] != raw_cell[field]:
            raise Refusal(
                f"analysis.json cell {field}={analysis_cell[field]!r} does not "
                f"match VM reqbench metadata {field}={raw_cell[field]!r}"
            )
    return raw_cell


def canonical_ip(value, label):
    if not isinstance(value, str) or not value:
        raise Refusal(f"{label} names no resolver address")
    try:
        return str(ipaddress.ip_address(value))
    except ValueError as error:
        raise Refusal(f"{label} has invalid resolver address {value!r}") from error


def vm_resolver(meta):
    rules = []
    guest_env = meta.get("guest_env")
    if not isinstance(guest_env, list):
        raise Refusal("VM metadata guest_env is not a list")
    for entry in guest_env:
        if not isinstance(entry, str):
            raise Refusal("VM metadata guest_env is not a list of KEY=VALUE strings")
        key, separator, value = entry.partition("=")
        if separator and key == "BENCH_RESOLVE_ALL_TO":
            rules.append(canonical_ip(value, "VM BENCH_RESOLVE_ALL_TO"))
    if len(set(rules)) > 1:
        raise Refusal("VM metadata names multiple BENCH_RESOLVE_ALL_TO addresses")
    if rules:
        return rules[-1]
    value = meta.get("guest_dns")
    return None if value is None else canonical_ip(value, "VM guest_dns")


def url_needs_resolver(url):
    try:
        host = urlsplit(url).hostname
    except ValueError as error:
        raise Refusal(f"VM corpus contains invalid URL {url!r}: {error}") from error
    if not host:
        raise Refusal(f"VM corpus URL {url!r} names no host")
    if host.lower() == "localhost":
        return False
    try:
        ipaddress.ip_address(host)
        return False
    except ValueError:
        return True


def validate_host_compatibility(label, meta, host_rows, counts, vm_meta, vm_rows):
    vm_urls = declared_urls(vm_meta, "VM reqbench metadata")
    host_urls = declared_urls(meta, f"host {label} run.json")
    if host_urls != vm_urls:
        raise Refusal(
            f"host {label} corpus does not match the VM corpus: "
            f"host={host_urls!r} VM={vm_urls!r}"
        )
    vm_warmup = integer_field(vm_meta, "warmup", "VM reqbench metadata")
    if counts["warmup"] != vm_warmup:
        raise Refusal(
            f"host {label} has {counts['warmup']} warmup reps but the VM arm "
            f"declares {vm_warmup}"
        )
    if len(host_rows) != len(vm_rows):
        raise Refusal(
            f"host {label} has {len(host_rows)} measured reps but the VM cdp arm "
            f"has {len(vm_rows)}"
        )
    host_by_url = Counter(row["url"] for row in host_rows)
    vm_by_url = Counter(row.get("url") for row in vm_rows)
    if host_by_url != vm_by_url:
        raise Refusal(
            f"host {label} measured corpus counts do not match the VM arm: "
            f"host={dict(host_by_url)!r} VM={dict(vm_by_url)!r}"
        )

    vm_image = vm_meta.get("image")
    host_image = meta.get("image")
    if not isinstance(vm_image, str) or not vm_image:
        raise Refusal("VM reqbench metadata names no image")
    if host_image != vm_image:
        raise Refusal(
            f"host {label} image {host_image!r} does not match VM image {vm_image!r}"
        )
    vm_image_id = normalize_image_id(vm_meta.get("image_id"), "VM reqbench metadata")
    host_image_id = normalize_image_id(meta.get("image_id"), f"host {label} run.json")
    if host_image_id != vm_image_id:
        raise Refusal(
            f"host {label} image_id {meta.get('image_id')!r} does not match "
            f"VM image_id {vm_meta.get('image_id')!r}"
        )

    compatibility = (
        ("host_boot_id", "host_boot_id"),
        ("host_machine", "host_machine"),
        ("host_kernel", "host_kernel_release"),
        ("source_revision", "source_revision"),
        ("harness_sha256", "harness_sha256"),
        ("runtime_bundle_sha256", "runtime_bundle_sha256"),
    )
    for host_key, vm_key in compatibility:
        host_value = required_string(meta, host_key, f"host {label} run.json")
        vm_value = required_string(vm_meta, vm_key, "VM reqbench metadata")
        if host_key in ("harness_sha256", "runtime_bundle_sha256"):
            sha256_field(meta, host_key, f"host {label} run.json")
            sha256_field(vm_meta, vm_key, "VM reqbench metadata")
        if host_value != vm_value:
            raise Refusal(
                f"host {label} {host_key}={host_value!r} does not match "
                f"VM {vm_key}={vm_value!r}"
            )

    sha256_field(meta, "hostcdp_sha256", f"host {label} run.json")
    if meta.get("driver") != "cdpdrive.py":
        raise Refusal(
            f"host {label} driver={meta.get('driver')!r}; expected 'cdpdrive.py'"
        )
    if meta.get("network") != "host (no VM, no DNAT)":
        raise Refusal(
            f"host {label} network={meta.get('network')!r}; expected "
            "'host (no VM, no DNAT)'"
        )
    if meta.get("comparison_label") != label:
        raise Refusal(
            f"host {label} label does not match run.json comparison_label="
            f"{meta.get('comparison_label')!r}"
        )
    budget = meta.get("cpu_budget")
    cpus = meta.get("cpus")
    if budget == "unlimited":
        if cpus is not None:
            raise Refusal(
                f"host {label} cpu_budget='unlimited' requires cpus=null, got {cpus!r}"
            )
    elif budget == "vm-matched":
        if not finite_positive(cpus):
            raise Refusal(
                f"host {label} cpu_budget='vm-matched' has invalid cpus={cpus!r}"
            )
        vm_cpus = vm_meta.get("cpu")
        if not finite_positive(vm_cpus):
            raise Refusal(f"VM reqbench metadata has invalid cpu={vm_cpus!r}")
        if cpus != vm_cpus:
            raise Refusal(
                f"host {label} cpus={cpus!r} does not match VM cpu={vm_cpus!r}"
            )
    else:
        raise Refusal(
            f"host {label} has invalid cpu_budget={budget!r}; expected "
            "'unlimited' or 'vm-matched'"
        )

    if any(url_needs_resolver(url) for url in vm_urls):
        vm_address = vm_resolver(vm_meta)
        if vm_address is None:
            raise Refusal("VM hostname corpus names no resolver identity")
        host_value = meta.get("resolve_all_to")
        if host_value is None:
            raise Refusal(f"host {label} hostname corpus names no resolver identity")
        host_address = canonical_ip(host_value, f"host {label} resolve_all_to")
        # corpus_extra exposes one replay server at these two role-specific
        # addresses: loopback to the host browser, gateway to the guest.
        role_pair = vm_address == "10.0.2.2" and host_address == "127.0.0.1"
        if host_address != vm_address and not role_pair:
            raise Refusal(
                f"host {label} resolver {host_address} is incompatible with "
                f"VM resolver {vm_address}"
            )


def summarize(rows, key):
    vals = [r[key] for r in rows if r.get(key) is not None]
    return {"n": len(vals), "p50": round(pct(vals, 50), 1) if vals else None,
            "p95": round(pct(vals, 95), 1) if vals else None,
            "mean": round(statistics.mean(vals), 1) if vals else None}


def per_url(rows, key):
    out = {}
    for r in rows:
        if r.get(key) is None:
            continue
        out.setdefault(r["url"], []).append(r[key])
    return {u: {"n": len(v), "p50": round(pct(v, 50), 1)} for u, v in out.items()}


def comparison_input_paths(args):
    inputs = [
        ("VM analysis.json", os.path.join(args.vm_run, "analysis.json")),
        ("VM reqbench.jsonl", os.path.join(args.vm_run, "reqbench.jsonl")),
        ("VM WITHDRAWN", os.path.join(args.vm_run, "WITHDRAWN")),
        ("VM dns-evidence.json", os.path.join(args.vm_run, "dns-evidence.json")),
        ("VM dns-owner.log", os.path.join(args.vm_run, "dns-owner.log")),
        ("VM verify-dns-pre.json",
         os.path.join(args.vm_run, "verify-dns-pre.json")),
        ("VM verify-dns-before-run.json",
         os.path.join(args.vm_run, "verify-dns-before-run.json")),
        ("VM verify-dns-after-run.json",
         os.path.join(args.vm_run, "verify-dns-after-run.json")),
        ("VM corpus-dns.log", os.path.join(args.vm_run, "corpus-dns.log")),
        ("VM corpus-access.log", os.path.join(args.vm_run, "corpus-access.log")),
        ("VM replay-queries.log", os.path.join(args.vm_run, "replay-queries.log")),
        ("VM diag/summary.json",
         os.path.join(args.vm_run, "diag", "summary.json")),
        ("running comparator source", __file__),
        ("running publication validator source", campaign_summary.__file__),
    ]
    for ordinal, spec in enumerate(args.host, 1):
        _label, separator, directory = spec.partition("=")
        if separator and directory:
            campaign_directory = os.path.dirname(os.path.realpath(directory))
            inputs.extend((
                (f"host {ordinal} run.json", os.path.join(directory, "run.json")),
                (f"host {ordinal} hostcdp.jsonl",
                 os.path.join(directory, "hostcdp.jsonl")),
                (f"host {ordinal} complete.json",
                 os.path.join(directory, "complete.json")),
                (f"host {ordinal} WITHDRAWN",
                 os.path.join(directory, "WITHDRAWN")),
                (f"host {ordinal} campaign-complete.json",
                 os.path.join(campaign_directory, "campaign-complete.json")),
                (f"host {ordinal} memory complete.json",
                 os.path.join(campaign_directory, "memory", "complete.json")),
                (f"host {ordinal} campaign WITHDRAWN",
                 os.path.join(campaign_directory, "WITHDRAWN")),
            ))
    return inputs


def path_stat(path, label, fail_on_error, directory_fd=None):
    try:
        return os.stat(path, dir_fd=directory_fd)
    except FileNotFoundError:
        return None
    except OSError as error:
        if fail_on_error:
            raise Refusal(f"cannot inspect {label} {path}: {error}") from error
        return None


def reject_output_alias(output, inputs, target=None):
    output_absolute = target["absolute"] if target else os.path.abspath(output)
    output_realpath = os.path.realpath(output_absolute)
    if target:
        output_stat = path_stat(
            target["name"], "output", True, target["directory_fd"]
        )
    else:
        output_stat = path_stat(output_absolute, "output", True)
    protected_inputs = []
    for label, path in inputs:
        input_absolute = os.path.abspath(path)
        input_realpath = os.path.realpath(input_absolute)
        input_stat = path_stat(input_absolute, label, False)
        protected_inputs.append({
            "label": label,
            "path": path,
            "absolute": input_absolute,
            "realpath": input_realpath,
            "stat": input_stat,
        })
        same_inode = (
            output_stat is not None
            and input_stat is not None
            and os.path.samestat(output_stat, input_stat)
        )
        if (
            output_absolute == input_absolute
            or output_realpath == input_realpath
            or same_inode
        ):
            raise Refusal(f"--out {output!r} aliases {label} {path!r}")
    return {"output_stat": output_stat, "protected_inputs": protected_inputs}


def quarantined_alias(quarantined_stat, protected_inputs):
    for protected in protected_inputs:
        current_stat = path_stat(
            protected["absolute"], protected["label"], False
        )
        for input_stat in (protected["stat"], current_stat):
            if input_stat is not None and os.path.samestat(
                    quarantined_stat, input_stat):
                return protected
    return None


def restore_quarantined_output(target, quarantine):
    try:
        rename_noreplace(target["directory_fd"], quarantine, target["name"])
    except OSError as error:
        preserved = os.path.join(target["directory"], quarantine)
        raise Refusal(
            f"cannot restore raced output {target['path']}; its bytes remain at "
            f"{preserved}: {error}"
        ) from error


def clear_stale_output(target, preflight):
    while True:
        quarantine = f".compare-stale-{os.getpid()}-{secrets.token_hex(8)}"
        try:
            rename_noreplace(
                target["directory_fd"], target["name"], quarantine
            )
            break
        except FileNotFoundError:
            return
        except FileExistsError:
            continue
        except OSError as error:
            raise Refusal(
                f"cannot quarantine stale output {target['path']}: {error}"
            ) from error

    try:
        quarantined_stat = os.stat(
            quarantine, dir_fd=target["directory_fd"]
        )
    except OSError as error:
        restore_quarantined_output(target, quarantine)
        raise Refusal(
            f"cannot inspect quarantined output {target['path']}: {error}"
        ) from error
    expected_stat = preflight["output_stat"]
    stable_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
    if expected_stat is None or any(
            getattr(expected_stat, field) != getattr(quarantined_stat, field)
            for field in stable_fields):
        restore_quarantined_output(target, quarantine)
        raise Refusal(
            f"output {target['path']} changed after preflight; refusing to remove it"
        )
    protected = quarantined_alias(
        quarantined_stat, preflight["protected_inputs"]
    )
    if protected is not None:
        restore_quarantined_output(target, quarantine)
        raise Refusal(
            f"--out {target['path']!r} changed after preflight and aliases "
            f"{protected['label']} {protected['path']!r}"
        )
    try:
        os.unlink(quarantine, dir_fd=target["directory_fd"])
    except OSError as error:
        restore_quarantined_output(target, quarantine)
        raise Refusal(f"cannot clear stale output {target['path']}: {error}") from error


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--vm-run", required=True)
    ap.add_argument("--host", action="append", default=[], metavar="LABEL=DIR")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    # The lock file is permanent. Unlinking a flock inode while another caller
    # waits on it creates two lock domains and admits concurrent writers.
    target = open_output_target(a.out)
    try:
        lock_fd = open_output_lock(target)
        try:
            try:
                acquire_output_lock(target, lock_fd)
                ensure_output_directory(target)
                preflight = reject_output_alias(
                    a.out, comparison_input_paths(a), target
                )
                reject_vm_withdrawn(a.vm_run)
                ensure_output_directory(target)
                clear_stale_output(target, preflight)
                validate_output_lock(target, lock_fd)
            except OSError as error:
                raise Refusal(
                    f"cannot clear stale output {a.out}: {error}"
                ) from error
            run_comparison(a, target, lock_fd)
        finally:
            os.close(lock_fd)
    finally:
        os.close(target["directory_fd"])


def run_comparison(a, output_target=None, output_lock_fd=None):
    analysis, analysis_artifact, publication_inputs = load_vm_publication(
        a.vm_run
    )
    vm_meta, vm_all, vm_input_identity = load_vm(a.vm_run, analysis)
    published_cell = bind_analysis_cell(analysis, vm_meta)
    vm = [dict(r, total_ms=driver_total(r), load_ms=nav_load(r)) for r in vm_all if r["arm"] == "cdp"]
    noop = [r for r in vm_all if r["arm"] == "noop"]
    if not vm:
        raise Refusal("the bound VM input has no successful measured cdp records")

    out = {"vm_run": os.path.abspath(a.vm_run), "run_id": analysis.get("run_id"),
           "input_identity": {
               "analysis_json": artifact_identity(analysis_artifact),
               "vm_publication": publication_inputs,
               "reqbench_jsonl": vm_input_identity,
               "hosts": {},
           },
           "cell": published_cell,
           "vm": {"arm": "cdp",
                  "blocking_ms": summarize(vm, "blocking_ms"),
                  "wall_ms": summarize(vm, "wall_ms"),
                  "driver_total_ms": summarize(vm, "total_ms"),
                  "load_event_ms": summarize(vm, "load_ms"),
                  "per_url_blocking_p50": per_url(vm, "blocking_ms"),
                  "per_url_load_p50": per_url(vm, "load_ms")},
           "vm_noop": {"blocking_ms": summarize(noop, "blocking_ms"),
                       "wall_ms": summarize(noop, "wall_ms")},
           "hosts": {}, "ratios": {}}

    seen_host_datasets = {}
    seen_host_run_ids = {}
    host_rechecks = []
    hostcdp_sha256 = None
    for spec in a.host:
        label, _, d = spec.partition("=")
        if not label or not d:
            raise Refusal(f"invalid --host {spec!r}; expected LABEL=DIR")
        if label in out["hosts"]:
            raise Refusal(f"duplicate --host label {label!r}")
        meta, _records, rows, counts, identities = load_host_dataset(
            d, require_driver=True
        )
        dataset_identity = (
            identities["run_json"]["size"],
            identities["run_json"]["sha256"],
            identities["hostcdp_jsonl"]["size"],
            identities["hostcdp_jsonl"]["sha256"],
        )
        previous = seen_host_datasets.get(dataset_identity)
        if previous is not None:
            raise Refusal(
                f"the same host dataset was supplied as labels {previous!r} "
                f"and {label!r}"
            )
        previous = seen_host_run_ids.get(meta["run_id"])
        if previous is not None:
            raise Refusal(
                f"the same host dataset run_id={meta['run_id']!r} was supplied "
                f"as labels {previous!r} and {label!r}"
            )
        seen_host_datasets[dataset_identity] = label
        seen_host_run_ids[meta["run_id"]] = label
        host_rechecks.append((d, identities))
        validate_host_compatibility(label, meta, rows, counts, vm_meta, vm)
        current_hostcdp_sha256 = meta["hostcdp_sha256"]
        if hostcdp_sha256 is None:
            hostcdp_sha256 = current_hostcdp_sha256
        elif current_hostcdp_sha256 != hostcdp_sha256:
            raise Refusal(
                f"host {label} hostcdp_sha256={current_hostcdp_sha256!r} does "
                f"not match the comparison producer {hostcdp_sha256!r}"
            )
        out["input_identity"]["hosts"][label] = identities
        out["hosts"][label] = {
            "dir": os.path.abspath(d), "run_id": meta["run_id"],
            "comparison_label": meta["comparison_label"],
            "cpu_budget": meta["cpu_budget"], "cpus": meta.get("cpus"),
            "host_boot_id": meta["host_boot_id"],
            "host_machine": meta["host_machine"],
            "host_kernel": meta["host_kernel"],
            "source_revision": meta["source_revision"],
            "harness_sha256": meta["harness_sha256"],
            "runtime_bundle_sha256": meta["runtime_bundle_sha256"],
            "hostcdp_sha256": meta["hostcdp_sha256"],
            "driver": meta["driver"], "network": meta["network"],
            "image_id": meta.get("image_id"), "resolve_all_to": meta.get("resolve_all_to"),
            "reps": counts["measured"], "warmup": counts["warmup"],
            "total_reps": counts["total"], "count_convention": counts["convention"],
            "wall_ms": summarize(rows, "wall_ms"),
            "driver_total_ms": summarize(rows, "total_ms"),
            "load_event_ms": summarize(rows, "load_ms"),
            "per_url_wall_p50": per_url(rows, "wall_ms"),
            "per_url_load_p50": per_url(rows, "load_ms")}
        h = out["hosts"][label]
        out["ratios"][label] = {
            "vm_blocking_over_host_wall": round(out["vm"]["blocking_ms"]["p50"] / h["wall_ms"]["p50"], 2)
            if h["wall_ms"]["p50"] else None,
            "vm_driver_total_over_host_driver_total": round(
                out["vm"]["driver_total_ms"]["p50"] / h["driver_total_ms"]["p50"], 2)
            if h["driver_total_ms"]["p50"] else None,
            "vm_load_event_over_host_load_event": round(
                out["vm"]["load_event_ms"]["p50"] / h["load_event_ms"]["p50"], 2)
            if h["load_event_ms"]["p50"] else None,
        }

    def recheck_inputs_before_publication():
        for ordinal, identity in enumerate(
                out["input_identity"]["vm_publication"], 1):
            revalidate_artifact_identity(
                f"VM publication input {ordinal}", identity
            )
        revalidate_artifact_identity(
            "VM reqbench_jsonl", out["input_identity"]["reqbench_jsonl"]
        )
        for directory, identities in host_rechecks:
            revalidate_host_inputs(directory, identities)
        if output_target is not None and output_lock_fd is not None:
            validate_output_lock(output_target, output_lock_fd)
        reject_vm_withdrawn(a.vm_run)

    write_json_atomic(
        a.out,
        out,
        output_target,
        before_publish=recheck_inputs_before_publication,
    )
    print(json.dumps({k: out[k] for k in ("cell", "vm", "vm_noop", "ratios")}, indent=1)[:6000])
    print("\nper-URL wall/blocking p50 (ms)")
    urls = list(out["vm"]["per_url_blocking_p50"])
    hdr = f"{'url':60s} {'VM blocking':>12s} {'VM load':>9s}"
    for label in out["hosts"]:
        hdr += f" {label + ' wall':>14s} {label + ' load':>13s}"
    print(hdr)
    for u in urls:
        line = f"{u[:60]:60s} {out['vm']['per_url_blocking_p50'][u]['p50']:12.1f} " \
               f"{out['vm']['per_url_load_p50'].get(u, {}).get('p50', float('nan')):9.1f}"
        for label, h in out["hosts"].items():
            line += f" {h['per_url_wall_p50'].get(u, {}).get('p50', float('nan')):14.1f}" \
                    f" {h['per_url_load_p50'].get(u, {}).get('p50', float('nan')):13.1f}"
        print(line)
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    try:
        main()
    except Refusal as error:
        sys.exit(f"REFUSING: {error}")
