#!/usr/bin/env python3
"""Robot Park's held layout, from three screenshots (scripts/phone/scene-test.sh, check 6):
CHROME (bars shown, not held), HELD (the same, held) and GIVEN_BACK (not held again, after the
held bit's tail). Held, the bottom band is Robot Park's stamp with the held bit (stamp.h,
STAMP_HELD_BIT) and the bottom bar is the unheld one lifted by the band's 8 rows; not held, the
bar covers the band and no stamp reads. Prints one line per check, "ok" or "FAIL"; exits 1 on a
failure.

  held-check.py CHROME HELD GIVEN_BACK"""
import struct
import sys
import zlib

ROWS, CELLS, HELD_BIT = 8, 32, 0x80000000
BAR_ROWS = 40  # rows of the bottom bar compared, above the band


class Png:
    """An 8-bit RGB or RGBA PNG, not interlaced (what screencap writes), as rows of RGB tuples."""

    def __init__(self, path):
        data = open(path, "rb").read()
        assert data[:8] == b"\x89PNG\r\n\x1a\n", f"{path}: not a PNG"
        pos, idat = 8, b""
        while pos < len(data):
            n, kind = struct.unpack(">I4s", data[pos:pos + 8])
            body = data[pos + 8:pos + 8 + n]
            if kind == b"IHDR":
                self.w, self.h, depth, color, _, _, interlace = struct.unpack(">IIBBBBB", body)
                assert depth == 8 and color in (2, 6) and interlace == 0, f"{path}: unsupported PNG"
                bpp = 4 if color == 6 else 3
            elif kind == b"IDAT":
                idat += body
            pos += 12 + n
        raw, stride, prev = zlib.decompress(idat), self.w * bpp, bytearray(self.w * bpp)
        self.rows = []
        for y in range(self.h):
            f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b, c = prev[i], (prev[i - bpp] if i >= bpp else 0)
                if f == 1:
                    line[i] = (line[i] + a) & 255
                elif f == 2:
                    line[i] = (line[i] + b) & 255
                elif f == 3:
                    line[i] = (line[i] + (a + b) // 2) & 255
                elif f == 4:
                    pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                    line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
            self.rows.append([tuple(line[x * bpp:x * bpp + 3]) for x in range(self.w)])
            prev = line


def stamp(img):
    """The 32 bits of the bottom band's middle row, or None when a cell is not clean black or
    white, or every cell is the same."""
    w, h = img.w, img.h
    row = img.rows[h - ROWS // 2]
    v = 0
    for i in range(CELLS):
        c = row[((i * w // CELLS) + ((i + 1) * w // CELLS)) // 2]
        if min(c) >= 160:
            v = v * 2 + 1
        elif max(c) <= 96:
            v = v * 2
        else:
            return None
    # all black or all white is a plain bar or a dark screen, not a frame number
    return None if v in (0, 0xFFFFFFFF) else v


def rows(img, top, n):
    return [p for r in img.rows[top:top + n] for p in r]


def main(chrome_path, held_path, back_path):
    chrome, held, back = (Png(p) for p in (chrome_path, held_path, back_path))
    h = held.h
    fail = False

    def check(cond, text):
        nonlocal fail
        print(("ok   " if cond else "FAIL ") + text)
        fail |= not cond

    s = stamp(held)
    check(s is not None and bool(s & HELD_BIT) and (s & ~HELD_BIT) > 0,
          f"held: the band below the bars is a stamp with the held bit ({'none' if s is None else hex(s)})")
    for name, img in (("not held", chrome), ("given back", back)):
        s = stamp(img)
        check(s is None, f"{name}: the bar covers the band, no stamp reads ({'none' if s is None else hex(s)})")
    # the bar's rows, unheld at the screen's bottom and held 8 rows up: the same pixels but for
    # the 5% of the scene the bar lets through
    a = rows(chrome, h - ROWS - BAR_ROWS, BAR_ROWS)
    b = rows(held, h - 2 * ROWS - BAR_ROWS, BAR_ROWS)
    diffs = [abs(x - y) for p, q in zip(a, b) for x, y in zip(p, q)]
    mean = sum(diffs) / len(diffs)
    check(mean <= 4 and max(diffs) <= 40,
          f"held, the bottom bar looks as it does unheld, 8 rows up (mean channel difference {mean:.2f}, max {max(diffs)})")
    return 1 if fail else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:4]))
