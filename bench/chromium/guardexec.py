#!/usr/bin/env python3
"""Enter one cgroup, arm parent-death, and replace this process.

The expected parent PID is supplied before ``fork()``.  Checking it on both
sides of ``PR_SET_PDEATHSIG`` closes the usual parent-exits-before-prctl race:
the child either observes its original parent, or exits without executing the
workload.  ``execvpe`` preserves the PID, so state files and pidfds still name
the real fcvm/bpftrace/Chromium process rather than a wrapper.
"""

import argparse
import ctypes
import os
import signal
import sys


PR_SET_PDEATHSIG = 1


def arm_parent_death(expected_parent: int) -> None:
    if expected_parent <= 1:
        raise RuntimeError("expected parent PID must be greater than 1")
    if os.getppid() != expected_parent:
        raise RuntimeError(
            f"parent changed before PR_SET_PDEATHSIG: expected {expected_parent}, "
            f"found {os.getppid()}"
        )
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    if os.getppid() != expected_parent:
        raise RuntimeError(
            f"parent changed while arming PR_SET_PDEATHSIG: expected {expected_parent}, "
            f"found {os.getppid()}"
        )


def enter_cgroup(cgroup_procs: str) -> None:
    if not os.path.isabs(cgroup_procs) or os.path.basename(cgroup_procs) != "cgroup.procs":
        raise RuntimeError(f"unsafe cgroup.procs path: {cgroup_procs!r}")
    with open(cgroup_procs, "w") as stream:
        stream.write(f"{os.getpid()}\n")


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

    try:
        arm_parent_death(args.expected_parent)
        enter_cgroup(args.cgroup_procs)
        if os.getppid() != args.expected_parent:
            raise RuntimeError(
                f"parent changed before exec: expected {args.expected_parent}, "
                f"found {os.getppid()}"
            )
        os.execvpe(command[0], command, os.environ)
    except (OSError, RuntimeError) as error:
        print(f"guardexec: {type(error).__name__}: {error}", file=sys.stderr)
        return 125
    return 125


if __name__ == "__main__":
    sys.exit(main())
