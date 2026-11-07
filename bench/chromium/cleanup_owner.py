#!/usr/bin/env python3
"""Finish a root phase supervisor's cleanup when the supervisor itself dies.

corpus_extra.sh's replay phase runs as root under phase_supervisor.py, which
publishes `armed <token>` before it starts the phase (stopping dnsmasq, then
corpus_serve on ports 53, 80 and 443) and `complete <token>` only after it has
drained the phase's process set and its finalizer restored dnsmasq.
serve_guardian.py holds the host-wide replay lease until it reads
`complete <token>`. A supervisor destroyed after `armed` (SIGKILL, an OOM kill)
publishes nothing more, so the lease stayed held for good while a detached
replay process could still own the ports and dnsmasq stayed stopped (#895).

This process is the supervisor's parent and a child subreaper, so its lifetime
does not depend on the supervisor's. When the supervisor exits it:

- returns at once when the supervisor published `complete <token>`;
- otherwise drains the phase's processes, which the kernel reparents to it as
  the nearest subreaper (kernel identity, not command matching), with the
  supervisor's own drain, SIGTERM then SIGKILL, until it has no child left;
- runs the phase finalizer (host_resource_finalizer.py, which restarts dnsmasq
  when it was active and requires `systemctl is-active` to say so);
- publishes `complete <token>` only after both, so the guardian releases the
  lease only then.

Any step it cannot prove leaves the record at `armed` and the lease held. A
record that names another token or state is never overwritten.
"""

from __future__ import annotations

import argparse
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
# This runs as root from corpus_extra's sealed runtime bundle, whose files and
# directory are mode 0555 and checked at the campaign's end. Root writes past
# 0555, so importing phase_supervisor would leave a __pycache__ directory in
# the bundle and fail that check ("runtime directory './__pycache__' has mode
# 0755"); sudo drops PYTHONDONTWRITEBYTECODE from the environment.
sys.dont_write_bytecode = True

import phase_supervisor  # noqa: E402

RECORD_LIMIT = 256


def read_completion(path, token):
    """'armed', 'complete', None (no record) or 'foreign' (anything else)."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    try:
        raw = os.read(fd, RECORD_LIMIT + 1)
    finally:
        os.close(fd)
    for state in phase_supervisor.COMPLETION_STATES:
        if raw == f"{state} {token}\n".encode():
            return state
    return "foreign"


def own(argv, completion_path, token, finalizer, finalizer_timeout,
        term_grace, kill_grace):
    phase_supervisor.become_subreaper()
    drained = [False]
    try:
        status = phase_supervisor._supervise_armed(
            argv, term_grace=term_grace, kill_grace=kill_grace,
            return_command_status_on_signal=True, drain_certificate=drained)
    except BaseException as exc:
        print(f"FAILED: cleanup owner: the supervisor's process set was not "
              f"drained: {exc}", file=sys.stderr, flush=True)
        return 125
    state = read_completion(completion_path, token)
    if state == "complete":
        return status
    if state == "foreign":
        print(f"FAILED: cleanup owner: {completion_path} names another run; "
              "leaving it and the lease as they are", file=sys.stderr, flush=True)
        return 125
    print(f"FAILED: root supervisor exited {status} without completing "
          f"(record {state or 'absent'}); its process set is drained, "
          "finishing its finalizer", file=sys.stderr, flush=True)
    if not drained[0]:
        return 125
    finalized = [False]
    try:
        result = phase_supervisor._supervise_armed(
            [finalizer], term_grace=term_grace, kill_grace=kill_grace,
            command_timeout=finalizer_timeout,
            return_command_status_on_signal=True, drain_certificate=finalized)
    except BaseException as exc:
        print(f"FAILED: cleanup owner: finalizer: {exc}", file=sys.stderr, flush=True)
        return 125
    if result != 0 or not finalized[0]:
        print(f"FAILED: cleanup owner: finalizer exited {result}; the lease stays "
              "held", file=sys.stderr, flush=True)
        return 125
    if read_completion(completion_path, token) != state:
        print(f"FAILED: cleanup owner: {completion_path} changed during the "
              "cleanup; leaving it", file=sys.stderr, flush=True)
        return 125
    phase_supervisor.publish_completion_state(completion_path, token, "complete")
    return status if status != 0 else 125


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--completion-path", required=True)
    parser.add_argument("--completion-token", required=True)
    parser.add_argument("--finalizer", required=True)
    parser.add_argument("--finalizer-timeout", type=float,
                        default=phase_supervisor.FINALIZER_TIMEOUT_SECONDS)
    parser.add_argument("--term-grace", type=float, default=phase_supervisor.GRACE_SECONDS)
    parser.add_argument("--kill-grace", type=float, default=phase_supervisor.KILL_REAP_SECONDS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("the supervisor command is required after --")
    if (len(args.completion_token) != 32
            or any(c not in "0123456789abcdef" for c in args.completion_token)):
        parser.error("--completion-token must be 32 lowercase hexadecimal characters")
    if not 0 < args.finalizer_timeout < float("inf"):
        parser.error("--finalizer-timeout must be positive and finite")
    return own(command, args.completion_path, args.completion_token,
               args.finalizer, args.finalizer_timeout, args.term_grace, args.kill_grace)


if __name__ == "__main__":
    raise SystemExit(main())
