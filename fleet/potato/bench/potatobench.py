#!/usr/bin/env python3
"""Hot potato measurements on one fleet host (docs/fleet/ARCHITECTURE.md, "Potato mode").

Run as root on a host with fcvm set up (a gpubox from the fleet host image), with the player
image built (make potato-image). It plays the agent's part itself: it boots player VMs in
fcvm's routed mode at <prefix>::<0x1000 + k>, reads their reports on 127.0.0.1:7071 (each VM
forwards that port to the host), and puts them to sleep (fcvm snapshot create, then the VM's
fcvm exits) and wakes them (fcvm snapshot run --snapshot).

  potatobench.py cycles --players 4 --cycles 200 [--full] OUT
      one player at a time: ball in, catch, throw to a host sink, sleep, wake; each stage
      timed, every wake's proof checked, one JSON line per cycle in OUT/cycles.jsonl
  potatobench.py ladder --sizes 2,4,8 --hold 2000 --shrink 100 OUT
      the game: the ball goes VM to VM; each size runs until the ball drops, from the
      given hold, shrinking it each lap; OUT/ladder.jsonl gets a line per catch and per drop
  potatobench.py memory --players 96 OUT
      boot N players, sleep them all, wake them one at a time; host memory and each awake
      player's resident memory at every step, and the wake latency with N - 1 asleep

Every time is taken on the host, by this process's monotonic clock.
"""
import argparse, asyncio, json, os, re, signal, socket, statistics, sys, time, urllib.request

FCVM = os.environ.get("FCVM", "/usr/local/bin/fcvm")
IMAGE = os.environ.get("POTATO_IMAGE", "localhost/fcvm-potato:dev")
REPORT_PORT = 7071
SINK_PORT = 7072
BALL_PORT = 7070
COLOURS = 12
MEM = int(os.environ.get("POTATO_MEM", "256"))
CPU = os.environ.get("POTATO_CPU", "1")
PROFILE = os.environ.get("POTATO_PROFILE", "")
STOP = os.environ.get("POTATO_STOP", "1") == "1"


ENV = dict(os.environ, RUST_LOG=os.environ.get("POTATO_RUST_LOG", "fcvm=info"))
TS = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def lines(path):
    try:
        return [ANSI.sub("", l) for l in open(path, errors="replace")]
    except OSError:
        return []


def stamp(line):
    m = TS.match(line)
    if not m:
        return None
    from datetime import datetime
    return datetime.fromisoformat(m.group(1)[:26]).timestamp()


def first(ls, needle):
    for l in ls:
        if needle in l:
            t = stamp(l)
            if t is not None:
                return t, l
    return None, None


def field(line, name):
    m = re.search(rf"\b{name}=([0-9.]+)", line or "")
    return float(m.group(1)) if m else None


def parse_create(path):
    """The steps of one `fcvm snapshot create`, from its info log."""
    ls = lines(path)
    t0 = stamp(ls[0]) if ls and stamp(ls[0]) else None
    tp, _ = first(ls, "preparing guest network and pausing VM")
    td, _ = first(ls, "copying disk (VM paused)")
    tr, _ = first(ls, "VM and guest network resumed")
    if tr is None:
        tr, _ = first(ls, "disposable source remains paused")
    tf, _ = first(ls, "snapshot created successfully")
    _, dl = first(ls, "diff written in place")
    _, kl = first(ls, "creating diff snapshot")
    _, sl = first(ls, "stopped the VM after its snapshot")
    _, bl = first(ls, "snapshot boundary timings")
    out = {"create_kind": "diff" if kl else "full", "diff_written_bytes": field(dl, "written_bytes"),
           "stop_ms": field(sl, "stop_ms")}
    for k in ("prepare_ms", "pause_ms", "save_ms", "disk_ms", "resume_ms"):
        out["boundary_" + k] = field(bl, k)
    if None not in (t0, tp, td, tr, tf):
        out.update(create_setup_ms=ms(t0, tp), create_boundary_to_disk_ms=ms(tp, td),
                   create_disk_and_resume_ms=ms(td, tr), create_finish_ms=ms(tr, tf))
    return out


def parse_firecracker(run_log):
    """The pause and save as the VMM timed them, from the log fcvm copies at teardown."""
    _, l = first(lines(run_log), "saved firecracker log")
    m = re.search(r"log=(\S+)", l or "")
    if not m:
        return {}
    pause = save = None
    for l in lines(m.group(1)):
        x = re.search(r"'pause vm' API request took (\d+) us", l)
        if x:
            pause = int(x.group(1)) / 1000.0
        x = re.search(r"'create (?:diff|full) snapshot' API request took (\d+) us", l)
        if x:
            save = int(x.group(1)) / 1000.0
    return {"vmm_pause_ms": pause, "vmm_save_ms": save}


def parse_teardown(run_log):
    _, l = first(lines(run_log), "vm teardown complete")
    return {k: field(l, k) for k in ("until_gone_ms", "processes_us", "network_us", "disk_us")}


def parse_wake(run_log):
    """The steps of one `fcvm snapshot run`, from its info log and its timing JSON."""
    ls = lines(run_log)
    t0 = stamp(ls[0]) if ls and stamp(ls[0]) else None
    tn, _ = first(ls, "network configured for clone")
    tk, _ = first(ls, "CoW disk prepared from snapshot")
    tl, ll = first(ls, "snapshot load completed")
    ta, _ = first(ls, "fc-agent acknowledged exact restore generation")
    out = {}
    if None not in (t0, tn, tk, tl, ta):
        out.update(wake_network_ms=ms(t0, tn), wake_disk_ms=ms(tn, tk), wake_vmm_to_loaded_ms=ms(tk, tl),
                   wake_load_ms=field(ll, "duration_ms"), wake_guest_to_ack_ms=ms(tl, ta))
    for l in ls:
        if l.startswith('{"ack_ms"'):
            try:
                j = json.loads(l)
                out["guest_phases"] = j.get("guest_phases")
                out["ack_ms"] = j.get("ack_ms")
            except ValueError:
                pass
    return out


def now():
    return time.monotonic()


def ms(a, b):
    return round((b - a) * 1000.0, 3)


def imds(path):
    tok = urllib.request.urlopen(urllib.request.Request(
        "http://169.254.169.254/latest/api/token", method="PUT",
        headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"}), timeout=2).read().decode()
    return urllib.request.urlopen(urllib.request.Request(
        "http://169.254.169.254/latest/meta-data/" + path,
        headers={"X-aws-ec2-metadata-token": tok}), timeout=2).read().decode().strip()


def host_prefix():
    if os.environ.get("POTATO_PREFIX"):
        return os.environ["POTATO_PREFIX"]
    mac = imds("mac")
    return imds(f"network/interfaces/macs/{mac}/ipv6-prefix").split()[0]


def host_ipv6(prefix):
    """<prefix>::1 on the uplink (the address fcvm never gives a VM), as the agent puts the
    aggregator's there; the throw sink listens on it."""
    import subprocess
    a = addr_in(prefix, 1)
    dev = subprocess.run(["ip", "-6", "route", "show", "default"], capture_output=True, text=True,
                         check=True).stdout.split(" dev ")[1].split()[0]
    subprocess.run(["ip", "-6", "addr", "replace", f"{a}/128", "dev", dev, "nodad"], check=True)
    return a


def addr_in(prefix, n):
    import ipaddress
    net = ipaddress.IPv6Network(prefix, strict=False)
    return str(net.network_address + n)


class Player:
    def __init__(self, k, prefix, out):
        self.k = k
        self.addr = addr_in(prefix, 0x1000 + k)
        self.prefix = prefix
        self.name = f"potato-{k}"
        self.tag = f"potato-{k}"
        self.proc = None
        self.phase = "new"
        self.queue = asyncio.Queue()  # caught, threw, dropped
        self.hellos = asyncio.Queue()  # hello, one per report connection
        self.colour = None
        self.counter = 0
        self.chain = None
        self.out = out
        self.listening_at = None
        self.lives = 0
        # called the moment a wake makes the player awake
        self.on_awake = None
        # a throw reported while the player was still waking: sleep it once awake
        self.sleep_pending = False

    def ball_addr(self):
        return f"[{self.addr}]:{BALL_PORT}"

    def logpath(self, what):
        return os.path.join(self.out, "logs", f"{self.name}.{self.lives:04d}.{what}.log")

    def log(self, what):
        return open(self.logpath(what), "ab")

    async def boot(self):
        args = [FCVM, "podman", "run", "--name", self.name, "--network", "routed",
                "--ipv6-prefix", self.prefix, "--ipv6-address", self.addr,
                "--mem", str(MEM), "--cpu", CPU, "--forward-localhost", str(REPORT_PORT),
                "--env", f"POTATO_PLAYER={self.k}", "--env", f"POTATO_COLOURS={COLOURS}"]
        if PROFILE:
            args += ["--kernel-profile", PROFILE]
        args.append(IMAGE)
        t = now()
        self.proc = await asyncio.create_subprocess_exec(
            *args, stdin=asyncio.subprocess.DEVNULL, stdout=self.log("run"), stderr=asyncio.subprocess.STDOUT,
            start_new_session=True, env=ENV)
        self.phase = "booting"
        _, hello = await self.hello(600)
        self.adopt(hello)
        self.phase = "awake"
        return ms(t, now())

    def adopt(self, hello):
        self.colour, self.counter, self.chain = hello["colour"], hello["counter"], hello["chain"]

    async def hello(self, timeout):
        return await asyncio.wait_for(self.hellos.get(), timeout)

    async def expect(self, kind, timeout):
        deadline = now() + timeout
        while True:
            left = deadline - now()
            if left <= 0:
                raise TimeoutError(f"{self.name}: no {kind} within {timeout} s (phase {self.phase})")
            t, msg = await asyncio.wait_for(self.queue.get(), left)
            if msg["type"] == kind:
                return t, msg

    async def sleep(self):
        """`fcvm snapshot create --stop` (POTATO_STOP=0: create, then SIGTERM to the VM's fcvm).
        Returns the stage times."""
        assert self.phase in ("awake", "sleeping"), self.phase
        self.phase = "sleeping"
        t0 = now()
        args = [FCVM, "snapshot", "create", "--pid", str(self.proc.pid), "--tag", self.tag]
        if STOP:
            args.append("--stop")
        p = await asyncio.create_subprocess_exec(
            *args, stdin=asyncio.subprocess.DEVNULL, stdout=self.log("create"), stderr=asyncio.subprocess.STDOUT, env=ENV)
        rc = await p.wait()
        t1 = now()
        if rc != 0:
            self.phase = "failed"
            raise RuntimeError(f"{self.name}: snapshot create exited {rc}")
        if not STOP:
            self.phase = "stopping"
            self.proc.send_signal(signal.SIGTERM)
        await self.proc.wait()
        t2 = now()
        self.proc = None
        self.phase = "asleep"
        self.listening_at = None
        out = {"snapshot_ms": ms(t0, t1), "teardown_ms": ms(t1, t2), "stop": STOP}
        out.update(parse_create(self.logpath("create")))
        if STOP and out.get("stop_ms") is not None:
            # the create's own wait for the VM's fcvm to exit is the teardown
            out["snapshot_ms"] = round(out["snapshot_ms"] - out["stop_ms"], 3)
            out["teardown_ms"] = round(out["teardown_ms"] + out["stop_ms"], 3)
        out.update(parse_teardown(self.logpath("run")))
        out.update(parse_firecracker(self.logpath("run")))
        return out

    async def wake(self, full=False):
        """fcvm snapshot run; returns when the ball port answers a probe and the player said hello."""
        self.phase = "waking"
        t0 = now()
        args = [FCVM, "snapshot", "run", "--snapshot", self.tag, "--name", self.name,
                "--ipv6-prefix", self.prefix, "--ipv6-address", self.addr]
        if full:
            args.append("--no-dirty-tracking")
        self.lives += 1
        self.proc = await asyncio.create_subprocess_exec(
            *args, stdin=asyncio.subprocess.DEVNULL, stdout=self.log("run"), stderr=asyncio.subprocess.STDOUT,
            start_new_session=True, env=ENV)
        while not self.hellos.empty():
            self.hellos.get_nowait()
        probe_task = asyncio.create_task(probe_until_open(self.addr, BALL_PORT, 60))
        hello_t, hello = await self.hello(60)
        probe_t, probe, refusals = await probe_task
        # a restored player catches again only once it is told it woke
        await tell(self.addr, {"type": "wake"})
        # awake as soon as it accepts and has said hello; the log is read afterwards
        self.phase = "awake"
        self.listening_at = probe_t
        if self.on_awake:
            self.on_awake(self)
        for _ in range(300):  # the log's acknowledgement line can land after the hello
            self.last_wake = parse_wake(self.logpath("run"))
            if "guest_phases" in self.last_wake and "wake_guest_to_ack_ms" in self.last_wake:
                break
            await asyncio.sleep(0.01)
        return t0, hello_t, hello, probe_t, probe, refusals


async def probe_until_open(addr, port, timeout):
    """Connect and send a probe every 2 ms until one is answered; returns (t, answer, refused)."""
    deadline = now() + timeout
    refused = 0
    line = (json.dumps({"type": "probe"}) + "\n").encode()
    while now() < deadline:
        try:
            r, w = await asyncio.wait_for(asyncio.open_connection(addr, port), 0.05)
            try:
                w.write(line)
                await w.drain()
                ans = await asyncio.wait_for(r.readline(), 0.5)
                if ans:
                    return now(), json.loads(ans), refused
            finally:
                w.close()
        except (OSError, asyncio.TimeoutError, json.JSONDecodeError):
            refused += 1
        await asyncio.sleep(0.002)
    raise TimeoutError(f"[{addr}]:{port} never answered a probe")


class Host:
    """The report server, the throw sink, and the players."""

    def __init__(self, out, n, prefix):
        self.out = out
        os.makedirs(os.path.join(out, "logs"), exist_ok=True)
        self.players = [Player(k, prefix, out) for k in range(n)]
        self.sink = asyncio.Queue()

    async def start(self):
        self.report_srv = await asyncio.start_server(self.on_report, "127.0.0.1", REPORT_PORT)
        self.sink_srv = await asyncio.start_server(self.on_sink, "::", SINK_PORT)

    async def on_report(self, r, w):
        player = None
        try:
            while True:
                line = await r.readline()
                if not line:
                    return
                t = now()
                msg = json.loads(line)
                if player is None:
                    player = self.players[msg["player"]]
                await (player.hellos if msg["type"] == "hello" else player.queue).put((t, msg))
        except (ConnectionError, ValueError):
            return
        finally:
            w.close()

    async def on_sink(self, r, w):
        line = await r.readline()
        t = now()
        w.write(b'{"type":"ok"}\n')
        await w.drain()
        w.close()
        await self.sink.put((t, json.loads(line)))


async def tell(addr, msg):
    """One message to a player's ball port; returns its answer."""
    r, w = await asyncio.wait_for(asyncio.open_connection(addr, BALL_PORT), 5)
    w.write((json.dumps(msg) + "\n").encode())
    await w.drain()
    ans = await asyncio.wait_for(r.readline(), 5)
    w.close()
    assert reply_ok(ans), ans
    return ans


async def send_ball(addr, ball):
    t = now()
    r, w = await asyncio.wait_for(asyncio.open_connection(addr, BALL_PORT), 5)
    w.write((json.dumps({"type": "ball", **ball}) + "\n").encode())
    await w.drain()
    ack = await asyncio.wait_for(r.readline(), 5)
    w.close()
    assert reply_ok(ack), ack
    return ms(t, now())


def tree_memory():
    """Rss and Pss (KiB) of every fcvm process tree, keyed by the root fcvm's pid."""
    ppid, comm = {}, {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            st = open(f"/proc/{d}/stat").read()
            ppid[int(d)] = int(st[st.rfind(")") + 2:].split()[1])
            comm[int(d)] = st[st.find("(") + 1:st.rfind(")")]
        except OSError:
            pass
    kids = {}
    for p, q in ppid.items():
        kids.setdefault(q, []).append(p)

    def rollup(p):
        out = {"rss": 0, "pss": 0}
        try:
            for line in open(f"/proc/{p}/smaps_rollup"):
                k, v = line.split(":", 1)
                if k == "Rss":
                    out["rss"] += int(v.split()[0])
                elif k == "Pss":
                    out["pss"] += int(v.split()[0])
        except OSError:
            pass
        return out

    trees = {}
    for p, c in comm.items():
        if c == "fcvm" and comm.get(ppid.get(p)) != "fcvm":
            tot = {"rss": 0, "pss": 0, "procs": 0}
            stack = [p]
            while stack:
                q = stack.pop()
                m = rollup(q)
                tot["rss"] += m["rss"]
                tot["pss"] += m["pss"]
                tot["procs"] += 1
                stack.extend(kids.get(q, []))
            trees[p] = tot
    return trees


def meminfo():
    out = {}
    for line in open("/proc/meminfo"):
        k, v = line.split(":", 1)
        out[k] = int(v.split()[0])
    return {k: out[k] for k in ("MemTotal", "MemFree", "MemAvailable", "Cached", "AnonPages", "Shmem")}


def chain_step(prev_hex, rnd, seq, colour):
    """fleet_potato::chain_step."""
    import hashlib
    import struct
    h = hashlib.sha256(b"fcvm-potato-v1" + bytes.fromhex(prev_hex) + struct.pack("<QQI", rnd, seq, colour))
    return h.hexdigest()


def proof_follows(want, hello):
    """Whether a woken player's hello holds the state it slept with (`want`: colour, counter,
    chain), or that state stepped by one catch whose report has not been read yet."""
    colour, counter, chain = want
    if (hello["colour"], hello["counter"], hello["chain"]) == want:
        return True
    return (hello["counter"] == counter + 1 and hello["colour"] == (colour + 1) % COLOURS
            and hello.get("last_seq") is not None
            and hello["chain"] == chain_step(chain, hello["last_round"], hello["last_seq"], hello["colour"]))


def reply_ok(line):
    """Whether a player's answer is an `ok`: decoded, never searched for the word."""
    try:
        return json.loads(line).get("type") == "ok"
    except (ValueError, AttributeError):
        return False


def hold_at(start, shrink, min_hold, n, seq):
    """The hold the ball carries when caught at `seq` in a ring of `n` (fleet_potato::Ball)."""
    h = start
    for _ in range(seq // max(n, 1)):
        h = max(min_hold, h - h * shrink // 1000)
    return h


def catch_ok(colour, counter, chain, msg):
    """Whether a catch report is the step of the player's last verified colour, counter and
    chain: all three, the chain replayed (fleet_potato::chain_step)."""
    want = (colour + 1) % COLOURS
    return (msg["colour"] == want and msg["counter"] == counter + 1
            and msg["chain"] == chain_step(chain, msg["round"], msg["seq"], want))


def cycle_ball(i, addr, sink):
    """The ball of serial cycle `i`: a round of its own, so a player that saw an earlier
    cycle's (round, seq) does not take this one for a repeat."""
    return {"round": i + 1, "seq": 0, "ring": [addr, sink], "hold_ms": 0,
            "shrink_permille": 0, "min_hold_ms": 0, "flight_ms": 3000, "trail": []}


def expected_after_catch(p):
    return (p.colour + 1) % COLOURS, p.counter + 1


async def cmd_cycles(a):
    prefix = host_prefix()
    host = Host(a.out, a.players, prefix)
    await host.start()
    sink = f"[{host_ipv6(prefix)}]:{SINK_PORT}"
    rec = open(os.path.join(a.out, "cycles.jsonl"), "a")
    for p in host.players:
        b = await p.boot()
        print(f"{p.name} booted in {b:.0f} ms", flush=True)
    for i in range(a.cycles):
        p = host.players[i % len(host.players)]
        exp_colour, exp_counter = expected_after_catch(p)
        ball = cycle_ball(i, p.ball_addr(), sink)
        t0 = now()
        in_ms = await send_ball(p.addr, ball)
        tc, caught = await p.expect("caught", 10)
        tt, threw = await p.expect("threw", 10)
        await host.sink.get()
        ok_catch = catch_ok(p.colour, p.counter, p.chain, caught)
        p.colour, p.counter, p.chain = caught["colour"], caught["counter"], caught["chain"]
        s = await p.sleep()
        t_w, hello_t, hello, probe_t, probe, refusals = await p.wake(full=a.full)
        ok_wake = (hello["colour"], hello["counter"], hello["chain"]) == (p.colour, p.counter, p.chain) \
            and (probe["colour"], probe["counter"]) == (p.colour, p.counter)
        row = {"cycle": i, "player": p.k, "full": a.full, "ball_in_ms": in_ms,
               "catch_report_ms": ms(t0, tc), "throw_send_ms": threw["send_ms"],
               "threw_report_ms": ms(t0, tt), **s,
               "restore_to_hello_ms": ms(t_w, hello_t), "restore_to_listen_ms": ms(t_w, probe_t),
               "probe_refusals": refusals, "cycle_ms": ms(t0, max(hello_t, probe_t)),
               **{"wake_" + k if not k.startswith("wake_") and k not in ("guest_phases", "ack_ms") else k: v
                  for k, v in p.last_wake.items()},
               "proof_ok": ok_catch and ok_wake, "colour": p.colour, "counter": p.counter}
        rec.write(json.dumps(row) + "\n")
        rec.flush()
        if i % 10 == 0 or not row["proof_ok"]:
            print(json.dumps(row), flush=True)
        if not row["proof_ok"]:
            print(f"PROOF MISMATCH: caught={caught} hello={hello} probe={probe}", flush=True)
    for p in host.players:
        if p.proc:
            await p.sleep()


async def cmd_memory(a):
    host = Host(a.out, a.players, host_prefix())
    await host.start()
    rec = open(os.path.join(a.out, "memory.jsonl"), "a")

    def sample(step, **kw):
        trees = tree_memory()
        row = {"step": step, "t": time.time(), "meminfo_kib": meminfo(), "awake_trees": len(trees),
               "rss_kib": sum(t["rss"] for t in trees.values()), "pss_kib": sum(t["pss"] for t in trees.values()),
               "per_tree": sorted(trees.values(), key=lambda t: -t["pss"])[:4], **kw}
        rec.write(json.dumps(row) + "\n")
        rec.flush()
        print(step, row["awake_trees"], "trees", row["pss_kib"] // 1024, "MiB pss",
              row["meminfo_kib"]["MemAvailable"] // 1024, "MiB available", flush=True)

    sample("start")
    sem = asyncio.Semaphore(a.parallel)

    async def boot(p):
        async with sem:
            return await p.boot()

    boots = await asyncio.gather(*(boot(p) for p in host.players))
    sample("all_awake", boot_ms=boots)

    async def sleep(p):
        async with sem:
            return await p.sleep()

    sleeps = await asyncio.gather(*(sleep(p) for p in host.players))
    sample("all_asleep", sleeps=sleeps)
    wakes = []
    for p in host.players[: a.wakes]:
        t_w, hello_t, hello, probe_t, probe, refusals = await p.wake()
        wakes.append({"player": p.k, "restore_to_listen_ms": ms(t_w, probe_t),
                      "restore_to_hello_ms": ms(t_w, hello_t)})
        await asyncio.sleep(1)
        sample("one_awake", player=p.k, wake=wakes[-1])
        s = await p.sleep()
        wakes[-1].update(s)
    sample("all_asleep_again", wakes=wakes)


async def cmd_ladder(a):
    host = Host(a.out, max(a.sizes), host_prefix())
    await host.start()
    rec = open(os.path.join(a.out, "ladder.jsonl"), "a")
    sem = asyncio.Semaphore(a.parallel)

    async def boot(p):
        async with sem:
            await p.boot()
            await p.sleep()

    await asyncio.gather(*(boot(p) for p in host.players))
    print("all players booted and asleep", flush=True)
    game = Game(host, a, rec)
    for p in host.players:
        asyncio.create_task(game.watch(p))
    for rnd, size in enumerate(a.sizes, start=1):
        ring = host.players[:size]
        a.hold = max(a.min_hold, a.start_floor, a.budget // max(size - 1, 1)) if a.budget else a.hold
        res = await game.play(ring, rnd)
        rec.write(json.dumps({"type": "size_done", "size": size, "start_hold_ms": a.hold, **res}) + "\n")
        rec.flush()
        print("SIZE", size, json.dumps(res), flush=True)
        await game.quiesce(ring)


class Game:
    """The agent's part in the game, as the host plays it.

    One watcher per player for the whole run checks every catch's proof, whatever round it
    belongs to (a ball can still be in the air when a round ends). A player that threw goes to
    sleep at once; a sleeping player is woken when the ball is `lead` catches away from it,
    `lead` set from the wakes seen so far (the slowest of the last 20 plus `a.margin` ms, in
    holds), and checked again whenever a sleep ends."""

    def __init__(self, host, a, rec):
        self.host, self.a, self.rec = host, a, rec
        self.round = None
        for p in host.players:
            p.on_awake = self.awake

    def awake(self, p):
        if p.sleep_pending:
            p.sleep_pending = False
            p.phase = "sleeping"
            t = asyncio.create_task(self.sleep(p))
            if self.round:
                self.round["tasks"].add(t)

    def write(self, row):
        self.rec.write(json.dumps(row) + "\n")
        self.rec.flush()

    async def watch(self, p):
        while True:
            t, msg = await p.queue.get()
            r = self.round
            if msg["type"] == "caught":
                ok = catch_ok(p.colour, p.counter, p.chain, msg)
                p.colour, p.counter, p.chain = msg["colour"], msg["counter"], msg["chain"]
                if r and msg["round"] == r["id"]:
                    r["seq"] = msg["seq"]
                    # the catch's own hold, from its seq: the thrower's report of the throw can
                    # come after it, on another connection
                    hold = hold_at(self.a.hold, self.a.shrink, self.a.min_hold, r["n"], msg["seq"])
                    r["hold"] = hold
                    self.write({"type": "caught", "round": r["id"], "size": r["n"], "player": p.k, "seq": msg["seq"],
                                "lap": msg["seq"] // r["n"], "hold_ms": hold, "t": t, "proof_ok": ok,
                                "lead": self.lead(), "awake": sum(q.phase != "asleep" for q in r["ring"])})
                    if not ok:
                        self.end(reason="colour", player=p.k, seq=msg["seq"])
                    self.schedule_wakes()
            elif msg["type"] == "threw":
                if r and msg["round"] == r["id"]:
                    self.write({"type": "threw", "round": r["id"], "player": p.k, "seq": msg["seq"],
                                "wait_ms": msg["wait_ms"], "send_ms": msg["send_ms"], "attempts": msg["attempts"], "t": t})
                    if p.phase == "awake":
                        p.phase = "sleeping"  # one sleep per life, even if a second throw is reported
                        r["tasks"].add(asyncio.create_task(self.sleep(p)))
                    elif p.phase == "waking":
                        p.sleep_pending = True
                    else:
                        self.write({"type": "threw_while", "player": p.k, "phase": p.phase, "seq": msg["seq"]})
            elif msg["type"] == "dropped":
                if r and msg["round"] == r["id"]:
                    r["landed"].set()
                    target = r["ring"][msg["seq"] % r["n"]]
                    self.end(reason="dropped", thrower=p.k, catcher=target.k, seq=msg["seq"], lap=msg["seq"] // r["n"],
                             hold_ms=r["hold"], catcher_phase=target.phase, error=msg["reason"],
                             attempts=msg["attempts"], dropped_at=t)
                    r["late_target"] = target

    def end(self, **kw):
        r = self.round
        if r and not r["stop"].is_set():
            r["result"].update(kw)
            r["stop"].set()

    def lead(self):
        r = self.round
        wakes = r["wakes"][-20:]
        slow = max(wakes) if wakes else self.a.first_wake_ms
        return max(1, int((slow + self.a.margin) // max(r["hold"], 1)) + 1)

    def schedule_wakes(self):
        r = self.round
        if not r or r["stop"].is_set():
            return
        n, L = r["n"], self.lead()
        for j in range(1, min(L, n - 1) + 1):
            p = r["ring"][(r["seq"] + j) % n]
            if p.phase == "asleep" and p.k not in r["waking"]:
                r["waking"].add(p.k)
                t = asyncio.create_task(self.wake(p))
                t.add_done_callback(lambda _t, k=p.k: r["waking"].discard(k))
                r["tasks"].add(t)

    async def sleep(self, p):
        try:
            s = await p.sleep()
            self.write({"type": "slept", "round": self.round["id"] if self.round else None, "player": p.k,
                        **{k: v for k, v in s.items() if not isinstance(v, dict)}})
        except Exception as e:  # noqa: BLE001
            self.write({"type": "sleep_failed", "player": p.k, "error": str(e)})
            return
        self.schedule_wakes()

    async def wake(self, p):
        r = self.round
        if p.phase != "asleep":
            return
        want = (p.colour, p.counter, p.chain)
        try:
            t_w, hello_t, hello, probe_t, probe, refusals = await p.wake()
        except Exception as e:  # noqa: BLE001
            self.write({"type": "wake_failed", "player": p.k, "error": str(e)})
            self.end(reason="wake_failed", player=p.k)
            return
        ok = proof_follows(want, hello)
        if r:
            r["wakes"].append(ms(t_w, probe_t))
        self.write({"type": "woke", "round": r["id"] if r else None, "player": p.k,
                    "restore_to_listen_ms": ms(t_w, probe_t), "restore_to_hello_ms": ms(t_w, hello_t),
                    "listening_at": probe_t, "proof_ok": ok,
                    **{k: v for k, v in p.last_wake.items() if not isinstance(v, dict)}})
        if not ok:
            self.end(reason="proof", player=p.k, hello=hello, want=list(want))
        late = r.get("late_target") if r else None
        if late is p and "dropped_at" in r["result"]:
            r["result"]["catcher_listening_after_drop_ms"] = ms(r["result"]["dropped_at"], probe_t)

    async def play(self, ring, rnd):
        a = self.a
        self.round = r = {"id": rnd, "n": len(ring), "ring": ring, "seq": 0, "hold": a.hold, "wakes": [],
                          "waking": set(), "tasks": set(), "stop": asyncio.Event(), "landed": asyncio.Event(), "result": {}}
        # every player wakes before the first ball, so the round starts from a full ring
        await asyncio.gather(*(self.wake(p) for p in ring))
        if r["stop"].is_set():
            return r["result"]
        ball = {"round": rnd, "seq": 0, "ring": [p.ball_addr() for p in ring], "hold_ms": a.hold,
                "shrink_permille": a.shrink, "min_hold_ms": a.min_hold, "flight_ms": a.flight, "trail": []}
        t0 = now()
        await send_ball(ring[0].addr, ball)
        try:
            await asyncio.wait_for(r["stop"].wait(), a.round_secs)
        except asyncio.TimeoutError:
            self.end(reason="timeout")
        res = r["result"]
        res.update(size=r["n"], seconds=round(now() - t0, 3), last_hold_ms=r["hold"], catches=r["seq"],
                   wakes=len(r["wakes"]), wake_p50=statistics.median(r["wakes"]) if r["wakes"] else None,
                   wake_max=max(r["wakes"]) if r["wakes"] else None)
        res.pop("dropped_at", None)
        return res

    async def quiesce(self, ring):
        """Let the round's sleeps and wakes finish, then put every awake player to sleep, so a
        ball still in the air drops and the next round starts from a quiet host."""
        r = self.round
        lap = r["n"] * (r["hold"] + self.a.flight) / 1000.0
        try:
            await asyncio.wait_for(r["landed"].wait(), lap + 30)
        except asyncio.TimeoutError:
            self.write({"type": "ball_still_up", "round": r["id"]})
        await asyncio.gather(*r["tasks"], return_exceptions=True)
        # the round is over for every awake player before it sleeps: no snapshot holds a throw
        await asyncio.gather(*(tell(p.addr, {"type": "stop", "round": r["id"]}) for p in ring if p.phase == "awake"),
                             return_exceptions=True)
        await asyncio.gather(*(p.sleep() for p in ring if p.phase == "awake"), return_exceptions=True)
        await asyncio.sleep(0.5)
        # a catch after the round ended still stepped the player; the watcher recorded it


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("cycles")
    c.add_argument("--players", type=int, default=4)
    c.add_argument("--cycles", type=int, default=200)
    c.add_argument("--full", action="store_true", help="restore with --no-dirty-tracking, so every sleep is a full snapshot")
    c.add_argument("out")
    m = sub.add_parser("memory")
    m.add_argument("--players", type=int, default=96)
    m.add_argument("--parallel", type=int, default=8)
    m.add_argument("--wakes", type=int, default=8)
    m.add_argument("out")
    l = sub.add_parser("ladder")
    l.add_argument("--sizes", type=lambda s: [int(x) for x in s.split(",")], default=[2, 4, 8, 16])
    l.add_argument("--hold", type=int, default=4000)
    l.add_argument("--budget", type=int, default=0,
                   help="start each size at hold = budget / (size - 1) ms: the time a player has to sleep and wake")
    l.add_argument("--shrink", type=int, default=100, help="per lap, in thousandths")
    l.add_argument("--min-hold", type=int, default=10)
    l.add_argument("--flight", type=int, default=250)
    l.add_argument("--margin", type=int, default=150)
    l.add_argument("--first-wake-ms", type=int, default=1500)
    l.add_argument("--round-secs", type=int, default=900)
    l.add_argument("--start-floor", type=int, default=0, help="no size starts below this hold, ms")
    l.add_argument("--parallel", type=int, default=8)
    l.add_argument("out")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    asyncio.run({"cycles": cmd_cycles, "memory": cmd_memory, "ladder": cmd_ladder}[a.cmd](a))


if __name__ == "__main__":
    main()
