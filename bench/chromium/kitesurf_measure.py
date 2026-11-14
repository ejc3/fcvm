#!/usr/bin/env python3
"""The Kitesurf arm's measured loop and its summary (kitesurf.sh runs both).

    kitesurf_measure.py --address H:P --urls U,U --arms cdp,html --reps N --warmup W \
        --cgroup DIR --loadavg-file F --run-json R --out kitesurf.jsonl
    kitesurf_measure.py summarize --jsonl kitesurf.jsonl --run-json R \
        --memory-peak BYTES --correctness DIR --out summary.json

The loop imports cdpdrive once and times one drive() per request, as hostcdp.sh
and reqbench.py do, with the arguments reqbench's cdp arm gives it (JPEG at
quality 80, navigation timing on; the html arm swaps the screenshot for
outerHTML). Rep r renders urls[r % len(urls)], every arm once per rep, so the
arms interleave. A failed request is recorded and the loop goes on: Kitesurf
does not render every page, and its failures are part of the result, not a
reason to stop. Each row is written before the next request starts.

Per request it records the engine cgroup's usage_usec before and after, the
CPU of each process in it by command name (from /proc/<pid>/stat), its
memory.current after, and the 1-minute load at the start.
"""

import argparse
import importlib.util
import json
import os
import re
import statistics
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
CLK_TCK = os.sysconf("SC_CLK_TCK")


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def read_int_field(path, key):
    with open(path) as handle:
        for line in handle:
            k, _, v = line.partition(" ")
            if k == key:
                return int(v)
    raise KeyError(f"{key} not in {path}")


def proc_cpu_ms(cgroup):
    """CPU milliseconds of each process now in the cgroup, summed by command name."""
    out = {}
    with open(os.path.join(cgroup, "cgroup.procs")) as handle:
        pids = [p.strip() for p in handle if p.strip()]
    for pid in pids:
        try:
            with open(f"/proc/{pid}/stat") as handle:
                stat = handle.read()
        except OSError:
            continue
        comm = stat[stat.index("(") + 1:stat.rindex(")")]
        fields = stat[stat.rindex(")") + 2:].split()
        ms = (int(fields[11]) + int(fields[12])) * 1000.0 / CLK_TCK
        out[comm] = out.get(comm, 0.0) + ms
    return out


def read_load(path):
    try:
        with open(path) as handle:
            raw = handle.readline().split(" ")[0]
    except OSError:
        return None
    return float(raw) if re.fullmatch(r"[0-9]+([.][0-9]+)?", raw) else None


def measure(args):
    cdpdrive = load_module("cdpdrive", os.path.join(HERE, "cdpdrive.py"))
    with open(args.run_json, "rb") as handle:
        import hashlib
        run_sha = hashlib.sha256(handle.read()).hexdigest()
    urls = [u for u in args.urls.split(",") if u]
    arms = [a for a in args.arms.split(",") if a]
    total = args.warmup + args.reps
    cpu_stat = os.path.join(args.cgroup, "cpu.stat")
    mem_current = os.path.join(args.cgroup, "memory.current")
    failures = 0
    with open(args.out, "a") as out:
        for rep in range(total):
            url = urls[rep % len(urls)]
            for arm in arms:
                ns = argparse.Namespace(
                    cdp_host=args.address, url=url, format="jpeg", quality=80, timeout=30.0,
                    idle_wait_ms=0.0, out_prefix="", ws_url="", connect_retries=2000,
                    nav_timing=True, print_target=False, host_header="", net_trace=None,
                    net_trace_drain_ms=5000.0, render_module=os.path.join(HERE, "render.py"),
                    op="html" if arm == "html" else "screenshot",
                )
                load = read_load(args.loadavg_file)
                cpu_before = read_int_field(cpu_stat, "usage_usec")
                procs_before = proc_cpu_ms(args.cgroup)
                started = time.monotonic_ns()
                try:
                    result = cdpdrive.drive(ns)
                except Exception as error:
                    result = {"ok": False, "error": f"{type(error).__name__}: {error}"}
                wall_ms = round((time.monotonic_ns() - started) / 1e6, 1)
                cpu_after = read_int_field(cpu_stat, "usage_usec")
                procs_after = proc_cpu_ms(args.cgroup)
                with open(mem_current) as handle:
                    mem_after = int(handle.read())
                # memory.current counts the page cache wrangler's file reads
                # leave in the scope; anon is what the engine itself holds.
                anon_after = read_int_field(os.path.join(args.cgroup, "memory.stat"), "anon")
                ok = result.get("ok") is True
                failures += not ok
                proc_delta = {k: round(procs_after.get(k, 0.0) - procs_before.get(k, 0.0), 1)
                              for k in set(procs_before) | set(procs_after)}
                out.write(json.dumps({
                    "run_json_sha256": run_sha, "engine": "kitesurf", "arm": arm, "rep": rep,
                    "warmup": rep < args.warmup, "url": url, "ok": ok, "wall_ms": wall_ms,
                    "loadavg1": load, "cgroup_cpu_usec_before": cpu_before,
                    "cgroup_cpu_usec_after": cpu_after, "proc_cpu_ms": proc_delta,
                    "memory_current_after": mem_after, "memory_anon_after": anon_after,
                    "driver": json.dumps(result, separators=(",", ":"), default=repr),
                }) + "\n")
                out.flush()
                if not ok:
                    print(f"rep {rep} {arm} FAILED ({url}): {result.get('stage')} {result.get('error')}",
                          file=sys.stderr, flush=True)
    print(f"{total * len(arms)} requests, {failures} failed", file=sys.stderr)
    return 0


def summarize(args):
    rows = [json.loads(line) for line in open(args.jsonl) if line.strip()]
    completed = {}
    correctness = {}
    for engine in ("kitesurf", "chromium"):
        path = os.path.join(args.correctness, engine, "correctness.jsonl")
        if os.path.exists(path):
            entries = [json.loads(line) for line in open(path) if line.strip()]
            correctness[engine] = {e["url"]: {k: e.get(k) for k in (
                "site", "completed", "shot_ok", "html_ok", "blank", "dominant_fraction",
                "width", "height", "html_bytes", "shot_error", "html_error")} for e in entries}
    completed = {url: c["completed"] for url, c in correctness.get("kitesurf", {}).items()}

    def pct(values, p):
        values = sorted(values)
        if not values:
            return None
        if p == 50:
            return round(statistics.median(values), 1)
        return round(values[max(0, -(-p * len(values) // 100) - 1)], 1)

    arms = {}
    for arm in sorted({r["arm"] for r in rows}):
        measured = [r for r in rows if r["arm"] == arm and not r["warmup"]]
        # A request counts as a success only if the driver finished AND the page
        # completed in the correctness pass (not blank, both operations worked).
        def success(r):
            return r["ok"] and completed.get(r["url"], False)
        good = [r for r in measured if success(r)]
        per_page = {}
        for url in sorted({r["url"] for r in measured}):
            mine = [r for r in measured if r["url"] == url]
            ok_mine = [r for r in mine if success(r)]
            drivers = [json.loads(r["driver"]) for r in mine]
            per_page[url] = {
                "attempts": len(mine), "succeeded": len(ok_mine),
                "driver_ok": sum(r["ok"] for r in mine),
                "page_completed_in_check": completed.get(url),
                "p50_ms": pct([r["wall_ms"] for r in ok_mine], 50),
                "distinct_images": len({d.get("image_sha256") for d in drivers if d.get("image_sha256")}),
                "failure_stages": sorted({d.get("stage") for d in drivers if not d.get("ok") and d.get("stage")}),
            }
        cpu = [(r["cgroup_cpu_usec_after"] - r["cgroup_cpu_usec_before"]) / 1000.0 for r in good]
        workerd = [r["proc_cpu_ms"].get("workerd", 0.0) for r in good]
        arms[arm] = {
            "attempts": len(measured), "succeeded": len(good),
            "failed": len(measured) - len(good),
            "p50_ms": pct([r["wall_ms"] for r in good], 50),
            "p95_ms": pct([r["wall_ms"] for r in good], 95),
            "p50_ms_all_attempts": pct([r["wall_ms"] for r in measured], 50),
            "cgroup_cpu_ms_p50": pct(cpu, 50), "cgroup_cpu_ms_mean": round(statistics.fmean(cpu), 1) if cpu else None,
            "workerd_cpu_ms_p50": pct(workerd, 50),
            "memory_current_max_bytes": max((r["memory_current_after"] for r in measured), default=None),
            "memory_anon_max_bytes": max((r.get("memory_anon_after", 0) for r in measured), default=None),
            "memory_anon_first_last_bytes": ([measured[0].get("memory_anon_after"),
                                              measured[-1].get("memory_anon_after")]
                                             if measured else None),
            "per_page": per_page,
        }
    loads = [r["loadavg1"] for r in rows if isinstance(r.get("loadavg1"), (int, float))]
    summary = {
        "engine": "kitesurf",
        "run_json_sha256": rows[0]["run_json_sha256"] if rows else None,
        "success_rule": "driver ok AND the page completed (not blank, screenshot and HTML) in render_check",
        "p50_convention": "statistics.median over successful measured requests",
        "arms": arms,
        "correctness": correctness,
        "pages_completed": {e: sum(bool(c["completed"]) for c in v.values()) for e, v in correctness.items()},
        "pages": len(completed),
        "memory_peak_bytes": int(args.memory_peak),
        "loadavg1": {"min": min(loads), "median": statistics.median(loads), "max": max(loads)} if loads else None,
    }
    with open(args.out + ".tmp", "w") as handle:
        json.dump(summary, handle, indent=1, sort_keys=True)
        handle.write("\n")
    os.replace(args.out + ".tmp", args.out)
    for arm, a in arms.items():
        print(f"kitesurf/{arm}: {a['succeeded']}/{a['attempts']} succeeded, p50 {a['p50_ms']} ms, "
              f"cgroup CPU p50 {a['cgroup_cpu_ms_p50']} ms", file=sys.stderr)
    return 0


COMPLETION_ARTIFACTS = ("kitesurf-build.json", "run.json", "kitesurf.jsonl", "summary.json",
                        "correctness/kitesurf/correctness.jsonl",
                        "correctness/chromium/correctness.jsonl")


def complete(args):
    """complete.json: schema_version 1, the run id, and each artifact's size and sha256."""
    import hashlib
    artifacts = []
    for rel in COMPLETION_ARTIFACTS:
        path = os.path.join(args.results, rel)
        if not os.path.exists(path):
            if rel == "correctness/chromium/correctness.jsonl":
                continue  # CHROMIUM_CHECK=0
            raise SystemExit(f"FAILED: {rel} is missing; the run is not complete")
        with open(path, "rb") as handle:
            data = handle.read()
        artifacts.append({"path": rel, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    record = {"schema_version": 1, "run_id": args.run_id, "artifacts": artifacts}
    with open(args.out + ".tmp", "w") as handle:
        json.dump(record, handle, sort_keys=True)
        handle.write("\n")
    os.replace(args.out + ".tmp", args.out)
    return 0


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "complete":
        ap = argparse.ArgumentParser()
        ap.add_argument("cmd")
        ap.add_argument("--results", required=True)
        ap.add_argument("--run-id", required=True)
        ap.add_argument("--out", required=True)
        return complete(ap.parse_args())
    if len(sys.argv) > 1 and sys.argv[1] == "summarize":
        ap = argparse.ArgumentParser()
        ap.add_argument("cmd")
        ap.add_argument("--jsonl", required=True)
        ap.add_argument("--run-json", required=True)
        ap.add_argument("--memory-peak", required=True)
        ap.add_argument("--correctness", required=True)
        ap.add_argument("--out", required=True)
        return summarize(ap.parse_args())
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--address", required=True)
    ap.add_argument("--urls", required=True)
    ap.add_argument("--arms", required=True)
    ap.add_argument("--reps", type=int, required=True)
    ap.add_argument("--warmup", type=int, required=True)
    ap.add_argument("--cgroup", required=True)
    ap.add_argument("--loadavg-file", default="/proc/loadavg")
    ap.add_argument("--run-json", required=True)
    ap.add_argument("--out", required=True)
    return measure(ap.parse_args())


if __name__ == "__main__":
    sys.exit(main())
