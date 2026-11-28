#!/usr/bin/env python3
"""One run of fleetfield.sh as a few lines: fleetfield_summary.py OUT

fleetfield_summary.py --all-up OUT N prints only the time the N servers took to come up, from the
run's coordinator log or the excerpt of it a record keeps (coord-up.log)."""
import datetime, json, os, re, sys
out = sys.argv[1]


def all_up_from_log(path, n):
    """How long the N servers took to come up, from the coordinator's own log: from the gold
    snapshot made in this run (or the agent's hello, where the gold was already there) to the last
    server's first "server up". None unless every server came up. The client's own figure
    (`restore_all_s`) starts at its first sight of a restoring server, which can come late."""
    stamp = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z ")
    hello = gold = last = None
    up = set()
    for line in open(path, errors="replace"):
        m = stamp.match(line)
        if not m:
            continue
        t = datetime.datetime.fromisoformat(m.group(1)).timestamp()
        if hello is None and "agent hello" in line:
            hello = t
        elif gold is None and "gold snapshot made" in line:
            gold = t
        elif "server up" in line and " generation=1 " in line:
            name = re.search(r"server=(\S+)", line)
            up.add(name.group(1) if name else len(up))
            last = t
    start = gold if gold is not None else hello
    if start is None or last is None or len(up) < n:
        return None
    return {"from": "gold snapshot made" if gold is not None else "agent hello", "s": round(last - start, 1)}


def up_log(out):
    """The run's coordinator log, or the excerpt of it kept with a record: its hello, gold and "server up" lines."""
    for name in ("coord.log", "coord-up.log"):
        if os.path.exists(f"{out}/{name}"):
            return f"{out}/{name}"
    return None


if out == "--all-up":
    print(json.dumps(all_up_from_log(up_log(sys.argv[2]), int(sys.argv[3]))))
    sys.exit(0)
ev = [json.loads(l) for l in open(f"{out}/run.jsonl")]
samples = [json.loads(l) for l in open(f"{out}/samples.jsonl")]
disk, neigh = [], {"ndisc": [], "arp": []}
for l in open(f"{out}/disk.log"):
    p = l.split()
    kv = dict(x.split("=", 1) for x in p[1:] if "=" in x)
    disk.append((int(p[0]), int(kv["store_mib"])))
    for k in neigh:
        if kv.get(k, "").isdigit():
            neigh[k].append(int(kv[k]))
n = next(e["n"] for e in ev if e["event"] == "start")
res = {"servers": n, "host": open(f"{out}/host.txt").read().strip().splitlines()}
if up_log(out):
    res["all_up_s"] = all_up_from_log(up_log(out), n)
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
if "restore_all_s" not in res and samples:
    # A run in which not every server came to run: where it stood at its last sample.
    last = samples[-1]
    res["never_all_running"] = {"at_s": last["at"], "phases": last["phases"], "running_unanswered": last["tps"]["no_answer"], "running_with_a_rate": last["tps"]["n"],
                                "host": last.get("host")}
# The kernel's neighbour entries over every namespace (fleetfield.sh logs them): the most seen.
if neigh["ndisc"] or neigh["arp"]:
    res["neighbour_entries_max"] = {"ipv6": max(neigh["ndisc"], default=None), "ipv4": max(neigh["arp"], default=None),
                                    "ipv6_per_server": round(max(neigh["ndisc"]) / n, 1) if neigh["ndisc"] else None}
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
