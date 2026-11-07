"""Tests for the fleet/tools operator scripts: the /live client (fleetlive.py), aggrun.py's
stream helpers (aggstream.py) and the run scripts' wait for the coordinator (lib.sh).
No GPU, coordinator or image library: fake servers on loopback ports the kernel picks.

    make -C fleet/tools test
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "renderer", "tools"))
sys.path.insert(0, HERE)

import aggstream  # noqa: E402
import fleetlive  # noqa: E402
import pathrun  # noqa: E402


def server(handle):
    """A loopback server that runs `handle(conn)` for one connection; returns its port."""
    ls = socket.socket()
    ls.bind(("127.0.0.1", 0))
    ls.listen(1)

    def run():
        conn, _ = ls.accept()
        try:
            handle(conn)
        finally:
            conn.close()
            ls.close()

    threading.Thread(target=run, daemon=True).start()
    return ls.getsockname()[1]


def read_request(conn):
    head = b""
    while b"\r\n\r\n" not in head:
        c = conn.recv(1)
        if not c:
            break
        head += c
    return head


def free_port():
    """A port nothing listens on: bound by the kernel's choice, then released."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def in_thread(fn, timeout):
    """Run fn in a thread; returns (finished, its exception or None)."""
    out = {}

    def run():
        try:
            fn()
            out["error"] = None
        except BaseException as e:  # noqa: BLE001 - SystemExit included
            out["error"] = e

    t = threading.Thread(target=run, daemon=True)
    t.start()
    t.join(timeout)
    return not t.is_alive(), out.get("error")


class FleetLive(unittest.TestCase):
    def test_a_handshake_the_coordinator_closes_raises(self):
        # The coordinator reads the upgrade request and closes before answering it all.
        def handle(conn):
            read_request(conn)
            conn.sendall(b"HTTP/1.1 101 Switching")

        fleetlive.connect_to(f"ws://127.0.0.1:{server(handle)}/live")
        done, err = in_thread(fleetlive.Live, 5)
        self.assertTrue(done, "Live() still waits for the rest of a handshake on a closed socket")
        self.assertIsInstance(err, ConnectionError)

    def test_an_intent_whose_connection_closes_raises(self):
        # The coordinator accepts the WebSocket, reads the intent and goes away.
        def handle(conn):
            read_request(conn)
            conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                         b"Connection: Upgrade\r\n\r\n")
            conn.recv(4096)

        fleetlive.connect_to(f"ws://127.0.0.1:{server(handle)}/live")
        live = fleetlive.Live()
        self.addCleanup(live.close)
        done, err = in_thread(lambda: live.intent("relaunch", {"tile": "slot-1"}), 5)
        self.assertTrue(done, "intent() still waits for a receipt on a closed connection")
        self.assertIsInstance(err, ConnectionError)


class AggStream(unittest.TestCase):
    def test_no_listening_process_exits_with_the_reason(self):
        port = free_port()
        with self.assertRaises(SystemExit) as e:
            aggstream.require_listening_pid(port)
        self.assertIn(str(port), str(e.exception.code))

    def test_a_listening_process_of_this_user_is_found(self):
        ls = socket.socket()
        ls.bind(("127.0.0.1", 0))
        ls.listen(1)
        try:
            self.assertEqual(aggstream.require_listening_pid(ls.getsockname()[1]), os.getpid())
        finally:
            ls.close()

    def recorder(self, frames, alive):
        stop = threading.Event()
        t = threading.Thread(target=stop.wait, daemon=True)
        if alive:
            t.start()
            self.addCleanup(stop.set)
        return types.SimpleNamespace(frames=frames, t=t)

    def test_waiting_for_a_frame_ends_when_the_stream_ends(self):
        rec = self.recorder([], alive=False)
        done, err = in_thread(lambda: aggstream.wait_for_t(rec, 10.0, 60), 5)
        self.assertTrue(done, "the wait goes on after the recorder's stream ended")
        self.assertIsInstance(err, SystemExit)
        self.assertIn("ended", str(err.code))

    def test_waiting_for_a_frame_ends_at_its_deadline(self):
        rec = self.recorder([({"t": 1.0}, b"")], alive=True)
        t0 = time.time()
        done, err = in_thread(lambda: aggstream.wait_for_t(rec, 10.0, 0.5), 5)
        self.assertTrue(done, "the wait has no deadline")
        self.assertIsInstance(err, SystemExit)
        self.assertGreaterEqual(time.time() - t0, 0.5)

    def test_waiting_for_a_frame_returns_once_it_arrives(self):
        rec = self.recorder([({"t": 9.0}, b""), ({"t": 10.0}, b"")], alive=True)
        aggstream.wait_for_t(rec, 10.0, 5)


def wait_listening(port, secs, pid=""):
    return subprocess.run(
        ["bash", "-c", f'. "{HERE}/lib.sh"; wait_listening 127.0.0.1 {port} {secs} {pid}'],
        capture_output=True, text=True, timeout=30)


class RunScripts(unittest.TestCase):
    def test_wait_listening_returns_once_the_port_listens(self):
        port = free_port()
        ls = socket.socket()

        def bind_later():
            time.sleep(1.0)
            ls.bind(("127.0.0.1", port))
            ls.listen(8)

        threading.Thread(target=bind_later, daemon=True).start()
        try:
            r = wait_listening(port, 10)
            self.assertEqual(r.returncode, 0, r.stderr)
        finally:
            ls.close()

    def test_wait_listening_fails_after_its_time(self):
        r = wait_listening(free_port(), 1)
        self.assertEqual(r.returncode, 1)
        self.assertIn("nothing listens", r.stderr)

    def test_wait_listening_fails_when_the_server_exits(self):
        p = subprocess.Popen(["true"])
        p.wait()
        t0 = time.time()
        r = wait_listening(free_port(), 30, p.pid)
        self.assertEqual(r.returncode, 1)
        self.assertIn("exited", r.stderr)
        self.assertLess(time.time() - t0, 10)

    def test_the_run_scripts_wait_for_the_coordinator_instead_of_sleeping(self):
        for name in ("gpurun.sh", "aggrun.sh"):
            with open(os.path.join(HERE, name)) as f:
                text = f.read()
            lines = [line.strip() for line in text.splitlines()]
            self.assertNotIn("sleep 2", lines, f"{name} sleeps a fixed time for the coordinator")
            self.assertIn('. "$SRC/fleet/tools/lib.sh"', lines, name)
            self.assertIn("if ! wait_listening 127.0.0.1 20000 120 $CP; then", lines, name)


FAKE_AWS = r"""#!/bin/bash
# A stand-in for the AWS CLI: answers what `gpubox.sh up` asks, from FAKE_* variables, and
# logs every call to $FAKE_LOG. The box it launches is listed from then on, at FAKE_MY_TIME.
echo "$*" | tr '\n' ' ' >> "$FAKE_LOG"; echo >> "$FAKE_LOG"
args=" $* "
case "$args" in
  *" sts get-caller-identity "*) echo 123456789012 ;;
  *" ec2 describe-images "*"--image-ids "*) echo "${FAKE_IMAGE_STATE:-available}" ;;
  *" ec2 describe-images "*) case "$args" in *tag:FleetImageValidated,Values=true*) echo ami-fake ;; *) echo ami-unvalidated ;; esac ;;
  *" ec2 describe-security-groups "*) echo sg-fake ;;
  *" ec2 describe-subnets "*) [ -z "${FAKE_SLOW_SUBNETS:-}" ] || sleep 3; echo subnet-fake ;;
  *" service-quotas get-service-quota "*L-3819A6DF*) echo "$FAKE_SPOT_QUOTA" ;;
  *" service-quotas get-service-quota "*L-DB2E81BA*) echo "$FAKE_OD_QUOTA" ;;
  # One gpubox, i-fakebox, lives in FAKE_BOX_REGION: found by id or by its Purpose tag only there.
  *" ec2 describe-instances "*"--instance-ids "*|*" ec2 describe-instances "*"tag:Purpose"*)
    region=$(sed -n 's/.*--region \([^ ]*\).*/\1/p' <<<"$args")
    if [ "$region" != "${FAKE_BOX_REGION:-us-east-1}" ]; then
      case "$args" in *"--instance-ids "*) echo "An error occurred (InvalidInstanceID.NotFound)" >&2; exit 254 ;; esac
    else
      case "$args" in *"--instance-ids "*) echo fcvm-fleet-gpubox ;; *) printf 'i-fakebox\trunning\tsome one\t2026-11-07T12:00:00Z\tspot\t2026-11-07T08:00:00+00:00\n' ;; esac
    fi ;;
  *" ec2 describe-instance-type-offerings "*) printf '%s\n' ${FAKE_OFFER_AZS:-us-west-2a us-west-2c} ;;
  *" ec2 describe-instances "*)
    [ -z "${FAKE_USAGE_FAIL:-}" ] || { echo "An error occurred (RequestLimitExceeded)" >&2; exit 254; }
    # Rows are "<time> <id> <purpose> <type> <cores> <threads>"; the reply has the columns
    # the query names, and VT rows only when the query asks for VT.
    rows=$(case "$args" in *"InstanceLifecycle==\`spot\`"*) m=spot; printf '%b' "$FAKE_SPOT_IN_USE" ;; *) m=on-demand; printf '%b' "$FAKE_OD_IN_USE" ;; esac
      if [ -s "$FAKE_LOG.launched" ]; then
        # Another box launched at the same time, listed only now.
        case $m in spot) printf '%b' "${FAKE_SPOT_IN_USE_AFTER:-}" ;; *) printf '%b' "${FAKE_OD_IN_USE_AFTER:-}" ;; esac
        [ "$(cat "$FAKE_LOG.launched")" != "$m" ] || printf '%s\ti-fakebox\tfcvm-fleet-gpubox\tg4dn.metal\t48\t2\n' "$FAKE_MY_TIME"
      fi)
    vt=0; case "$args" in *"\`vt\`"*) vt=1 ;; esac
    cpu=0; case "$args" in *CpuOptions.CoreCount*) cpu=1 ;; esac
    awk -v vt=$vt -v cpu=$cpu 'BEGIN {OFS = "\t"} NF {if (!vt && $4 ~ /^vt/) next; if (cpu) print $1, $2, $3, $5, $6; else print $1, $2, $3, $4}' <<<"$rows" ;;
  *" ec2 run-instances "*)
    [ -z "${FAKE_RUN_FAIL:-}" ] || { echo launched > "$FAKE_LOG.launched"; echo "An error occurred (InternalError)" >&2; exit 254; }
    case "$args" in *MarketType=spot*) echo spot ;; *) echo on-demand ;; esac > "$FAKE_LOG.launched"
    echo i-fakebox ;;
  *" ec2 terminate-instances "*) [ -z "${FAKE_TERMINATE_FAIL:-}" ] || exit 254 ;;
  *" ec2 describe-instance-types "*)
    for t in g4dn.metal:96 g4dn.xlarge:4 g5.16xlarge:64 vt1.24xlarge:96; do
      case "$args" in *" ${t%:*} "*)
        case "$args" in *"InstanceTypes[].["*) printf '%s\t%s\n' "${t%:*}" "${t#*:}" ;; *) echo "${t#*:}" ;; esac ;; esac
    done ;;
  *" ssm get-parameter "*"/fcvm-fleet/ami-release-copies"*)
    [ -n "${FAKE_RELEASE_RECORD:-}" ] || { echo "An error occurred (ParameterNotFound)" >&2; exit 254; }
    echo "$FAKE_RELEASE_RECORD" ;;
  *" ssm get-parameter "*"/fcvm-fleet/ami-release "*)
    echo "${FAKE_RELEASE:-{\"host_amis\": {\"us-east-1\": {\"x86_64\": \"ami-00000000000000ea1\"}}}}" ;;
  *" ssm get-parameter "*) echo ami-ubuntu ;;
  *" ec2 wait "*) : ;;
  *" ssm describe-instance-information "*) echo Online ;;
  # S3 in $FAKE_S3: a key is a file, its ETag beside it; put-object --if-none-match * creates
  # it only if absent, delete-object --if-match deletes it only at that ETag.
  *" s3api put-object "*)
    key=$(sed -n 's/.*--key \([^ ]*\).*/\1/p' <<<"$*"); body=$(sed -n 's/.*--body \([^ ]*\).*/\1/p' <<<"$*")
    mkdir -p "$FAKE_S3/$(dirname "$key")"
    ( set -o noclobber; cat "$body" > "$FAKE_S3/$key" ) 2>/dev/null || { echo "An error occurred (PreconditionFailed)" >&2; exit 254; }
    etag="\"$RANDOM$RANDOM\""; echo "$etag" > "$FAKE_S3/$key.etag"; echo "$etag" ;;
  *" s3api get-object "*)
    key=$(sed -n 's/.*--key \([^ ]*\).*/\1/p' <<<"$*"); out=$(sed -n "s|.*--key [^ ]* \([^ ]*\).*|\1|p" <<<"$*")
    [ -z "${FAKE_S3_READ_FAIL:-}" ] || { echo "An error occurred (AccessDenied)" >&2; exit 254; }
    [ -e "$FAKE_S3/$key" ] || { echo "An error occurred (NoSuchKey)" >&2; exit 254; }
    cp "$FAKE_S3/$key" "$out"; cat "$FAKE_S3/$key.etag" 2>/dev/null || echo '"none"' ;;
  *" s3api head-object "*)
    key=$(sed -n 's/.*--key \([^ ]*\).*/\1/p' <<<"$*")
    [ -z "${FAKE_LOCK_STOLEN:-}" ] || { echo '"another"'; exit 0; }
    [ -e "$FAKE_S3/$key" ] || { echo "An error occurred (404)" >&2; exit 254; }
    cat "$FAKE_S3/$key.etag" ;;
  *" s3api delete-object "*)
    key=$(sed -n 's/.*--key \([^ ]*\).*/\1/p' <<<"$*"); want=$(sed -n 's/.*--if-match \([^ ]*\).*/\1/p' <<<"$*")
    [ "$(cat "$FAKE_S3/$key.etag" 2>/dev/null)" = "$want" ] || { echo "An error occurred (PreconditionFailed)" >&2; exit 254; }
    rm -f "$FAKE_S3/$key" "$FAKE_S3/$key.etag" ;;
  *" s3 cp "*"- s3://"*) [ -z "${FAKE_LEDGER_WRITE_FAIL:-}" ] || exit 1; key=$(sed -n 's|.*- s3://[^/]*/\([^ ]*\).*|\1|p' <<<"$args"); mkdir -p "$FAKE_S3/$(dirname "$key")"; cat > "$FAKE_S3/$key" ;;
  *) echo "fake aws: unexpected $*" >&2; exit 1 ;;
esac
"""


# The publisher's record of the live release's images in each region (SSM /fcvm-fleet/ami-release-copies).
RELEASE_RECORD = '{"images": {"us-east-1": {"host": "ami-00000000000000ea1"}, "us-west-2": {"host": "ami-00000000000000eb2"}}}'


def boxes(n, purpose="fcvm-fleet-gpubox", at="2026-11-05T23:00:00+00:00", type="g4dn.metal", cpu="48\\t2"):
    """The fake's rows for n instances: launch time, id, purpose, type, cores, threads."""
    return "".join(f"{at}\\ti-{type}-{k}\\t{purpose}\\t{type}\\t{cpu}\\n" for k in range(n))


class GpuboxWallReserve(unittest.TestCase):
    """`gpubox.sh up` leaves the wall's GPU host room in the account's G quotas (#1257)."""

    def up(self, spot_quota=192, spot_in_use="", od_quota=384, od_in_use="", s3=None, cmd=None, **extra):
        import tempfile
        d = tempfile.mkdtemp()
        s3 = s3 or os.path.join(d, "s3")
        with open(os.path.join(d, "aws"), "w") as f:
            f.write(FAKE_AWS)
        os.chmod(os.path.join(d, "aws"), 0o755)
        log = os.path.join(d, "calls")
        env = dict(os.environ, PATH=f"{d}:{os.environ['PATH']}", FAKE_LOG=log, GPUBOX_USE_ENV="1",
                   FAKE_S3=s3, FAKE_SPOT_QUOTA=str(spot_quota), FAKE_OD_QUOTA=str(od_quota),
                   FAKE_SPOT_IN_USE=spot_in_use, FAKE_OD_IN_USE=od_in_use,
                   FAKE_MY_TIME="2026-11-06T00:00:00+00:00", **{"GPUBOX_REGION": "us-east-1", "REGION": "us-east-1", "GPUBOX_AZS": "a", "AZS": "a",
                      "FAKE_RELEASE_RECORD": RELEASE_RECORD, **extra})
        env = {k: v for k, v in env.items() if v is not None}
        cmd = cmd or ["bash", os.path.join(HERE, "gpubox.sh"), "up", "--no-wait"]
        r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
        with open(log) as f:
            calls = f.read().splitlines()
        r.log, r.calls = log, calls
        self.calls = calls
        def named(word):
            return [c for c in calls if f" {word} " in f" {c} "]
        return r, named("run-instances"), named("terminate-instances")

    def test_a_full_quota_launches_nothing(self):
        # 2026-11-05: gpuboxes held the 384 on-demand vCPUs and the spot quota had no room
        # for a box and the wall's host: the wall's host waited 13 minutes.
        r, launches, _ = self.up(spot_in_use=boxes(1), od_in_use=boxes(3))
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)
        self.assertIn("wall", r.stderr)

    def test_spot_goes_first_when_it_leaves_the_wall_room(self):
        r, launches, ends = self.up(od_in_use=boxes(3))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(launches), 1, launches)
        self.assertIn("MarketType=spot", launches[0])
        self.assertEqual(ends, [])

    def test_on_demand_only_with_room_for_the_wall_host_and_its_tiles(self):
        # 96 for the box, 96 for the wall's host and 32 for its on-demand tiles.
        r, launches, _ = self.up(spot_in_use=boxes(1), od_in_use=boxes(1))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(launches), 1, launches)
        self.assertNotIn("MarketType=spot", launches[0])
        r, launches, _ = self.up(spot_in_use=boxes(1), od_in_use=boxes(2))
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [])

    def test_unreadable_usage_launches_nothing(self):
        r, launches, _ = self.up(FAKE_USAGE_FAIL="1")
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)
        self.assertIn("could not be read", r.stderr)
        self.assertNotIn("unexpected", r.stderr)

    def test_two_ups_at_once_admit_one_box_into_the_room(self):
        # Spot has room for one box and the wall's host, on demand for none, and neither
        # listing ever shows the other's box (describe-instances is eventually consistent).
        import tempfile
        from concurrent.futures import ThreadPoolExecutor
        s3 = tempfile.mkdtemp()
        with ThreadPoolExecutor(2) as ex:
            runs = list(ex.map(lambda _: self.up(od_in_use=boxes(3), s3=s3), range(2)))
        self.assertEqual(sorted(r.returncode == 0 for r, _, _ in runs), [False, True], [r.stderr for r, _, _ in runs])
        self.assertEqual(sum(len(launches) for _, launches, _ in runs), 1)
        self.assertFalse(os.path.exists(os.path.join(s3, "gpubox", "admission.lock")), "the lock was left behind")

    def test_a_lock_broken_before_the_launch_launches_nothing(self):
        r, launches, _ = self.up(od_in_use=boxes(3), FAKE_LOCK_STOLEN="1")
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)
        self.assertIn("the admission lock was broken", r.stderr)

    def test_a_lock_held_past_half_its_stale_age_launches_nothing(self):
        # A holder whose quota and subnet calls took long could otherwise pass the fence just
        # before another up breaks its lock as stale.
        r, launches, _ = self.up(od_in_use=boxes(3), FAKE_SLOW_SUBNETS="1", GPUBOX_ADMIT_STALE_S="4")
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)

    def test_an_unreadable_lock_or_ledger_launches_nothing(self):
        import tempfile
        # A lock is held and cannot be read; then no lock and a ledger that cannot be read.
        for held in (True, False):
            s3 = tempfile.mkdtemp()
            os.makedirs(os.path.join(s3, "gpubox"))
            if held:
                with open(os.path.join(s3, "gpubox", "admission.lock"), "w") as f:
                    f.write(f"{int(time.time())} someone\n")
            r, launches, _ = self.up(od_in_use=boxes(3), s3=s3, FAKE_S3_READ_FAIL="1", GPUBOX_ADMIT_WAIT_S="4")
            self.assertNotEqual(r.returncode, 0, r.stderr)
            self.assertEqual(launches, [], r.stderr)

    def test_every_admitted_launch_carries_the_admission_tag(self):
        # ejc3/aws denies the fleet's Terraform role a G RunInstances without it, so a launcher
        # that skips the reserve (an old checkout's gpubox.sh, 2026-11-07) is refused by EC2.
        r, launches, _ = self.up(od_in_use=boxes(3))
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        self.assertIn("{Key=fcvm-admission,Value=wall-reserve}", launches[0])

    def ami_launch(self, kind, **kw):
        return self.up(cmd=["bash", os.path.join(HERE, "..", "ami", "build-ami.sh"), "launch", kind], **kw)

    def test_an_ami_builder_goes_through_the_wall_reserve(self):
        # A host builder (96 vCPU) on demand at 192 of 384 in use would leave the wall 96 of
        # its 128: refused. A tile builder (4) fits, tagged, and its ledger entry says 4.
        import tempfile
        r, launches, _ = self.ami_launch("host", od_in_use=boxes(2))
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)
        self.assertIn("g4dn.metal builder", r.stderr)
        s3 = tempfile.mkdtemp()
        r, launches, _ = self.ami_launch("tile", od_in_use=boxes(2), s3=s3)
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        self.assertIn("{Key=fcvm-admission,Value=wall-reserve}", launches[0])
        with open(os.path.join(s3, "gpubox", "admitted")) as f:
            self.assertEqual(f.read().split()[2:], ["on-demand", "4"])

    def test_a_boot_measurement_goes_through_the_wall_reserve(self):
        # measure-boot.sh host launches a g4dn.metal on demand: at 192 of 384 it would leave
        # the wall 96 of its 128.
        r, launches, _ = self.up(od_in_use=boxes(2),
                                 cmd=["bash", os.path.join(HERE, "..", "ami", "measure-boot.sh"), "host", "ami-x"])
        self.assertNotEqual(r.returncode, 0, r.stderr)
        self.assertEqual(launches, [], r.stderr)
        self.assertIn("g4dn.metal to measure", r.stderr)

    def test_every_fleet_launcher_goes_through_g_admission(self):
        # Every script under fleet/ that calls run-instances sources g-admission.sh, checks
        # room_for and tags the launch: a launcher that does not is the 2026-11-07 outage.
        import glob, re
        fleet = os.path.join(HERE, "..")
        launchers = [f for f in glob.glob(os.path.join(fleet, "**", "*.sh"), recursive=True)
                     if re.search(r"^[^#]*\bec2 run-instances\b", open(f).read(), re.M)]
        self.assertEqual(sorted(os.path.basename(f) for f in launchers),
                         ["build-ami.sh", "gpubox.sh", "measure-boot.sh"])
        for f in launchers:
            src = open(f).read()
            with self.subTest(f=os.path.basename(f)):
                self.assertTrue(re.search(r'(?m)^\s*\. ".*/g-admission\.sh"$', src), "does not source g-admission.sh")
                self.assertIn("room_for ", src)
                self.assertIn("admit_held", src)
                self.assertIn("ledger_add ", src)
                self.assertIn("admit_abandon", src)
                self.assertIn("$ADMISSION_TAG]", src)

    def test_usage_counts_each_type_s_default_vcpus(self):
        # The quota charges a type's default vCPUs whatever the CPU options: one g4dn.metal and
        # two g5.16xlarge at 16 CPUs each hold 96 + 64 + 64 = 224, and 96 more + 128 is 448.
        # Counted from CpuOptions they were 128, and a box was admitted.
        r, launches, _ = self.up(spot_in_use=boxes(1),
                                 od_in_use=boxes(1) + boxes(2, type="g5.16xlarge", cpu="8\\t2"))
        self.assertEqual(launches, [], r.stderr)

    def test_vt_instances_count_against_the_g_quota(self):
        # VT shares the G quotas: two vt1.24xlarge hold 192 of 384 on demand.
        r, launches, _ = self.up(spot_in_use=boxes(1), od_in_use=boxes(2, type="vt1.24xlarge"))
        self.assertEqual(launches, [], r.stderr)

    def test_an_ami_builder_resolves_its_ami_before_taking_the_lock(self):
        # Every call that can stall runs before the lock: none between admit_held and the launch.
        r, launches, _ = self.ami_launch("tile")
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        at = lambda word: next(i for i, c in enumerate(self.calls) if word in c)
        self.assertLess(at("ssm get-parameter"), at("s3api put-object"))
        self.assertEqual(at("s3api head-object") + 1, at("ec2 run-instances"), self.calls)

    def test_a_launch_in_no_ledger_keeps_the_lock(self):
        # A launch that may exist and is in no ledger leaves the lock to go stale, so the next
        # launcher waits until describe-instances lists it; one that was terminated releases it.
        import tempfile
        cases = [("gpubox", dict(FAKE_RUN_FAIL="1"), True),
                 ("gpubox", dict(FAKE_LEDGER_WRITE_FAIL="1", FAKE_TERMINATE_FAIL="1"), True),
                 ("gpubox", dict(FAKE_LEDGER_WRITE_FAIL="1"), False),
                 ("builder", dict(FAKE_RUN_FAIL="1"), True),
                 ("builder", dict(FAKE_LEDGER_WRITE_FAIL="1", FAKE_TERMINATE_FAIL="1"), True)]
        for who, knobs, kept in cases:
            with self.subTest(who=who, knobs=knobs):
                s3 = tempfile.mkdtemp()
                if who == "gpubox":
                    r, _, _ = self.up(od_in_use=boxes(1), s3=s3, **knobs)
                else:
                    r, _, _ = self.ami_launch("tile", s3=s3, **knobs)
                self.assertNotEqual(r.returncode, 0, r.stderr)
                self.assertEqual(os.path.exists(os.path.join(s3, "gpubox", "admission.lock")), kept, r.stderr)

    def test_a_ledger_entry_counts_its_own_vcpus(self):
        # An admission a moment ago, not listed yet. 96 listed + 4 + this box's 96 + the wall's
        # 128 = 324 of 384: admitted. The same entry at 96 makes it 416: refused.
        import tempfile
        for vcpus, admitted in ((4, 1), (96, 0)):
            s3 = tempfile.mkdtemp()
            os.makedirs(os.path.join(s3, "gpubox"))
            with open(os.path.join(s3, "gpubox", "admitted"), "w") as f:
                f.write(f"{int(time.time())} i-other on-demand {vcpus}\n")
            r, launches, _ = self.up(spot_in_use=boxes(1), od_in_use=boxes(1), s3=s3)
            self.assertEqual(len(launches), admitted, (vcpus, r.stderr))
            for c in launches:
                self.assertNotIn("MarketType=spot", c)

    def gpubox(self, *args, **kw):
        return self.up(cmd=["bash", os.path.join(HERE, "gpubox.sh"), *args], **kw)

    def test_a_box_in_us_west_2_never_consults_the_staging_reserve(self):
        # Both markets full by the staging region's reserve; us-west-2 has no wall to leave room for.
        r, launches, _ = self.up(spot_in_use=boxes(2), od_in_use=boxes(4), GPUBOX_REGION="us-west-2")
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        self.assertEqual(r.stdout.split(), ["i-fakebox", "us-west-2"])
        self.assertIn("{Key=fcvm-admission,Value=outside-staging}", launches[0])
        for call in r.calls:
            self.assertNotIn("service-quotas", call)
            self.assertNotIn("gpubox/admission.lock", call)
            self.assertNotIn("gpubox/admitted", call)

    def test_the_region_is_used_in_every_aws_call(self):
        for region in ("us-west-2", "us-east-1"):
            with self.subTest(region=region):
                r, launches, _ = self.up(od_in_use=boxes(3), GPUBOX_REGION=region)
                self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
                # STS is global, the asset bucket's S3 has its own region (us-east-2), and the release
                # record is one parameter in us-east-1.
                regional = [c for c in r.calls if not c.startswith("sts ") and " s3 " not in f" {c} " and " s3api " not in f" {c} "
                            and not c.startswith("--region us-east-1 ssm get-parameter --name /fcvm-fleet/ami-release")]
                self.assertTrue(any(" run-instances " in c for c in regional))
                for call in regional:
                    self.assertTrue(call.startswith(f"--region {region} "), call)
                self.assertTrue(any(f"Values=fcvm-fleet-{region}a" in c for c in r.calls), r.calls)

    def test_agents_default_to_us_west_2_and_its_g4dn_metal_zones(self):
        r, launches, _ = self.up(GPUBOX_REGION=None, GPUBOX_AZS=None, FAKE_OFFER_AZS="us-west-2a us-west-2c")
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        self.assertTrue(launches[0].startswith("--region us-west-2 "), launches[0])
        subnets = [c for c in r.calls if " describe-subnets " in c]
        self.assertEqual(len(subnets), 1, "spot in the first zone with g4dn.metal")
        self.assertIn("Values=fcvm-fleet-us-west-2a", subnets[0])

    def test_down_finds_a_box_in_either_region_and_nowhere_else(self):
        for home in ("us-west-2", "us-east-1"):
            with self.subTest(home=home):
                r, _, ends = self.gpubox("down", "i-fakebox", FAKE_BOX_REGION=home, GPUBOX_REGION=None)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertEqual(len(ends), 1, r.calls)
                self.assertTrue(ends[0].startswith(f"--region {home} "), ends)
                r, _, ends = self.gpubox("down", "i-fakebox", home, FAKE_BOX_REGION=home, GPUBOX_REGION=None)
                self.assertEqual((r.returncode, len(ends)), (0, 1), r.stderr)
                other = "us-east-1" if home == "us-west-2" else "us-west-2"
                r, _, ends = self.gpubox("down", "i-fakebox", other, FAKE_BOX_REGION=home, GPUBOX_REGION=None)
                self.assertNotEqual(r.returncode, 0)
                self.assertEqual(ends, [])
        r, _, ends = self.gpubox("down", "i-fakebox", FAKE_BOX_REGION="us-east-2", GPUBOX_REGION=None)
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(ends, [])

    def test_list_shows_both_regions(self):
        for home in ("us-west-2", "us-east-1"):
            r, _, _ = self.gpubox("list", FAKE_BOX_REGION=home, GPUBOX_REGION=None)
            self.assertEqual(r.returncode, 0, r.stderr)
            rows = [line.split("\t") for line in r.stdout.splitlines()]
            self.assertEqual(rows, [["i-fakebox", home, "running", "some one", "2026-11-07T12:00:00Z", "spot",
                                     "2026-11-07T08:00:00+00:00"]], r.stdout)

    def test_launch_errors_go_to_a_file_mktemp_made(self):
        # A PID-named file in /tmp can be pre-created or a symlink planted there by another user.
        src = open(os.path.join(HERE, "gpubox.sh")).read()
        self.assertNotRegex(src, r"/tmp/[A-Za-z0-9._-]*\$\$")
        r, launches, _ = self.up(od_in_use=boxes(3))
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)

    def test_nightly_dogfood_pins_gpuboxes_to_us_west_2_and_downs_them_there(self):
        # A session that inherits GPUBOX_REGION=us-east-1 would take the staging quota.
        run = open(os.path.join(HERE, "..", "..", ".claude", "skills", "nightly-dogfood", "run.sh")).read()
        pin = run.find("export GPUBOX_REGION=us-west-2")
        self.assertGreater(pin, 0, "run.sh does not pin the gpubox region")
        self.assertLess(pin, run.index('claude -p "/nightly-dogfood"'), "the pin comes after the session starts")
        self.assertIn('bash fleet/tools/gpubox.sh down "$a" ${b:+"$b"}', run)

    def test_ami_launchers_in_us_west_2_never_consult_the_staging_reserve(self):
        # The image factory's and the boot measurements' G instances run in us-west-2 by default.
        for cmd, word in ((["bash", os.path.join(HERE, "..", "ami", "build-ami.sh"), "launch", "host"], "build"),
                          (["bash", os.path.join(HERE, "..", "ami", "measure-boot.sh"), "host", "ami-x"], "measure")):
            with self.subTest(launcher=word):
                r, launches, _ = self.up(od_in_use=boxes(4), spot_in_use=boxes(2), cmd=cmd, REGION=None, AZS=None,
                                         FAKE_OFFER_AZS="us-west-2a", FAKE_USAGE_FAIL="1")
                self.assertEqual(len(launches), 1, r.stderr)
                self.assertTrue(launches[0].startswith("--region us-west-2 "), launches[0])
                self.assertIn("{Key=fcvm-admission,Value=outside-staging}", launches[0])
                for call in r.calls:
                    self.assertNotIn("service-quotas", call)
                    self.assertNotIn("gpubox/admission.lock", call)
                    self.assertNotIn("gpubox/admitted", call)

    def test_up_launches_the_live_releases_image_in_its_region(self):
        record = json.dumps({"images": {"us-east-1": {"host": "ami-" + "1" * 17}, "us-west-2": {"host": "ami-" + "2" * 17}}})
        for region, want in (("us-west-2", "ami-" + "2" * 17), ("us-east-1", "ami-" + "1" * 17)):
            with self.subTest(region=region):
                r, launches, _ = self.up(od_in_use=boxes(3), GPUBOX_REGION=region, FAKE_RELEASE_RECORD=record)
                self.assertEqual(len(launches), 1, r.stderr)
                self.assertIn(f"--image-id {want} ", launches[0])
        # No record: the release itself, which names us-east-1 only. Never the newest image by name
        # or tag, which can be a candidate or a validated image whose publication failed.
        r, launches, _ = self.up(od_in_use=boxes(3), GPUBOX_REGION="us-east-1", FAKE_RELEASE_RECORD=None)
        self.assertEqual(len(launches), 1, r.stderr)
        self.assertIn("--image-id ami-00000000000000ea1 ", launches[0])
        for extra in ({"FAKE_RELEASE_RECORD": None}, {"FAKE_RELEASE_RECORD": record, "FAKE_IMAGE_STATE": "pending"}):
            with self.subTest(extra=sorted(extra)):
                r, launches, _ = self.up(od_in_use=boxes(3), GPUBOX_REGION="us-west-2", **extra)
                self.assertNotEqual(r.returncode, 0)
                self.assertEqual(launches, [], r.stderr)
                self.assertIn("names no available host image", r.stderr)
                self.assertFalse(any(" describe-images " in c and "--image-ids" not in c for c in r.calls), r.calls)

    def test_a_held_lock_is_broken_once_stale(self):
        import tempfile
        s3 = tempfile.mkdtemp()
        os.makedirs(os.path.join(s3, "gpubox"))
        with open(os.path.join(s3, "gpubox", "admission.lock"), "w") as f:
            f.write(f"{int(time.time()) - 400} gone\n")
        with open(os.path.join(s3, "gpubox", "admission.lock.etag"), "w") as f:
            f.write('"stale"\n')
        r, launches, _ = self.up(od_in_use=boxes(3), s3=s3)
        self.assertEqual((r.returncode, len(launches)), (0, 1), r.stderr)
        self.assertIn("breaking an admission lock", r.stderr)


if __name__ == "__main__":
    unittest.main()


class PathrunLighting(unittest.TestCase):
    def test_a_late_pass_posts_the_latest_time_of_day_not_the_next(self):
        # The director's time of day moves every second while the camera flies; pathrun
        # polls every 2 s, so a pass must catch up to the value due now, not the one after
        # the last it posted, or the lighting falls behind the camera for the whole run.
        tods = [(0.0, 0.71), (11.0, 0.7106), (12.0, 0.7112), (13.0, 0.7118), (30.0, 0.735)]
        v, i = pathrun.due(tods, 1, 13.5)
        self.assertEqual((v, i), (0.7118, 4))
        self.assertEqual(pathrun.due(tods, i, 14.0), (None, 4))
