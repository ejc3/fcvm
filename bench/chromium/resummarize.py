#!/usr/bin/env python3
"""Recompute a hostcdp summary.json from one complete host run.

The producer's complete.json must bind the run metadata and every record before
any summary is written. A corpus-extra run also needs its parent campaign's
campaign-complete.json to bind the completed child.
The p50 uses the convention reqanalyze publishes (statistics.median), so the
host and VM descriptive tables use the same kind of number. The records are
untouched. compare.py does not publish an effect ratio from separately timed
host and VM runs.

hostcdp.sh can write "failures": 0 because it exits 4 on the first failed rep,
so a summary it reaches is a run that had none. This script has no such
invariant: it is pointed at a directory. Filtering on `warmup` alone put a
failed rep's wall_ms -- a timeout, the largest number in the file -- into the
distribution while the field beside it still said no failures. Measured on a
five-rep fixture with one failure: p95 30000.0, mean 6082.0, failures 0.

Refusal removes any earlier summary before returning, so an interrupted or
failed recomputation cannot leave an old success beside the rejected records.
It also rewrites the file hostcdp.sh wrote, so it has to carry
loadavg1_measured forward. That field is the answer to "was the box busy while
this was measured", and a summary that dropped it cannot be checked against
that question again.
"""
import fcntl
import os
import statistics
import sys
import time

from compare import (
    Refusal,
    acquire_output_lock,
    load_host_dataset,
    open_output_lock,
    open_output_target,
    revalidate_host_inputs,
    validate_output_lock,
    write_json_atomic,
)


def pct(values, p):
    values = sorted(values)
    n = len(values)
    if p == 50:
        return statistics.median(values)
    return values[max(0, -(-p * n // 100) - 1)]


def await_producer_publication(target, lock_target, lock_fd, wait_seconds=5.0):
    """Yield the summary lock while a successful worker awaits its guardian."""
    deadline = time.monotonic() + wait_seconds

    def entry_exists(name):
        try:
            os.stat(name, dir_fd=target["directory_fd"], follow_symlinks=False)
            return True
        except FileNotFoundError:
            return False
        except OSError as error:
            raise Refusal(
                f"cannot inspect producer publication state {name}: {error}"
            ) from error

    while (
        entry_exists(".summary.pending")
        and not entry_exists("complete.json")
        and not entry_exists("WITHDRAWN")
    ):
        validate_output_lock(lock_target, lock_fd)
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Refusal(
                "producer publication remained pending after its worker exited"
            )
        time.sleep(min(0.01, remaining))
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Refusal(
                "producer publication remained pending after its worker exited"
            )
        acquire_output_lock(lock_target, lock_fd, wait_seconds=remaining)


d = sys.argv[1]
summary_path = os.path.join(d, "summary.json")
try:
    summary_target = open_output_target(summary_path)
except Refusal as error:
    sys.exit(f"REFUSING: {error}")
lock_target = dict(summary_target)
lock_target.update({
    "path": os.path.join(d, ".summary"),
    "absolute": os.path.join(summary_target["directory"], ".summary"),
    "name": ".summary",
})
lock_fd = None
try:
    lock_fd = open_output_lock(lock_target)
    acquire_output_lock(lock_target, lock_fd)
    await_producer_publication(summary_target, lock_target, lock_fd)
except Refusal as error:
    if lock_fd is not None:
        os.close(lock_fd)
    os.close(summary_target["directory_fd"])
    sys.exit(f"REFUSING: {error}")
try:
    try:
        os.unlink(
            summary_target["name"],
            dir_fd=summary_target["directory_fd"],
        )
    except FileNotFoundError:
        pass
    except OSError as error:
        sys.exit(f"REFUSING: cannot remove stale {summary_path}: {error}")
    try:
        _meta, rows, _transformed, _counts, identities = load_host_dataset(d)
    except Refusal as error:
        sys.exit(f"REFUSING: {error}")
    measured_rows = [r for r in rows if r["warmup"] is False]

    vals = [r["wall_ms"] for r in measured_rows]
    by_url = {}
    for r in measured_rows:
        by_url.setdefault(r.get("url", ""), []).append(r["wall_ms"])
    per_url = {
        u: {"n": len(v), "p50_ms": round(pct(v, 50), 1),
            "p95_ms": round(pct(v, 95), 1),
            "mean_ms": round(statistics.mean(v), 1)}
        for u, v in by_url.items()
    }
    la = [r["loadavg1"] for r in measured_rows]
    load = {"n": len(la), "min": round(min(la), 2),
            "median": round(statistics.median(la), 2), "max": round(max(la), 2)}
    out = {
        "n": len(vals), "p50_ms": round(pct(vals, 50), 1),
        "p95_ms": round(pct(vals, 95), 1),
        "mean_ms": round(statistics.mean(vals), 1), "failures": 0,
        "p50_convention": "statistics.median", "loadavg1_measured": load,
        "per_url": per_url,
    }

    def recheck_inputs_before_publication():
        revalidate_host_inputs(d, identities)
        validate_output_lock(lock_target, lock_fd)

    write_json_atomic(
        summary_path,
        out,
        output_target=summary_target,
        before_publish=recheck_inputs_before_publication,
        after_publish=recheck_inputs_before_publication,
    )
    print(d, out["n"], out["p50_ms"], out["p95_ms"], out["mean_ms"])
finally:
    os.close(lock_fd)
    os.close(summary_target["directory_fd"])
