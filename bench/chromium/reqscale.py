#!/usr/bin/env python3
"""Auditable open-loop CDP-fast scalability and page-fault measurements.

This is deliberately an extension of ``reqbench.py`` rather than a revival of
``bench.sh``'s old phase4b. Every request uses the production CDP-fast path:
restore one clone, drive Chromium's CDP endpoint from the host, return the
artifact milestone, then synchronously prove teardown and disk cleanup.

The experimental unit is a burst.  At each declared per-backend rate, every
rate interval contains one FILE request and one UFFD request separated by half
an interval.  Their within-pair order comes from the recorded seed, so backend
is interleaved request-by-request rather than confounded with wall-clock blocks.
One full warmup burst is excluded, then at least five scored bursts each contain
a 15 second ramp and a 60 second scoring interval.  Launches follow absolute
monotonic deadlines and never wait for an earlier request to finish; artifact
completion and final teardown drain are separate milestones.

Sibling driver, host-control, FILE, and UFFD cgroups keep the accounting bases
separate.  A persistent host-native Chromium is driven every 10 seconds at a
seeded phase while a 5-second sampler records complete /proc/stat, load, PSI,
MemAvailable, and every cgroup's cpu.stat.  Per-request fault counters are
explicitly Firecracker-process counters sampled from endpoint-ready through the
artifact return.  Optional handle_mm_fault count/total/histogram data has that
same process scope; it is not a guest-RAM-VMA or UFFD-event measurement.

No chart or finding is produced here. This file produces measurement records;
the dataviz skill required by bench/chromium/AGENTS.md is not installed in this
session, so visualization stays a separate, later step.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fcntl
import hashlib
import json
import math
import os
import platform
import queue
import re
import select
import signal
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from typing import Callable, Iterable, Optional

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, HERE)

import reqbench  # noqa: E402


SCHEDULE_SCHEMA = "fcvm.chromium.reqscale.schedule.v2"
RECORD_SCHEMA = "fcvm.chromium.reqscale.record.v2"
RUN_ID_RE = re.compile(r"^[0-9a-f]{32}$")
CGROUP_NAME_RE = re.compile(r"^fcvm-reqscale-[0-9a-f]{32}$")
SNAPSHOT_TAG_RE = re.compile(r"^[A-Za-z0-9_.-]{1,128}$")
RAMP_SECONDS = 15.0
SCORE_SECONDS = 60.0
WARMUP_BURSTS = 1
CONTROL_INTERVAL_SECONDS = 10.0
SAMPLE_INTERVAL_SECONDS = 5.0
GUARDEXEC = os.path.join(HERE, "guardexec.py")
GUARDSUPERVISE = os.path.join(HERE, "guardsupervise.py")
CPU_FIELDS = (
    "user", "nice", "system", "idle", "iowait", "irq", "softirq",
    "steal", "guest", "guest_nice",
)


class MeasurementInvalid(RuntimeError):
    """The harness cannot defend the measurement and therefore stops."""

    def __init__(self, message: str, details=None):
        super().__init__(message)
        self.details = details


class MeasurementInterrupted(BaseException):
    """A requested shutdown that must wait for in-flight request cleanup."""

    def __init__(self, signum: int):
        self.signum = signum
        super().__init__(f"measurement interrupted by signal {signum}")


_TERMINATION_SIGNALS = (signal.SIGINT, signal.SIGTERM)


class TerminationFence:
    """Turn the first SIGINT/SIGTERM into a cleanup-capable interruption.

    Later signals are remembered but do not interrupt the bounded teardown.  An
    uncatchable process death is covered separately by every long-lived child's
    parent-death contract.
    """

    def __init__(self):
        self.previous = {}
        self.received: list[int] = []

    def _handle(self, signum, _frame):
        self.received.append(signum)
        if len(self.received) == 1:
            raise MeasurementInterrupted(signum)

    def __enter__(self):
        if threading.current_thread() is not threading.main_thread():
            raise MeasurementInvalid("termination fencing requires the main thread")
        if getattr(_termination_state, "fence", None) is not None:
            raise MeasurementInvalid("termination fencing cannot be nested")
        prior_mask = signal.pthread_sigmask(signal.SIG_BLOCK, _TERMINATION_SIGNALS)
        installed = []
        try:
            for signum in _TERMINATION_SIGNALS:
                self.previous[signum] = signal.getsignal(signum)
                signal.signal(signum, self._handle)
                installed.append(signum)
            _termination_state.fence = self
        except BaseException:
            for signum in reversed(installed):
                signal.signal(signum, self.previous[signum])
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)
            raise
        try:
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)
        except BaseException:
            # __enter__ will not receive a matching __exit__ when a pending signal
            # is delivered by the unmask. Restore the process-wide handlers here.
            signal.pthread_sigmask(signal.SIG_BLOCK, _TERMINATION_SIGNALS)
            for signum in reversed(installed):
                signal.signal(signum, self.previous[signum])
            _termination_state.fence = None
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)
            raise
        return self

    def __exit__(self, _type, _value, _traceback):
        prior_mask = signal.pthread_sigmask(signal.SIG_BLOCK, _TERMINATION_SIGNALS)
        try:
            for signum, handler in self.previous.items():
                signal.signal(signum, handler)
        finally:
            _termination_state.fence = None
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)


_termination_state = threading.local()


class DeferredTermination:
    """Record termination signals while one bounded critical section drains."""

    def __init__(self):
        self.previous = {}
        self.received: list[int] = []

    def _handle(self, signum, _frame):
        self.received.append(signum)

    def __enter__(self):
        if threading.current_thread() is not threading.main_thread():
            return self
        prior_mask = signal.pthread_sigmask(signal.SIG_BLOCK, _TERMINATION_SIGNALS)
        installed = []
        try:
            for signum in _TERMINATION_SIGNALS:
                self.previous[signum] = signal.getsignal(signum)
                signal.signal(signum, self._handle)
                installed.append(signum)
        except BaseException:
            for signum in reversed(installed):
                signal.signal(signum, self.previous[signum])
            raise
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)
        return self

    def __exit__(self, error_type, _value, _traceback):
        if threading.current_thread() is not threading.main_thread():
            return
        prior_mask = signal.pthread_sigmask(signal.SIG_BLOCK, _TERMINATION_SIGNALS)
        fence = getattr(_termination_state, "fence", None)
        if self.received:
            # The outer fence must know that the first signal already happened.
            # Otherwise a second SIGINT/SIGTERM during teardown would look like the
            # first one and interrupt cleanup.  Keep every later signal inert until
            # the bounded cleanup path has finished.
            if fence is not None:
                fence.received.extend(self.received)
        try:
            for signum, handler in self.previous.items():
                signal.signal(signum, handler)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, prior_mask)
        if self.received:
            if error_type is None:
                raise MeasurementInterrupted(self.received[0])


def guard_prefix(cgroup_path: str, parent_pid: Optional[int] = None) -> list[str]:
    """Prefix a later command with cgroup placement and parent-death fencing."""
    parent_pid = os.getpid() if parent_pid is None else parent_pid
    if parent_pid <= 1:
        raise MeasurementInvalid(f"invalid guard parent pid {parent_pid}")
    if not os.path.isabs(cgroup_path):
        raise MeasurementInvalid(f"child cgroup path is not absolute: {cgroup_path}")
    return [
        sys.executable,
        GUARDEXEC,
        "--expected-parent", str(parent_pid),
        "--cgroup-procs", os.path.join(cgroup_path, "cgroup.procs"),
        "--",
    ]


def guarded_command(cgroup_path: str, argv: Iterable[str], parent_pid: Optional[int] = None) -> list[str]:
    """Build the only allowed command path for measured child processes."""
    command = list(argv)
    if not command:
        raise MeasurementInvalid("guarded child command is empty")
    return guard_prefix(cgroup_path, parent_pid) + command


def supervised_command(
    cgroup_path: str, argv: Iterable[str], parent_pid: Optional[int] = None,
) -> list[str]:
    command = list(argv)
    if not command:
        raise MeasurementInvalid("supervised child command is empty")
    parent_pid = os.getpid() if parent_pid is None else parent_pid
    if parent_pid <= 1 or not os.path.isabs(cgroup_path):
        raise MeasurementInvalid("invalid supervised child ownership")
    return [
        sys.executable, GUARDSUPERVISE,
        "--expected-parent", str(parent_pid),
        "--cgroup-procs", os.path.join(cgroup_path, "cgroup.procs"),
        "--", *command,
    ]


@dataclass(frozen=True)
class CapacityCriteria:
    max_offered_rps_error_pct: float
    min_departure_ratio: float
    max_score_end_backlog: int
    max_p95_launch_lag_ms: float
    max_control_median_drift_pct: float


@dataclass(frozen=True)
class ScheduleConfig:
    rates: tuple[float, ...]
    scored_bursts: int
    seed: int
    criteria: CapacityCriteria
    warmup_bursts: int = WARMUP_BURSTS
    ramp_seconds: float = RAMP_SECONDS
    score_seconds: float = SCORE_SECONDS
    trace_rate: Optional[float] = None
    trace_pairs: int = 0


@dataclass(frozen=True)
class RequestPlan:
    request_index: int
    pair_index: int
    segment: str
    backend: str
    scheduled_offset_ns: int
    seed: int

    @classmethod
    def from_dict(cls, value: dict) -> "RequestPlan":
        fields = {field.name for field in dataclasses.fields(cls)}
        if set(value) != fields:
            raise MeasurementInvalid(
                f"request plan fields differ: missing={sorted(fields - set(value))} "
                f"extra={sorted(set(value) - fields)}"
            )
        return cls(**{key: value[key] for key in fields})

    def to_dict(self) -> dict:
        return dataclasses.asdict(self)


@dataclass(frozen=True)
class BurstSpec:
    burst_id: str
    block_id: str
    population: str
    target_rps: float
    repeat: int
    seed: int
    traced: bool
    trace_pair_id: Optional[str]
    ramp_seconds: float
    score_seconds: float
    requests: tuple[RequestPlan, ...]

    @classmethod
    def from_dict(cls, value: dict) -> "BurstSpec":
        fields = {field.name for field in dataclasses.fields(cls)}
        missing = fields - set(value)
        extra = set(value) - fields
        if missing:
            raise MeasurementInvalid(f"burst is missing fields {sorted(missing)}")
        if extra:
            raise MeasurementInvalid(f"burst has unknown fields {sorted(extra)}")
        converted = dict(value)
        raw_requests = converted.pop("requests")
        if not isinstance(raw_requests, list):
            raise MeasurementInvalid("burst requests are not a list")
        converted["requests"] = tuple(RequestPlan.from_dict(item) for item in raw_requests)
        return cls(**converted)

    def to_dict(self) -> dict:
        value = dataclasses.asdict(self)
        value["requests"] = [request.to_dict() for request in self.requests]
        return value


def _validate_run_id(run_id: str) -> None:
    if not RUN_ID_RE.fullmatch(run_id):
        raise ValueError("run_id must be 32 lowercase hexadecimal characters")


def _validate_snapshot_tag(snapshot_tag: str) -> None:
    if (
        not SNAPSHOT_TAG_RE.fullmatch(snapshot_tag)
        or snapshot_tag in (".", "..")
    ):
        raise ValueError(
            "snapshot tag must be 1..128 ASCII letters, digits, '.', '-', or '_', "
            "excluding '.' and '..'"
        )


def _rate_slug(rate: float) -> str:
    return format(rate, ".12g").replace(".", "p")


def _planned_count(rate: float, duration_s: float) -> int:
    product = rate * duration_s
    count = round(product)
    if count < 1 or not math.isclose(product, count, rel_tol=0.0, abs_tol=1e-9):
        raise ValueError(
            f"target_rps * window_seconds must be a positive integer, got "
            f"{rate} * {duration_s} = {product}"
        )
    return int(count)


def _validate_schedule_config(config: ScheduleConfig) -> None:
    if config.scored_bursts < 5:
        raise ValueError("every cell needs at least 5 independent scored bursts")
    if config.warmup_bursts != WARMUP_BURSTS:
        raise ValueError("exactly 1 explicit warmup burst is required")
    if config.ramp_seconds != RAMP_SECONDS or config.score_seconds != SCORE_SECONDS:
        raise ValueError("scored bursts must use exactly a 15s ramp and 60s score")
    if not config.rates:
        raise ValueError("at least one target rate is required")
    if len(set(config.rates)) != len(config.rates):
        raise ValueError("target rates must be unique")
    for rate in config.rates:
        if not math.isfinite(rate) or rate <= 0:
            raise ValueError(f"target rate must be finite and positive: {rate}")
        _planned_count(rate, config.ramp_seconds)
        _planned_count(rate, config.score_seconds)
        if config.scored_bursts * _planned_count(rate, config.score_seconds) < 200:
            raise ValueError(
                f"rate {rate:g} supplies fewer than 200 scored requests per backend "
                f"across {config.scored_bursts} bursts"
            )
    criteria = dataclasses.asdict(config.criteria)
    for field in (
        "max_offered_rps_error_pct", "max_p95_launch_lag_ms",
        "max_control_median_drift_pct",
    ):
        value = criteria[field]
        if not math.isfinite(value) or value < 0:
            raise ValueError(f"{field} must be finite and nonnegative")
    if (
        not math.isfinite(config.criteria.min_departure_ratio)
        or not 0 < config.criteria.min_departure_ratio <= 1
    ):
        raise ValueError("min_departure_ratio must be in (0, 1]")
    if (
        isinstance(config.criteria.max_score_end_backlog, bool)
        or not isinstance(config.criteria.max_score_end_backlog, int)
        or config.criteria.max_score_end_backlog < 0
    ):
        raise ValueError("max_score_end_backlog must be a nonnegative integer")
    if (config.trace_rate is None) != (config.trace_pairs == 0):
        raise ValueError("trace_rate and trace_pairs must be supplied together")
    if config.trace_rate is not None:
        if config.trace_rate not in config.rates:
            raise ValueError("trace_rate must name one of the scale cells")
        if config.trace_pairs < 3:
            raise ValueError("fault tracing needs at least 3 matched perturbation pairs")


def _derive_u64(seed: int, *parts) -> int:
    """Version-independent seeded derivation used by every durable schedule."""
    payload = json.dumps(
        ["fcvm-reqscale-seed-v1", seed, *parts],
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode()
    return int.from_bytes(hashlib.sha256(payload).digest()[:8], "big")


def _stable_shuffle(items: Iterable[dict], seed: int, domain: str) -> list[dict]:
    """Return a hash-ranked order independent of Python's random implementation."""
    return sorted(
        items,
        key=lambda item: (
            hashlib.sha256(json.dumps(
                ["fcvm-reqscale-order-v1", seed, domain, item],
                sort_keys=True,
                separators=(",", ":"),
                allow_nan=False,
            ).encode()).digest(),
            json.dumps(item, sort_keys=True, separators=(",", ":"), allow_nan=False),
        ),
    )


def _build_request_plan(
    rate: float, ramp_seconds: float, score_seconds: float, seed: int,
) -> tuple[RequestPlan, ...]:
    """Return a fully materialized mixed-backend schedule for one burst."""
    requests = []
    request_index = 0
    pair_base = 0
    for segment, start_s, duration_s in (
        ("ramp", 0.0, ramp_seconds),
        ("score", ramp_seconds, score_seconds),
    ):
        count = _planned_count(rate, duration_s)
        for local_pair in range(count):
            pair_index = pair_base + local_pair
            first = (
                "file"
                if _derive_u64(seed, "backend-order", segment, local_pair) & 1 == 0
                else "uffd"
            )
            order = (first, "uffd" if first == "file" else "file")
            pair_start = start_s + local_pair / rate
            for half, backend in enumerate(order):
                offset_s = pair_start + half / (2.0 * rate)
                requests.append(RequestPlan(
                    request_index=request_index,
                    pair_index=pair_index,
                    segment=segment,
                    backend=backend,
                    scheduled_offset_ns=round(offset_s * 1_000_000_000),
                    seed=_derive_u64(seed, "request", segment, local_pair, backend),
                ))
                request_index += 1
        pair_base += count
    offsets = [request.scheduled_offset_ns for request in requests]
    if offsets != sorted(offsets) or len(offsets) != len(set(offsets)):
        raise ValueError(f"rate {rate:g} cannot be represented as unique nanosecond deadlines")
    return tuple(requests)


def build_schedule(config: ScheduleConfig, run_id: str) -> dict:
    """Build the complete schedule before any workload process exists.

    Every request deadline and backend assignment is serialized.  Warmups are
    shuffled by rate but always precede scored work.  Scored bursts are shuffled
    by rate/repeat.  Each trace pair uses one identical request plan for its
    traced and untraced members, with their order chosen by the recorded seed.
    """
    _validate_run_id(run_id)
    _validate_schedule_config(config)
    warmup_blocks = [{
        "population": "warmup",
        "block_id": f"warmup-r{_rate_slug(rate)}",
        "repeat": 0,
        "rate": rate,
    } for rate in config.rates]
    warmup_blocks = _stable_shuffle(warmup_blocks, config.seed, "warmup-bursts")

    measurement_blocks: list[dict] = []
    for repeat in range(config.scored_bursts):
        for rate in config.rates:
            measurement_blocks.append({
                "population": "scored",
                "block_id": f"scored-r{_rate_slug(rate)}-n{repeat}",
                "repeat": repeat,
                "rate": rate,
            })
    measurement_blocks = _stable_shuffle(
        measurement_blocks, config.seed, "scored-bursts"
    )
    blocks = warmup_blocks + measurement_blocks
    if config.trace_rate is not None:
        trace_blocks = []
        for pair in range(config.trace_pairs):
            trace_pair_id = f"trace:{pair}"
            members = [False, True]
            if _derive_u64(config.seed, "trace-member-order", pair) & 1:
                members.reverse()
            plan_seed = _derive_u64(config.seed, "trace-plan", pair)
            for traced in members:
                trace_blocks.append({
                    "population": "trace-perturbation",
                    "block_id": f"trace-n{pair}-{'on' if traced else 'off'}",
                    "trace_pair_id": trace_pair_id,
                    "repeat": pair,
                    "rate": config.trace_rate,
                    "traced": traced,
                    "plan_seed": plan_seed,
                })
        pair_order = sorted(
            range(config.trace_pairs),
            key=lambda pair: (_derive_u64(config.seed, "trace-pair-order", pair), pair),
        )
        blocks.extend([
            block for pair in pair_order for block in trace_blocks
            if block["repeat"] == pair
        ])

    bursts = []
    for index, block in enumerate(blocks):
        traced = bool(block.get("traced", False))
        plan_seed = (
            block["plan_seed"] if "plan_seed" in block else _derive_u64(
                config.seed, "burst-plan", block["block_id"]
            )
        )
        spec = BurstSpec(
            burst_id=f"b{index:04d}-r{_rate_slug(block['rate'])}"
                     f"{'-trace' if traced else ''}",
            block_id=block["block_id"],
            population=block["population"],
            target_rps=block["rate"],
            repeat=block["repeat"],
            seed=plan_seed,
            traced=traced,
            trace_pair_id=block.get("trace_pair_id"),
            ramp_seconds=config.ramp_seconds,
            score_seconds=config.score_seconds,
            requests=_build_request_plan(
                block["rate"], config.ramp_seconds, config.score_seconds, plan_seed
            ),
        )
        bursts.append(spec.to_dict())

    control_phase_ns = _derive_u64(
        config.seed, "control-phase"
    ) % round(CONTROL_INTERVAL_SECONDS * 1e9)

    return {
        "schema": SCHEDULE_SCHEMA,
        "run_id": run_id,
        "seed": config.seed,
        "rates": list(config.rates),
        "cells": [
            {
                "cell_id": f"{backend}:r{format(rate, '.12g')}",
                "backend": backend,
                "target_rps": rate,
                "independent_bursts": config.scored_bursts,
                "planned_scored_requests_per_burst": _planned_count(
                    rate, config.score_seconds
                ),
                "planned_scored_requests_total": (
                    config.scored_bursts * _planned_count(rate, config.score_seconds)
                ),
                "warmup_bursts": config.warmup_bursts,
            }
            for rate in config.rates
            for backend in ("file", "uffd")
        ],
        "randomization": {
            "unit": "burst",
            "burst_order": "rates and repeats shuffled",
            "request_order": (
                "one FILE and one UFFD per rate interval; seeded order within each "
                "pair; half-interval separation"
            ),
            "warmup_precedes_measurement": True,
        },
        "independent_bursts_per_cell": config.scored_bursts,
        "warmup_bursts_per_rate": config.warmup_bursts,
        "warmup_included_in_primary_measurement": False,
        "ramp_seconds": config.ramp_seconds,
        "score_seconds": config.score_seconds,
        "capacity_criteria": dataclasses.asdict(config.criteria) | {
            "require_zero_failures": True,
        },
        "control": {
            "kind": "persistent-host-native-chromium",
            "interval_ns": round(CONTROL_INTERVAL_SECONDS * 1e9),
            "phase_seed": config.seed,
            "phase_derivation": "sha256 fcvm-reqscale-seed-v1/control-phase",
            "phase_offset_ns": control_phase_ns,
            "warmup_requests": 1,
        },
        "host_sample_interval_ns": round(SAMPLE_INTERVAL_SECONDS * 1e9),
        "trace_rate": config.trace_rate,
        "trace_pairs": config.trace_pairs,
        "fault_metric_scope": (
            "Firecracker process endpoint-ready through artifact return; includes all "
            "Firecracker VMAs and is not a guest-RAM-VMA or UFFD-event count"
        ),
        "bursts": bursts,
    }


def canonical_json(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()


def _strict_json_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise MeasurementInvalid(f"JSON object has duplicate key {key!r}")
        value[key] = item
    return value


def _reject_json_constant(value):
    raise MeasurementInvalid(f"JSON contains non-standard constant {value}")


def strict_json_loads(raw: str | bytes, source: str):
    try:
        return json.loads(
            raw,
            object_pairs_hook=_strict_json_object,
            parse_constant=_reject_json_constant,
        )
    except (json.JSONDecodeError, UnicodeError) as error:
        raise MeasurementInvalid(f"malformed JSON in {source}: {error}") from error


def schedule_sha256(schedule: dict) -> str:
    return hashlib.sha256(canonical_json(schedule)).hexdigest()


# ---------------------------------------------------------------- open-loop launch


@dataclass(frozen=True)
class RequestContext:
    run_id: str
    burst_id: str
    population: str
    segment: str
    backend: str
    target_rps: float
    request_index: int
    pair_index: int
    request_id: str
    scheduled_ns: int
    actual_launch_ns: int
    request_seed: int


class SystemClock:
    def monotonic_ns(self) -> int:
        return time.monotonic_ns()

    def sleep_until_ns(self, deadline_ns: int) -> None:
        while True:
            remaining_ns = deadline_ns - time.monotonic_ns()
            if remaining_ns <= 0:
                return
            time.sleep(remaining_ns / 1_000_000_000)


@dataclass
class _ThreadHandle:
    thread: threading.Thread
    result: queue.Queue


class ThreadLauncher:
    """One short-lived client thread per request; no executor queue or cap.

    A fixed worker pool creates a hidden closed-loop ceiling once its queue fills.
    Here each thread exits with its request, so peak threads track actual in-flight
    work and the scheduler never waits for capacity before launching the next one.
    """

    def launch(self, context: RequestContext, request_fn: Callable) -> _ThreadHandle:
        result: queue.Queue = queue.Queue(maxsize=1)

        def worker():
            actual_ns = time.monotonic_ns()
            owned = dataclasses.replace(context, actual_launch_ns=actual_ns)
            try:
                record = request_fn(owned)
                if not isinstance(record, dict):
                    raise TypeError(f"request_fn returned {type(record).__name__}, not dict")
            except Exception as error:  # preserve a durable failed request
                supplied = getattr(error, "record", None)
                record = dict(supplied) if isinstance(supplied, dict) else {}
                record["ok"] = False
                record.setdefault("error", f"{type(error).__name__}: {error}")
                elapsed_ms = (time.monotonic_ns() - actual_ns) / 1_000_000
                record.setdefault("blocking_ms", elapsed_ms)
                record.setdefault("wall_ms", elapsed_ms)
            except BaseException as fatal:
                # Signals represented by reqbench's BaseException subclass must
                # unwind the run. They are delivered to drain only after every
                # already-launched client thread has joined, so cleanup cannot
                # race requests that were still tearing down.
                result.put(("fatal", fatal))
                return
            record.setdefault("request_id", owned.request_id)
            record.setdefault("request_index", owned.request_index)
            record.setdefault("pair_index", owned.pair_index)
            record.setdefault("burst_id", owned.burst_id)
            record.setdefault("population", owned.population)
            record.setdefault("segment", owned.segment)
            record.setdefault("backend", owned.backend)
            record.setdefault("target_rps", owned.target_rps)
            record.setdefault("request_seed", owned.request_seed)
            record.setdefault("scheduled_ns", owned.scheduled_ns)
            record.setdefault("actual_launch_ns", owned.actual_launch_ns)
            record.setdefault("finished_ns", time.monotonic_ns())
            result.put(("record", record))

        thread = threading.Thread(
            target=worker,
            name=f"reqscale-{context.burst_id}-{context.request_index}",
            daemon=False,
        )
        thread.start()
        return _ThreadHandle(thread=thread, result=result)

    def drain(self, handles: Iterable[_ThreadHandle]) -> list[dict]:
        records = []
        fatals = []
        for handle in handles:
            handle.thread.join()
            try:
                kind, value = handle.result.get_nowait()
            except queue.Empty:
                fatals.append(MeasurementInvalid(
                    f"client thread {handle.thread.name} exited without a result"
                ))
                continue
            if kind == "fatal":
                fatals.append(value)
            elif kind == "record":
                records.append(value)
            else:
                fatals.append(MeasurementInvalid(
                    f"client thread {handle.thread.name} returned unknown result kind {kind!r}"
                ))
        if fatals:
            raise fatals[0]
        return records


def _percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    if not ordered:
        raise MeasurementInvalid("cannot summarize an empty measurement")
    # Nearest-rank keeps the raw observation instead of interpolating a latency
    # the run never observed.
    index = max(0, min(len(ordered) - 1, math.ceil(q * len(ordered)) - 1))
    return float(ordered[index])


def distribution(values: list[float]) -> dict:
    if not values:
        return {"n": 0, "min": None, "median": None, "p95": None, "max": None}
    return {
        "n": len(values),
        "min": float(min(values)),
        "median": float(statistics.median(values)),
        "p95": _percentile(values, 0.95),
        "max": float(max(values)),
    }


def _validate_request_record(record: dict) -> None:
    required = (
        "request_id", "request_index", "scheduled_ns", "actual_launch_ns",
        "finished_ns", "blocking_ms", "wall_ms", "ok",
    )
    for field in required:
        if field not in record:
            raise MeasurementInvalid(f"request record is missing {field}: {record}")
    for field in ("request_index", "scheduled_ns", "actual_launch_ns", "finished_ns"):
        if not isinstance(record[field], int) or isinstance(record[field], bool):
            raise MeasurementInvalid(f"request record {field} is not an integer: {record}")
        if record[field] < 0:
            raise MeasurementInvalid(f"request record {field} is negative: {record}")
    if not isinstance(record["request_id"], str) or not record["request_id"]:
        raise MeasurementInvalid(f"request record has invalid request_id: {record}")
    if not isinstance(record["ok"], bool):
        raise MeasurementInvalid(f"request record ok is not boolean: {record}")
    for field in ("blocking_ms", "wall_ms"):
        value = record[field]
        if (
            not isinstance(value, (int, float))
            or isinstance(value, bool)
            or not math.isfinite(value)
            or value < 0
        ):
            raise MeasurementInvalid(f"request record {field} is invalid: {record}")
    if record.get("ok") and "artifact_ns" not in record:
        raise MeasurementInvalid(f"successful request record is missing artifact_ns: {record}")
    scheduled = record["scheduled_ns"]
    launched = record["actual_launch_ns"]
    finished = record["finished_ns"]
    artifact = record.get("artifact_ns")
    if artifact is not None and (not isinstance(artifact, int) or isinstance(artifact, bool)):
        raise MeasurementInvalid(f"request record artifact_ns is not an integer: {record}")
    if launched < scheduled:
        raise MeasurementInvalid(f"request launched before its deadline: {record}")
    if artifact is not None and artifact < launched:
        raise MeasurementInvalid(f"request artifact predates launch: {record}")
    if artifact is not None and finished < artifact:
        raise MeasurementInvalid(f"request drain predates artifact: {record}")
    if record["blocking_ms"] > record["wall_ms"] + 1.0:
        raise MeasurementInvalid(f"request blocking time exceeds wall time: {record}")
    if artifact is not None:
        artifact_elapsed_ms = (artifact - launched) / 1_000_000
        if record["blocking_ms"] > artifact_elapsed_ms + 1.0:
            raise MeasurementInvalid(
                f"request blocking time exceeds its artifact milestone: {record}"
            )
    wall_elapsed_ms = (finished - launched) / 1_000_000
    if record["wall_ms"] > wall_elapsed_ms + 1.0:
        raise MeasurementInvalid(
            f"request wall time exceeds its finished milestone: {record}"
        )


def _backlog_at(records: list[dict], backend: str, at_ns: int) -> int:
    return sum(
        record["backend"] == backend
        and record["actual_launch_ns"] <= at_ns
        and (record.get("artifact_ns") is None or record["artifact_ns"] > at_ns)
        for record in records
    )


def _max_backlog(records: list[dict], backend: str, start_ns: int, end_ns: int) -> int:
    backlog = _backlog_at(records, backend, start_ns)
    maximum = backlog
    events = []
    for record in records:
        if record["backend"] != backend:
            continue
        launched = record["actual_launch_ns"]
        artifact = record.get("artifact_ns")
        if start_ns < launched <= end_ns:
            events.append((launched, 1))
        if artifact is not None and start_ns < artifact <= end_ns:
            events.append((artifact, -1))
    by_timestamp = {}
    for timestamp, delta in events:
        by_timestamp.setdefault(timestamp, []).append(delta)
    for timestamp in sorted(by_timestamp):
        deltas = by_timestamp[timestamp]
        # Count same-timestamp launches before completions. This handles a
        # zero-duration request without allowing its completion to drive the
        # pre-existing backlog negative.
        backlog += sum(delta == 1 for delta in deltas)
        maximum = max(maximum, backlog)
        backlog -= sum(delta == -1 for delta in deltas)
        if backlog < 0:
            raise MeasurementInvalid("artifact completion drove burst backlog below zero")
    return maximum


def _burst_metadata(spec: BurstSpec) -> dict:
    value = spec.to_dict()
    requests = value.pop("requests")
    value["request_plan_count"] = len(requests)
    value["request_plan_sha256"] = hashlib.sha256(canonical_json(requests)).hexdigest()
    return value


def run_open_loop_burst(
    run_id: str,
    spec: BurstSpec,
    request_fn: Callable[[RequestContext], dict],
    clock=None,
    launcher=None,
) -> tuple[list[dict], dict]:
    """Launch the serialized mixed-backend burst, then enter drain once."""
    _validate_run_id(run_id)
    clock = clock or SystemClock()
    launcher = launcher or ThreadLauncher()
    if not spec.requests:
        raise MeasurementInvalid(f"burst {spec.burst_id} has no requests")
    indices = [request.request_index for request in spec.requests]
    if indices != list(range(len(spec.requests))):
        raise MeasurementInvalid(f"burst {spec.burst_id} request indices are not contiguous")
    offsets = [request.scheduled_offset_ns for request in spec.requests]
    if offsets != sorted(offsets) or len(offsets) != len(set(offsets)):
        raise MeasurementInvalid(f"burst {spec.burst_id} request deadlines are not unique and sorted")
    burst_start_ns = clock.monotonic_ns()
    handles = []
    launch_error = None
    try:
        for request in spec.requests:
            scheduled_ns = burst_start_ns + request.scheduled_offset_ns
            clock.sleep_until_ns(scheduled_ns)
            actual_ns = clock.monotonic_ns()
            context = RequestContext(
                run_id=run_id,
                burst_id=spec.burst_id,
                population=spec.population,
                segment=request.segment,
                backend=request.backend,
                target_rps=spec.target_rps,
                request_index=request.request_index,
                pair_index=request.pair_index,
                request_id=f"{run_id}:{spec.burst_id}:{request.request_index}",
                scheduled_ns=scheduled_ns,
                actual_launch_ns=actual_ns,
                request_seed=request.seed,
            )
            # A termination signal cannot land after the worker starts but before
            # its handle becomes owned by this burst.  The deferred signal is
            # raised immediately after ownership is recorded.
            with DeferredTermination():
                handle = launcher.launch(context, request_fn)
                handles.append(handle)
    except BaseException as error:
        launch_error = error
    schedule_submission_end_ns = clock.monotonic_ns()

    try:
        # Signals are deferred until every launched request finishes its bounded
        # teardown.  This prevents outer cleanup from racing live clone workers.
        with DeferredTermination():
            records = launcher.drain(handles)
    except BaseException as drain_error:
        if launch_error is not None:
            raise MeasurementInvalid(
                "open-loop launch stopped and in-flight request drain also failed",
                {
                    "launch_error": f"{type(launch_error).__name__}: {launch_error}",
                    "drain_error": f"{type(drain_error).__name__}: {drain_error}",
                    "launched": len(handles),
                    "planned": len(spec.requests),
                },
            ) from drain_error
        raise
    if launch_error is not None:
        raise launch_error
    records.sort(key=lambda record: record.get("request_index", -1))
    count = len(spec.requests)
    if len(records) != count:
        raise MeasurementInvalid(f"burst drained {len(records)} records for {count} launches")
    if len({record.get("request_id") for record in records}) != count:
        raise MeasurementInvalid("burst has duplicate or missing request ids")
    for record, planned in zip(records, spec.requests):
        _validate_request_record(record)
        expected = {
            "request_index": planned.request_index,
            "pair_index": planned.pair_index,
            "segment": planned.segment,
            "backend": planned.backend,
            "request_seed": planned.seed,
            "burst_id": spec.burst_id,
            "population": spec.population,
            "target_rps": spec.target_rps,
        }
        mismatched = {
            field: {"expected": value, "actual": record.get(field)}
            for field, value in expected.items() if record.get(field) != value
        }
        if mismatched:
            raise MeasurementInvalid(
                f"burst {spec.burst_id} request metadata diverged from schedule: {mismatched}"
            )

    artifact_ns = [record["artifact_ns"] for record in records if "artifact_ns" in record]
    finished_ns = [record["finished_ns"] for record in records]
    score_start_ns = burst_start_ns + round(spec.ramp_seconds * 1_000_000_000)
    score_end_ns = score_start_ns + round(spec.score_seconds * 1_000_000_000)
    scored = [record for record in records if record["segment"] == "score"]
    per_backend = {}
    for backend in ("file", "uffd"):
        rows = [record for record in scored if record["backend"] == backend]
        planned = _planned_count(spec.target_rps, spec.score_seconds)
        if len(rows) != planned:
            raise MeasurementInvalid(
                f"burst {spec.burst_id} has {len(rows)} scored {backend} requests, "
                f"expected {planned}"
            )
        launched_by_end = sum(row["actual_launch_ns"] <= score_end_ns for row in rows)
        artifact_by_end = sum(
            row.get("artifact_ns") is not None and row["artifact_ns"] <= score_end_ns
            for row in rows
        )
        launch_lags = [
            (row["actual_launch_ns"] - row["scheduled_ns"]) / 1_000_000
            for row in rows
        ]
        artifact_latencies = [
            (row["artifact_ns"] - row["actual_launch_ns"]) / 1_000_000
            for row in rows if row.get("artifact_ns") is not None
        ]
        drain_latencies = [
            (row["finished_ns"] - row["artifact_ns"]) / 1_000_000
            for row in rows if row.get("artifact_ns") is not None
        ]
        wall_latencies = [
            (row["finished_ns"] - row["actual_launch_ns"]) / 1_000_000
            for row in rows
        ]
        per_backend[backend] = {
            "cell_id": f"{backend}:r{format(spec.target_rps, '.12g')}",
            "planned": planned,
            "launched": len(rows),
            "launched_by_score_end": launched_by_end,
            "artifact_completed": sum(row.get("artifact_ns") is not None for row in rows),
            "artifact_completed_by_score_end": artifact_by_end,
            "drained": sum("finished_ns" in row for row in rows),
            "cleanup_confirmed": sum(
                isinstance(row.get("teardown"), dict)
                and row["teardown"].get("all_gone") is True
                for row in rows
            ),
            "ok": sum(bool(row.get("ok")) for row in rows),
            "failed": sum(not bool(row.get("ok")) for row in rows),
            "offered_rps": launched_by_end / spec.score_seconds,
            "departure_rps": artifact_by_end / spec.score_seconds,
            "departure_ratio": artifact_by_end / planned,
            "score_start_backlog": _backlog_at(records, backend, score_start_ns),
            "score_end_backlog": _backlog_at(records, backend, score_end_ns),
            "max_backlog_during_score": _max_backlog(
                records, backend, score_start_ns, score_end_ns
            ),
            "launch_lag_ms": distribution(launch_lags),
            "artifact_latency_ms": distribution(artifact_latencies),
            "drain_latency_ms": distribution(drain_latencies),
            "wall_latency_ms": distribution(wall_latencies),
            "blocking_latency_ms": distribution([
                float(row["blocking_ms"]) for row in rows if row.get("ok")
            ]),
        }
    summary = {
        "schema": RECORD_SCHEMA,
        "kind": "burst",
        **_burst_metadata(spec),
        "burst_start_ns": burst_start_ns,
        "score_start_ns": score_start_ns,
        "score_end_ns": score_end_ns,
        "planned": len(scored),
        "launched": len(scored),
        "artifact_completed": sum("artifact_ns" in row for row in scored),
        "drained": sum("finished_ns" in row for row in scored),
        "cleanup_confirmed": sum(
            record.get("teardown", {}).get("all_gone") is True
            for record in scored
            if isinstance(record.get("teardown"), dict)
        ),
        "ok": sum(bool(record.get("ok")) for record in scored),
        "failed": sum(not bool(record.get("ok")) for record in records),
        "total_planned": count,
        "total_artifact_completed": len(artifact_ns),
        "total_drained": len(finished_ns),
        "total_cleanup_confirmed": sum(
            isinstance(record.get("teardown"), dict)
            and record["teardown"].get("all_gone") is True
            for record in records
        ),
        "schedule_submission_span_ms": (
            schedule_submission_end_ns - burst_start_ns
        ) / 1_000_000,
        "launch_span_ms": (
            max(record["actual_launch_ns"] for record in records) - burst_start_ns
        ) / 1_000_000,
        "completion_span_ms": (
            (max(artifact_ns) - burst_start_ns) / 1_000_000 if artifact_ns else None
        ),
        "drain_span_ms": (max(finished_ns) - burst_start_ns) / 1_000_000,
        "launch_lag_ms": distribution([
            (record["actual_launch_ns"] - record["scheduled_ns"]) / 1_000_000
            for record in scored
        ]),
        "latency_ms": distribution([
            float(record["blocking_ms"]) for record in scored if record.get("ok")
        ]),
        "backends": per_backend,
    }
    return records, summary


# ---------------------------------------------------------------- procfs/cgroup


@dataclass(frozen=True)
class ProcessStat:
    pid: int
    state: str
    minor_faults: int
    major_faults: int
    start_time_ticks: int


def parse_process_stat(raw: str) -> ProcessStat:
    """Parse /proc/PID/stat without being fooled by parentheses in comm."""
    try:
        pid_text, rest = raw.split(" ", 1)
        fields = rest.rsplit(") ", 1)[1].split()
        return ProcessStat(
            pid=int(pid_text),
            state=fields[0],
            minor_faults=int(fields[7]),   # field 10
            major_faults=int(fields[9]),   # field 12
            start_time_ticks=int(fields[19]),  # field 22
        )
    except (IndexError, ValueError) as error:
        raise MeasurementInvalid(f"malformed /proc stat record: {error}") from error


def parse_cpu_stat(raw: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for number, line in enumerate(raw.splitlines(), 1):
        parts = line.split()
        if not parts:
            continue
        if len(parts) != 2 or not parts[1].isdigit():
            raise MeasurementInvalid(f"malformed cpu.stat line {number}: {line!r}")
        if parts[0] in out:
            raise MeasurementInvalid(f"duplicate cpu.stat counter {parts[0]!r}")
        out[parts[0]] = int(parts[1])
    if "usage_usec" not in out:
        raise MeasurementInvalid("cpu.stat has no usage_usec counter")
    return out


def counter_delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    if set(before) != set(after):
        raise MeasurementInvalid(
            f"counter set changed during window: before={sorted(before)} after={sorted(after)}"
        )
    out = {key: after[key] - before[key] for key in before}
    negative = {key: value for key, value in out.items() if value < 0}
    if negative:
        raise MeasurementInvalid(f"counters moved backwards: {negative}")
    return out


def read_machine_proc_stat(path: str = "/proc/stat") -> dict:
    """Capture the complete machine-wide /proc/stat plus its aggregate CPU row."""
    with open(path) as stream:
        raw = stream.read()
    lines = raw.splitlines()
    first = lines[0].split() if lines else []
    if not first or first[0] != "cpu" or len(first) < 5:
        raise MeasurementInvalid(f"{path} has no aggregate cpu line")
    try:
        values = [int(value) for value in first[1:]]
    except ValueError as error:
        raise MeasurementInvalid(f"{path} aggregate cpu line is malformed") from error
    names = list(CPU_FIELDS)
    if len(values) > len(names):
        names.extend(f"field_{index}" for index in range(len(names), len(values)))
    return {
        "path": path,
        "captured_wall_ns": time.time_ns(),
        "captured_monotonic_ns": time.monotonic_ns(),
        "clk_tck": os.sysconf("SC_CLK_TCK"),
        # Preserve every kernel-supplied row.  The aggregate is parsed separately
        # for a checked delta, but dropping intr/ctxt/processes/per-CPU rows would
        # not satisfy the run's whole-/proc/stat accounting contract.
        "raw": raw,
        "raw_sha256": hashlib.sha256(raw.encode()).hexdigest(),
        "cpu": dict(zip(names, values)),
    }


def _read_text_artifact(path: str) -> dict:
    try:
        with open(path) as stream:
            raw = stream.read()
    except OSError as error:
        raise MeasurementInvalid(f"cannot read host accounting input {path}: {error}") from error
    return {"path": path, "raw": raw, "raw_sha256": hashlib.sha256(raw.encode()).hexdigest()}


def parse_loadavg(raw: str, source: str) -> dict:
    fields = raw.split()
    try:
        if len(fields) != 5:
            raise ValueError(f"expected 5 fields, found {len(fields)}")
        running, total = fields[3].split("/", 1)
        parsed = {
            "load_1": float(fields[0]),
            "load_5": float(fields[1]),
            "load_15": float(fields[2]),
            "running_tasks": int(running),
            "total_tasks": int(total),
            "last_pid": int(fields[4]),
        }
    except (ValueError, IndexError) as error:
        raise MeasurementInvalid(f"malformed loadavg {source}: {error}") from error
    if any(not math.isfinite(parsed[key]) or parsed[key] < 0 for key in ("load_1", "load_5", "load_15")):
        raise MeasurementInvalid(f"invalid load averages in {source}")
    if (
        parsed["running_tasks"] < 0
        or parsed["total_tasks"] < parsed["running_tasks"]
        or parsed["last_pid"] < 0
    ):
        raise MeasurementInvalid(f"invalid task counts in loadavg {source}")
    return parsed


def read_loadavg(path: str = "/proc/loadavg") -> dict:
    captured = _read_text_artifact(path)
    return {**captured, "parsed": parse_loadavg(captured["raw"], path)}


def parse_psi(raw: str, source: str) -> dict:
    parsed = {}
    for number, line in enumerate(raw.splitlines(), 1):
        fields = line.split()
        if not fields:
            continue
        category = fields[0]
        if category not in ("some", "full") or category in parsed:
            raise MeasurementInvalid(f"malformed PSI category at {source}:{number}: {line!r}")
        values = {}
        for field in fields[1:]:
            key, separator, raw_value = field.partition("=")
            if not separator or key in values:
                raise MeasurementInvalid(f"malformed PSI field at {source}:{number}: {field!r}")
            try:
                values[key] = int(raw_value) if key == "total" else float(raw_value)
            except ValueError as error:
                raise MeasurementInvalid(
                    f"non-numeric PSI field at {source}:{number}: {field!r}"
                ) from error
        if set(values) != {"avg10", "avg60", "avg300", "total"}:
            raise MeasurementInvalid(f"incomplete PSI row at {source}:{number}: {line!r}")
        if any(not math.isfinite(values[key]) or values[key] < 0 for key in ("avg10", "avg60", "avg300")) or values["total"] < 0:
            raise MeasurementInvalid(f"invalid PSI values at {source}:{number}")
        parsed[category] = values
    if "some" not in parsed:
        raise MeasurementInvalid(f"PSI input {source} has no some row")
    return parsed


def read_psi(root: str = "/proc/pressure") -> dict:
    out = {}
    for resource in ("cpu", "memory", "io"):
        captured = _read_text_artifact(os.path.join(root, resource))
        captured["parsed"] = parse_psi(captured["raw"], captured["path"])
        out[resource] = captured
    return out


def parse_meminfo(raw: str, source: str) -> dict:
    values = {}
    for number, line in enumerate(raw.splitlines(), 1):
        key, separator, rest = line.partition(":")
        fields = rest.split()
        if not separator or not key or not fields or len(fields) > 2 or key in values:
            raise MeasurementInvalid(
                f"malformed meminfo line {source}:{number}: {line!r}"
            )
        try:
            value = int(fields[0])
        except ValueError as error:
            raise MeasurementInvalid(
                f"non-numeric meminfo line {source}:{number}: {line!r}"
            ) from error
        if value < 0:
            raise MeasurementInvalid(
                f"negative meminfo value at {source}:{number}: {line!r}"
            )
        values[key] = {"value": value, "unit": fields[1] if len(fields) > 1 else None}
    if "MemAvailable" not in values:
        raise MeasurementInvalid(f"{source} has no MemAvailable")
    return values


def read_meminfo(path: str = "/proc/meminfo") -> dict:
    captured = _read_text_artifact(path)
    return {**captured, "parsed": parse_meminfo(captured["raw"], path)}


class ProcReader:
    def __init__(self, root: str = "/proc"):
        self.root = root

    def children_of(self, pid: int) -> list[int]:
        children = []
        try:
            tids = os.listdir(os.path.join(self.root, str(pid), "task"))
        except OSError as error:
            raise MeasurementInvalid(f"cannot enumerate children of pid {pid}: {error}")
        for tid in tids:
            try:
                with open(os.path.join(self.root, str(pid), "task", tid, "children")) as f:
                    children.extend(int(value) for value in f.read().split())
            except OSError:
                continue
        return sorted(set(children))

    def comm(self, pid: int) -> str:
        try:
            with open(os.path.join(self.root, str(pid), "comm")) as stream:
                return stream.read().strip()
        except OSError as error:
            raise MeasurementInvalid(f"cannot read comm for pid {pid}: {error}")

    def stat(self, pid: int) -> ProcessStat:
        try:
            with open(os.path.join(self.root, str(pid), "stat")) as stream:
                return parse_process_stat(stream.read())
        except OSError as error:
            raise MeasurementInvalid(f"cannot read stat for pid {pid}: {error}")

    def identity(self, pid: int) -> "ProcIdentity":
        return ProcIdentity(self.root, pid)


class ProcIdentity:
    """An open /proc/PID directory that cannot drift onto a reused pathname."""

    def __init__(self, proc_root: str, pid: int):
        self.pid = pid
        try:
            self.fd = os.open(
                os.path.join(proc_root, str(pid)),
                os.O_RDONLY | getattr(os, "O_DIRECTORY", 0),
            )
        except OSError as error:
            raise MeasurementInvalid(f"cannot open /proc identity for pid {pid}: {error}")

    def read(self, name: str) -> str:
        if self.fd is None:
            raise MeasurementInvalid(f"/proc identity for pid {self.pid} is closed")
        try:
            return _read_proc_member(self.fd, name)
        except (OSError, UnicodeError) as error:
            raise MeasurementInvalid(
                f"cannot read {name} for pinned pid {self.pid}: {error}"
            ) from error

    def stat(self) -> ProcessStat:
        value = parse_process_stat(self.read("stat"))
        if value.pid != self.pid:
            raise MeasurementInvalid(
                f"pinned proc identity says pid {value.pid}, expected {self.pid}"
            )
        return value

    def comm(self) -> str:
        return self.read("comm").strip()

    def cgroup(self) -> str:
        return _unified_cgroup_from_raw(self.read("cgroup"), f"pinned pid {self.pid} cgroup")

    def close(self) -> None:
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


def _read_unified_cgroup(path: str) -> str:
    with open(path) as stream:
        rows = [line.rstrip("\n") for line in stream]
    unified = [line.partition("::")[2] for line in rows if "::" in line]
    if len(unified) != 1 or not unified[0].startswith("/"):
        raise MeasurementInvalid(f"{path} has no single cgroup-v2 membership: {rows}")
    return unified[0]


def _read_proc_member(pid_dir_fd: int, name: str) -> str:
    """Read one file through an already-open /proc/PID directory identity."""
    fd = os.open(name, os.O_RDONLY, dir_fd=pid_dir_fd)
    try:
        chunks = []
        while True:
            chunk = os.read(fd, 64 * 1024)
            if not chunk:
                return b"".join(chunks).decode()
            chunks.append(chunk)
    finally:
        os.close(fd)


def _unified_cgroup_from_raw(raw: str, source: str) -> str:
    rows = raw.splitlines()
    unified = [line.partition("::")[2] for line in rows if "::" in line]
    if len(unified) != 1 or not unified[0].startswith("/"):
        raise MeasurementInvalid(f"{source} has no single cgroup-v2 membership: {rows}")
    return unified[0]


class CgroupAudit:
    """Read-only membership and run-level cpu.stat accounting."""

    def __init__(self, path: str, proc_root: str = "/proc", cgroup_root: str = "/sys/fs/cgroup"):
        self.path = os.path.realpath(path)
        self.proc_root = proc_root
        self.cgroup_root = os.path.realpath(cgroup_root)
        relative = os.path.relpath(self.path, self.cgroup_root)
        if relative == ".." or relative.startswith(f"..{os.sep}"):
            raise MeasurementInvalid(f"run cgroup {path} is outside {cgroup_root}")
        self.relative = "/" if relative == "." else "/" + relative
        self.observed: dict[tuple[int, int], dict] = {}
        self.observed_lock = threading.Lock()

    def observe(self, pid: int, role: str) -> ProcessStat:
        # Open /proc/PID once and perform every read relative to that directory.
        # A pathname re-open could silently bind the cgroup row and stat row to
        # different processes if the first process exits and the PID is reused.
        # The directory fd pins the procfs identity; a disappearing member fails
        # this measurement instead of being relabelled as its replacement.
        identity = ProcIdentity(self.proc_root, pid)
        try:
            return self.observe_identity(identity, role)
        finally:
            identity.close()

    def observe_identity(self, identity: ProcIdentity, role: str) -> ProcessStat:
        before = identity.stat()
        membership = identity.cgroup()
        comm = identity.comm()
        after = identity.stat()
        if before.start_time_ticks != after.start_time_ticks:
            raise MeasurementInvalid(
                f"{role} pid {identity.pid} changed identity while cgroup membership was sampled"
            )
        if membership != self.relative and not membership.startswith(self.relative.rstrip("/") + "/"):
            raise MeasurementInvalid(
                f"{role} pid {identity.pid} is outside run cgroup {self.relative}: {membership}"
            )
        key = (identity.pid, before.start_time_ticks)
        with self.observed_lock:
            existing = self.observed.get(key)
            if existing is not None and existing["role"] != role:
                raise MeasurementInvalid(
                    f"process {key} was observed as both {existing['role']} and {role}"
                )
            self.observed[key] = {"role": role, "comm": comm}
        return before

    def cpu_snapshot(self) -> dict[str, int]:
        with open(os.path.join(self.path, "cpu.stat")) as stream:
            return parse_cpu_stat(stream.read())

    def live_pids(self) -> list[int]:
        try:
            with open(os.path.join(self.path, "cgroup.procs")) as stream:
                return sorted(int(value) for value in stream.read().split())
        except (OSError, ValueError) as error:
            raise MeasurementInvalid(f"cannot read run cgroup.procs: {error}")

    def record(self) -> dict:
        with self.observed_lock:
            observed = dict(self.observed)
        return {
            "path": self.relative,
            "observed": [
                {"pid": pid, "pid_start_time_ticks": start, **observed[(pid, start)]}
                for pid, start in sorted(observed)
            ],
            "live_pids": self.live_pids(),
            "cpu_stat": self.cpu_snapshot(),
        }


class FirecrackerFaultProbe:
    """Minor/major faults during one render, bound to an exact VMM process."""

    def __init__(
        self,
        proc_reader: ProcReader,
        cgroup_audit: CgroupAudit,
        trace_marker: Optional[Callable[[int], None]] = None,
    ):
        self.proc = proc_reader
        self.audit = cgroup_audit
        self.trace_marker = trace_marker
        self.before: Optional[ProcessStat] = None
        self.identity: Optional[ProcIdentity] = None
        self.trace_open = False

    def begin(self, fcvm_pid: int) -> None:
        self.audit.observe(fcvm_pid, "fcvm")
        children = self.proc.children_of(fcvm_pid)
        named = []
        observed = []
        try:
            for pid in children:
                identity = self.proc.identity(pid)
                try:
                    comm = identity.comm()
                    self.audit.observe_identity(identity, comm or "unnamed-child")
                    observed.append((pid, comm))
                    if comm == "firecracker":
                        named.append(identity)
                        identity = None
                finally:
                    if identity is not None:
                        identity.close()
            if len(named) != 1:
                raise MeasurementInvalid(
                    f"fcvm pid {fcvm_pid} has {len(named)} firecracker children; "
                    f"expected exactly one: {observed}"
                )
            self.identity = named[0]
            self.before = self.identity.stat()
            if self.trace_marker is not None:
                self.trace_marker(self.before.pid)
                self.trace_open = True
        except BaseException:
            for identity in named:
                identity.close()
            self.identity = None
            self.before = None
            raise

    def finish(self) -> dict:
        if self.before is None or self.identity is None:
            raise MeasurementInvalid("Firecracker fault probe finish called before begin")
        try:
            if self.trace_open:
                self.trace_marker(self.before.pid)
                self.trace_open = False
            after = self.identity.stat()
            if after.start_time_ticks != self.before.start_time_ticks:
                raise MeasurementInvalid(
                    f"Firecracker pid {after.pid} identity changed from "
                    f"{self.before.start_time_ticks} to {after.start_time_ticks}"
                )
            minor = after.minor_faults - self.before.minor_faults
            major = after.major_faults - self.before.major_faults
            if minor < 0 or major < 0:
                raise MeasurementInvalid(
                    f"Firecracker fault counters moved backwards: minor={minor} major={major}"
                )
            return {
                "pid": after.pid,
                "pid_start_time_ticks": after.start_time_ticks,
                "minor_faults": minor,
                "major_faults": major,
                "before": {
                    "minor_faults": self.before.minor_faults,
                    "major_faults": self.before.major_faults,
                },
                "after": {
                    "minor_faults": after.minor_faults,
                    "major_faults": after.major_faults,
                },
                "scope": (
                    "Firecracker process endpoint-ready through artifact return; "
                    "all process VMAs, not guest-RAM-filtered and not UFFD events"
                ),
            }
        finally:
            self.identity.close()
            self.identity = None


# ---------------------------------------------------------------- output and gates


def _fsync_directory(path: str) -> None:
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_json_exclusive(path: str, value) -> None:
    """Atomically create one JSON artifact; never replace an earlier run."""
    directory = os.path.dirname(os.path.abspath(path))
    os.makedirs(directory, exist_ok=True)
    temp = os.path.join(directory, f".{os.path.basename(path)}.{uuid.uuid4().hex}.tmp")
    try:
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        with os.fdopen(fd, "wb") as stream:
            stream.write(json.dumps(value, indent=2, sort_keys=True, allow_nan=False).encode())
            stream.write(b"\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temp, path)  # atomic and fails with FileExistsError
        _fsync_directory(directory)
    finally:
        try:
            os.unlink(temp)
        except FileNotFoundError:
            pass


class JsonlSink:
    def __init__(self, path: str):
        directory = os.path.dirname(os.path.abspath(path))
        os.makedirs(directory, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        self.directory = directory
        self.stream = os.fdopen(fd, "w", buffering=1)
        self.lock = threading.Lock()
        _fsync_directory(directory)

    def write(self, value) -> None:
        encoded = json.dumps(value, sort_keys=True, allow_nan=False)
        with self.lock:
            self.stream.write(encoded + "\n")
            self.stream.flush()

    def sync(self) -> None:
        with self.lock:
            self.stream.flush()
            os.fsync(self.stream.fileno())

    def close(self) -> None:
        if not self.stream.closed:
            self.sync()
            self.stream.close()
            _fsync_directory(self.directory)

    def __enter__(self):
        return self

    def __exit__(self, _type, _value, _traceback):
        self.close()


class HostSampleSource:
    """Injectable boundary for one complete host/cgroup accounting sample."""

    def __init__(
        self,
        proc_stat: str = "/proc/stat",
        loadavg: str = "/proc/loadavg",
        pressure_root: str = "/proc/pressure",
        meminfo: str = "/proc/meminfo",
    ):
        self.proc_stat = proc_stat
        self.loadavg = loadavg
        self.pressure_root = pressure_root
        self.meminfo = meminfo

    def capture(self, audits: dict[str, CgroupAudit], phase: dict) -> dict:
        captured_wall_ns = time.time_ns()
        captured_monotonic_ns = time.monotonic_ns()
        cgroups = {}
        seen_pids = {}
        for name, audit in sorted(audits.items()):
            pids = audit.live_pids()
            if name != "run":
                for pid in pids:
                    prior = seen_pids.setdefault(pid, name)
                    if prior != name:
                        raise MeasurementInvalid(
                            f"pid {pid} appears in both {prior} and {name} leaf cgroups"
                        )
            cgroups[name] = {
                "path": audit.relative,
                "live_pids": pids,
                "cpu_stat": audit.cpu_snapshot(),
            }
        sample = {
            "schema": "fcvm.chromium.reqscale.host-sample.v1",
            "captured_wall_ns": captured_wall_ns,
            "captured_monotonic_ns": captured_monotonic_ns,
            "phase": dict(phase),
            "proc_stat": read_machine_proc_stat(self.proc_stat),
            "loadavg": read_loadavg(self.loadavg),
            "pressure": read_psi(self.pressure_root),
            "meminfo": read_meminfo(self.meminfo),
            "cgroups": cgroups,
        }
        sample["completed_wall_ns"] = time.time_ns()
        sample["completed_monotonic_ns"] = time.monotonic_ns()
        return sample


class RunPhase:
    def __init__(self):
        self.lock = threading.Lock()
        self.value = {"name": "setup", "burst_id": None}

    def set(self, name: str, burst_id: Optional[str] = None) -> None:
        with self.lock:
            self.value = {"name": name, "burst_id": burst_id}

    def snapshot(self) -> dict:
        with self.lock:
            return dict(self.value)


class HostSampler:
    """Continuous absolute-deadline sampler with durable per-sample writes."""

    def __init__(
        self,
        sink: JsonlSink,
        audits: dict[str, CgroupAudit],
        phase_provider: Callable[[], dict],
        interval_ns: int,
        source: Optional[HostSampleSource] = None,
    ):
        if interval_ns != round(SAMPLE_INTERVAL_SECONDS * 1_000_000_000):
            raise MeasurementInvalid("host sample interval must be exactly 5 seconds")
        self.sink = sink
        self.audits = audits
        self.phase_provider = phase_provider
        self.interval_ns = interval_ns
        self.source = source or HostSampleSource()
        self.stop_event = threading.Event()
        self.thread = None
        self.error = None
        self.error_lock = threading.Lock()
        self.origin_ns = None
        self.samples = 0

    def _run(self) -> None:
        assert self.origin_ns is not None
        index = 0
        try:
            while True:
                deadline_ns = self.origin_ns + index * self.interval_ns
                remaining_s = (deadline_ns - time.monotonic_ns()) / 1_000_000_000
                if remaining_s > 0 and self.stop_event.wait(remaining_s):
                    return
                if self.stop_event.is_set():
                    return
                sample = self.source.capture(self.audits, self.phase_provider())
                sample.update(
                    sample_index=index,
                    scheduled_monotonic_ns=deadline_ns,
                    launch_lag_ms=(sample["captured_monotonic_ns"] - deadline_ns) / 1_000_000,
                )
                self.sink.write(sample)
                self.sink.sync()
                self.samples += 1
                index += 1
        except BaseException as error:
            with self.error_lock:
                self.error = error
            self.stop_event.set()

    def start(self) -> None:
        if self.thread is not None:
            raise MeasurementInvalid("host sampler started twice")
        self.origin_ns = time.monotonic_ns()
        self.thread = threading.Thread(
            target=self._run, name="reqscale-host-sampler", daemon=False
        )
        self.thread.start()

    def check(self) -> None:
        with self.error_lock:
            error = self.error
        if error is not None:
            raise MeasurementInvalid(
                f"host sampler failed: {type(error).__name__}: {error}"
            ) from error

    def stop(self, timeout_s: float = 15.0) -> dict:
        if self.thread is None:
            return {"samples": 0, "started": False}
        stop_requested_ns = time.monotonic_ns()
        self.stop_event.set()
        self.thread.join(timeout_s)
        if self.thread.is_alive():
            raise MeasurementInvalid("host sampler thread survived stop")
        self.check()
        terminal = self.source.capture(self.audits, self.phase_provider())
        terminal.update(
            sample_index=self.samples,
            scheduled_monotonic_ns=None,
            launch_lag_ms=None,
            terminal=True,
        )
        self.sink.write(terminal)
        self.samples += 1
        self.sink.sync()
        return {
            "samples": self.samples,
            "periodic_samples": self.samples - 1,
            "terminal_sample": True,
            "started": True,
            "origin_monotonic_ns": self.origin_ns,
            "interval_ns": self.interval_ns,
            "stop_requested_monotonic_ns": stop_requested_ns,
        }


def evaluate_trace_perturbation(summaries: list[dict], max_delta_pct: float) -> dict:
    """Gate each predeclared traced/control pair; no unpaired aggregate."""
    if not math.isfinite(max_delta_pct) or max_delta_pct < 0:
        raise MeasurementInvalid("trace perturbation limit must be finite and nonnegative")
    grouped: dict[str, list[dict]] = {}
    for summary in summaries:
        pair_id = summary.get("trace_pair_id")
        if pair_id:
            grouped.setdefault(pair_id, []).append(summary)
    if not grouped:
        raise MeasurementInvalid("no matched trace perturbation pairs were recorded")
    rows = []
    failures = []
    for pair_id in sorted(grouped):
        pair = grouped[pair_id]
        controls = [item for item in pair if item.get("traced") is False]
        traces = [item for item in pair if item.get("traced") is True]
        if len(controls) != 1 or len(traces) != 1:
            raise MeasurementInvalid(
                f"trace pair {pair_id} is not matched: controls={len(controls)} traced={len(traces)}"
            )
        control, traced = controls[0], traces[0]
        shape_fields = ("target_rps", "request_plan_count", "request_plan_sha256")
        if any(control.get(field) != traced.get(field) for field in shape_fields):
            raise MeasurementInvalid(f"trace pair {pair_id} has mismatched cells")
        for item in (control, traced):
            if (
                item.get("total_artifact_completed") != item.get("request_plan_count")
                or item.get("total_drained") != item.get("request_plan_count")
                or item.get("total_cleanup_confirmed") != item.get("request_plan_count")
                or item.get("failed", 0) != 0
            ):
                raise MeasurementInvalid(f"trace pair {pair_id} has incomplete requests")
        for backend in ("file", "uffd"):
            control_cell = control.get("backends", {}).get(backend, {})
            traced_cell = traced.get("backends", {}).get(backend, {})
            base = control_cell.get("artifact_latency_ms", {}).get("median")
            measured = traced_cell.get("artifact_latency_ms", {}).get("median")
            if (
                not isinstance(base, (int, float)) or isinstance(base, bool)
                or not math.isfinite(base) or base <= 0
                or not isinstance(measured, (int, float)) or isinstance(measured, bool)
                or not math.isfinite(measured) or measured <= 0
            ):
                raise MeasurementInvalid(
                    f"trace pair {pair_id} {backend} has no positive median"
                )
            delta = (measured - base) * 100.0 / base
            row = {
                "trace_pair_id": pair_id,
                "backend": backend,
                "target_rps": control["target_rps"],
                "control_median_ms": base,
                "traced_median_ms": measured,
                "median_delta_pct": delta,
                "limit_pct": max_delta_pct,
                "passed": abs(delta) <= max_delta_pct,
            }
            rows.append(row)
            if not row["passed"]:
                failures.append(row)
    verdict = {"passed": not failures, "limit_pct": max_delta_pct, "pairs": rows}
    if failures:
        raise MeasurementInvalid(
            f"fault tracing perturbation exceeded {max_delta_pct}% in {len(failures)} pair(s)",
            verdict,
        )
    return verdict


# ---------------------------------------------------------------- bpftrace fault cost


def _numeric_pid_map(value, name: str) -> dict[int, int]:
    if not isinstance(value, dict):
        raise MeasurementInvalid(f"bpftrace {name} is not a map")
    out = {}
    for pid_text, count in value.items():
        try:
            pid = int(pid_text)
        except (TypeError, ValueError) as error:
            raise MeasurementInvalid(f"bpftrace {name} has non-pid key {pid_text!r}") from error
        if pid <= 0 or str(pid) != pid_text or pid in out:
            raise MeasurementInvalid(f"bpftrace {name} has invalid pid key {pid_text!r}")
        if not isinstance(count, int) or isinstance(count, bool) or count < 0:
            raise MeasurementInvalid(f"bpftrace {name}[{pid}] is not a nonnegative integer")
        out[pid] = count
    return out


def _strict_trace_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise MeasurementInvalid(f"bpftrace JSON has duplicate key {key!r}")
        value[key] = item
    return value


def _reject_trace_constant(value):
    raise MeasurementInvalid(f"bpftrace JSON has non-standard constant {value}")


def parse_fault_trace(lines: Iterable[str]) -> dict:
    """Parse bpftrace ``-f json`` output and prove every aggregate reconciles."""
    ready = False
    maps: dict[str, dict] = {}
    histogram = None
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            event = json.loads(
                line,
                object_pairs_hook=_strict_trace_object,
                parse_constant=_reject_trace_constant,
            )
        except (json.JSONDecodeError, ValueError) as error:
            raise MeasurementInvalid(f"bpftrace output line {number} is not JSON: {error}")
        if not isinstance(event, dict):
            raise MeasurementInvalid(f"bpftrace output line {number} is not an object")
        if event.get("type") == "printf" and event.get("data") == "REQSCALE_TRACE_READY\\n":
            ready = True
        if event.get("type") == "map":
            data = event.get("data")
            if not isinstance(data, dict) or len(data) != 1:
                raise MeasurementInvalid(f"malformed bpftrace map event on line {number}")
            name, value = next(iter(data.items()))
            if name in maps:
                raise MeasurementInvalid(f"bpftrace printed {name} more than once")
            maps[name] = _numeric_pid_map(value, name)
        if event.get("type") == "hist":
            data = event.get("data")
            if not isinstance(data, dict) or set(data) != {"@latency_ns"}:
                raise MeasurementInvalid(f"unexpected bpftrace histogram event on line {number}")
            if histogram is not None:
                raise MeasurementInvalid("bpftrace printed @latency_ns more than once")
            raw_hist = data["@latency_ns"]
            if not isinstance(raw_hist, dict):
                raise MeasurementInvalid("bpftrace @latency_ns is not keyed by pid")
            histogram = {}
            for pid_text, buckets in raw_hist.items():
                try:
                    pid = int(pid_text)
                except (TypeError, ValueError) as error:
                    raise MeasurementInvalid(f"histogram has non-pid key {pid_text!r}") from error
                if pid <= 0 or str(pid) != pid_text or pid in histogram:
                    raise MeasurementInvalid(f"histogram has invalid pid key {pid_text!r}")
                if not isinstance(buckets, list):
                    raise MeasurementInvalid(f"histogram for pid {pid} is not a list")
                checked = []
                for bucket in buckets:
                    if (
                        not isinstance(bucket, dict)
                        or set(bucket) != {"min", "max", "count"}
                        or not all(
                            isinstance(bucket[key], int)
                            and not isinstance(bucket[key], bool)
                            for key in bucket
                        )
                        or bucket["min"] < 0
                        or bucket["max"] < bucket["min"]
                        or bucket["count"] < 0
                    ):
                        raise MeasurementInvalid(f"malformed histogram bucket for pid {pid}: {bucket}")
                    checked.append(dict(bucket))
                if any(
                    current["min"] <= previous["max"]
                    for previous, current in zip(checked, checked[1:])
                ):
                    raise MeasurementInvalid(
                        f"histogram buckets overlap or are unordered for pid {pid}"
                    )
                histogram[pid] = checked

    if not ready:
        raise MeasurementInvalid("bpftrace never emitted REQSCALE_TRACE_READY")
    required_markers = {"@opened", "@closed"}
    missing = required_markers - set(maps)
    if missing:
        raise MeasurementInvalid(f"bpftrace output is missing marker maps {sorted(missing)}")
    # A legitimate interval can contain zero handle_mm_fault calls. bpftrace
    # does not materialize a map (or histogram key) until its first update, so
    # absent event maps mean zero only for a pid whose open+close markers exist.
    for name in ("@entered", "@completed", "@total_ns"):
        maps.setdefault(name, {})
    histogram = histogram or {}
    event_maps = {"@entered", "@completed", "@total_ns"}
    stray = set().union(*(set(maps[name]) for name in event_maps), set(histogram)) - set(maps["@opened"])
    if stray:
        raise MeasurementInvalid(f"bpftrace recorded unmarked pids {sorted(stray)}")
    pids = set(maps["@opened"]) | set(maps["@closed"])
    processes = {}
    for pid in sorted(pids):
        values = {
            "@opened": maps["@opened"].get(pid),
            "@closed": maps["@closed"].get(pid),
            "@entered": maps["@entered"].get(pid, 0),
            "@completed": maps["@completed"].get(pid, 0),
            "@total_ns": maps["@total_ns"].get(pid, 0),
        }
        buckets = histogram.get(pid, [])
        if values["@opened"] is None or values["@closed"] is None:
            raise MeasurementInvalid(f"bpftrace marker aggregates are incomplete for pid {pid}")
        if values["@opened"] != 1 or values["@closed"] != 1:
            raise MeasurementInvalid(
                f"bpftrace pid {pid} marker mismatch: opened={values['@opened']} "
                f"closed={values['@closed']}"
            )
        if values["@entered"] != values["@completed"]:
            raise MeasurementInvalid(
                f"bpftrace pid {pid} entered {values['@entered']} faults but completed "
                f"{values['@completed']}"
            )
        histogram_count = sum(bucket["count"] for bucket in buckets)
        if histogram_count != values["@completed"]:
            raise MeasurementInvalid(
                f"bpftrace pid {pid} histogram counts {histogram_count}, expected "
                f"{values['@completed']}"
            )
        processes[pid] = {
            "count": values["@completed"],
            "total_ns": values["@total_ns"],
            "histogram": buckets,
        }
    return {"ready": True, "processes": processes}


def _is_trace_ready_line(line: str) -> bool:
    """Recognize only bpftrace's exact JSON readiness event."""
    try:
        event = json.loads(
            line,
            object_pairs_hook=_strict_trace_object,
            parse_constant=_reject_trace_constant,
        )
    except (json.JSONDecodeError, ValueError, MeasurementInvalid):
        return False
    return (
        isinstance(event, dict)
        and event.get("type") == "printf"
        and event.get("data") == "REQSCALE_TRACE_READY\\n"
    )


def join_fault_trace(records: list[dict], trace: dict) -> None:
    """Join aggregate fault cost to exact per-request Firecracker identities."""
    identities: dict[int, int] = {}
    for record in records:
        faults = record.get("firecracker_process_faults_ready_to_artifact")
        if not isinstance(faults, dict):
            if record.get("ok"):
                raise MeasurementInvalid(
                    f"successful request {record.get('request_id')} has no Firecracker fault sample"
                )
            continue
        pid = faults.get("pid")
        start = faults.get("pid_start_time_ticks")
        if not isinstance(pid, int) or not isinstance(start, int):
            raise MeasurementInvalid(f"request has invalid Firecracker identity: {faults}")
        if pid in identities:
            if identities[pid] != start:
                raise MeasurementInvalid(
                    f"Firecracker pid {pid} was reused inside one traced window: "
                    f"{identities[pid]} then {start}"
                )
            raise MeasurementInvalid(f"Firecracker identity ({pid}, {start}) served two requests")
        identities[pid] = start
        measurement = trace.get("processes", {}).get(pid)
        if measurement is None:
            raise MeasurementInvalid(
                f"request {record.get('request_id')} has no handle_mm_fault aggregate for pid {pid}"
            )
        record["firecracker_process_handle_mm_fault_ready_to_artifact"] = {
            **measurement,
            "scope": (
                "all Firecracker-process VMAs; not filtered to guest RAM and not an "
                "UFFD-event count"
            ),
        }
    extra = set(trace.get("processes", {})) - set(identities)
    if extra:
        raise MeasurementInvalid(
            f"handle_mm_fault aggregates have no exact request identity: pids {sorted(extra)}"
        )


class BpftraceFaultTracer:
    """One actively-drained bpftrace process for one traced burst."""

    def __init__(
        self,
        backend_cgroup_paths: dict[str, str],
        driver_cgroup_path: str,
        harness_pid: int,
        out_dir: str,
        burst_id: str,
        cgroup_audit: CgroupAudit,
        bpftrace: str = "bpftrace",
    ):
        if set(backend_cgroup_paths) != {"file", "uffd"}:
            raise MeasurementInvalid("bpftrace requires exact FILE and UFFD cgroup paths")
        for path in (*backend_cgroup_paths.values(), driver_cgroup_path):
            if not os.path.isabs(path) or not path.startswith("/sys/fs/cgroup/"):
                raise MeasurementInvalid(f"unsafe cgroup path for bpftrace: {path}")
            if not re.fullmatch(r"[A-Za-z0-9_./@-]+", path):
                raise MeasurementInvalid(f"cgroup path contains unsupported characters: {path}")
        if harness_pid <= 0:
            raise MeasurementInvalid(f"invalid harness pid {harness_pid}")
        self.backend_cgroup_paths = dict(backend_cgroup_paths)
        self.driver_cgroup_path = driver_cgroup_path
        self.harness_pid = harness_pid
        self.out_dir = out_dir
        self.burst_id = burst_id
        self.audit = cgroup_audit
        self.bpftrace = bpftrace
        self.process = None
        self.lines: list[str] = []
        self.reader_woke = threading.Event()
        self.ready_seen = False
        self.reader_error = None
        self.reader_state_lock = threading.Lock()
        self.stdout_thread = None
        self.stderr_thread = None
        self.stdout_stream = None
        self.stderr_stream = None
        self.program_path = None
        self.raw_path = None
        self.stderr_path = None

    def _generated_program(self) -> str:
        with open(os.path.join(HERE, "faulttrace.bt")) as stream:
            program = stream.read()
        program = program.replace(
            "__FILE_CGROUP_PATH__", self.backend_cgroup_paths["file"]
        )
        program = program.replace(
            "__UFFD_CGROUP_PATH__", self.backend_cgroup_paths["uffd"]
        )
        program = program.replace("__HARNESS_PID__", str(self.harness_pid))
        if re.search(r"__[A-Z_]+__", program):
            raise MeasurementInvalid("faulttrace template substitution was incomplete")
        path = os.path.join(self.out_dir, f"{self.burst_id}.faulttrace.bt")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        with os.fdopen(fd, "w") as stream:
            stream.write(program)
            stream.flush()
            os.fsync(stream.fileno())
        _fsync_directory(self.out_dir)
        self.program_path = path
        return path

    def start(self, timeout_s: float = 30.0) -> None:
        program = self._generated_program()
        self.raw_path = os.path.join(self.out_dir, f"{self.burst_id}.bpftrace.jsonl")
        self.stderr_path = os.path.join(
            self.out_dir, f"{self.burst_id}.bpftrace.stderr"
        )
        try:
            self.stdout_stream = open(self.raw_path, "x", buffering=1)
            self.stderr_stream = open(self.stderr_path, "x", buffering=1)
            _fsync_directory(self.out_dir)
            self.process = subprocess.Popen(
                guarded_command(
                    self.driver_cgroup_path,
                    [self.bpftrace, "-q", "-f", "json", program],
                    self.harness_pid,
                ),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
            )
            def read_stdout():
                try:
                    for line in self.process.stdout:
                        self.lines.append(line.rstrip("\n"))
                        self.stdout_stream.write(line)
                        if _is_trace_ready_line(line):
                            with self.reader_state_lock:
                                self.ready_seen = True
                            self.reader_woke.set()
                except BaseException as error:
                    with self.reader_state_lock:
                        if self.reader_error is None:
                            self.reader_error = error
                finally:
                    # EOF and reader failures must wake start() immediately. Waiting
                    # the full readiness timeout would hide a tracer that already died.
                    self.reader_woke.set()

            def read_stderr():
                try:
                    for line in self.process.stderr:
                        self.stderr_stream.write(line)
                except BaseException as error:
                    with self.reader_state_lock:
                        if self.reader_error is None:
                            self.reader_error = error
                    self.reader_woke.set()

            self.stdout_thread = threading.Thread(target=read_stdout, name="reqscale-bpf-out")
            self.stderr_thread = threading.Thread(target=read_stderr, name="reqscale-bpf-err")
            self.stdout_thread.start()
            self.stderr_thread.start()
            woke = self.reader_woke.wait(timeout_s)
            with self.reader_state_lock:
                ready_seen = self.ready_seen
                reader_error = self.reader_error
            if not woke or not ready_seen:
                rc = self.process.poll()
                if reader_error is not None:
                    raise MeasurementInvalid(
                        f"bpftrace output reader failed before readiness: "
                        f"{type(reader_error).__name__}: {reader_error}"
                    ) from reader_error
                raise MeasurementInvalid(
                    f"bpftrace did not become ready within {timeout_s}s (exit={rc})"
                )
            self.audit.observe(self.process.pid, "bpftrace")
        except BaseException:
            self.abort()
            raise

    def marker(self, firecracker_pid: int) -> None:
        if self.process is None or self.process.poll() is not None:
            raise MeasurementInvalid("bpftrace marker used while tracer is not running")
        # Signal 0 performs only existence/permission checks. The generated
        # tracepoint program filters on this exact harness tgid and toggles the
        # target Firecracker pid; no signal is delivered.
        os.kill(firecracker_pid, 0)

    def stop(self, timeout_s: float = 30.0) -> dict:
        if self.process is None:
            raise MeasurementInvalid("bpftrace stop called before start")
        try:
            self.process.send_signal(signal.SIGINT)
        except ProcessLookupError:
            # Reap and inspect the process below.  Exiting between poll/kill is
            # not itself an error, but a nonzero terminal status still is.
            pass
        try:
            rc = self.process.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired as error:
            self.abort()
            raise MeasurementInvalid(f"bpftrace did not stop within {timeout_s}s") from error
        live_threads = []
        for thread in (self.stdout_thread, self.stderr_thread):
            if thread is None:
                live_threads.append("missing-reader")
                continue
            thread.join(timeout=10)
            if thread.is_alive():
                live_threads.append(thread.name)
        if live_threads:
            raise MeasurementInvalid(
                f"bpftrace reader threads did not stop after process exit: {live_threads}"
            )
        close_errors = []
        for stream in (self.stdout_stream, self.stderr_stream):
            if stream is None:
                close_errors.append("missing bpftrace artifact stream")
                continue
            if stream.closed:
                continue
            try:
                stream.flush()
                os.fsync(stream.fileno())
            except OSError as error:
                close_errors.append(f"{stream.name}: {error}")
            finally:
                try:
                    stream.close()
                except OSError as error:
                    close_errors.append(f"{stream.name} close: {error}")
        try:
            _fsync_directory(self.out_dir)
        except OSError as error:
            close_errors.append(f"{self.out_dir}: {error}")
        if close_errors:
            raise MeasurementInvalid(
                f"cannot durably close bpftrace artifacts: {close_errors}"
            )
        with self.reader_state_lock:
            reader_error = self.reader_error
        if reader_error is not None:
            raise MeasurementInvalid(
                f"bpftrace output reader failed: {type(reader_error).__name__}: {reader_error}"
            ) from reader_error
        if rc != 0:
            raise MeasurementInvalid(f"bpftrace exited with status {rc}")
        return parse_fault_trace(self.lines)

    def abort(self) -> None:
        cleanup_errors = []
        if self.process is not None and self.process.poll() is None:
            try:
                self.process.kill()
            except ProcessLookupError:
                pass
            except OSError as error:
                cleanup_errors.append(f"cannot SIGKILL bpftrace: {error}")
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                cleanup_errors.append("bpftrace survived SIGKILL")
            except OSError as error:
                cleanup_errors.append(f"cannot reap bpftrace: {error}")
        live_threads = []
        for thread in (self.stdout_thread, self.stderr_thread):
            if thread is not None:
                thread.join(timeout=10)
                if thread.is_alive():
                    live_threads.append(thread.name)
        if live_threads:
            cleanup_errors.append(
                f"bpftrace reader threads survived process exit: {live_threads}"
            )
        if not live_threads:
            for stream in (self.stdout_stream, self.stderr_stream):
                if stream is not None and not stream.closed:
                    try:
                        stream.flush()
                        os.fsync(stream.fileno())
                    except OSError as error:
                        cleanup_errors.append(f"{stream.name}: {error}")
                    finally:
                        try:
                            stream.close()
                        except OSError as error:
                            cleanup_errors.append(f"{stream.name} close: {error}")
            try:
                _fsync_directory(self.out_dir)
            except OSError as error:
                cleanup_errors.append(f"{self.out_dir}: {error}")
        if cleanup_errors:
            raise MeasurementInvalid(
                f"cannot fully stop and durably close bpftrace: {cleanup_errors}"
            )


# ---------------------------------------------------------------- production orchestration


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def harness_sha256() -> str:
    digest = hashlib.sha256(b"fcvm-reqscale-harness-v1\0")
    for name in (
        "reqscale.py", "reqscale_analyze.py", "reqbench.py", "cdpdrive.py",
        "render.py", "faulttrace.bt", "guardexec.py", "guardsupervise.py",
    ):
        encoded = name.encode()
        digest.update(len(encoded).to_bytes(4, "big"))
        digest.update(encoded)
        with open(os.path.join(HERE, name), "rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    return digest.hexdigest()


def _command(argv: list[str]) -> str:
    try:
        return subprocess.check_output(argv, text=True, stderr=subprocess.STDOUT).strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise MeasurementInvalid(f"provenance command failed: {argv}: {error}") from error


def parse_vm_process_rows(raw: str) -> list[dict]:
    """Return active fcvm/VMM rows from ``ps -eo stat=,comm=`` output."""
    matches = []
    for number, line in enumerate(raw.splitlines(), 1):
        if not line.strip():
            continue
        parts = line.strip().split(maxsplit=1)
        if len(parts) != 2:
            raise MeasurementInvalid(f"malformed ps process row {number}: {line!r}")
        state, comm = parts
        if state.startswith("Z"):
            continue
        if comm == "fcvm" or comm.startswith("firecracker") or comm.startswith("cloud-hypervis"):
            matches.append({"state": state, "comm": comm})
    return matches


def quiet_host_snapshot(
    loadavg_path: str = "/proc/loadavg", process_rows: Optional[str] = None,
) -> dict:
    """Fail-closed inputs for the repo's established quiet-host gate."""
    if process_rows is None:
        process_rows = _command(["ps", "-eo", "stat=,comm="])
    try:
        with open(loadavg_path) as stream:
            fields = stream.read().split()
        loadavg1 = float(fields[0])
    except (OSError, IndexError, ValueError) as error:
        raise MeasurementInvalid(f"cannot read quiet-host load from {loadavg_path}: {error}")
    if not math.isfinite(loadavg1) or loadavg1 < 0:
        raise MeasurementInvalid(f"quiet-host load is invalid: {loadavg1}")
    processes = parse_vm_process_rows(process_rows)
    return {
        "loadavg1": loadavg1,
        "loadavg1_limit": 2.0,
        "vm_process_count": len(processes),
        "vm_processes": processes,
    }


def snapshot_identity(data_root: str, snapshot_tag: str) -> dict:
    """Durable identity of the exact snapshot generation and runtime shape."""
    snapshots_root = os.path.realpath(os.path.join(data_root, "snapshots"))
    snapshot_dir = os.path.realpath(os.path.join(snapshots_root, snapshot_tag))
    if os.path.commonpath((snapshots_root, snapshot_dir)) != snapshots_root:
        raise MeasurementInvalid(f"snapshot tag escapes the snapshot root: {snapshot_tag!r}")
    config_path = os.path.join(snapshot_dir, "config.json")
    try:
        with open(config_path, "rb") as stream:
            raw = stream.read()
        config = strict_json_loads(raw, config_path)
    except OSError as error:
        raise MeasurementInvalid(f"cannot identify snapshot from {config_path}: {error}")
    if not isinstance(config, dict):
        raise MeasurementInvalid(f"snapshot config {config_path} is not an object")
    generation_id = config.get("generation_id")
    try:
        canonical_generation_id = str(uuid.UUID(generation_id))
    except (AttributeError, TypeError, ValueError) as error:
        raise MeasurementInvalid(
            f"snapshot config {config_path} has invalid generation_id"
        ) from error
    if canonical_generation_id != generation_id:
        raise MeasurementInvalid(
            f"snapshot config {config_path} has non-canonical generation_id"
        )
    metadata = config.get("metadata")
    required_top = ("created_at", "vm_id")
    if any(not isinstance(config.get(field), str) or not config[field] for field in required_top):
        raise MeasurementInvalid(f"snapshot config {config_path} has no generation identity")
    if not isinstance(metadata, dict):
        raise MeasurementInvalid(f"snapshot config {config_path} has no metadata object")
    required_shape = ("image", "vcpu", "memory_mib", "network_mode", "port_mappings")
    missing = [field for field in required_shape if field not in metadata]
    if missing:
        raise MeasurementInvalid(f"snapshot config {config_path} lacks shape fields {missing}")
    files = {}
    for field in ("memory_path", "vmstate_path", "disk_path"):
        value = config.get(field)
        if not isinstance(value, str) or not value:
            raise MeasurementInvalid(f"snapshot config {config_path} has no {field}")
        path = os.path.realpath(value)
        if path != snapshot_dir and not path.startswith(snapshot_dir + os.sep):
            raise MeasurementInvalid(f"snapshot {field} escapes its generation directory: {path}")
        try:
            stat = os.stat(path, follow_symlinks=False)
        except FileNotFoundError as error:
            raise MeasurementInvalid(f"snapshot artifact is missing: {path}") from error
        if not os.path.isfile(path):
            raise MeasurementInvalid(f"snapshot artifact is not a regular file: {path}")
        files[field] = {
            "path": os.path.relpath(path, snapshot_dir),
            "size": stat.st_size,
            "mtime_ns": stat.st_mtime_ns,
            "inode": stat.st_ino,
        }
    config_stat = os.stat(config_path, follow_symlinks=False)
    files["config"] = {
        "path": "config.json", "size": config_stat.st_size,
        "mtime_ns": config_stat.st_mtime_ns, "inode": config_stat.st_ino,
    }
    return {
        "tag": snapshot_tag,
        "generation_id": generation_id,
        "created_at": config["created_at"],
        "vm_id": config["vm_id"],
        "config_sha256": hashlib.sha256(raw).hexdigest(),
        "shape": {field: metadata[field] for field in required_shape},
        "files": files,
    }


class SnapshotGenerationLease:
    """Hold fcvm's shared tag lock for the complete mixed-backend run."""

    def __init__(self, data_root: str, snapshot_tag: str):
        _validate_snapshot_tag(snapshot_tag)
        self.data_root = data_root
        self.snapshot_tag = snapshot_tag
        self.path = os.path.join(data_root, "snapshots", f"{snapshot_tag}.lock")
        self.stream = None
        self.identity = None

    def __enter__(self) -> "SnapshotGenerationLease":
        try:
            self.stream = open(self.path, "a+")
            fcntl.flock(self.stream, fcntl.LOCK_SH)
            self.identity = snapshot_identity(self.data_root, self.snapshot_tag)
        except BaseException:
            self.close()
            raise
        return self

    def verify(self) -> dict:
        if self.stream is None or self.identity is None:
            raise MeasurementInvalid("snapshot generation lease is not held")
        current = snapshot_identity(self.data_root, self.snapshot_tag)
        if current != self.identity:
            raise MeasurementInvalid(
                "snapshot identity changed while its shared generation lease was held",
                {"before": self.identity, "after": current},
            )
        return current

    def close(self) -> None:
        if self.stream is not None:
            try:
                fcntl.flock(self.stream, fcntl.LOCK_UN)
            finally:
                self.stream.close()
                self.stream = None

    def __exit__(self, _type, _value, _traceback):
        self.close()


def collect_provenance(args, schedule: dict, snapshot: dict) -> dict:
    fcvm = os.path.abspath(args.fcvm)
    if not os.path.isfile(fcvm):
        raise MeasurementInvalid(f"fcvm binary is missing: {fcvm}")
    revision = _command(["git", "-C", REPO, "rev-parse", "HEAD"])
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise MeasurementInvalid(f"git returned invalid revision {revision!r}")
    dirty = _command(["git", "-C", REPO, "status", "--porcelain=v1", "--untracked-files=all"])
    if dirty:
        raise MeasurementInvalid(
            "benchmark source tree is dirty; commit the harness before measuring"
        )
    quiet = quiet_host_snapshot()
    if quiet["loadavg1"] > quiet["loadavg1_limit"] or quiet["vm_process_count"]:
        raise MeasurementInvalid(
            "quiet-host gate refused the run: "
            f"load={quiet['loadavg1']} vm-processes={quiet['vm_process_count']}",
            quiet,
        )
    return {
        "schema": "fcvm.chromium.reqscale.provenance.v1",
        "run_id": schedule["run_id"],
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "argv": list(sys.argv),
        "schedule_sha256": schedule_sha256(schedule),
        "source_revision": revision,
        "source_dirty": bool(dirty),
        "source_status_sha256": hashlib.sha256(dirty.encode()).hexdigest(),
        "harness_sha256": harness_sha256(),
        "fcvm_path": fcvm,
        "fcvm_sha256": sha256_file(fcvm),
        "fcvm_version": _command([fcvm, "--version"]),
        "host_control": {
            "chromium_path": args.control_chromium,
            "chromium_sha256": sha256_file(args.control_chromium),
            "chromium_version": _command([args.control_chromium, "--version"]),
            "url": args.control_url,
            "interval_seconds": CONTROL_INTERVAL_SECONDS,
            "timeout_seconds": args.control_timeout,
        },
        "snapshot": snapshot,
        "snapshot_generation_lease": {
            "path": os.path.abspath(args.snapshot_generation_lease.path),
            "mode": "shared",
            "held_from_identity_read_through_terminal_verification": True,
        },
        "host": {
            "hostname": platform.node(),
            "kernel": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            "cpu_count": os.cpu_count(),
            "quiet_gate": quiet,
        },
        "fault_trace": {
            "enabled": bool(args.trace_faults),
            "bpftrace_version": (
                _command([args.bpftrace, "--version"]) if args.trace_faults else None
            ),
            "max_median_delta_pct": args.max_trace_perturbation_pct,
            "scope": (
                "Firecracker process endpoint-ready through artifact return; all VMAs, "
                "not guest-RAM-filtered and not UFFD events"
            ),
        },
    }


class RunCgroups:
    """One empty accounting root with four exclusive process leaves."""

    LEAVES = ("driver", "control", "file", "uffd")

    def __init__(self, run_id: str, root: str = "/sys/fs/cgroup", proc_root: str = "/proc"):
        _validate_run_id(run_id)
        self.run_id = run_id
        self.root = os.path.realpath(root)
        self.proc_root = proc_root
        self.original_relative = _read_unified_cgroup(os.path.join(proc_root, "self", "cgroup"))
        self.original_path = os.path.realpath(
            os.path.join(self.root, self.original_relative.lstrip("/"))
        )
        if os.path.commonpath((self.root, self.original_path)) != self.root:
            raise MeasurementInvalid(
                f"original cgroup {self.original_relative} escapes root {self.root}"
            )
        self.name = f"fcvm-reqscale-{run_id}"
        if not CGROUP_NAME_RE.fullmatch(self.name):
            raise MeasurementInvalid(f"unsafe run cgroup name {self.name}")
        self.path = os.path.join(self.original_path, self.name)
        self.paths = {name: os.path.join(self.path, name) for name in self.LEAVES}
        self.audits = None
        self.entered = False

    def enter(self) -> dict[str, CgroupAudit]:
        if self.entered:
            raise MeasurementInvalid("run cgroups entered twice")
        os.mkdir(self.path, 0o755)
        created = []
        moved = False
        try:
            for name in self.LEAVES:
                os.mkdir(self.paths[name], 0o755)
                created.append(self.paths[name])
            with open(os.path.join(self.paths["driver"], "cgroup.procs"), "w") as stream:
                stream.write(f"{os.getpid()}\n")
            moved = True
            self.audits = {
                "run": CgroupAudit(self.path, self.proc_root, self.root),
                **{
                    name: CgroupAudit(path, self.proc_root, self.root)
                    for name, path in self.paths.items()
                },
            }
            self.audits["run"].observe(os.getpid(), "reqscale")
            self.audits["driver"].observe(os.getpid(), "reqscale")
            # cpu.stat on the empty parent is the recursive total; every leaf
            # must expose the same accounting interface before any child starts.
            for audit in self.audits.values():
                audit.cpu_snapshot()
            self.entered = True
            return self.audits
        except BaseException as enter_error:
            cleanup_errors = []
            if moved:
                try:
                    with open(os.path.join(self.original_path, "cgroup.procs"), "w") as stream:
                        stream.write(f"{os.getpid()}\n")
                except BaseException as move_error:
                    cleanup_errors.append(f"move-back: {type(move_error).__name__}: {move_error}")
            for path in reversed(created):
                try:
                    os.rmdir(path)
                except OSError as remove_error:
                    cleanup_errors.append(
                        f"rmdir {path}: {type(remove_error).__name__}: {remove_error}"
                    )
            try:
                os.rmdir(self.path)
            except OSError as remove_error:
                cleanup_errors.append(
                    f"rmdir {self.path}: {type(remove_error).__name__}: {remove_error}"
                )
            if cleanup_errors:
                raise MeasurementInvalid(
                    f"run cgroup enter failed and cleanup was incomplete: {enter_error}",
                    {"enter_error": f"{type(enter_error).__name__}: {enter_error}",
                     "cleanup_errors": cleanup_errors},
                ) from enter_error
            raise

    def leave(self) -> dict:
        if not self.entered:
            return {}
        final = None
        accounting_error = None
        try:
            final = {
                name: audit.record() for name, audit in sorted(self.audits.items())
            }
        except BaseException as error:
            accounting_error = error
        expected_members = {
            "run": [],
            "driver": [os.getpid()],
            "control": [],
            "file": [],
            "uffd": [],
        }
        unexpected = {} if final is None else {
            name: {"expected": expected_members[name], "actual": row["live_pids"]}
            for name, row in final.items()
            if row["live_pids"] != expected_members[name]
        }
        # Move the harness first so it can report a failed cleanup. Never rmdir a
        # cgroup that still owns an unaccounted workload process.
        try:
            with open(os.path.join(self.original_path, "cgroup.procs"), "w") as stream:
                stream.write(f"{os.getpid()}\n")
        except BaseException as move_error:
            raise MeasurementInvalid(
                f"cannot move reqscale back to its original cgroup: {move_error}",
                {"final_cgroups": final},
            ) from move_error
        self.entered = False
        cleanup_errors = []
        if accounting_error is not None or unexpected:
            # The harness is now outside the owned tree, so recursive cgroup.kill
            # cannot kill the reporter.  Use it even when the audit itself failed:
            # inability to enumerate members must never become permission to
            # leave an unknown workload running.
            try:
                with open(os.path.join(self.path, "cgroup.kill"), "w") as stream:
                    stream.write("1\n")
            except OSError as error:
                cleanup_errors.append(f"cgroup.kill {self.path}: {error}")
            deadline = time.monotonic() + 10.0
            while True:
                try:
                    survivors = {}
                    for name, audit in sorted(self.audits.items()):
                        live = audit.live_pids()
                        if live:
                            survivors[name] = live
                except BaseException as error:
                    cleanup_errors.append(
                        f"cannot verify run cgroups after cgroup.kill: "
                        f"{type(error).__name__}: {error}"
                    )
                    break
                if not survivors:
                    break
                if time.monotonic() >= deadline:
                    cleanup_errors.append(
                        f"run cgroup processes survived cgroup.kill: {survivors}"
                    )
                    break
                time.sleep(0.01)
        removal_errors = []
        for name in reversed(self.LEAVES):
            try:
                os.rmdir(self.paths[name])
            except OSError as error:
                removal_errors.append(
                    f"cannot remove {name} cgroup {self.paths[name]}: {error}"
                )
        try:
            os.rmdir(self.path)
        except OSError as error:
            removal_errors.append(f"cannot remove run cgroup {self.path}: {error}")
        cleanup_errors.extend(removal_errors)
        if accounting_error is not None:
            raise MeasurementInvalid(
                f"cannot audit the run cgroups before removal: {accounting_error}",
                {
                    "final_cgroups": final,
                    "accounting_error": (
                        f"{type(accounting_error).__name__}: {accounting_error}"
                    ),
                    "cleanup_errors": cleanup_errors,
                },
            ) from accounting_error
        if unexpected:
            raise MeasurementInvalid(
                f"run cgroups have unexpected final membership {unexpected}",
                {"final_cgroups": final, "cleanup_errors": cleanup_errors},
            )
        if cleanup_errors:
            raise MeasurementInvalid(
                f"cannot remove empty run cgroup tree: {cleanup_errors}",
                {"final_cgroups": final, "cleanup_errors": cleanup_errors},
            )
        return final


class UffdServe:
    """One guarded UFFD server, confined to the UFFD accounting leaf."""

    def __init__(self, args, audit: CgroupAudit, cgroup_path: str, log_dir: str):
        self.args = args
        self.audit = audit
        self.cgroup_path = cgroup_path
        self.log_path = os.path.join(log_dir, "uffd-serve.log")
        self.proc = None
        self.log_stream = None
        self.state_path = None
        self.state = None
        self.record = None

    def start(self) -> int:
        watch = reqbench.DirWatch(self.args.state_dir)
        try:
            baseline = {
                os.path.join(self.args.state_dir, name)
                for name in os.listdir(self.args.state_dir)
                if name.endswith(".json")
            }
            self.log_stream = open(self.log_path, "xb")
            self.proc = subprocess.Popen(
                guarded_command(
                    self.cgroup_path,
                    [self.args.fcvm, "snapshot", "serve", self.args.snapshot_tag],
                ),
                stdout=self.log_stream,
                stderr=self.log_stream,
                stdin=subprocess.DEVNULL,
                env=dict(os.environ, RUST_LOG=self.args.rust_log),
            )
            pidfd = reqbench.pidfd_open(self.proc.pid)
            poller = select.poll()
            poller.register(watch.fd, select.POLLIN)
            if pidfd is not None:
                poller.register(pidfd, select.POLLIN)
            deadline = time.monotonic() + self.args.timeout
            try:
                while True:
                    watch.drain()
                    for name in os.listdir(self.args.state_dir):
                        path = os.path.join(self.args.state_dir, name)
                        if not name.endswith(".json") or path in baseline:
                            continue
                        try:
                            with open(path) as stream:
                                state = json.load(stream)
                        except (OSError, ValueError):
                            continue
                        if (
                            state.get("pid") == self.proc.pid
                        ):
                            identity = self.audit.observe(self.proc.pid, "uffd-serve")
                            if state.get("pid_start_time") != identity.start_time_ticks:
                                raise MeasurementInvalid(
                                    f"serve state {path} has pid start "
                                    f"{state.get('pid_start_time')}, expected "
                                    f"{identity.start_time_ticks}"
                                )
                            config = state.get("config") or {}
                            if config.get("process_type") != "serve":
                                raise MeasurementInvalid(f"state {path} is not a serve process")
                            if config.get("snapshot_name") != self.args.snapshot_tag:
                                raise MeasurementInvalid(
                                    f"serve state names {config.get('snapshot_name')!r}, expected "
                                    f"{self.args.snapshot_tag!r}"
                                )
                            if config.get("uffd_mode") not in ("copy", "minor"):
                                raise MeasurementInvalid(f"serve state {path} has invalid UFFD mode")
                            self.state_path, self.state = path, state
                            self.record = {
                                "schema": RECORD_SCHEMA,
                                "kind": "uffd-serve",
                                "run_id": self.args.run_id,
                                "pid": self.proc.pid,
                                "pid_start_time_ticks": identity.start_time_ticks,
                                "state_path": os.path.relpath(path, self.args.data_root),
                                "uffd_mode": config["uffd_mode"],
                                "snapshot_tag": self.args.snapshot_tag,
                                "snapshot_generation_id": self.args.snapshot_identity[
                                    "generation_id"
                                ],
                                "snapshot_config_sha256": self.args.snapshot_identity[
                                    "config_sha256"
                                ],
                            }
                            return self.proc.pid
                    if self.proc.poll() is not None:
                        raise MeasurementInvalid(
                            f"UFFD serve exited with {self.proc.returncode} before publishing state"
                        )
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise MeasurementInvalid("UFFD serve did not publish state before deadline")
                    poller.poll(min(remaining, 0.25) * 1000)
            finally:
                if pidfd is not None:
                    os.close(pidfd)
        finally:
            watch.close()

    def stop(self) -> None:
        errors = []
        if self.proc is not None:
            if self.proc.poll() is None:
                try:
                    self.proc.send_signal(signal.SIGTERM)
                except ProcessLookupError:
                    # The process exited between poll() and kill(2).  wait()
                    # below still has to reap it and establish its status.
                    pass
                try:
                    self.proc.wait(timeout=self.args.teardown_timeout)
                except subprocess.TimeoutExpired:
                    try:
                        self.proc.kill()
                    except ProcessLookupError:
                        pass
                    try:
                        self.proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        errors.append("UFFD serve survived SIGKILL")
                    errors.append("UFFD serve required SIGKILL during shutdown")
            if self.proc.returncode is None:
                errors.append("UFFD serve has no terminal status after shutdown")
            elif self.proc.returncode not in (0, -signal.SIGTERM):
                errors.append(f"UFFD serve exited with status {self.proc.returncode}")
        if self.log_stream is not None and not self.log_stream.closed:
            try:
                self.log_stream.flush()
                os.fsync(self.log_stream.fileno())
            except OSError as error:
                errors.append(f"UFFD serve log sync failed: {error}")
            finally:
                try:
                    self.log_stream.close()
                except OSError as error:
                    errors.append(f"UFFD serve log close failed: {error}")
        if self.state_path and os.path.exists(self.state_path):
            errors.append(f"UFFD serve state survived shutdown: {self.state_path}")
        audit_failed = False
        try:
            remaining = self.audit.live_pids()
        except BaseException as error:
            remaining = []
            audit_failed = True
            errors.append(f"cannot verify UFFD cgroup cleanup: {error}")
        if remaining or audit_failed:
            if remaining:
                errors.append(
                    f"UFFD cgroup remained populated after serve shutdown: {remaining}"
                )
            fds = [reqbench.pidfd_open(pid) for pid in remaining]
            try:
                with open(os.path.join(self.cgroup_path, "cgroup.kill"), "w") as stream:
                    stream.write("1\n")
            except OSError as error:
                errors.append(f"UFFD cgroup.kill failed: {error}")
            if fds and not reqbench.wait_pidfds(fds, 10.0):
                errors.append(f"UFFD processes survived cgroup.kill: {remaining}")
            for fd in fds:
                if fd is not None:
                    os.close(fd)
            try:
                survivors = self.audit.live_pids()
            except BaseException as error:
                errors.append(f"cannot verify UFFD cgroup after cgroup.kill: {error}")
            else:
                if survivors:
                    errors.append(
                        f"UFFD cgroup is still populated after cgroup.kill: {survivors}"
                    )
        if errors:
            raise MeasurementInvalid("; ".join(errors), {"cleanup_errors": errors})


class NativeChromiumControl:
    """One persistent host-native Chromium process in the control cgroup."""

    def __init__(self, args, audit: CgroupAudit, cgroup_path: str, log_dir: str):
        self.args = args
        self.audit = audit
        self.cgroup_path = cgroup_path
        self.log_path = os.path.join(log_dir, "host-control-chromium.log")
        self.proc = None
        self.log_stream = None
        self.profile_dir = None
        self.endpoint = None

    def _drive_args(self, timeout_s: float) -> argparse.Namespace:
        return argparse.Namespace(
            cdp_host=self.endpoint,
            url=self.args.control_url,
            format=self.args.format,
            quality=self.args.quality,
            timeout=timeout_s,
            idle_wait_ms=0.0,
            out_prefix="",
            ws_url="",
            connect_retries=200,
            nav_timing=True,
            print_target=False,
            host_header="",
            render_module=os.path.join(HERE, "render.py"),
        )

    def drive(self) -> dict:
        import cdpdrive

        if self.proc is None or self.proc.poll() is not None or not self.endpoint:
            raise MeasurementInvalid("host control Chromium is not running")
        return cdpdrive.drive(self._drive_args(self.args.control_timeout))

    def start(self) -> dict:
        self.profile_dir = tempfile.mkdtemp(
            prefix=f"fcvm-reqscale-control-{self.args.run_id}-",
            dir=self.args.control_tmp_root,
        )
        watch = reqbench.DirWatch(self.profile_dir)
        pidfd = None
        try:
            self.log_stream = open(self.log_path, "xb")
            command = [
                self.args.control_chromium,
                "--headless=new",
                "--no-sandbox",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--remote-allow-origins=*",
                "--ignore-certificate-errors",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--window-size=1280,800",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-breakpad",
                "--disable-component-update",
                f"--user-data-dir={self.profile_dir}",
                "about:blank",
            ]
            self.proc = subprocess.Popen(
                supervised_command(self.cgroup_path, command),
                stdout=self.log_stream,
                stderr=self.log_stream,
                stdin=subprocess.DEVNULL,
            )
            pidfd = reqbench.pidfd_open(self.proc.pid)
            poller = select.poll()
            poller.register(watch.fd, select.POLLIN)
            if pidfd is not None:
                poller.register(pidfd, select.POLLIN)
            port_path = os.path.join(self.profile_dir, "DevToolsActivePort")
            deadline = time.monotonic() + self.args.timeout
            while True:
                watch.drain()
                try:
                    with open(port_path) as stream:
                        fields = stream.read().splitlines()
                    port = int(fields[0])
                    if not 1 <= port <= 65535 or len(fields) < 2 or not fields[1].startswith("/"):
                        raise ValueError("incomplete DevToolsActivePort")
                except (FileNotFoundError, IndexError, ValueError):
                    port = 0
                if port:
                    self.endpoint = f"127.0.0.1:{port}"
                    self.audit.observe(self.proc.pid, "host-control-chromium")
                    warmup_start_ns = time.monotonic_ns()
                    warmup = self.drive()
                    warmup_end_ns = time.monotonic_ns()
                    if not isinstance(warmup, dict) or not warmup.get("ok"):
                        raise MeasurementInvalid(
                            f"host control Chromium warmup failed: {warmup}"
                        )
                    return {
                        "schema": RECORD_SCHEMA,
                        "kind": "host-control-warmup",
                        "included_in_analysis": False,
                        "started_monotonic_ns": warmup_start_ns,
                        "artifact_monotonic_ns": warmup_end_ns,
                        "latency_ms": (warmup_end_ns - warmup_start_ns) / 1_000_000,
                        "result": warmup,
                    }
                if self.proc.poll() is not None:
                    raise MeasurementInvalid(
                        f"host control Chromium exited with {self.proc.returncode} before CDP ready"
                    )
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise MeasurementInvalid("host control Chromium did not publish CDP readiness")
                poller.poll(min(remaining, 0.25) * 1000)
        except BaseException as start_error:
            try:
                self.stop()
            except BaseException as cleanup_error:
                raise MeasurementInvalid(
                    f"host control start failed and cleanup also failed: {start_error}",
                    {
                        "start_error": f"{type(start_error).__name__}: {start_error}",
                        "cleanup_error": f"{type(cleanup_error).__name__}: {cleanup_error}",
                    },
                ) from start_error
            raise
        finally:
            if pidfd is not None:
                os.close(pidfd)
            watch.close()

    def stop(self) -> None:
        errors = []
        pids = []
        pidfds = []
        audit_failed = False
        if self.proc is not None:
            try:
                pids = self.audit.live_pids()
                pidfds = [reqbench.pidfd_open(pid) for pid in pids]
            except BaseException as error:
                audit_failed = True
                errors.append(f"cannot snapshot control cgroup before stop: {error}")
            if self.proc.poll() is None:
                try:
                    self.proc.send_signal(signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if pidfds:
                if not reqbench.wait_pidfds(pidfds, self.args.teardown_timeout):
                    errors.append(
                        f"control processes did not all exit after SIGTERM: {pids}"
                    )
            try:
                remaining = self.audit.live_pids()
            except BaseException as error:
                remaining = []
                audit_failed = True
                errors.append(f"cannot verify control cgroup after SIGTERM: {error}")
            if remaining or audit_failed:
                if remaining:
                    errors.append(
                        f"control cgroup remained populated after SIGTERM: {remaining}"
                    )
                remaining_fds = [reqbench.pidfd_open(pid) for pid in remaining]
                try:
                    with open(os.path.join(self.cgroup_path, "cgroup.kill"), "w") as stream:
                        stream.write("1\n")
                except OSError as error:
                    errors.append(f"control cgroup.kill failed: {error}")
                if remaining_fds and not reqbench.wait_pidfds(remaining_fds, 10.0):
                    errors.append(f"control processes survived cgroup.kill: {remaining}")
                for fd in remaining_fds:
                    if fd is not None:
                        os.close(fd)
                try:
                    survivors = self.audit.live_pids()
                except BaseException as error:
                    errors.append(
                        f"cannot verify control cgroup after cgroup.kill: {error}"
                    )
                else:
                    if survivors:
                        errors.append(
                            "control cgroup is still populated after cgroup.kill: "
                            f"{survivors}"
                        )
            for fd in pidfds:
                if fd is not None:
                    os.close(fd)
            try:
                self.proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                errors.append("control Chromium parent survived cleanup")
            if self.proc.returncode is None:
                errors.append("control Chromium parent has no terminal status")
            elif self.proc.returncode not in (0, -signal.SIGTERM, -signal.SIGKILL):
                errors.append(f"control Chromium exited with status {self.proc.returncode}")
        if self.log_stream is not None and not self.log_stream.closed:
            try:
                self.log_stream.flush()
                os.fsync(self.log_stream.fileno())
            except OSError as error:
                errors.append(f"control log sync failed: {error}")
            finally:
                try:
                    self.log_stream.close()
                except OSError as error:
                    errors.append(f"control log close failed: {error}")
        if self.profile_dir is not None and os.path.exists(self.profile_dir):
            try:
                shutil.rmtree(self.profile_dir)
            except OSError as error:
                errors.append(f"control profile cleanup failed: {error}")
        if errors:
            raise MeasurementInvalid("; ".join(errors), {"cleanup_errors": errors})


class ControlScheduler:
    """Drive persistent Chromium on global 10-second absolute deadlines."""

    def __init__(self, control: NativeChromiumControl, sink: JsonlSink, schedule: dict):
        interval_ns = schedule.get("interval_ns")
        if interval_ns != round(CONTROL_INTERVAL_SECONDS * 1_000_000_000):
            raise MeasurementInvalid("host control interval must be exactly 10 seconds")
        phase_ns = schedule.get("phase_offset_ns")
        if not isinstance(phase_ns, int) or isinstance(phase_ns, bool) or not 0 <= phase_ns < interval_ns:
            raise MeasurementInvalid("host control phase offset is invalid")
        self.control = control
        self.sink = sink
        self.interval_ns = interval_ns
        self.phase_ns = phase_ns
        self.stop_event = threading.Event()
        self.thread = None
        self.origin_ns = None
        self.records = []
        self.lock = threading.Lock()
        self.error = None

    def _run(self) -> None:
        assert self.origin_ns is not None
        index = 0
        try:
            while True:
                scheduled_ns = self.origin_ns + self.phase_ns + index * self.interval_ns
                remaining_s = (scheduled_ns - time.monotonic_ns()) / 1_000_000_000
                if remaining_s > 0 and self.stop_event.wait(remaining_s):
                    return
                if self.stop_event.is_set():
                    return
                actual_ns = time.monotonic_ns()
                record = {
                    "schema": RECORD_SCHEMA,
                    "kind": "host-control",
                    "control_index": index,
                    "scheduled_ns": scheduled_ns,
                    "actual_launch_ns": actual_ns,
                    "launch_lag_ms": (actual_ns - scheduled_ns) / 1_000_000,
                }
                try:
                    result = self.control.drive()
                    artifact_ns = time.monotonic_ns()
                    record.update(
                        artifact_ns=artifact_ns,
                        latency_ms=(artifact_ns - actual_ns) / 1_000_000,
                        ok=bool(result.get("ok")),
                        result=result,
                    )
                    if not record["ok"]:
                        record["error"] = result.get("error", "control drive returned ok=false")
                except Exception as error:
                    artifact_ns = time.monotonic_ns()
                    record.update(
                        artifact_ns=artifact_ns,
                        latency_ms=(artifact_ns - actual_ns) / 1_000_000,
                        ok=False,
                        error=f"{type(error).__name__}: {error}",
                    )
                self.sink.write(record)
                self.sink.sync()
                with self.lock:
                    self.records.append(record)
                index += 1
        except BaseException as error:
            with self.lock:
                self.error = error
            self.stop_event.set()

    def start(self) -> None:
        if self.thread is not None:
            raise MeasurementInvalid("host control scheduler started twice")
        self.origin_ns = time.monotonic_ns()
        self.thread = threading.Thread(
            target=self._run, name="reqscale-host-control", daemon=False
        )
        self.thread.start()

    def check(self) -> None:
        with self.lock:
            error = self.error
            failures = [record for record in self.records if not record.get("ok")]
        if error is not None:
            raise MeasurementInvalid(
                f"host control scheduler failed: {type(error).__name__}: {error}"
            ) from error
        if failures:
            raise MeasurementInvalid(
                f"host control arm has {len(failures)} failed requests", failures
            )

    def stop(self) -> dict:
        if self.thread is None:
            return {"started": False, "requests": 0}
        stop_requested_ns = time.monotonic_ns()
        self.stop_event.set()
        self.thread.join(self.control.args.control_timeout + 5.0)
        if self.thread.is_alive():
            cleanup_error = None
            try:
                self.control.stop()
            except BaseException as error:
                cleanup_error = error
            self.thread.join(10.0)
            if self.thread.is_alive():
                raise MeasurementInvalid(
                    "host control scheduler thread survived browser termination",
                    {"control_cleanup_error": repr(cleanup_error)},
                )
            raise MeasurementInvalid(
                "host control scheduler required browser termination to stop",
                {"control_cleanup_error": repr(cleanup_error)},
            )
        self.check()
        self.sink.sync()
        with self.lock:
            records = list(self.records)
        return {
            "started": True,
            "origin_monotonic_ns": self.origin_ns,
            "phase_offset_ns": self.phase_ns,
            "interval_ns": self.interval_ns,
            "requests": len(records),
            "stop_requested_monotonic_ns": stop_requested_ns,
        }


def _request_args(
    args, context: RequestContext, serve_pid: int, log_dir: str, probe,
) -> argparse.Namespace:
    return argparse.Namespace(
        serve_pid=serve_pid if context.backend == "uffd" else 0,
        snapshot_tag=args.snapshot_tag if context.backend == "file" else "",
        url=args.url,
        format=args.format,
        quality=args.quality,
        cdp_port=args.cdp_port,
        ws_url=args.ws_url,
        fcvm=args.fcvm,
        data_root=args.data_root,
        state_dir=args.state_dir,
        out_dir=log_dir,
        timeout=args.timeout,
        teardown_timeout=args.teardown_timeout,
        rust_log=args.rust_log,
        run_id=args.run_id,
        firecracker_fault_probe=probe,
        spawn_prefix=guard_prefix(args.cgroup_paths[context.backend]),
    )


def _make_request_fn(args, spec, serve_pid, log_dir, audits, tracer, global_base):
    def request(context: RequestContext) -> dict:
        marker = tracer.marker if tracer is not None else None
        probe = FirecrackerFaultProbe(ProcReader(), audits[context.backend], marker)
        request_args = _request_args(args, context, serve_pid, log_dir, probe)
        rep = global_base + context.request_index
        record = reqbench.run_cdp_request(request_args, rep, fast=True)
        record.update(
            schema=RECORD_SCHEMA,
            kind="request",
            run_id=args.run_id,
            burst_id=spec.burst_id,
            block_id=spec.block_id,
            cell_id=f"{context.backend}:r{format(spec.target_rps, '.12g')}",
            population=spec.population,
            segment=context.segment,
            pair_index=context.pair_index,
            backend=context.backend,
            target_rps=spec.target_rps,
            traced=spec.traced,
            trace_pair_id=spec.trace_pair_id,
            request_seed=context.request_seed,
            snapshot_generation_id=args.snapshot_identity["generation_id"],
            snapshot_config_sha256=args.snapshot_identity["config_sha256"],
        )
        record["actual_launch_ns"] = record.pop(
            "started_monotonic_ns", context.actual_launch_ns
        )
        if "artifact_monotonic_ns" in record:
            record["artifact_ns"] = record.pop("artifact_monotonic_ns")
        record["finished_ns"] = record.pop(
            "finished_monotonic_ns", time.monotonic_ns()
        )
        return record

    return request


def _augment_burst_accounting(summary, proc_before, proc_after, cpu_before, cpu_after):
    summary["machine_proc_stat_before"] = proc_before
    summary["machine_proc_stat_after"] = proc_after
    summary["machine_proc_stat_delta"] = counter_delta(
        proc_before["cpu"], proc_after["cpu"]
    )
    summary["cgroup_cpu_stat_before"] = cpu_before
    summary["cgroup_cpu_stat_after"] = cpu_after
    if set(cpu_before) != set(cpu_after):
        raise MeasurementInvalid("cgroup accounting set changed during burst")
    summary["cgroup_cpu_stat_delta"] = {
        name: counter_delta(cpu_before[name], cpu_after[name])
        for name in sorted(cpu_before)
    }


def _failure_record(phase: str, error: BaseException) -> dict:
    details = getattr(error, "details", None)
    try:
        canonical_json(details)
    except (TypeError, ValueError):
        details = repr(details)
    return {
        "phase": phase,
        "type": type(error).__name__,
        "message": str(error),
        "details": details,
    }


def _cpu_snapshots(audits: dict[str, CgroupAudit]) -> dict:
    return {name: audit.cpu_snapshot() for name, audit in sorted(audits.items())}


def _cgroup_records(audits: dict[str, CgroupAudit]) -> dict:
    return {name: audit.record() for name, audit in sorted(audits.items())}


def _interburst_membership(
    audits: dict[str, CgroupAudit], serve_pid: int, control_pid: int,
) -> dict[str, list[int]]:
    members = {name: audit.live_pids() for name, audit in sorted(audits.items())}
    expected_exact = {
        "run": [],
        "driver": [os.getpid()],
        "file": [],
        "uffd": [serve_pid],
    }
    mismatch = {
        name: {"expected": expected, "actual": members.get(name)}
        for name, expected in expected_exact.items() if members.get(name) != expected
    }
    if control_pid not in members.get("control", []):
        mismatch["control"] = {
            "expected_to_contain": control_pid,
            "actual": members.get("control"),
        }
    if mismatch:
        raise MeasurementInvalid(f"inter-burst cgroup membership is not quiescent: {mismatch}")
    return members


def execute(args, schedule: dict, provenance: dict) -> int:
    os.mkdir(args.out_dir)
    _fsync_directory(os.path.dirname(args.out_dir))
    log_dir = os.path.join(args.out_dir, "logs")
    trace_dir = os.path.join(args.out_dir, "fault-trace")
    os.mkdir(log_dir)
    os.mkdir(trace_dir)
    _fsync_directory(args.out_dir)
    write_json_exclusive(os.path.join(args.out_dir, "schedule.json"), schedule)
    write_json_exclusive(os.path.join(args.out_dir, "provenance.json"), provenance)

    scope = None
    audits = None
    serve = None
    control = None
    control_scheduler = None
    sampler = None
    active_tracer = None
    sinks = {"request": None, "burst": None, "control": None, "sample": None}
    summaries = []
    run_before = None
    run_after = None
    final_cgroups = None
    control_summary = None
    sampler_summary = None
    final_snapshot_identity = None
    failures = []
    phase = RunPhase()
    try:
        scope = RunCgroups(args.run_id, args.cgroup_root)
        audits = scope.enter()
        args.cgroup_paths = dict(scope.paths)
        run_before = {
            "machine_proc_stat": read_machine_proc_stat(),
            "cgroup_cpu_stat": _cpu_snapshots(audits),
            "cgroups": _cgroup_records(audits),
        }
        sinks["request"] = JsonlSink(os.path.join(args.out_dir, "requests.jsonl"))
        sinks["burst"] = JsonlSink(os.path.join(args.out_dir, "bursts.jsonl"))
        sinks["control"] = JsonlSink(os.path.join(args.out_dir, "host-control.jsonl"))
        sinks["sample"] = JsonlSink(os.path.join(args.out_dir, "host-samples.jsonl"))
        sampler = HostSampler(
            sinks["sample"], audits, phase.snapshot, schedule["host_sample_interval_ns"]
        )
        sampler.start()

        serve = UffdServe(args, audits["uffd"], scope.paths["uffd"], log_dir)
        serve_pid = serve.start()
        write_json_exclusive(
            os.path.join(args.out_dir, "uffd-serve.json"), serve.record
        )
        control = NativeChromiumControl(
            args, audits["control"], scope.paths["control"], log_dir
        )
        control_warmup = control.start()
        write_json_exclusive(
            os.path.join(args.out_dir, "host-control-warmup.json"), control_warmup
        )
        control_scheduler = ControlScheduler(control, sinks["control"], schedule["control"])
        control_scheduler.start()

        global_base = 0
        for raw_spec in schedule["bursts"]:
            sampler.check()
            control_scheduler.check()
            spec = BurstSpec.from_dict(raw_spec)
            phase.set("burst", spec.burst_id)
            if spec.traced:
                active_tracer = BpftraceFaultTracer(
                    {name: scope.paths[name] for name in ("file", "uffd")},
                    scope.paths["driver"], os.getpid(), trace_dir, spec.burst_id,
                    audits["driver"], args.bpftrace,
                )
                active_tracer.start(args.trace_start_timeout)
            proc_before = read_machine_proc_stat()
            cpu_before = _cpu_snapshots(audits)
            records = None
            try:
                request_fn = _make_request_fn(
                    args, spec, serve_pid, log_dir, audits, active_tracer, global_base
                )
                records, summary = run_open_loop_burst(
                    args.run_id, spec, request_fn, SystemClock(), ThreadLauncher()
                )
            finally:
                if active_tracer is not None and records is None:
                    active_tracer.abort()
                    active_tracer = None
            if active_tracer is not None:
                trace = active_tracer.stop(args.trace_stop_timeout)
                join_fault_trace(records, trace)
                trace_paths = {
                    "raw": active_tracer.raw_path,
                    "stderr": active_tracer.stderr_path,
                    "program": active_tracer.program_path,
                }
                if any(not isinstance(path, str) for path in trace_paths.values()):
                    raise MeasurementInvalid("bpftrace did not retain every artifact path")
                summary["fault_trace"] = {
                    "scope": (
                        "Firecracker process endpoint-ready through artifact return; all "
                        "VMAs, not guest-RAM-filtered and not UFFD events"
                    ),
                    "processes": len(trace["processes"]),
                    "artifacts": {
                        name: {
                            "path": os.path.relpath(path, args.out_dir),
                            "sha256": sha256_file(path),
                            "bytes": os.stat(path).st_size,
                        }
                        for name, path in sorted(trace_paths.items())
                    },
                }
                active_tracer = None
            _augment_burst_accounting(
                summary, proc_before, read_machine_proc_stat(),
                cpu_before, _cpu_snapshots(audits),
            )
            summary["interburst_cgroup_membership"] = _interburst_membership(
                audits, serve_pid, control.proc.pid
            )
            for record in records:
                sinks["request"].write(record)
            sinks["request"].sync()
            sinks["burst"].write(summary)
            sinks["burst"].sync()
            summaries.append(summary)
            global_base += summary["total_planned"]
            print(
                f"{spec.burst_id} target={spec.target_rps:g}rps/backend "
                f"file={summary['backends']['file']['artifact_completed']}/"
                f"{summary['backends']['file']['planned']} "
                f"uffd={summary['backends']['uffd']['artifact_completed']}/"
                f"{summary['backends']['uffd']['planned']} "
                f"drain={summary['drain_span_ms']:.1f}ms",
                flush=True,
            )
            if (
                summary["failed"]
                or summary["total_artifact_completed"] != summary["total_planned"]
                or summary["total_drained"] != summary["total_planned"]
                or summary["total_cleanup_confirmed"] != summary["total_planned"]
            ):
                raise MeasurementInvalid(
                    f"burst {spec.burst_id} has failed or unconfirmed-clean requests", summary
                )
            sampler.check()
            control_scheduler.check()
        if args.trace_faults:
            gate_path = os.path.join(args.out_dir, "trace-perturbation.json")
            try:
                verdict = evaluate_trace_perturbation(
                    summaries, args.max_trace_perturbation_pct
                )
            except MeasurementInvalid as gate_error:
                if isinstance(gate_error.details, dict):
                    write_json_exclusive(gate_path, gate_error.details)
                raise
            write_json_exclusive(gate_path, verdict)
    except BaseException as caught:
        failures.append(_failure_record("run", caught))
    finally:
        phase.set("teardown")
        if active_tracer is not None:
            try:
                active_tracer.abort()
            except BaseException as abort_error:
                failures.append(_failure_record("tracer-abort", abort_error))
        if control_scheduler is not None:
            try:
                control_summary = control_scheduler.stop()
            except BaseException as stop_error:
                failures.append(_failure_record("control-scheduler-stop", stop_error))
        if control is not None:
            try:
                control.stop()
            except BaseException as stop_error:
                failures.append(_failure_record("control-stop", stop_error))
        if serve is not None:
            try:
                serve.stop()
            except BaseException as stop_error:
                failures.append(_failure_record("serve-stop", stop_error))
        if sampler is not None:
            try:
                sampler_summary = sampler.stop()
            except BaseException as stop_error:
                failures.append(_failure_record("sampler-stop", stop_error))
        lease = getattr(args, "snapshot_generation_lease", None)
        if lease is not None:
            try:
                final_snapshot_identity = lease.verify()
            except BaseException as generation_error:
                failures.append(_failure_record("snapshot-generation-verify", generation_error))
        else:
            failures.append(_failure_record(
                "snapshot-generation-verify",
                MeasurementInvalid("measured run has no snapshot generation lease"),
            ))
        if audits is not None:
            try:
                run_after = {
                    "machine_proc_stat": read_machine_proc_stat(),
                    "cgroup_cpu_stat": _cpu_snapshots(audits),
                    "cgroups": _cgroup_records(audits),
                }
            except BaseException as accounting_error:
                failures.append(_failure_record("final-accounting", accounting_error))
        for name, sink in sinks.items():
            if sink is not None:
                try:
                    sink.close()
                except BaseException as close_error:
                    failures.append(_failure_record(f"{name}-sink-close", close_error))
        if scope is not None:
            try:
                final_cgroups = scope.leave()
            except BaseException as leave_error:
                failures.append(_failure_record("cgroup-leave", leave_error))

    primary = failures[0] if failures else None
    status = {
        "schema": "fcvm.chromium.reqscale.status.v2",
        "run_id": args.run_id,
        "valid": not failures,
        "bursts_completed": len(summaries),
        "bursts_planned": len(schedule["bursts"]),
        "control": control_summary,
        "sampler": sampler_summary,
        "snapshot_identity_after": final_snapshot_identity,
        "run_before": run_before,
        "run_after": run_after,
        "final_cgroups": final_cgroups,
        "error": (
            f"{primary['type']}: {primary['message']}" if primary is not None else None
        ),
        "error_details": primary["details"] if primary is not None else None,
        "errors": failures,
    }
    write_json_exclusive(os.path.join(args.out_dir, "status.json"), status)
    if failures:
        print(status["error"], file=sys.stderr)
        return 4
    return 0


def _parse_rates(value: str) -> tuple[float, ...]:
    try:
        rates = tuple(float(item.strip()) for item in value.split(",") if item.strip())
    except ValueError as error:
        raise argparse.ArgumentTypeError(f"invalid comma-separated rates: {value}") from error
    if not rates:
        raise argparse.ArgumentTypeError("at least one rate is required")
    return rates


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot-tag", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--rates", required=True, type=_parse_rates)
    parser.add_argument("--bursts", type=int, required=True)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--max-offered-rps-error-pct", type=float, required=True)
    parser.add_argument("--min-departure-ratio", type=float, required=True)
    parser.add_argument("--max-score-end-backlog", type=int, required=True)
    parser.add_argument("--max-p95-launch-lag-ms", type=float, required=True)
    parser.add_argument("--max-control-median-drift-pct", type=float, required=True)
    parser.add_argument("--run-id", default="")
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--fcvm", default=os.path.join(REPO, "target", "release", "fcvm"))
    parser.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    parser.add_argument("--state-dir", default="")
    parser.add_argument("--cgroup-root", default="/sys/fs/cgroup")
    parser.add_argument("--control-chromium", default="chromium")
    parser.add_argument("--control-url", default="")
    parser.add_argument("--control-timeout", type=float, default=8.0)
    parser.add_argument("--control-tmp-root", default="/tmp")
    parser.add_argument("--format", choices=("png", "jpeg"), default="jpeg")
    parser.add_argument("--quality", type=int, default=80)
    parser.add_argument("--cdp-port", type=int, default=9222)
    parser.add_argument("--ws-url", default="")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--teardown-timeout", type=float, default=60.0)
    parser.add_argument("--rust-log", default="fcvm=debug")
    parser.add_argument("--trace-faults", action="store_true")
    parser.add_argument("--trace-rate", type=float)
    parser.add_argument("--trace-pairs", type=int, default=0)
    parser.add_argument("--max-trace-perturbation-pct", type=float)
    parser.add_argument("--trace-start-timeout", type=float, default=30.0)
    parser.add_argument("--trace-stop-timeout", type=float, default=30.0)
    parser.add_argument("--bpftrace", default="bpftrace")
    parser.add_argument(
        "--plan-only", action="store_true",
        help="write only a schedule.json; no cgroup, serve, clone, or benchmark",
    )
    args = parser.parse_args()

    args.run_id = args.run_id or uuid.uuid4().hex
    args.fcvm = os.path.abspath(args.fcvm)
    args.data_root = os.path.abspath(args.data_root)
    args.state_dir = args.state_dir or os.path.join(args.data_root, "state")
    args.out_dir = os.path.abspath(args.out_dir)
    args.control_url = args.control_url or args.url
    args.control_tmp_root = os.path.abspath(args.control_tmp_root)
    try:
        _validate_snapshot_tag(args.snapshot_tag)
    except ValueError as error:
        parser.error(str(error))
    if args.rust_log != "fcvm=debug":
        parser.error("--rust-log must be exactly fcvm=debug for measured runs")
    if args.trace_faults:
        if args.trace_rate is None or args.trace_pairs < 3:
            parser.error("--trace-faults requires --trace-rate and --trace-pairs >= 3")
        if args.max_trace_perturbation_pct is None:
            parser.error(
                "--trace-faults requires a predeclared --max-trace-perturbation-pct"
            )
    elif (
        args.trace_rate is not None
        or args.trace_pairs
        or args.max_trace_perturbation_pct is not None
    ):
        parser.error("trace options require --trace-faults")

    config = ScheduleConfig(
        rates=args.rates,
        scored_bursts=args.bursts,
        seed=args.seed,
        criteria=CapacityCriteria(
            max_offered_rps_error_pct=args.max_offered_rps_error_pct,
            min_departure_ratio=args.min_departure_ratio,
            max_score_end_backlog=args.max_score_end_backlog,
            max_p95_launch_lag_ms=args.max_p95_launch_lag_ms,
            max_control_median_drift_pct=args.max_control_median_drift_pct,
        ),
        trace_rate=args.trace_rate,
        trace_pairs=args.trace_pairs,
    )
    try:
        schedule = build_schedule(config, args.run_id)
    except ValueError as error:
        parser.error(str(error))
    if args.plan_only:
        os.mkdir(args.out_dir)
        _fsync_directory(os.path.dirname(args.out_dir))
        write_json_exclusive(os.path.join(args.out_dir, "schedule.json"), schedule)
        return 0
    if os.geteuid() != 0:
        parser.error("measured runs must be root so owned accounting cgroups can be created")
    resolved_chromium = (
        args.control_chromium if os.path.isabs(args.control_chromium)
        else shutil.which(args.control_chromium)
    )
    if not resolved_chromium or not os.path.isfile(resolved_chromium):
        parser.error(f"host control Chromium is missing: {args.control_chromium}")
    args.control_chromium = os.path.abspath(resolved_chromium)
    if (
        not math.isfinite(args.control_timeout)
        or not 0 < args.control_timeout < CONTROL_INTERVAL_SECONDS
    ):
        parser.error("--control-timeout must be finite, positive, and below 10 seconds")
    if not os.path.isdir(args.control_tmp_root):
        parser.error(f"--control-tmp-root is not a directory: {args.control_tmp_root}")
    try:
        with SnapshotGenerationLease(args.data_root, args.snapshot_tag) as lease:
            args.snapshot_generation_lease = lease
            args.snapshot_identity = dict(lease.identity)
            provenance = collect_provenance(args, schedule, args.snapshot_identity)
            with TerminationFence():
                return execute(args, schedule, provenance)
    except (MeasurementInvalid, OSError, ValueError) as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        return 4


if __name__ == "__main__":
    sys.exit(main())
