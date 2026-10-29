#!/usr/bin/env python3
"""validate.py <run dir>: Robot Park's pipe traffic counted by strace in the guest against the
host counters for the puid that is silent while the app is stopped."""
import json, re, sys, os
from collections import defaultdict
run = sys.argv[1]
ev = [json.loads(l) for l in open(os.path.join(run, "validate.markers.jsonl"))]
m = {e["ev"]: e["t_ms"] for e in ev}
# guest clock - host clock, from the tightest of the date round trips
clk = min((e for e in ev if e["ev"].startswith("clock")), key=lambda e: e["after"] - e["before"])
offset_ms = clk["guest_ns"] / 1e6 - (clk["before"] + clk["after"]) / 2
rt_ms = clk["after"] - clk["before"]
lines = [json.loads(l) for l in open(os.path.join(run, "stats.jsonl")) if l.strip()]
# strace: "<tid> write(12</dev/goldfish_pipe>, ..., 64) = 64", or split as unfinished/resumed
w = r = nr = nw = 0
pending = {}
events = []  # (host ms, wrote, read)
fdre = re.compile(r"^(\d+)\s+([0-9.]+)\s+(read|write|readv|writev)\((\d+)<([^>]*)>")
for l in open(os.path.join(run, "validate.strace.txt")):
    l = l.rstrip()
    mm = fdre.match(l)
    if mm:
        tid, ts, call, fd, path = mm.groups()
        if "unfinished" in l:
            pending[tid] = (call, path)
            continue
    else:
        mr = re.match(r"^(\d+)\s+([0-9.]+)\s+<\.\.\. (read|write|readv|writev) resumed>", l)
        if not mr or mr.group(1) not in pending:
            continue
        ts = mr.group(2)
        call, path = pending.pop(mr.group(1))
    if "goldfish_pipe" not in path and "qemu_pipe" not in path:
        continue
    ret = re.search(r"= (-?\d+)", l)
    if not ret or int(ret.group(1)) < 0:
        continue
    n = int(ret.group(1))
    t_host = float(ts) * 1000 - offset_ms
    if call.startswith("write"):
        w += n; nw += 1
        events.append((t_host, n, 0, 0))
    else:
        r += n; nr += 1
        events.append((t_host, 0, n, 1))
# host: per puid totals inside [stopped, resumed]; the app's puid is the one with traffic
# while running and none in the stopped spans.
def tot(t0, t1, inside=False):
    """per puid totals of the lines overlapping [t0, t1], or with inside, of those fully in it"""
    d = defaultdict(lambda: defaultdict(int))
    for l in lines:
        hit = (l["t_ms"] - 1000 >= t0 and l["t_ms"] <= t1) if inside else (l["t_ms"] > t0 and l["t_ms"] - 1000 < t1)
        if hit:
            for t in l["threads"]:
                for k in ("read", "replies", "reply_bytes"):
                    d[t["puid"]][k] += t[k]
    return d
quiet1 = tot(m["stopped"] + 1000, m["cont"], inside=True)
run_ = tot(m["stopped"], m["resumed"])
quiet2 = tot(m["stopped_again"] + 1000, m["resumed"], inside=True)
cands = [p for p in run_ if run_[p]["read"] > 0 and quiet1[p]["read"] == 0 and quiet2[p]["read"] == 0
         and sum(1 for l in lines if l["t_ms"] - 1000 >= m["stopped"] + 1000 and l["t_ms"] <= m["cont"]) >= 2]
out = {"strace": {"guest_wrote": w, "guest_read": r, "writes": nw, "reads": nr}, "candidates": {}}
for p in cands:
    h = run_[p]
    out["candidates"][p] = {"host_read": h["read"], "host_replies": h["replies"], "host_reply_bytes": h["reply_bytes"],
                            "read_vs_strace_write": h["read"] / w if w else None,
                            "reply_bytes_vs_strace_read": h["reply_bytes"] / r if r else None}
# the exact comparison: whole host seconds inside the strace capture, both sides summed there
t_first, t_last = min(e[0] for e in events), max(e[0] for e in events)
inside = [l for l in lines if l["t_ms"] - 1000 >= t_first and l["t_ms"] <= t_last]
for p in cands:
    hw = sum(t["read"] for l in inside for t in l["threads"] if t["puid"] == p)
    hrb = sum(t["reply_bytes"] for l in inside for t in l["threads"] if t["puid"] == p)
    hr = sum(t["replies"] for l in inside for t in l["threads"] if t["puid"] == p)
    sw = sr = sn = 0
    for l in inside:
        for e in events:
            if l["t_ms"] - 1000 <= e[0] < l["t_ms"]:
                sw += e[1]; sr += e[2]; sn += e[3]
    out["candidates"][p]["windowed"] = {"seconds": len(inside), "host_read": hw, "strace_wrote": sw,
        "host_reply_bytes": hrb, "strace_read": sr, "host_replies": hr, "strace_reads": sn,
        "read_error": hw / sw - 1 if sw else None, "reply_bytes_error": hrb / sr - 1 if sr else None}
out["clock"] = {"guest_minus_host_ms": offset_ms, "round_trip_ms": rt_ms, "capture_ms": [t_first - m["stopped"], t_last - m["stopped"]], "seconds_inside": len(inside)}
json.dump(out, open(os.path.join(run, "validate.json"), "w"), indent=1)
print(json.dumps(out, indent=1))
