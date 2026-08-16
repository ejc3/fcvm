#!/usr/bin/env python3
"""Resident health loop shared by every engine's probe.

Extracted from cdp_health.py when the WebKit arm arrived and copied the
PRE-resident shape: a per-second `python3 <probe>` HEALTHCHECK. That is the
design this file exists to prevent. Measured in this image, per clone, per
second, forever: 9.1ms of interpreter startup and 43.6ms for the whole check
even when it fails fast. Run resident and paid once at the golden instead, the
interpreter's pages are dirtied BEFORE the snapshot and are therefore shared by
every clone rather than privately re-dirtied -- which is the per-clone memory
figure this benchmark exists to produce.

Everything here is engine-agnostic. A probe supplies only
`main_with_reason() -> (exit_code, reason)`; the verdict file, its boottime
stamp, the atomic publish and the crash guard are identical for all of them,
so reqbench.sh's `grep -q health_state` gate stays engine-independent too.
"""

import os
import sys
import time

# MUST match health_state.sh:31 -- writer and reader resolve the same path.
DEFAULT_STATE_FILE = "/run/bench-health"
LOOP_INTERVAL = float(os.environ.get("BENCH_HEALTH_INTERVAL", "1.0"))


def state_file() -> str:
    """Resolved per call, not bound at import.

    Binding it at import makes the value depend on WHEN this module is first
    imported relative to the environment being set, which is an ordering the
    caller cannot see. test_health_state loads a cache-free COPY of a probe to
    prove the writer and reader agree, and with an import-time constant that
    copy silently wrote to the real /run path instead of the test's temp file.
    """
    return os.environ.get("BENCH_HEALTH_STATE", DEFAULT_STATE_FILE)


def monotonic_seconds() -> float:
    """Seconds from /proc/uptime, so a clock step cannot age a verdict.

    NOT time.time(). fc-agent steps CLOCK_REALTIME on every restore
    (set_system_clock), which would make every clone's freshly written verdict
    look hours old to a wall-clock reader and fail the gate on every clone.

    /proc/uptime is CLOCK_BOOTTIME (ktime_get_boottime_ts64), not
    CLOCK_MONOTONIC. The reader uses the same file, so both sides measure the
    same clock either way, but the freshness scheme rests on guest boottime
    being CONTINUOUS across snapshot and restore. That holds where the
    firecracker fork owns the VM-wide counter offset and advances it by the
    pause duration (AGENTS.md, NV2 snapshot lifecycle). It is NOT established
    for the cloud-hypervisor backend. If boottime jumps on restore, every
    clone reports "verdict is Ns old" forever, which is the same total failure
    this design moved away from, relocated from realtime to boottime.
    """
    with open("/proc/uptime", "r", encoding="ascii") as handle:
        return float(handle.read().split()[0])


def publish(verdict: str, detail: str) -> None:
    """Write the verdict atomically, so a reader never sees a half-written line."""
    target = state_file()
    tmp = f"{target}.tmp"
    with open(tmp, "w", encoding="utf-8") as handle:
        handle.write(f"{verdict} {monotonic_seconds():.3f} {detail}\n")
    os.replace(tmp, target)


def loop(main_with_reason, label: str) -> int:
    """Run the check forever, publishing each verdict.

    Why resident: as a HEALTHCHECK command this cost a fresh CPython per second
    in EVERY clone, forever. Measured in this image: 9.1ms of interpreter
    startup, 43.6ms for the whole check even when it fails fast. Paid once at
    the golden instead, the interpreter's pages are dirtied before the snapshot
    and are therefore SHARED by every clone rather than privately re-dirtied.
    Each iteration then writes one small file on tmpfs.
    """
    while True:
        started = monotonic_seconds()
        try:
            code, reason = main_with_reason()
            publish("healthy" if code == 0 else "unhealthy", reason)
        except Exception as error:  # a crash here must not look healthy
            # Guarded. The unguarded version raised the SAME error the handler
            # was catching (a full /run tmpfs, EROFS, a missing directory), let
            # it escape loop(), and the process exited. Nothing supervises this
            # loop, so the container would then be unhealthy forever with no
            # verdict file at all, and the golden would wait out its full 300s
            # timeout with nothing to say why.
            try:
                publish("unhealthy", f"loop error: {type(error).__name__}: {error}")
            except Exception as publish_error:
                print(
                    f"{label} loop: cannot publish ({type(publish_error).__name__}: "
                    f"{publish_error}) after {type(error).__name__}: {error}",
                    file=sys.stderr,
                    flush=True,
                )
        elapsed = monotonic_seconds() - started
        time.sleep(max(0.0, LOOP_INTERVAL - elapsed))


