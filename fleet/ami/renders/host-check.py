#!/usr/bin/env python3
"""The renders host image's own check, and the files the build writes from renders.conf.

    host-check.py boot        the running kernel is the -fcvm kernel the build installed, and the
                              boot line is the reference's (RENDERS_BOOT_PARAMS)
    host-check.py host        boot, and the 2 MiB pool, /dev/kvm, the units, the agent's
                              environment, the build record and the pieces the mode runs on
    host-check.py record      write /etc/fcvm-fleet/renders-build.json (the build phase)
    host-check.py agent-env   print the agent's environment file (the build phase)

`boot` and `host` print one JSON object, {"ok": ..., "problems": [...]}, and exit 1 when a
problem is found. Each problem is one short sentence without a secret or an address, which the
image factory copies into its validation record (pipeline_guest.py verify_renders). Run as root.
"""
import glob
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

CONF = Path("/etc/fcvm-fleet/renders.conf")
RECORD = Path("/etc/fcvm-fleet/renders-build.json")
KERNEL = Path("/etc/fcvm-fleet/renders-kernel")
ENV = Path("/etc/fcvm-fleet/renders.env")
POOL = Path("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages")
SNAPSHOTS = Path("/mnt/fcvm-btrfs/snapshots")
UNITS = ("fcvm-fleet-hugepages.service", "fcvm-fleet-store.service", "fcvm-fleet-renders-agent.service")

RECORD_FIELDS = ("fcvm_commit", "kitesurf_commit", "bundle_sha256", "kernel_release", "boot_params", "hugepages")
COMMIT = re.compile(r"[0-9a-f]{40}")
RELEASE = re.compile(r"[0-9]+\.[0-9]+(\.[0-9]+)?-fcvm-[0-9a-f]{12}")


def read_conf(text):
    """renders.conf: KEY=value lines, a value with spaces double-quoted. Nothing else, so the
    file means the same to bash, which sources it, and to this parser."""
    values = {}
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        match = re.fullmatch(r'([A-Z][A-Z0-9_]*)=(?:"([^"$`\\]*)"|([^\s"\'$`\\;&|<>()]*))', line)
        if not match:
            raise ValueError(f"renders.conf line {number} is not KEY=value")
        if match.group(1) in values:
            raise ValueError(f"renders.conf sets {match.group(1)} twice")
        values[match.group(1)] = match.group(2) if match.group(2) is not None else match.group(3)
    return values


def boot_problems(release, expected, cmdline, params, base_keys):
    """Why this boot is not the reference's configuration: the kernel the build installed, and a
    command line that holds every one of `params` and nothing else but parameters whose keys are
    in `base_keys` (what the base image and its boot loader put there). Anything else is named
    by its key alone, since a value could identify the host."""
    problems = []
    if not RELEASE.fullmatch(release):
        problems.append(f"the running kernel {release} is not fcvm's host kernel")
    elif release != expected:
        problems.append(f"the running kernel {release} is not the one the build installed ({expected or 'none recorded'})")
    tokens = cmdline.split()
    wanted = params.split()
    base = set(base_keys.split())
    for param in wanted:
        if param not in tokens:
            problems.append(f"the boot line lacks {param}")
    for token in tokens:
        key = token.split("=", 1)[0]
        if token not in wanted and key not in base:
            problems.append(f"the boot line has {key}, which is neither a base parameter nor one of the reference's")
    return problems


NIC_MODULES_LOAD = Path("/etc/modules-load.d/fcvm-fleet-nic.conf")
NIC_DROPIN = Path("/etc/systemd/system/cloud-init-local.service.d/10-fcvm-fleet-nic.conf")


def nic_load_problems(driver, modules_load, dropin_exists, cloud_init_after, loader, builtin):
    """Why the NIC's driver may bind after cloud-init-local looks for a NIC on the initrdless boot.
    The driver is a module of the arm64 host kernel, so systemd-modules-load must load it
    (`modules_load`, the text of NIC_MODULES_LOAD, None when absent) and must have run this boot
    (`loader`, its LoadState, ActiveState and Result, None when they could not be read), and
    cloud-init-local.service must be ordered after it (NIC_DROPIN, and systemd's resolved `After`,
    `cloud_init_after`, None when it could not be read). A driver built into the kernel
    (modules.builtin) binds during the kernel's start and needs none of it."""
    if not driver:
        return ["no NIC driver to look for: this boot has no default route, or its interface has no driver"]
    module = re.compile(r"(^|/)" + re.escape(driver) + r"\.ko(\.(zst|xz|gz))?$")
    if any(module.search(line.strip()) for line in builtin.splitlines()):
        return []
    problems = []
    if modules_load is None:
        problems.append(f"{NIC_MODULES_LOAD} does not exist: nothing loads the {driver} driver early")
    elif driver not in [line.strip() for line in modules_load.splitlines() if not line.strip().startswith(("#", ";"))]:
        problems.append(f"{NIC_MODULES_LOAD} does not name the {driver} driver")
    if not dropin_exists:
        problems.append(f"{NIC_DROPIN} does not exist")
    if cloud_init_after is None:
        problems.append("systemctl show failed: the order of cloud-init-local.service is unknown")
    elif "systemd-modules-load.service" not in cloud_init_after.split():
        problems.append("cloud-init-local.service is not ordered after systemd-modules-load.service")
    if loader is None:
        problems.append("systemctl show failed: the state of systemd-modules-load.service is unknown")
    else:
        state = {key: loader.get(key, "") for key in ("LoadState", "ActiveState", "Result")}
        if state != {"LoadState": "loaded", "ActiveState": "active", "Result": "success"}:
            problems.append("systemd-modules-load.service did not run this boot ("
                            + ", ".join(f"{k}={v or 'none'}" for k, v in state.items()) + ")")
    return problems


def pool_problems(text, want):
    """Why the 2 MiB pool (nr_hugepages) is not the reservation."""
    value = text.strip()
    if not value.isdigit():
        return ["the 2 MiB pool cannot be read"]
    if int(value) != want:
        return [f"the pool holds {int(value)} of {want} 2 MiB pages"]
    return []


def campaign_urls(path):
    """The pages, in the wall's order: bench/chromium/corpus_campaign.sh's URLS, as
    renders_bench.sh reads them."""
    for line in Path(path).read_text().splitlines():
        match = re.match(r'URLS="(https[^"]*)"', line)
        if match:
            return match.group(1)
    raise ValueError(f"{path} names no URLS")


def agent_args(conf, record, urls):
    """The agent's arguments the image decides: pages mode on, harmless values for the agent's
    required GPU-slot options (renders_bench.sh agent_args uses the same; a host without GPUs
    starts no slot), the host named by its instance id, and the module's options."""
    bench = conf["RENDERS_SRC"] + "/bench/chromium"
    module = [
        ("--out", conf["RENDERS_OUT"]), ("--run-dir", conf["RENDERS_RUN_DIR"]),
        ("--status-file", conf["RENDERS_OUT"] + ".status.json"),
        ("--bench", bench), ("--corpus", bench + "/corpus-live"), ("--urls", urls),
        ("--snapshot-tag", conf["RENDERS_SNAPSHOT_TAG"]),
        ("--kitesurf", conf["RENDERS_KITESURF"]), ("--kitesurf-dist", conf["RENDERS_DIST"]),
        ("--cpus-fcvm", conf["RENDERS_CPUS_FCVM"]), ("--cpus-engines", conf["RENDERS_CPUS_ENGINES"]),
        ("--cpus-harness", conf["RENDERS_CPUS_HARNESS"]),
        ("--max-fcvm", conf["RENDERS_MAX_FCVM"]), ("--max-engines", conf["RENDERS_MAX_ENGINES"]),
        ("--setup", "fcvm.commit=" + record["fcvm_commit"]),
        ("--setup", "kitesurf.commit=" + record["kitesurf_commit"]),
    ]
    words = ["--renders", "--image", "unused", "--base-port", "1", "--host-from-imds"]
    words += ["--renders-arg=" + name + "=" + value for name, value in module]
    for word in words:
        if not word or re.search(r"[\s\"'$`\\]", word):
            raise ValueError(f"agent argument {word!r} cannot go in a systemd environment file")
    return words


def agent_env(conf, record, urls):
    """/etc/fcvm-fleet/renders.env, read by fcvm-fleet-renders-agent.service."""
    return ("# Written at build by fleet/ami/renders/build.sh (host-check.py agent-env) from renders.conf.\n"
            "# The host's own arguments (the coordinator, the access token) are in agent.env, which the\n"
            "# instance's user data writes.\n"
            'RENDERS_AGENT_ARGS="' + " ".join(agent_args(conf, record, urls)) + '"\n'
            "# The frame server's options for viewers. Empty until the frame server exists; its change adds\n"
            "# them here.\n"
            "RENDERS_FRAME_ARGS=\n")


def build_record(*, fcvm_commit, kitesurf_commit, index_js, release, conf):
    """What the build made, for the image factory's manifest and this check."""
    if not COMMIT.fullmatch(fcvm_commit) or not COMMIT.fullmatch(kitesurf_commit):
        raise ValueError("a commit in the build record is not a full commit id")
    if not RELEASE.fullmatch(release):
        raise ValueError(f"the build ran on {release}, not fcvm's host kernel")
    return {"fcvm_commit": fcvm_commit, "kitesurf_commit": kitesurf_commit,
            "bundle_sha256": hashlib.sha256(index_js).hexdigest(), "kernel_release": release,
            "boot_params": conf["RENDERS_BOOT_PARAMS"], "hugepages": int(conf["RENDERS_HUGEPAGES"])}


def kitesurf_rev(conf):
    """The Kitesurf commit the image builds and runs: renders.conf's, the recorded campaign's."""
    rev = conf.get("RENDERS_KITESURF_REV", "")
    if not COMMIT.fullmatch(rev):
        raise ValueError("renders.conf's RENDERS_KITESURF_REV is not a full commit id")
    return rev


def run(args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, timeout=kwargs.pop("timeout", 120), **kwargs)


def git_head(path):
    done = run(["git", "-c", "safe.directory=*", "-C", str(path), "rev-parse", "HEAD"])
    return done.stdout.strip() if done.returncode == 0 else ""


def node_binary(kitesurf):
    found = sorted(glob.glob(os.path.join(kitesurf, "tools", "node-*", "bin", "node")))
    return found[0] if len(found) == 1 else ""


def nic_driver(route, sys_root=Path("/sys")):
    """The driver of the interface the default route uses (`route`: /proc/net/route's text), or ""
    when there is no default route or its interface has no driver."""
    for line in route.splitlines()[1:]:
        fields = line.split()
        if len(fields) > 2 and fields[1] == "00000000":
            try:
                return Path(sys_root, "class/net", fields[0], "device", "driver").resolve(strict=True).name
            except OSError:
                return ""
    return ""


def systemctl_show(unit, properties, runner=None):
    """`properties` of `unit` as systemd reports them ({name: value}), or None when systemctl
    exits non-zero: a read that failed is never taken for an answer."""
    args = ["systemctl", "show", unit]
    for name in properties:
        args += ["-p", name]
    done = (runner or run)(args)
    if done.returncode != 0:
        return None
    return dict(line.split("=", 1) for line in done.stdout.splitlines() if "=" in line)


def check_boot(conf):
    release = os.uname().release
    expected = KERNEL.read_text().strip() if KERNEL.is_file() else ""
    problems = boot_problems(release, expected, Path("/proc/cmdline").read_text(), conf["RENDERS_BOOT_PARAMS"],
                             conf["RENDERS_BOOT_BASE_KEYS"])
    after = systemctl_show("cloud-init-local.service", ("After",))
    loader = systemctl_show("systemd-modules-load.service", ("LoadState", "ActiveState", "Result"))
    builtin = Path("/lib/modules", release, "modules.builtin")
    return problems + nic_load_problems(nic_driver(Path("/proc/net/route").read_text()),
                                        NIC_MODULES_LOAD.read_text() if NIC_MODULES_LOAD.is_file() else None,
                                        NIC_DROPIN.is_file(), after.get("After", "") if after is not None else None,
                                        loader, builtin.read_text() if builtin.is_file() else "")


def check_host(conf):
    problems = check_boot(conf)
    want = int(conf["RENDERS_HUGEPAGES"])
    problems += pool_problems(POOL.read_text() if POOL.exists() else "", want)
    kvm = Path("/dev/kvm")
    if not kvm.is_char_device():
        problems.append("/dev/kvm is missing")
    uffd = Path("/dev/userfaultfd")
    if not uffd.is_char_device() or uffd.stat().st_mode & 0o777 != 0o666:
        problems.append("/dev/userfaultfd is missing or not mode 0666")
    for unit in UNITS:
        if run(["systemctl", "is-enabled", unit]).stdout.strip() != "enabled":
            problems.append(f"{unit} is not enabled")
    if run(["systemctl", "is-active", UNITS[0]]).stdout.strip() != "active":
        problems.append(f"{UNITS[0]} did not reserve the pool this boot")
    user = conf["RENDERS_USER"]
    if run(["id", "-u", user]).stdout.strip() != conf["RENDERS_UID"]:
        problems.append(f"{user}'s uid is not {conf['RENDERS_UID']}, which the agent's unit names")
    for binary in ("/usr/local/bin/fcvm", "/usr/local/bin/fc-agent"):
        if not os.access(binary, os.X_OK):
            problems.append(f"{binary} is missing")
    try:
        record = json.loads(RECORD.read_text())
        assert set(record) == set(RECORD_FIELDS)
    except (OSError, ValueError, AssertionError):
        return problems + ["the build record is missing or malformed"]
    src, kitesurf, dist = conf["RENDERS_SRC"], conf["RENDERS_KITESURF"], conf["RENDERS_DIST"]
    if git_head(src) != record["fcvm_commit"]:
        problems.append("the checkout is not the commit the build record names")
    try:
        pinned = kitesurf_rev(conf)
    except ValueError:
        pinned = ""
    if record["kitesurf_commit"] != pinned or git_head(Path(kitesurf, "src")) != pinned:
        problems.append("Kitesurf is not at renders.conf's RENDERS_KITESURF_REV")
    try:
        bundle = hashlib.sha256(Path(dist, "index.js").read_bytes()).hexdigest()
    except OSError:
        bundle = ""
    if bundle != record["bundle_sha256"]:
        problems.append("the engine bundle is missing or not the one the build made")
    if record["kernel_release"] != os.uname().release:
        problems.append("the build record names another kernel")
    if record["boot_params"] != conf["RENDERS_BOOT_PARAMS"] or record["hugepages"] != want:
        problems.append("the build record's boot line or pool differs from renders.conf")
    try:
        urls = campaign_urls(Path(src, "bench/chromium/corpus_campaign.sh"))
        wanted_env = agent_env(conf, record, urls)
    except (OSError, ValueError):
        wanted_env = None
    if not ENV.is_file() or ENV.read_text() != wanted_env:
        problems.append("renders.env is not what renders.conf and the build record give")
    for path in (Path(src, "bench/chromium"), Path(src, "bench/chromium/corpus-live")):
        if not path.is_dir():
            problems.append(f"{path} is missing")
    if not Path(SNAPSHOTS, conf["RENDERS_SNAPSHOT_TAG"], "config.json").is_file():
        problems.append("the module's snapshot is not in the store")
    node = node_binary(kitesurf)
    seal = run(["runuser", "-u", user, "--", "python3", src + "/bench/chromium/kitesurf_seal.py", "verify",
                "--src", kitesurf + "/src", "--seal", kitesurf + "/RUNTIME.json", "--rev", pinned,
                "--node-bin", node], timeout=1800) if node and pinned else None
    if seal is None or seal.returncode != 0:
        problems.append("the Kitesurf tree is not the one sealed after its build")
    return problems


def main(argv):
    mode = argv[1] if len(argv) > 1 else ""
    # The image's own conf, which the build installed; the recipe's copy is not the image's.
    if not CONF.is_file():
        raise SystemExit(f"host-check.py: {CONF} is missing: this is not a renders image (or its prep did not run)")
    conf = read_conf(CONF.read_text())
    if mode == "record":
        src = conf["RENDERS_SRC"]
        record = build_record(fcvm_commit=git_head(src), kitesurf_commit=kitesurf_rev(conf),
            index_js=Path(conf["RENDERS_DIST"], "index.js").read_bytes(), release=os.uname().release, conf=conf)
        if git_head(Path(conf["RENDERS_KITESURF"], "src")) != record["kitesurf_commit"]:
            raise SystemExit("Kitesurf's checkout is not at renders.conf's RENDERS_KITESURF_REV")
        RECORD.write_text(json.dumps(record, indent=1, sort_keys=True) + "\n")
        print(json.dumps(record, sort_keys=True))
        return 0
    if mode == "agent-env":
        record = json.loads(RECORD.read_text())
        sys.stdout.write(agent_env(conf, record, campaign_urls(Path(conf["RENDERS_SRC"], "bench/chromium/corpus_campaign.sh"))))
        return 0
    if mode not in ("boot", "host"):
        raise SystemExit("usage: host-check.py boot|host|record|agent-env")
    problems = check_boot(conf) if mode == "boot" else check_host(conf)
    for problem in problems:
        print("renders host check: " + problem, file=sys.stderr)
    print(json.dumps({"ok": not problems, "problems": problems}))
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
