#!/usr/bin/env python3
"""Validate and summarize one reqscale run without inventing missing evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import stat
import statistics
import sys
import uuid
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import reqscale  # noqa: E402


ANALYSIS_SCHEMA = "fcvm.chromium.reqscale.analysis.v1"
BOOTSTRAP_DRAWS = 10_000


class AnalysisInvalid(RuntimeError):
    pass


def _strict_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise AnalysisInvalid(f"duplicate JSON object key {key!r}")
        value[key] = item
    return value


def _reject_json_constant(value):
    raise AnalysisInvalid(f"non-standard JSON constant {value}")


def _strict_load(stream):
    return json.load(
        stream,
        object_pairs_hook=_strict_object,
        parse_constant=_reject_json_constant,
    )


def _load_json(path: str):
    try:
        with open(path) as stream:
            value = _strict_load(stream)
        if not isinstance(value, dict):
            raise AnalysisInvalid(f"top-level JSON artifact {path} is not an object")
        return value
    except (OSError, ValueError) as error:
        raise AnalysisInvalid(f"cannot read JSON artifact {path}: {error}") from error


def _load_jsonl(path: str) -> list[dict]:
    rows = []
    try:
        with open(path) as stream:
            for number, line in enumerate(stream, 1):
                if not line.strip():
                    raise AnalysisInvalid(f"blank JSONL row at {path}:{number}")
                try:
                    row = json.loads(
                        line,
                        object_pairs_hook=_strict_object,
                        parse_constant=_reject_json_constant,
                    )
                except ValueError as error:
                    raise AnalysisInvalid(f"malformed JSONL at {path}:{number}: {error}") from error
                if not isinstance(row, dict):
                    raise AnalysisInvalid(f"non-object JSONL row at {path}:{number}")
                rows.append(row)
    except OSError as error:
        raise AnalysisInvalid(f"cannot read JSONL artifact {path}: {error}") from error
    return rows


def _nearest(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    if not ordered:
        raise AnalysisInvalid("cannot take a quantile of no values")
    index = max(0, min(len(ordered) - 1, math.ceil(quantile * len(ordered)) - 1))
    return float(ordered[index])


def bootstrap_mean_ci(
    values: list[float], seed: int, draws: int | None = None,
) -> dict:
    """A deterministic burst-resampling CI; requests are never resampled."""
    draws = BOOTSTRAP_DRAWS if draws is None else draws
    if len(values) < 5:
        raise AnalysisInvalid("a burst-level CI needs at least 5 independent bursts")
    if draws < 1000:
        raise AnalysisInvalid("bootstrap draw count is too small for a 95% interval")
    if any(not isinstance(value, (int, float)) or not math.isfinite(value) for value in values):
        raise AnalysisInvalid(f"bootstrap input contains a non-finite value: {values}")
    means = []
    count = len(values)
    for draw in range(draws):
        total = 0.0
        for sample in range(count):
            index = reqscale._derive_u64(seed, "bootstrap", draw, sample) % count
            total += values[index]
        means.append(total / count)
    return {
        "unit": "burst",
        "n": count,
        "point": statistics.mean(values),
        "ci95_low": _nearest(means, 0.025),
        "ci95_high": _nearest(means, 0.975),
        "method": "seeded percentile bootstrap of the burst mean",
        "draws": draws,
        "seed": seed,
    }


def _canonical_generation(value) -> str:
    try:
        canonical = str(uuid.UUID(value))
    except (AttributeError, TypeError, ValueError) as error:
        raise AnalysisInvalid("provenance has an invalid snapshot generation_id") from error
    if canonical != value:
        raise AnalysisInvalid("provenance has a non-canonical snapshot generation_id")
    return canonical


def _validate_schedule(schedule: dict) -> reqscale.ScheduleConfig:
    if not isinstance(schedule, dict):
        raise AnalysisInvalid("schedule is not an object")
    if schedule.get("schema") != reqscale.SCHEDULE_SCHEMA:
        raise AnalysisInvalid(f"unsupported schedule schema {schedule.get('schema')!r}")
    try:
        raw_criteria = schedule["capacity_criteria"]
        if not isinstance(raw_criteria, dict):
            raise AnalysisInvalid("capacity criteria are not an object")
        if raw_criteria.get("require_zero_failures") is not True:
            raise AnalysisInvalid("zero failures must be a mandatory capacity gate")
        criteria = reqscale.CapacityCriteria(
            max_offered_rps_error_pct=raw_criteria["max_offered_rps_error_pct"],
            min_departure_ratio=raw_criteria["min_departure_ratio"],
            max_score_end_backlog=raw_criteria["max_score_end_backlog"],
            max_p95_launch_lag_ms=raw_criteria["max_p95_launch_lag_ms"],
            max_control_median_drift_pct=raw_criteria[
                "max_control_median_drift_pct"
            ],
        )
        config = reqscale.ScheduleConfig(
            rates=tuple(schedule["rates"]),
            scored_bursts=schedule["independent_bursts_per_cell"],
            seed=schedule["seed"],
            criteria=criteria,
            warmup_bursts=schedule["warmup_bursts_per_rate"],
            ramp_seconds=schedule["ramp_seconds"],
            score_seconds=schedule["score_seconds"],
            trace_rate=schedule["trace_rate"],
            trace_pairs=schedule["trace_pairs"],
        )
        rebuilt = reqscale.build_schedule(config, schedule["run_id"])
    except (
        KeyError, TypeError, ValueError, OverflowError, reqscale.MeasurementInvalid,
    ) as error:
        raise AnalysisInvalid(f"durable schedule is invalid: {error}") from error
    if reqscale.canonical_json(rebuilt) != reqscale.canonical_json(schedule):
        raise AnalysisInvalid(
            "durable schedule does not match its seed, cells, and declared experiment shape"
        )
    return config


def _finite_number(value, name: str, *, minimum=None, maximum=None) -> float:
    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(value)
    ):
        raise AnalysisInvalid(f"{name} is not a finite number")
    if minimum is not None and value < minimum:
        raise AnalysisInvalid(f"{name} is below {minimum}")
    if maximum is not None and value > maximum:
        raise AnalysisInvalid(f"{name} is above {maximum}")
    return float(value)


def _validate_fault_metric(row: dict, traced: bool) -> None:
    faults = row.get("firecracker_process_faults_ready_to_artifact")
    if not isinstance(faults, dict):
        raise AnalysisInvalid(f"request {row.get('request_id')} lacks Firecracker fault counters")
    for field in ("pid", "pid_start_time_ticks", "minor_faults", "major_faults"):
        value = faults.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise AnalysisInvalid(
                f"request {row.get('request_id')} has invalid fault field {field}"
            )
    for side in ("before", "after"):
        counters = faults.get(side)
        if not isinstance(counters, dict) or set(counters) != {"minor_faults", "major_faults"}:
            raise AnalysisInvalid(
                f"request {row.get('request_id')} has incomplete fault {side} counters"
            )
        if any(
            not isinstance(value, int) or isinstance(value, bool) or value < 0
            for value in counters.values()
        ):
            raise AnalysisInvalid(
                f"request {row.get('request_id')} has invalid fault {side} counters"
            )
    if faults["after"]["minor_faults"] - faults["before"]["minor_faults"] != faults["minor_faults"]:
        raise AnalysisInvalid(f"request {row.get('request_id')} minor faults do not reconcile")
    if faults["after"]["major_faults"] - faults["before"]["major_faults"] != faults["major_faults"]:
        raise AnalysisInvalid(f"request {row.get('request_id')} major faults do not reconcile")
    if (
        not isinstance(faults.get("scope"), str)
        or "not guest-RAM-filtered" not in faults["scope"]
    ):
        raise AnalysisInvalid(f"request {row.get('request_id')} overclaims fault scope")
    traced_faults = row.get("firecracker_process_handle_mm_fault_ready_to_artifact")
    if traced and not isinstance(traced_faults, dict):
        raise AnalysisInvalid(f"traced request {row.get('request_id')} lacks joined fault timing")
    if not traced and traced_faults is not None:
        raise AnalysisInvalid(f"untraced request {row.get('request_id')} has trace-only fault timing")
    if traced:
        count = traced_faults.get("count")
        total = traced_faults.get("total_ns")
        histogram = traced_faults.get("histogram")
        valid_histogram = False
        histogram_count = -1
        if isinstance(histogram, list):
            valid_histogram = all(
                isinstance(bucket, dict)
                and set(bucket) == {"min", "max", "count"}
                and all(
                    isinstance(bucket[field], int) and not isinstance(bucket[field], bool)
                    for field in ("min", "max", "count")
                )
                and 0 <= bucket["min"] <= bucket["max"]
                and bucket["count"] >= 0
                for bucket in histogram
            )
            if valid_histogram:
                histogram_count = sum(bucket["count"] for bucket in histogram)
                valid_histogram = all(
                    current["min"] > previous["max"]
                    for previous, current in zip(histogram, histogram[1:])
                )
        if (
            not isinstance(count, int) or isinstance(count, bool) or count < 0
            or not isinstance(total, int) or isinstance(total, bool) or total < 0
            or not valid_histogram
            or histogram_count != count
            or not isinstance(traced_faults.get("scope"), str)
            or "not filtered to guest RAM" not in traced_faults["scope"]
        ):
            raise AnalysisInvalid(f"traced request {row.get('request_id')} has invalid fault timing")


def _recompute_backend_metrics(spec: reqscale.BurstSpec, records: list[dict], backend: str) -> dict:
    score_start_ns = records[0]["scheduled_ns"] - spec.requests[0].scheduled_offset_ns
    score_start_ns += round(spec.ramp_seconds * 1_000_000_000)
    score_end_ns = score_start_ns + round(spec.score_seconds * 1_000_000_000)
    rows = [
        row for row in records
        if row["segment"] == "score" and row["backend"] == backend
    ]
    planned = reqscale._planned_count(spec.target_rps, spec.score_seconds)
    if len(rows) != planned:
        raise AnalysisInvalid(
            f"burst {spec.burst_id} has {len(rows)} raw scored {backend} requests, "
            f"expected {planned}"
        )
    launched_by_end = sum(row["actual_launch_ns"] <= score_end_ns for row in rows)
    artifact_by_end = sum(row["artifact_ns"] <= score_end_ns for row in rows)
    return {
        "cell_id": f"{backend}:r{format(spec.target_rps, '.12g')}",
        "planned": planned,
        "launched": len(rows),
        "launched_by_score_end": launched_by_end,
        "artifact_completed": len(rows),
        "artifact_completed_by_score_end": artifact_by_end,
        "drained": len(rows),
        "cleanup_confirmed": sum(row["teardown"]["all_gone"] is True for row in rows),
        "ok": sum(row["ok"] is True for row in rows),
        "failed": sum(row["ok"] is not True for row in rows),
        "offered_rps": launched_by_end / spec.score_seconds,
        "departure_rps": artifact_by_end / spec.score_seconds,
        "departure_ratio": artifact_by_end / planned,
        "score_start_backlog": reqscale._backlog_at(records, backend, score_start_ns),
        "score_end_backlog": reqscale._backlog_at(records, backend, score_end_ns),
        "max_backlog_during_score": reqscale._max_backlog(
            records, backend, score_start_ns, score_end_ns
        ),
        "launch_lag_ms": reqscale.distribution([
            (row["actual_launch_ns"] - row["scheduled_ns"]) / 1_000_000
            for row in rows
        ]),
        "artifact_latency_ms": reqscale.distribution([
            (row["artifact_ns"] - row["actual_launch_ns"]) / 1_000_000
            for row in rows
        ]),
        "drain_latency_ms": reqscale.distribution([
            (row["finished_ns"] - row["artifact_ns"]) / 1_000_000
            for row in rows
        ]),
        "wall_latency_ms": reqscale.distribution([
            (row["finished_ns"] - row["actual_launch_ns"]) / 1_000_000
            for row in rows
        ]),
        "blocking_latency_ms": reqscale.distribution([
            float(row["blocking_ms"]) for row in rows
        ]),
    }


def _validate_burst_accounting(summary: dict) -> None:
    machine = {}
    for side in ("before", "after"):
        value = summary.get(f"machine_proc_stat_{side}")
        if (
            not isinstance(value, dict)
            or value.get("path") != "/proc/stat"
            or not isinstance(value.get("raw"), str)
            or not value["raw"]
        ):
            raise AnalysisInvalid(f"burst {summary.get('burst_id')} lacks whole /proc/stat {side}")
        raw = value["raw"]
        if value.get("raw_sha256") != hashlib.sha256(raw.encode()).hexdigest():
            raise AnalysisInvalid(f"burst {summary.get('burst_id')} /proc/stat {side} digest differs")
        _validate_whole_proc_stat(
            raw, f"burst {summary.get('burst_id')} /proc/stat {side}"
        )
        parsed = _proc_cpu_from_raw(raw, f"burst {summary.get('burst_id')} /proc/stat {side}")
        if value.get("cpu") != parsed:
            raise AnalysisInvalid(f"burst {summary.get('burst_id')} /proc/stat {side} parsing differs")
        for field in ("captured_wall_ns", "captured_monotonic_ns", "clk_tck"):
            captured = value.get(field)
            if (
                not isinstance(captured, int)
                or isinstance(captured, bool)
                or captured <= 0
            ):
                raise AnalysisInvalid(
                    f"burst {summary.get('burst_id')} /proc/stat {side} lacks {field}"
                )
        machine[side] = parsed
    before_capture = summary["machine_proc_stat_before"]["captured_monotonic_ns"]
    after_capture = summary["machine_proc_stat_after"]["captured_monotonic_ns"]
    if before_capture > after_capture:
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} /proc/stat timestamps moved backwards")
    if (
        summary["machine_proc_stat_before"]["clk_tck"]
        != summary["machine_proc_stat_after"]["clk_tck"]
    ):
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} /proc/stat clock changed")
    try:
        expected_machine_delta = reqscale.counter_delta(machine["before"], machine["after"])
    except (TypeError, reqscale.MeasurementInvalid) as error:
        raise AnalysisInvalid(f"burst machine counters are invalid: {error}") from error
    if summary.get("machine_proc_stat_delta") != expected_machine_delta:
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} machine CPU delta differs")

    required = {"run", "driver", "control", "file", "uffd"}
    before = summary.get("cgroup_cpu_stat_before")
    after = summary.get("cgroup_cpu_stat_after")
    deltas = summary.get("cgroup_cpu_stat_delta")
    if not all(isinstance(value, dict) and set(value) == required for value in (before, after, deltas)):
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} lacks split cgroup CPU accounting")
    for name in required:
        before_value = _validate_counter_snapshot(
            before[name], f"burst {summary.get('burst_id')} {name} cpu.stat before"
        )
        after_value = _validate_counter_snapshot(
            after[name], f"burst {summary.get('burst_id')} {name} cpu.stat after"
        )
        delta_value = _validate_counter_snapshot(
            deltas[name], f"burst {summary.get('burst_id')} {name} cpu.stat delta"
        )
        try:
            expected = reqscale.counter_delta(before_value, after_value)
        except (TypeError, reqscale.MeasurementInvalid) as error:
            raise AnalysisInvalid(f"burst {summary.get('burst_id')} {name} CPU counters are invalid: {error}") from error
        if delta_value != expected:
            raise AnalysisInvalid(f"burst {summary.get('burst_id')} {name} CPU delta differs")
    membership = summary.get("interburst_cgroup_membership")
    if not isinstance(membership, dict) or set(membership) != required:
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} lacks inter-burst membership")
    if membership["run"] != [] or membership["file"] != []:
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} cgroups are not quiescent")
    for name, pids in membership.items():
        if (
            not isinstance(pids, list)
            or any(
                not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0
                for pid in pids
            )
            or pids != sorted(set(pids))
        ):
            raise AnalysisInvalid(
                f"burst {summary.get('burst_id')} {name} membership is invalid"
            )
    if (
        len(membership["driver"]) != 1
        or len(membership["uffd"]) != 1
        or not membership["control"]
    ):
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} persistent cgroup members are missing")
    leaf_pids = [
        pid for name in required - {"run"} for pid in membership[name]
    ]
    if len(leaf_pids) != len(set(leaf_pids)):
        raise AnalysisInvalid(f"burst {summary.get('burst_id')} PID appears in multiple leaf cgroups")


def _validate_requests(
    schedule: dict,
    summaries: list[dict],
    requests: list[dict],
    generation_id: str,
    config_sha256: str,
) -> dict[str, list[dict]]:
    summary_by_id = {}
    for summary in summaries:
        if summary.get("schema") != reqscale.RECORD_SCHEMA or summary.get("kind") != "burst":
            raise AnalysisInvalid("burst summary has an unsupported schema or kind")
        burst_id = summary.get("burst_id")
        if not isinstance(burst_id, str) or burst_id in summary_by_id:
            raise AnalysisInvalid(f"duplicate or invalid burst summary id {burst_id!r}")
        summary_by_id[burst_id] = summary
        _validate_burst_accounting(summary)
    expected_bursts = [reqscale.BurstSpec.from_dict(raw) for raw in schedule["bursts"]]
    expected_ids = [spec.burst_id for spec in expected_bursts]
    if [summary.get("burst_id") for summary in summaries] != expected_ids:
        raise AnalysisInvalid("ordered burst summaries do not exactly match the durable schedule")

    rows_by_key = {}
    firecracker_identities = set()
    for row in requests:
        key = (row.get("burst_id"), row.get("request_index"))
        if key in rows_by_key:
            raise AnalysisInvalid(f"duplicate request record {key}")
        rows_by_key[key] = row
    expected_count = sum(len(spec.requests) for spec in expected_bursts)
    if len(rows_by_key) != expected_count:
        raise AnalysisInvalid(
            f"request artifact has {len(rows_by_key)} rows, expected {expected_count}"
        )
    previous_drain_ns = None
    previous_machine_after = None
    previous_cgroup_after = None
    for spec in expected_bursts:
        summary = summary_by_id[spec.burst_id]
        start_ns = summary.get("burst_start_ns")
        if not isinstance(start_ns, int):
            raise AnalysisInvalid(f"burst {spec.burst_id} has no monotonic start")
        if previous_drain_ns is not None and start_ns < previous_drain_ns:
            raise AnalysisInvalid(f"burst {spec.burst_id} overlaps the preceding burst drain")
        try:
            if previous_machine_after is not None:
                reqscale.counter_delta(
                    previous_machine_after, summary["machine_proc_stat_before"]["cpu"]
                )
            if previous_cgroup_after is not None:
                for name in ("run", "driver", "control", "file", "uffd"):
                    reqscale.counter_delta(
                        previous_cgroup_after[name],
                        summary["cgroup_cpu_stat_before"][name],
                    )
        except reqscale.MeasurementInvalid as error:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} accounting moved backwards between bursts: {error}"
            ) from error
        if summary.get("request_plan_sha256") != hashlib.sha256(
            reqscale.canonical_json([item.to_dict() for item in spec.requests])
        ).hexdigest():
            raise AnalysisInvalid(f"burst {spec.burst_id} summary names a different request plan")
        expected_summary_metadata = {
            "block_id": spec.block_id,
            "population": spec.population,
            "target_rps": spec.target_rps,
            "repeat": spec.repeat,
            "seed": spec.seed,
            "traced": spec.traced,
            "trace_pair_id": spec.trace_pair_id,
            "ramp_seconds": spec.ramp_seconds,
            "score_seconds": spec.score_seconds,
            "request_plan_count": len(spec.requests),
        }
        mismatch = {
            name: {"expected": value, "actual": summary.get(name)}
            for name, value in expected_summary_metadata.items()
            if summary.get(name) != value
        }
        if mismatch:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} summary metadata diverges from schedule: {mismatch}"
            )
        burst_rows = []
        for planned in spec.requests:
            key = (spec.burst_id, planned.request_index)
            row = rows_by_key.get(key)
            if row is None:
                raise AnalysisInvalid(f"missing request record {key}")
            expected = {
                "schema": reqscale.RECORD_SCHEMA,
                "kind": "request",
                "run_id": schedule["run_id"],
                "request_id": f"{schedule['run_id']}:{spec.burst_id}:{planned.request_index}",
                "block_id": spec.block_id,
                "cell_id": f"{planned.backend}:r{format(spec.target_rps, '.12g')}",
                "backend": planned.backend,
                "segment": planned.segment,
                "pair_index": planned.pair_index,
                "request_seed": planned.seed,
                "population": spec.population,
                "target_rps": spec.target_rps,
                "scheduled_ns": start_ns + planned.scheduled_offset_ns,
                "traced": spec.traced,
                "trace_pair_id": spec.trace_pair_id,
                "snapshot_generation_id": generation_id,
                "snapshot_config_sha256": config_sha256,
            }
            mismatch = {
                name: {"expected": value, "actual": row.get(name)}
                for name, value in expected.items() if row.get(name) != value
            }
            if mismatch:
                raise AnalysisInvalid(f"request {key} diverges from schedule: {mismatch}")
            try:
                reqscale._validate_request_record(row)
            except reqscale.MeasurementInvalid as error:
                raise AnalysisInvalid(f"request {key} is invalid: {error}") from error
            if row.get("ok") is not True:
                raise AnalysisInvalid(f"request {key} failed")
            if not isinstance(row.get("teardown"), dict) or row["teardown"].get("all_gone") is not True:
                raise AnalysisInvalid(f"request {key} lacks confirmed teardown")
            _validate_fault_metric(row, spec.traced)
            faults = row["firecracker_process_faults_ready_to_artifact"]
            identity = (faults["pid"], faults["pid_start_time_ticks"])
            if identity in firecracker_identities:
                raise AnalysisInvalid(f"Firecracker identity {identity} served multiple requests")
            firecracker_identities.add(identity)
            burst_rows.append(row)

        expected_start_ns = start_ns + round(spec.ramp_seconds * 1_000_000_000)
        expected_end_ns = expected_start_ns + round(spec.score_seconds * 1_000_000_000)
        if summary.get("score_start_ns") != expected_start_ns or summary.get("score_end_ns") != expected_end_ns:
            raise AnalysisInvalid(f"burst {spec.burst_id} score boundaries are inconsistent")
        expected_backends = {
            backend: _recompute_backend_metrics(spec, burst_rows, backend)
            for backend in ("file", "uffd")
        }
        if summary.get("backends") != expected_backends:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} derived backend metrics differ from raw requests"
            )
        scored = [row for row in burst_rows if row["segment"] == "score"]
        expected_totals = {
            "planned": len(scored),
            "launched": len(scored),
            "artifact_completed": len(scored),
            "drained": len(scored),
            "cleanup_confirmed": len(scored),
            "ok": len(scored),
            "failed": 0,
            "total_planned": len(burst_rows),
            "total_artifact_completed": len(burst_rows),
            "total_drained": len(burst_rows),
            "total_cleanup_confirmed": len(burst_rows),
        }
        mismatch = {
            name: {"expected": value, "actual": summary.get(name)}
            for name, value in expected_totals.items() if summary.get(name) != value
        }
        if mismatch:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} totals differ from raw requests: {mismatch}"
            )
        expected_spans = {
            "launch_span_ms": (
                max(row["actual_launch_ns"] for row in burst_rows) - start_ns
            ) / 1_000_000,
            "completion_span_ms": (
                max(row["artifact_ns"] for row in burst_rows) - start_ns
            ) / 1_000_000,
            "drain_span_ms": (
                max(row["finished_ns"] for row in burst_rows) - start_ns
            ) / 1_000_000,
            "launch_lag_ms": reqscale.distribution([
                (row["actual_launch_ns"] - row["scheduled_ns"]) / 1_000_000
                for row in scored
            ]),
            "latency_ms": reqscale.distribution([
                float(row["blocking_ms"]) for row in scored
            ]),
        }
        mismatch = {
            name: {"expected": value, "actual": summary.get(name)}
            for name, value in expected_spans.items() if summary.get(name) != value
        }
        if mismatch:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} milestone summaries differ from raw requests: {mismatch}"
            )
        last_deadline_ns = start_ns + spec.requests[-1].scheduled_offset_ns
        submission_span = _finite_number(
            summary.get("schedule_submission_span_ms"),
            f"burst {spec.burst_id} schedule submission span",
            minimum=0,
        )
        submission_ns = start_ns + round(submission_span * 1_000_000)
        if submission_ns < last_deadline_ns:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} claims submission before its last deadline"
            )
        previous_drain_ns = max(row["finished_ns"] for row in burst_rows)
        proc_before_ns = summary["machine_proc_stat_before"]["captured_monotonic_ns"]
        proc_after_ns = summary["machine_proc_stat_after"]["captured_monotonic_ns"]
        if proc_before_ns > start_ns or proc_after_ns < previous_drain_ns:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} /proc/stat samples do not bracket launch through drain"
            )
        if submission_ns > previous_drain_ns:
            raise AnalysisInvalid(
                f"burst {spec.burst_id} schedule submission follows its final drain"
            )
        previous_machine_after = summary["machine_proc_stat_after"]["cpu"]
        previous_cgroup_after = summary["cgroup_cpu_stat_after"]
    return {
        spec.burst_id: [
            rows_by_key[(spec.burst_id, planned.request_index)]
            for planned in spec.requests
        ]
        for spec in expected_bursts
    }


def evaluate_cell_burst(cell: dict, criteria: dict) -> dict:
    target = _finite_number(cell.get("target_rps"), "target_rps", minimum=0)
    if target == 0:
        raise AnalysisInvalid("target_rps must be positive")
    planned = cell.get("planned")
    if not isinstance(planned, int) or isinstance(planned, bool) or planned <= 0:
        raise AnalysisInvalid("planned request count must be a positive integer")
    count_fields = (
        "launched", "launched_by_score_end", "artifact_completed",
        "artifact_completed_by_score_end", "drained", "cleanup_confirmed", "ok", "failed",
    )
    for field in count_fields:
        value = cell.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or not 0 <= value <= planned:
            raise AnalysisInvalid(f"{field} is not a valid request count")
    if cell["ok"] + cell["failed"] != planned:
        raise AnalysisInvalid("successful and failed request counts do not reconcile")
    offered = _finite_number(cell.get("offered_rps"), "offered_rps", minimum=0)
    departure = _finite_number(cell.get("departure_rps"), "departure_rps", minimum=0)
    ratio = _finite_number(cell.get("departure_ratio"), "departure_ratio", minimum=0, maximum=1)
    score_seconds = planned / target
    if not math.isclose(offered, cell["launched_by_score_end"] / score_seconds):
        raise AnalysisInvalid("offered rate does not reconcile with launch count")
    if not math.isclose(departure, cell["artifact_completed_by_score_end"] / score_seconds):
        raise AnalysisInvalid("departure rate does not reconcile with completion count")
    if not math.isclose(ratio, cell["artifact_completed_by_score_end"] / planned):
        raise AnalysisInvalid("departure ratio does not reconcile with completion count")
    backlog = cell.get("score_end_backlog")
    if not isinstance(backlog, int) or isinstance(backlog, bool) or not 0 <= backlog <= planned:
        raise AnalysisInvalid("score-end backlog is not a valid request count")
    launch_distribution = cell.get("launch_lag_ms")
    if not isinstance(launch_distribution, dict):
        raise AnalysisInvalid("launch lag distribution is missing")
    p95_lag = _finite_number(
        launch_distribution.get("p95"), "p95 launch lag", minimum=0
    )
    offered_error = abs(offered - target) * 100.0 / target
    gates = {
        "offered": (
            cell.get("launched") == planned
            and offered_error <= criteria["max_offered_rps_error_pct"]
        ),
        "departure": (
            ratio >= criteria["min_departure_ratio"]
        ),
        "backlog": (
            backlog <= criteria["max_score_end_backlog"]
        ),
        "lag": (
            p95_lag <= criteria["max_p95_launch_lag_ms"]
        ),
        "zero_failure": (
            cell.get("failed") == 0
            and cell.get("ok") == planned
            and cell.get("artifact_completed") == planned
            and cell.get("drained") == planned
            and cell.get("cleanup_confirmed") == planned
        ),
    }
    return {
        "passed": all(gates.values()),
        "gates": gates,
        "offered_rps_error_pct": offered_error,
    }


def _control_gate(
    records: list[dict], schedule: dict, status: dict,
    measurement_start_ns: int, measurement_end_ns: int,
) -> dict:
    if len(records) < 4:
        raise AnalysisInvalid("host drift control has fewer than 4 scored observations")
    control_schedule = schedule["control"]
    interval = control_schedule["interval_ns"]
    expected_origin = status.get("control", {}).get("origin_monotonic_ns")
    if not isinstance(expected_origin, int):
        raise AnalysisInvalid("status has no host-control origin")
    control_status = status["control"]
    stop_ns = control_status.get("stop_requested_monotonic_ns")
    if (
        control_status.get("started") is not True
        or control_status.get("requests") != len(records)
        or control_status.get("interval_ns") != interval
        or control_status.get("phase_offset_ns") != control_schedule["phase_offset_ns"]
        or not isinstance(stop_ns, int)
        or stop_ns < expected_origin
        or expected_origin > measurement_start_ns
    ):
        raise AnalysisInvalid("status does not reconcile with the host-control arm")
    if measurement_end_ns > stop_ns:
        raise AnalysisInvalid("host-control arm stopped before the request workload drained")
    first_deadline = expected_origin + control_schedule["phase_offset_ns"]
    expected_count = max(0, (stop_ns - 1 - first_deadline) // interval + 1)
    if len(records) != expected_count:
        raise AnalysisInvalid(
            f"host-control arm recorded {len(records)} requests but {expected_count} "
            "ten-second deadlines elapsed"
        )
    for index, row in enumerate(records):
        if row.get("control_index") != index:
            raise AnalysisInvalid("host-control indices are not contiguous")
        expected = expected_origin + control_schedule["phase_offset_ns"] + index * interval
        if row.get("scheduled_ns") != expected:
            raise AnalysisInvalid(f"host-control row {index} diverges from its seeded schedule")
        if row.get("schema") != reqscale.RECORD_SCHEMA or row.get("kind") != "host-control":
            raise AnalysisInvalid(f"host-control row {index} has an invalid schema or kind")
        if row.get("ok") is not True:
            raise AnalysisInvalid(f"host-control row {index} failed")
        actual = row.get("actual_launch_ns")
        artifact = row.get("artifact_ns")
        if (
            not isinstance(actual, int) or isinstance(actual, bool)
            or not isinstance(artifact, int) or isinstance(artifact, bool)
            or not expected <= actual <= artifact < expected + interval
        ):
            raise AnalysisInvalid(
                f"host-control row {index} missed or overran its ten-second interval"
            )
        latency = _finite_number(row.get("latency_ms"), "host-control latency", minimum=0)
        lag = _finite_number(row.get("launch_lag_ms"), "host-control launch lag", minimum=0)
        if not math.isclose(latency, (artifact - actual) / 1_000_000):
            raise AnalysisInvalid(f"host-control row {index} latency does not match timestamps")
        if not math.isclose(lag, (actual - expected) / 1_000_000):
            raise AnalysisInvalid(f"host-control row {index} launch lag does not match timestamps")
        result = row.get("result")
        if not isinstance(result, dict) or result.get("ok") is not True:
            raise AnalysisInvalid(f"host-control row {index} has no successful CDP result")
    midpoint = len(records) // 2
    first = statistics.median(float(row["latency_ms"]) for row in records[:midpoint])
    second = statistics.median(float(row["latency_ms"]) for row in records[midpoint:])
    if first <= 0:
        raise AnalysisInvalid("host-control first-half median is not positive")
    drift_pct = (second - first) * 100.0 / first
    limit = schedule["capacity_criteria"]["max_control_median_drift_pct"]
    return {
        "passed": abs(drift_pct) <= limit,
        "n": len(records),
        "first_half_median_ms": first,
        "second_half_median_ms": second,
        "drift_pct": drift_pct,
        "absolute_limit_pct": limit,
    }


def _validate_control_warmup(warmup: dict, schedule: dict, status: dict) -> None:
    if (
        warmup.get("schema") != reqscale.RECORD_SCHEMA
        or warmup.get("kind") != "host-control-warmup"
        or warmup.get("included_in_analysis") is not False
        or not isinstance(warmup.get("result"), dict)
        or warmup["result"].get("ok") is not True
    ):
        raise AnalysisInvalid("host-control warmup artifact is invalid")
    started = warmup.get("started_monotonic_ns")
    artifact = warmup.get("artifact_monotonic_ns")
    origin = status.get("control", {}).get("origin_monotonic_ns")
    if (
        not isinstance(started, int) or not isinstance(artifact, int)
        or not isinstance(origin, int) or not started <= artifact <= origin
    ):
        raise AnalysisInvalid("host-control warmup is not ordered before scored controls")
    latency = _finite_number(warmup.get("latency_ms"), "host-control warmup latency", minimum=0)
    if not math.isclose(latency, (artifact - started) / 1_000_000):
        raise AnalysisInvalid("host-control warmup latency does not match timestamps")
    if schedule.get("control", {}).get("warmup_requests") != 1:
        raise AnalysisInvalid("schedule does not declare exactly one control warmup")


def _validate_raw_artifact(
    value: dict, name: str, expected_path: str | None = None,
) -> str:
    if not isinstance(value, dict):
        raise AnalysisInvalid(f"{name} is not an artifact object")
    raw = value.get("raw")
    digest = value.get("raw_sha256")
    if not isinstance(value.get("path"), str) or not isinstance(raw, str) or not raw:
        raise AnalysisInvalid(f"{name} lacks its raw source")
    if expected_path is not None and value["path"] != expected_path:
        raise AnalysisInvalid(
            f"{name} came from {value['path']!r}, expected {expected_path!r}"
        )
    if digest != hashlib.sha256(raw.encode()).hexdigest():
        raise AnalysisInvalid(f"{name} raw digest does not match")
    return raw


def _proc_cpu_from_raw(raw: str, name: str) -> dict[str, int]:
    lines = raw.splitlines()
    fields = lines[0].split() if lines else []
    if not fields or fields[0] != "cpu" or len(fields) < 5:
        raise AnalysisInvalid(f"{name} has no aggregate cpu row")
    try:
        values = [int(value) for value in fields[1:]]
    except ValueError as error:
        raise AnalysisInvalid(f"{name} has a malformed aggregate cpu row") from error
    if any(value < 0 for value in values):
        raise AnalysisInvalid(f"{name} has a negative aggregate cpu counter")
    names = list(reqscale.CPU_FIELDS)
    names.extend(f"field_{index}" for index in range(len(names), len(values)))
    return dict(zip(names, values))


def _validate_whole_proc_stat(raw: str, name: str) -> None:
    labels = [line.split(maxsplit=1)[0] for line in raw.splitlines() if line.strip()]
    required = {
        "cpu0", "intr", "ctxt", "btime", "processes", "procs_running",
        "procs_blocked", "softirq",
    }
    missing = required - set(labels)
    if missing:
        raise AnalysisInvalid(f"{name} is truncated; missing rows {sorted(missing)}")


def _validate_counter_snapshot(value, name: str) -> dict[str, int]:
    if (
        not isinstance(value, dict)
        or "usage_usec" not in value
        or any(
            not isinstance(counter, int)
            or isinstance(counter, bool)
            or counter < 0
            for counter in value.values()
        )
    ):
        raise AnalysisInvalid(f"{name} is not a complete nonnegative cpu.stat snapshot")
    return value


def _validate_samples(
    samples: list[dict], schedule: dict, status: dict,
    measurement_start_ns: int, measurement_end_ns: int,
) -> dict:
    if len(samples) < 3:
        raise AnalysisInvalid("continuous host accounting has fewer than 2 periodic samples and a terminal sample")
    interval = schedule["host_sample_interval_ns"]
    required_cgroups = {"run", "driver", "control", "file", "uffd"}
    sampler_status = status.get("sampler")
    if not isinstance(sampler_status, dict):
        raise AnalysisInvalid("status has no host sampler result")
    origin = sampler_status.get("origin_monotonic_ns")
    stop_ns = sampler_status.get("stop_requested_monotonic_ns")
    periodic = samples[:-1]
    terminal = samples[-1]
    if (
        sampler_status.get("started") is not True
        or sampler_status.get("samples") != len(samples)
        or sampler_status.get("periodic_samples") != len(periodic)
        or sampler_status.get("terminal_sample") is not True
        or sampler_status.get("interval_ns") != interval
        or not isinstance(origin, int)
        or not isinstance(stop_ns, int)
        or not origin <= measurement_start_ns <= measurement_end_ns <= stop_ns
    ):
        raise AnalysisInvalid("status does not reconcile with continuous host accounting")
    expected_periodic = max(0, (stop_ns - 1 - origin) // interval + 1)
    if len(periodic) != expected_periodic:
        raise AnalysisInvalid(
            f"host accounting recorded {len(periodic)} periodic samples but "
            f"{expected_periodic} five-second deadlines elapsed"
        )
    stable_paths = None
    stable_clk_tck = None
    previous_cpu = None
    previous_capture = None
    scheduled_bursts = {
        raw.get("burst_id") for raw in schedule.get("bursts", [])
        if isinstance(raw, dict) and isinstance(raw.get("burst_id"), str)
    }
    for index, sample in enumerate(samples):
        if sample.get("sample_index") != index:
            raise AnalysisInvalid("host sample indices are not contiguous")
        if sample.get("schema") != "fcvm.chromium.reqscale.host-sample.v1":
            raise AnalysisInvalid(f"host sample {index} has an unsupported schema")
        captured = sample.get("captured_monotonic_ns")
        completed = sample.get("completed_monotonic_ns")
        captured_wall = sample.get("captured_wall_ns")
        completed_wall = sample.get("completed_wall_ns")
        if (
            not isinstance(captured, int) or isinstance(captured, bool) or captured <= 0
            or not isinstance(completed, int) or isinstance(completed, bool)
            or completed < captured
            or not isinstance(captured_wall, int) or isinstance(captured_wall, bool)
            or captured_wall <= 0
            or not isinstance(completed_wall, int) or isinstance(completed_wall, bool)
            or completed_wall < captured_wall
        ):
            raise AnalysisInvalid(f"host sample {index} has invalid capture boundaries")
        if previous_capture is not None and captured < previous_capture:
            raise AnalysisInvalid("host sample capture times moved backwards")
        previous_capture = captured
        if index < len(periodic):
            expected_deadline = origin + index * interval
            if sample.get("scheduled_monotonic_ns") != expected_deadline:
                raise AnalysisInvalid("host samples do not use exact 5-second deadlines")
            if not expected_deadline <= captured <= completed < expected_deadline + interval:
                raise AnalysisInvalid(f"host sample {index} missed its five-second interval")
            lag = _finite_number(sample.get("launch_lag_ms"), "host sample launch lag", minimum=0)
            if not math.isclose(lag, (captured - expected_deadline) / 1_000_000):
                raise AnalysisInvalid(f"host sample {index} launch lag does not match timestamps")
            if sample.get("terminal") is True:
                raise AnalysisInvalid(f"periodic host sample {index} is marked terminal")
        else:
            if (
                sample.get("terminal") is not True
                or sample.get("scheduled_monotonic_ns") is not None
                or sample.get("launch_lag_ms") is not None
                or captured < stop_ns
                or completed >= stop_ns + interval
                or captured < measurement_end_ns
            ):
                raise AnalysisInvalid("terminal host sample does not bracket the workload")
        phase = sample.get("phase")
        if not isinstance(phase, dict) or set(phase) != {"name", "burst_id"}:
            raise AnalysisInvalid(f"host sample {index} has no exact run phase")
        if phase["name"] == "burst":
            if phase["burst_id"] not in scheduled_bursts:
                raise AnalysisInvalid(f"host sample {index} names an unknown burst phase")
        elif phase["name"] not in ("setup", "teardown") or phase["burst_id"] is not None:
            raise AnalysisInvalid(f"host sample {index} has an invalid run phase")
        if set(sample.get("cgroups", {})) != required_cgroups:
            raise AnalysisInvalid(f"host sample {index} lacks split cgroup accounting")
        proc_value = sample.get("proc_stat")
        raw_proc = _validate_raw_artifact(
            proc_value, f"host sample {index} /proc/stat", "/proc/stat"
        )
        _validate_whole_proc_stat(raw_proc, f"host sample {index} /proc/stat")
        if proc_value.get("cpu") != _proc_cpu_from_raw(raw_proc, f"host sample {index} /proc/stat"):
            raise AnalysisInvalid(f"host sample {index} parsed /proc/stat differs from raw input")
        if (
            not isinstance(proc_value.get("clk_tck"), int)
            or isinstance(proc_value.get("clk_tck"), bool)
            or proc_value["clk_tck"] <= 0
        ):
            raise AnalysisInvalid(f"host sample {index} /proc/stat lacks clock frequency")
        proc_captured = proc_value.get("captured_monotonic_ns")
        proc_captured_wall = proc_value.get("captured_wall_ns")
        if (
            not isinstance(proc_captured, int)
            or isinstance(proc_captured, bool)
            or not captured <= proc_captured <= completed
            or not isinstance(proc_captured_wall, int)
            or isinstance(proc_captured_wall, bool)
            or not captured_wall <= proc_captured_wall <= completed_wall
        ):
            raise AnalysisInvalid(
                f"host sample {index} /proc/stat capture escapes its sample boundary"
            )
        if stable_clk_tck is None:
            stable_clk_tck = proc_value["clk_tck"]
        elif proc_value["clk_tck"] != stable_clk_tck:
            raise AnalysisInvalid("host /proc/stat clock frequency changed during measurement")
        loadavg = sample.get("loadavg")
        raw_loadavg = _validate_raw_artifact(
            loadavg, f"host sample {index} loadavg", "/proc/loadavg"
        )
        try:
            parsed_loadavg = reqscale.parse_loadavg(raw_loadavg, "/proc/loadavg")
        except reqscale.MeasurementInvalid as error:
            raise AnalysisInvalid(f"host sample {index} loadavg is invalid: {error}") from error
        if loadavg.get("parsed") != parsed_loadavg:
            raise AnalysisInvalid(f"host sample {index} parsed loadavg differs from raw input")
        pressure = sample.get("pressure", {})
        if set(pressure) != {"cpu", "memory", "io"}:
            raise AnalysisInvalid(f"host sample {index} lacks complete PSI")
        for resource, value in pressure.items():
            expected_path = f"/proc/pressure/{resource}"
            raw = _validate_raw_artifact(
                value, f"host sample {index} {resource} PSI", expected_path
            )
            try:
                parsed_psi = reqscale.parse_psi(raw, expected_path)
            except reqscale.MeasurementInvalid as error:
                raise AnalysisInvalid(
                    f"host sample {index} {resource} PSI is invalid: {error}"
                ) from error
            if value.get("parsed") != parsed_psi:
                raise AnalysisInvalid(f"host sample {index} parsed {resource} PSI differs from raw input")
        meminfo = sample.get("meminfo")
        raw_meminfo = _validate_raw_artifact(
            meminfo, f"host sample {index} meminfo", "/proc/meminfo"
        )
        try:
            parsed_meminfo = reqscale.parse_meminfo(raw_meminfo, "/proc/meminfo")
        except reqscale.MeasurementInvalid as error:
            raise AnalysisInvalid(f"host sample {index} meminfo is invalid: {error}") from error
        if meminfo.get("parsed") != parsed_meminfo:
            raise AnalysisInvalid(f"host sample {index} parsed meminfo differs from raw input")
        paths = {name: sample["cgroups"][name].get("path") for name in required_cgroups}
        if any(not isinstance(path, str) or not path.startswith("/") for path in paths.values()):
            raise AnalysisInvalid(f"host sample {index} has invalid cgroup paths")
        if len(set(paths.values())) != len(paths):
            raise AnalysisInvalid(f"host sample {index} cgroup paths are not distinct")
        if any(
            not paths[name].startswith(paths["run"].rstrip("/") + "/")
            for name in required_cgroups - {"run"}
        ):
            raise AnalysisInvalid(f"host sample {index} leaf cgroup is outside the run cgroup")
        if stable_paths is None:
            stable_paths = paths
        elif paths != stable_paths:
            raise AnalysisInvalid("host cgroup paths changed during measurement")
        seen_pids = {}
        current_cpu = {}
        for name in required_cgroups:
            row = sample["cgroups"][name]
            pids = row.get("live_pids")
            if (
                not isinstance(pids, list)
                or any(not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0 for pid in pids)
                or pids != sorted(set(pids))
            ):
                raise AnalysisInvalid(f"host sample {index} {name} has invalid PID membership")
            if name != "run":
                for pid in pids:
                    if pid in seen_pids:
                        raise AnalysisInvalid(
                            f"host sample {index} pid {pid} appears in {seen_pids[pid]} and {name}"
                        )
                    seen_pids[pid] = name
            cpu_stat = _validate_counter_snapshot(
                row.get("cpu_stat"), f"host sample {index} {name} cpu.stat"
            )
            current_cpu[name] = cpu_stat
        if sample["cgroups"]["run"]["live_pids"] != []:
            raise AnalysisInvalid(f"host sample {index} run parent contains direct processes")
        harness_pid = status["final_cgroups"]["driver"]["live_pids"][0]
        if harness_pid not in sample["cgroups"]["driver"]["live_pids"]:
            raise AnalysisInvalid(f"host sample {index} driver omits the harness")
        if previous_cpu is not None:
            try:
                for name in required_cgroups:
                    reqscale.counter_delta(previous_cpu[name], current_cpu[name])
            except reqscale.MeasurementInvalid as error:
                raise AnalysisInvalid(f"host cgroup counters are not monotonic: {error}") from error
        previous_cpu = current_cpu
    final = status["final_cgroups"]
    expected_paths = {name: final[name]["path"] for name in required_cgroups}
    if stable_paths != expected_paths:
        raise AnalysisInvalid("host sample cgroup paths differ from the terminal cgroup audit")
    if {
        name: terminal["cgroups"][name]["live_pids"] for name in required_cgroups
    } != {
        name: final[name]["live_pids"] for name in required_cgroups
    }:
        raise AnalysisInvalid("terminal host sample membership differs from final cgroup audit")
    try:
        reqscale.counter_delta(
            status["run_before"]["machine_proc_stat"]["cpu"],
            samples[0]["proc_stat"]["cpu"],
        )
        reqscale.counter_delta(
            terminal["proc_stat"]["cpu"],
            status["run_after"]["machine_proc_stat"]["cpu"],
        )
        for name in required_cgroups:
            reqscale.counter_delta(
                terminal["cgroups"][name]["cpu_stat"], final[name]["cpu_stat"]
            )
            reqscale.counter_delta(
                status["run_before"]["cgroup_cpu_stat"][name],
                samples[0]["cgroups"][name]["cpu_stat"],
            )
            reqscale.counter_delta(
                terminal["cgroups"][name]["cpu_stat"],
                status["run_after"]["cgroup_cpu_stat"][name],
            )
    except reqscale.MeasurementInvalid as error:
        raise AnalysisInvalid(
            f"host sample counters escape the run-level accounting window: {error}"
        ) from error
    return {
        "passed": True,
        "samples": len(samples),
        "periodic_samples": len(periodic),
        "terminal_sample": True,
        "interval_ns": interval,
    }


def _require_hex(value, length: int, name: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != length
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise AnalysisInvalid(f"{name} is not {length}-character lowercase hex")
    return value


def _validate_snapshot_provenance(snapshot: dict) -> tuple[str, str]:
    required = {
        "tag", "generation_id", "created_at", "vm_id", "config_sha256",
        "shape", "files",
    }
    if not isinstance(snapshot, dict) or set(snapshot) != required:
        raise AnalysisInvalid("provenance snapshot identity is incomplete")
    tag = snapshot.get("tag")
    try:
        reqscale._validate_snapshot_tag(tag)
    except (TypeError, ValueError) as error:
        raise AnalysisInvalid(f"provenance has an invalid snapshot tag: {error}") from error
    for field in ("created_at", "vm_id"):
        if not isinstance(snapshot.get(field), str) or not snapshot[field]:
            raise AnalysisInvalid(f"provenance snapshot lacks {field}")
    generation_id = _canonical_generation(snapshot.get("generation_id"))
    config_sha256 = _require_hex(
        snapshot.get("config_sha256"), 64, "snapshot config digest"
    )
    shape = snapshot.get("shape")
    shape_fields = {"image", "vcpu", "memory_mib", "network_mode", "port_mappings"}
    if not isinstance(shape, dict) or set(shape) != shape_fields:
        raise AnalysisInvalid("provenance snapshot shape is incomplete")
    if (
        not isinstance(shape["image"], str) or not shape["image"]
        or not isinstance(shape["network_mode"], str) or not shape["network_mode"]
        or not isinstance(shape["vcpu"], int) or isinstance(shape["vcpu"], bool)
        or shape["vcpu"] <= 0
        or not isinstance(shape["memory_mib"], int)
        or isinstance(shape["memory_mib"], bool)
        or shape["memory_mib"] <= 0
        or not isinstance(shape["port_mappings"], list)
        or any(not isinstance(value, str) or not value for value in shape["port_mappings"])
    ):
        raise AnalysisInvalid("provenance snapshot shape contains invalid values")
    files = snapshot.get("files")
    expected_files = {"memory_path", "vmstate_path", "disk_path", "config"}
    if not isinstance(files, dict) or set(files) != expected_files:
        raise AnalysisInvalid("provenance snapshot artifact identity is incomplete")
    for name, value in files.items():
        if not isinstance(value, dict) or set(value) != {"path", "size", "mtime_ns", "inode"}:
            raise AnalysisInvalid(f"provenance snapshot artifact {name} is incomplete")
        path = value.get("path")
        if (
            not isinstance(path, str) or not path
            or os.path.isabs(path)
            or os.path.normpath(path).startswith("..")
        ):
            raise AnalysisInvalid(f"provenance snapshot artifact {name} has an unsafe path")
        for field in ("size", "mtime_ns", "inode"):
            number = value.get(field)
            if (
                not isinstance(number, int)
                or isinstance(number, bool)
                or number < 0
            ):
                raise AnalysisInvalid(
                    f"provenance snapshot artifact {name} has invalid {field}"
                )
    return generation_id, config_sha256


def _validate_provenance(provenance: dict, schedule: dict) -> tuple[str, str]:
    required = {
        "schema", "run_id", "created_at", "argv", "schedule_sha256",
        "source_revision", "source_dirty", "source_status_sha256",
        "harness_sha256", "fcvm_path", "fcvm_sha256", "fcvm_version",
        "host_control", "snapshot", "snapshot_generation_lease", "host",
        "fault_trace",
    }
    if not isinstance(provenance, dict) or set(provenance) != required:
        raise AnalysisInvalid("provenance fields are incomplete or unknown")
    if provenance.get("schema") != "fcvm.chromium.reqscale.provenance.v1":
        raise AnalysisInvalid("unsupported provenance schema")
    if provenance.get("run_id") != schedule["run_id"]:
        raise AnalysisInvalid("run identity differs between schedule and provenance")
    if provenance.get("source_dirty") is not False:
        raise AnalysisInvalid("measurement source tree was dirty")
    _require_hex(provenance.get("source_revision"), 40, "source revision")
    for name in ("source_status_sha256", "harness_sha256", "fcvm_sha256"):
        _require_hex(provenance.get(name), 64, name)
    if not isinstance(provenance.get("created_at"), str) or not provenance["created_at"]:
        raise AnalysisInvalid("provenance has no creation time")
    if (
        not isinstance(provenance.get("argv"), list)
        or not provenance["argv"]
        or any(not isinstance(value, str) for value in provenance["argv"])
    ):
        raise AnalysisInvalid("provenance argv is incomplete")
    if (
        not isinstance(provenance.get("fcvm_path"), str)
        or not os.path.isabs(provenance["fcvm_path"])
        or not isinstance(provenance.get("fcvm_version"), str)
        or not provenance["fcvm_version"]
    ):
        raise AnalysisInvalid("fcvm executable provenance is incomplete")
    if provenance.get("source_status_sha256") != hashlib.sha256(b"").hexdigest():
        raise AnalysisInvalid("clean source status digest is not the digest of empty output")
    if provenance.get("schedule_sha256") != reqscale.schedule_sha256(schedule):
        raise AnalysisInvalid("schedule hash differs from provenance")
    snapshot = provenance.get("snapshot")
    generation_id, config_sha256 = _validate_snapshot_provenance(snapshot)
    lease = provenance.get("snapshot_generation_lease")
    if (
        not isinstance(lease, dict)
        or lease.get("mode") != "shared"
        or lease.get("held_from_identity_read_through_terminal_verification") is not True
        or not isinstance(lease.get("path"), str)
        or not os.path.isabs(lease["path"])
        or os.path.basename(lease["path"]) != f"{snapshot['tag']}.lock"
    ):
        raise AnalysisInvalid("snapshot generation lease provenance is incomplete")
    host = provenance.get("host")
    if not isinstance(host, dict) or set(host) != {
        "hostname", "kernel", "machine", "python", "cpu_count", "quiet_gate",
    }:
        raise AnalysisInvalid("host provenance is incomplete")
    for field in ("hostname", "kernel", "machine", "python"):
        if not isinstance(host.get(field), str) or not host[field]:
            raise AnalysisInvalid(f"host provenance lacks {field}")
    if (
        not isinstance(host.get("cpu_count"), int)
        or isinstance(host["cpu_count"], bool)
        or host["cpu_count"] <= 0
    ):
        raise AnalysisInvalid("host provenance has an invalid CPU count")
    quiet = host.get("quiet_gate")
    if (
        not isinstance(quiet, dict)
        or quiet.get("vm_process_count") != 0
        or quiet.get("vm_processes") != []
        or _finite_number(quiet.get("loadavg1"), "quiet-host load", minimum=0)
        > _finite_number(quiet.get("loadavg1_limit"), "quiet-host load limit", minimum=0)
    ):
        raise AnalysisInvalid("quiet-host provenance did not pass")
    host_control = provenance.get("host_control")
    if (
        not isinstance(host_control, dict)
        or set(host_control) != {
            "chromium_path", "chromium_sha256", "chromium_version", "url",
            "interval_seconds", "timeout_seconds",
        }
        or host_control.get("interval_seconds") != reqscale.CONTROL_INTERVAL_SECONDS
        or not isinstance(host_control.get("chromium_path"), str)
        or not os.path.isabs(host_control["chromium_path"])
        or not isinstance(host_control.get("chromium_version"), str)
        or not host_control["chromium_version"]
        or not isinstance(host_control.get("url"), str)
        or not host_control["url"]
        or not 0 < _finite_number(
            host_control.get("timeout_seconds"), "host-control timeout", minimum=0,
        ) < reqscale.CONTROL_INTERVAL_SECONDS
    ):
        raise AnalysisInvalid("host-control provenance is incomplete")
    _require_hex(host_control.get("chromium_sha256"), 64, "host Chromium digest")
    fault_trace = provenance.get("fault_trace")
    if (
        not isinstance(fault_trace, dict)
        or set(fault_trace) != {
            "enabled", "bpftrace_version", "max_median_delta_pct", "scope",
        }
        or not isinstance(fault_trace.get("enabled"), bool)
        or not isinstance(fault_trace.get("scope"), str)
        or "not guest-RAM-filtered" not in fault_trace["scope"]
    ):
        raise AnalysisInvalid("fault-trace provenance is incomplete")
    if fault_trace["enabled"]:
        if (
            not isinstance(fault_trace.get("bpftrace_version"), str)
            or not fault_trace["bpftrace_version"]
        ):
            raise AnalysisInvalid("enabled fault tracing lacks a bpftrace version")
        _finite_number(
            fault_trace.get("max_median_delta_pct"),
            "fault-trace perturbation limit",
            minimum=0,
        )
    elif (
        fault_trace.get("bpftrace_version") is not None
        or fault_trace.get("max_median_delta_pct") is not None
    ):
        raise AnalysisInvalid("disabled fault tracing contains active trace settings")
    return generation_id, config_sha256


def _validate_status(status: dict, schedule: dict, provenance: dict) -> None:
    required_status = {
        "schema", "run_id", "valid", "bursts_completed", "bursts_planned",
        "control", "sampler", "snapshot_identity_after", "run_before",
        "run_after", "final_cgroups", "error", "error_details", "errors",
    }
    if not isinstance(status, dict) or set(status) != required_status:
        raise AnalysisInvalid("terminal status fields are incomplete or unknown")
    if status.get("schema") != "fcvm.chromium.reqscale.status.v2":
        raise AnalysisInvalid("unsupported terminal status schema")
    if status.get("valid") is not True or status.get("errors") != []:
        raise AnalysisInvalid("run status is not valid and error-free")
    if status.get("error") is not None or status.get("error_details") is not None:
        raise AnalysisInvalid("valid run status contains an error")
    if status.get("run_id") != schedule["run_id"]:
        raise AnalysisInvalid("run identity differs between schedule and status")
    if (
        status.get("bursts_completed") != len(schedule["bursts"])
        or status.get("bursts_planned") != len(schedule["bursts"])
    ):
        raise AnalysisInvalid("terminal status does not cover every scheduled burst")
    if status.get("snapshot_identity_after") != provenance.get("snapshot"):
        raise AnalysisInvalid("terminal snapshot identity differs from starting provenance")
    final = status.get("final_cgroups")
    required = {"run", "driver", "control", "file", "uffd"}
    if not isinstance(final, dict) or set(final) != required:
        raise AnalysisInvalid("terminal status lacks all split cgroups")
    run_path = final.get("run", {}).get("path")
    if (
        not isinstance(run_path, str)
        or not run_path.startswith("/")
        or not run_path.endswith(f"/fcvm-reqscale-{schedule['run_id']}")
    ):
        raise AnalysisInvalid("terminal run cgroup has an invalid path")
    leaf_identities = {}
    harness_identity = None
    for name in required:
        row = final[name]
        if not isinstance(row, dict) or set(row) != {"path", "observed", "live_pids", "cpu_stat"}:
            raise AnalysisInvalid(f"terminal {name} cgroup record is incomplete")
        expected_path = run_path if name == "run" else f"{run_path}/{name}"
        if row["path"] != expected_path:
            raise AnalysisInvalid(f"terminal {name} cgroup path differs")
        _validate_counter_snapshot(row.get("cpu_stat"), f"terminal {name} cpu.stat")
        live = row.get("live_pids")
        if (
            not isinstance(live, list)
            or any(
                not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0
                for pid in live
            )
            or live != sorted(set(live))
        ):
            raise AnalysisInvalid(f"terminal {name} cgroup has invalid membership")
        observed = row.get("observed")
        if not isinstance(observed, list):
            raise AnalysisInvalid(f"terminal {name} cgroup has no process audit")
        identities = []
        for item in observed:
            if (
                not isinstance(item, dict)
                or set(item) != {"pid", "pid_start_time_ticks", "role", "comm"}
                or not isinstance(item["pid"], int) or isinstance(item["pid"], bool)
                or item["pid"] <= 0
                or not isinstance(item["pid_start_time_ticks"], int)
                or isinstance(item["pid_start_time_ticks"], bool)
                or item["pid_start_time_ticks"] <= 0
                or not isinstance(item["role"], str) or not item["role"]
                or not isinstance(item["comm"], str) or not item["comm"]
            ):
                raise AnalysisInvalid(f"terminal {name} cgroup has an invalid process audit")
            identity = (item["pid"], item["pid_start_time_ticks"])
            identities.append(identity)
            if name != "run":
                prior = leaf_identities.setdefault(identity, name)
                if prior != name:
                    raise AnalysisInvalid(
                        f"process identity {identity} appears in both {prior} and {name}"
                    )
        if identities != sorted(set(identities)):
            raise AnalysisInvalid(f"terminal {name} cgroup process audit is not unique and sorted")
        if name == "driver":
            if len(live) != 1:
                raise AnalysisInvalid("terminal driver cgroup does not contain exactly the harness")
            harness_pid = live[0]
            harness_rows = [
                item for item in observed
                if item["pid"] == harness_pid and item["role"] == "reqscale"
            ]
            if len(harness_rows) != 1:
                raise AnalysisInvalid("terminal driver cgroup does not identify its harness")
            harness_identity = (
                harness_rows[0]["pid"], harness_rows[0]["pid_start_time_ticks"]
            )
        elif live != []:
            raise AnalysisInvalid(f"terminal {name} cgroup is not empty")
    if not any(
        item["pid"] == harness_identity[0]
        and item["pid_start_time_ticks"] == harness_identity[1]
        and item["role"] == "reqscale"
        for item in final["run"]["observed"]
    ):
        raise AnalysisInvalid("terminal run cgroup does not identify the harness")


def _validate_machine_accounting_artifact(value, name: str) -> dict[str, int]:
    raw = _validate_raw_artifact(value, name, "/proc/stat")
    _validate_whole_proc_stat(raw, name)
    parsed = _proc_cpu_from_raw(raw, name)
    if value.get("cpu") != parsed:
        raise AnalysisInvalid(f"{name} parsed counters differ from raw /proc/stat")
    for field in ("captured_wall_ns", "captured_monotonic_ns", "clk_tck"):
        number = value.get(field)
        if (
            not isinstance(number, int)
            or isinstance(number, bool)
            or number <= 0
        ):
            raise AnalysisInvalid(f"{name} lacks {field}")
    return parsed


def _validate_run_accounting(
    status: dict, summaries: list[dict],
    measurement_start_ns: int, measurement_end_ns: int,
) -> None:
    required = {"run", "driver", "control", "file", "uffd"}
    final = status["final_cgroups"]
    sides = {}
    for side in ("before", "after"):
        record = status.get(f"run_{side}")
        if (
            not isinstance(record, dict)
            or set(record) != {"machine_proc_stat", "cgroup_cpu_stat", "cgroups"}
        ):
            raise AnalysisInvalid(f"run-level {side} accounting is incomplete")
        machine = _validate_machine_accounting_artifact(
            record["machine_proc_stat"], f"run-level {side} /proc/stat"
        )
        cpu = record["cgroup_cpu_stat"]
        cgroups = record["cgroups"]
        if (
            not isinstance(cpu, dict) or set(cpu) != required
            or not isinstance(cgroups, dict) or set(cgroups) != required
        ):
            raise AnalysisInvalid(f"run-level {side} accounting lacks split cgroups")
        for name in required:
            cpu_value = _validate_counter_snapshot(
                cpu[name], f"run-level {side} {name} cpu.stat"
            )
            row = cgroups[name]
            if (
                not isinstance(row, dict)
                or set(row) != {"path", "observed", "live_pids", "cpu_stat"}
                or row["path"] != final[name]["path"]
                or row["live_pids"] != final[name]["live_pids"]
                or not isinstance(row["observed"], list)
            ):
                raise AnalysisInvalid(f"run-level {side} {name} cgroup audit differs")
            row_cpu = _validate_counter_snapshot(
                row["cpu_stat"], f"run-level {side} {name} cgroup audit cpu.stat"
            )
            try:
                reqscale.counter_delta(cpu_value, row_cpu)
                reqscale.counter_delta(row_cpu, final[name]["cpu_stat"])
            except reqscale.MeasurementInvalid as error:
                raise AnalysisInvalid(
                    f"run-level {side} {name} counters are out of order: {error}"
                ) from error
            final_observed = {
                (item["pid"], item["pid_start_time_ticks"], item["role"], item["comm"])
                for item in final[name]["observed"]
            }
            observed = []
            for item in row["observed"]:
                if not isinstance(item, dict) or set(item) != {
                    "pid", "pid_start_time_ticks", "role", "comm"
                }:
                    raise AnalysisInvalid(
                        f"run-level {side} {name} has an invalid process audit"
                    )
                identity = (
                    item["pid"], item["pid_start_time_ticks"],
                    item["role"], item["comm"],
                )
                if identity not in final_observed:
                    raise AnalysisInvalid(
                        f"run-level {side} {name} process audit is absent from final evidence"
                    )
                observed.append(identity)
            if observed != sorted(set(observed)):
                raise AnalysisInvalid(
                    f"run-level {side} {name} process audit is not unique and sorted"
                )
            if side == "after" and row["observed"] != final[name]["observed"]:
                raise AnalysisInvalid(
                    f"run-level after {name} process audit differs from final evidence"
                )
        sides[side] = {"machine": machine, "cpu": cpu, "record": record}
    if (
        sides["before"]["record"]["machine_proc_stat"]["captured_monotonic_ns"]
        > measurement_start_ns
        or sides["after"]["record"]["machine_proc_stat"]["captured_monotonic_ns"]
        < measurement_end_ns
    ):
        raise AnalysisInvalid("run-level accounting does not bracket the measured workload")
    try:
        reqscale.counter_delta(sides["before"]["machine"], sides["after"]["machine"])
        for name in required:
            reqscale.counter_delta(
                sides["before"]["cpu"][name], sides["after"]["cpu"][name]
            )
    except reqscale.MeasurementInvalid as error:
        raise AnalysisInvalid(f"run-level accounting moved backwards: {error}") from error
    if (
        sides["before"]["record"]["machine_proc_stat"]["clk_tck"]
        != sides["after"]["record"]["machine_proc_stat"]["clk_tck"]
    ):
        raise AnalysisInvalid("run-level /proc/stat clock frequency changed")
    try:
        reqscale.counter_delta(
            sides["before"]["machine"], summaries[0]["machine_proc_stat_before"]["cpu"]
        )
        reqscale.counter_delta(
            summaries[-1]["machine_proc_stat_after"]["cpu"], sides["after"]["machine"]
        )
        for name in required:
            reqscale.counter_delta(
                sides["before"]["cpu"][name],
                summaries[0]["cgroup_cpu_stat_before"][name],
            )
            reqscale.counter_delta(
                summaries[-1]["cgroup_cpu_stat_after"][name],
                sides["after"]["cpu"][name],
            )
    except reqscale.MeasurementInvalid as error:
        raise AnalysisInvalid(
            f"burst accounting escapes the run-level accounting window: {error}"
        ) from error


def _validate_uffd_serve(
    serve: dict, schedule: dict, provenance: dict, status: dict,
) -> None:
    required_fields = {
        "schema", "kind", "run_id", "pid", "pid_start_time_ticks",
        "state_path", "uffd_mode", "snapshot_tag", "snapshot_generation_id",
        "snapshot_config_sha256",
    }
    if not isinstance(serve, dict) or set(serve) != required_fields:
        raise AnalysisInvalid("UFFD serve record fields are incomplete or unknown")
    snapshot = provenance["snapshot"]
    expected = {
        "schema": reqscale.RECORD_SCHEMA,
        "kind": "uffd-serve",
        "run_id": schedule["run_id"],
        "snapshot_tag": snapshot["tag"],
        "snapshot_generation_id": snapshot["generation_id"],
        "snapshot_config_sha256": snapshot["config_sha256"],
    }
    mismatch = {
        name: {"expected": value, "actual": serve.get(name)}
        for name, value in expected.items() if serve.get(name) != value
    }
    if mismatch:
        raise AnalysisInvalid(f"UFFD serve is not bound to this snapshot generation: {mismatch}")
    if serve.get("uffd_mode") not in ("copy", "minor"):
        raise AnalysisInvalid("UFFD serve has an invalid memory mode")
    state_path = serve.get("state_path")
    if (
        not isinstance(state_path, str)
        or not state_path
        or os.path.isabs(state_path)
        or os.path.normpath(state_path).startswith("..")
    ):
        raise AnalysisInvalid("UFFD serve has an unsafe state path")
    pid = serve.get("pid")
    start = serve.get("pid_start_time_ticks")
    if (
        not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0
        or not isinstance(start, int) or isinstance(start, bool) or start <= 0
    ):
        raise AnalysisInvalid("UFFD serve has an invalid process identity")
    observed = status["final_cgroups"]["uffd"].get("observed", [])
    if not any(
        item.get("pid") == pid
        and item.get("pid_start_time_ticks") == start
        and item.get("role") == "uffd-serve"
        for item in observed if isinstance(item, dict)
    ):
        raise AnalysisInvalid("UFFD cgroup audit does not contain the recorded serve identity")


def _validate_fault_cgroup_membership(requests: list[dict], status: dict) -> None:
    final = status["final_cgroups"]
    observed = {
        backend: {
            (item.get("pid"), item.get("pid_start_time_ticks"), item.get("role"))
            for item in final[backend].get("observed", []) if isinstance(item, dict)
        }
        for backend in ("file", "uffd")
    }
    for row in requests:
        faults = row["firecracker_process_faults_ready_to_artifact"]
        identity = (faults["pid"], faults["pid_start_time_ticks"], "firecracker")
        if identity not in observed[row["backend"]]:
            raise AnalysisInvalid(
                f"request {row['request_id']} Firecracker identity is absent from its "
                f"{row['backend']} cgroup audit"
            )


def _trace_artifact_path(
    run_dir: str, record: dict, expected_relative: str, name: str,
) -> str:
    if not isinstance(record, dict) or set(record) != {"path", "sha256", "bytes"}:
        raise AnalysisInvalid(f"{name} trace artifact record is incomplete")
    if record.get("path") != expected_relative:
        raise AnalysisInvalid(
            f"{name} trace artifact path is {record.get('path')!r}, "
            f"expected {expected_relative!r}"
        )
    expected_digest = _require_hex(record.get("sha256"), 64, f"{name} trace digest")
    expected_bytes = record.get("bytes")
    if (
        not isinstance(expected_bytes, int)
        or isinstance(expected_bytes, bool)
        or expected_bytes < 0
    ):
        raise AnalysisInvalid(f"{name} trace artifact has an invalid byte count")
    root = os.path.realpath(run_dir)
    path = os.path.join(root, expected_relative)
    if os.path.realpath(path) != os.path.abspath(path):
        raise AnalysisInvalid(f"{name} trace artifact traverses a symbolic link")
    try:
        metadata = os.lstat(path)
    except OSError as error:
        raise AnalysisInvalid(f"cannot stat {name} trace artifact: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise AnalysisInvalid(f"{name} trace artifact is not a regular file")
    if metadata.st_size != expected_bytes:
        raise AnalysisInvalid(f"{name} trace artifact byte count differs")
    if reqscale.sha256_file(path) != expected_digest:
        raise AnalysisInvalid(f"{name} trace artifact digest differs")
    return path


def _validate_fault_trace_artifacts(
    run_dir: str,
    schedule: dict,
    summaries: list[dict],
    requests_by_burst: dict[str, list[dict]],
) -> None:
    specs = {
        spec.burst_id: spec
        for spec in (reqscale.BurstSpec.from_dict(raw) for raw in schedule["bursts"])
    }
    for summary in summaries:
        burst_id = summary["burst_id"]
        spec = specs[burst_id]
        trace_record = summary.get("fault_trace")
        if not spec.traced:
            if trace_record is not None:
                raise AnalysisInvalid(f"untraced burst {burst_id} names trace artifacts")
            continue
        if (
            not isinstance(trace_record, dict)
            or set(trace_record) != {"scope", "processes", "artifacts"}
            or not isinstance(trace_record.get("scope"), str)
            or "not guest-RAM-filtered" not in trace_record["scope"]
            or not isinstance(trace_record.get("processes"), int)
            or isinstance(trace_record.get("processes"), bool)
            or trace_record["processes"] < 0
        ):
            raise AnalysisInvalid(f"traced burst {burst_id} has incomplete trace provenance")
        artifacts = trace_record.get("artifacts")
        if not isinstance(artifacts, dict) or set(artifacts) != {"raw", "stderr", "program"}:
            raise AnalysisInvalid(f"traced burst {burst_id} lacks every trace artifact")
        prefix = os.path.join("fault-trace", burst_id)
        raw_path = _trace_artifact_path(
            run_dir, artifacts["raw"], f"{prefix}.bpftrace.jsonl", "raw bpftrace",
        )
        _trace_artifact_path(
            run_dir, artifacts["stderr"], f"{prefix}.bpftrace.stderr", "bpftrace stderr",
        )
        _trace_artifact_path(
            run_dir, artifacts["program"], f"{prefix}.faulttrace.bt", "bpftrace program",
        )
        try:
            with open(raw_path) as stream:
                trace = reqscale.parse_fault_trace(stream)
        except (OSError, UnicodeError, reqscale.MeasurementInvalid) as error:
            raise AnalysisInvalid(
                f"cannot reconstruct traced burst {burst_id}: {error}"
            ) from error
        if trace_record["processes"] != len(trace["processes"]):
            raise AnalysisInvalid(f"traced burst {burst_id} process count differs from raw trace")
        evidence_rows = [
            {
                "request_id": row["request_id"],
                "ok": row["ok"],
                "firecracker_process_faults_ready_to_artifact": row[
                    "firecracker_process_faults_ready_to_artifact"
                ],
            }
            for row in requests_by_burst[burst_id]
        ]
        try:
            reqscale.join_fault_trace(evidence_rows, trace)
        except reqscale.MeasurementInvalid as error:
            raise AnalysisInvalid(
                f"raw trace cannot be joined to burst {burst_id}: {error}"
            ) from error
        for actual, reconstructed in zip(requests_by_burst[burst_id], evidence_rows):
            if actual.get(
                "firecracker_process_handle_mm_fault_ready_to_artifact"
            ) != reconstructed.get(
                "firecracker_process_handle_mm_fault_ready_to_artifact"
            ):
                raise AnalysisInvalid(
                    f"request {actual['request_id']} fault timing differs from raw bpftrace"
                )


def analyze(run_dir: str) -> dict:
    schedule = _load_json(os.path.join(run_dir, "schedule.json"))
    provenance = _load_json(os.path.join(run_dir, "provenance.json"))
    status = _load_json(os.path.join(run_dir, "status.json"))
    summaries = _load_jsonl(os.path.join(run_dir, "bursts.jsonl"))
    requests = _load_jsonl(os.path.join(run_dir, "requests.jsonl"))
    controls = _load_jsonl(os.path.join(run_dir, "host-control.jsonl"))
    samples = _load_jsonl(os.path.join(run_dir, "host-samples.jsonl"))
    control_warmup = _load_json(os.path.join(run_dir, "host-control-warmup.json"))
    uffd_serve = _load_json(os.path.join(run_dir, "uffd-serve.json"))

    _validate_schedule(schedule)
    generation_id, config_sha256 = _validate_provenance(provenance, schedule)
    _validate_status(status, schedule, provenance)
    _validate_uffd_serve(uffd_serve, schedule, provenance, status)
    requests_by_burst = _validate_requests(
        schedule, summaries, requests, generation_id, config_sha256
    )
    _validate_fault_cgroup_membership(requests, status)
    _validate_fault_trace_artifacts(
        run_dir, schedule, summaries, requests_by_burst
    )
    run_id = schedule["run_id"]
    measurement_start_ns = min(summary["burst_start_ns"] for summary in summaries)
    measurement_end_ns = max(row["finished_ns"] for row in requests)
    _validate_run_accounting(
        status, summaries, measurement_start_ns, measurement_end_ns
    )
    sample_gate = _validate_samples(
        samples, schedule, status, measurement_start_ns, measurement_end_ns
    )
    _validate_control_warmup(control_warmup, schedule, status)
    control_gate = _control_gate(
        controls, schedule, status, measurement_start_ns, measurement_end_ns
    )
    if not control_gate["passed"]:
        raise AnalysisInvalid(
            f"host drift control changed {control_gate['drift_pct']:.3f}%, beyond "
            f"{control_gate['absolute_limit_pct']:.3f}%"
        )

    criteria = schedule.get("capacity_criteria")
    required_criteria = {
        "max_offered_rps_error_pct", "min_departure_ratio",
        "max_score_end_backlog", "max_p95_launch_lag_ms",
        "max_control_median_drift_pct", "require_zero_failures",
    }
    if not isinstance(criteria, dict) or set(criteria) != required_criteria:
        raise AnalysisInvalid("capacity criteria are missing or undeclared")
    if criteria["require_zero_failures"] is not True:
        raise AnalysisInvalid("zero failures must be a mandatory capacity gate")

    grouped = defaultdict(list)
    for summary in summaries:
        if summary.get("population") != "scored":
            continue
        for backend in ("file", "uffd"):
            grouped[(backend, summary["target_rps"])].append(summary)
    cells = []
    analysis_seed = schedule["seed"] ^ 0xA11A515
    for cell_index, declared in enumerate(schedule["cells"]):
        key = (declared["backend"], declared["target_rps"])
        bursts = sorted(grouped.get(key, []), key=lambda row: row["repeat"])
        if len(bursts) != declared["independent_bursts"] or len(bursts) < 5:
            raise AnalysisInvalid(f"cell {declared['cell_id']} lacks independent bursts")
        cell_rows = []
        for burst in bursts:
            metrics = burst.get("backends", {}).get(declared["backend"])
            if not isinstance(metrics, dict):
                raise AnalysisInvalid(
                    f"burst {burst.get('burst_id')} lacks {declared['backend']} metrics"
                )
            verdict = evaluate_cell_burst(
                {**metrics, "target_rps": declared["target_rps"]}, criteria
            )
            cell_rows.append({
                "burst_id": burst["burst_id"],
                "repeat": burst["repeat"],
                **verdict,
            })
        metric_seed = analysis_seed + cell_index * 100
        departure = [burst["backends"][declared["backend"]]["departure_rps"] for burst in bursts]
        offered = [burst["backends"][declared["backend"]]["offered_rps"] for burst in bursts]
        latency = [
            burst["backends"][declared["backend"]]["artifact_latency_ms"]["p95"]
            for burst in bursts
        ]
        drain_latency = [
            burst["backends"][declared["backend"]]["drain_latency_ms"]["p95"]
            for burst in bursts
        ]
        backlog = [
            float(burst["backends"][declared["backend"]]["score_end_backlog"])
            for burst in bursts
        ]
        minor_faults = []
        major_faults = []
        for burst in bursts:
            rows = [
                row for row in requests_by_burst[burst["burst_id"]]
                if row["segment"] == "score" and row["backend"] == declared["backend"]
            ]
            minor_faults.append(statistics.mean(
                row["firecracker_process_faults_ready_to_artifact"]["minor_faults"]
                for row in rows
            ))
            major_faults.append(statistics.mean(
                row["firecracker_process_faults_ready_to_artifact"]["major_faults"]
                for row in rows
            ))
        cells.append({
            **declared,
            "passed": all(row["passed"] for row in cell_rows),
            "burst_verdicts": cell_rows,
            "offered_rps": bootstrap_mean_ci(offered, metric_seed),
            "departure_rps": bootstrap_mean_ci(departure, metric_seed + 1),
            "artifact_latency_p95_ms": bootstrap_mean_ci(latency, metric_seed + 2),
            "drain_latency_p95_ms": bootstrap_mean_ci(drain_latency, metric_seed + 3),
            "score_end_backlog": bootstrap_mean_ci(backlog, metric_seed + 4),
            "firecracker_process_minor_faults_per_request_ready_to_artifact": (
                bootstrap_mean_ci(minor_faults, metric_seed + 5)
            ),
            "firecracker_process_major_faults_per_request_ready_to_artifact": (
                bootstrap_mean_ci(major_faults, metric_seed + 6)
            ),
        })

    capacities = {}
    for backend in ("file", "uffd"):
        backend_cells = sorted(
            (cell for cell in cells if cell["backend"] == backend),
            key=lambda cell: cell["target_rps"],
        )
        capacity = None
        prefix_passes = True
        for cell in backend_cells:
            prefix_passes = prefix_passes and cell["passed"]
            if prefix_passes:
                capacity = cell["target_rps"]
        capacities[backend] = {
            "highest_contiguous_passing_rate_per_second": capacity,
            "all_declared_rates": [
                {"target_rps": cell["target_rps"], "passed": cell["passed"]}
                for cell in backend_cells
            ],
        }
    joint = None
    for rate in sorted(schedule["rates"]):
        at_rate = [cell for cell in cells if cell["target_rps"] == rate]
        if len(at_rate) != 2 or not all(cell["passed"] for cell in at_rate):
            break
        joint = rate

    trace_provenance = provenance.get("fault_trace")
    trace_enabled = schedule["trace_rate"] is not None
    if (
        not isinstance(trace_provenance, dict)
        or trace_provenance.get("enabled") is not trace_enabled
    ):
        raise AnalysisInvalid("fault-trace provenance differs from the schedule")
    if trace_enabled:
        limit = _finite_number(
            trace_provenance.get("max_median_delta_pct"),
            "fault-trace perturbation limit",
            minimum=0,
        )
        try:
            recomputed_trace_gate = reqscale.evaluate_trace_perturbation(summaries, limit)
        except reqscale.MeasurementInvalid as error:
            raise AnalysisInvalid(f"fault tracing perturbation gate failed: {error}") from error
        trace_gate = _load_json(os.path.join(run_dir, "trace-perturbation.json"))
        if trace_gate != recomputed_trace_gate:
            raise AnalysisInvalid("fault tracing perturbation artifact differs from raw burst data")
        if len(trace_gate.get("pairs", [])) != schedule["trace_pairs"] * 2:
            raise AnalysisInvalid("fault tracing perturbation artifact lacks matched backend pairs")
    else:
        trace_gate = {"enabled": False}

    return {
        "schema": ANALYSIS_SCHEMA,
        "run_id": run_id,
        "snapshot_generation_id": generation_id,
        "publishable": True,
        "experimental_unit": "burst",
        "analysis_seed": analysis_seed,
        "bootstrap_draws": BOOTSTRAP_DRAWS,
        "criteria": criteria,
        "control_gate": control_gate,
        "host_sample_gate": sample_gate,
        "trace_perturbation_gate": trace_gate,
        "fault_metric_scope": schedule["fault_metric_scope"],
        "cells": cells,
        "capacity": {
            "by_backend": capacities,
            "joint_highest_contiguous_passing_rate_per_second": joint,
        },
    }


def markdown_report(analysis: dict) -> str:
    lines = [
        "# Chromium request scalability",
        "",
        f"Run `{analysis['run_id']}` used snapshot generation "
        f"`{analysis['snapshot_generation_id']}`. The run passed its provenance, "
        "continuous-accounting, and host drift-control checks.",
        "",
        "FILE and UFFD were offered the same per-backend rate in one mixed stream. "
        "Each rate interval contained one request for each backend, separated by "
        "half an interval with seeded order. One warmup burst was excluded. Each "
        "scored burst used a 15 second ramp and 60 second score, and confidence "
        "intervals resample whole bursts rather than requests.",
        "",
        "## Capacity gates",
        "",
        "A burst passes only when offered-rate error, departure ratio, score-end "
        "backlog, launch lag, and zero-failure cleanup all meet the limits recorded "
        "before the run.",
        "",
        "| Backend | Target rps/backend | Pass | Offered rps mean (95% CI) | "
        "Departure rps mean (95% CI) | Artifact p95 ms mean (95% CI) | "
        "Drain p95 ms mean (95% CI) | Backlog mean (95% CI) |",
        "|---|---:|:---:|---:|---:|---:|---:|---:|",
    ]
    for cell in sorted(analysis["cells"], key=lambda row: (row["target_rps"], row["backend"])):
        departure = cell["departure_rps"]
        offered = cell["offered_rps"]
        latency = cell["artifact_latency_p95_ms"]
        drain = cell["drain_latency_p95_ms"]
        backlog = cell["score_end_backlog"]
        lines.append(
            f"| {cell['backend'].upper()} | {cell['target_rps']:g} | "
            f"{'yes' if cell['passed'] else 'no'} | "
            f"{offered['point']:.3g} ({offered['ci95_low']:.3g}–{offered['ci95_high']:.3g}) | "
            f"{departure['point']:.3g} ({departure['ci95_low']:.3g}–{departure['ci95_high']:.3g}) | "
            f"{latency['point']:.3g} ({latency['ci95_low']:.3g}–{latency['ci95_high']:.3g}) | "
            f"{drain['point']:.3g} ({drain['ci95_low']:.3g}–{drain['ci95_high']:.3g}) | "
            f"{backlog['point']:.3g} ({backlog['ci95_low']:.3g}–{backlog['ci95_high']:.3g}) |"
        )
    lines.extend([
        "",
        "The reported capacity is the highest contiguous declared rate for which "
        "every lower rate also passed. This avoids selecting an isolated high-rate "
        "pass after a lower-rate failure.",
        "",
        f"- FILE: {analysis['capacity']['by_backend']['file']['highest_contiguous_passing_rate_per_second']} requests/s",
        f"- UFFD: {analysis['capacity']['by_backend']['uffd']['highest_contiguous_passing_rate_per_second']} requests/s",
        f"- Joint: {analysis['capacity']['joint_highest_contiguous_passing_rate_per_second']} requests/s per backend",
        "",
        "## Firecracker process faults",
        "",
        "These counters cover endpoint readiness through artifact return. Values are "
        "per-request means with confidence intervals over burst means.",
        "",
        "| Backend | Target rps/backend | Minor faults/request mean (95% CI) | "
        "Major faults/request mean (95% CI) |",
        "|---|---:|---:|---:|",
    ])
    for cell in sorted(analysis["cells"], key=lambda row: (row["target_rps"], row["backend"])):
        minor = cell["firecracker_process_minor_faults_per_request_ready_to_artifact"]
        major = cell["firecracker_process_major_faults_per_request_ready_to_artifact"]
        lines.append(
            f"| {cell['backend'].upper()} | {cell['target_rps']:g} | "
            f"{minor['point']:.3g} ({minor['ci95_low']:.3g}–{minor['ci95_high']:.3g}) | "
            f"{major['point']:.3g} ({major['ci95_low']:.3g}–{major['ci95_high']:.3g}) |"
        )
    lines.extend([
        "",
        "## Drift and fault scope",
        "",
        f"The persistent host-native Chromium control changed "
        f"{analysis['control_gate']['drift_pct']:.3g}% between its first- and "
        f"second-half medians (absolute limit "
        f"{analysis['control_gate']['absolute_limit_pct']:.3g}%).",
        "",
        "Fault counters and optional handle_mm_fault timings cover the Firecracker "
        "process from endpoint readiness through artifact return. They include all "
        "Firecracker VMAs. They are not guest-RAM-filtered page faults and are not "
        "UFFD event counts.",
        "",
    ])
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True)
    parser.add_argument("--json-out")
    parser.add_argument("--markdown-out")
    args = parser.parse_args()
    if not args.json_out and not args.markdown_out:
        parser.error("at least one of --json-out or --markdown-out is required")
    try:
        analysis = analyze(os.path.abspath(args.run_dir))
        if args.json_out:
            reqscale.write_json_exclusive(os.path.abspath(args.json_out), analysis)
        if args.markdown_out:
            report_path = os.path.abspath(args.markdown_out)
            directory = os.path.dirname(report_path)
            os.makedirs(directory, exist_ok=True)
            temp = os.path.join(
                directory, f".{os.path.basename(report_path)}.{uuid.uuid4().hex}.tmp"
            )
            try:
                fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
                with os.fdopen(fd, "w") as stream:
                    stream.write(markdown_report(analysis))
                    stream.flush()
                    os.fsync(stream.fileno())
                os.link(temp, report_path)
                reqscale._fsync_directory(directory)
            finally:
                try:
                    os.unlink(temp)
                except FileNotFoundError:
                    pass
    except (AnalysisInvalid, reqscale.MeasurementInvalid, OSError, ValueError) as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        return 4
    return 0


if __name__ == "__main__":
    sys.exit(main())
