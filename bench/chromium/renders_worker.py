#!/usr/bin/env python3
"""One render at a time for pages mode's host module (`fcvm fleet renders-bench`,
src/fleet/renders). The module starts one worker per render it may have in
flight on a side, writes a job per line on the worker's stdin and reads one
result per line from its stdout. A worker adds no render path of its own:

  --side fcvm      each render is reqbench.run_cdp_request(args, rep, fast=True),
                   the request of reqbench's cdp-fast arm: restore a clone from
                   the memory server, cdpdrive from the host, one SIGKILL. The
                   clone starts through guardexec.py, which puts fcvm in the
                   side's cgroup before it executes and ends it if this worker
                   dies.
  --side kitesurf  each render is cdpdrive.drive() against the engine the job
                   names, with the arguments kitesurf_measure.py passes.

Lines in:   {"seq": n, "page": i, "url": "...", "level": l, "address": "host:port"}
            ("address" for kitesurf only). End of input ends the worker.
Lines out:  {"ready": true} once the harness modules are imported, then one
            {"seq", "page", "level", "ok", "ms", "error", "stage", "sha256",
             "bytes", "cpu_ms", "t0_ns", "t1_ns", "detail"} per job, in order.
            An fcvm job whose driver returned writes {"answered": true, "seq",
            "ok", "ms", "error"} first, when the answer is in hand and before the
            clone's teardown, so the module can start its next render then.

"ms" is what the page shows: for fcvm the request's blocking_ms (from just
before `fcvm snapshot run` is spawned until the driver returns; the teardown
that follows is outside it and inside this worker's turn), for kitesurf the
time of drive(). Clocks are time.monotonic_ns().
"""

import argparse
import json
import os
import sys
import time


def fcvm_args(opts, bench):
    prefix = [sys.executable, os.path.join(bench, "guardexec.py"),
              "--expected-parent", str(os.getpid()),
              "--cgroup-procs", opts.cgroup_procs, "--"]
    log_dir = os.path.join(opts.out_dir, f"logs-{opts.index}")
    os.makedirs(log_dir, exist_ok=True)
    return argparse.Namespace(
        serve_pid=opts.serve_pid, snapshot_tag="", url="", format="jpeg", quality=80,
        cdp_port=opts.cdp_port, ws_url="", fcvm=opts.fcvm, data_root=opts.data_root,
        state_dir=os.path.join(opts.data_root, "state"), out_dir=log_dir,
        timeout=opts.timeout, teardown_timeout=60.0, rust_log="fcvm=debug",
        run_id=f"{opts.run_id}w{opts.index}", firecracker_fault_probe=None,
        spawn_prefix=prefix, prewire=True, engine="chromium", resolve_poll_ms=5.0,
        viewport=opts.viewport,
    )


def fcvm_render(reqbench, args, job, rep, emit):
    args.url = job["url"]
    args.on_answer = lambda rec: emit({
        "answered": True, "seq": job["seq"], "ok": bool(rec.get("ok")),
        "ms": rec.get("blocking_ms"), "error": rec.get("error"),
    })
    try:
        rec = reqbench.run_cdp_request(args, rep, fast=True)
    except BaseException as error:  # noqa: BLE001 - recorded; the loop decides what follows
        supplied = getattr(error, "record", None)
        rec = dict(supplied) if isinstance(supplied, dict) else {}
        rec["ok"] = False
        rec.setdefault("error", f"{type(error).__name__}: {error}")
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            rec["fatal"] = True
    teardown = rec.get("teardown") or {}
    life = teardown.get("lifetime_cpu_ms_by_child") or {}
    reaped = teardown.get("reaped_children_cpu_ms_by_child") or {}
    per_child = teardown.get("per_child_cpu") or {}
    cpu = None
    if life:
        try:
            cpu = (sum(life.values()) + sum(reaped.values())
                   + sum((c or {}).get("reclaim_cpu_ms") or 0.0 for c in per_child.values()))
        except TypeError:
            cpu = None
    render = rec.get("render") or {}
    ok = bool(rec.get("ok"))
    log = rec.get("log")
    if ok and log:
        try:
            os.unlink(log)
        except OSError:
            pass
    return {
        "ok": ok, "ms": rec.get("blocking_ms"), "error": rec.get("error"),
        "stage": rec.get("failure_stage"), "sha256": render.get("image_sha256"),
        "bytes": render.get("image_bytes"), "cpu_ms": cpu, "fatal": bool(rec.get("fatal")),
        "detail": {
            "stages": render.get("stages"), "viewport": render.get("viewport"),
            "width": render.get("width"), "height": render.get("height"),
            "spawn_to_port_ms": rec.get("spawn_to_port_ms"), "wall_ms": rec.get("wall_ms"),
            "teardown_ms": teardown.get("teardown_total_ms"),
            "ack_ms": (rec.get("restore_ack") or {}).get("ack_ms"),
            "prewired": rec.get("ws_url_prewired"), "failure_class": rec.get("failure_class"),
            "log": None if ok else log,
        },
    }


def engine_render(cdpdrive, opts, bench, job):
    ns = argparse.Namespace(
        cdp_host=job["address"], url=job["url"], format="jpeg", quality=80, timeout=opts.timeout,
        idle_wait_ms=0.0, out_prefix="", ws_url="", connect_retries=2000, nav_timing=True,
        print_target=False, host_header="", net_trace=None, net_trace_drain_ms=5000.0,
        render_module=os.path.join(bench, "render.py"), op="screenshot", viewport=opts.viewport,
    )
    started = time.monotonic_ns()
    try:
        result = cdpdrive.drive(ns)
    except Exception as error:  # noqa: BLE001 - recorded, then the loop goes on
        result = {"ok": False, "error": f"{type(error).__name__}: {error}"}
    ms = (time.monotonic_ns() - started) / 1e6
    ok = result.get("ok") is True
    return {
        "ok": ok, "ms": ms, "error": result.get("error"), "stage": result.get("stage"),
        "sha256": result.get("image_sha256"), "bytes": result.get("image_bytes"), "cpu_ms": None,
        "fatal": False,
        "detail": {"stages": result.get("stages"), "viewport": result.get("viewport"),
                   "width": result.get("width"), "height": result.get("height"),
                   "address": job["address"]},
    }


def main(argv=None, stdin=None, stdout=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--side", required=True, choices=("fcvm", "kitesurf"))
    p.add_argument("--index", type=int, required=True)
    p.add_argument("--bench", default=os.path.dirname(os.path.abspath(__file__)),
                   help="where reqbench.py, cdpdrive.py, render.py and guardexec.py are")
    p.add_argument("--timeout", type=float, default=120.0)
    p.add_argument("--viewport", default="", metavar="WIDTHxHEIGHT")
    p.add_argument("--run-id", default="")
    p.add_argument("--out-dir", default="")
    p.add_argument("--fcvm", default="")
    p.add_argument("--serve-pid", type=int, default=0)
    p.add_argument("--data-root", default="")
    p.add_argument("--cgroup-procs", default="")
    p.add_argument("--cdp-port", type=int, default=9222)
    opts = p.parse_args(argv)
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    sys.path.insert(0, opts.bench)

    def emit(row):
        stdout.write(json.dumps(row, default=repr) + "\n")
        stdout.flush()

    if opts.side == "fcvm":
        for need in ("run_id", "out_dir", "fcvm", "serve_pid", "data_root", "cgroup_procs"):
            if not getattr(opts, need):
                p.error(f"--side fcvm needs --{need.replace('_', '-')}")
        import reqbench
        # A killed fcvm's children stay readable until this worker reaps them, which is
        # how the request's teardown reads their CPU (reqbench.become_child_subreaper).
        reqbench.become_child_subreaper()
        args = fcvm_args(opts, opts.bench)
    else:
        import cdpdrive
    emit({"ready": True, "pid": os.getpid()})

    rep = 0
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        job = json.loads(line)
        started = time.monotonic_ns()
        if opts.side == "fcvm":
            row = fcvm_render(reqbench, args, job, rep, emit)
        else:
            row = engine_render(cdpdrive, opts, opts.bench, job)
        row.update(seq=job["seq"], page=job["page"], level=job.get("level"),
                   t0_ns=started, t1_ns=time.monotonic_ns())
        if not row["ok"] and row.get("ms") is None:
            row["ms"] = (row["t1_ns"] - started) / 1e6
        fatal = row.pop("fatal", False)
        emit(row)
        if fatal:
            return 1
        rep += 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
