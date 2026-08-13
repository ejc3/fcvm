#!/usr/bin/env python3
"""Chrome-free restore-path matrix: is the fast path fast AND safe, everywhere?

The chromium request benchmark measures a browser workload end to end, so the
restore path is a minority of its wall clock and a regression there hides in
page-render noise. This driver removes the workload entirely: every cell
restores a clone whose container does nothing, so what is measured and
asserted IS the restore path.

It crosses the dimensions that change the restore path's behaviour, shuffles
the cell order each run so a slow cell cannot always sit behind the same warm
neighbour, and asserts the safety properties on every repetition rather than
only timing them:

  backend      uffd (a serve process feeds pages on demand) | file (clones
               restore MAP_PRIVATE straight from the snapshot files)
  network      rootless (pasta) | bridged (netns + iptables, needs root)
  concurrency  1, 4, 16 clones restored simultaneously from one source
  volumes      with and without a --map, so the NFS/FUSE remount phase is
               exercised rather than skipped
  lifecycle    ordinary restore, and SIGKILL inside the restore window (the
               next clone must still restore cleanly and the killed one must
               leave nothing behind)

Per repetition it asserts: the clone ACKed its exact restore generation, the
ACK carried per-phase telemetry, the boundary took the verified fast path (an
untampered snapshot must never need the repair path), an exec round trip
works, a mapped volume is readable where one is mapped, and the clone's state
file, lock and disk are all gone after teardown.

Results land under --out (default bench/chromium/results/restore-matrix/) as
matrix.jsonl plus summary.txt, one JSON record per cell repetition.

Bridged cells need an IPv4 default route: a host that routes only over IPv6
cannot pick an interface to NAT through, and those cells fail at golden
creation with that diagnosis. Rootless cells run anywhere.

Usage:
  sudo bench/restore-matrix.py --reps 5
  sudo bench/restore-matrix.py --reps 3 --cells backend,concurrency
"""

from __future__ import annotations

import argparse
import math
import json
import os
import random
import re
import signal
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass, field, asdict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_FCVM = REPO / "target" / "release" / "fcvm"
IMAGE = os.environ.get("MATRIX_IMAGE", "ecr-public.aws.com/nginx/nginx:alpine")

ACK = re.compile(r"acknowledged exact restore generation.*?guest_phases=(\{.*?\})")
SERVE_PID = re.compile(r"Serve PID: (\d+)")


def run(argv, timeout=120, check=False, env=None):
    return subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=check,
        env=env or os.environ.copy(),
    )


@dataclass
class Cell:
    backend: str
    network: str
    concurrency: int
    volumes: bool
    lifecycle: str

    def name(self) -> str:
        return (
            f"{self.backend}-{self.network}-c{self.concurrency}"
            f"-{'vol' if self.volumes else 'novol'}-{self.lifecycle}"
        )


@dataclass
class Result:
    cell: str
    rep: int
    ok: bool
    failures: list = field(default_factory=list)
    phases: list = field(default_factory=list)
    ready_ms: list = field(default_factory=list)


class Harness:
    def __init__(self, args):
        self.fcvm = str(args.fcvm)
        self.run_id = uuid.uuid4().hex[:12]
        # Everything this run writes lives under its own run_id directory:
        # two overlapping invocations otherwise truncate each other's
        # serve-<tag>.log (corrupting the OTHER run's readiness poll with this
        # run's PID) and the shared matrix.jsonl.
        self.out = Path(args.out) / self.run_id
        self.out.mkdir(parents=True, exist_ok=True)
        self.data_root = Path(args.data_root)
        self.state_dir = self.data_root / "state"
        self.prepared_tags: set = set()
        self.records = []

    # ---- infrastructure -------------------------------------------------

    def fcvm_json(self, *argv):
        out = run([self.fcvm, *argv], timeout=60)
        if out.returncode != 0 or not out.stdout.strip():
            return []
        try:
            return json.loads(out.stdout)
        except json.JSONDecodeError:
            return []

    def vm_by_name(self, name):
        for vm in self.fcvm_json("ls", "--json"):
            if vm.get("name") == name:
                return vm
        return None

    def make_golden(self, tag, network, volumes):
        """One cold VM, snapshotted at its health gate, reused by every rep.

        Prepared once per (tag, run): the tag's content key does not change
        within a run, so later cells take `podman prepare`'s verified cache hit
        instead of cold-building the same snapshot 33 times for 4 distinct tags.
        `--force` applies only to the first build, to shed a stale
        previous-run entry.
        """
        if tag in self.prepared_tags:
            return []
        mapping = []
        if volumes:
            host_dir = Path(f"/mnt/fcvm-btrfs/matrix-vol-{self.run_id}")
            host_dir.mkdir(parents=True, exist_ok=True)
            (host_dir / "canary").write_text("matrix\n")
            mapping = ["--map", f"{host_dir}:/mnt/matrix"]
        argv = [
            self.fcvm, "podman", "prepare", "--tag", tag, "--force",
            "--name", f"matrix-src-{self.run_id}",
            "--cpu", "2", "--mem", "1024", "--network", network,
            "--publish", "18400:80", *mapping, IMAGE,
        ]
        out = run(argv, timeout=900)
        if out.returncode != 0:
            raise RuntimeError(f"golden failed for {tag}: {out.stderr[-2000:]}")
        self.prepared_tags.add(tag)
        return mapping

    def start_serve(self, tag):
        log = self.out / f"serve-{tag}.log"
        handle = open(log, "w")
        proc = subprocess.Popen(
            [self.fcvm, "snapshot", "serve", tag], stdout=handle, stderr=handle
        )
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            text = log.read_text(errors="replace")
            if "Waiting for VMs" in text:
                match = SERVE_PID.search(text)
                if match:
                    return proc, int(match.group(1))
            if proc.poll() is not None:
                raise RuntimeError(f"serve exited: {log.read_text()[-2000:]}")
            time.sleep(0.1)
        # The caller never learns this proc's handle (the assignment it would
        # land in does not complete), so reap it HERE or it outlives the cell.
        proc.terminate()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=15)
        raise RuntimeError("serve never became ready")

    # ---- one repetition -------------------------------------------------

    def restore_clone(self, cell, tag, serve_pid, index, rep):
        name = f"m-{self.run_id}-{cell.name()}-{rep}-{index}"
        log = self.out / f"{name}.log"
        backend = (
            ["--pid", str(serve_pid)] if cell.backend == "uffd"
            else ["--snapshot", tag]
        )
        env = os.environ.copy()
        env["RUST_LOG"] = "fcvm=debug"
        handle = open(log, "w")
        started = time.monotonic()
        proc = subprocess.Popen(
            [self.fcvm, "snapshot", "run", *backend, "--name", name,
             "--no-dirty-tracking", "--no-swap"],
            stdout=handle, stderr=handle, env=env,
        )
        return {"name": name, "log": log, "proc": proc, "started": started}

    def wait_ready(self, clone, timeout=120):
        """Ready == the guest ACKed its exact restore generation."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            text = clone["log"].read_text(errors="replace")
            match = ACK.search(text)
            if match:
                clone["ready_ms"] = (time.monotonic() - clone["started"]) * 1000
                try:
                    clone["phases"] = json.loads(match.group(1))
                except json.JSONDecodeError:
                    clone["phases"] = {}
                return True
            if clone["proc"].poll() is not None:
                return False
            time.sleep(0.05)
        return False

    def assert_live(self, clone, cell, failures):
        vm = self.vm_by_name(clone["name"])
        if not vm:
            failures.append(f"{clone['name']}: no state file after readiness")
            return
        pid = vm.get("pid")
        exec_out = run(
            [self.fcvm, "exec", "--pid", str(pid), "--vm", "--", "true"],
            timeout=60,
        )
        if exec_out.returncode != 0:
            failures.append(
                f"{clone['name']}: exec round trip failed rc={exec_out.returncode} "
                f"{exec_out.stderr[-200:]}"
            )
        if cell.volumes:
            cat = run(
                [self.fcvm, "exec", "--pid", str(pid), "--vm", "--",
                 "cat", "/mnt/matrix/canary"],
                timeout=60,
            )
            if "matrix" not in cat.stdout:
                failures.append(f"{clone['name']}: mapped volume unreadable")

    def teardown(self, clone, failures, expect_clean=True):
        vm = self.vm_by_name(clone["name"])
        vm_id = (vm or {}).get("vm_id")
        proc = clone["proc"]
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=30)
                failures.append(f"{clone['name']}: needed SIGKILL to exit")
        if not expect_clean or not vm_id:
            return
        deadline = time.monotonic() + 30
        leftovers = []
        while time.monotonic() < deadline:
            leftovers = [
                str(p)
                for p in (
                    self.state_dir / f"{vm_id}.json",
                    self.state_dir / f"{vm_id}.json.lock",
                    self.data_root / "vm-disks" / vm_id,
                )
                if p.exists()
            ]
            if not leftovers:
                return
            time.sleep(0.25)
        failures.append(f"{clone['name']}: teardown left {leftovers}")

    RESTORE_STARTED = re.compile(r"starting pasta for rootless networking|network namespace configured")

    def validate_ready_clone(self, clone, cell, phases, ready, failures):
        """The full per-clone contract: telemetry present, verified fast path,
        exec round trip, mapped volume readable. One place, so the
        kill-mid-restore survivor cannot silently get a weaker check than an
        ordinary clone."""
        if not clone["phases"]:
            failures.append(f"{clone['name']}: ACK carried no phase telemetry")
        elif not clone["phases"].get("tcp_verified"):
            failures.append(
                f"{clone['name']}: boundary took the repair path "
                f"(tcp_verified=false) on an untampered snapshot"
            )
        phases.append(clone["phases"])
        ready.append(clone["ready_ms"])
        self.assert_live(clone, cell, failures)

    def kill_mid_restore(self, cell, tag, serve_pid, rep, clones, phases, ready, failures):
        """SIGKILL inside a PROVEN restore window, then prove the aftermath.

        The window is pinned by observation, not by a sleep: each clone must
        have logged the start of its network setup (restore underway) and must
        NOT yet have ACKed its restore generation. A kill before restore starts
        or after the ACK exercises a different, easier property, and the cell
        says so instead of passing vacuously.

        What SIGKILL is ASSERTED to leave behind follows what fcvm promises
        (AGENTS.md "PROCESS TEARDOWN IS PER-HOP" + "Stale State File
        Handling"): the process TREE dies with the killed fcvm — pdeathsig is
        kernel-enforced and survives SIGKILL — but cleanup CODE does not run,
        so the state file and disk may legitimately remain, as documented, for
        the next run's stale-state handling to collect. Asserting "no
        artifacts" here would assert a property fcvm deliberately does not
        have. The load-bearing assertions are: no VMM/pasta process outlives
        its killed fcvm, and the survivor restores with the FULL ordinary
        contract despite the stale residue. The residue is then removed so
        later cells start clean.
        """
        for clone in clones:
            deadline = time.monotonic() + 30
            started = False
            while time.monotonic() < deadline:
                text = clone["log"].read_text(errors="replace")
                if ACK.search(text):
                    break
                if self.RESTORE_STARTED.search(text):
                    started = True
                    break
                if clone["proc"].poll() is not None:
                    break
                time.sleep(0.01)
            text = clone["log"].read_text(errors="replace")
            if ACK.search(text):
                failures.append(
                    f"{clone['name']}: ACKed before the kill could land — this rep "
                    f"did not exercise a mid-restore kill"
                )
            elif not started:
                failures.append(
                    f"{clone['name']}: restore never observably started before kill"
                )
        victims = []
        for clone in clones:
            vm = self.vm_by_name(clone["name"])
            victims.append((clone, (vm or {}).get("vm_id")))
        for clone in clones:
            clone["proc"].kill()
            clone["proc"].wait(timeout=30)

        # pdeathsig must reap the whole subtree of each killed fcvm.
        deadline = time.monotonic() + 15
        stragglers = []
        while time.monotonic() < deadline:
            ps = run(["ps", "-eo", "args"], timeout=30).stdout
            stragglers = [
                clone["name"] for clone in clones if clone["name"] in ps
            ]
            if not stragglers:
                break
            time.sleep(0.25)
        if stragglers:
            failures.append(
                f"processes outlived their SIGKILLed fcvm (pdeathsig hole): {stragglers}"
            )

        survivor = self.restore_clone(cell, tag, serve_pid, 99, rep)
        try:
            if not self.wait_ready(survivor):
                failures.append("a clone after a killed restore never became ready")
            else:
                self.validate_ready_clone(survivor, cell, phases, ready, failures)
        finally:
            self.teardown(survivor, failures)

        # Sweep the documented crash residue so later cells start clean.
        for _clone, vm_id in victims:
            if not vm_id:
                continue
            for path in (
                self.state_dir / f"{vm_id}.json",
                self.state_dir / f"{vm_id}.json.lock",
            ):
                path.unlink(missing_ok=True)
            disk = self.data_root / "vm-disks" / vm_id
            if disk.exists():
                subprocess.run(["rm", "-rf", str(disk)], timeout=60, check=False)

    def run_cell(self, cell, tag, serve_pid, rep) -> Result:
        failures = []
        phases, ready = [], []
        clones = []
        try:
            # Launched incrementally: if a later spawn raises, every clone
            # started before it is still in `clones` for the finally below.
            for i in range(cell.concurrency):
                clones.append(self.restore_clone(cell, tag, serve_pid, i, rep))

            if cell.lifecycle == "kill-mid-restore":
                self.kill_mid_restore(
                    cell, tag, serve_pid, rep, clones, phases, ready, failures
                )
                return Result(cell.name(), rep, not failures, failures, phases, ready)

            for clone in clones:
                if not self.wait_ready(clone):
                    failures.append(
                        f"{clone['name']}: never ACKed a restore generation; "
                        f"log tail: {clone['log'].read_text(errors='replace')[-400:]}"
                    )
                    continue
                self.validate_ready_clone(clone, cell, phases, ready, failures)
            return Result(cell.name(), rep, not failures, failures, phases, ready)
        finally:
            # Every launched clone is torn down on EVERY path — the ordinary
            # return, the kill-cell return, and any exception out of
            # restore_clone/wait_ready/assert_live. Idempotent for clones the
            # kill path already reaped (their proc has exited; teardown then
            # only checks residue).
            for clone in clones:
                self.teardown(
                    clone,
                    failures,
                    expect_clean=(cell.lifecycle != "kill-mid-restore"),
                )


def build_cells(selected):
    cells = []
    for backend in ("uffd", "file"):
        for network in ("rootless", "bridged"):
            cells.append(Cell(backend, network, 1, False, "ordinary"))
    for concurrency in (4, 16):
        cells.append(Cell("uffd", "rootless", concurrency, False, "ordinary"))
        cells.append(Cell("file", "rootless", concurrency, False, "ordinary"))
    cells.append(Cell("uffd", "rootless", 1, True, "ordinary"))
    cells.append(Cell("file", "rootless", 1, True, "ordinary"))
    cells.append(Cell("uffd", "rootless", 4, False, "kill-mid-restore"))
    if selected:
        # Selectors are matrix DIMENSION VALUES (uffd, file, rootless, bridged,
        # ordinary, kill-mid-restore, vol, novol, c1/c4/c16), matched as whole
        # facts about a cell rather than substrings of its name — "c1" must not
        # select c16. Every token must be known and the result non-empty:
        # a selection that evaluates nothing must not report success.
        known = {
            "uffd": lambda c: c.backend == "uffd",
            "file": lambda c: c.backend == "file",
            "rootless": lambda c: c.network == "rootless",
            "bridged": lambda c: c.network == "bridged",
            "ordinary": lambda c: c.lifecycle == "ordinary",
            "kill-mid-restore": lambda c: c.lifecycle == "kill-mid-restore",
            "vol": lambda c: c.volumes,
            "novol": lambda c: not c.volumes,
            "c1": lambda c: c.concurrency == 1,
            "c4": lambda c: c.concurrency == 4,
            "c16": lambda c: c.concurrency == 16,
        }
        tokens = [t for t in selected.split(",") if t]
        unknown = [t for t in tokens if t not in known]
        if unknown:
            raise SystemExit(
                f"--cells: unknown selector(s) {unknown}; valid: {sorted(known)}"
            )
        cells = [c for c in cells if any(known[t](c) for t in tokens)]
        if not cells:
            raise SystemExit(f"--cells {selected!r} selects no cells")
    return cells


def summarise(records):
    lines = []
    for name in sorted({r["cell"] for r in records}):
        rows = [r for r in records if r["cell"] == name]
        failed = [r for r in rows if not r["ok"]]
        totals = sorted(
            p.get("total_ms", 0.0) for r in rows for p in r["phases"] if p
        )
        # Spawn-to-ACK, measured host-side from process spawn to the ACK line:
        # includes snapshot load, VMM startup and delivery — the guest-only
        # phase total below starts inside fc-agent and hides exactly the
        # backend overhead this matrix compares.
        spawn_acks = sorted(ms for r in rows for ms in r["ready_ms"])
        verified = sum(
            1 for r in rows for p in r["phases"] if p.get("tcp_verified")
        )
        observed = sum(len(r["phases"]) for r in rows)
        def pct(sorted_values, q):
            # Ceiling rank: with 3 samples p95 is the maximum, not the median.
            if not sorted_values:
                return float("nan")
            rank = max(0, math.ceil(q * len(sorted_values)) - 1)
            return sorted_values[rank]

        lines.append(
            f"{name:44s} reps={len(rows):2d} restores={observed:3d} "
            f"verified={verified:3d} "
            f"spawn_ack_p50={pct(spawn_acks, 0.50):7.1f} "
            f"spawn_ack_p95={pct(spawn_acks, 0.95):7.1f} "
            f"guest_p50={pct(totals, 0.50):7.1f} "
            f"{'FAIL x' + str(len(failed)) if failed else 'ok'}"
        )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fcvm", default=str(DEFAULT_FCVM))
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--cells", default="")
    parser.add_argument("--data-root", default="/mnt/fcvm-btrfs")
    parser.add_argument(
        "--out",
        default=str(REPO / "bench" / "chromium" / "results" / "restore-matrix"),
    )
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()

    if os.geteuid() != 0:
        print("bridged cells need root; re-run under sudo", file=sys.stderr)
        return 2

    harness = Harness(args)
    cells = build_cells(args.cells)
    random.seed(args.seed or int(time.time()))

    print(f"run {harness.run_id}: {len(cells)} cells x {args.reps} reps")
    jsonl = harness.out / "matrix.jsonl"
    with open(jsonl, "w") as sink:
        for rep in range(1, args.reps + 1):
            order = cells[:]
            random.shuffle(order)
            for cell in order:
                tag = f"matrix-{cell.network}-{'vol' if cell.volumes else 'novol'}"
                serve_proc = None
                try:
                    harness.make_golden(tag, cell.network, cell.volumes)
                    serve_pid = 0
                    if cell.backend == "uffd":
                        serve_proc, serve_pid = harness.start_serve(tag)
                    result = harness.run_cell(cell, tag, serve_pid, rep)
                except Exception as error:  # noqa: BLE001 - reported, not raised
                    result = Result(cell.name(), rep, False, [f"cell error: {error}"])
                finally:
                    if serve_proc is not None:
                        serve_proc.terminate()
                        try:
                            serve_proc.wait(30)
                        except subprocess.TimeoutExpired:
                            serve_proc.kill()
                            serve_proc.wait(15)
                record = asdict(result)
                harness.records.append(record)
                sink.write(json.dumps(record) + "\n")
                sink.flush()
                status = "ok" if result.ok else f"FAIL {result.failures[:2]}"
                print(f"  rep{rep} {cell.name():44s} {status}", flush=True)

    summary = summarise(harness.records)
    (harness.out / "summary.txt").write_text(summary + "\n")
    print("\n" + summary)
    failed = [r for r in harness.records if not r["ok"]]
    print(f"\n{len(harness.records) - len(failed)}/{len(harness.records)} cells clean")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
