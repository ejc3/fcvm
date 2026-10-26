#!/usr/bin/env python3
"""Where the added time goes when a scale run's offered rate rises (#1159).

Reads one scale run directory (requests.jsonl, bursts.jsonl, host-samples.jsonl)
and, optionally, the replay server's --listen-log from the same run, and prints
per offered rate:

  * medians and p95 of each request stage over the successful requests launched
    in a scoring window (spawn to port, wait for the page target, navigate,
    screenshot, whole request), and how many of those requests took more than 2 s;
  * from the listen log, over the same windows: connections accepted, how busy
    the accept thread was (the largest one-second share, and the mean), the
    longest accept()/handshake, the longest handler thread start, requests
    answered and the handler time of the slowest, the host's accept-queue
    overflows and SYN drops, and the deepest accept queue seen.

The listen log is stamped with wall-clock time and the run with CLOCK_MONOTONIC;
host-samples.jsonl holds both clocks and joins them.

Usage: reqscale_stages.py RUN_DIR [--listen-log PATH] [--json-out PATH]
"""

import argparse
import json
import statistics
import sys

STAGES = ("spawn_to_port_ms", "resolve_ms", "navigate_ms", "screenshot_ms", "total_ms")
SLOW_MS = 2000.0


def read_jsonl(path):
    with open(path) as handle:
        return [json.loads(line) for line in handle if line.strip()]


def p95(values):
    values = sorted(values)
    if not values:
        return None
    return values[min(len(values) - 1, int(0.95 * len(values) + 0.999999) - 1)]


def stage_values(request):
    stages = (request.get("render") or {}).get("stages") or {}
    out = {"spawn_to_port_ms": request.get("spawn_to_port_ms")}
    for name in STAGES[1:]:
        out[name] = stages.get(name)
    out["blocking_ms"] = request.get("blocking_ms")
    return out


def clock_offset_ns(samples):
    """wall_ns - monotonic_ns, the median over the host samples."""
    offsets = [s["captured_wall_ns"] - s["captured_monotonic_ns"] for s in samples
               if "captured_wall_ns" in s and "captured_monotonic_ns" in s]
    if not offsets:
        raise ValueError("host-samples.jsonl holds no sample with both clocks")
    return int(statistics.median(offsets))


def summarize_requests(requests, windows):
    """{rate: stats} over successful requests launched inside a scoring window."""
    by_rate = {}
    for request in requests:
        if request.get("kind") != "request" or request.get("population") != "scored":
            continue
        window = windows.get(request["burst_id"])
        if window is None:
            continue
        launched = request["actual_launch_ns"]
        if not (window["score_start_ns"] <= launched < window["score_end_ns"]):
            continue
        slot = by_rate.setdefault(window["target_rps"], {"ok": [], "failed": 0})
        if not request.get("ok"):
            slot["failed"] += 1
            continue
        slot["ok"].append(stage_values(request))
    out = {}
    for rate, slot in sorted(by_rate.items()):
        row = {"rate": rate, "scored_ok": len(slot["ok"]), "scored_failed": slot["failed"]}
        for name in STAGES + ("blocking_ms",):
            values = [v[name] for v in slot["ok"] if v.get(name) is not None]
            row[name] = ({"n": len(values), "median": round(statistics.median(values), 1),
                          "p95": round(p95(values), 1)} if values else None)
        slow = [v for v in slot["ok"] if (v.get("navigate_ms") or 0) > SLOW_MS]
        row["navigate_over_2s"] = len(slow)
        out[rate] = row
    return out


def summarize_listen(rows, windows, offset_ns):
    """{rate: stats} of the listen-log rows whose second falls in a scoring window."""
    spans = {}
    for burst in windows.values():
        spans.setdefault(burst["target_rps"], []).append(
            ((burst["score_start_ns"] + offset_ns) / 1e9, (burst["score_end_ns"] + offset_ns) / 1e9))
    out = {}
    for rate, ranges in sorted(spans.items()):
        picked = [r for r in rows if any(a <= r["ts"] < b for a, b in ranges)]
        seconds = sum(b - a for a, b in ranges)
        agg = {"rate": rate, "window_s": round(seconds, 1)}
        for listener in ("tls", "http"):
            mine = [r for r in picked if r.get("listener") == listener]
            if not mine:
                continue
            busy = [r["accept_busy"] for r in mine]
            agg[listener] = {
                "accepts": sum(r["accepts"] for r in mine),
                "accept_errors": sum(r["accept_errors"] for r in mine),
                "accept_busy_mean": round(sum(r["accept_ms_sum"] for r in mine) / (seconds * 1000.0), 4),
                "accept_busy_max_second": max(busy),
                "accept_ms_mean": round(sum(r["accept_ms_sum"] for r in mine)
                                        / max(1, sum(r["accepts"] for r in mine)), 3),
                "accept_ms_max": max(r["accept_ms_max"] for r in mine),
                "handshakes_in_handler": sum(r["handshakes"] for r in mine),
                "handshake_ms_max": max(r["handshake_ms_max"] for r in mine),
                "thread_start_ms_max": max(r["thread_start_ms_max"] for r in mine),
                "requests": sum(r["requests"] for r in mine),
                "request_ms_mean": round(sum(r["request_ms_sum"] for r in mine)
                                         / max(1, sum(r["requests"] for r in mine)), 3),
                "request_ms_max": max(r["request_ms_max"] for r in mine),
                "thread_start_ms_mean": round(sum(r["thread_start_ms_sum"] for r in mine)
                                              / max(1, sum(r["thread_starts"] for r in mine)), 3),
                "active_peak": max(r["active_peak"] for r in mine),
            }
        kernel = [r for r in picked if r.get("listener") == "kernel"]
        if kernel:
            depth = [q[0] for r in kernel for q in (r.get("accept_queue") or {}).values()]
            agg["kernel"] = {
                "listen_overflows": sum(r.get("listen_overflows") or 0 for r in kernel),
                "listen_drops": sum(r.get("listen_drops") or 0 for r in kernel),
                "accept_queue_peak": max(depth) if depth else None,
            }
        out[rate] = agg
    return out


def summarize(run_dir, listen_log=None):
    requests = read_jsonl(f"{run_dir}/requests.jsonl")
    bursts = [b for b in read_jsonl(f"{run_dir}/bursts.jsonl") if b.get("kind") == "burst"]
    windows = {b["burst_id"]: {"target_rps": b["target_rps"],
                               "score_start_ns": b["score_start_ns"],
                               "score_end_ns": b["score_end_ns"]}
               for b in bursts if b.get("population") == "scored"}
    result = {"requests": summarize_requests(requests, windows)}
    if listen_log:
        offset = clock_offset_ns(read_jsonl(f"{run_dir}/host-samples.jsonl"))
        rows = [r for r in read_jsonl(listen_log) if r.get("listener") != "start"]
        starts = [r for r in read_jsonl(listen_log) if r.get("listener") == "start"]
        result["server"] = starts[0] if starts else None
        result["listen"] = summarize_listen(rows, windows, offset)
    return result


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("run_dir")
    ap.add_argument("--listen-log")
    ap.add_argument("--json-out")
    args = ap.parse_args(argv)
    result = summarize(args.run_dir, args.listen_log)
    text = json.dumps(result, indent=1, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w") as handle:
            handle.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
