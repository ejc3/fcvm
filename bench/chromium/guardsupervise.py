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
import time


PR_SET_PDEATHSIG = 1


# `cgroup.kill` is synchronous for the signal delivery but not for the reaping, so a
# short drain is needed to tell "killed" from "still running". A signal handler is
# running against someone else's shutdown clock, so this stays small.
KILL_DRAIN_SECONDS = 2.0


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
        # Appending, not truncating: writing to a real cgroup.procs MOVES one pid into
        # the cgroup and never replaces its membership, so "w" misrepresents what this
        # write does and, on any file-backed stand-in, silently drops every other member.
        with open(cgroup_procs, "a") as stream:
            stream.write(f"{os.getpid()}\n")
    except OSError as error:
        print(f"guardsupervise: cannot enter cgroup: {error}", file=sys.stderr)
        return 125
    if os.getppid() != args.expected_parent:
        print("guardsupervise: parent changed before child launch", file=sys.stderr)
        return 125

    child = None
    cgroup_dir = os.path.dirname(cgroup_procs)

    def kill_cgroup():
        """Kill everything left in the owned cgroup, and say whether it emptied.

        The browser process exiting does not mean its tree has: the zygote, utility
        and crash-handler hops cannot arm PR_SET_PDEATHSIG, which is why this wrapper
        owns a cgroup at all. Those survivors keep running while the harness believes
        the request is over.

        Draining is only checked when `cgroup.kill` actually took. Without a real
        cgroup there is nothing that empties `cgroup.procs`, so polling it would just
        burn the shutdown budget and report survivors that the fallback already
        killed.
        """
        kill_path = os.path.join(cgroup_dir, "cgroup.kill")
        # Opening for write CREATES a regular file, so a successful write proves
        # nothing. Every cgroup v2 directory already has this attribute; a path that
        # does not is not a cgroup and has no kill semantics at all.
        try:
            if not os.path.exists(kill_path):
                raise OSError(f"{kill_path} is not a cgroup v2 attribute")
            with open(kill_path, "w") as stream:
                stream.write("1\n")
        except OSError:
            if child is not None and child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            return []
        deadline = time.monotonic() + KILL_DRAIN_SECONDS
        while True:
            try:
                with open(cgroup_procs) as stream:
                    remaining = [line for line in stream.read().split() if line]
            except OSError:
                return []
            if not remaining or time.monotonic() >= deadline:
                return remaining
            time.sleep(0.01)

    def terminate(_signum, _frame):
        # The signal path owes the same answer as the normal one: a cgroup that will
        # not empty is not a clean shutdown just because a signal started it.
        remaining = kill_cgroup()
        if remaining:
            print(f"guardsupervise: {len(remaining)} process(es) still in {cgroup_dir} "
                  f"after cgroup.kill: {remaining}", file=sys.stderr)
            raise SystemExit(126)
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    try:
        child = subprocess.Popen(command, start_new_session=True)
        rc = child.wait()
        # The normal path needs the same sweep as the signal path: the child exiting
        # says nothing about the hops it spawned.
        remaining = kill_cgroup()
        if remaining:
            print(f"guardsupervise: {len(remaining)} process(es) still in {cgroup_dir} "
                  f"after cgroup.kill: {remaining}", file=sys.stderr)
            return 126
        return rc
    except (OSError, subprocess.SubprocessError) as error:
        print(f"guardsupervise: cannot run child: {error}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    sys.exit(main())
