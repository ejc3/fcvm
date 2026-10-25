"""Seam ratio of a stitched mosaic.

    python3 seam_metric.py IMAGE COLS ROWS

For every tile boundary: the mean |difference| (gray levels) between the two
pixel columns (rows) that meet there, divided by the same measure for the
neighbouring column (row) pairs 3 to 12 pixels away on either side. 1.0 means
the pair across the boundary differs like any other pair of neighbours; a seam
shows as a ratio well above 1. Needs the ffmpeg CLI to read the image.
"""
import subprocess
import sys

path, cols, rows = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height",
                        "-of", "csv=p=0", path], capture_output=True, text=True, check=True).stdout.strip().split(",")
w, h = int(probe[0]), int(probe[1])
raw = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", path, "-f", "rawvideo", "-pix_fmt", "gray", "-"],
                     capture_output=True, check=True).stdout


def px(x, y):
    return raw[y * w + x]


def coldiff(x, y0, y1):
    return sum(abs(px(x - 1, y) - px(x, y)) for y in range(y0, y1)) / (y1 - y0)


def rowdiff(y, x0, x1):
    return sum(abs(px(x, y - 1) - px(x, y)) for x in range(x0, x1)) / (x1 - x0)


offsets = list(range(-12, -2)) + list(range(3, 13))
ratios = []
xs = [round(k * w / cols) for k in range(cols + 1)]
ys = [round(k * h / rows) for k in range(rows + 1)]
for r in range(rows):
    for b in xs[1:-1]:
        seam = coldiff(b, ys[r], ys[r + 1])
        ref = sum(coldiff(b + o, ys[r], ys[r + 1]) for o in offsets) / len(offsets)
        ratios.append(seam / ref)
        print(f"x={b} rows {ys[r]}-{ys[r + 1]}: {seam / ref:.2f}")
for c in range(cols):
    for b in ys[1:-1]:
        seam = rowdiff(b, xs[c], xs[c + 1])
        ref = sum(rowdiff(b + o, xs[c], xs[c + 1]) for o in offsets) / len(offsets)
        ratios.append(seam / ref)
        print(f"y={b} cols {xs[c]}-{xs[c + 1]}: {seam / ref:.2f}")
print(f"range {min(ratios):.2f} to {max(ratios):.2f}, mean {sum(ratios) / len(ratios):.2f}")
