#!/usr/bin/env python3
"""Minecraft servers on one host in the fleet's shape: each server an fcvm VM of 1 vCPU and MC_MEM MiB (1024) running the tile image with its own
bots inside (MCTILE_BOTS), bridged, restored from one gold snapshot. Run as root on a bare-metal
box with fcvm set up and the tile image built (never on the shared dev host). mcbench.py beside
this file is the other shape: bots outside the VMs, and the VMs held to a few cores.

  FCVM=<fcvm> hostramp.py gold BOTS               boot the tile, wait until ready at 20 TPS, snapshot --stop
  FCVM=<fcvm> hostramp.py ramp BOTS OUT COUNTS    for each count: restore that many (MC_BATCH in
                                                  flight), warm MC_WARM s, sample every tile's /health
                                                  every 5 s for MC_MEASURE s, restore three more, tear down

MC_CORES holds the VMs' cgroup to that many cores (the edge in servers per core); MC_SUFFIX names
the record. A reading counts only with every bot online and a game time under 3 s old, else it is 0.
"""
import concurrent.futures as cf
import json, os, signal, subprocess, sys, time, urllib.request

FCVM = os.environ["FCVM"]
IMAGE = os.environ.get("MC_IMAGE", "localhost/fcvm-minecraft:dev")
MEM = int(os.environ.get("MC_MEM", "1024"))
CG = "mcramp"
CGROOT = "/sys/fs/cgroup"
HOLD = 19.5
WARM = int(os.environ.get("MC_WARM", "30"))
MEASURE = int(os.environ.get("MC_MEASURE", "60"))
NCPU = os.cpu_count()
CHILDREN = []
POOL = cf.ThreadPoolExecutor(128)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def env():
    return dict(os.environ, FCVM_CGROUP_PARENT=CG, RUST_LOG="fcvm=info")


def spawn(argv, logpath):
    lf = open(logpath, "w")
    p = subprocess.Popen(argv, start_new_session=True, stdout=lf, stderr=subprocess.STDOUT, env=env())
    CHILDREN.append(p)
    return p


def kill_all(procs):
    for p in procs:
        if p.poll() is None:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    end = time.monotonic() + 120
    for p in procs:
        try:
            p.wait(max(0.1, end - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            p.wait()


def get(url, timeout=2.0):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.load(r)
    except Exception:
        return None


def post(url, body, timeout=3.0):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"content-type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return f"{e.code} {e.read()[:200]!r}"
    except Exception as e:
        return repr(e)


def vms():
    out = subprocess.run([FCVM, "ls", "--json"], text=True, capture_output=True).stdout or "[]"
    try:
        return {v.get("name"): v for v in json.loads(out)}
    except json.JSONDecodeError:
        return {}


def ip_of(v):
    return (v or {}).get("config", {}).get("network", {}).get("host_ip")


def cg_setup():
    cores = os.environ.get("MC_CORES")
    ctl = "+cpuset +cpu +memory" if cores else "+cpu +memory"
    with open(f"{CGROOT}/cgroup.subtree_control", "w") as f:
        f.write(ctl)
    os.makedirs(f"{CGROOT}/{CG}", exist_ok=True)
    if cores:
        # The VMs share this many cores: the edge in servers per core.
        with open(f"{CGROOT}/{CG}/cpuset.cpus", "w") as f:
            f.write(f"0-{int(cores) - 1}")
    with open(f"{CGROOT}/{CG}/cgroup.subtree_control", "w") as f:
        f.write(ctl)


def cg_stat():
    path = f"{CGROOT}/{CG}"
    out = {"current": int(open(f"{path}/memory.current").read())}
    for line in open(f"{path}/memory.stat"):
        k, v = line.split()
        if k in ("anon", "file", "shmem", "kernel", "file_mapped"):
            out[k] = int(v)
    for line in open(f"{path}/cpu.stat"):
        k, v = line.split()
        if k == "usage_usec":
            out["cpu_usec"] = int(v)
    return out


def host_cpu():
    f = open("/proc/stat").readline().split()[1:]
    v = list(map(int, f))
    idle = v[3] + v[4]
    return sum(v[:8]), idle


def meminfo():
    out = {}
    for line in open("/proc/meminfo"):
        k, v = line.split(":")
        if k in ("MemTotal", "MemAvailable", "Cached", "AnonPages", "Shmem"):
            out[k] = int(v.split()[0]) * 1024
    return out


def smaps_firecracker():
    tot = {"Pss": 0, "Rss": 0, "Pss_Anon": 0, "Pss_File": 0, "Pss_Shmem": 0, "Shared_Clean": 0, "Private_Dirty": 0, "n": 0}
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


def health(ip):
    return get(f"http://{ip}:8080/health")


def reading(h, bots):
    """A tick rate that counts: every bot online and the game time fresh, else 0."""
    if not h or h.get("tps") is None:
        return 0.0
    if (h.get("bots") or {}).get("online", 0) < bots:
        return 0.0
    age = h.get("tps_age_ms")
    if age is None or age > 3000:
        return 0.0
    return float(h["tps"])


def tag(bots):
    return f"mcr-gold-b{bots}"


def cmd_gold(bots):
    cg_setup()
    name = f"mcr-goldvm-b{bots}"
    t0 = time.monotonic()
    p = spawn([FCVM, "podman", "run", "--name", name, "--network", "bridged", "--cpu", "1", "--mem", str(MEM),
               "--publish", "8080:8080", "--env", f"MCTILE_BOTS={bots}", IMAGE], f"/tmp/{name}.log")
    try:
        ip = pid = None
        end = time.monotonic() + 900
        while time.monotonic() < end and not ip:
            v = vms().get(name)
            ip, pid = ip_of(v), (v or {}).get("pid")
            time.sleep(0.5)
        if not ip:
            raise RuntimeError("gold has no address")
        steady, h = 0, None
        while time.monotonic() < end:
            h = health(ip) or {}
            if bots > 0:
                ok = h.get("ready") and reading(h, bots) >= HOLD
            else:
                ok = (h.get("times_unix_ms") or {}).get("server_listening") is not None
            steady = steady + 1 if ok else 0
            if steady >= (10 if bots > 0 else 30):
                break
            time.sleep(1)
        else:
            raise RuntimeError(f"gold not ready: {h}")
        ready_s = time.monotonic() - t0
        tc = time.monotonic()
        r = subprocess.run([FCVM, "snapshot", "create", "--pid", str(pid), "--tag", tag(bots), "--stop"], text=True, capture_output=True, env=env())
        if r.returncode:
            raise RuntimeError(f"snapshot create failed: {r.stdout[-1500:]} {r.stderr[-1500:]}")
        print(json.dumps({"gold": tag(bots), "boot_to_ready_s": round(ready_s, 1), "snapshot_s": round(time.monotonic() - tc, 2), "health": h}), flush=True)
    finally:
        kill_all([p])


def restore(bots, name, logdir):
    return spawn([FCVM, "snapshot", "run", "--snapshot", tag(bots), "--name", name, "--on-restore", "mctile restored"], f"{logdir}/{name}.log")


def is_up(h, bots):
    if not h:
        return False
    if bots == 0:
        return (h.get("times_unix_ms") or {}).get("server_listening") is not None
    return bool(h.get("ready")) and (h.get("bots") or {}).get("online", 0) >= bots


def step(bots, k, out, batch):
    logdir = f"{out}/logs/b{bots}-{k}"
    os.makedirs(logdir, exist_ok=True)
    rec = {"bots": bots, "servers": k, "mem_mib": MEM, "ncpu": NCPU, "batch": batch, "vm_cores_allowed": int(os.environ.get("MC_CORES", NCPU))}
    procs, names, started, up_at, ips = [], [], {}, {}, {}
    cg0 = cg_stat()
    mi0 = meminfo()
    t0 = time.monotonic()
    try:
        deadline = t0 + 300 + k * 3
        nxt = 0
        while len(up_at) < k and time.monotonic() < deadline:
            # Keep `batch` restores in flight.
            while nxt < k and nxt - len(up_at) < batch:
                name = f"mcr-b{bots}-{k}-{nxt}"
                procs.append(restore(bots, name, logdir))
                names.append(name)
                started[name] = time.monotonic()
                nxt += 1
            dead = [n for n, p in zip(names, procs) if p.poll() is not None and n not in up_at]
            if dead:
                rec["died"] = dead[:5]
                break
            all_v = vms()
            for n in names:
                if n not in ips:
                    ip = ip_of(all_v.get(n))
                    if ip:
                        ips[n] = ip
            pend = [n for n in names if n in ips and n not in up_at]
            for n, h in zip(pend, POOL.map(lambda n: health(ips[n]), pend)):
                if is_up(h, bots):
                    up_at[n] = time.monotonic()
            time.sleep(0.2)
        rec["up"] = len(up_at)
        rec["start_wall_s"] = round(time.monotonic() - t0, 1)
        lat = sorted(up_at[n] - started[n] for n in up_at)
        if lat:
            rec["start_latency_s"] = {"p50": round(lat[len(lat) // 2], 2), "max": round(lat[-1], 2)}
        if len(up_at) < k:
            rec["held"] = False
            rec["why"] = "not every server came up"
            return rec
        # Each clone its own scene, as the agent posts it: the tile reseeds its bots from it.
        scene = list(POOL.map(lambda a: post(f"http://{ips[a[1]]}:8080/scene", {"tile": f"mc-{a[0] + 1}", "generation": 1, "admit_keys": [], "driver": None, "driver_until": None}), enumerate(names)))
        rec["scene_ok"] = sum(1 for s in scene if s == 200)
        if rec["scene_ok"] < k:
            rec["scene_first_error"] = str(next(s for s in scene if s != 200))
        log(f"b{bots} k={k}: up in {rec['start_wall_s']} s, scenes {rec['scene_ok']}; warm {WARM} s")
        time.sleep(WARM)
        cpu0, idle0 = host_cpu()
        cg1 = cg_stat()
        tm0 = time.monotonic()
        readings = {n: [] for n in names}
        load = []
        while time.monotonic() - tm0 < MEASURE:
            ts = time.monotonic()
            for n, h in zip(names, POOL.map(lambda n: health(ips[n]), names)):
                readings[n].append(round(reading(h, bots), 2) if bots else (1.0 if h else 0.0))
            load.append(open("/proc/loadavg").read().split()[0])
            time.sleep(max(0.0, 5 - (time.monotonic() - ts)))
        dt = time.monotonic() - tm0
        cpu1, idle1 = host_cpu()
        cg2 = cg_stat()
        mi1 = meminfo()
        flat = sorted(v for r in readings.values() for v in r)
        mins = sorted(min(r) for r in readings.values())
        rec["tps"] = {"min": flat[0], "p01": flat[len(flat) // 100], "p50": flat[len(flat) // 2], "readings": len(flat),
                      "servers_below_hold": sum(1 for m in mins if m < HOLD)}
        rec["held"] = bool(bots) and flat[0] >= HOLD
        rec["host_busy_cores"] = round((1 - (idle1 - idle0) / max(1, cpu1 - cpu0)) * NCPU, 2)
        rec["vm_cgroup_cores"] = round((cg2["cpu_usec"] - cg1["cpu_usec"]) / 1e6 / dt, 2)
        rec["loadavg_1m"] = load
        rec["cgroup_mem"] = {kk: cg2[kk] - cg0.get(kk, 0) for kk in cg2 if kk != "cpu_usec"}
        rec["smaps_firecracker"] = smaps_firecracker()
        rec["meminfo_before"] = mi0
        rec["meminfo_after"] = mi1
        # One more server while these run: `snapshot run` to its view ready, polled every 20 ms.
        trials = []
        for t in range(3):
            name = f"mcr-b{bots}-{k}-x{t}"
            ts = time.monotonic()
            p = restore(bots, name, logdir)
            procs.append(p)
            ip, t_ip, t_up = None, None, None
            while time.monotonic() - ts < 60 and t_up is None and p.poll() is None:
                if ip is None:
                    ip = ip_of(vms().get(name))
                    if ip:
                        t_ip = time.monotonic() - ts
                if ip is not None and is_up(get(f"http://{ip}:8080/health", 0.25), bots):
                    t_up = time.monotonic() - ts
                time.sleep(0.02)
            trials.append({"address_ms": None if t_ip is None else round(t_ip * 1000), "ready_ms": None if t_up is None else round(t_up * 1000)})
        rec["restore_one_more"] = trials
        return rec
    finally:
        td = time.monotonic()
        kill_all(procs)
        rec["teardown_s"] = round(time.monotonic() - td, 1)
        time.sleep(5)


def cmd_ramp(bots, out, counts, batch):
    cg_setup()
    os.makedirs(out, exist_ok=True)
    failed = 0
    with open(f"{out}/ramp-b{bots}{os.environ.get('MC_SUFFIX', '')}.jsonl", "a") as f:
        for k in counts:
            log(f"b{bots} step {k}")
            rec = step(bots, k, out, batch)
            f.write(json.dumps(rec) + "\n")
            f.flush()
            log(json.dumps({x: rec.get(x) for x in ("servers", "up", "held", "tps", "host_busy_cores", "vm_cgroup_cores", "start_wall_s", "restore_one_more", "why")}))
            if bots and not rec.get("held"):
                failed += 1
                if failed >= 2:
                    break


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        if sys.argv[1] == "gold":
            cmd_gold(int(sys.argv[2]))
        else:
            cmd_ramp(int(sys.argv[2]), sys.argv[3], [int(x) for x in sys.argv[4].split(",")], int(os.environ.get("MC_BATCH", "16")))
    finally:
        kill_all(CHILDREN)
