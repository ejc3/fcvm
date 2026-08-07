#!/usr/bin/env python3
"""Regenerate the deterministic PNG fixtures in pages/ (stdlib only).

Gradient + xorshift noise so the files are a realistic size (do not compress
to nothing) yet byte-identical on every run. Run from anywhere:

    python3 bench/chromium/gen_images.py
"""

import os
import struct
import zlib

PAGES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pages")


def xorshift(state: int) -> int:
    state ^= (state << 13) & 0xFFFFFFFF
    state ^= state >> 17
    state ^= (state << 5) & 0xFFFFFFFF
    return state & 0xFFFFFFFF


def png_chunk(tag: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def write_png(path: str, width: int, height: int, seed: int) -> None:
    rows = []
    for y in range(height):
        row = bytearray(b"\x00")  # filter type 0 (None)
        for x in range(width):
            # Noise per 8x8 BLOCK, not per pixel: per-pixel noise interleaved
            # with the color channels leaves zlib no >=3-byte matches and the
            # file balloons to ~190KB; block noise keeps a visible texture and
            # a real decode while rows repeat and compress to fixture size.
            block_hash = xorshift(seed ^ (x >> 3) * 73856093 ^ (y >> 3) * 19349663)
            r = ((x >> 3) * 255 // (width >> 3) + (block_hash & 0x1F)) & 0xFF
            g = (y * 255 // height) & 0xFF
            b = (x * x + y * y) * 255 // (width * width + height * height)
            row += bytes((r, g, b))
        rows.append(bytes(row))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)  # 8-bit RGB
    idat = zlib.compress(b"".join(rows), 6)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", ihdr)
        + png_chunk(b"IDAT", idat)
        + png_chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(png)
    print(f"{path}: {width}x{height} {len(png)} bytes")


def main() -> None:
    write_png(os.path.join(PAGES, "img1.png"), 256, 256, seed=0xDEADBEEF)
    write_png(os.path.join(PAGES, "img2.png"), 256, 256, seed=0xC0FFEE42)
    write_png(os.path.join(PAGES, "img3.png"), 256, 256, seed=0x8BADF00D)
    write_png(os.path.join(PAGES, "img4.png"), 384, 256, seed=0x1BADB002)


if __name__ == "__main__":
    main()
