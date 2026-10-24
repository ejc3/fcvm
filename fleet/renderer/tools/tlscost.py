"""TLS cost per stream: the same N streams over plain WebSocket and over WSS.

    python3 tools/tlscost.py HOST PORT TLS_PORT N H Q SECONDS

Posts a scene whose camera never holds (every frame is drawn and encoded),
then for each arm opens N streams at height H and quality Q, lets them settle
for 2 s, and over SECONDS counts frames and bytes per stream and reads the
renderer's CPU seconds (/health cpu_s) at both ends. The delay of a frame is
its arrival time minus its slot time (epoch + t), on the client's clock; the
two arms share that clock, so compare them, not the absolute values. Prints
one CSV row per arm.
"""
import json
import os
import socket
import statistics
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fleetclient as fc  # noqa: E402

host, port, tport, n, h, q, secs = (sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]),
                                    int(sys.argv[5]), sys.argv[6], float(sys.argv[7]))
epoch = int(time.time() * 1000)
cam = [{"t": 0.0, "pos": [-22, 0.7, -30], "look": [-45, 1.2, -85], "fov_deg": 60.0},
       {"t": 600.0, "pos": [-80, 2.4, -170], "look": [-20, 0.8, -40], "fov_deg": 64.0}]
fc.post_scene(host, port, tls=False, scene={"epoch_unix_ms": epoch, "rect": {"x": 0, "y": 0, "w": 0.25, "h": 0.5},
                           "image_aspect": 16 / 9, "camera": cam, "time_of_day": 0.245, "words": [], "lights": []})


def arm(tls):
    p = tport if tls else port
    stop = threading.Event()
    window = [None, None]
    stats = [dict(frames=0, bytes=0, delays=[], connect_ms=None) for _ in range(n)]

    def run(i):
        t0 = time.time()
        st = fc.Stream(host, p, h, timeout=30.0, q=q, tls=tls)
        stats[i]["connect_ms"] = (time.time() - t0) * 1000.0
        try:
            while not stop.is_set():
                op, pl = st.recv()
                if op != 2:
                    continue
                now = time.time()
                if window[0] is None or now < window[0] or now > window[1]:
                    continue
                meta, au = fc.parse_fcvf(pl)
                stats[i]["frames"] += 1
                stats[i]["bytes"] += len(pl)
                stats[i]["delays"].append((now - (epoch / 1000.0 + meta["t"])) * 1000.0)
        except (ConnectionError, socket.timeout, OSError):
            pass
        finally:
            st.close()

    th = [threading.Thread(target=run, args=(i,)) for i in range(n)]
    for t in th:
        t.start()
    time.sleep(2.0)
    c0 = fc.health(host, port, tls=False)["cpu_s"]
    window[0] = time.time()
    window[1] = window[0] + secs
    time.sleep(secs)
    c1 = fc.health(host, port, tls=False)["cpu_s"]
    stop.set()
    for t in th:
        t.join(timeout=10)
    d = sorted(x for s in stats for x in s["delays"])
    fps = statistics.mean(s["frames"] for s in stats) / secs
    mbit = statistics.mean(s["bytes"] for s in stats) * 8 / secs / 1e6
    cpu = (c1 - c0) / secs * 100.0
    return dict(mode="wss" if tls else "ws", streams=n, h=h, q=q, fps_per_stream=round(fps, 1),
                mbit_per_stream=round(mbit, 2), cpu_pct=round(cpu, 1), delay_p50_ms=round(d[len(d) // 2], 1) if d else None,
                delay_p95_ms=round(d[int(len(d) * 0.95)], 1) if d else None,
                connect_ms=round(statistics.median(s["connect_ms"] for s in stats if s["connect_ms"]), 1))


rows = [arm(False), arm(True)]
print(",".join(rows[0].keys()))
for r in rows:
    print(",".join(str(v) for v in r.values()))
print(json.dumps({"cpu_pct_per_stream_added": round((rows[1]["cpu_pct"] - rows[0]["cpu_pct"]) / n, 2),
                  "delay_p50_added_ms": round(rows[1]["delay_p50_ms"] - rows[0]["delay_p50_ms"], 2)}))
