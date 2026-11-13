#!/usr/bin/env python3
"""boot-race driver: time from asking for compute to the workload answering, for every lane, interleaved.

Runs on a parallel box (bare metal, /dev/kvm) in us-east-2. The fcvm lanes restore a snapshot on this host; the AWS
lanes start the same workload through the AWS APIs, with boot-race-driver credentials read from a JSON file
(BOOT_RACE_CREDS, default /dev/shm/boot-race-creds.json) that the operator refreshes.

Every lane is timed the same way: t0 is taken immediately before the call that asks for compute (Popen of
`fcvm snapshot run`, RunInstances, StartInstances, RunTask, or the HTTPS request to a function URL), and t1 when an
HTTP GET from this host, outside the VM/instance/task/function, has read a complete 200 answer from the workload.

Subcommands: run, reports (the Lambda REPORT lines), setup (setup.json for the page). Samples are appended to
<results>/samples.jsonl as they are taken.
"""

import argparse
import asyncio
import datetime
import fcntl
import http.client
import json
import os
import signal
import subprocess
import sys
import time
import urllib.parse

REGION = "us-east-2"
TAG = {"Key": "Project", "Value": "boot-race"}
LANES = [
    "fcvm-file",
    "fcvm-uffd",
    "ec2-launch",
    "ec2-start",
    "ec2-hibernate",
    "ecs-fargate",
    "ecs-ec2",
    "lambda-cold",
    "lambda-warm",
]


def now_ms():
    return time.time_ns() // 1_000_000


def load1():
    """The campaign host's 1-minute load average, recorded with every sample as quiet-box evidence."""
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def log(*a):
    print(datetime.datetime.now(datetime.timezone.utc).strftime("%H:%M:%S.%f")[:-3], *a, file=sys.stderr, flush=True)


# ------------------------------------------------------------------ AWS
def session():
    import boto3

    path = os.environ.get("BOOT_RACE_CREDS", "/dev/shm/boot-race-creds.json")
    with open(path) as f:
        c = json.load(f)
    exp = datetime.datetime.fromisoformat(c["Expiration"].replace("Z", "+00:00"))
    left = (exp - datetime.datetime.now(datetime.timezone.utc)).total_seconds()
    if left < 120:
        raise SystemExit(f"boot-race credentials expire in {left:.0f} s; refresh {path}")
    return boto3.Session(
        aws_access_key_id=c["AccessKeyId"],
        aws_secret_access_key=c["SecretAccessKey"],
        aws_session_token=c["SessionToken"],
        region_name=REGION,
    )


def tags(name):
    return [
        {"ResourceType": rt, "Tags": [TAG, {"Key": "Name", "Value": name}]}
        for rt in ("instance", "volume", "network-interface")
    ]


# ------------------------------------------------------------------ the probe
async def one_get(host, port, timeout):
    reader, writer = await asyncio.wait_for(asyncio.open_connection(host, port), timeout)
    try:
        writer.write(f"GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n".encode())
        await writer.drain()
        head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout)
        if not head.startswith(b"HTTP/1.1 200"):
            raise ConnectionError(head[:40])
        length = None
        for line in head.split(b"\r\n")[1:]:
            k, _, v = line.partition(b":")
            if k.strip().lower() == b"content-length":
                length = int(v.strip())
        if length is None:
            raise ConnectionError("answer without Content-Length")
        body = await asyncio.wait_for(reader.readexactly(length), timeout)
    finally:
        writer.close()
    doc = json.loads(body)
    if not doc.get("ok"):
        raise ConnectionError("answer without ok")
    return doc


def port_answers(host, port):
    """True when something already accepts on host:port (the endpoint is not ours to measure)."""
    import socket

    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex((host, port)) == 0


async def probe(host, port, interval, deadline_s, attempt_timeout=1.0, max_inflight=64, child=None):
    """Start a GET every `interval` seconds until one returns a complete 200; return (t1_ns, answer, attempts)."""
    done = asyncio.get_running_loop().create_future()
    inflight = set()
    attempts = 0

    async def attempt():
        try:
            doc = await one_get(host, port, attempt_timeout)
        except (OSError, asyncio.TimeoutError, ConnectionError, ValueError, asyncio.IncompleteReadError,
                asyncio.LimitOverrunError):
            return
        t1 = time.monotonic_ns()
        if not done.done():
            done.set_result((t1, doc))

    end = time.monotonic() + deadline_s
    try:
        while not done.done():
            if child is not None and child.poll() is not None:
                raise RuntimeError(f"fcvm exited with {child.returncode} before the workload answered")
            if time.monotonic() > end:
                raise TimeoutError(f"no answer from {host}:{port} in {deadline_s} s after {attempts} attempts")
            inflight = {t for t in inflight if not t.done()}
            if len(inflight) < max_inflight:
                attempts += 1
                inflight.add(asyncio.ensure_future(attempt()))
            try:
                await asyncio.wait_for(asyncio.shield(done), interval)
            except asyncio.TimeoutError:
                pass
        t1, doc = done.result()
        return t1, doc, attempts
    finally:
        for t in inflight:
            t.cancel()
        await asyncio.gather(*inflight, return_exceptions=True)


# ------------------------------------------------------------------ fcvm lanes
class Fcvm:
    def __init__(self, cfg):
        self.bin = cfg["fcvm_bin"]
        self.snapshot = cfg["fcvm_snapshot"]
        self.serve_pid = cfg.get("fcvm_serve_pid")
        self.logdir = cfg["results"] + "/fcvm-logs"
        os.makedirs(self.logdir, exist_ok=True)

    async def sample(self, lane, name, port):
        if lane == "fcvm-file":
            src = ["--snapshot", self.snapshot]
        else:
            src = ["--pid", str(self.serve_pid)]
        args = [self.bin, "snapshot", "run", *src, "--name", name, "--publish", f"127.0.0.1:{port}:8080"]
        if port_answers("127.0.0.1", port):
            raise RuntimeError(f"port {port} already answers before the restore")
        logf = open(f"{self.logdir}/{name}.log", "w")
        env = dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "fcvm=info"))
        t0 = time.monotonic_ns()
        start_ms = now_ms()
        proc = subprocess.Popen(args, stdout=logf, stderr=subprocess.STDOUT, env=env, start_new_session=True)
        try:
            t1, doc, attempts = await probe(
                "127.0.0.1", port, 0.001, 60, attempt_timeout=0.5, max_inflight=512, child=proc
            )
        finally:
            await self.stop(proc)
            logf.close()
        return {"t_start_unix_ms": start_ms, "ms": (t1 - t0) / 1e6, "attempts": attempts, "answer": doc}

    async def stop(self, proc):
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
        for _ in range(600):
            if proc.poll() is not None:
                return
            await asyncio.sleep(0.05)
        proc.kill()
        proc.wait()


# ------------------------------------------------------------------ EC2 lanes
class Ec2:
    def __init__(self, cfg):
        self.cfg = cfg
        self.ec2 = session().client("ec2")

    def ip(self, iid):
        r = self.ec2.describe_instances(InstanceIds=[iid])["Reservations"][0]["Instances"][0]
        return r["PrivateIpAddress"], r["State"]["Name"]

    def wait_state(self, iid, want, timeout=900):
        end = time.time() + timeout
        while time.time() < end:
            _, st = self.ip(iid)
            if st == want:
                return
            time.sleep(2)
        raise TimeoutError(f"instance did not reach {want}")

    async def launch_sample(self):
        t0 = time.monotonic_ns()
        start_ms = now_ms()
        r = self.ec2.run_instances(
            LaunchTemplate={"LaunchTemplateName": "boot-race-ec2"},
            ImageId=self.cfg["ami"],
            MinCount=1,
            MaxCount=1,
            HibernationOptions={"Configured": False},
            TagSpecifications=tags("boot-race-ec2-launch"),
        )
        t_api = time.monotonic_ns()
        inst = r["Instances"][0]
        iid, ip = inst["InstanceId"], inst["PrivateIpAddress"]
        try:
            t1, doc, attempts = await probe(ip, 8080, 0.02, 300)
        finally:
            self.ec2.terminate_instances(InstanceIds=[iid])
        return {
            "t_start_unix_ms": start_ms,
            "ms": (t1 - t0) / 1e6,
            "api_ms": (t_api - t0) / 1e6,
            "attempts": attempts,
            "answer": doc,
        }

    async def start_sample(self, iid, hibernate):
        await asyncio.to_thread(self.wait_state, iid, "stopped")
        ip, _ = self.ip(iid)
        t0 = time.monotonic_ns()
        start_ms = now_ms()
        try:
            self.ec2.start_instances(InstanceIds=[iid])
            t_api = time.monotonic_ns()
            t1, doc, attempts = await probe(ip, 8080, 0.02, 300)
        finally:
            # Give the workload a moment so a hibernating guest has nothing in flight, then stop it for next time.
            await asyncio.sleep(1)
            await asyncio.to_thread(self.wait_state, iid, "running")
            self.ec2.stop_instances(InstanceIds=[iid], Hibernate=hibernate)
        return {
            "t_start_unix_ms": start_ms,
            "ms": (t1 - t0) / 1e6,
            "api_ms": (t_api - t0) / 1e6,
            "attempts": attempts,
            "answer": doc,
        }


# ------------------------------------------------------------------ ECS lanes
class Launch:
    """One RunTask through boot-race-launcher, the only caller of RunTask (its code fixes the network).

    The launcher returns its wall-clock times just before and just after RunTask. A sample's t0 is the launcher's
    `before`; the probe's success is placed on the same wall clock by anchoring the monotonic clock at the invoke.
    The invoke's own cost is recorded beside the sample, not in it."""

    def __init__(self, invoked):
        self.invoke_wall_ms, self.invoke_mono_ns, self.out = invoked
        self.before_ms = self.out["before_unix_ms"]
        self.after_ms = self.out["after_unix_ms"]

    def wall_ms(self, mono_ns):
        return self.invoke_wall_ms + (mono_ns - self.invoke_mono_ns) / 1e6

    def timing(self, t1_mono_ns, returned_mono_ns):
        return {
            "t_start_unix_ms": int(self.before_ms),
            "ms": self.wall_ms(t1_mono_ns) - self.before_ms,
            "runtask_ms": self.after_ms - self.before_ms,
            "invoke_to_runtask_ms": self.before_ms - self.invoke_wall_ms,
            "runtask_to_return_ms": self.wall_ms(returned_mono_ns) - self.after_ms,
        }


class Ecs:
    def __init__(self, cfg):
        self.cfg = cfg
        s = session()
        self.ecs = s.client("ecs")
        self.lam = s.client("lambda")

    def invoke(self, payload):
        r = self.lam.invoke(FunctionName="boot-race-launcher", Payload=json.dumps(payload).encode())
        body = json.loads(r["Payload"].read() or b"null")
        if r.get("FunctionError"):
            raise RuntimeError(f"launcher error: {body}")
        return body

    def run(self, payload):
        wall, mono = time.time_ns() / 1e6, time.monotonic_ns()
        out = self.invoke({"action": "run", **payload})
        returned = time.monotonic_ns()
        if out.get("task_arn"):
            # Every task this campaign started, for prepare.py leftovers (kept on the box: ARNs are not public).
            with open(self.cfg["results"] + "/tasks.txt", "a") as f:
                f.write(out["task_arn"] + "\n")
        if out.get("failures") or not out.get("task_arn"):
            raise RuntimeError(f"RunTask failures: {out.get('failures')}")
        return Launch((wall, mono, out)), returned

    def stop(self, arn):
        self.invoke({"action": "stop", "task": arn})

    def describe(self, arn):
        """The task, or None while DescribeTasks still reports it MISSING (it can, just after RunTask)."""
        tasks = self.ecs.describe_tasks(cluster="boot-race", tasks=[arn])["tasks"]
        return tasks[0] if tasks else None

    def describe_settled(self, arn, timeout=30):
        end = time.time() + timeout
        while (t := self.describe(arn)) is None:
            if time.time() > end:
                raise TimeoutError(f"DescribeTasks reported the task missing for {timeout} s")
            time.sleep(0.5)
        return t

    def wait_stopped(self, arn, timeout=300):
        end = time.time() + timeout
        while time.time() < end:
            t = self.describe(arn)
            if t is not None and t["lastStatus"] == "STOPPED":
                return
            time.sleep(1)
        raise TimeoutError("task did not stop")

    async def fargate_sample(self):
        launch, returned = await asyncio.to_thread(self.run, {"lane": "fargate"})
        arn = launch.out["task_arn"]
        try:
            ip = None
            while ip is None:
                if time.monotonic_ns() - launch.invoke_mono_ns > 300e9:
                    raise TimeoutError("no task address in 300 s")
                t = self.describe(arn) or {}
                for a in t.get("attachments", []):
                    for d in a.get("details", []):
                        if d["name"] == "privateIPv4Address":
                            ip = d["value"]
                if ip is None:
                    await asyncio.sleep(0.2)
            t_ip = time.monotonic_ns()
            t1, doc, attempts = await probe(ip, 8080, 0.02, 300)
            t = self.describe_settled(arn)
        finally:
            await asyncio.to_thread(self.stop, arn)
        stamps = {
            k: int(t[k].timestamp() * 1000)
            for k in ("createdAt", "pullStartedAt", "pullStoppedAt", "startedAt")
            if k in t
        }
        return {
            **launch.timing(t1, returned),
            "address_ms": launch.wall_ms(t_ip) - launch.before_ms,
            "attempts": attempts,
            "aws_stamps_unix_ms": stamps,
            "platform_version": t.get("platformVersion"),
            "answer": doc,
        }

    async def ec2_sample(self, host_ip):
        if port_answers(host_ip, 8080):
            raise RuntimeError("the container instance already answers on 8080 before RunTask")
        launch, returned = await asyncio.to_thread(
            self.run, {"lane": "ec2", "instance": self.cfg["ecs_host_instance"]}
        )
        arn = launch.out["task_arn"]
        try:
            t1, doc, attempts = await probe(host_ip, 8080, 0.005, 120)
        finally:
            await asyncio.to_thread(self.stop, arn)
            await asyncio.to_thread(self.wait_stopped, arn)
        return {**launch.timing(t1, returned), "attempts": attempts, "answer": doc}


# ------------------------------------------------------------------ Lambda lanes
class Lambda:
    def __init__(self, cfg):
        self.cfg = cfg
        self.creds = session().get_credentials().get_frozen_credentials()

    def signed_get(self, url):
        from botocore.auth import SigV4Auth
        from botocore.awsrequest import AWSRequest
        from botocore.credentials import Credentials

        req = AWSRequest(method="GET", url=url, headers={"Host": urllib.parse.urlparse(url).netloc})
        SigV4Auth(Credentials(*self.creds), "lambda", REGION).add_auth(req)
        return dict(req.headers.items())

    def call(self, url):
        u = urllib.parse.urlparse(url)
        headers = self.signed_get(url)  # signing is before t0: it is the client's work, not Lambda's
        t0 = time.monotonic_ns()
        start_ms = now_ms()
        conn = http.client.HTTPSConnection(u.netloc, timeout=30)
        try:
            conn.request("GET", "/", headers=headers)
            resp = conn.getresponse()
            body = resp.read()
            t1 = time.monotonic_ns()
            rid = resp.getheader("x-amzn-RequestId")
            status = resp.status
        finally:
            conn.close()
        if status != 200:
            raise RuntimeError(f"function URL answered {status}: {body[:200]!r}")
        doc = json.loads(body)
        if not doc.get("ok"):
            raise RuntimeError("answer without ok")
        return {"t_start_unix_ms": start_ms, "ms": (t1 - t0) / 1e6, "request_id": rid, "answer": doc}

    async def sample(self, lane):
        url = self.cfg["function_urls"]["cold" if lane == "lambda-cold" else "warm"]
        return await asyncio.to_thread(self.call, url)


# ------------------------------------------------------------------ the campaign
def check_identity(lane, prev, answer, cfg):
    """Refuse a sample whose answer did not come from the process the lane claims; return the reason or None."""
    same = prev is not None and prev["proc_id"] == answer["proc_id"]
    if lane == "ec2-hibernate":
        if prev is None:
            return "no identity recorded before the hibernation"
        if not same:
            return "answered from a new process: the instance booted instead of resuming from hibernation"
    elif lane == "lambda-warm":
        if not same:
            return "answered from a new runtime process: a cold start, not a warm invocation"
    elif lane.startswith("fcvm"):
        want = cfg.get("fcvm_proc_id")
        if not want:
            return "no fcvm_proc_id in the config (fcvm-prep.sh prints it)"
        if answer["proc_id"] != want:
            return "answered from a process that is not the snapshot's workload"
    elif lane != "lambda-cold" and same:
        return "answered from the previous sample's process"
    return None


def last_answers(cfg):
    """The identity each lane last answered with: prepare-time identities, then this campaign's records."""
    prev = dict(cfg.get("identities", {}))
    path = cfg["results"] + "/samples.jsonl"
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                r = json.loads(line)
                if r.get("answer"):
                    prev[r["lane"]] = r["answer"]
    return prev


def taken_pairs(cfg):
    """(round, lane) of every attempt already recorded, failed ones included: a resumed run takes none of them again."""
    path = cfg["results"] + "/samples.jsonl"
    if not os.path.exists(path):
        return set()
    with open(path) as f:
        return {(r["round"], r["lane"]) for r in map(json.loads, filter(str.strip, f))}


def campaign_lock(cfg):
    """Hold the results directory for one driver: a second run would share the instances and interleave records."""
    f = open(cfg["results"] + "/campaign.lock", "w")
    try:
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        f.close()
        raise SystemExit(f"another race.py run holds {cfg['results']}; refusing to start a second one")
    return f


async def run(cfg, rounds, first_round, lanes):
    lock = campaign_lock(cfg)  # held until the process exits
    prev_answer = last_answers(cfg)
    taken = taken_pairs(cfg)
    out = open(cfg["results"] + "/samples.jsonl", "a")
    fcvm = Fcvm(cfg) if any(l.startswith("fcvm") for l in lanes) else None
    for rnd in range(first_round, first_round + rounds):
        k = rnd % len(lanes)
        order = [l for l in lanes[k:] + lanes[:k] if (rnd, l) not in taken]
        if len(order) < len(lanes):
            log(f"round {rnd}: {len(lanes) - len(order)} lanes already recorded, not taken again")
        ec2 = Ec2(cfg) if any(l.startswith("ec2") for l in order) else None
        ecs = Ecs(cfg) if any(l.startswith("ecs") for l in order) else None
        lam = Lambda(cfg) if any(l.startswith("lambda") for l in order) else None
        for lane in order:
            log(f"round {rnd} lane {lane}")
            rec = {"round": rnd, "lane": lane, "t_attempt_unix_ms": now_ms(), "load1": load1()}
            try:
                if lane.startswith("fcvm"):
                    s = await fcvm.sample(lane, f"br-{lane}-{rnd}", 21000 + rnd * 10 + LANES.index(lane))
                elif lane == "ec2-launch":
                    s = await ec2.launch_sample()
                elif lane == "ec2-start":
                    s = await ec2.start_sample(cfg["start_instance"], hibernate=False)
                elif lane == "ec2-hibernate":
                    s = await ec2.start_sample(cfg["hibernate_instance"], hibernate=True)
                elif lane == "ecs-fargate":
                    s = await ecs.fargate_sample()
                elif lane == "ecs-ec2":
                    s = await ecs.ec2_sample(cfg["ecs_host_ip"])
                else:
                    s = await lam.sample(lane)
                prev = prev_answer.get(lane)
                rec.update(s)
                if prev is not None:
                    rec["same_proc_as_previous"] = prev["proc_id"] == s["answer"]["proc_id"]
                    rec["same_boot_as_previous"] = prev["boot_id"] == s["answer"]["boot_id"]
                prev_answer[lane] = s["answer"]
                why = check_identity(lane, prev, s["answer"], cfg)
                if why:
                    rec.update(ok=False, error=f"IdentityCheck: {why}")
                    log(f"  {lane}: {s['ms']:.1f} ms REFUSED {why}")
                else:
                    rec["ok"] = True
                    log(f"  {lane}: {s['ms']:.1f} ms")
            except Exception as e:  # a failed sample is recorded, never dropped
                rec.update(ok=False, error=f"{type(e).__name__}: {e}", t_failed_unix_ms=now_ms())
                log(f"  {lane}: FAILED {rec['error']}")
            out.write(json.dumps(rec) + "\n")
            out.flush()


def fetch_reports(cfg):
    """REPORT lines of both functions: request id -> {duration, init_duration} (Init Duration only on a cold start)."""
    logs = session().client("logs")
    found = {}
    for k in ("cold", "warm"):
        token = None
        while True:
            kw = {"logGroupName": f"/aws/lambda/boot-race-{k}", "filterPattern": "REPORT"}
            if token:
                kw["nextToken"] = token
            r = logs.filter_log_events(**kw)
            for e in r["events"]:
                parts = dict(
                    p.strip().split(": ", 1) for p in e["message"].strip().split("\t") if ": " in p
                )
                rid = parts.get("REPORT RequestId")
                if rid:
                    found[rid] = {
                        "function": k,
                        "duration": parts.get("Duration"),
                        "init_duration": parts.get("Init Duration"),
                        "log_stream": e["logStreamName"],
                    }
            token = r.get("nextToken")
            if not token:
                break
    path = cfg["results"] + "/lambda-reports.json"
    with open(path, "w") as f:
        json.dump(found, f, indent=1)
    log(f"{len(found)} REPORT lines -> {path}")


# ------------------------------------------------------------------ setup.json
def imds(path):
    """One instance metadata value of the campaign host (IMDSv2), or None off EC2."""
    import urllib.request

    try:
        tok = urllib.request.urlopen(urllib.request.Request(
            "http://169.254.169.254/latest/api/token", method="PUT",
            headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"}), timeout=2).read().decode()
        return urllib.request.urlopen(urllib.request.Request(
            f"http://169.254.169.254/latest/meta-data/{path}",
            headers={"X-aws-ec2-metadata-token": tok}), timeout=2).read().decode()
    except OSError:
        return None


def run_out(args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def setup_rows(cfg, s):
    """The configuration every lane ran in, read from the host and from AWS (not from memory), as page rows."""
    ec2, ecs, lam = s.client("ec2"), s.client("ecs"), s.client("lambda")
    rows = [["Region", REGION]]
    rows.append(["Campaign host", f"{imds('instance-type')}, {os.cpu_count()} vCPUs, "
                 f"{imds('instance-life-cycle')}, kernel {os.uname().release}"])
    # The checkout this driver runs from is the one fcvm was built from (box-setup.sh).
    commit = run_out(["git", "-C", os.path.dirname(os.path.abspath(__file__)), "rev-parse", "HEAD"])
    rows.append(["fcvm", f"commit {commit[:12]}, {run_out([cfg['fcvm_bin'], '--version'])}"])
    rows.append(["fcvm VM", "1 vCPU, 2048 MiB, rootless networking, port published on the host's loopback; one "
                 "snapshot, restored by both fcvm lanes"])
    lt = ec2.describe_launch_template_versions(LaunchTemplateName="boot-race-ec2", Versions=["$Default"])
    lt = lt["LaunchTemplateVersions"][0]["LaunchTemplateData"]
    root = lt["BlockDeviceMappings"][0]["Ebs"]
    base = ec2.describe_images(ImageIds=[lt["ImageId"]])["Images"][0]["Name"]
    rows.append(["EC2 lanes", f"{lt['InstanceType']}, {root['VolumeType']} {root['VolumeSize']} GiB root, "
                 f"AMI made from {base} with the workload as a systemd unit"])
    host_lt = ec2.describe_launch_template_versions(LaunchTemplateName="boot-race-ecs-host", Versions=["$Default"])
    host_lt = host_lt["LaunchTemplateVersions"][0]["LaunchTemplateData"]
    host_ami = ec2.describe_images(ImageIds=[host_lt["ImageId"]])["Images"][0]["Name"]
    for fam, label in (("boot-race-fargate", "ECS on Fargate"), ("boot-race-ec2", "ECS on EC2")):
        td = ecs.describe_task_definition(taskDefinition=fam)["taskDefinition"]
        cpu = int(td["cpu"]) / 1024
        text = f"task {cpu:g} vCPU, {td['memory']} MiB, {td['runtimePlatform']['cpuArchitecture']}, " \
               f"network mode {td['networkMode']}"
        if fam == "boot-race-ec2":
            text += f"; container instance {host_lt['InstanceType']} from {host_ami}, image cached"
        else:
            text += "; no public IP, image pulled through ECR endpoints in the VPC"
        rows.append([label, text])
    for k in ("cold", "warm"):
        fc = lam.get_function_configuration(FunctionName=f"boot-race-{k}")
        rows.append([f"Lambda, {k}", f"{fc['Runtime']}, {fc['Architectures'][0]}, {fc['MemorySize']} MB, "
                     f"function URL with IAM auth, BOOT_RACE_EXIT_AFTER_RESPONSE="
                     f"{fc['Environment']['Variables'].get('BOOT_RACE_EXIT_AFTER_RESPONSE')}"])
    return rows


def write_setup(cfg):
    path = cfg["results"] + "/setup.json"
    with open(path, "w") as f:
        json.dump({"region": REGION, "rows": setup_rows(cfg, session())}, f, indent=1)
    log(f"setup -> {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--rounds", type=int, required=True)
    r.add_argument("--first-round", type=int, default=0)
    r.add_argument("--lanes", default=",".join(LANES))
    sub.add_parser("reports")
    sub.add_parser("setup")
    a = ap.parse_args()
    with open(a.config) as f:
        cfg = json.load(f)
    os.makedirs(cfg["results"], exist_ok=True)
    if a.cmd == "run":
        lanes = [l for l in a.lanes.split(",") if l]
        unknown = set(lanes) - set(LANES)
        if unknown:
            raise SystemExit(f"unknown lanes: {sorted(unknown)}")
        asyncio.run(run(cfg, a.rounds, a.first_round, lanes))
    elif a.cmd == "reports":
        fetch_reports(cfg)
    elif a.cmd == "setup":
        write_setup(cfg)


if __name__ == "__main__":
    main()
