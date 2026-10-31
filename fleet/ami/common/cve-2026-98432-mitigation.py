#!/usr/bin/env python3
"""Interim CVE-2026-98432 mitigation. Own tables only; never restarts networking.

AWS/AlmaLinux/Akamai recommend dropping IPv4 TCP with IP options and disabling
MPTCP. A patched kernel remains the complete fix. OUTPUT also covers locally
generated/loopback IP options; bridge PREROUTING covers bridge-netfilter bypass.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

NAME = "cve_2026_98432"
INSTALLED = Path("/usr/local/sbin/cve-2026-98432-mitigation")
UNIT_NAME = "cve-2026-98432-mitigation.service"
SYSCTL = Path("/etc/sysctl.d/99-cve-2026-98432.conf")
SETTINGS = {"net.mptcp.enabled": "0", "net.ipv4.tcp_migrate_req": "0"}
SYSCTL_TEXT = "# Interim CVE-2026-98432 reduction; IPv4-options filtering is also required.\n-net.mptcp.enabled = 0\n-net.ipv4.tcp_migrate_req = 0\n"
RULES = '''table inet cve_2026_98432 {
 chain prerouting {
  type filter hook prerouting priority -310; policy accept;
  meta nfproto ipv4 meta l4proto tcp ip hdrlength > 5 counter drop
 }
 chain output {
  type filter hook output priority -310; policy accept;
  meta nfproto ipv4 meta l4proto tcp ip hdrlength > 5 counter drop
 }
}
table bridge cve_2026_98432 {
 chain prerouting {
  type filter hook prerouting priority -310; policy accept;
  ether type ip ip protocol tcp ip hdrlength > 5 counter drop
 }
}
'''
UNIT = '''[Unit]
Description=Interim IPv4 TCP IP-options guard for CVE-2026-98432
DefaultDependencies=no
After=systemd-remount-fs.service systemd-modules-load.service systemd-sysctl.service sys-kernel-security.mount
Before=network-pre.target network.target machines.target docker.service containerd.service
Wants=network-pre.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/cve-2026-98432-mitigation --load
RemainAfterExit=yes

[Install]
WantedBy=network-pre.target multi-user.target
'''


class Failed(Exception):
    pass


def require(value, message):
    if not value:
        raise Failed(message)


def run(argv, text=None, check=True):
    result = subprocess.run(argv, input=text, text=True, capture_output=True, timeout=45)
    require(not check or result.returncode == 0, argv[0] + " " + argv[1] + " failed; existing protections were not removed")
    return result


def label_preflight():
    lsm_path = Path("/sys/kernel/security/lsm")
    require(lsm_path.is_file(), "LSM state unavailable: inspect NetLabel/CIPSO before applying an IP-options filter")
    lsm = {name.strip().lower() for name in lsm_path.read_text().split(",")}
    require(not lsm.intersection({"selinux", "smack"}), "SELinux/Smack may require NetLabel/CIPSO: manual security review needed")
    require(not Path("/sys/fs/selinux/enforce").exists() and not Path("/sys/fs/smackfs").exists()
            and not Path("/smack").exists(), "A label-security interface is present: manual review needed")
    configuration = Path("/etc/netlabel.rules")
    require(not configuration.exists() or not any(line.strip() and not line.lstrip().startswith("#")
            for line in configuration.read_text().splitlines()), "NetLabel configuration present: manual review needed")
    tool = shutil.which("netlabelctl")
    if tool:
        for arguments in (("cipsov4", "list"), ("map", "list")):
            result = run([tool, *arguments], check=False)
            require(result.returncode == 0 and not result.stdout.strip(), "NetLabel/CIPSO state is nonempty or unknown: manual review needed")


def preflight():
    require(os.geteuid() == 0, "Run this administration helper as root")
    for tool in ("nft", "sysctl", "systemctl"):
        require(shutil.which(tool), tool + " is missing; no package installation was attempted")
    label_preflight()


def atomic_file(path, content, mode):
    require(path.parent.is_dir() and not path.parent.is_symlink(), "Unsafe destination directory")
    if path.exists() or path.is_symlink():
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_uid == 0 and not info.st_mode & 0o022,
                "Destination is not a root-owned safe regular file")
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            os.fchmod(output.fileno(), mode)
            os.fchown(output.fileno(), 0, 0)
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def load():
    preflight()
    # Delete/recreate ONLY the two task-owned tables in ONE nft transaction. An
    # invalid bridge/inet rule aborts the complete transaction, preserving old rules.
    tables = json.loads(run(["nft", "-j", "list", "tables"]).stdout)["nftables"]
    found = {(item["table"]["family"], item["table"]["name"]) for item in tables if "table" in item}
    remove = "".join("delete table " + family + " " + NAME + "\n" for family in ("inet", "bridge")
                     if (family, NAME) in found)
    transaction = remove + RULES
    run(["nft", "--check", "--file", "-"], transaction)
    run(["nft", "--file", "-"], transaction)
    for key, value in SETTINGS.items():
        path = Path("/proc/sys/" + key.replace(".", "/"))
        if path.exists():
            run(["sysctl", "-q", "-w", key + "=" + value])
            require(path.read_text().strip() == value, "Runtime sysctl verification failed")


def check():
    result = {"status": "PASS", "inet_prerouting": False, "inet_output": False,
              "bridge_prerouting": False, "sysctls": {}}
    for family, chains in (("inet", {"prerouting", "output"}), ("bridge", {"prerouting"})):
        items = json.loads(run(["nft", "-j", "list", "table", family, NAME]).stdout)["nftables"]
        for chain in chains:
            definitions = [x["chain"] for x in items if "chain" in x and x["chain"]["name"] == chain]
            rules = [x["rule"] for x in items if "rule" in x and x["rule"]["chain"] == chain]
            require(len(definitions) == len(rules) == 1 and definitions[0]["hook"] == chain
                    and definitions[0]["prio"] == -310 and any("drop" in x for x in rules[0]["expr"]),
                    "Owned nft rule verification failed")
            result[family + "_" + chain] = True
    for key in SETTINGS:
        path = Path("/proc/sys/" + key.replace(".", "/"))
        result["sysctls"][key] = path.read_text().strip() if path.exists() else "not_present"
        require(result["sysctls"][key] in ("0", "not_present"), "Runtime sysctl is not disabled")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("--apply", "--load", "--check"))
    # Explicit fixed modes also make the installed systemd command unambiguous.
    require(sys.argv[1:] in (["--apply"], ["--load"], ["--check"]), "Use --apply, --load or --check")
    mode = sys.argv[1]
    preflight()
    fd = os.open("/run/cve-2026-98432.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        if mode == "--apply":
            load()
            atomic_file(INSTALLED, Path(__file__).read_bytes(), 0o700)
            atomic_file(SYSCTL, SYSCTL_TEXT.encode(), 0o644)
            atomic_file(Path("/etc/systemd/system") / UNIT_NAME, UNIT.encode(), 0o644)
            run(["systemctl", "daemon-reload"])
            run(["systemctl", "enable", UNIT_NAME])
        elif mode == "--load":
            load()
        result = check()
        result["persistence_installed"] = INSTALLED.is_file() and SYSCTL.is_file()
        print(json.dumps(result, sort_keys=True))
    finally:
        os.close(fd)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"status": "FAIL", "category": str(error) if isinstance(error, Failed)
                          else type(error).__name__}), file=sys.stderr)
        raise SystemExit(1)
