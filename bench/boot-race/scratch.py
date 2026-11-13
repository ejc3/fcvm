"""Convert only the fresh pbox instance-store mount to benchmark Btrfs."""

import argparse
from dataclasses import dataclass
import fcntl
import os
from pathlib import Path
import pwd
import re
import stat
import subprocess


LOCAL = Path('/mnt/nvme')
SCRATCH = Path('/mnt/fcvm-btrfs')
SYS_BLOCK = Path('/sys/dev/block')
LOCAL_MODEL = 'Amazon EC2 NVMe Instance Storage'
LABEL = 'boot-race-local'


@dataclass(frozen=True)
class Mount:
    number: str
    root: str
    path: Path
    fstype: str
    source: str
    readonly: bool = False
    super_readonly: bool = False


@dataclass(frozen=True)
class Device:
    path: Path
    number: str
    members: tuple[str, ...]


def need(condition, message):
    if not condition:
        raise ValueError(message)


def run(*argv):
    return subprocess.run(argv, check=True, text=True, stdout=subprocess.PIPE,
                          env={**os.environ, 'LC_ALL': 'C'}).stdout


def mount_table():
    def unescape(value):
        return re.sub(r'\\([0-7]{3})', lambda m: chr(int(m[1], 8)), value)

    mounts = []
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        left, right = (part.split() for part in line.split(' - ', 1))
        mounts.append(Mount(left[2], unescape(left[3]), Path(unescape(left[4])),
                            right[0], unescape(right[1]),
                            'ro' in left[5].split(','), 'ro' in right[2].split(',')))
    return mounts


def block_number(path):
    info = path.stat()
    need(stat.S_ISBLK(info.st_mode), f'{path} is not a block device')
    return f'{os.major(info.st_rdev)}:{os.minor(info.st_rdev)}'


def read(path):
    return path.read_text().strip()


def whole_device(node):
    return not (node / 'partition').exists() and not any(
        (child / 'partition').exists() for child in node.iterdir() if child.is_dir())


def verified_device(source):
    # Resolve the mount's actual block identity; md numbering and /dev aliases can change.
    need(source.startswith('/dev/'), f'unsupported mount source: {source}')
    number = block_number(Path(source))
    node = (SYS_BLOCK / number).resolve(strict=True)
    device = Path('/dev') / node.name
    need(read(node / 'dev') == number and block_number(device) == number and whole_device(node),
         'scratch device identity or topology is ambiguous')
    slaves = sorted(p.resolve(strict=True) for p in (node / 'slaves').iterdir())
    need(not any((node / 'holders').iterdir()), 'scratch device has a block-device holder')
    if (node / 'md').is_dir():
        need(read(node / 'md/level') == 'raid0' and len(set(slaves)) == len(slaves) >= 2
             and read(node / 'md/raid_disks') == str(len(slaves)),
             'scratch array is not a complete RAID0')
        leaves, expected_holders = slaves, {node}
    else:
        need(not slaves, 'unexpected scratch device stack')
        leaves, expected_holders = [node], set()
    for leaf in leaves:
        need(re.fullmatch(r'nvme\d+n\d+', leaf.name) and whole_device(leaf)
             and not any((leaf / 'slaves').iterdir()) and read(leaf / 'device/model') == LOCAL_MODEL
             and {p.resolve(strict=True) for p in (leaf / 'holders').iterdir()} == expected_holders,
             f'{leaf.name} is not a whole, exclusively assigned EC2 instance-store disk')
    return Device(device, number, tuple(read(leaf / 'dev') for leaf in leaves))


def mount_at(mounts, path):
    matches = [mount for mount in mounts if mount.path == path]
    need(len(matches) <= 1 and not any(path in mount.path.parents for mount in mounts),
         f'{path} has stacked or child mounts')
    return matches[0] if matches else None


def require_no_other_mounts(mounts, device, allowed=None):
    numbers = {device.number, *device.members}
    for mount in mounts:
        if mount is allowed:
            continue
        alias = mount.source.startswith('/dev/') and Path(mount.source).resolve() == device.path
        need(mount.number not in numbers and not alias, f'scratch storage is also mounted at {mount.path}')


def filesystem(device, fstype, label):
    properties = dict(line.split('=', 1) for line in run('blkid', '-p', '-o', 'export', str(device.path)).splitlines())
    need(properties.get('TYPE') == fstype and properties.get('LABEL') == label,
         f'{device.path} is not the expected {label} {fstype} filesystem')


def require_empty(path, allow_lost_found=False):
    need(path.resolve() == path, f'{path} is a symlink or reached through a symlink')
    if path.exists():
        for entry in path.iterdir():
            need(allow_lost_found and entry.name == 'lost+found' and entry.is_dir()
                 and not entry.is_symlink() and not any(entry.iterdir()), f'{path} is not empty')


def inspect(mounts=None):
    if mounts is None:
        mounts = mount_table()
    source, target = mount_at(mounts, LOCAL), mount_at(mounts, SCRATCH)
    need(not (source and target), f'both {LOCAL} and {SCRATCH} are mounted')
    if target:
        need(target.root == '/' and target.fstype == 'btrfs', f'{SCRATCH} is not a whole Btrfs mount')
        need(not (target.readonly or target.super_readonly), f'{SCRATCH} is read-only')
        device = verified_device(target.source)
        require_no_other_mounts(mounts, device, target)
        filesystem(device, 'btrfs', LABEL)
        info = run('btrfs', 'filesystem', 'show', '--raw', str(SCRATCH))
        members = re.findall(r'^\s*devid\s+.*?\s+path (\S+)\s*$', info, re.M)
        need(re.findall(r'Total devices (\d+)', info) == ['1'] and len(members) == 1
             and block_number(Path(members[0])) == device.number, 'benchmark Btrfs has unexpected devices')
        return device, True
    need(source and source.root == '/' and source.fstype == 'ext4',
         f'no fresh pbox instance-store mount at {LOCAL}; use pbox up N kvm-96 --local-nvme and wait for bootstrap')
    device = verified_device(source.source)
    need(source.number == device.number, 'ext4 mount and block device disagree')
    require_no_other_mounts(mounts, device, source)
    filesystem(device, 'ext4', 'local-nvme')
    require_empty(LOCAL, allow_lost_found=True)
    require_empty(SCRATCH)
    return device, False


def ensure_owner():
    owner = pwd.getpwnam('ubuntu')
    actual = SCRATCH.stat()
    if (actual.st_uid, actual.st_gid) != (owner.pw_uid, owner.pw_gid):
        os.chown(SCRATCH, owner.pw_uid, owner.pw_gid, follow_symlinks=False)


def restore_local(device, original):
    # Nothing has formatted this device yet. Restore only the same ext4
    # filesystem if it is still recognizable and exclusively assigned.
    mounts = mount_table()
    current = mount_at(mounts, LOCAL)
    need(not mount_at(mounts, SCRATCH), 'another benchmark mount appeared during recovery')
    need(verified_device(str(device.path)) == device, 'scratch block device changed during recovery')
    require_no_other_mounts(mounts, device, current)
    filesystem(device, 'ext4', 'local-nvme')
    mode = 'ro' if original.readonly or original.super_readonly else 'rw'
    if current:
        need(current.number == device.number and current.root == '/' and current.fstype == 'ext4'
             and verified_device(current.source) == device, 'another filesystem occupies the original scratch mount')
        if (current.readonly or current.super_readonly) != (mode == 'ro'):
            run('mount', '-o', 'remount,' + mode, '--', str(device.path), str(LOCAL))
    else:
        require_empty(LOCAL)
        run('mount', '-t', 'ext4', '-o', mode, '--', str(device.path), str(LOCAL))
    restored = mount_at(mount_table(), LOCAL)
    need(restored and restored.number == device.number and restored.root == '/' and restored.fstype == 'ext4'
         and (restored.readonly or restored.super_readonly) == (mode == 'ro'), 'original scratch mount was not restored')


def prepare():
    mounts = mount_table()
    device, ready = inspect(mounts)
    if ready:
        ensure_owner()
        return
    original = mount_at(mounts, LOCAL)
    SCRATCH.mkdir(exist_ok=True)
    try:
        # Freeze writes before the last emptiness check. A busy filesystem must
        # fail normally; never force or lazily unmount it.
        run('mount', '-o', 'remount,ro', '--', str(device.path), str(LOCAL))
        mounts = mount_table()
        local = mount_at(mounts, LOCAL)
        need(local and local.readonly and local.super_readonly and inspect(mounts) == (device, False),
             'scratch filesystem did not become read-only before formatting')
        run('umount', '--', str(LOCAL))
        mounts = mount_table()
        need(not mount_at(mounts, LOCAL) and not mount_at(mounts, SCRATCH), 'scratch mount changed before formatting')
        require_no_other_mounts(mounts, device)
        need(verified_device(str(device.path)) == device, 'scratch block device changed before formatting')
        filesystem(device, 'ext4', 'local-nvme')
        require_empty(SCRATCH)
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        try:
            restore_local(device, original)
        except (OSError, ValueError, subprocess.CalledProcessError) as recovery:
            raise ValueError(f'{exc}; could not restore the original scratch mount: {recovery}') from exc
        raise
    # Once mkfs starts, the old filesystem cannot safely be restored.
    run('mkfs.btrfs', '-f', '-L', LABEL, '--', str(device.path))
    run('mount', '-t', 'btrfs', '--', str(device.path), str(SCRATCH))
    need(inspect() == (device, True), 'expected benchmark Btrfs mount was not established')
    ensure_owner()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['check', 'prepare'])
    action = parser.parse_args().action
    try:
        need(os.geteuid() == 0 and os.path.samefile('/proc/self/ns/mnt', '/proc/1/ns/mnt'),
             'run this with sudo on the benchmark host')
        if action == 'check':
            inspect()
        else:
            fd = os.open('/run/boot-race-scratch.lock', os.O_CREAT | os.O_RDONLY | os.O_NOFOLLOW, 0o600)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                prepare()
            finally:
                os.close(fd)
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f'boot-race scratch: {exc}\n')


if __name__ == '__main__':
    main()
