#!/usr/bin/env python3
"""One run of fleetfield.sh as a few lines: fleetfield_summary.py OUT"""
import json, sys
out = sys.argv[1]
ev = [json.loads(l) for l in open(f"{out}/run.jsonl")]
samples = [json.loads(l) for l in open(f"{out}/samples.jsonl")]
disk = []
for l in open(f"{out}/disk.log"):
    p = l.split()
    disk.append((int(p[0]), int(dict(x.split("=", 1) for x in p[1:] if "=" in x)["store_mib"])))
n = next(e["n"] for e in ev if e["event"] == "start")
res = {"servers": n, "host": open(f"{out}/host.txt").read().strip().splitlines()}
for e in ev:
    if e["event"] == "all_running":
        res["restore_all_s"] = round(e["restore_all_s"], 1)
        res["launch_ms_first_wave"] = e["summary"]["launch_ms"]
    if e["event"] == "mark":
        key = "late" if e.get("late") else f"{int(e['since_all_up'])}s"
        res[f"mark_{key}"] = {"since_all_up_s": e["since_all_up"], "oldest_age_s": e.get("oldest_age_s"), "tps": e["summary"]["tps"], "busy_cpus": e["host"]["busy"],
                             "busy_per_server": round(e["host"]["busy"] / n, 3), "per_server_mib": e["per_server_mib"], "host_used_mib": e["host"]["mem_used_mib"], "meminfo": e["meminfo"]}
    if e["event"] == "reset_all_done":
        res["reset_all_s"] = round(e["reset_all_s"], 1)
        res["launch_ms_after_reset"] = e["summary"]["launch_ms"]
# The steady state: from every server running until a reset of all was sent (its wave is not a steady reading).
at = {e["event"]: e["at"] for e in ev if e["event"] in ("all_running", "reset_all_sent") and "at" in e}
reset_since = at["reset_all_sent"] - at["all_running"] if len(at) == 2 else None
res["reset_sent_s_after_all_ran"] = None if reset_since is None else round(reset_since, 1)
running = [s for s in samples if s["since_all_up"] is not None and (reset_since is None or s["since_all_up"] < reset_since)]
res["samples_all_up"] = len(running)
# A reading is one server's tick rate in one sample; a running server whose tile gave none is unanswered.
res["readings"] = sum(s["tps"].get("n", 0) + s["tps"]["no_answer"] for s in running)
res["readings_under_19_5"] = sum(s["tps"]["below_19_5"] for s in running)
res["readings_unanswered"] = sum(s["tps"]["no_answer"] for s in running)
# Every sample that had one of either: [seconds since every server ran, lowest reading, under 19.5, unanswered].
res["low_or_unanswered"] = [[s["since_all_up"], s["tps"]["min"], s["tps"]["below_19_5"], s["tps"]["no_answer"]] for s in running if s["tps"]["below_19_5"] or s["tps"]["no_answer"]]
if "host" in (running[0] if running else {}):
    by = {}
    for s in running:
        by.setdefault(int(s["since_all_up"] // 60), []).append(s["host"]["busy"])
    res["busy_cpus_per_server_by_minute"] = {m: round(sum(v) / len(v) / n, 3) for m, v in sorted(by.items()) if m in (0, 1, 5, 10, 15, 20, 25, 29)}
res["samples_with_a_reading_under_19_5"] = sum(1 for s in running if s["tps"]["below_19_5"] > 0)
res["lowest_tps_reading"] = min((s["tps"]["min"] for s in running if s["tps"]["min"] is not None), default=None)
res["readings_no_answer_max"] = max((s["tps"]["no_answer"] for s in running), default=None)
res["store_mib"] = {"start": disk[0][1], "max": max(d[1] for d in disk), "end": disk[-1][1], "per_server_at_max": round((max(d[1] for d in disk) - disk[0][1]) / n, 1)}
last = samples[-1]["wire"]
res["wire"] = {"snapshots": last["snapshots"], "snapshot_bytes_mean": round(last["snap_bytes"] / max(1, last["snapshots"])), "status_msgs": last["statuses"], "status_bytes_mean": round(last["status_bytes"] / max(1, last["statuses"]))}
print(json.dumps(res, indent=1))
