#!/usr/bin/env python3
"""Seam check for Robot Park captures (scripts/phone/run.sh).

    seam.py MANIFEST.json OUTDIR

MANIFEST lists frames the app drew offscreen with the control protocol's `capture`
(fleet/PROTOCOL.md, "Phone app"): for each `t`, the whole image and the tiles of a grid,
each with the reply's `path` (a local copy of the raw RGBA file), `w`, `h`, `x0`, `y0`:

    {"frames": [{"t": 12.0, "whole": {...}, "tiles": [{...}, {...}]}],
     "repeats": [[{...}, {...}]]}

For every tile pixel, the whole image's pixel at the same whole-image position must be the
same bytes; that makes the tiles stitch into the whole image with no seam. One allowance,
measured on the T4 through the emulator: in some runs a pixel where a light's glow is blended
comes out 1 level apart (the same pixel at t=41.5 in 3 of 9 runs, while 96 repeat captures of
the same frames were identical), so a tile may have up to ONE_LEVEL_MAX pixels 1 level apart.
Anything more, or any pixel more than 1 level apart, fails. Each `repeats` pair is one rect
drawn twice, which must match byte for byte. Prints a JSON summary with the exact counts,
writes the whole image and the stitched tiles as PNG into OUTDIR, and exits 1 on a failure.
"""
import json
import os
import struct
import sys
import zlib

ONE_LEVEL_MAX = 9  # 0.001% of a 720 x 1280 tile

def load(c):
    data = open(c["path"], "rb").read()
    if len(data) != c["w"] * c["h"] * 4:
        sys.exit(f"{c['path']}: {len(data)} bytes, expected {c['w']}x{c['h']}x4")
    return data


def png(path, w, h, rgba):
    rows = b"".join(b"\x00" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xffffffff)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b""))


def main():
    manifest = json.load(open(sys.argv[1]))
    out = sys.argv[2]
    os.makedirs(out, exist_ok=True)
    report = {"frames": [], "repeats": [], "ok": True}
    for fr in manifest["frames"]:
        whole = fr["whole"]
        W, H = whole["w"], whole["h"]
        wp = load(whole)
        stitched = bytearray(W * H * 4)
        r = {"t": fr["t"], "tiles": []}
        for tile in fr["tiles"]:
            tp = load(tile)
            w, h, x0, y0 = tile["w"], tile["h"], tile["x0"], tile["y0"]
            diff = edge = maxd = n = 0
            cols = max(0, min(w, W - x0))
            for j in range(h):
                Y = y0 + j
                if Y >= H or cols == 0:
                    continue
                a0, b0 = 4 * (Y * W + x0), 4 * (j * w)
                seg_w, seg_t = wp[a0:a0 + 4 * cols], tp[b0:b0 + 4 * cols]
                stitched[a0:a0 + 4 * cols] = seg_t
                n += cols
                if seg_w == seg_t:
                    continue
                for i in range(cols):
                    p, q = seg_w[4 * i:4 * i + 4], seg_t[4 * i:4 * i + 4]
                    if p != q:
                        diff += 1
                        if i < 2 or i >= w - 2:
                            edge += 1
                        maxd = max(maxd, max(abs(p[k] - q[k]) for k in range(3)))
            r["tiles"].append({"x0": x0, "y0": y0, "w": w, "h": h, "pixels": n, "differ": diff,
                               "differ_at_edges": edge, "max_diff": maxd})
            report["ok"] &= maxd <= 1 and diff <= ONE_LEVEL_MAX
            report["exact"] = report.get("exact", True) and diff == 0
        name = f"t{fr['t']:g}".replace(".", "_")
        png(os.path.join(out, f"{name}-whole.png"), W, H, wp)
        png(os.path.join(out, f"{name}-stitched.png"), W, H, bytes(stitched))
        report["frames"].append(r)
    for a, b in manifest.get("repeats", []):
        same = load(a) == load(b)
        report["repeats"].append({"x0": a["x0"], "t": a.get("t"), "identical": same})
        report["ok"] &= same
    print(json.dumps(report))
    sys.exit(0 if report["ok"] else 1)


main()
