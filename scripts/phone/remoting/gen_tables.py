#!/usr/bin/env python3
"""gen_tables.py <summary A> <label A> [<summary B> <label B>]: the results page's tables (markdown)."""
import json, sys

runs = [(json.load(open(sys.argv[i])), sys.argv[i + 1]) for i in range(1, len(sys.argv), 2)]
ORDER = ["robotpark", "launcher", "settings", "shade", "firefox_article", "firefox_images", "wikipedia", "maps", "vlc", "game"]
TITLE = {"robotpark": "Robot Park", "launcher": "Launcher3: home pages and app drawer", "settings": "Settings list",
         "shade": "Notification shade and quick settings", "firefox_article": "Firefox (Fennec F-Droid): long article",
         "firefox_images": "Firefox (Fennec F-Droid): image-heavy page", "wikipedia": "Wikipedia app: article",
         "maps": "Organic Maps: pan and zoom, offline region", "vlc": "VLC: local 720p H.264", "game": "SuperTuxKart: race"}
CBR_MBPS = 20.0
RTTS = [20, 50, 100]


def mbit(Bps):
    return Bps * 8 / 1e6


def f(x, d=1):
    if x is None:
        return "-"
    if abs(x) >= 100:
        return f"{x:,.0f}"
    if 0 < abs(x) < 1:
        return f"{x:.{max(d, 2)}f}"
    return f"{x:,.{d}f}"


def cap(fps, r, rtt):
    """the frame rate a thread can reach when each of its r round trips per frame costs rtt ms"""
    if not r:
        return fps
    return min(fps, 1000.0 / (r * rtt))


def rows(s):
    a, m, c, st = s["all"], s["main"], s["composer"], s["startup"]
    x, z = s.get("x264", {}), s.get("compress", {})
    raw = mbit(a["read_Bps"])
    zr, lr = z.get("zstd3_ratio"), z.get("lz4_ratio")
    out = [
        ("HWUI renderer", s["pipeline"] or s["hwui_prop"]),
        ("guest->host, all threads (Mbit/s)", f(raw)),
        ("  GLES / renderControl / Vulkan (Mbit/s)", f"{f(mbit(a['gles_Bps']))} / {f(mbit(a['rc_Bps']), 2)} / {f(mbit(a['vk_Bps']))}"),
        ("  texture uploads (Mbit/s)", f(mbit(a["texture_upload_Bps"]), 2)),
        ("  buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s)", f(mbit(a["buffer_upload_Bps"]), 2)),
        ("  client-side vertex arrays (Mbit/s)", f(mbit(a["client_arrays_Bps"]), 2)),
        ("host->guest replies (kbit/s)", f(a["reply_Bps"] * 8 / 1e3)),
        ("blocking round trips, all threads (/s)", f(a["replies_ps"], 0)),
        ("top round trips (/s)", ", ".join(f"`{n}` {f(r, 0)}" for n, r, _ in a["top_replies"][:4] if r >= 1)),
        ("main rendering thread: frames/s", f(m["frames_ps"])),
        ("main thread round trips per frame", f(m["replies_per_frame"])),
        ("all app threads' round trips per main frame", f(m["app_replies_per_frame"])),
        ("composer: composed frames/s, round trips per frame", f"{f(c['frames_ps'])}, {f(c['replies_per_frame'])}"),
        ("fence signals to the guest (/s)", f(a["fence_signals_ps"], 0)),
        ("startup: `am start` TotalTime (ms)", f(s["launch_total_time_ms"] or None, 0)),
        ("startup: bytes to first frame (MB)", f(st["to_first_frame_all_bytes"] / 1e6, 2)),
        (f"startup: bytes in the {st['to_window_s']:.0f} s before the window (MB)", f(st["to_window_all_bytes"] / 1e6)),
        ("  of which shaders / textures / buffers (MB)", " / ".join(f(st["to_window_categories"][k] / 1e6, 2) for k in ("shader", "texture_upload", "buffer_upload"))),
        ("screen updates/s (captured)", f(s["capture"].get("screen_updates", 0) / s["capture"]["seconds"]) if s.get("capture", {}).get("seconds") else "-"),
        ("video, configured NVENC CBR (Mbit/s)", f(CBR_MBPS)),
        ("video, x264 CRF 23 of the captured screen (Mbit/s)", f(x.get("kbps", 0) / 1e3, 2) if x else "-"),
        ("command stream after zstd -3 / lz4 (Mbit/s)", f"{f(raw / zr) if zr else '-'} / {f(raw / lr) if lr else '-'}"),
        ("  ratio zstd -3 / lz4, on a sample of (MB)", f"{f(zr)} / {f(lr)}, {f(z.get('raw_bytes', 0) / 1e6)}" if z else "-"),
    ]
    for rtt in RTTS:
        out.append((f"frames/s if each round trip costs {rtt} ms: main thread, composer",
                    f"{f(cap(m['frames_ps'], m['replies_per_frame'], rtt))}, {f(cap(c['frames_ps'], c['replies_per_frame'], rtt))}"))
    return out


SHORT = {"robotpark": "Robot Park", "launcher": "Launcher3", "settings": "Settings", "shade": "Notification shade",
         "firefox_article": "Firefox, article", "firefox_images": "Firefox, images", "wikipedia": "Wikipedia app",
         "maps": "Organic Maps", "vlc": "VLC 720p", "game": "SuperTuxKart"}


def summary(d):
    print("| workload | stream (Mbit/s) | after zstd -3 | video at CRF 23 | round trips/s | main frames/s | round trips per main frame | frames/s at 20 / 50 / 100 ms RTT |")
    print("|---|---|---|---|---|---|---|---|")
    for w in ORDER:
        s = d.get(w)
        if not s or "error" in s:
            continue
        a, m, c = s["all"], s["main"], s["composer"]
        raw = mbit(a["read_Bps"])
        zr = s.get("compress", {}).get("zstd3_ratio")
        x = s.get("x264", {}).get("kbps")
        fr = [min(cap(m["frames_ps"], m["replies_per_frame"], r), cap(c["frames_ps"], c["replies_per_frame"], r)) for r in RTTS]
        print(f"| {SHORT[w]} | {f(raw)} | {f(raw / zr) if zr else '-'} | {f(x / 1e3, 2) if x else '-'} | {f(a['replies_ps'], 0)} | "
              f"{f(m['frames_ps'])} | {f(m['replies_per_frame'])} | {' / '.join(f(v) for v in fr)} |")
    print()


summary(runs[0][0])
print("<!-- per workload -->\n")
for w in ORDER:
    have = [(d[w], lbl) for d, lbl in runs if w in d and "error" not in d[w]]
    if not have:
        continue
    print(f"### {TITLE[w]}\n")
    print("| | " + " | ".join(lbl for _, lbl in have) + " |")
    print("|---|" + "---|" * len(have))
    rs = [rows(s) for s, _ in have]
    for i, (k, _) in enumerate(rs[0]):
        print(f"| {k} | " + " | ".join(r[i][1] for r in rs) + " |")
    print()
