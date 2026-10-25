"""aggrun.py's pieces that need no image library: reading a frame stream, finding the
aggregator's process and its CPU time, and waiting for a stream to reach a frame. Kept
apart so fleet/tools/test_tools.py can test them without Pillow, ffmpeg or a GPU."""
import json
import os
import subprocess
import sys
import threading
import time

import fleetclient as fc


def hostport(url):
    hp = url[len("ws://"):].split("/")[0]
    h, p = hp.rsplit(":", 1)
    return h, int(p)


def proc_cpu(pid):
    """utime+stime in seconds of pid and its children (the ffmpeg processes, which any of
    its threads may have started)."""
    tck = os.sysconf("SC_CLK_TCK")
    kids = []
    for task in os.listdir(f"/proc/{pid}/task"):
        try:
            kids += [int(c) for c in open(f"/proc/{pid}/task/{task}/children").read().split()]
        except OSError:
            pass
    total = 0.0
    for p in [pid] + kids:
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            total += (int(f[11]) + int(f[12])) / tck
        except OSError:
            pass
    return total, 1 + len(kids)


def pid_listening(port):
    out = subprocess.run(["ss", "-ltnpH", f"sport = :{port}"], capture_output=True, text=True).stdout
    for part in out.split("pid=")[1:]:
        return int(part.split(",")[0])
    return None


def require_listening_pid(port):
    """The pid listening on `port`, or exit saying why there is none. `ss` names no
    process another user owns, nor one listening on another port, and the CPU readings
    need the pid."""
    pid = pid_listening(port)
    if pid is None:
        sys.exit(f"no process of this user listens on port {port}: run aggrun.py as the "
                 "aggregator's user on its host, at the port its frame URL names")
    return pid


class Recorder:
    """A frame stream: receive time per t, bytes, header, (optionally) frames kept."""

    def __init__(self, host, port, h, keep_until=None):
        self.host, self.port, self.h, self.keep_until = host, port, h, keep_until
        self.recv, self.bytes, self.header, self.frames, self.stop = {}, 0, None, [], False
        self.t = threading.Thread(target=self.run, daemon=True)
        self.t.start()

    def run(self):
        s = fc.Stream(self.host, self.port, self.h)
        try:
            while not self.stop:
                op, pl = s.recv()
                now = time.time()
                if op == 1:
                    self.header = json.loads(pl)
                elif op == 2:
                    m, au = fc.parse_fcvf(pl)
                    self.recv.setdefault(m["t"], now)
                    self.bytes += len(pl)
                    if self.keep_until is not None and (not self.frames or self.frames[-1][0]["t"] < self.keep_until):
                        self.frames.append((m, au))
        finally:
            s.close()


def wait_for_t(rec, t, timeout):
    """Wait until `rec` has kept a frame at scene time `t` or later. Exits when its stream
    ended (the recorder's thread is gone) or `timeout` seconds passed first."""
    deadline = time.time() + timeout
    while not rec.frames or rec.frames[-1][0]["t"] < t:
        if not rec.t.is_alive():
            sys.exit(f"the aggregate stream ended before t={t}")
        if time.time() > deadline:
            sys.exit(f"the aggregate stream did not reach t={t} within {timeout} s")
        time.sleep(0.05)
