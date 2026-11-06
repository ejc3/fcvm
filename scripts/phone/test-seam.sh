#!/bin/bash
# scripts/phone/seam.py on made-up captures: a tile equal to its part of the whole image passes,
# and one pixel 1 level apart fails.
#   scripts/phone/test-seam.sh
set -uo pipefail
seam=$(cd "$(dirname "$0")" && pwd)/seam.py
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
fail=0
python3 -I - "$dir" <<'PY'
import json, os, sys
d = sys.argv[1]
W, H, w = 8, 4, 4
whole = bytes((x * 7 + y * 13 + c) % 256 for y in range(H) for x in range(W) for c in range(4))
def tile(x0, bump):
    t = bytearray(whole[4 * (y * W + x0 + x) + c] for y in range(H) for x in range(w) for c in range(4))
    if bump: t[0] = (t[0] + 1) % 256
    return bytes(t)
for name, data in (("whole", whole), ("left", tile(0, False)), ("right", tile(4, False)), ("right1", tile(4, True))):
    open(os.path.join(d, name + ".rgba"), "wb").write(data)
def c(n, ww, x0): return {"path": os.path.join(d, n + ".rgba"), "w": ww, "h": H, "x0": x0, "y0": 0}
for m, right in (("same", "right"), ("apart", "right1")):
    json.dump({"frames": [{"t": 1, "whole": c("whole", W, 0), "tiles": [c("left", w, 0), c(right, w, 4)]}], "repeats": []},
              open(os.path.join(d, m + ".json"), "w"))
PY
if python3 -I "$seam" "$dir/same.json" "$dir/out" > /dev/null; then echo "ok   tiles equal to the whole image pass"; else echo "FAIL equal tiles failed"; fail=1; fi
if python3 -I "$seam" "$dir/apart.json" "$dir/out" > /dev/null; then echo "FAIL a tile with one pixel 1 level apart passed"; fail=1; else echo "ok   one pixel 1 level apart fails"; fi
exit $fail
