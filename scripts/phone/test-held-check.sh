#!/bin/bash
# held-check.py on drawn stand-ins of the three screenshots: the real held layout passes, and
# each way it can be wrong (no held bit, bars not lifted, a stamp showing unheld) fails.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
D=$(mktemp -d); trap 'rm -rf "$D"' EXIT
python3 - "$D" <<'PY'
import struct, sys, zlib
d = sys.argv[1]
W, H, BAR = 720, 1280, 96
def png(path, rows):
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xffffffff)
    raw = b"".join(b"\x00" + bytes(v for p in r for v in p) for r in rows)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))
def shot(name, lifted, stamp):
    rows = [[(90, 160, 90)] * W for _ in range(H)]  # the scene
    if stamp is not None:
        band = [(255,) * 3 if (stamp >> (31 - x * 32 // W)) & 1 else (0,) * 3 for x in range(W)]
        for y in range(H - 8, H):
            rows[y] = list(band)
    off = 8 if lifted else 0
    for y in range(H - off - BAR, H - off):  # the bar, with a label's stroke in it
        rows[y] = [(103, 80, 164) if 100 <= x < 160 and (y - (H - off - BAR)) % 7 == 0 else (254, 247, 255) for x in range(W)]
    png(f"{d}/{name}.png", rows)
shot("chrome", False, 76139400)          # the stamp under the bar
shot("held", True, 0x80000000 | 76139400)
shot("back", False, 76139400)
shot("held-nobit", True, 76139400)
shot("held-notlifted", False, 0x80000000 | 76139400)
shot("back-lifted", True, 76139400)
PY
fail=0
run() { local want=$1; shift; python3 "$HERE/held-check.py" "$@" > "$D/out" 2>&1; local rc=$?
  if { [ "$want" = pass ] && [ $rc = 0 ]; } || { [ "$want" = fail ] && [ $rc != 0 ]; }; then echo "ok   $want: $*"; else echo "FAIL want $want: $*"; cat "$D/out"; fail=1; fi; }
run pass "$D/chrome.png" "$D/held.png" "$D/back.png"
run fail "$D/chrome.png" "$D/held-nobit.png" "$D/back.png"
run fail "$D/chrome.png" "$D/held-notlifted.png" "$D/back.png"
run fail "$D/chrome.png" "$D/held.png" "$D/back-lifted.png"
exit $fail
