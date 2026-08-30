#!/usr/bin/env python3
"""Set the VM arm and the host-container arms side by side, per URL and overall.

Reads only records: the campaign's reqbench.jsonl + analysis.json (the VM arm,
already through its publication gate) and hostcdp.jsonl + run.json (the host
arms). The VM bytes must match the analysis identity, and every host row must
name the exact run.json bytes beside it. Prints two tables and the ratios, and
writes comparison.json.

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
import fcntl
import hashlib
import ipaddress
import json
import math
import os
import statistics
import sys
import tempfile
from urllib.parse import urlsplit


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


def write_json_atomic(path, value):
    directory = os.path.dirname(os.path.abspath(path)) or "."
    fd, temporary = tempfile.mkstemp(prefix=".compare-", dir=directory)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle, indent=1, allow_nan=False)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
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


def load_vm(run_dir, analysis):
    artifact = read_artifact(os.path.join(run_dir, "reqbench.jsonl"))
    bind_analysis_input(analysis, artifact)
    rows = parse_jsonl(artifact["text"], artifact["path"])
    metas = [row for row in rows if row.get("kind") == "meta"]
    if len(metas) != 1:
        raise Refusal(
            f"{artifact['path']} has {len(metas)} meta records; exactly one is required"
        )
    recs = []
    for row in rows:
        # A rep that does not say it worked is not a rep known to have worked.
        if row.get("arm") and row.get("warmup") is False and row.get("ok") is True:
            recs.append(row)
    return metas[0], recs, artifact_identity(artifact)


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


def load_host_dataset(directory, require_driver=False):
    meta_artifact = read_artifact(os.path.join(directory, "run.json"))
    rows_artifact = read_artifact(os.path.join(directory, "hostcdp.jsonl"))
    meta = parse_json(meta_artifact["text"], meta_artifact["path"])
    records = parse_jsonl(rows_artifact["text"], rows_artifact["path"])
    run_id = meta.get("run_id")
    if not isinstance(run_id, str) or not run_id:
        raise Refusal(f"{meta_artifact['path']} names no host run_id")
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
    }
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--vm-run", required=True)
    ap.add_argument("--host", action="append", default=[], metavar="LABEL=DIR")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    # The lock file is permanent. Unlinking a flock inode while another caller
    # waits on it creates two lock domains and admits concurrent writers.
    lock_path = f"{a.out}.lock"
    try:
        output_lock = open(lock_path, "a+")
    except OSError as error:
        raise Refusal(f"cannot open output lock {lock_path}: {error}") from error
    with output_lock:
        try:
            fcntl.flock(output_lock.fileno(), fcntl.LOCK_EX)
            os.unlink(a.out)
        except FileNotFoundError:
            pass
        except OSError as error:
            raise Refusal(f"cannot clear stale output {a.out}: {error}") from error
        run_comparison(a)


def run_comparison(a):
    analysis_artifact = read_artifact(os.path.join(a.vm_run, "analysis.json"))
    analysis = parse_json(analysis_artifact["text"], analysis_artifact["path"])
    if not analysis.get("publishable") or not analysis.get("gate", {}).get("passed"):
        raise Refusal(
            "the VM run did not pass its publication gate; its numbers are not quotable"
        )
    vm_meta, vm_all, vm_input_identity = load_vm(a.vm_run, analysis)
    vm = [dict(r, total_ms=driver_total(r), load_ms=nav_load(r)) for r in vm_all if r["arm"] == "cdp"]
    noop = [r for r in vm_all if r["arm"] == "noop"]
    if not vm:
        raise Refusal("the bound VM input has no successful measured cdp records")

    out = {"vm_run": os.path.abspath(a.vm_run), "run_id": analysis.get("run_id"),
           "input_identity": {
               "analysis_json": artifact_identity(analysis_artifact),
               "reqbench_jsonl": vm_input_identity,
               "hosts": {},
           },
           "cell": {k: analysis["cell"][k] for k in
                    ("cpu", "memory_mib", "backend", "uffd_mode", "snapshot", "image_id",
                     "source_revision", "fcvm_sha256", "runtime_bundle_sha256",
                     "host_kernel_release", "host_machine")},
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

    for spec in a.host:
        label, _, d = spec.partition("=")
        if not label or not d:
            raise Refusal(f"invalid --host {spec!r}; expected LABEL=DIR")
        if label in out["hosts"]:
            raise Refusal(f"duplicate --host label {label!r}")
        meta, _records, rows, counts, identities = load_host_dataset(
            d, require_driver=True
        )
        validate_host_compatibility(label, meta, rows, counts, vm_meta, vm)
        out["input_identity"]["hosts"][label] = identities
        out["hosts"][label] = {
            "dir": os.path.abspath(d), "run_id": meta["run_id"],
            "cpus": meta.get("cpus"),
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

    write_json_atomic(a.out, out)
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
