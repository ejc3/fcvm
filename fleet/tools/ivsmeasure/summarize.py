# Summarize serve.py's results: per cycle startup and latency, and the pooled per-frame latency.
import json, sys, statistics as st
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
cyc = [r for r in rows if "cycle" in r]
def pct(xs, q):
    xs = sorted(xs); return xs[min(len(xs) - 1, round((len(xs) - 1) * q))] if xs else None
pool, live = [], []
for r in cyc:
    lat = r["latency"][len(r["latency"]) // 6:]  # drop the first 10 s of each cycle (catching up to live)
    pool += lat; live += r["live_latency"]
    print(f"cycle {r['cycle']}: startup {r['startup']}, sei {r['sei']}, rendition {r['rendition']}, "
          f"latency p50 {pct(lat, .5)} p95 {pct(lat, .95)} min {min(lat) if lat else None} max {max(lat) if lat else None}, "
          f"ivs live_latency p50 {pct(r['live_latency'], .5)}, errors {r['errors'][:3]}")
for k in ("frame_ms", "playing_ms", "sei_ms"):
    xs = [r["startup"].get(k) for r in cyc if r["startup"].get(k) is not None]
    if xs: print(f"startup {k}: median {round(st.median(xs))} min {round(min(xs))} max {round(max(xs))} (n={len(xs)})")
if pool: print(f"glass-to-glass (n={len(pool)}): p50 {pct(pool,.5)} p95 {pct(pool,.95)} p99 {pct(pool,.99)} ms")
if live: print(f"IVS getLiveLatency (n={len(live)}): p50 {pct(live,.5)} p95 {pct(live,.95)} ms")
fl_steady, fl_first = [], []
for r in cyc:
    for at_ms, lat in r.get("frame_latency", []):
        (fl_first if at_ms < 10000 else fl_steady).append(lat)
if fl_steady:
    print(f"frame-exact glass-to-glass, after 10 s (n={len(fl_steady)}): p50 {pct(fl_steady,.5)} p95 {pct(fl_steady,.95)} p99 {pct(fl_steady,.99)} min {min(fl_steady)} max {max(fl_steady)} ms")
if fl_first:
    print(f"frame-exact glass-to-glass, first 10 s (n={len(fl_first)}): p50 {pct(fl_first,.5)} p95 {pct(fl_first,.95)} ms")
