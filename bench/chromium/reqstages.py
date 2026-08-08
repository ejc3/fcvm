#!/usr/bin/env python3
"""Attribute every wait in the exec arm from RUST_LOG=fcvm=debug timestamps.

AGENTS.md defect 4: at `info` the exec client's retry ladder is invisible, which
made ~70 ms of a 310 ms gap unattributable. At `debug` every step is stamped, so
the exec arm decomposes without guessing -- and the guest-python startup, which
no fcvm log can see, falls out as the residual between the exec GO and
render.py's own first timestamp.

Emits medians with a bootstrap CI so each stage is quotable at its own precision.
"""

import argparse
import glob
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from reqanalyze import fmt, median_ci  # noqa: E402

TS = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z)")


def ts(line):
    m = TS.match(line)
    if not m:
        return None
    h = m.group(1)
    hh, mm, rest = h.split("T")[1].split(":")[0], h.split("T")[1].split(":")[1], \
        h.split("T")[1].split(":")[2].rstrip("Z")
    return int(hh) * 3600 + int(mm) * 60 + float(rest)


MARKS = [
    ("holder_ready", "namespace ready (nsenter probe succeeded)"),
    ("pasta_ready", "pasta ready (PID file created)"),
    ("resume_done", "VM resume completed"),
    ("exec_start", "executing command in VM"),
    ("handshake_ack", "exec handshake: ACK received"),
    ("handshake_done", "exec handshake complete"),
    ("exec_finished", "exec finished, cleaning up"),
]


def parse(path):
    out = {}
    first = None
    render = {}
    retries = 0
    for line in open(path, errors="replace"):
        t = ts(line)
        if t is not None and first is None:
            first = t
        for key, needle in MARKS:
            if needle in line and key not in out and t is not None:
                out[key] = t
        if "RENDER_OK" in line:
            for tok in line.split():
                if "=" in tok:
                    k, v = tok.split("=", 1)
                    try:
                        render[k] = float(v)
                    except ValueError:
                        pass
            # render.py's stdout has no fcvm timestamp; use the NEXT fcvm line
            # ("exec finished") as its upper bound instead of inventing one.
        if "retrying" in line or "retry attempt" in line:
            retries += 1
    out["_first"] = first
    out["_render"] = render
    out["_retries"] = retries
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("globs", nargs="+")
    a = ap.parse_args()
    files = []
    for g in a.globs:
        files += sorted(glob.glob(g))
    stages = {}
    retries_total = 0
    n_ok = 0
    for f in files:
        p = parse(f)
        r = p.get("_render") or {}
        if not r or "total_ms" not in r:
            continue
        n_ok += 1
        retries_total += p["_retries"]

        def d(a_, b_):
            if p.get(a_) is None or p.get(b_) is None:
                return None
            return (p[b_] - p[a_]) * 1000

        add = stages.setdefault
        add("spawn->holder_ready", []).append(d("_first", "holder_ready"))
        add("holder_ready->pasta_ready", []).append(d("holder_ready", "pasta_ready"))
        add("pasta_ready->resume_done", []).append(d("pasta_ready", "resume_done"))
        add("resume_done->exec_start", []).append(d("resume_done", "exec_start"))
        add("exec handshake (start->GO)", []).append(d("exec_start", "handshake_done"))
        add("handshake ACK wait", []).append(d("exec_start", "handshake_ack"))
        # Residual: from exec GO until render.py's own measured work ends.
        # render.py's total_ms covers connect+navigate+screenshot ONLY, so this
        # residual IS the guest python interpreter boot + imports. It is a
        # residual and is labelled as one -- no fcvm log can see inside the guest.
        gо = d("handshake_done", "exec_finished")
        if gо is not None:
            add("GO->exec_finished (guest python up + render + exit)", []).append(gо)
            add("  = render.py measured work", []).append(r["total_ms"])
            add("  = RESIDUAL guest python up + exit", []).append(gо - r["total_ms"])
        for k in ("connect_ms", "navigate_ms", "screenshot_ms", "total_ms"):
            if k in r:
                add(f"render.py {k}", []).append(r[k])

    print(f"parsed {n_ok} exec logs;  exec-client retry lines seen: {retries_total}")
    print(f"{'stage':52s} {'median [95% CI]'}")
    for k, vs in stages.items():
        vs = [v for v in vs if v is not None]
        if vs:
            print(f"{k:52s} {fmt(*median_ci(vs))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
