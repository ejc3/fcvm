#!/usr/bin/env python3
"""Matched-basis per-instance memory: fcvm clones vs host containers, same corpus.

Both sides are measured through the SAME two bases, over the SAME process set
definition, which is the error report.py's header records as having refuted the
first density claim (fcvm side summed PSS over firecracker processes only while
the container side measured a whole cgroup):

  cgroup   memory.current of the one cgroup that contains EVERY process of one
           instance -- on the fcvm side a leaf cgroup the launcher joins before
           exec'ing fcvm (so fcvm, firecracker, the namespace holder and pasta
           are all inside it); on the container side podman's own container
           cgroup.
  pss      PSS summed over exactly that cgroup's process set.

A third, attribution-free basis is recorded beside them: MemAvailable measured
on a quiesced box before the instances exist and again while they are held, so a
reader can check the attributed numbers against a machine-level delta.

Every instance renders one corpus page before it is sampled, so what is measured
is an instance that has done the work, not one that has merely booted.  The
sampling itself is report.py's `sample` subcommand, unmodified.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import fcntl
import hashlib
import json
import math
import os
import random
import re
import secrets
import shutil
import signal
import socket
import stat
import statistics
import subprocess
import sys
import time
import uuid

from reqbench import snapshot_generation, valid_snapshot_name

HERE = os.path.dirname(os.path.abspath(__file__))
REPORT = os.path.join(HERE, "report.py")
CDPDRIVE = os.path.join(HERE, "cdpdrive.py")
PHASE_SUPERVISOR = os.path.join(HERE, "phase_supervisor.py")


def log(msg):
    print(f"{time.strftime('%H:%M:%S')} {msg}", file=sys.stderr, flush=True)


def die(msg, code=2):
    log("BLOCKED: " + msg)
    sys.exit(code)


def sh(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def loadavg1():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def wait_quiet(limit, timeout_s):
    """Refuse a contaminated box; wait out a decaying average up to timeout_s."""
    deadline = time.monotonic() + timeout_s
    while True:
        la = loadavg1()
        if la < limit:
            return la
        if time.monotonic() >= deadline:
            die(f"1-min load {la} did not fall below {limit} within {timeout_s}s")
        log(f"settling: load={la}, need < {limit}")
        time.sleep(10)


class CloneStateReadError(RuntimeError):
    """The state directory cannot prove whether an owned clone is gone."""


def sh_bounded(cmd, timeout):
    """Run cmd, and turn a hang into a failed attempt rather than a hang.

    A loop that bounds itself with a deadline evaluates that deadline only
    between attempts, so one unbounded `podman exec` against a wedged container
    holds the harness there forever and the diagnostic the deadline exists to
    produce is never reached. An attempt that times out is a failed attempt,
    which is what these loops already handle.
    """
    try:
        return sh(cmd, timeout=timeout)
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(cmd, 124, "", f"timed out after {timeout}s")


def parse_csv(raw, option):
    """Parse a comma-separated option without silently dropping cells."""
    values = [value.strip() for value in raw.split(",")]
    if not raw or any(not value for value in values):
        die(f"{option} must not be empty or contain empty members")
    if len(set(values)) != len(values):
        die(f"{option} contains duplicates")
    return values


def canonical_image_id(raw):
    """Return the image identity shape snapshot provenance records."""
    digest = raw[7:] if raw.startswith("sha256:") else raw
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        die(f"podman returned invalid image ID {raw!r}")
    return "sha256:" + digest


def exact_container_id(raw):
    """Return one full podman container ID, never a prefix or display token."""
    if not re.fullmatch(r"[0-9a-f]{64}", raw or ""):
        raise RuntimeError(f"podman returned invalid exact container ID {raw!r}")
    return raw


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


MEMORY_COMPLETION_ARTIFACTS = ("run.json", "samples.jsonl", "summary.json")


def read_memory_artifact(directory_fd, name, *, capture=False):
    """Read one final memory artifact without following or changing its entry."""
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(name, flags, dir_fd=directory_fd)
    except OSError as exc:
        raise RuntimeError(f"cannot open final memory artifact {name}: {exc}") from exc
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode):
            raise RuntimeError(f"final memory artifact {name} is not a regular file")
        digest = hashlib.sha256()
        chunks = [] if capture else None
        size = 0
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            if chunks is not None:
                chunks.append(chunk)
            digest.update(chunk)
            size += len(chunk)
        after = os.fstat(fd)
    finally:
        os.close(fd)
    identity_before = (
        before.st_dev, before.st_ino, before.st_size,
        before.st_mtime_ns, before.st_ctime_ns,
    )
    identity_after = (
        after.st_dev, after.st_ino, after.st_size,
        after.st_mtime_ns, after.st_ctime_ns,
    )
    if identity_before != identity_after or size != after.st_size:
        raise RuntimeError(f"final memory artifact {name} changed while being read")
    return (
        {"path": name, "size": size, "sha256": digest.hexdigest()},
        b"".join(chunks) if chunks is not None else None,
    )


def publish_completion(results, run_id):
    """Atomically commit the three closed files that define a memory run."""
    directory_flags = (
        os.O_RDONLY
        | getattr(os, "O_DIRECTORY", 0)
        | getattr(os, "O_CLOEXEC", 0)
    )
    try:
        directory_fd = os.open(results, directory_flags)
    except OSError as exc:
        raise RuntimeError(
            f"cannot open memory result directory {results}: {exc}"
        ) from exc
    directory_stat = os.fstat(directory_fd)
    temporary = f".complete.{os.getpid()}.{secrets.token_hex(8)}"
    target = "complete.json"
    temporary_exists = False
    linked = False
    written_stat = None

    def ensure_directory():
        try:
            current = os.stat(results)
        except OSError as exc:
            raise RuntimeError(
                f"memory result directory {results} cannot be rechecked: {exc}"
            ) from exc
        if not os.path.samestat(current, directory_stat):
            raise RuntimeError(
                f"memory result directory {results} changed before completion"
            )

    def read_all():
        rows = []
        raw = {}
        for name in MEMORY_COMPLETION_ARTIFACTS:
            capture = name in {"run.json", "summary.json"}
            identity, payload = read_memory_artifact(
                directory_fd, name, capture=capture
            )
            rows.append(identity)
            if capture:
                raw[name] = payload
        return rows, raw

    try:
        ensure_directory()
        artifacts, raw = read_all()
        for name in ("run.json", "summary.json"):
            try:
                record = json.loads(raw[name])
            except (TypeError, ValueError) as exc:
                raise RuntimeError(f"final memory artifact {name} is not JSON") from exc
            if not isinstance(record, dict) or record.get("run_id") != run_id:
                raise RuntimeError(
                    f"final memory artifact {name} does not name run_id {run_id}"
                )
        record = {
            "schema_version": 1,
            "run_id": run_id,
            "artifacts": artifacts,
        }
        flags = (
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0)
        )
        fd = os.open(temporary, flags, 0o600, dir_fd=directory_fd)
        temporary_exists = True
        try:
            handle = os.fdopen(fd, "w")
        except BaseException:
            os.close(fd)
            raise
        with handle:
            json.dump(record, handle, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
            written_stat = os.fstat(handle.fileno())

        ensure_directory()
        current_artifacts, _raw = read_all()
        if current_artifacts != artifacts:
            raise RuntimeError("final memory artifacts changed before completion")
        try:
            os.link(
                temporary,
                target,
                src_dir_fd=directory_fd,
                dst_dir_fd=directory_fd,
                follow_symlinks=False,
            )
        except FileExistsError as exc:
            raise RuntimeError(
                f"memory completion {os.path.join(results, target)} already exists"
            ) from exc
        linked = True
        os.unlink(temporary, dir_fd=directory_fd)
        temporary_exists = False
        published = os.stat(target, dir_fd=directory_fd, follow_symlinks=False)
        if not os.path.samestat(published, written_stat):
            raise RuntimeError("memory completion changed during publication")
        ensure_directory()
        os.fsync(directory_fd)
    except BaseException:
        if linked:
            try:
                published = os.stat(
                    target, dir_fd=directory_fd, follow_symlinks=False
                )
                if written_stat is not None and os.path.samestat(
                        published, written_stat):
                    os.unlink(target, dir_fd=directory_fd)
            except FileNotFoundError:
                pass
        if temporary_exists:
            try:
                os.unlink(temporary, dir_fd=directory_fd)
            except FileNotFoundError:
                pass
        try:
            os.fsync(directory_fd)
        except OSError:
            pass
        raise
    finally:
        os.close(directory_fd)


def validate_args(args):
    """Refuse an empty or ambiguous measurement grid before creating output."""
    if not args.urls or any(not isinstance(url, str) or not url for url in args.urls):
        die("--urls must name at least one nonempty URL")
    if not args.ns or any(not isinstance(n, int) or isinstance(n, bool) or n <= 0
                          for n in args.ns):
        die("--ns must name one or more positive integers")
    if len(set(args.ns)) != len(args.ns):
        die("--ns contains duplicate cell sizes")
    if not isinstance(args.reps, int) or isinstance(args.reps, bool) or args.reps <= 0:
        die("--reps must be a positive integer")
    if args.reps % len(args.urls) != 0:
        die("--reps must be a whole number of corpus cycles so every N sees "
            "the same balanced page workload")
    timings = (args.settle, args.quiet_limit, args.quiet_wait)
    if any(not isinstance(value, (int, float)) or isinstance(value, bool)
           or not math.isfinite(value) or value < 0 for value in timings):
        die("settle and quiet-box values must be finite and nonnegative")
    if not re.fullmatch(r"[0-9a-f]{32}", args.run_id or ""):
        die("--run-id must be a 32-character lowercase hexadecimal owner ID")
    if not re.fullmatch(r"[0-9a-f]{32}", args.container_owner_token or ""):
        die("--container-owner-token must be a 32-character lowercase hexadecimal token")
    if not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})",
                        args.source_revision or ""):
        die("--source-revision must be a 40- or 64-character lowercase commit ID")
    for option, value in (
            ("--runtime-bundle-sha256", args.runtime_bundle_sha256),
            ("--corpus-extra-runtime-bundle-sha256",
             args.corpus_extra_runtime_bundle_sha256)):
        if not re.fullmatch(r"[0-9a-f]{64}", value or ""):
            die(f"{option} must be a lowercase sha256")


def claim_results_dir(path):
    """Claim a new result directory so records from two runs cannot mix."""
    try:
        os.makedirs(path)
    except FileExistsError:
        die(f"results directory {path} already exists; refusing to reuse prior output")
    except OSError as exc:
        die(f"cannot create results directory {path}: {exc}")
    try:
        os.mkdir(os.path.join(path, "logs"))
    except OSError as exc:
        die(f"cannot create the owned log directory under {path}: {exc}")


def validate_snapshot_for_benchmark(generation, image, image_id, expected_dns,
                                    fcvm_sha256, runtime_bundle_sha256,
                                    source_revision):
    """Bind the snapshot tag to the image bytes and replay resolver in use."""
    if generation.get("image") != image:
        die(f"snapshot image {generation.get('image')!r} does not match {image!r}")
    if generation.get("image_id") != image_id:
        die(f"snapshot image ID {generation.get('image_id')!r} does not match "
            f"current {image} ID {image_id!r}")
    if (generation.get("guest_dns") != expected_dns
            or generation.get("dns_server") != expected_dns):
        die(f"snapshot did not bake replay DNS {expected_dns}: "
            f"guest_dns={generation.get('guest_dns')!r} "
            f"dns_server={generation.get('dns_server')!r}")
    if generation.get("guest_env") != []:
        die(f"snapshot has unexpected baked container environment: "
            f"{generation.get('guest_env')!r}")
    expected_creator = {
        "creator_fcvm_sha256": fcvm_sha256,
        "creator_runtime_bundle_sha256": runtime_bundle_sha256,
        "source_revision": source_revision,
    }
    for field, expected in expected_creator.items():
        if generation.get(field) != expected:
            die(f"snapshot {field} {generation.get(field)!r} does not match "
                f"current staged runtime {expected!r}")


def snapshot_generation_under_lease(resources, data_root, tag):
    """Read one snapshot generation and keep its shared lock until stack exit."""
    if not valid_snapshot_name(tag):
        raise RuntimeError(
            "snapshot tag must be 1..128 ASCII letters, digits, '-', '_', or '.', "
            "excluding . and .."
        )
    lock_path = os.path.join(data_root, "snapshots", f"{tag}.lock")
    try:
        lock_file = resources.enter_context(open(lock_path, "a+"))
        fcntl.flock(lock_file, fcntl.LOCK_SH)
    except OSError as exc:
        raise RuntimeError(
            f"cannot hold snapshot generation lock {lock_path}: {exc}"
        ) from exc
    return snapshot_generation(data_root, tag)


_LISTENER_OWNER_PROBE = r"""
import os
import sys

port = int(sys.argv[1])
inodes = set()
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        rows = open(table).read().splitlines()[1:]
    except OSError:
        continue
    for row in rows:
        fields = row.split()
        if len(fields) > 9 and fields[3] == "0A":
            try:
                local_port = int(fields[1].rsplit(":", 1)[1], 16)
            except (IndexError, ValueError):
                continue
            if local_port == port:
                inodes.add(fields[9])
if not inodes:
    raise SystemExit(1)
for pid in os.listdir("/proc"):
    if not pid.isdigit():
        continue
    try:
        fds = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        continue
    for fd in fds:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{fd}")
        except OSError:
            continue
        if target.startswith("socket:[") and target[8:-1] in inodes:
            raise SystemExit(0)
raise SystemExit(1)
"""


def container_owns_tcp_listener(name, port):
    """Prove a host-network listener belongs to this container's PID namespace."""
    result = sh_bounded(
        ["podman", "exec", name, "python3", "-c", _LISTENER_OWNER_PROBE, str(port)],
        30)
    return result.returncode == 0


def install_signal_cleanup():
    """Turn termination into SystemExit so main's finally block runs."""
    def terminate(signum, _frame):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)


def port_open(host, port, timeout=0.5):
    try:
        with socket.create_connection((host, port), timeout):
            return True
    except OSError:
        return False


# --------------------------------------------------------------------------
# sampling (report.py, unmodified, is the only thing that reads memory)
# --------------------------------------------------------------------------
def sample(extra: dict, cgroup_root=None, cgroup_prefix=None, podman_prefix=None,
           state_dir=None, name_prefix=None):
    cmd = [sys.executable, REPORT, "sample"]
    if cgroup_root:
        cmd += ["--cgroup-root", cgroup_root, "--cgroup-prefix", cgroup_prefix]
    if podman_prefix:
        cmd += ["--podman-prefix", podman_prefix]
    if state_dir:
        # The REFUTED basis, recorded on purpose: PSS over the firecracker
        # processes alone. It is never the headline; it sits in the record so a
        # reader can see how far off the old accounting was from the same run.
        cmd += ["--state-dir", state_dir, "--name-prefix", name_prefix]
    cmd += ["--extra", ", ".join(f'"{k}": {json.dumps(v)}' for k, v in extra.items())]
    r = sh(cmd)
    if r.returncode != 0:
        die(f"report.py sample failed: {r.stderr.strip()}")
    return json.loads(r.stdout)


def stray_vmm_processes():
    """fcvm/firecracker processes already on the box, or BLOCKED if unknowable.

    The refusal this feeds exists because a leftover VMM is charged to whatever
    this run measures. `pgrep ... || true` reported the same empty string for a
    clean box and for every way pgrep can fail to answer: 127 (not installed),
    2 (bad pattern), 3 (fatal error). Only exit 1 is "no match"; anything else
    means the box was never checked, and a check that did not run cannot clear
    it.
    """
    try:
        r = sh(["pgrep", "-a", "fcvm|firecracker"], timeout=30)
    except (OSError, subprocess.SubprocessError) as exc:
        die(f"cannot run pgrep to check for stray fcvm/firecracker processes: {exc}")
    if r.returncode == 1:
        return ""
    if r.returncode != 0:
        die(f"pgrep exited {r.returncode} looking for stray fcvm/firecracker "
            f"processes, so this box was never cleared: {r.stderr.strip()}")
    return r.stdout.strip()


def quiesce(seconds=4):
    subprocess.run(["sync"], check=False)
    time.sleep(seconds)


# --------------------------------------------------------------------------
# fcvm side
# --------------------------------------------------------------------------
class CgroupSet:
    """One slice with the memory controller delegated, one leaf per instance."""

    def __init__(self, base):
        self.base = base
        self.created = False

    def setup(self):
        if os.path.exists(self.base):
            die(f"cgroup {self.base} already exists; this run does not own it")
        made = sh_bounded(["sudo", "-n", "mkdir", self.base], 30)
        if made.returncode != 0:
            die(f"cannot create {self.base}; per-instance cgroup accounting is the measurement")
        self.created = True
        r = sh_bounded(
            ["sudo", "-n", "sh", "-c",
             f"echo '+memory' > {self.base}/cgroup.subtree_control"], 30)
        if r.returncode != 0:
            die(f"cannot delegate +memory to {self.base}: {r.stderr.strip()}")

    def leaf(self, name):
        path = f"{self.base}/{name}"
        if not self.created:
            die(f"cannot create leaf before owning cgroup {self.base}")
        if os.path.exists(path):
            die(f"leaf cgroup {path} already exists; this run does not own it")
        if sh_bounded(["sudo", "-n", "mkdir", path], 30).returncode != 0:
            die(f"cannot create leaf cgroup {path}")
        return path

    def rm(self, name):
        path = f"{self.base}/{name}"
        result = sh_bounded(["sudo", "-n", "rmdir", path], 30)
        if result.returncode != 0 and os.path.isdir(path):
            raise RuntimeError(
                f"cannot remove cgroup {path}: {result.stderr.strip()}")

    def rm_all(self):
        if not self.created:
            return
        try:
            names = sorted(os.listdir(self.base)) if os.path.isdir(self.base) else []
        except OSError as exc:
            raise RuntimeError(f"cannot enumerate owned cgroup {self.base}: {exc}") from exc
        errors = []
        for name in names:
            if os.path.isdir(os.path.join(self.base, name)):
                try:
                    self.rm(name)
                except RuntimeError as exc:
                    errors.append(exc)
        result = sh_bounded(["sudo", "-n", "rmdir", self.base], 30)
        if result.returncode != 0 and os.path.isdir(self.base):
            errors.append(RuntimeError(
                f"cannot remove cgroup {self.base}: {result.stderr.strip()}"))
        else:
            self.created = False
        if errors:
            raise errors[0]


def spawn_in_cgroup(cg_path, argv, log_path, env=None):
    """Launch argv with the launching shell moved into cg_path first.

    cgroup membership is inherited across fork and survives the user-namespace
    and uid transitions fcvm makes, so every descendant lands in the same leaf.
    """
    script = f'sudo -n sh -c "echo $BASHPID > {cg_path}/cgroup.procs" || exit 90; exec "$0" "$@"'
    handle = open(log_path, "wb")
    return subprocess.Popen(["bash", "-c", script] + list(argv),
                            stdout=handle, stderr=subprocess.STDOUT,
                            env=env or os.environ.copy())


def state_files(state_dir):
    try:
        return [os.path.join(state_dir, f) for f in os.listdir(state_dir) if f.endswith(".json")]
    except OSError as exc:
        raise CloneStateReadError(
            f"cannot enumerate clone state directory {state_dir}: {exc}"
        ) from exc


def find_clone_state(state_dir, name, deadline, proc):
    """The clone's own state record, once it carries a pid and a host-side IP."""
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return None
        for path in state_files(state_dir):
            try:
                with open(path) as f:
                    st = json.load(f)
            except (OSError, ValueError):
                continue
            if st.get("name") != name or not st.get("pid"):
                continue
            net = (st.get("config") or {}).get("network") or {}
            for key in ("loopback_ip", "host_ip", "guest_ip"):
                if net.get(key):
                    return st, net[key]
        time.sleep(0.05)
    return None


def clone_gone(state_dir, name):
    for path in state_files(state_dir):
        try:
            with open(path) as f:
                state = json.load(f)
        except (OSError, ValueError) as exc:
            raise CloneStateReadError(
                f"cannot read clone state {path} while proving {name} is gone: {exc}"
            ) from exc
        if not isinstance(state, dict):
            raise CloneStateReadError(
                f"clone state {path} is not an object while proving {name} is gone"
            )
        if state.get("name") == name:
            return False
    return True


def render(endpoint, url, timeout=120):
    r = sh([sys.executable, CDPDRIVE, endpoint, url, "--format", "jpeg"], timeout=timeout)
    return r.returncode == 0, (r.stdout or r.stderr)[-400:]


class FcvmSide:
    name = "fcvm-clone"

    def __init__(self, args, cg: CgroupSet, run_id):
        self.args = args
        self.cg = cg
        self.run_id = run_id
        self.serve_proc = None
        self.serve_pid = None
        self.owned = {}

    def start_serve(self):
        cg = self.cg.leaf("serve-0")
        log_path = os.path.join(self.args.results, "logs", "serve.log")
        argv = [self.args.fcvm, "snapshot", "serve", self.args.tag,
                "--uffd-mode", self.args.uffd_mode,
                "--uffd-prefetch", self.args.uffd_prefetch]
        self.serve_proc = spawn_in_cgroup(cg, argv, log_path)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if self.serve_proc.poll() is not None:
                die(f"snapshot serve exited {self.serve_proc.returncode}; see {log_path}")
            try:
                text = open(log_path, errors="replace").read()
            except OSError:
                text = ""
            m = re.search(r"Serve PID: (\d+)", text)
            if m and "Waiting for VMs" in text:
                self.serve_pid = int(m.group(1))
                log(f"serve up: pid={self.serve_pid} ({self.args.uffd_mode}, prefetch={self.args.uffd_prefetch})")
                return
            time.sleep(0.25)
        die(f"snapshot serve never announced a pid; see {log_path}")

    def stop_serve(self):
        if not self.serve_proc:
            return
        self.serve_proc.terminate()
        try:
            self.serve_proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.serve_proc.kill()
            self.serve_proc.wait(timeout=30)
        self.serve_proc = None
        self.cg.rm("serve-0")

    def bring_up(self, n, cell_tag, url_indices):
        """n clones, each restored, each having rendered one corpus page."""
        live = []
        for i in range(n):
            leaf = f"req-{cell_tag}-{i}"
            cgp = self.cg.leaf(leaf)
            name = f"mem-{self.run_id}-{cell_tag}-{i}"
            log_path = os.path.join(self.args.results, "logs", f"{name}.log")
            argv = [self.args.fcvm, "snapshot", "run", "--pid", str(self.serve_pid),
                    "--name", name, "--no-dirty-tracking", "--no-swap"]
            env = dict(os.environ, RUST_LOG="fcvm=info")
            proc = spawn_in_cgroup(cgp, argv, log_path, env)
            clone = {"i": i, "leaf": leaf, "name": name,
                     "proc": proc, "log": log_path}
            self.owned[name] = clone
            live.append(clone)
        # wait for every clone's published CDP port, then render one page on each
        for c in live:
            found = find_clone_state(self.args.state_dir, c["name"], time.monotonic() + 180, c["proc"])
            if not found:
                die(f"clone {c['name']} never published a usable state file; see {c['log']}")
            st, ip = found
            c["vm_id"] = st.get("vm_id")
            c["endpoint"] = f"{ip}:{self.args.cdp_port}"
            deadline = time.monotonic() + 180
            while not port_open(ip, self.args.cdp_port):
                if time.monotonic() >= deadline:
                    die(f"clone {c['name']} never answered on {c['endpoint']}; see {c['log']}")
                time.sleep(0.05)
        for c in live:
            url = self.args.urls[url_indices[c["i"]]]
            ok, out = render(c["endpoint"], url)
            if not ok:
                die(f"clone {c['name']} failed to render {url}: {out}")
            c["url"] = url
        return live

    def tear_down(self, live):
        errors = []
        for c in live:
            try:
                c["proc"].terminate()
            except ProcessLookupError:
                pass
        for c in live:
            clean = True
            try:
                c["proc"].wait(timeout=120)
            except subprocess.TimeoutExpired:
                try:
                    c["proc"].kill()
                    c["proc"].wait(timeout=60)
                except BaseException as exc:
                    errors.append(RuntimeError(
                        f"cannot stop clone {c['name']}: {type(exc).__name__}: {exc}"))
                    clean = False
            except BaseException as exc:
                errors.append(RuntimeError(
                    f"cannot reap clone {c['name']}: {type(exc).__name__}: {exc}"))
                clean = False
            deadline = time.monotonic() + 120
            try:
                while not clone_gone(self.args.state_dir, c["name"]):
                    if time.monotonic() >= deadline:
                        errors.append(RuntimeError(
                            f"clone {c['name']} state file outlived its process; "
                            "a later cell would be contaminated"))
                        clean = False
                        break
                    time.sleep(0.2)
            except CloneStateReadError as exc:
                errors.append(exc)
                clean = False
            try:
                self.cg.rm(c["leaf"])
            except BaseException as exc:
                errors.append(RuntimeError(
                    f"cannot remove clone {c['name']} cgroup: "
                    f"{type(exc).__name__}: {exc}"))
                clean = False
            if clean:
                self.owned.pop(c["name"], None)
        if errors:
            raise errors[0]

    def stop_all(self):
        if self.owned:
            self.tear_down(list(self.owned.values()))

    def sample(self, extra, cell_tag):
        """The clones, plus the UFFD serve process measured on the same bases.

        The serve is SHARED by every clone, so it is never summed into the
        per-instance total; it is recorded beside it as the fixed cost of the
        arrangement, on the same two bases, and the fit's intercept is where a
        reader can see it again from the other direction."""
        rec = sample(extra, cgroup_root=self.cg.base, cgroup_prefix=f"req-{cell_tag}-",
                     state_dir=self.args.state_dir,
                     name_prefix=f"mem-{self.run_id}-{cell_tag}-")
        serve = sample({"_": 0}, cgroup_root=self.cg.base, cgroup_prefix="serve-")
        rec["serve_cgroup_kb"] = serve.get("clone_cgroup_kb", 0)
        rec["serve_pss_kb"] = serve.get("clone_pss_kb", 0)
        rec["serve_procs"] = serve.get("clone_procs", 0)
        return rec


# --------------------------------------------------------------------------
# container side
# --------------------------------------------------------------------------
# Every container keeps the image's DEFAULT ports (CDP 9222, pageserver 8000) in
# a network namespace of its own, and the host reaches its CDP through a
# published port. The obvious alternative, one shared host network namespace
# with BENCH_CDP_PORT per container, cannot work with this image: entry.sh warms
# Chromium with `render.py ... ` and render.py's --cdp-host defaults to
# 127.0.0.1:9222, so a container told to listen anywhere else fails its own
# warmup and never writes /run/bench-ready ("CDP discovery failed: Connection
# refused", 2026-08-30 18:04).
#
# slirp4netns:allow_host_loopback=true puts the host's loopback at 10.0.2.2
# inside the namespace, which is the same address the guest VM reaches the
# replay server on through pasta, so BENCH_RESOLVE_ALL_TO is the same value on
# both sides of the comparison.
#
# The render is then driven INSIDE the container with the image's own render.py,
# because Chromium binds the namespace's loopback and ignores
# --remote-debugging-address, so a published port reaches an address nothing is
# listening on (cdpdrive: 130 resolve attempts, no page target, 2026-08-30
# 18:10). What the memory measurement needs is that the instance rendered the
# page; where the driver runs does not change the instance's footprint, and the
# exec'd python has exited before anything is sampled.
CONTAINER_NET = "slirp4netns:allow_host_loopback=true"
CONTAINER_RESOLVE_TO = "10.0.2.2"
CONTAINER_OWNER_LABEL = "io.fcvm.bench.owner"
CREATE_OPERATION_TIMEOUT = 120
CREATE_QUIESCE_TIMEOUT = 180
CREATE_TERM_TIMEOUT = 5.0
CREATE_KILL_TIMEOUT = 30.0


class ContainerCreateOperation:
    """A podman create whose shared lock survives a caller-side timeout."""

    def __init__(self, command, lock_dir, operation_name,
                 supervisor_term_grace=CREATE_TERM_TIMEOUT,
                 supervisor_kill_reap=CREATE_KILL_TIMEOUT,
                 command_timeout=CREATE_OPERATION_TIMEOUT):
        if not os.path.isdir(lock_dir):
            raise RuntimeError(
                f"container create lease directory does not exist: {lock_dir}")
        lock_path = os.path.join(lock_dir, operation_name + ".lock")
        self.lock_path = lock_path
        try:
            self.lock_fd = os.open(
                lock_path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
        except OSError as exc:
            raise RuntimeError(f"cannot claim container create lease {lock_path}: {exc}") from exc
        try:
            fcntl.flock(self.lock_fd, fcntl.LOCK_SH)
            supervised_command = [
                sys.executable, PHASE_SUPERVISOR,
                "--expected-parent", str(os.getpid()),
                "--timeout", str(command_timeout),
                "--term-grace", str(supervisor_term_grace),
                "--kill-grace", str(supervisor_kill_reap),
                "--pass-fd", str(self.lock_fd), "--", *command,
            ]
            self.process = subprocess.Popen(
                supervised_command,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, pass_fds=(self.lock_fd,), start_new_session=True)
        except BaseException:
            os.close(self.lock_fd)
            self.lock_fd = None
            raise
        self.command = command
        self.supervisor_shutdown_timeout = (
            supervisor_term_grace + supervisor_kill_reap + 5.0)
        self.complete = None
        self.reconcile_fd = None

    def finish(self, timeout):
        if self.complete is not None:
            return self.complete
        try:
            stdout, stderr = self.process.communicate(
                timeout=timeout + self.supervisor_shutdown_timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                stdout, stderr = self.process.communicate(
                    timeout=self.supervisor_shutdown_timeout)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    stdout, stderr = self.process.communicate(
                        timeout=CREATE_KILL_TIMEOUT)
                except subprocess.TimeoutExpired as exc:
                    raise RuntimeError(
                        f"cannot kill and reap container create operation "
                        f"{self.process.pid}") from exc
        self.complete = subprocess.CompletedProcess(
            self.command, self.process.returncode, stdout, stderr)
        return self.complete

    def acquire_reconciliation(self, timeout):
        """Wait for every inherited shared holder, then retain an exclusive lock."""
        if self.reconcile_fd is not None:
            return
        if self.process.poll() is None:
            raise RuntimeError(
                f"container create operation {self.process.pid} is still running")
        if self.lock_fd is not None:
            os.close(self.lock_fd)
            self.lock_fd = None
        reconcile_fd = os.open(self.lock_path, os.O_RDWR | os.O_CLOEXEC)
        helper = subprocess.Popen(
            [sys.executable, "-c",
             "import fcntl,os,sys; "
             "fcntl.flock(int(sys.argv[1]), fcntl.LOCK_EX); "
             "os.write(1, b'acquired')",
             str(reconcile_fd)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            pass_fds=(reconcile_fd,), start_new_session=True,
        )
        try:
            stdout, stderr = helper.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            try:
                os.killpg(helper.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            helper.communicate(timeout=CREATE_KILL_TIMEOUT)
            os.close(reconcile_fd)
            raise RuntimeError(
                f"container create operation {self.process.pid} left inherited "
                f"lease holders after {timeout}s") from exc
        if helper.returncode != 0 or stdout != b"acquired":
            os.close(reconcile_fd)
            detail = stderr.decode(errors="replace").strip()
            raise RuntimeError(
                f"cannot acquire exclusive create reconciliation lease: {detail}")
        self.reconcile_fd = reconcile_fd

    def release(self):
        """Release the create lease only after name/owner reconciliation."""
        if self.process.poll() is None:
            raise RuntimeError(
                f"container create operation {self.process.pid} is still running")
        if self.reconcile_fd is None:
            raise RuntimeError("container create lease was not reconciled")
        os.close(self.reconcile_fd)
        self.reconcile_fd = None


def start_container_create(command, lock_dir, operation_name, timeout):
    """Start a create and retain its completion identity after a timeout."""
    if lock_dir is None:
        return None, sh_bounded(command, timeout)
    operation = ContainerCreateOperation(
        command, lock_dir, operation_name, command_timeout=timeout)
    return operation, None


def inspected_container_identity(name):
    """Return (exact ID, benchmark owner token), None if the name is absent."""
    inspected = sh_bounded(
        ["podman", "inspect", "--format",
         f'{{{{.Id}}}} {{{{ index .Config.Labels "{CONTAINER_OWNER_LABEL}" }}}}',
         name], 30)
    if inspected.returncode != 0:
        exists = sh_bounded(["podman", "container", "exists", name], 30)
        if exists.returncode == 1:
            return None
        raise RuntimeError(
            f"cannot identify container {name}: {inspected.stderr.strip() or exists.stderr.strip()}"
        )
    fields = inspected.stdout.split()
    if len(fields) != 2:
        raise RuntimeError(
            f"podman returned no exact ID and owner label for {name}: "
            f"{inspected.stdout.strip()!r}"
        )
    return exact_container_id(fields[0]), fields[1]


def remove_owned_container(name, owner_token, expected_id=None):
    """Remove only the exact container this invocation can prove it created."""
    identity = inspected_container_identity(name)
    if identity is None:
        return
    container_id, actual_owner = identity
    if actual_owner != owner_token:
        raise RuntimeError(
            f"same-name container {name} ({container_id}) is not owned by this run")
    if expected_id is not None and container_id != exact_container_id(expected_id):
        raise RuntimeError(
            f"owned container {name} changed identity from {expected_id} to {container_id}")
    removed = sh_bounded(["podman", "rm", "-f", "--", container_id], 30)
    if removed.returncode != 0:
        raise RuntimeError(
            f"cannot remove owned container {name} ({container_id}): "
            f"{removed.stderr.strip()}"
        )
    exists = sh_bounded(["podman", "container", "exists", container_id], 30)
    if exists.returncode == 0:
        raise RuntimeError(f"owned container {name} ({container_id}) survived podman rm")
    if exists.returncode != 1:
        raise RuntimeError(
            f"cannot verify removal of owned container {name} ({container_id}): "
            f"{exists.stderr.strip()}"
        )


class ContainerSide:
    name = "host-container"

    def __init__(self, args, run_id):
        self.args = args
        self.run_id = run_id
        self.owned = set()
        self.owned_ids = {}
        self.create_operations = {}

    def prefix(self, cell_tag):
        return f"cbmem-{self.run_id}-{cell_tag}-"

    def bring_up(self, n, cell_tag, url_indices):
        live = []
        for i in range(n):
            name = f"{self.prefix(cell_tag)}{i}"
            self.owned.add(name)
            command = ["podman", "create", "--name", name,
                       "--label", f"{CONTAINER_OWNER_LABEL}={self.args.container_owner_token}",
                       "--network", CONTAINER_NET,
                       "-e", f"BENCH_RESOLVE_ALL_TO={CONTAINER_RESOLVE_TO}",
                       getattr(self.args, "image_id", self.args.image)]
            operation, r = start_container_create(
                command, getattr(self.args, "container_create_ops_dir", None),
                name, CREATE_OPERATION_TIMEOUT)
            if operation is not None:
                self.create_operations[name] = operation
                if r is None:
                    r = operation.finish(CREATE_OPERATION_TIMEOUT)
            if r is None:
                die(f"podman create {name} did not complete within "
                    f"{CREATE_OPERATION_TIMEOUT}s")
            if r.returncode != 0:
                die(f"podman create {name} failed: {r.stderr.strip()}")
            try:
                if operation is not None:
                    operation.acquire_reconciliation(CREATE_QUIESCE_TIMEOUT)
                container_id = exact_container_id(r.stdout.strip())
                identity = inspected_container_identity(name)
                if identity is None:
                    raise RuntimeError(
                        f"podman create returned {container_id}, but {name} is absent")
                actual_id, owner = identity
                if owner != self.args.container_owner_token:
                    raise RuntimeError(
                        f"created container {name} ({actual_id}) has owner {owner!r}")
                if actual_id != container_id:
                    raise RuntimeError(
                        f"created container {name} changed identity from "
                        f"{container_id} to {actual_id}")
            except RuntimeError as exc:
                die(str(exc))
            if operation is not None:
                operation.release()
                self.create_operations.pop(name, None)
            self.owned_ids[name] = container_id
            started = sh_bounded(
                ["podman", "start", "--", container_id],
                CREATE_OPERATION_TIMEOUT)
            if started.returncode != 0:
                die(f"podman start {name} ({container_id}) failed: "
                    f"{started.stderr.strip()}")
            live.append({"i": i, "name": name, "container_id": container_id})
        for c in live:
            deadline = time.monotonic() + 180
            while sh_bounded(["podman", "exec", c["name"], "test", "-f",
                              "/run/bench-ready"], 30).returncode != 0:
                if time.monotonic() >= deadline:
                    logs = sh_bounded(
                        ["podman", "logs", "--tail", "20", c["name"]], 30).stdout
                    die(f"container {c['name']} never became ready: {logs}")
                time.sleep(0.25)
        for c in live:
            url = self.args.urls[url_indices[c["i"]]]
            r = sh(["podman", "exec", c["name"], "python3", "/opt/bench/render.py", url,
                    "--out-prefix", "/tmp/mem", "--format", "jpeg"], timeout=180)
            if r.returncode != 0:
                die(f"container {c['name']} failed to render {url}: "
                    f"{(r.stdout + r.stderr)[-400:]}")
            removed = sh_bounded(
                ["podman", "exec", c["name"], "rm", "-f",
                 "/tmp/mem.jpeg", "/tmp/mem.dom.html"], 30)
            if removed.returncode != 0:
                die(f"container {c['name']} could not discard render outputs: "
                    f"{removed.stderr.strip()}")
            c["url"] = url
        return live

    def tear_down(self, live):
        errors = []
        for c in live:
            name = c["name"]
            try:
                operation = self.create_operations.get(name)
                if operation is not None:
                    result = operation.finish(CREATE_QUIESCE_TIMEOUT)
                    operation.acquire_reconciliation(CREATE_QUIESCE_TIMEOUT)
                    if result.returncode == 0:
                        try:
                            output_id = exact_container_id(result.stdout.strip())
                        except RuntimeError:
                            output_id = None
                        if output_id is not None:
                            recorded_id = self.owned_ids.get(name)
                            if recorded_id is not None and recorded_id != output_id:
                                raise RuntimeError(
                                    f"container create for {name} returned "
                                    f"{output_id}, expected {recorded_id}")
                            self.owned_ids[name] = output_id
                remove_owned_container(
                    name, self.args.container_owner_token,
                    c.get("container_id", self.owned_ids.get(name)))
                if operation is not None:
                    operation.release()
                    self.create_operations.pop(name, None)
            except RuntimeError as exc:
                errors.append(exc)
                continue
            self.owned.discard(name)
            self.owned_ids.pop(name, None)
        if errors:
            raise errors[0]

    def stop_all(self):
        if self.owned:
            self.tear_down([
                {"name": name, "container_id": self.owned_ids.get(name)}
                for name in sorted(self.owned)
            ])

    def sample(self, extra, cell_tag):
        return sample(extra, podman_prefix=self.prefix(cell_tag))


# --------------------------------------------------------------------------
def cleanup_harness_resources(container_side, fcvm_side, cg, out):
    """Attempt every independent cleanup even when an earlier one fails."""
    first_error = None
    cleanups = (
        ("host containers", container_side.stop_all if container_side else None),
        ("fcvm clones", fcvm_side.stop_all if fcvm_side else None),
        ("fcvm serve", fcvm_side.stop_serve if fcvm_side else None),
        ("cgroups", cg.rm_all),
        ("sample output", out.close),
    )
    for label, cleanup in cleanups:
        if cleanup is None:
            continue
        try:
            cleanup()
        except BaseException as exc:  # cleanup after SystemExit must still continue
            if first_error is None:
                first_error = exc
            else:
                log(f"cleanup of {label} also failed: {type(exc).__name__}: {exc}")
    if first_error is not None:
        raise first_error


def slope_intercept(xs, ys):
    """Least squares over the concrete N grid: marginal cost and fixed cost."""
    n = len(xs)
    if n < 2:
        return None, None
    mx, my = statistics.mean(xs), statistics.mean(ys)
    denom = sum((x - mx) ** 2 for x in xs)
    if denom == 0:
        return None, None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / denom
    return slope, my - slope * mx


def build_cell_schedule(sides, ns, reps, seed, url_count):
    """Pair sides while every instance at every N sees the same page history."""
    rng = random.Random(seed)
    url_cycle = list(range(url_count))
    rng.shuffle(url_cycle)
    pairs = [(n, rep) for n in ns for rep in range(1, reps + 1)]
    rng.shuffle(pairs)
    schedule = []
    for n, rep in pairs:
        url_index = url_cycle[(rep - 1) % url_count]
        url_indices = (url_index,) * n
        pair_sides = list(sides)
        rng.shuffle(pair_sides)
        schedule.extend((side, n, rep, url_indices) for side in pair_sides)
    return schedule


def empty_bases(s, side):
    """Which of this steady sample's bases came back as nothing.

    Every basis is a sum over the processes of live instances that have each
    rendered a page, so once the instance count is satisfied none of them can be
    zero. A zero is a sample that could not see the process set. report.py's
    single-node cgroup.procs read produced exactly that on the container side:
    pool_containers counted from `podman ps` was right while pool_procs and
    pool_pss_kb were 0, and cell_values reads both with `.get(key, 0)`, so the
    zero reached summary.json and the least-squares fit as a number.

    A missing key is treated as a zero because `.get(key, 0)` downstream cannot
    tell them apart either.
    """
    keys = (("clone_procs", "clone_cgroup_kb", "clone_pss_kb")
            if side == "fcvm-clone" else
            ("pool_procs", "pool_cgroup_kb", "pool_pss_kb"))
    return [k for k in keys if not s.get(k)]


def run_cell(side, args, n, rep, url_indices, out):
    cell_tag = f"{side.name.split('-')[0]}{n}r{rep}"
    common = {"side": side.name, "n": n, "rep": rep, "run_id": args.run_id,
              "snapshot": args.tag if side.name == "fcvm-clone" else None,
              "image": args.image, "uffd_mode": args.uffd_mode if side.name == "fcvm-clone" else None,
              "uffd_prefetch": args.uffd_prefetch if side.name == "fcvm-clone" else None,
              "url_indices": list(url_indices)}
    quiesce()
    pre = side.sample(dict(common, phase="pre"), cell_tag)
    out.write(json.dumps(pre) + "\n"); out.flush()
    log(f"{side.name} n={n} rep={rep}: bringing up")
    live = side.bring_up(n, cell_tag, url_indices)
    time.sleep(args.settle)
    steady = []
    for k in range(3):
        rec = side.sample(dict(common, phase="steady", sample=k,
                               instances_expected=n,
                               mem_available_pre_kb=pre["mem_available_kb"]), cell_tag)
        out.write(json.dumps(rec) + "\n"); out.flush()
        steady.append(rec)
        time.sleep(1)
    log(f"{side.name} n={n} rep={rep}: "
        + " ".join(f"{k}={steady[1].get(k)}" for k in
                   ("clones", "clone_cgroup_kb", "clone_pss_kb", "pool_containers",
                    "pool_cgroup_kb", "pool_pss_kb") if k in steady[1]))
    # Fail closed on a cell that lost an instance. Per-instance memory divides
    # by the n that was ASKED for, so a cell measured with fewer live instances
    # than that would silently report a smaller number per instance.
    counted = steady[1].get("clones" if side.name == "fcvm-clone" else "pool_containers")
    if counted != n:
        die(f"{side.name} n={n} rep={rep}: {counted} instance(s) were accounted, not {n}; "
            "the per-instance figure from this cell would be wrong")
    empty = empty_bases(steady[1], side.name)
    if empty:
        die(f"{side.name} n={n} rep={rep}: {', '.join(empty)} came back zero with "
            f"{counted} instance(s) accounted; a zero basis is a sample that could "
            "not see the process set, not a measurement of one")
    side.tear_down(live)
    quiesce()
    post = side.sample(dict(common, phase="post"), cell_tag)
    out.write(json.dumps(post) + "\n"); out.flush()
    return {"side": side.name, "n": n, "rep": rep, "pre": pre, "steady": steady, "post": post,
            "urls": [c["url"] for c in live]}


def cell_values(cell):
    """The middle steady sample, on each basis, as MiB totals."""
    s = cell["steady"][1]
    if cell["side"] == "fcvm-clone":
        counted = s.get("clones", 0)
        cg = s.get("clone_cgroup_kb", 0) / 1024
        pss = s.get("clone_pss_kb", 0) / 1024
    else:
        counted = s.get("pool_containers", 0)
        cg = s.get("pool_cgroup_kb", 0) / 1024
        pss = s.get("pool_pss_kb", 0) / 1024
    avail_delta = (cell["pre"]["mem_available_kb"] - s["mem_available_kb"]) / 1024
    out = {"instances_counted": counted, "cgroup_mib": cg, "pss_mib": pss,
           "mem_available_delta_mib": avail_delta}
    if "fc_only_pss_kb" in s:
        out["refuted_fc_only_pss_mib"] = s["fc_only_pss_kb"] / 1024
    if "serve_pss_kb" in s:
        out["serve_pss_mib"] = s["serve_pss_kb"] / 1024
        out["serve_cgroup_mib"] = s.get("serve_cgroup_kb", 0) / 1024
    return out


def main():
    """Run one measurement while releasing every whole-run lease on exit."""
    with ExitStack() as resources:
        return main_with_resources(resources)


def main_with_resources(resources):
    p = argparse.ArgumentParser()
    p.add_argument("--results", required=True)
    p.add_argument("--tag", required=True, help="golden snapshot the clones restore from")
    p.add_argument("--image", default="localhost/chromium-bench-req")
    p.add_argument("--image-id", default="",
                   help="exact preflight image identity; the logical tag remains in provenance")
    p.add_argument("--urls", required=True, help="comma-separated corpus")
    p.add_argument("--ns", default="1,2,4,8")
    p.add_argument("--reps", type=int, default=14)
    p.add_argument("--seed", type=int, default=20260830,
                   help="recorded seed for the interleaved memory-cell schedule")
    p.add_argument("--fcvm", default=os.path.join(os.path.dirname(os.path.dirname(HERE)), "target/release/fcvm"))
    p.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    p.add_argument("--cdp-port", type=int, default=9222)
    p.add_argument("--container-create-ops-dir", default="")
    p.add_argument("--uffd-mode", default="minor")
    p.add_argument("--uffd-prefetch", default="on")
    p.add_argument("--settle", type=float, default=5.0)
    p.add_argument("--quiet-limit", type=float, default=1.0)
    p.add_argument("--quiet-wait", type=float, default=300.0)
    p.add_argument("--run-id", default="",
                   help="owner ID shared with the outer cleanup; defaults to a UUID")
    p.add_argument("--container-owner-token", default="",
                   help="32-hex token proving which containers this invocation created")
    p.add_argument("--source-revision", required=True)
    p.add_argument("--runtime-bundle-sha256", required=True)
    p.add_argument("--corpus-extra-runtime-bundle-sha256", required=True)
    args = p.parse_args()

    args.urls = parse_csv(args.urls, "--urls")
    try:
        args.ns = [int(x) for x in parse_csv(args.ns, "--ns")]
    except ValueError as exc:
        die(f"--ns must be comma-separated integers: {exc}")
    args.state_dir = os.path.join(args.data_root, "state")
    args.run_id = args.run_id or uuid.uuid4().hex
    args.container_owner_token = args.container_owner_token or uuid.uuid4().hex
    args.container_create_ops_dir = (
        args.container_create_ops_dir
        or os.path.join(args.results, "container-create-ops")
    )
    validate_args(args)
    install_signal_cleanup()

    for tool in ("podman", "sudo", "bash"):
        if not shutil.which(tool):
            die(f"'{tool}' is missing; this harness cannot render a verdict without it")
    if not os.access(args.fcvm, os.X_OK):
        die(f"no fcvm binary at {args.fcvm}")
    try:
        fcvm_sha256 = sha256_file(args.fcvm)
    except OSError as exc:
        die(f"cannot identify current fcvm bytes at {args.fcvm}: {exc}")
    try:
        generation = snapshot_generation_under_lease(
            resources, args.data_root, args.tag)
    except RuntimeError as exc:
        die(str(exc))
    inspected = sh_bounded(
        ["podman", "image", "inspect", "--format", "{{.Id}}", args.image], 30)
    raw_image_id = inspected.stdout.strip()
    if inspected.returncode != 0:
        die(f"cannot identify current image {args.image}: "
            f"{inspected.stderr.strip() or raw_image_id!r}")
    image_id = canonical_image_id(raw_image_id)
    if args.image_id:
        supplied_image_id = canonical_image_id(args.image_id)
        if supplied_image_id != image_id:
            die(f"current image {args.image} resolved to {image_id}, not staged "
                f"identity {supplied_image_id}")
    args.image_id = image_id
    validate_snapshot_for_benchmark(
        generation, args.image, image_id, CONTAINER_RESOLVE_TO,
        fcvm_sha256, args.runtime_bundle_sha256, args.source_revision)
    if sh(["sudo", "-n", "true"]).returncode != 0:
        die("passwordless sudo is required to create the per-instance cgroups")
    stray = stray_vmm_processes()
    if stray:
        die(f"stray fcvm/firecracker processes would be charged to this measurement:\n{stray}")
    la = wait_quiet(args.quiet_limit, args.quiet_wait)

    schedule = build_cell_schedule(
        ["fcvm-clone", "host-container"], args.ns, args.reps, args.seed,
        len(args.urls))
    meta = {"run_id": args.run_id, "started": time.time(), "loadavg1_at_start": la,
            "host_kernel": os.uname().release, "machine": os.uname().machine,
            "source_revision": args.source_revision,
            "runtime_bundle_sha256": args.runtime_bundle_sha256,
            "corpus_extra_runtime_bundle_sha256":
                args.corpus_extra_runtime_bundle_sha256,
            "snapshot": args.tag, "image": args.image,
            "image_id": image_id,
            "snapshot_generation": generation,
            "fcvm_sha256": fcvm_sha256,
            "report_py_sha256": sh(["sha256sum", REPORT]).stdout.split()[0],
            "cdpdrive_sha256": sh(["sha256sum", CDPDRIVE]).stdout.split()[0],
            "urls": args.urls, "ns": args.ns, "reps": args.reps,
            "schedule_seed": args.seed,
            "schedule": [{"side": side, "n": n, "rep": rep,
                          "url_indices": list(url_indices),
                          "urls": [args.urls[index] for index in url_indices]}
                         for side, n, rep, url_indices in schedule],
            "uffd_mode": args.uffd_mode, "uffd_prefetch": args.uffd_prefetch,
            "basis": "cgroup memory.current and PSS summed over EXACTLY that cgroup's "
                     "process set, on both sides: an fcvm clone's leaf cgroup holds fcvm, "
                     "firecracker, the namespace holder and pasta; a container's cgroup is "
                     "podman's own. MemAvailable delta from a quiesced pre-sample is recorded "
                     "beside them as an attribution-free check."}
    claim_results_dir(args.results)
    try:
        os.makedirs(args.container_create_ops_dir, exist_ok=True)
    except OSError as exc:
        die(f"cannot create container operation lease directory "
            f"{args.container_create_ops_dir}: {exc}")
    with open(os.path.join(args.results, "run.json"), "x") as f:
        json.dump(meta, f, indent=1)

    cg = CgroupSet(f"/sys/fs/cgroup/cbmem-{args.run_id}.slice")
    cells = []
    out = open(os.path.join(args.results, "samples.jsonl"), "x")
    fcvm_side = None
    container_side = None
    failure = None
    try:
        cg.setup()
        fcvm_side = FcvmSide(args, cg, args.run_id)
        fcvm_side.start_serve()
        container_side = ContainerSide(args, args.run_id)
        sides = {fcvm_side.name: fcvm_side, container_side.name: container_side}
        for side_name, n, rep, url_indices in schedule:
            cells.append(run_cell(
                sides[side_name], args, n, rep, url_indices, out))
    except BaseException as exc:
        failure = exc
    try:
        cleanup_harness_resources(container_side, fcvm_side, cg, out)
    except BaseException as exc:
        if failure is None:
            failure = exc
        else:
            log(f"cleanup also failed: {type(exc).__name__}: {exc}")
    if failure is not None:
        raise failure

    summary = {"run_id": args.run_id, "meta": meta, "cells": []}
    for c in cells:
        v = cell_values(c)
        summary["cells"].append({"side": c["side"], "n": c["n"], "rep": c["rep"], **v,
                                 "cgroup_mib_per_instance": round(v["cgroup_mib"] / c["n"], 1),
                                 "pss_mib_per_instance": round(v["pss_mib"] / c["n"], 1),
                                 "mem_available_delta_mib_per_instance": round(
                                     v["mem_available_delta_mib"] / c["n"], 1)})
    summary["fits"] = {}
    for side in sorted({c["side"] for c in cells}):
        rows = [(c["n"], cell_values(c)) for c in cells if c["side"] == side]
        fit = {}
        for basis in ("cgroup_mib", "pss_mib", "mem_available_delta_mib"):
            xs = [n for n, _ in rows]
            ys = [v[basis] for _, v in rows]
            slope, intercept = slope_intercept(xs, ys)
            fit[basis] = {"marginal_mib_per_instance": None if slope is None else round(slope, 1),
                          "fixed_mib": None if intercept is None else round(intercept, 1),
                          "points": sorted(zip(xs, [round(y, 1) for y in ys]))}
        # Per instance AT EACH N, with the observed spread across repetitions.
        # A single average across the whole N grid would mix N=1, where the
        # fixed cost is charged entirely to one instance, with N=8, where it is
        # spread over eight, and read as neither.
        fit["per_n"] = {}
        for n in sorted({n for n, _ in rows}):
            at = [v for m, v in rows if m == n]
            cell = {"reps": len(at)}
            for basis in ("cgroup_mib", "pss_mib", "mem_available_delta_mib"):
                vals = [v[basis] / n for v in at]
                cell[basis] = {"mean": round(statistics.mean(vals), 1),
                               "min": round(min(vals), 1), "max": round(max(vals), 1)}
            for extra in ("serve_pss_mib", "serve_cgroup_mib", "refuted_fc_only_pss_mib"):
                vals = [v[extra] for v in at if extra in v]
                if vals:
                    cell[extra + "_total"] = {"mean": round(statistics.mean(vals), 1),
                                              "min": round(min(vals), 1),
                                              "max": round(max(vals), 1)}
            fit["per_n"][n] = cell
        summary["fits"][side] = fit
    with open(os.path.join(args.results, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    try:
        publish_completion(args.results, args.run_id)
    except RuntimeError as exc:
        die(str(exc))
    print(json.dumps(summary["fits"], indent=1))
    log(f"records in {args.results}")


if __name__ == "__main__":
    main()
