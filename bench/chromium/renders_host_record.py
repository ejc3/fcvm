#!/usr/bin/env python3
"""Write a pages-mode run's `host-record.txt`, when the run has ended.

    renders_host_record.py OUT --source COMMIT --kitesurf COMMIT --instance-type TYPE
                           [--arguments TEXT]

OUT is the run's directory (`make fleet-renders-bench OUT=...`). The record is what
renders_campaign.py holds every run of a campaign to: the kernel release, the kernel command
line, the fcvm commit the binary was built from, Kitesurf's commit, the instance type and a
hash of the boot id, the run's arguments and the time. It names no instance, address or
host: the boot id is hashed, and a partition or filesystem id on the command line is
written as `elided`.
"""

import argparse
import hashlib
import os
import re
import sys
import time

COMMIT = re.compile(r"^[0-9a-f]{40}$")


def elide(cmdline):
    """The command line with every `...UUID=<id>` value replaced."""
    return re.sub(r"(UUID=)\S+", r"\1elided", cmdline.strip())


def record(kernel, cmdline, boot_id, source, kitesurf, instance_type, arguments, ended):
    for name, commit in (("--source", source), ("--kitesurf", kitesurf)):
        if not COMMIT.fullmatch(commit):
            raise ValueError(f"{name} {commit!r} is not a commit")
    if not re.fullmatch(r"[a-z0-9]+\.[a-z0-9-]+", instance_type):
        raise ValueError(f"--instance-type {instance_type!r} is not an instance type")
    boot = hashlib.sha256(boot_id.encode()).hexdigest()[:16]
    one_line = " ".join(arguments.split())
    return (f"kernel: {kernel.strip()}\ncmdline: {elide(cmdline)}\nsource: {source}\nkitesurf: {kitesurf}\n"
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
