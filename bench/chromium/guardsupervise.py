#!/usr/bin/env python3
"""Supervise a third-party process tree with parent-death cgroup cleanup.

Unlike guardexec, this wrapper remains alive because Chromium creates process
hops we cannot teach to arm PR_SET_PDEATHSIG.  If the harness dies, the wrapper
receives SIGTERM and atomically kills its owned cgroup, including Chromium's
browser, renderer, utility, and crash-handler processes.
"""

import argparse
import ctypes
import os
import signal
import subprocess
import sys


PR_SET_PDEATHSIG = 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-parent", required=True, type=int)
    parser.add_argument("--cgroup-procs", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = list(args.command)
    if command[:1] == ["--"]:
        command.pop(0)
    if not command:
        parser.error("a command is required after --")
    cgroup_procs = os.path.abspath(args.cgroup_procs)
    if os.path.basename(cgroup_procs) != "cgroup.procs":
        parser.error("--cgroup-procs must name cgroup.procs")
    if args.expected_parent <= 1 or os.getppid() != args.expected_parent:
        print("guardsupervise: expected parent is already gone", file=sys.stderr)
        return 125

    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_PDEATHSIG, signal.SIGTERM, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        print(f"guardsupervise: prctl failed: {os.strerror(error)}", file=sys.stderr)
        return 125
    if os.getppid() != args.expected_parent:
        print("guardsupervise: parent changed while arming parent-death", file=sys.stderr)
        return 125
    try:
        with open(cgroup_procs, "w") as stream:
            stream.write(f"{os.getpid()}\n")
    except OSError as error:
        print(f"guardsupervise: cannot enter cgroup: {error}", file=sys.stderr)
        return 125
    if os.getppid() != args.expected_parent:
        print("guardsupervise: parent changed before child launch", file=sys.stderr)
        return 125

    child = None

    def terminate(_signum, _frame):
        kill_path = os.path.join(os.path.dirname(cgroup_procs), "cgroup.kill")
        try:
            with open(kill_path, "w") as stream:
                stream.write("1\n")
        except OSError:
            if child is not None and child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    try:
        child = subprocess.Popen(command, start_new_session=True)
        return child.wait()
    except (OSError, subprocess.SubprocessError) as error:
        print(f"guardsupervise: cannot run child: {error}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    sys.exit(main())
