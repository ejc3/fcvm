"""Relaunch timing: stop a renderer VM, start it again, and time the first frame.

    python3 relaunch.py FCVM NAME GPU TAG [runs] [height]

Each run: SIGTERM the VM's fcvm process and wait for it to exit, then start
`fcvm podman run` (a snapshot cache hit after the first run) and connect to the
renderer as soon as its address answers. Prints, per run, the time from the
start command to the first decoded-ready frame, and the renderer's own
process-start-to-first-frame numbers from /health.
"""
import json
import os
import signal
import socket
import subprocess
import sys
import time

import fleetclient as fc

fcvm, name, gpu, tag = sys.argv[1:5]
runs = int(sys.argv[5]) if len(sys.argv) > 5 else 3
h = int(sys.argv[6]) if len(sys.argv) > 6 else 240


def vms():
    return json.loads(subprocess.run([fcvm, "ls", "--json"], capture_output=True, text=True).stdout or "[]")


children = {}


def stop(name):
    for v in vms():
        if v.get("name") == name and v.get("pid"):
            try:
                os.kill(v["pid"], signal.SIGTERM)
            except ProcessLookupError:
                pass
            if v["pid"] in children:  # our own child: reap it
                children.pop(v["pid"]).wait()
                continue
            while True:
                try:
                    os.kill(v["pid"], 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)


print("run,start_to_first_frame_s,renderer_process_to_first_sent_ms,renderer_gpu_ready_ms")
for r in range(runs):
    stop(name)
    log = open(f"/mnt/work/{name}-relaunch-{r}.log", "w")
    t0 = time.time()
    p = subprocess.Popen([fcvm, "podman", "run", "--name", name, "--kernel-profile", "gpu", "--gpu", gpu,
                          "--publish", "8080:8080", f"localhost/fcvm-renderer:{tag}"],
                         stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    children[p.pid] = p
    ip = None
    while ip is None:
        for v in vms():
            if v.get("name") == name and (v.get("config") or {}).get("network", {}).get("loopback_ip"):
                ip = v["config"]["network"]["loopback_ip"]
        if p.poll() is not None:
            sys.exit(f"fcvm exited with {p.returncode}")
        time.sleep(0.02)
    first = None
    while first is None:
        try:
            _, fr = fc.frames(ip, 8080, h, count=1, timeout=10)
            first = time.time()
        except (OSError, ConnectionError):
            time.sleep(0.02)
    hl = fc.health(ip, 8080)
    print(f"{r},{first - t0:.2f},{hl['startup_ms']['first_frame_sent']},{hl['startup_ms']['gpu_ready']}", flush=True)
stop(name)
