#!/usr/bin/env python3
"""Run one benchmark phase while its process-group identity cannot be reused."""

from __future__ import annotations

import argparse
import ctypes
import os
import select
import selectors
import signal
import stat
import subprocess
import sys
import time


PR_SET_CHILD_SUBREAPER = 36
PR_GET_CHILD_SUBREAPER = 37
PR_SET_PDEATHSIG = 1
PR_GET_PDEATHSIG = 2
GRACE_SECONDS = 5.0
KILL_REAP_SECONDS = 30.0


def get_process_control(option):
    libc = ctypes.CDLL(None, use_errno=True)
    value = ctypes.c_int()
    if libc.prctl(option, ctypes.byref(value), 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    return value.value


def set_process_control(option, value):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(option, value, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def become_subreaper():
    set_process_control(PR_SET_CHILD_SUBREAPER, 1)


def arm_parent_death(expected_parent):
    if expected_parent <= 1 or os.getppid() != expected_parent:
        raise RuntimeError("expected parent is already gone")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_PDEATHSIG, signal.SIGTERM, 0, 0, 0) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    if os.getppid() != expected_parent:
        raise RuntimeError("parent changed while arming parent-death signal")


def read_process_stat(pid, proc_root="/proc"):
    """Read scheduler identity without faulting another process's address space."""
    path = os.path.join(proc_root, str(pid), "stat")
    try:
        with open(path) as handle:
            raw = handle.read()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise RuntimeError(f"cannot read {path}: {exc}") from exc
    close = raw.rfind(")")
    fields = raw[close + 2:].split() if close >= 0 else []
    if len(fields) < 20:
        raise RuntimeError(f"cannot parse process identity from {path}")
    try:
        return {
            "pid": int(pid),
            "state": fields[0],
            "ppid": int(fields[1]),
            "pgid": int(fields[2]),
            "starttime": int(fields[19]),
        }
    except ValueError as exc:
        raise RuntimeError(f"cannot parse process identity from {path}") from exc


def direct_live_children(parent_pid, proc_root="/proc"):
    """Return live direct children, whose identities this parent keeps pinned."""
    children = []
    try:
        entries = os.listdir(proc_root)
    except OSError as exc:
        raise RuntimeError(f"cannot enumerate {proc_root}: {exc}") from exc
    for entry in entries:
        if not entry.isdigit():
            continue
        identity = read_process_stat(int(entry), proc_root)
        if (identity is not None and identity["ppid"] == parent_pid
                and identity["state"] != "Z"):
            children.append(identity)
    return children


def signal_direct_children(children, parent_pid, signum, proc_root="/proc"):
    """Signal only identities still directly owned by this supervisor."""
    for snapshot in children:
        try:
            pidfd = os.pidfd_open(snapshot["pid"])
        except ProcessLookupError:
            continue
        try:
            current = read_process_stat(snapshot["pid"], proc_root)
            if (current is None or current["state"] == "Z"
                    or current["ppid"] != parent_pid
                    or current["starttime"] != snapshot["starttime"]):
                continue
            try:
                signal.pidfd_send_signal(pidfd, signum)
            except ProcessLookupError:
                pass
        finally:
            os.close(pidfd)


def status_code(info):
    if info.si_code == os.CLD_EXITED:
        return info.si_status
    return 128 + info.si_status


def reap_available():
    while True:
        try:
            pid, _status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def drain(fd):
    while True:
        try:
            if not os.read(fd, 4096):
                return
        except BlockingIOError:
            return


def open_control_path(path):
    """Open a precreated FIFO before any supervised process can start."""
    fd = os.open(
        path,
        os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC | os.O_NOFOLLOW,
    )
    try:
        if not stat.S_ISFIFO(os.fstat(fd).st_mode):
            raise RuntimeError(f"phase control path is not a FIFO: {path}")
        return fd
    except BaseException:
        os.close(fd)
        raise


def consume_events(selector, events, wake_read, control_fd, pending):
    """Translate signal-pipe and one-shot control-FIFO readiness to signals."""
    if any(key.data == "signal" for key, _mask in events):
        drain(wake_read)
    if control_fd is None or not any(
            key.data == "control" for key, _mask in events):
        return
    try:
        command = os.read(control_fd, 4096)
    except BlockingIOError:
        return
    try:
        selector.unregister(control_fd)
    except KeyError:
        pass
    if not command:
        pending.append(signal.SIGTERM)
        return
    mapping = {
        ord("T"): signal.SIGTERM,
        ord("I"): signal.SIGINT,
        ord("H"): signal.SIGHUP,
    }
    requested = [mapping.get(byte) for byte in command]
    if any(signum is None for signum in requested):
        raise RuntimeError("phase control FIFO received an invalid command")
    pending.extend(requested)


def pending_termination(pending):
    return next(
        (signum for signum in pending
         if signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)),
        None,
    )


def format_identities(identities):
    return ",".join(
        f"{item['pid']}@{item['starttime']}({item['state']})"
        for item in identities
    )


def wait_for_phase_leader(selector, child, pending, wake_read,
                          term_grace, kill_grace, command_timeout=None,
                          control_fd=None):
    """Wait for a WNOWAIT-pinned leader with bounded TERM and KILL stages."""
    external_signal = None
    command_deadline = (None if command_timeout is None else
                        time.monotonic() + command_timeout)
    escalation_deadline = None
    timed_out = False
    killed = False
    while True:
        deadline = (escalation_deadline if escalation_deadline is not None
                    else command_deadline)
        timeout = None if deadline is None else max(0.0, deadline - time.monotonic())
        events = selector.select(timeout)
        if not events and deadline is not None:
            if escalation_deadline is None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                timed_out = True
                command_deadline = None
                escalation_deadline = time.monotonic() + term_grace
            elif killed:
                identity = read_process_stat(child.pid)
                detail = format_identities([identity]) if identity else str(child.pid)
                raise RuntimeError(
                    f"phase leader survived SIGKILL past its reap deadline: {detail}")
            else:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                killed = True
                escalation_deadline = time.monotonic() + kill_grace
            continue
        consume_events(selector, events, wake_read, control_fd, pending)
        if events:
            termination = pending_termination(pending)
            if termination is not None and external_signal is None:
                try:
                    os.killpg(child.pid, termination)
                except ProcessLookupError:
                    pass
                external_signal = termination
                command_deadline = None
                escalation_deadline = time.monotonic() + term_grace
        if any(key.data == "leader" for key, _mask in events):
            info = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
            return info, external_signal, timed_out


def drain_adopted_children(selector, wake_read, pending, external_signal,
                           parent_pid, term_grace, kill_grace,
                           control_fd=None):
    """Drain every adopted phase descendant without a PID-reuse signal race."""
    survivors = direct_live_children(parent_pid)
    escaped = bool(survivors)
    if escaped:
        print(
            "FAILED: phase leader exited with live descendants "
            + format_identities(survivors),
            file=sys.stderr,
            flush=True,
        )
    deadline = time.monotonic() + term_grace if survivors else None
    killed = False
    while survivors:
        signum = (signal.SIGKILL if killed else
                  (external_signal or signal.SIGTERM))
        signal_direct_children(survivors, parent_pid, signum)
        timeout = max(0.0, deadline - time.monotonic())
        events = selector.select(timeout)
        if not events:
            if killed:
                current = direct_live_children(parent_pid)
                raise RuntimeError(
                    "phase descendants survived SIGKILL past their reap deadline: "
                    + format_identities(current or survivors))
            killed = True
            deadline = time.monotonic() + kill_grace
        else:
            consume_events(selector, events, wake_read, control_fd, pending)
            termination = pending_termination(pending)
            if termination is not None and external_signal is None:
                external_signal = termination
        survivors = direct_live_children(parent_pid)
    return escaped, external_signal


def wait_child_exit_wnowait(child, wake_read, timeout):
    """Wait to pin a direct child's exited identity without reaping it."""
    deadline = time.monotonic() + timeout
    while True:
        info = os.waitid(
            os.P_PID, child.pid,
            os.WEXITED | os.WNOWAIT | os.WNOHANG,
        )
        if info is not None:
            return info
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        ready, _writable, _exceptional = select.select(
            [wake_read], [], [], remaining)
        if ready:
            drain(wake_read)


def emergency_cleanup(child, wake_read, term_grace, kill_grace):
    """Best-effort bounded cleanup after the supervisor itself faults."""
    info = os.waitid(
        os.P_PID, child.pid,
        os.WEXITED | os.WNOWAIT | os.WNOHANG,
    )
    if info is None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        info = wait_child_exit_wnowait(child, wake_read, term_grace)
    if info is None:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        info = wait_child_exit_wnowait(child, wake_read, kill_grace)
    if info is None:
        identity = read_process_stat(child.pid)
        detail = format_identities([identity]) if identity else str(child.pid)
        raise RuntimeError(
            f"emergency cleanup leader survived SIGKILL: {detail}")

    emergency_selector = selectors.DefaultSelector()
    try:
        emergency_selector.register(wake_read, selectors.EVENT_READ, "signal")
        drain_adopted_children(
            emergency_selector, wake_read, [], signal.SIGTERM, os.getpid(),
            term_grace, kill_grace)
    finally:
        emergency_selector.close()
    _pid, wait_status = os.waitpid(child.pid, 0)
    child.returncode = os.waitstatus_to_exitcode(wait_status)
    reap_available()


def _supervise_armed(argv, term_grace=None, kill_grace=None, pass_fds=(),
                     command_timeout=None, control_path=None,
                     return_command_status_on_signal=False):
    term_grace = GRACE_SECONDS if term_grace is None else term_grace
    kill_grace = KILL_REAP_SECONDS if kill_grace is None else kill_grace
    wake_read, wake_write = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
    pending = []

    def remember(signum, _frame):
        pending.append(signum)

    previous_wakeup = signal.set_wakeup_fd(wake_write)
    previous_handlers = {}
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        previous_handlers[signum] = signal.signal(signum, remember)
    previous_handlers[signal.SIGCHLD] = signal.signal(signal.SIGCHLD, lambda *_: None)
    child = None
    pidfd = None
    selector = None
    control_fd = None
    try:
        if control_path is not None:
            control_fd = open_control_path(control_path)
        child = subprocess.Popen(argv, start_new_session=True,
                                 pass_fds=tuple(pass_fds))
        pidfd = os.pidfd_open(child.pid)
        selector = selectors.DefaultSelector()
        selector.register(wake_read, selectors.EVENT_READ, "signal")
        selector.register(pidfd, selectors.EVENT_READ, "leader")
        if control_fd is not None:
            selector.register(control_fd, selectors.EVENT_READ, "control")
        leader_info, external_signal, timed_out = wait_for_phase_leader(
            selector, child, pending, wake_read,
            term_grace, kill_grace, command_timeout, control_fd)

        selector.unregister(pidfd)
        result = status_code(leader_info)
        parent_pid = os.getpid()
        escaped, external_signal = drain_adopted_children(
            selector, wake_read, pending, external_signal, parent_pid,
            term_grace, kill_grace, control_fd)

        _pid, wait_status = os.waitpid(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(wait_status)
        reap_available()
        if (external_signal in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
                and not return_command_status_on_signal):
            return 128 + external_signal
        if timed_out:
            return 124
        if escaped and result == 0:
            return 1
        return result
    finally:
        if child is not None and child.returncode is None:
            try:
                emergency_cleanup(
                    child, wake_read, term_grace, kill_grace)
            except BaseException as cleanup_error:
                print(
                    f"FAILED: phase supervisor emergency cleanup: {cleanup_error}",
                    file=sys.stderr,
                    flush=True,
                )
        if selector is not None:
            selector.close()
        if pidfd is not None:
            os.close(pidfd)
        if control_fd is not None:
            os.close(control_fd)
        signal.set_wakeup_fd(previous_wakeup)
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)
        os.close(wake_read)
        os.close(wake_write)


def supervise(argv, expected_parent, term_grace=None, kill_grace=None,
              pass_fds=(), command_timeout=None, control_path=None,
              return_command_status_on_signal=False):
    previous_subreaper = get_process_control(PR_GET_CHILD_SUBREAPER)
    previous_pdeathsig = get_process_control(PR_GET_PDEATHSIG)
    try:
        arm_parent_death(expected_parent)
        become_subreaper()
        return _supervise_armed(
            argv, term_grace, kill_grace, pass_fds, command_timeout,
            control_path, return_command_status_on_signal,
        )
    finally:
        # _supervise_armed drains and reaps the complete adopted process set
        # before returning. Restore process-wide controls only after that
        # lifecycle boundary, and restore both even if one prctl fails.
        try:
            set_process_control(
                PR_SET_CHILD_SUBREAPER, previous_subreaper)
        finally:
            set_process_control(PR_SET_PDEATHSIG, previous_pdeathsig)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected-parent", required=True, type=int)
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--term-grace", type=float, default=GRACE_SECONDS)
    parser.add_argument("--kill-grace", type=float, default=KILL_REAP_SECONDS)
    parser.add_argument("--pass-fd", action="append", type=int, default=[])
    parser.add_argument("--control-path")
    parser.add_argument("--return-command-status-on-signal",
                        action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a phase command is required after --")
    if (not 0 <= args.term_grace < float("inf")
            or not 0 <= args.kill_grace < float("inf")
            or (args.timeout is not None
                and not 0 < args.timeout < float("inf"))):
        parser.error("supervisor deadlines must be finite and nonnegative")
    try:
        return supervise(
            command, args.expected_parent, args.term_grace, args.kill_grace,
            args.pass_fd, args.timeout, args.control_path,
            args.return_command_status_on_signal)
    except (OSError, RuntimeError) as exc:
        print(f"FAILED: phase supervisor: {exc}", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
