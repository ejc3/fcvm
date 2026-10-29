#!/usr/bin/env python3
"""validate.py <run dir>: Robot Park's pipe traffic counted by strace in the guest against the
host counters for the puid that is silent while the app is stopped."""
import json, re, sys, os
from collections import defaultdict
run = sys.argv[1]
m = {json.loads(l)["ev"]: json.loads(l)["t_ms"] for l in open(os.path.join(run, "validate.markers.jsonl"))}
lines = [json.loads(l) for l in open(os.path.join(run, "stats.jsonl")) if l.strip()]
# strace: "<tid> write(12</dev/goldfish_pipe>, ..., 64) = 64", or split as unfinished/resumed
w = r = nr = nw = 0
pending = {}
fdre = re.compile(r"^(\d+)\s+(read|write|readv|writev)\((\d+)<([^>]*)>")
for l in open(os.path.join(run, "validate.strace.txt")):
    l = l.rstrip()
    mm = fdre.match(l)
    if mm:
        tid, call, fd, path = mm.groups()
        if "unfinished" in l:
            pending[tid] = (call, path)
            continue
    else:
        mr = re.match(r"^(\d+)\s+<\.\.\. (read|write|readv|writev) resumed>", l)
        if not mr or mr.group(1) not in pending:
            continue
        call, path = pending.pop(mr.group(1))
    if "goldfish_pipe" not in path and "qemu_pipe" not in path:
        continue
    ret = re.search(r"= (-?\d+)", l)
    if not ret or int(ret.group(1)) < 0:
        continue
    n = int(ret.group(1))
    if call.startswith("write"):
        w += n; nw += 1
    else:
        r += n; nr += 1
# host: per puid totals inside [stopped, resumed]; the app's puid is the one with traffic
# while running and none in the stopped spans.
def tot(t0, t1):
    d = defaultdict(lambda: defaultdict(int))
    for l in lines:
        if l["t_ms"] > t0 and l["t_ms"] - 1000 < t1:
            for t in l["threads"]:
                for k in ("read", "replies", "reply_bytes"):
                    d[t["puid"]][k] += t[k]
    return d
quiet1 = tot(m["stopped"] + 1000, m["cont"])
run_ = tot(m["stopped"], m["resumed"])
quiet2 = tot(m["stopped_again"] + 1000, m["resumed"])
cands = [p for p in run_ if run_[p]["read"] > 0 and quiet1[p]["read"] == 0 and quiet2[p]["read"] == 0]
out = {"strace": {"guest_wrote": w, "guest_read": r, "writes": nw, "reads": nr}, "candidates": {}}
for p in cands:
    h = run_[p]
    out["candidates"][p] = {"host_read": h["read"], "host_replies": h["replies"], "host_reply_bytes": h["reply_bytes"],
                            "read_vs_strace_write": h["read"] / w if w else None,
                            "reply_bytes_vs_strace_read": h["reply_bytes"] / r if r else None}
json.dump(out, open(os.path.join(run, "validate.json"), "w"), indent=1)
print(json.dumps(out, indent=1))
