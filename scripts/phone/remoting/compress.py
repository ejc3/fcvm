#!/usr/bin/env python3
"""compress.py <run dir> <workload>: the dumped guest->host stream, split by render thread,
compressed with zstd -3 and lz4 (the CLIs, default level 1 for lz4), each thread's stream on
its own as a link per channel would; writes <w>.compress.json."""
import json, os, struct, subprocess, sys, tempfile
run, w = sys.argv[1], sys.argv[2]
p = os.path.join(run, f"{w}.dump")
data = open(p, "rb").read()
streams, off, records = {}, 0, 0
while off + 12 <= len(data):
    tid, dec, n = struct.unpack_from("<III", data, off)
    off += 12
    streams.setdefault(tid, bytearray()).extend(data[off:off + n])
    off += n
    records += 1
def csize(cmd, b):
    return len(subprocess.run(cmd, input=bytes(b), capture_output=True, check=True).stdout)
raw = sum(len(b) for b in streams.values())
z = sum(csize(["zstd", "-3", "-q", "-c"], b) for b in streams.values())
l4 = sum(csize(["lz4", "-1", "-q", "-c"], b) for b in streams.values())
out = {"dump_bytes": len(data), "records": records, "threads": len(streams), "raw_bytes": raw,
       "zstd3_bytes": z, "lz4_bytes": l4, "zstd3_ratio": raw / z if z else None,
       "lz4_ratio": raw / l4 if l4 else None}
json.dump(out, open(os.path.join(run, f"{w}.compress.json"), "w"))
print(w, out)
