#!/usr/bin/env python3
"""Did each corpus page render? One untimed pass over a CDP engine, kept on disk.

    render_check.py ADDRESS --urls URL,URL,... --out DIR --engine LABEL

For every URL it drives cdpdrive twice, as the measured arms do: a screenshot
(PNG here, so the pixels can be read with the standard library) and the HTML
operation (document.documentElement.outerHTML). It keeps the image and the
document under DIR and writes one JSON line per page to DIR/correctness.jsonl:

    engine, url, site, shot_ok, shot_error, width, height, dominant_fraction,
    distinct_colors, blank, html_ok, html_error, html_bytes, completed

`blank` is true when one colour covers at least BLANK_FRACTION of the pixels: a
page whose screenshot comes back that way rendered nothing a person would call
the page, however fast it was. `completed` is shot_ok and html_ok and not blank.
The measured runs count a request on a page that did not complete here as a
failure (kitesurf.sh), so an engine that returns a white rectangle quickly is
not credited with a fast render.

This pass is not timed and is not part of any measured arm: it runs before the
measured reps, against the same engine, over the same replay.
"""

import argparse
import importlib.util
import json
import os
import struct
import sys
import zlib
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
BLANK_FRACTION = 0.998


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def png_pixels(data: bytes):
    """(width, height, channels, rows) of an 8-bit RGB or RGBA PNG, unfiltered."""
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("not a PNG")
    pos, idat, ihdr = 8, [], None
    while pos + 8 <= len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
        pos += 12 + length
    if ihdr is None:
        raise ValueError("PNG has no IHDR")
    width, height, depth, color, _comp, _filt, interlace = ihdr
    channels = {2: 3, 6: 4}.get(color)
    if depth != 8 or channels is None or interlace:
        raise ValueError(f"unsupported PNG (depth {depth}, colour type {color}, interlace {interlace})")
    raw = zlib.decompress(b"".join(idat))
    stride = width * channels
    rows, prev = [], bytearray(stride)
    for y in range(height):
        start = y * (stride + 1)
        ftype = raw[start]
        line = bytearray(raw[start + 1:start + 1 + stride])
        if ftype == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        elif ftype != 0:
            raise ValueError(f"bad PNG filter {ftype} on row {y}")
        rows.append(bytes(line))
        prev = line
    return width, height, channels, rows


def colour_stats(data: bytes) -> dict:
    width, height, channels, rows = png_pixels(data)
    counts = Counter()
    for row in rows:
        counts.update(row[i:i + 3] for i in range(0, len(row), channels))
    total = width * height
    top = counts.most_common(1)[0][1] if counts else 0
    fraction = top / total if total else 1.0
    return {"width": width, "height": height, "distinct_colors": len(counts),
            "dominant_fraction": round(fraction, 6), "blank": fraction >= BLANK_FRACTION}


def drive(cdpdrive, address, url, op, out_prefix, timeout):
    args = argparse.Namespace(
        cdp_host=address, url=url, format="png", quality=80, timeout=timeout,
        idle_wait_ms=0.0, out_prefix=out_prefix, ws_url="", connect_retries=200,
        nav_timing=False, print_target=False, host_header="", net_trace=None,
        net_trace_drain_ms=5000.0, render_module=os.path.join(HERE, "render.py"), op=op,
    )
    try:
        return cdpdrive.drive(args)
    except Exception as error:  # drive() should not raise; a check must not die on it
        return {"ok": False, "error": f"{type(error).__name__}: {error}"}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("address", help="CDP endpoint, host:port")
    ap.add_argument("--urls", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--engine", required=True)
    ap.add_argument("--timeout", type=float, default=30.0)
    args = ap.parse_args()
    cdpdrive = load("cdpdrive", os.path.join(HERE, "cdpdrive.py"))
    report = load("report", os.path.join(HERE, "report.py"))
    site_of = {url: key for key, url in report.KITESURF_CORPUS.items()}
    os.makedirs(args.out, exist_ok=True)
    rows = []
    for url in [u for u in args.urls.split(",") if u]:
        site = site_of.get(url, url.replace("://", "_").replace("/", "_"))
        prefix = os.path.join(args.out, site)
        row = {"engine": args.engine, "url": url, "site": site}
        shot = drive(cdpdrive, args.address, url, "screenshot", prefix, args.timeout)
        row["shot_ok"] = shot.get("ok") is True
        row["shot_error"] = shot.get("error")
        row["shot_stage"] = shot.get("stage")
        if row["shot_ok"]:
            try:
                with open(prefix + ".png", "rb") as handle:
                    row.update(colour_stats(handle.read()))
            except (OSError, ValueError, zlib.error) as error:
                row["shot_ok"] = False
                row["shot_error"] = f"screenshot unreadable: {type(error).__name__}: {error}"
        html = drive(cdpdrive, args.address, url, "html", prefix, args.timeout)
        row["html_ok"] = html.get("ok") is True
        row["html_error"] = html.get("error")
        row["html_bytes"] = html.get("html_bytes")
        row["completed"] = bool(row["shot_ok"] and row["html_ok"] and row.get("blank") is False)
        rows.append(row)
        print(json.dumps(row, separators=(",", ":")), flush=True)
    path = os.path.join(args.out, "correctness.jsonl")
    with open(path + ".tmp", "w") as handle:
        for row in rows:
            handle.write(json.dumps(row, separators=(",", ":")) + "\n")
    os.replace(path + ".tmp", path)
    done = sum(r["completed"] for r in rows)
    print(f"{args.engine}: {done}/{len(rows)} pages completed", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
