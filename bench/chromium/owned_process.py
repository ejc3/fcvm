#!/usr/bin/env python3
"""Signal a process only while its PID still names one recorded start time."""

import argparse
import os
import signal


class IdentityUnavailable(RuntimeError):
    """The process is absent or its kernel identity cannot be read."""


def process_start_time(pid, proc_root="/proc"):
    path = os.path.join(proc_root, str(pid), "stat")
    try:
        with open(path) as handle:
            raw = handle.read()
    except OSError as exc:
        raise IdentityUnavailable(f"cannot read {path}: {exc}") from exc
    close = raw.rfind(")")
    if close < 0:
        raise IdentityUnavailable(f"cannot parse {path}: no command terminator")
    fields = raw[close + 2:].split()
    # fields[0] is proc stat field 3 (state), so field 22 (starttime) is 19.
    if len(fields) <= 19:
        raise IdentityUnavailable(f"cannot parse {path}: only {len(fields) + 2} fields")
    try:
        return int(fields[19])
    except ValueError as exc:
        raise IdentityUnavailable(f"cannot parse {path} starttime") from exc


def signal_if_identity(pid, expected_start_time, signum, *,
                       read_identity=process_start_time,
                       open_pidfd=os.pidfd_open,
                       send_signal=None,
                       close_pidfd=os.close):
    """Return True only after signaling a pidfd for the expected process."""
    if send_signal is None:
        send_signal = lambda fd, sig: signal.pidfd_send_signal(fd, sig)
    try:
        if read_identity(pid) != expected_start_time:
            return False
        pidfd = open_pidfd(pid)
    except (IdentityUnavailable, ProcessLookupError):
        return False
    try:
        # The PID can exit and be reused between the first read and pidfd_open.
        # Re-read after opening so a pidfd for its replacement is never used.
        try:
            if read_identity(pid) != expected_start_time:
                return False
        except IdentityUnavailable:
            return False
        try:
            send_signal(pidfd, signum)
        except ProcessLookupError:
            return False
        return True
    finally:
        close_pidfd(pidfd)


def main(argv=None):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    identity = sub.add_parser("identity")
    identity.add_argument("pid", type=int)
    send = sub.add_parser("signal")
    send.add_argument("pid", type=int)
    send.add_argument("start_time", type=int)
    send.add_argument("signal", type=int)
    args = parser.parse_args(argv)
    if args.command == "identity":
        try:
            print(process_start_time(args.pid))
        except IdentityUnavailable as exc:
            parser.exit(3, f"{exc}\n")
        return 0
    try:
        sent = signal_if_identity(args.pid, args.start_time, args.signal)
    except (OSError, ValueError) as exc:
        parser.exit(2, f"cannot signal owned process: {exc}\n")
    return 0 if sent else 3


if __name__ == "__main__":
    raise SystemExit(main())
