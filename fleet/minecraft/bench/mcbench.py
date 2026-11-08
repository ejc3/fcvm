#!/usr/bin/env python3
"""Minecraft server density on one host (docs/fleet/minecraft.md, "Density").

Run as root on a bare-metal pbox with fcvm set up (never on the shared dev host). Every server
is an fcvm VM of one shape (--cpu 1, --mem MCB_MEM MiB, bridged), restored from a gold snapshot
of its arm's server with its world loaded. The bots are `mctile bots` processes on the host,
one per server, on cores of their own; the VMs run in one cgroup (FCVM_CGROUP_PARENT) whose
cpuset holds --cores cores, so what the VMs use is what the servers use.

  mcbench.py java-image ARM           build the vanilla or Paper image (downloads the jar here)
  mcbench.py gold ARM                 boot ARM's server, join the bots, snapshot it (--stop)
  mcbench.py ladder OUT               for each count, every arm in turn: K clones, N bots each,
                                      warm, then sample every server's ticks per second
  mcbench.py restore OUT              one clone at a time: snapshot run to a bot's first chunk
  mcbench.py fork OUT                 a running clone with bots: snapshot create (the source
                                      keeps running), then the copy's restore to first chunk

Arms: pumpkin (the tile image, its own bots off), vanilla, paper. Every time is taken on the
host by this process's clock, and every server's tick rate by its bots (mctile's Tps).
"""
import argparse, atexit, ctypes, json, os, signal, socket, subprocess, sys, time, urllib.request

FCVM = os.environ.get("FCVM", "/usr/local/bin/fcvm")
MCTILE = os.environ.get("MCTILE", "/usr/local/bin/mctile")
MEM = int(os.environ.get("MCB_MEM", "3072"))
CG = "mcbench"
CGROOT = "/sys/fs/cgroup"
IMAGES = {
    "pumpkin": os.environ.get("MCB_PUMPKIN_IMAGE", "localhost/fcvm-minecraft:bench"),
    "vanilla": "localhost/mcbench-vanilla:26.3",
    "paper": "localhost/mcbench-paper:26.3",
}
# The servers' exact builds (the campaign page names them).
JARS = {
    "vanilla": {"version": "26.3", "sha1": "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"},
    "paper": {"version": "26.3", "build": 244, "sha256": "9601f9a26a2a52a0f2c807cf1abf691f96ee4fb287878313b601cebc2c51eabf"},
}
HOLD_TPS = 19.5
BOT_PORT0 = 23200
# Every process group this run started: stopped when it ends, however it ends.
CHILDREN = []


def _reap():
    for p in CHILDREN:
        if p.poll() is None:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    for p in CHILDREN:
        try:
            p.wait(30)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)


atexit.register(_reap)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))

PR_SET_PDEATHSIG = 1


def _dies_with(parent):
    """preexec_fn: the child gets SIGTERM when this process dies, SIGKILL and OOM kills included,
    which no atexit handler survives. Checked again after arming, for a parent that died first."""

    def arm():
        if ctypes.CDLL(None, use_errno=True).prctl(PR_SET_PDEATHSIG, signal.SIGTERM, 0, 0, 0) != 0:
            os._exit(127)
        if os.getppid() != parent:
            os._exit(1)

    return arm


def popen(argv, **kw):
    """Every child of a run: its own process group (stopped as one), dying with this process."""
    p = subprocess.Popen(argv, start_new_session=True, preexec_fn=_dies_with(os.getpid()), **kw)
    CHILDREN.append(p)
    return p


def now():
    return time.monotonic()


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def sh(*args, check=True, **kw):
    return subprocess.run(args, check=check, text=True, capture_output=True, **kw)


def get(url, timeout=1.0):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.load(r)
    except Exception:
        return None


def vms():
    out = sh(FCVM, "ls", "--json", check=False).stdout or "[]"
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return []


def vm(name):
    return next((v for v in vms() if v.get("name") == name), None)


def wait_ip(name, deadline=120):
    end = now() + deadline
    while now() < end:
        v = vm(name)
        ip = (v or {}).get("config", {}).get("network", {}).get("host_ip")
        if ip:
            return v["pid"], ip
        time.sleep(0.02)
    raise RuntimeError(f"{name} has no address after {deadline} s")


# --- cgroup: the VMs' cores and accounting ------------------------------------------------

def cg_setup(cores):
    path = f"{CGROOT}/{CG}"
    with open(f"{CGROOT}/cgroup.subtree_control", "w") as f:
        f.write("+cpuset +cpu +memory")
    os.makedirs(path, exist_ok=True)
    with open(f"{path}/cpuset.cpus", "w") as f:
        f.write(f"0-{cores - 1}")
    with open(f"{path}/cgroup.subtree_control", "w") as f:
        f.write("+cpuset +cpu +memory")
    return path


def cg_read(key):
    path = f"{CGROOT}/{CG}"
    if key == "memory":
        return int(open(f"{path}/memory.current").read())
    if key == "anon":
        for line in open(f"{path}/memory.stat"):
            k, v = line.split()
            if k == "anon":
                return int(v)
        return 0
    for line in open(f"{path}/cpu.stat"):
        k, v = line.split()
        if k == "usage_usec":
            return int(v)
    return 0


def fcvm_env():
    return dict(os.environ, FCVM_CGROUP_PARENT=CG, RUST_LOG="fcvm=info")


def bot_cores(cores):
    return f"{cores}-{os.cpu_count() - 1}"


# --- servers and bots ----------------------------------------------------------------------

class Bots:
    """One `mctile bots` process for one server, on the bots' cores."""

    def __init__(self, server, n, port, cores, prefix="b"):
        self.port = port
        with socket.socket() as probe:
            if probe.connect_ex(("127.0.0.1", port)) == 0:
                raise RuntimeError(f"port {port} is already served: a bots process left by another run")
        self.log = open(f"/tmp/mcbench-bots-{port}.log", "w")
        self.p = popen(
            ["taskset", "-c", bot_cores(cores), MCTILE, "bots", "--server", server, "--bots", str(n),
             "--port", str(port), "--bot-prefix", prefix],
            stdout=self.log, stderr=subprocess.STDOUT)

    def health(self):
        return get(f"http://127.0.0.1:{self.port}/health")

    def stop(self):
        try:
            os.killpg(self.p.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            os.killpg(self.p.pid, signal.SIGKILL)
        self.log.close()


def boot_argv(arm, name):
    a = [FCVM, "podman", "run", "--name", name, "--network", "bridged", "--cpu", "1", "--mem", str(MEM),
         "--publish", "25565:25565"]
    if arm == "pumpkin":
        a += ["--env", "MCTILE_BOTS=0"]
    else:
        a += ["--env", "MCBENCH_EULA=true"]
    return a + [IMAGES[arm]]


def gold_tag(arm):
    return f"mcb-{arm}-gold"


def spawn(argv, logpath):
    lf = open(logpath, "w")
    return popen(argv, stdout=lf, stderr=subprocess.STDOUT, env=fcvm_env())


def kill_vm(proc, deadline=30):
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(deadline)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()


def wait_bots(bots, n, deadline, tps=False):
    """Until every bots process reports n bots online (and a tick rate, with tps)."""
    end = now() + deadline
    while now() < end:
        hs = [b.health() for b in bots]
        if all(h and h["bots"]["online"] >= n and (not tps or h.get("tps")) for h in hs):
            return True
        time.sleep(0.25)
    return False


def cmd_java_image(args):
    arm = args.arm
    if arm == "vanilla":
        manifest = json.load(urllib.request.urlopen("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"))
        meta = next(v for v in manifest["versions"] if v["id"] == JARS[arm]["version"])
        d = json.load(urllib.request.urlopen(meta["url"]))["downloads"]["server"]
        assert d["sha1"] == JARS[arm]["sha1"], d
        build = ["--build-arg", f"SERVER_URL={d['url']}", "--build-arg", f"SERVER_SHA1={d['sha1']}"]
    else:
        j = JARS[arm]
        b = json.load(urllib.request.urlopen(f"https://fill.papermc.io/v3/projects/paper/versions/{j['version']}/builds/{j['build']}"))
        url = b["downloads"]["server:default"]["url"]
        build = ["--build-arg", f"SERVER_URL={url}", "--build-arg", f"SERVER_SHA256={j['sha256']}"]
    here = os.path.join(os.path.dirname(os.path.abspath(__file__)), "java")
    subprocess.run(["podman", "build", "--squash-all", "-t", IMAGES[arm], *build, here], check=True)


def make_gold(arm, cores, bots_n):
    """Boot ARM's server, join its bots until its world around the spawn is loaded and it holds
    20 ticks a second, then snapshot it, stopped."""
    name = f"mcb-{arm}-goldvm"
    t0 = now()
    p = spawn(boot_argv(arm, name), f"/tmp/mcbench-{name}.log")
    pid, ip = wait_ip(name, 600)
    b = Bots(f"{ip}:25565", bots_n, BOT_PORT0 - 1, cores)
    try:
        end = now() + 600
        steady = 0
        while now() < end:
            h = b.health() or {}
            ok = h.get("ready") and (h.get("tps") or 0) >= HOLD_TPS
            steady = steady + 1 if ok else 0
            if steady >= 10:
                break
            time.sleep(1)
        else:
            raise RuntimeError(f"{arm}'s gold server was not ready with 20 TPS in 600 s: {b.health()}")
        ready_s = now() - t0
        tc = now()
        r = sh(FCVM, "snapshot", "create", "--pid", str(pid), "--tag", gold_tag(arm), "--stop", check=False, env=fcvm_env())
        if r.returncode:
            raise RuntimeError(f"snapshot create failed: {r.stdout[-2000:]} {r.stderr[-2000:]}")
        return {"arm": arm, "boot_to_ready_s": ready_s, "snapshot_s": now() - tc}
    finally:
        b.stop()
        kill_vm(p)


def cmd_gold(args):
    cg_setup(args.cores)
    for arm in args.arms.split(","):
        log("gold", arm)
        print(json.dumps(make_gold(arm, args.cores, args.bots)), flush=True)


def restore(arm, name, logdir):
    argv = [FCVM, "snapshot", "run", "--snapshot", gold_tag(arm), "--name", name]
    return spawn(argv, f"{logdir}/{name}.log")


def pss_firecracker():
    total = 0
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            # comm is cut at 15 characters (firecracker-def for the default profile's binary).
            if not open(f"/proc/{pid}/comm").read().startswith("firecracker"):
                continue
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Pss:"):
                    total += int(line.split()[1]) * 1024
        except OSError:
            pass
    return total


def ladder_step(arm, k, args, out):
    logdir = f"{out}/logs/{arm}-{k}"
    os.makedirs(logdir, exist_ok=True)
    mem0 = cg_read("memory")
    procs, bots = [], []
    rec = {"arm": arm, "servers": k, "cores": args.cores, "bots_per_server": args.bots, "mem_mib": MEM}
    try:
        for i in range(k):
            name = f"mcb-{arm}-{k}-{i}"
            procs.append(restore(arm, name, logdir))
            _, ip = wait_ip(name, 300)
            bots.append(Bots(f"{ip}:25565", args.bots, BOT_PORT0 + i, args.cores))
        joined = wait_bots(bots, args.bots, 300, tps=True)
        rec["joined"] = joined
        if not joined:
            rec["holds"] = False
            rec["why"] = "bots did not all join within 300 s"
            return rec
        time.sleep(args.warm)
        cpu0, t0 = cg_read("cpu"), now()
        samples = [[] for _ in bots]
        stale = [0 for _ in bots]
        while now() - t0 < args.measure:
            for i, b in enumerate(bots):
                h = b.health() or {}
                # A reading counts only while every bot is online and the rate's last game time
                # is under 3 s old; anything else reads as 0, a failure.
                fresh = (h.get("tps_age_ms") is not None and h["tps_age_ms"] < 3000
                         and (h.get("bots") or {}).get("online") == args.bots)
                samples[i].append((h.get("tps") or 0.0) if fresh else 0.0)
                if not fresh:
                    stale[i] += 1
            time.sleep(5)
        dt = now() - t0
        rec["vm_cores_used"] = (cg_read("cpu") - cpu0) / 1e6 / dt
        rec["cgroup_mem_mib"] = cg_read("memory") / 2**20
        rec["cgroup_mem_added_mib"] = (cg_read("memory") - mem0) / 2**20
        rec["cgroup_anon_mib"] = cg_read("anon") / 2**20
        rec["firecracker_pss_mib"] = pss_firecracker() / 2**20
        mins = [min(s) for s in samples]
        rec["tps_min"] = min(mins)
        rec["tps_median_of_mins"] = sorted(mins)[len(mins) // 2]
        rec["servers_below"] = sum(1 for m in mins if m < HOLD_TPS)
        rec["stale_readings"] = sum(stale)
        rec["readings_per_server"] = len(samples[0]) if samples else 0
        rec["holds"] = rec["servers_below"] == 0
        rec["servers_per_core"] = k / args.cores
        return rec
    finally:
        for b in bots:
            b.stop()
        for p in procs:
            kill_vm(p)


def cmd_ladder(args):
    os.makedirs(args.out, exist_ok=True)
    cg_setup(args.cores)
    arms = args.arms.split(",")
    failed = {}
    passed = {a: 0 for a in arms}
    counts = [int(c) for c in args.counts.split(",")]
    with open(f"{args.out}/ladder.jsonl", "a") as f:
        for k in counts:
            for arm in arms:
                if arm in failed:
                    continue
                log("ladder", arm, k)
                rec = ladder_step(arm, k, args, args.out)
                f.write(json.dumps(rec) + "\n")
                f.flush()
                log(json.dumps(rec))
                if rec["holds"]:
                    passed[arm] = k
                else:
                    failed[arm] = k
        # Bisect each arm between its last pass and its first failure.
        for _ in range(args.bisect):
            for arm in arms:
                lo, hi = passed[arm], failed.get(arm)
                if hi is None or hi - lo <= max(1, args.cores // 4):
                    continue
                k = (lo + hi) // 2
                log("bisect", arm, k)
                rec = ladder_step(arm, k, args, args.out)
                rec["bisect"] = True
                f.write(json.dumps(rec) + "\n")
                f.flush()
                if rec["holds"]:
                    passed[arm] = k
                else:
                    failed[arm] = k
    print(json.dumps({"passed": passed, "failed": failed}))


def cmd_confirm(args):
    """Run given (arm, count) steps again, interleaved, --repeat times: a ladder's boundary is
    one run per count, and a density holds only where every run of it held."""
    os.makedirs(args.out, exist_ok=True)
    cg_setup(args.cores)
    plan = [(a, int(k)) for a, k in (x.split(":") for x in args.plan.split(","))]
    with open(f"{args.out}/ladder.jsonl", "a") as f:
        for rep in range(args.repeat):
            for arm, k in plan:
                log("confirm", rep, arm, k)
                rec = ladder_step(arm, k, args, args.out)
                rec["confirm"] = rep + 1
                f.write(json.dumps(rec) + "\n")
                f.flush()
                log(json.dumps(rec))


def first_chunk_after(b, t_start, deadline=120):
    """Host ms from t_start until the bots process saw its first bot's first chunk."""
    end = now() + deadline
    while now() < end:
        h = b.health() or {}
        if h.get("times_unix_ms", {}).get("first_chunk"):
            return (now() - t_start) * 1000
        time.sleep(0.005)
    return None


def cmd_restore(args):
    os.makedirs(args.out, exist_ok=True)
    cg_setup(args.cores)
    arms = args.arms.split(",")
    with open(f"{args.out}/restore.jsonl", "a") as f:
        for t in range(args.trials):
            for arm in arms:
                name = f"mcb-{arm}-r{t}"
                t0 = now()
                p = restore(arm, name, args.out)
                try:
                    _, ip = wait_ip(name, 120)
                    t_ip = (now() - t0) * 1000
                    b = Bots(f"{ip}:25565", 1, BOT_PORT0, args.cores)
                    try:
                        fc = first_chunk_after(b, t0)
                    finally:
                        b.stop()
                    # The probe's own cost: a fresh bot joining the same server, now warm.
                    tp = now()
                    b2 = Bots(f"{ip}:25565", 1, BOT_PORT0 + 1, args.cores, prefix="p")
                    try:
                        probe = first_chunk_after(b2, tp)
                    finally:
                        b2.stop()
                finally:
                    kill_vm(p)
                rec = {"arm": arm, "trial": t, "address_ms": t_ip, "first_chunk_ms": fc, "probe_join_ms": probe}
                f.write(json.dumps(rec) + "\n")
                f.flush()
                log(json.dumps(rec))


def fork_total(snap_ms, snap_rc, restore_ms):
    """A fork's time, or None for a fork that did not copy (its snapshot failed) or whose copy
    sent no first chunk: the summary counts those as failed."""
    if snap_rc != 0 or restore_ms is None:
        return None
    return snap_ms + restore_ms


def cmd_fork(args):
    os.makedirs(args.out, exist_ok=True)
    cg_setup(args.cores)
    arms = args.arms.split(",")
    with open(f"{args.out}/fork.jsonl", "a") as f:
        for t in range(args.trials):
            for arm in arms:
                src = f"mcb-{arm}-fs{t}"
                ps = restore(arm, src, args.out)
                procs, bots = [ps], []
                try:
                    pid, ip = wait_ip(src, 120)
                    bs = Bots(f"{ip}:25565", args.bots, BOT_PORT0, args.cores)
                    bots.append(bs)
                    wait_bots([bs], args.bots, 120, tps=True)
                    time.sleep(10)
                    before = bs.health()
                    tag = f"mcb-{arm}-fork{t}"
                    t0 = now()
                    r = sh(FCVM, "snapshot", "create", "--pid", str(pid), "--tag", tag, check=False, env=fcvm_env())
                    snap_ms = (now() - t0) * 1000
                    t1 = now()
                    dst = f"mcb-{arm}-fd{t}"
                    procs.append(spawn([FCVM, "snapshot", "run", "--snapshot", tag, "--name", dst], f"{args.out}/{dst}.log"))
                    _, ip2 = wait_ip(dst, 120)
                    bd = Bots(f"{ip2}:25565", 1, BOT_PORT0 + 1, args.cores, prefix="f")
                    bots.append(bd)
                    restore_ms = first_chunk_after(bd, t1)
                    time.sleep(5)
                    after = bs.health()
                    rec = {"arm": arm, "trial": t, "snapshot_ms": snap_ms, "snapshot_rc": r.returncode,
                           "restore_first_chunk_ms": restore_ms,
                           "fork_ms": fork_total(snap_ms, r.returncode, restore_ms),
                           "source_sessions_before": before and before.get("sessions"),
                           "source_sessions_after": after and after.get("sessions"),
                           "source_tps_after": after and after.get("tps")}
                    f.write(json.dumps(rec) + "\n")
                    f.flush()
                    log(json.dumps(rec))
                finally:
                    for b in bots:
                        b.stop()
                    for p in procs:
                        kill_vm(p)
                    sh("rm", "-rf", f"/mnt/fcvm-btrfs/snapshots/mcb-{arm}-fork{t}", f"/mnt/fcvm-btrfs/snapshots/mcb-{arm}-fork{t}.lock", check=False)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    j = sub.add_parser("java-image"); j.add_argument("arm", choices=["vanilla", "paper"])
    for name in ("gold", "ladder", "restore", "fork", "confirm"):
        p = sub.add_parser(name)
        p.add_argument("--arms", default="pumpkin,vanilla,paper")
        p.add_argument("--cores", type=int, default=4)
        p.add_argument("--bots", type=int, default=3)
        if name != "gold":
            p.add_argument("out")
        if name in ("ladder", "confirm"):
            p.add_argument("--warm", type=int, default=30)
            p.add_argument("--measure", type=int, default=60)
        if name == "confirm":
            p.add_argument("--plan", required=True, help="arm:count,arm:count,...")
            p.add_argument("--repeat", type=int, default=2)
        if name == "ladder":
            p.add_argument("--counts", default="4,8,16,32,64")
            p.add_argument("--bisect", type=int, default=2)
        if name in ("restore", "fork"):
            p.add_argument("--trials", type=int, default=10)
    args = ap.parse_args()
    {"java-image": cmd_java_image, "gold": cmd_gold, "ladder": cmd_ladder, "restore": cmd_restore, "fork": cmd_fork,
     "confirm": cmd_confirm}[args.cmd](args)


if __name__ == "__main__":
    main()
