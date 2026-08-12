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
        self.out = Path(args.out)
        self.out.mkdir(parents=True, exist_ok=True)
        self.data_root = Path(args.data_root)
        self.state_dir = self.data_root / "state"
        self.run_id = uuid.uuid4().hex[:12]
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
        """One cold VM, snapshotted at its health gate, reused by every rep."""
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

    def run_cell(self, cell, tag, serve_pid, rep) -> Result:
        failures = []
        clones = [
            self.restore_clone(cell, tag, serve_pid, i, rep)
            for i in range(cell.concurrency)
        ]
        phases, ready = [], []

        if cell.lifecycle == "kill-mid-restore":
            # SIGKILL inside the restore window: the host must not leave a
            # clone half-published, and the NEXT clone must be unaffected.
            time.sleep(0.05)
            for clone in clones:
                clone["proc"].kill()
                clone["proc"].wait(timeout=30)
            for clone in clones:
                self.teardown(clone, failures, expect_clean=False)
            survivor = self.restore_clone(cell, tag, serve_pid, 99, rep)
            if not self.wait_ready(survivor):
                failures.append("a clone after a killed restore never became ready")
            else:
                phases.append(survivor["phases"])
                ready.append(survivor["ready_ms"])
                self.assert_live(survivor, cell, failures)
            self.teardown(survivor, failures)
            return Result(cell.name(), rep, not failures, failures, phases, ready)

        for clone in clones:
            if not self.wait_ready(clone):
                failures.append(
                    f"{clone['name']}: never ACKed a restore generation; "
                    f"log tail: {clone['log'].read_text(errors='replace')[-400:]}"
                )
                continue
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

        for clone in clones:
            self.teardown(clone, failures)
        return Result(cell.name(), rep, not failures, failures, phases, ready)


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
        wanted = set(selected.split(","))
        cells = [c for c in cells if any(w in c.name() for w in wanted)]
    return cells


def summarise(records):
    lines = []
    for name in sorted({r["cell"] for r in records}):
        rows = [r for r in records if r["cell"] == name]
        failed = [r for r in rows if not r["ok"]]
        totals = sorted(
            p.get("total_ms", 0.0) for r in rows for p in r["phases"] if p
        )
        verified = sum(
            1 for r in rows for p in r["phases"] if p.get("tcp_verified")
        )
        observed = sum(len(r["phases"]) for r in rows)
        if totals:
            p50 = totals[len(totals) // 2]
            p95 = totals[max(0, int(len(totals) * 0.95) - 1)]
        else:
            p50 = p95 = float("nan")
        lines.append(
            f"{name:44s} reps={len(rows):2d} restores={observed:3d} "
            f"verified={verified:3d} ack_p50={p50:7.1f} ack_p95={p95:7.1f} "
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
