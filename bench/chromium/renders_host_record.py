#!/usr/bin/env python3
"""Write a pages-mode run's `host-record.txt`, when the run has ended.

    renders_host_record.py OUT --source COMMIT --kitesurf COMMIT --instance-type TYPE
                           [--arguments TEXT]

OUT is the run's directory (`make fleet-renders-bench OUT=...`). The record is what
renders_campaign.py holds every run of a campaign to: the kernel release, the kernel
parameters that change what is measured (from a list: KERNEL_PARAMETERS), the fcvm commit the
binary was built from, Kitesurf's commit, the instance type and a hash of the boot id, the
run's arguments and the time. Nothing else of the host is written: the boot id is hashed, the
rest of the kernel command line is dropped, and an argument whose value is a path keeps its
last name only.
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
# One argument of a run: a make variable and a plain value.
ARGUMENT = re.compile(r"^[A-Z][A-Z0-9_]*=[A-Za-z0-9_,-]*(\.[A-Za-z0-9_-]+)*$")
ADDRESS = re.compile(r"^\d+(\.\d+){3}$")


def kept_parameters(cmdline):
    """The command line's tokens that are a listed parameter with a plain value, in order."""
    kept = []
    for token in cmdline.split():
        name, sep, value = token.partition("=")
        if name in KERNEL_PARAMETERS and (not sep or PARAMETER_VALUE.fullmatch(value)):
            kept.append(token)
    return " ".join(kept) or NO_PARAMETERS


def argument(token):
    """NAME=value with a path's value cut to its last name; raises ValueError on anything else."""
    name, sep, value = token.partition("=")
    if sep and "/" in value:
        value = os.path.basename(value.rstrip("/"))
    out = f"{name}{sep}{value}"
    if not ARGUMENT.fullmatch(out) or ADDRESS.fullmatch(value):
        raise ValueError(f"argument {token!r} is not NAME=value with a plain value")
    return out


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
