#!/usr/bin/env python3
"""Summarises one run directory of the phone remoting measurement.

  analyze.py <gfxstream source dir> <run dir> [workload ...]   -> <run dir>/summary.json

A run dir holds stats.jsonl (the emulator's GFXSTREAM_STATS) and, per workload,
<w>.markers.jsonl, <w>.gfxinfo.txt, <w>.capture.json, and optionally <w>.x264.json and
<w>.compress.json (written by encode.sh and compress.py).
"""
import json
import os
import re
import sys
from collections import defaultdict

SRC, RUN = sys.argv[1], sys.argv[2]
ONLY = sys.argv[3:]


def opcode_names():
    names = {}
    for rel in ["host/gl/gles1_dec/gles1_opcodes.h", "host/gl/gles2_dec/gles2_opcodes.h",
                "host/renderControl_dec/renderControl_opcodes.h",
                "host/vulkan/cereal/common/goldfish_vk_marshaling.h"]:
        p = os.path.join(SRC, rel)
        for m in re.finditer(r"#define OP_(\w+)\s+(\d+)", open(p).read()):
            n, v = m.group(1), int(m.group(2))
            if rel.endswith("gles1_opcodes.h") and v in names:
                continue
            names.setdefault(v, n if not rel.endswith("gles1_opcodes.h") else "gles1:" + n)
    names[0xFFFFFFFF] = "(partial packet)"
    names[0] = "(no opcode)"
    return names


NAMES = opcode_names()

TEX = re.compile(r"^(gles1:)?gl(Compressed)?Tex(Sub)?Image[23]D(OES)?$")
BUF = re.compile(r"^(gles1:)?gl(BufferData|BufferDataSyncAEMU|BufferSubData|UnmapBuffer\w*AEMU|FlushMappedBufferRange\w*AEMU)$")
CLIENT_ARRAYS = re.compile(r"^(gles1:)?gl(\w*PointerData\w*|Draw\w*Data(Null)?AEMU|DrawElementsData)$")
CB_UPLOAD = re.compile(r"^rcUpdateColorBuffer$")
SWAP = {"rcFlushWindowColorBuffer", "rcFlushWindowColorBufferAsync", "rcFlushWindowColorBufferAsyncWithFrameNumber"}
COMPOSE = {"rcCompose", "rcComposeAsync", "rcComposeWithoutPost", "rcComposeAsyncWithoutPost"}


def name(op):
    return NAMES.get(int(op), f"op{op}")


def load_lines():
    out = []
    for l in open(os.path.join(RUN, "stats.jsonl")):
        l = l.strip()
        if l:
            out.append(json.loads(l))
    return out


def markers(w):
    m = {}
    for l in open(os.path.join(RUN, f"{w}.markers.jsonl")):
        e = json.loads(l)
        m[e["ev"]] = e
    return m


def window(lines, t0, t1):
    """Lines whose second (t_ms-1000, t_ms] lies inside [t0, t1]."""
    return [l for l in lines if l["t_ms"] - 1000 >= t0 and l["t_ms"] <= t1]


def overlapping(lines, t0, t1):
    return [l for l in lines if l["t_ms"] > t0 and l["t_ms"] - 1000 < t1]


def agg(ls, puids=None):
    a = defaultdict(int)
    ops = defaultdict(lambda: [0, 0, 0, 0])
    threads = defaultdict(lambda: defaultdict(int))
    for l in ls:
        for t in l["threads"]:
            if puids is not None and t["puid"] not in puids:
                continue
            for k in ("read", "gles1", "gles2", "rc", "vk", "replies", "reply_bytes"):
                a[k] += t[k]
            for op, v in t["ops"].items():
                o = ops[int(op)]
                for i in range(4):
                    o[i] += v[i]
                n = name(op)
                th = threads[(t["puid"], t["id"])]
                if n in SWAP:
                    th["swaps"] += v[0]
                if n in COMPOSE:
                    th["composes"] += v[0]
                th["bytes_" + ("vk" if n.startswith("vk") else "rc" if n.startswith("rc") else "gl")] += v[1]
            threads[(t["puid"], t["id"])]["replies"] += t["replies"]
            threads[(t["puid"], t["id"])]["read"] += t["read"]
    a["fence_signals"] = sum(l["fence_signals"] for l in ls)
    cat = defaultdict(int)
    for op, (c, b, r, rb) in ops.items():
        n = name(op)
        if TEX.match(n):
            cat["texture_upload"] += b
        elif BUF.match(n):
            cat["buffer_upload"] += b
        elif CLIENT_ARRAYS.match(n):
            cat["client_arrays"] += b
        elif CB_UPLOAD.match(n):
            cat["colorbuffer_upload"] += b
        if n in SWAP:
            cat["swaps"] += c
        if n in COMPOSE:
            cat["composes"] += c
        if n == "rcUpdateColorBufferDMA":
            cat["colorbuffer_dma_calls"] += c
    return a, ops, threads, cat


def puid_set(ls):
    s = set()
    for l in ls:
        for t in l["threads"]:
            s.add(t["puid"])
    return s


def summarize(lines, w):
    m = markers(w)
    t_launch, t_first = m["launch_start"]["t_ms"], m["launch_done"]["t_ms"]
    t0, t1 = m["steady_start"]["t_ms"], m["steady_end"]["t_ms"]
    before = puid_set(window(lines, t_launch - 60000, t_launch))
    after = puid_set(overlapping(lines, t_launch, t1))
    new = after - before
    steady = window(lines, t0, t1)
    secs = len(steady)
    a, ops, threads, cat = agg(steady)
    app_puids = new if new else None
    aa, aops, athreads, acat = agg(steady, app_puids) if app_puids else (a, ops, threads, cat)
    app_frames = acat["swaps"]
    # the app's busiest thread by replies, and its swaps
    busiest = max(athreads.items(), key=lambda kv: kv[1]["replies"], default=(None, {}))
    top = sorted(ops.items(), key=lambda kv: -kv[1][2])[:8]
    atop = sorted(aops.items(), key=lambda kv: -kv[1][2])[:8]
    bytes_top = sorted(ops.items(), key=lambda kv: -kv[1][1])[:8]
    # startup: from the launch to the end of the second holding the first frame, and to the window
    first = overlapping(lines, t_launch, t_first + 1)
    st_all, *_ = agg(first)
    st_app, *_ = agg(first, new) if new else (st_all,)
    settle = overlapping(lines, t_launch, t0)
    se_all, *_ = agg(settle)
    se_app, *_ = agg(settle, new) if new else (se_all,)
    gfx = open(os.path.join(RUN, f"{w}.gfxinfo.txt")).read() if os.path.exists(os.path.join(RUN, f"{w}.gfxinfo.txt")) else ""
    g = re.search(r"Total frames rendered: (\d+)", gfx)
    pipe = re.search(r"Pipeline=(.*)", gfx)
    cap = {}
    if os.path.exists(os.path.join(RUN, f"{w}.capture.json")):
        try:
            cap = json.load(open(os.path.join(RUN, f"{w}.capture.json")))
        except ValueError:
            pass
    extra = {}
    for k in ("x264", "compress"):
        p = os.path.join(RUN, f"{w}.{k}.json")
        if os.path.exists(p):
            extra[k] = json.load(open(p))
    hw = open(os.path.join(RUN, f"{w}.hwui.txt")).read().split("\n") if os.path.exists(os.path.join(RUN, f"{w}.hwui.txt")) else []
    s = {
        "workload": w,
        "seconds": secs,
        "hwui_prop": hw[0] if hw else "",
        "pipeline": pipe.group(1).strip() if pipe else None,
        "launch_total_time_ms": m["launch_done"].get("total_time_ms"),
        "app_puids": sorted(new),
        "all": {
            "read_Bps": a["read"] / secs,
            "gles_Bps": (a["gles1"] + a["gles2"]) / secs,
            "rc_Bps": a["rc"] / secs,
            "vk_Bps": a["vk"] / secs,
            "texture_upload_Bps": cat["texture_upload"] / secs,
            "buffer_upload_Bps": cat["buffer_upload"] / secs,
            "client_arrays_Bps": cat["client_arrays"] / secs,
            "colorbuffer_upload_Bps": cat["colorbuffer_upload"] / secs,
            "colorbuffer_dma_calls_ps": cat["colorbuffer_dma_calls"] / secs,
            "replies_ps": a["replies"] / secs,
            "reply_Bps": a["reply_bytes"] / secs,
            "fence_signals_ps": a["fence_signals"] / secs,
            "swaps_ps": cat["swaps"] / secs,
            "composes_ps": cat["composes"] / secs,
            "top_replies": [[name(op), v[2] / secs, v[3] / secs] for op, v in top],
            "top_bytes": [[name(op), v[1] / secs, v[0] / secs] for op, v in bytes_top],
        },
        "app": {
            "read_Bps": aa["read"] / secs,
            "replies_ps": aa["replies"] / secs,
            "reply_Bps": aa["reply_bytes"] / secs,
            "frames_ps": app_frames / secs,
            "replies_per_frame": aa["replies"] / app_frames if app_frames else None,
            "busiest_thread": {"replies_ps": busiest[1].get("replies", 0) / secs,
                               "swaps_ps": busiest[1].get("swaps", 0) / secs,
                               "read_Bps": busiest[1].get("read", 0) / secs},
            "top_replies": [[name(op), v[2] / secs] for op, v in atop],
        },
        "threads": [{"puid": k[0], "id": k[1], "app": k[0] in new,
                     "replies_ps": v.get("replies", 0) / secs, "swaps_ps": v.get("swaps", 0) / secs,
                     "composes_ps": v.get("composes", 0) / secs, "read_Bps": v.get("read", 0) / secs,
                     "replies_per_swap": (v.get("replies", 0) / v["swaps"]) if v.get("swaps") else None}
                    for k, v in sorted(threads.items(), key=lambda kv: -kv[1]["replies"])[:8]],
        "gfxinfo_frames": int(g.group(1)) if g else None,
        "startup": {
            "to_first_frame_all_bytes": st_all["read"],
            "to_first_frame_app_bytes": st_app["read"],
            "to_window_all_bytes": se_all["read"],
            "to_window_app_bytes": se_app["read"],
            "to_window_s": (t0 - t_launch) / 1000,
        },
        "capture": cap,
        **extra,
    }
    return s


def main():
    lines = load_lines()
    ws = ONLY or sorted(f[: -len(".markers.jsonl")] for f in os.listdir(RUN) if f.endswith(".markers.jsonl"))
    out = {}
    for w in ws:
        try:
            out[w] = summarize(lines, w)
        except (KeyError, FileNotFoundError, ZeroDivisionError) as e:
            out[w] = {"error": repr(e)}
    json.dump(out, open(os.path.join(RUN, "summary.json"), "w"), indent=1)
    for w, s in out.items():
        if "error" in s:
            print(w, s["error"])
            continue
        al, ap = s["all"], s["app"]
        print(f"{w:16} {s['pipeline'] or s['hwui_prop']:<16} in {al['read_Bps']/1e6:7.2f} MB/s (gl {al['gles_Bps']/1e6:.2f} rc {al['rc_Bps']/1e6:.2f} vk {al['vk_Bps']/1e6:.2f})"
              f" tex {al['texture_upload_Bps']/1e6:.2f} buf {al['buffer_upload_Bps']/1e6:.2f} replies {al['replies_ps']:.0f}/s out {al['reply_Bps']/1e3:.1f} kB/s"
              f" | app fps {ap['frames_ps']:.1f} rt/frame {ap['replies_per_frame'] or 0:.1f}")


main()
