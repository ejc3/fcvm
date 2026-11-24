#!/usr/bin/env python3
"""Memory of K servers against their age (hostramp.py's shape): restore K from the 3-bot gold
snapshot, then sample once a minute for MINUTES.

  FCVM=<fcvm> hostgrow.py K MINUTES OUT
"""
import json, os, subprocess, sys, time
import hostramp as R

K, MINUTES, OUT = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
R.cg_setup()
os.makedirs(f"{OUT}/logs/grow", exist_ok=True)
cg0 = R.cg_stat()
names = [f"mcg-{i}" for i in range(K)]
t0 = time.monotonic()
procs = [R.restore(3, n, f"{OUT}/logs/grow") for n in names]
ips = {}
try:
    while len(ips) < K:
        v = R.vms()
        ips = {n: R.ip_of(v.get(n)) for n in names if R.ip_of(v.get(n))}
        time.sleep(0.5)
    while not all(R.is_up(h, 3) for h in R.POOL.map(lambda n: R.health(ips[n]), names)):
        time.sleep(0.5)
    sc = list(R.POOL.map(lambda a: R.post(f"http://{ips[a[1]]}:8080/scene", {"tile": f"mc-{a[0] + 1}", "generation": 1, "admit_keys": [], "driver": None, "driver_until": None}), enumerate(names)))
    R.log("up", round(time.monotonic() - t0, 1), "scenes", sum(1 for s in sc if s == 200))

    def dump(tag):
        r = subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "viewdump.py"), ips[names[0]]], capture_output=True, text=True)
        open(f"{OUT}/world-{tag}.json", "w").write(r.stdout)
        R.log("dump", tag, len(r.stdout), r.stderr[-200:])

    time.sleep(15)
    dump("t0")
    with open(f"{OUT}/grow-{K}.jsonl", "a") as f:
        for minute in range(1, MINUTES + 1):
            c0, i0 = R.host_cpu()
            cga = R.cg_stat()
            ta = time.monotonic()
            time.sleep(max(0, t0 + minute * 60 - time.monotonic()))
            c1, i1 = R.host_cpu()
            cg = R.cg_stat()
            hs = list(R.POOL.map(lambda n: R.health(ips[n]), names))
            tps = sorted(R.reading(h, 3) for h in hs)
            pss = []
            for pid in os.listdir("/proc"):
                try:
                    if pid.isdigit() and open(f"/proc/{pid}/comm").read().startswith("firecracker"):
                        for line in open(f"/proc/{pid}/smaps_rollup"):
                            if line.startswith("Pss:"):
                                pss.append(int(line.split()[1]) // 1024)
                except OSError:
                    pass
            pss.sort()
            rec = {"minute": minute, "servers": K, "anon_mib_per_server": round((cg["anon"] - cg0["anon"]) / K / 2**20, 1),
                   "current_mib_per_server": round((cg["current"] - cg0["current"]) / K / 2**20, 1), "file_mib_per_server": round((cg["file"] - cg0["file"]) / K / 2**20, 1),
                   "pss_mib": {"min": pss[0], "p50": pss[len(pss) // 2], "max": pss[-1]} if pss else None,
                   "tps_min": tps[0], "tps_p50": tps[len(tps) // 2], "below_hold": sum(1 for t in tps if t < R.HOLD),
                   "host_busy_cores": round((1 - (i1 - i0) / max(1, c1 - c0)) * R.NCPU, 2),
                   "vm_cores": round((cg["cpu_usec"] - cga["cpu_usec"]) / 1e6 / (time.monotonic() - ta), 2),
                   "mem_available_gib": round(R.meminfo()["MemAvailable"] / 2**30, 1),
                   "chunks": sorted((h or {}).get("chunks", 0) for h in hs)[len(hs) // 2]}
            f.write(json.dumps(rec) + "\n"); f.flush()
            R.log(json.dumps(rec))
            if minute in (8, MINUTES):
                dump(f"t{minute}")
finally:
    R.kill_all(procs)
    print("GROW-DONE", flush=True)
