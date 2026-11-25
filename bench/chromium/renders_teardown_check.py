#!/usr/bin/env python3
"""Kill pages mode's module at each stage and count what it left.

    renders_teardown_check.py --unit NAME --status FILE --teardown FILE --run-dir DIR \\
        --stages cgroups,replays,serve,engines,workers,running,running:TERM \\
        [--port ADDR:PORT ...] [--snapshot-tag TAG] -- COMMAND...

COMMAND starts the module (`fcvm fleet renders-bench`) as the systemd unit NAME and
returns, passing the arguments appended to it on to the module; the module writes its
stage to FILE. For each stage named, the check starts the module with `--hold-at STAGE`,
waits until the status file names the stage (and, at `golden` and `running`, for
--after-s of that stage's work), signals the module's main process (SIGKILL, or the
signal after the colon), waits for the unit to end and for the reaper's
report, and counts:

  processes   the module's programs still running that were not running before
  cgroup      the unit's cgroup, if it is still there
  namespaces  network namespaces of this user's processes, beyond those before
  clones      fcvm state files and VM directories that were not there before
  sockets     the snapshot's memory server sockets
  ports       the module's addresses still listened on
  run dir     the module's scratch directory
  2 MiB pages pages the pool has fewer free of than before
  reaper      what the reaper's own report says it could not remove

Every count must be zero at every stage; the exit status is 1 otherwise. On a host where
other fcvm work runs, processes and namespaces that work starts during a stage are counted
too, so run it on a host that is otherwise idle.
"""

import argparse
import glob
import json
import os
import re
import signal
import subprocess
import sys
import time

PROGRAMS = (
    r"corpus_serve\.py", r"renders_worker\.py", r"guardexec\.py", r"wpt-engine\.mjs",
    r"(^|/)workerd( |$)", r"(^|/)firecracker( |$)", r"fcvm snapshot (run|serve)",
    r"(^|/)pasta( |$)", r"fleet renders-(bench|reap)",
)


UNHELD = ("golden", "running")


def out(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def processes():
    found = {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as handle:
                cmd = handle.read().replace(b"\0", b" ").decode(errors="replace").strip()
        except OSError:
            continue
        if cmd and any(re.search(p, cmd) for p in PROGRAMS) and "renders_teardown_check" not in cmd:
            found[int(pid)] = cmd[:120]
    return found


def files_under(root):
    found = set()
    for base, _dirs, names in os.walk(root):
        found.update(os.path.join(base, n) for n in names if n.endswith(".json"))
    return found


def listening():
    rows = out(["ss", "-H", "-ltn"]).splitlines() + out(["ss", "-H", "-lun"]).splitlines()
    return {row.split()[3] for row in rows if len(row.split()) > 3}


def hugepages_free():
    with open("/proc/meminfo") as handle:
        for line in handle:
            if line.startswith("HugePages_Free:"):
                return int(line.split()[1])
    return 0


def census(opts):
    return {
        "processes": processes(),
        "namespaces": set(out(["lsns", "-t", "net", "-n", "-o", "NS"]).split()),
        "states": files_under(opts.state_dir) if opts.state_dir else set(),
        "vms": set(os.listdir(opts.vm_dir)) if opts.vm_dir and os.path.isdir(opts.vm_dir) else set(),
        "pages": hugepages_free(),
    }


def unit_state(unit):
    return out(["systemctl", "is-active", unit]).strip()


def wait_for(what, test, timeout):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        value = test()
        if value:
            return value
        time.sleep(0.05)
    raise TimeoutError(f"{what} did not happen within {timeout} s")


def stage_now(path):
    try:
        with open(path) as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return {}


def one(opts, stage, signame):
    for path in (opts.status, opts.teardown):
        if os.path.exists(path):
            os.unlink(path)
    before = census(opts)
    # The module waits at the stage (--hold-at), so the signal lands in it however short
    # the stage is. Two stages are work in progress and are not held: at "golden" the
    # snapshot's command runs, and at "running" the sides render, each for --after-s first.
    hold = [] if stage in UNHELD else ["--hold-at", stage]
    subprocess.run(opts.command + hold, check=True)
    reached = wait_for(f"stage {stage}", lambda: (s := stage_now(opts.status)).get("stage") == stage and s,
                       opts.timeout_s)
    cgroup = out(["systemctl", "show", "-p", "ControlGroup", "--value", opts.unit]).strip()
    if opts.after_s and stage in UNHELD:
        time.sleep(opts.after_s)
    during = census(opts)
    os.kill(reached["pid"], getattr(signal, f"SIG{signame}"))
    killed = time.monotonic()
    wait_for("the unit's end", lambda: unit_state(opts.unit) not in ("active", "deactivating", "activating"), 180)
    wait_for("the reaper's report", lambda: os.path.exists(opts.teardown), 60)
    ended_s = time.monotonic() - killed
    time.sleep(0.5)
    after = census(opts)
    with open(opts.teardown) as handle:
        reaped = json.load(handle)
    ports = sorted(set(opts.port) & listening())
    sockets = glob.glob(os.path.join(opts.data_dir, f"uffd-{opts.snapshot_tag}-*.sock")) \
        if opts.data_dir and opts.snapshot_tag else []
    left = {
        "processes": sorted(cmd for pid, cmd in after["processes"].items() if pid not in before["processes"]),
        "cgroup": [cgroup] if cgroup and os.path.exists(f"/sys/fs/cgroup{cgroup}") else [],
        "namespaces": sorted(after["namespaces"] - before["namespaces"]),
        "clones": sorted((after["states"] - before["states"]) | (after["vms"] - before["vms"])),
        "sockets": sockets,
        "ports": ports,
        "run dir": [opts.run_dir] if os.path.exists(opts.run_dir) else [],
        "2 MiB pages": [f"{before['pages'] - after['pages']} fewer free"] if after["pages"] < before["pages"] else [],
        "reaper": [f"{k}={reaped[k]}" for k in ("cgroups_left", "resources_left", "run_dir_left") if reaped.get(k)],
    }
    return {
        "stage": stage, "signal": signame, "ended_s": round(ended_s, 2),
        "running_at_the_kill": {
            "processes": len(during["processes"]) - len(before["processes"]),
            "namespaces": len(during["namespaces"] - before["namespaces"]),
            "clones": len(during["states"] - before["states"]),
        },
        "reaper_found": {k: reaped.get(k) for k in ("cgroups_populated", "owners", "resources", "sockets")},
        "left": {k: v for k, v in left.items() if v},
    }


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--unit", required=True)
    p.add_argument("--status", required=True)
    p.add_argument("--teardown", required=True)
    p.add_argument("--run-dir", required=True)
    p.add_argument("--stages", required=True, help="comma separated; stage or stage:SIGNAL")
    p.add_argument("--port", action="append", default=[], metavar="ADDR:PORT")
    p.add_argument("--state-dir", default="")
    p.add_argument("--vm-dir", default="")
    p.add_argument("--data-dir", default="")
    p.add_argument("--snapshot-tag", default="")
    p.add_argument("--after-s", type=float, default=0.0, help="seconds of a stage's work before the signal, at 'golden' and 'running'")
    p.add_argument("--timeout-s", type=float, default=900.0)
    p.add_argument("--out", default="")
    p.add_argument("command", nargs=argparse.REMAINDER)
    opts = p.parse_args(argv)
    opts.command = [c for c in opts.command if c != "--"] if opts.command[:1] == ["--"] else opts.command
    if not opts.command:
        p.error("no command to start the module")
    rows = []
    for entry in opts.stages.split(","):
        stage, _, signame = entry.partition(":")
        row = one(opts, stage, signame or "KILL")
        rows.append(row)
        print(json.dumps(row), flush=True)
    failed = [r for r in rows if r["left"]]
    verdict = {"stages": len(rows), "with_something_left": len(failed)}
    print(json.dumps(verdict), flush=True)
    if opts.out:
        with open(opts.out, "w") as handle:
            json.dump({"rows": rows, **verdict}, handle, indent=1)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
