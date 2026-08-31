#!/usr/bin/env python3
"""Keep the corpus replay lease until its privileged server has finalized."""

import argparse
import errno
import fcntl
import os
import signal
import stat
import subprocess
import sys
import tempfile
import time


def publish(path, contents):
    """Atomically publish a small guardian record."""
    directory = os.path.dirname(path) or "."
    prefix = "." + os.path.basename(path) + "."
    fd, temporary = tempfile.mkstemp(prefix=prefix, dir=directory)
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(contents)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def shell_status(returncode):
    return returncode if returncode >= 0 else 128 - returncode


def retain_lease_on_signal(_signum, _frame):
    """Leave lifecycle control to the owned FIFO and completion protocol."""


def protect_lease_from_signals():
    """Install caught handlers without an unprotected delivery window."""
    signals = {signal.SIGINT, signal.SIGTERM, signal.SIGHUP}
    previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, signals)
    try:
        for signum in signals:
            signal.signal(signum, retain_lease_on_signal)
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)


def remove_record(path):
    """Remove a prior record before the guarded command can start."""
    try:
        os.unlink(path)
    except FileNotFoundError:
        return
    directory = os.path.dirname(path) or "."
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def read_record(path):
    """Read one bounded regular-file record without following a symlink."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 256:
            raise RuntimeError(
                f"completion record is not a small regular file: {path}")
        chunks = []
        remaining = 257
        while remaining:
            chunk = os.read(fd, remaining)
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        raw = b"".join(chunks)
        if len(raw) > 256:
            raise RuntimeError(f"completion record is too large: {path}")
        return raw.decode("ascii")
    finally:
        os.close(fd)


def live_process_group_peers(process_group, own_pid, proc_root="/proc"):
    """Return live peers that can still arm or finish the root supervisor."""
    peers = []
    for entry in os.listdir(proc_root):
        if not entry.isdigit():
            continue
        pid = int(entry)
        if pid == own_pid:
            continue
        try:
            with open(os.path.join(proc_root, entry, "stat")) as handle:
                raw = handle.read()
        except OSError as exc:
            if exc.errno in (errno.ENOENT, errno.ESRCH):
                continue
            raise
        close = raw.rfind(")")
        fields = raw[close + 2:].split() if close >= 0 else []
        if len(fields) < 3:
            raise RuntimeError(f"cannot parse process identity for {pid}")
        try:
            state = fields[0]
            group = int(fields[2])
        except ValueError as exc:
            raise RuntimeError(
                f"cannot parse process identity for {pid}") from exc
        if group == process_group and state != "Z":
            peers.append(pid)
    return peers


def wait_for_completion(path, token, process_group):
    """Hold the lease until completion, or prove no supervisor was armed."""
    armed = f"armed {token}\n"
    complete = f"complete {token}\n"
    last_problem = None
    while True:
        try:
            record = read_record(path)
            if record == complete:
                return True
            if record == armed:
                last_problem = None
            elif record is not None:
                problem = f"unexpected completion record at {path}"
                if problem != last_problem:
                    print(
                        f"BLOCKED: replay lease guardian: {problem}",
                        file=sys.stderr,
                        flush=True,
                    )
                    last_problem = problem
            else:
                peers = live_process_group_peers(
                    process_group, os.getpid())
                if not peers:
                    return False
                last_problem = None
        except (OSError, RuntimeError, UnicodeError) as exc:
            problem = str(exc)
            if problem != last_problem:
                print(
                    f"BLOCKED: replay lease guardian cannot verify completion: {exc}",
                    file=sys.stderr,
                    flush=True,
                )
                last_problem = problem
        time.sleep(0.01)


def guard(command, lease_fd, control_fd, ready_path, status_path,
          completion_path, completion_token):
    """Detach, retain the lease, and wait through command finalization."""
    status = 125
    try:
        protect_lease_from_signals()
        os.setsid()
        if lease_fd == control_fd:
            raise RuntimeError("lease and control descriptors must differ")
        if (len(completion_token) != 32
                or any(character not in "0123456789abcdef"
                       for character in completion_token)):
            raise RuntimeError(
                "completion token must be 32 lowercase hexadecimal characters")
        os.fstat(lease_fd)
        fcntl.flock(lease_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        os.close(control_fd)
        remove_record(completion_path)
        publish(ready_path, f"{os.getpid()}\n")
        child = subprocess.Popen(command, close_fds=True)
        status = shell_status(child.wait())
        completed = wait_for_completion(
            completion_path, completion_token, os.getpgrp())
        if not completed and status == 0:
            print(
                "FAILED: replay lease guardian: command exited 0 before "
                "a root supervisor armed completion",
                file=sys.stderr,
                flush=True,
            )
            status = 125
    except BaseException as exc:
        print(f"FAILED: replay lease guardian: {exc}", file=sys.stderr, flush=True)
    finally:
        publish(status_path, f"{status}\n")
    return status


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--lease-fd", type=int, required=True)
    parser.add_argument("--control-fd", type=int, required=True)
    parser.add_argument("--ready-path", required=True)
    parser.add_argument("--status-path", required=True)
    parser.add_argument("--completion-path", required=True)
    parser.add_argument("--completion-token", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("a command is required after --")
    return guard(
        command,
        args.lease_fd,
        args.control_fd,
        args.ready_path,
        args.status_path,
        args.completion_path,
        args.completion_token,
    )


if __name__ == "__main__":
    raise SystemExit(main())
