#!/usr/bin/env python3
"""Write a pages-mode run's `host-record.txt`, when the run has ended.

    renders_host_record.py OUT --source COMMIT --kitesurf COMMIT --instance-type TYPE
                           [--arguments TEXT]

OUT is the run's directory (`make fleet-renders-bench OUT=...`). The record is what
renders_campaign.py holds every run of a campaign to: the kernel release, the kernel
parameters that change what is measured (from a list: KERNEL_PARAMETERS), the fcvm commit the
binary was built from, Kitesurf's commit, the instance type and a hash of the boot id, the
run's arguments and the time. Nothing else of the host is written: the boot id is hashed, the
rest of the kernel command line is dropped, and an argument is one of the launcher's variables
(ARGUMENTS) with a value of its shape, a path cut to its last name. What a shape cannot tell
(a directory's last name, a snapshot's tag) is the operator's own word.
"""

import argparse
import hashlib
import os
import re
import sys
import time

COMMIT = re.compile(r"^[0-9a-f]{40}$")
# The kernel parameters a record keeps: the ones that change what a run measures. Nothing else
# of the command line is written (a root device, a console, an address, a host name), and a
# kept parameter's value is a plain word.
KERNEL_PARAMETERS = (
    "kvm-arm.mode", "arm64.nv2", "numa", "mitigations", "nosmt", "maxcpus", "nr_cpus", "isolcpus",
    "nohz_full", "rcu_nocbs", "preempt", "hugepagesz", "hugepages", "default_hugepagesz",
    "transparent_hugepage",
)
PARAMETER_VALUE = re.compile(r"^[A-Za-z0-9_,-]{1,40}$")
NO_PARAMETERS = "(none of the recorded parameters)"
# An instance type: a family and a size, as EC2 names them.
INSTANCE_TYPE = re.compile(r"^[a-z][a-z0-9]{1,8}(-[a-z0-9]+)?\.(nano|micro|small|medium|large|[0-9]{0,2}xlarge|metal(-[0-9]{1,2}xl)?)$")
# The arguments of a run a record keeps: the launcher's variables (renders_bench.sh), each with the
# shape of its value. A name outside the list, or a value of another shape, is refused: the line
# is the operator's text, and only what a run's arguments look like is written.
NUMBER, NAME = r"[0-9]{1,6}", r"[A-Za-z0-9][A-Za-z0-9_-]{0,39}"
ARGUMENTS = {
    "MAX_FCVM": NUMBER, "MAX_ENGINES": NUMBER, "SECONDS_RUN": NUMBER, "RECYCLE_MIB": NUMBER,
    "REPLAY_PROCS": NUMBER, "REPLAY_CPUS": NUMBER, "TIMEOUT_S": NUMBER, "CPU": NUMBER, "MEM": NUMBER,
    "LEVELS": NUMBER + "," + NUMBER, "VIEWPORT": r"[0-9]{2,5}x[0-9]{2,5}",
    "CPUS_FCVM": r"[0-9,-]{1,40}", "CPUS_ENGINES": r"[0-9,-]{1,40}", "CPUS_HARNESS": r"[0-9,-]{1,40}",
    "DIST": NAME, "TAG": NAME,
}


def kept_parameters(cmdline):
    """The command line's tokens that are a listed parameter with a plain value, in order."""
    kept = []
    for token in cmdline.split():
        name, sep, value = token.partition("=")
        if name in KERNEL_PARAMETERS and (not sep or PARAMETER_VALUE.fullmatch(value)):
            kept.append(token)
    return " ".join(kept) or NO_PARAMETERS


def argument(token):
    """A listed NAME=value, a path's value cut to its last name; raises ValueError on anything else."""
    name, sep, value = token.partition("=")
    if sep and "/" in value:
        value = os.path.basename(value.rstrip("/"))
    if not sep or name not in ARGUMENTS or not re.fullmatch(ARGUMENTS[name], value):
        raise ValueError(f"argument {token!r} is not one of {', '.join(sorted(ARGUMENTS))} with a value of its shape")
    return f"{name}={value}"


def record(kernel, cmdline, boot_id, source, kitesurf, instance_type, arguments, ended):
    for name, commit in (("--source", source), ("--kitesurf", kitesurf)):
        if not COMMIT.fullmatch(commit):
            raise ValueError(f"{name} {commit!r} is not a commit")
    if not INSTANCE_TYPE.fullmatch(instance_type):
        raise ValueError(f"--instance-type {instance_type!r} is not an instance type")
    boot = hashlib.sha256(boot_id.encode()).hexdigest()[:16]
    one_line = " ".join(argument(token) for token in arguments.split())
    return (f"kernel: {kernel.strip()}\ncmdline: {kept_parameters(cmdline)}\nsource: {source}\nkitesurf: {kitesurf}\n"
            f"host: {instance_type}  boot: {boot}\narguments: {one_line}\nended: {ended}\n")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("out")
    p.add_argument("--source", required=True)
    p.add_argument("--kitesurf", required=True)
    p.add_argument("--instance-type", required=True)
    p.add_argument("--arguments", default="")
    opts = p.parse_args(argv)
    try:
        with open("/proc/sys/kernel/osrelease") as handle:
            kernel = handle.read()
        with open("/proc/cmdline") as handle:
            cmdline = handle.read()
        with open("/proc/sys/kernel/random/boot_id") as handle:
            boot_id = handle.read()
        text = record(kernel, cmdline, boot_id, opts.source, opts.kitesurf, opts.instance_type, opts.arguments,
                      time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    except (OSError, ValueError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 2
    with open(os.path.join(opts.out, "host-record.txt"), "w") as handle:
        handle.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
