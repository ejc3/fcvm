#!/usr/bin/env python3
"""Watch a fleet's Minecraft servers through its coordinator, as a page does: the snapshot's
field and the hosts' status rows. Records a sample every 5 s to OUT/samples.jsonl, and on the
marks given (seconds after every server first ran) a full reading with the VMs' memory.

  fleetfield.py ws://127.0.0.1:PORT/live OUT N [--marks 300] [--until S] [--reset-at S] [--late-age S]

--reset-at sends a reset of every server S seconds after all ran and times the wave. --late-age
takes one more mark when the oldest server is S seconds old and then sends that reset (the
coordinator resets a server 30 minutes after its launch, so 1770 is the last steady reading).
fleetfield.sh starts the coordinator and agent this reads from. Run as root on the box (it reads
every firecracker's smaps_rollup at a mark).
"""
import argparse, base64, json, os, socket, struct, sys, threading, time

A = None
T0 = time.time()


def log(*a):
    print(f"{time.time() - T0:8.1f}", *a, flush=True)


def append(name, obj):
    with open(os.path.join(A.out, name), "a") as f:
        f.write(json.dumps(obj) + "\n")


class Live:
    """A /live socket: text frames in, masked text frames out."""

    def __init__(self, url):
        host, port = url.split("//")[1].split("/")[0].rsplit(":", 1)
        self.s = socket.create_connection((host, int(port)), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall(f"GET /live HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
        self.buf = b""
        while b"\r\n\r\n" not in self.buf:
            self.buf += self.s.recv(65536)
        head, self.buf = self.buf.split(b"\r\n\r\n", 1)
        assert b" 101 " in head.split(b"\r\n")[0], head[:200]
        self.s.settimeout(None)
        self.lock = threading.Lock()

    def need(self, n):
        while len(self.buf) < n:
            d = self.s.recv(1 << 20)
            if not d:
                raise EOFError("the coordinator closed the socket")
            self.buf += d
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv(self):
        data = b""
        while True:
            b0, b1 = self.need(2)
            n = b1 & 127
            if n == 126:
                n = struct.unpack(">H", self.need(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self.need(8))[0]
            payload = self.need(n)
            op = b0 & 15
            if op == 8:
                raise EOFError(f"closed: {payload!r}")
            if op == 9:
                self.send_raw(10, payload)
                continue
            if op == 10:
                continue
            data += payload
            if b0 & 128:
                return json.loads(data.decode())

    def send_raw(self, op, payload):
        mask = os.urandom(4)
        n = len(payload)
        head = bytes([0x80 | op]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n))
        with self.lock:
            self.s.sendall(head + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(payload)))

    def send(self, obj):
        self.send_raw(1, json.dumps(obj).encode())


def rows_of(msg):
    raw = base64.b64decode(msg["rows"])
    return [raw[i:i + 8] for i in range(0, len(raw), 8)]


def smaps():
    tot = {"Pss": 0, "Rss": 0, "Pss_Anon": 0, "n": 0}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            if not open(f"/proc/{pid}/comm").read().startswith("firecracker"):
                continue
            tot["n"] += 1
            for line in open(f"/proc/{pid}/smaps_rollup"):
                k = line.split(":")[0]
                if k in tot:
                    tot[k] += int(line.split()[1]) * 1024
        except OSError:
            pass
    return tot


def meminfo():
    out = {}
    for line in open("/proc/meminfo"):
        k, v = line.split(":")
        if k in ("MemTotal", "MemAvailable", "Cached", "AnonPages", "Shmem", "Dirty"):
            out[k] = int(v.split()[0]) // 1024
    return out


def summary(rows):
    phase = [r[0] for r in rows]
    tps = sorted(r[1] / 10 for r in rows if r[0] == 3 and r[1] != 255)
    silent = sum(1 for r in rows if r[0] == 3 and r[1] == 255)
    launch = sorted(struct.unpack("<H", r[6:8])[0] for r in rows if r[6:8] != b"\xff\xff")
    age = sorted(struct.unpack("<H", r[4:6])[0] for r in rows if r[0] == 3 and r[4:6] != b"\xff\xff")
    q = lambda v, f: v[min(len(v) - 1, int(len(v) * f))] if v else None
    return {
        "servers": len(rows),
        "phases": {k: phase.count(c) for k, c in (("none", 0), ("restoring", 1), ("forking", 2), ("running", 3), ("failed", 4))},
        "tps": {"n": len(tps), "min": q(tps, 0), "p01": q(tps, 0.01), "p10": q(tps, 0.1), "p50": q(tps, 0.5), "below_19_5": sum(1 for t in tps if t < 19.5), "no_answer": silent},
        "launch_ms": {"n": len(launch), "p50": q(launch, 0.5), "p90": q(launch, 0.9), "max": q(launch, 1)},
        "age_s": {"min": q(age, 0), "max": q(age, 1)},
    }


def main():
    global A, T0
    p = argparse.ArgumentParser()
    p.add_argument("url"); p.add_argument("out"); p.add_argument("n", type=int)
    p.add_argument("--reset-at", type=float, default=None)
    p.add_argument("--marks", default="300")
    p.add_argument("--until", type=float, default=420)
    # The coordinator resets a server 30 minutes after its launch. --late-age S takes one more mark when
    # the oldest server is S seconds old, then sends the reset of every server (before the first automatic one).
    p.add_argument("--late-age", type=float, default=None)
    A = p.parse_args()
    os.makedirs(A.out, exist_ok=True)
    T0 = time.time()
    live = Live(A.url)
    live.send({"type": "seat", "ticket": None})
    live.send({"type": "visibility", "visible": True, "minecraft": True})
    mem0 = meminfo()
    append("run.jsonl", {"event": "start", "n": A.n, "meminfo": mem0, "ncpu": os.cpu_count()})
    state = {"snapshot": None, "status": None, "status_at": 0.0, "snap_bytes": 0, "snapshots": 0, "status_bytes": 0, "statuses": 0}


    def reader():
        while True:
            m = live.recv()
            t = m.get("type")
            if t == "snapshot":
                state["snapshot"] = m["state"].get("minecraft")
                state["snapshots"] += 1
                state["snap_bytes"] += len(json.dumps(m))
            elif t == "minecraft_status":
                state["status"] = m
                state["status_at"] = time.time()
                state["statuses"] += 1
                state["status_bytes"] += len(json.dumps(m))
            elif t == "receipt":
                log("receipt", m)
                append("run.jsonl", {"event": "receipt", "at": time.time() - T0, "receipt": m})


    threading.Thread(target=reader, daemon=True).start()
    marks = sorted(float(x) for x in A.marks.split(",") if x)
    late_done = A.late_age is None
    all_up_at = None
    first_restoring_at = None
    reset = {"sent_at": None, "done_at": None, "down_seen": False}
    last = 0.0
    while True:
        time.sleep(0.2)
        now = time.time()
        st = state["status"]
        if st is None:
            if now - T0 > 900:
                log("no status rows in 900 s")
                sys.exit(2)
            continue
        rows = rows_of(st)
        s = summary(rows)
        if first_restoring_at is None and (s["phases"]["restoring"] or s["phases"]["running"]):
            first_restoring_at = now
            log("first server restoring")
            append("run.jsonl", {"event": "first_restoring", "at": now - T0})
        if all_up_at is None and s["phases"]["running"] >= A.n:
            all_up_at = now
            log("all", A.n, "running", round(now - first_restoring_at, 1), "s after the first began")
            append("run.jsonl", {"event": "all_running", "at": now - T0, "restore_all_s": now - first_restoring_at, "summary": s})
        # a reset of every server: from the intent until every row is running again with a new age
        if reset["sent_at"] is not None and reset["done_at"] is None:
            if s["phases"]["running"] < A.n:
                reset["down_seen"] = True
            young = sum(1 for r in rows if r[0] == 3 and r[4:6] != b"\xff\xff" and struct.unpack("<H", r[4:6])[0] <= now - reset["sent_at"] + 1)
            append("reset.jsonl", {"t": round(now - reset["sent_at"], 2), "running": s["phases"]["running"], "restoring": s["phases"]["restoring"], "relaunched": young})
            if young >= A.n:
                reset["done_at"] = now
                log("reset all done in", round(now - reset["sent_at"], 1), "s")
                append("run.jsonl", {"event": "reset_all_done", "at": now - T0, "reset_all_s": now - reset["sent_at"], "summary": s})
        if now - last >= 5:
            last = now
            snap = state["snapshot"] or {}
            rec = {"at": round(now - T0, 1), "since_all_up": None if all_up_at is None else round(now - all_up_at, 1), **s,
                   "host": {k: st.get(k) for k in ("cpus", "busy", "mem_total_mib", "mem_used_mib")}, "status_age_s": round(now - state["status_at"], 2),
                   "wire": {k: state[k] for k in ("snapshots", "snap_bytes", "statuses", "status_bytes")},
                   "snapshot": {"tiles": len(snap.get("tiles", [])), "field_servers": (snap.get("field") or {}).get("servers")}}
            append("samples.jsonl", rec)
            log(json.dumps({"up": s["phases"]["running"], "restoring": s["phases"]["restoring"], "failed": s["phases"]["failed"], "tps": s["tps"], "busy": st.get("busy"), "used_gib": round((st.get("mem_used_mib") or 0) / 1024, 1)}))
        if all_up_at is not None:
            since = now - all_up_at
            if marks and since >= marks[0]:
                m = marks.pop(0)
                sm = smaps()
                append("run.jsonl", {"event": "mark", "since_all_up": m, "summary": s, "host": {k: st.get(k) for k in ("cpus", "busy", "mem_total_mib", "mem_used_mib")},
                                     "meminfo": meminfo(), "meminfo_start": mem0, "smaps_firecracker": sm,
                                     "per_server_mib": {"host_used_delta": round(((mem0["MemAvailable"] - meminfo()["MemAvailable"])) / A.n, 1), "pss": round(sm["Pss"] / 2**20 / max(1, sm["n"]), 1), "pss_anon": round(sm["Pss_Anon"] / 2**20 / max(1, sm["n"]), 1), "rss": round(sm["Rss"] / 2**20 / max(1, sm["n"]), 1)}})
                log("mark", m, "s")
            if not late_done and (s["age_s"]["max"] or 0) >= A.late_age:
                late_done = True
                sm = smaps()
                append("run.jsonl", {"event": "mark", "since_all_up": round(since, 1), "oldest_age_s": s["age_s"]["max"], "late": True, "summary": s, "host": {k: st.get(k) for k in ("cpus", "busy", "mem_total_mib", "mem_used_mib")},
                                     "meminfo": meminfo(), "meminfo_start": mem0, "smaps_firecracker": sm,
                                     "per_server_mib": {"host_used_delta": round(((mem0["MemAvailable"] - meminfo()["MemAvailable"])) / A.n, 1), "pss": round(sm["Pss"] / 2**20 / max(1, sm["n"]), 1), "pss_anon": round(sm["Pss_Anon"] / 2**20 / max(1, sm["n"]), 1), "rss": round(sm["Rss"] / 2**20 / max(1, sm["n"]), 1)}})
                log("late mark at oldest age", s["age_s"]["max"])
                if A.reset_at is None:
                    A.reset_at = since + 3
            if A.reset_at is not None and reset["sent_at"] is None and since >= A.reset_at:
                reset["sent_at"] = now
                live.send({"type": "intent", "id": f"reset-{int(now)}", "name": "minecraft_reset", "args": {"all": True}})
                log("sent reset all")
                append("run.jsonl", {"event": "reset_all_sent", "at": now - T0})
            # a reset that was asked for is waited for: until every server is back, or ten minutes
            reset_over = A.reset_at is None or reset["done_at"] is not None or (reset["sent_at"] is not None and now - reset["sent_at"] >= 600)
            if since >= A.until and not marks and late_done and reset_over:
                append("run.jsonl", {"event": "end", "at": now - T0})
                log("done")
                break
        elif now - T0 > 3600:
            log("not every server ran within an hour")
            append("run.jsonl", {"event": "gave_up", "at": now - T0, "summary": s})
            sys.exit(3)


if __name__ == "__main__":
    main()
