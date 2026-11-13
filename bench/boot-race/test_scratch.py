"""Exercise storage safety against synthetic mount and sysfs state; no real disk writes."""

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import scratch


class ScratchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.local, self.target = self.root / 'nvme', self.root / 'btrfs'
        self.local.mkdir()
        self.sys = self.root / 'sys'
        self.sys.mkdir()
        self.numbers = {}
        self.commands = []
        self.mounts = []
        self.filesystem = ('ext4', 'local-nvme')
        self.readonly = True
        self.unmount_fails = False
        self.format_fails = False
        self.target_mount_fails = False
        self.remount_adds_data = False
        self.members = None
        self.old_source = '/dev/md/nvme'
        self.array = self.node('md42', '9:42')
        self.numbers[self.old_source] = '9:42'
        for i in range(1, 4):
            self.leaf(self.array, i)
        md = self.array / 'md'
        md.mkdir()
        (md / 'level').write_text('raid0\n')
        (md / 'raid_disks').write_text('3\n')
        self.mounts = [scratch.Mount('9:42', '/', self.local, 'ext4', self.old_source)]
        for name, value in {'LOCAL': self.local, 'SCRATCH': self.target, 'SYS_BLOCK': self.sys,
                            'block_number': self.block_number, 'mount_table': lambda: self.mounts[:],
                            'run': self.run_command, 'ensure_owner': lambda: None}.items():
            self.enterContext(patch.object(scratch, name, value))

    def node(self, name, number):
        node = self.root / name
        node.mkdir()
        (node / 'dev').write_text(number)
        for entry in ('slaves', 'holders'):
            (node / entry).mkdir()
        (self.sys / number).symlink_to(node)
        self.numbers['/dev/' + name] = number
        return node

    def leaf(self, parent, i):
        name = f'nvme{i}n1'
        node = self.node(name, f'259:{i}')
        (node / 'device').mkdir()
        (node / 'device/model').write_text(scratch.LOCAL_MODEL + '\n')
        (parent / 'slaves' / name).symlink_to(node)
        (node / 'holders' / parent.name).symlink_to(parent)

    def block_number(self, path):
        return self.numbers[str(path)]

    def run_command(self, *args):
        self.commands.append(args)
        if args[0] == 'blkid':
            return f'TYPE={self.filesystem[0]}\nLABEL={self.filesystem[1]}\n'
        if args[0] == 'btrfs':
            return self.members or ('Label: boot-race-local\n    Total devices 1 FS bytes used 0\n'
                                    f'    devid 1 size 0 used 0 path {self.mounts[0].source}\n')
        if args[:2] == ('mount', '-o'):
            if args[2] == 'remount,ro' and self.remount_adds_data:
                (self.local / 'new-data').write_text('keep me')
            current = self.mounts[0]
            self.mounts[0] = scratch.Mount(current.number, current.root, current.path, current.fstype,
                                           current.source, self.readonly and args[2] == 'remount,ro',
                                           self.readonly and args[2] == 'remount,ro')
        elif args[0] == 'umount':
            if self.unmount_fails:
                raise subprocess.CalledProcessError(32, args)
            self.mounts = []
        elif args[0] == 'mkfs.btrfs':
            if self.format_fails:
                raise subprocess.CalledProcessError(1, args)
            self.filesystem = ('btrfs', scratch.LABEL)
        elif args[:3] == ('mount', '-t', 'ext4'):
            self.mounts = [scratch.Mount(self.numbers[args[-2]], '/', self.local, 'ext4', args[-2], args[4] == 'ro')]
        elif args[:3] == ('mount', '-t', 'btrfs'):
            if self.target_mount_fails:
                raise subprocess.CalledProcessError(32, args)
            self.mounts = [scratch.Mount('0:42', '/', self.target, 'btrfs', args[-2])]
        else:
            self.fail(f'unexpected command: {args!r}')
        return ''

    def assert_no_format(self):
        self.assertFalse(any(command[0] == 'mkfs.btrfs' for command in self.commands))

    def test_verified_raid_replaces_exact_mounted_device_and_rerun_is_read_only(self):
        (self.local / 'lost+found').mkdir()
        scratch.prepare()
        self.assertIn(('mkfs.btrfs', '-f', '-L', scratch.LABEL, '--', '/dev/md42'), self.commands)
        self.assertLess(next(i for i, c in enumerate(self.commands) if c[0] == 'umount'),
                        next(i for i, c in enumerate(self.commands) if c[0] == 'mkfs.btrfs'))
        self.commands.clear()
        scratch.prepare()
        self.assertEqual([command[0] for command in self.commands], ['blkid', 'btrfs'])

    def test_verified_single_instance_disk_works(self):
        leaf = self.root / 'nvme1n1'
        (leaf / 'holders/md42').unlink()
        self.mounts = [scratch.Mount('259:1', '/', self.local, 'ext4', '/dev/nvme1n1')]
        scratch.prepare()
        self.assertIn(('mkfs.btrfs', '-f', '-L', scratch.LABEL, '--', '/dev/nvme1n1'), self.commands)

    def test_diskless_host_refuses_before_any_command(self):
        self.mounts.clear()
        with self.assertRaisesRegex(ValueError, 'no fresh pbox instance-store mount'):
            scratch.prepare()
        self.assertEqual(self.commands, [])

    def test_mixed_or_direct_ebs_is_never_formatted(self):
        (self.root / 'nvme2n1/device/model').write_text('Amazon Elastic Block Store')
        with self.assertRaisesRegex(ValueError, 'not a whole'):
            scratch.prepare()
        self.mounts = [scratch.Mount('259:2', '/', self.local, 'ext4', '/dev/nvme2n1')]
        (self.root / 'nvme2n1/holders/md42').unlink()
        with self.assertRaisesRegex(ValueError, 'not a whole'):
            scratch.prepare()
        self.assertEqual(self.commands, [])

    def test_existing_non_btrfs_or_multi_device_btrfs_is_rejected(self):
        self.mounts = [scratch.Mount('9:42', '/', self.target, 'ext4', self.old_source)]
        with self.assertRaisesRegex(ValueError, 'not a whole Btrfs mount'):
            scratch.prepare()
        self.mounts = [scratch.Mount('0:42', '/', self.target, 'btrfs', self.old_source)]
        self.filesystem = ('btrfs', scratch.LABEL)
        self.mounts = [scratch.Mount('0:42', '/', self.target, 'btrfs', self.old_source, readonly=True)]
        with self.assertRaisesRegex(ValueError, 'read-only'):
            scratch.prepare()
        self.mounts = [scratch.Mount('0:42', '/', self.target, 'btrfs', self.old_source)]
        self.members = ('Total devices 2\n    devid 1 size 0 used 0 path /dev/md42\n'
                        '    devid 2 size 0 used 0 path /dev/nvme2n1\n')
        with self.assertRaisesRegex(ValueError, 'unexpected devices'):
            scratch.prepare()
        self.assert_no_format()

    def test_refuses_nonempty_lost_found_and_nonempty_target(self):
        lost = self.local / 'lost+found'
        lost.mkdir()
        recovered = lost / 'recovered'
        recovered.touch()
        with self.assertRaisesRegex(ValueError, 'not empty'):
            scratch.prepare()
        recovered.unlink()
        self.target.mkdir()
        (self.target / 'previous-data').touch()
        with self.assertRaisesRegex(ValueError, 'not empty'):
            scratch.prepare()
        self.assert_no_format()

    def test_refuses_duplicate_mount_and_child_mount(self):
        self.mounts.append(scratch.Mount('9:42', '/', self.root / 'other', 'ext4', '/dev/md42'))
        with self.assertRaisesRegex(ValueError, 'also mounted'):
            scratch.prepare()
        self.mounts.pop()
        self.mounts.append(scratch.Mount('0:55', '/', self.local / 'child', 'tmpfs', 'tmpfs'))
        with self.assertRaisesRegex(ValueError, 'child mounts'):
            scratch.prepare()
        self.assertEqual(self.commands, [])

    def test_unmount_failure_never_reaches_mkfs(self):
        self.unmount_fails = True
        with self.assertRaises(subprocess.CalledProcessError):
            scratch.prepare()
        self.assert_no_format()
        self.assertFalse(self.mounts[0].readonly)
        self.assertIn(('mount', '-o', 'remount,rw', '--', '/dev/md42', str(self.local)), self.commands)

    def test_freezes_writes_and_rechecks_empty_before_unmounting(self):
        self.readonly = False
        with self.assertRaisesRegex(ValueError, 'did not become read-only'):
            scratch.prepare()
        self.readonly = True
        self.remount_adds_data = True
        with self.assertRaisesRegex(ValueError, 'not empty'):
            scratch.prepare()
        self.assertFalse(any(command[0] in {'umount', 'mkfs.btrfs'} for command in self.commands))
        self.assertFalse(self.mounts[0].readonly)
        self.assertEqual((self.local / 'new-data').read_text(), 'keep me')

    def test_reinstates_exact_ext4_if_a_recheck_fails_after_unmount(self):
        def fail_once(path, **kwargs):
            if path == self.target and not self.mounts:
                raise ValueError('target changed')
            return original(path, **kwargs)

        original = scratch.require_empty
        with patch.object(scratch, 'require_empty', side_effect=fail_once):
            with self.assertRaisesRegex(ValueError, 'target changed'):
                scratch.prepare()
        self.assert_no_format()
        self.assertEqual(self.mounts, [scratch.Mount('9:42', '/', self.local, 'ext4', '/dev/md42')])

    def test_never_attempts_ext4_recovery_after_format_starts(self):
        for case in ('format_fails', 'target_mount_fails'):
            with self.subTest(case=case):
                self.mounts = [scratch.Mount('9:42', '/', self.local, 'ext4', self.old_source)]
                self.filesystem = ('ext4', 'local-nvme')
                self.commands.clear()
                with patch.object(self, case, True):
                    with self.assertRaises(subprocess.CalledProcessError):
                        scratch.prepare()
                self.assertFalse(any(c[:3] == ('mount', '-t', 'ext4') for c in self.commands))



class BoxSetup(unittest.TestCase):
    def test_the_build_runs_in_a_new_login_not_under_sg(self):
        """sg kvm makes kvm the primary group, and newuidmap then refuses setup-fcvm's rootless podman. RED BEFORE
        THE FIX: box-setup.sh ran make build and make setup-fcvm under sg kvm, and setup-fcvm failed on the box."""
        text = (Path(__file__).resolve().parent / "box-setup.sh").read_text()
        commands = [l for l in text.splitlines() if not l.lstrip().startswith("#")]
        self.assertFalse([l for l in commands if l.lstrip().startswith("sg ")])
        self.assertTrue([l for l in commands if l.startswith("sudo -u ubuntu -H bash -c") and "setup-fcvm" in l])


if __name__ == '__main__':
    unittest.main()
