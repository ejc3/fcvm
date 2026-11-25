"""renders_host_record.py: a run's host record names the build and the host's kind, and no host."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_campaign  # noqa: E402
import renders_host_record  # noqa: E402

CMDLINE = ("BOOT_IMAGE=/vmlinuz-7.3.3-fcvm-0123456789ab root=PARTUUID=0b1c2d3e-0000-4000-8000-000000000000 ro "
           "console=ttyS0 kvm-arm.mode=nested\n")


class HostRecord(unittest.TestCase):
    def text(self, **change):
        values = dict(kernel="7.3.3-fcvm-0123456789ab\n", cmdline=CMDLINE, boot_id="made-up-boot-id\n",
                      source="a" * 40, kitesurf="b" * 40, instance_type="m8gd.metal-24xl",
                      arguments="MAX_FCVM=40  MAX_ENGINES=48\n", ended="2026-01-02T03:04:05Z")
        values.update(change)
        return renders_host_record.record(**values)

    def test_the_record_is_one_the_campaign_tool_takes(self):
        text = self.text()
        self.assertIn("\ncmdline: kvm-arm.mode=nested\n", text)
        self.assertNotIn("0b1c2d3e", text)
        self.assertNotIn("made-up-boot-id", text)
        self.assertIn("arguments: MAX_FCVM=40 MAX_ENGINES=48\n", text)
        self.assertEqual(text.count("\n"), 7)
        path = os.path.join(self.enterContext(__import__("tempfile").TemporaryDirectory()), "host-record.txt")
        with open(path, "w") as handle:
            handle.write(text)
        record = renders_campaign.host_record(path)
        self.assertEqual((record["kernel"], record["instance_type"], record["ended"], record["arguments"]),
                         ("7.3.3-fcvm-0123456789ab", "m8gd.metal-24xl", "2026-01-02T03:04:05Z",
                          "MAX_FCVM=40 MAX_ENGINES=48"))
        self.assertRegex(record["boot"], r"^[0-9a-f]{16}$")

    def test_only_the_named_kernel_parameters_are_kept(self):
        """The record keeps the parameters that change what is measured, from a list, and nothing else of
        the command line: a root device, an address, a host name or an id never reaches it, however spelled."""
        hostile = ("BOOT_IMAGE=/vmlinuz root=/dev/disk/by-uuid/0b1c2d3e-0000-4000-8000-000000000000 "
                   "root=/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_" + "vol" + "0123456789abcdef0-part1 "
                   "nfsroot=10.1.2.3:/export ip=10.1.2.4::10.1.2.1:255.255.255.0:build-host-7:eth0:off "
                   "systemd.hostname=build-host-7 rd.luks.uuid=luks-0b1c2d3e root=uuid=0b1c2d3e resume=UUID=y "
                   "console=ttyS0 kvm-arm.mode=nested numa=off arm64.nv2 hugepagesz=2M hugepages=34000 "
                   "isolcpus=80-95 mitigations=off numa=10.1.2.3 kvm-arm.mode=build-host-7/x")
        line = self.text(cmdline=hostile).splitlines()[1]
        self.assertEqual(line, "cmdline: kvm-arm.mode=nested numa=off arm64.nv2 hugepagesz=2M hugepages=34000 "
                               "isolcpus=80-95 mitigations=off")
        self.assertEqual(self.text(cmdline="quiet splash\n").splitlines()[1], "cmdline: (none of the recorded parameters)")

    def test_an_arguments_value_that_is_a_path_keeps_its_last_name_only(self):
        text = self.text(arguments="MAX_FCVM=0 LEVELS=0,38 DIST=/home/someone/kitesurf/dist-opt TAG=golden")
        self.assertIn("arguments: MAX_FCVM=0 LEVELS=0,38 DIST=dist-opt TAG=golden\n", text)
        self.assertIn("arguments: LEVELS=15,38 SECONDS_RUN=360 VIEWPORT=1280x720 CPUS_FCVM=0-39 RECYCLE_MIB=0\n",
                      self.text(arguments="LEVELS=15,38 SECONDS_RUN=360 VIEWPORT=1280x720 CPUS_FCVM=0-39 RECYCLE_MIB=0"))
        for bad in ("MAX_FCVM", "lower=1", "X=a b;c", "HOST=10.1.2.3", "X=$(id)", "HOST=build-host-7",
                    "MAX_FCVM=build-host-7", "LEVELS=10.1.2.3", "SECONDS_RUN=1,2", "DIST=a.internal.example",
                    "DIST=/", "TAG=", "KITESURF=/srv/k"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.text(arguments=bad)

    def test_what_is_not_a_commit_or_an_instance_type_is_refused(self):
        for change in ({"source": "abc123"}, {"kitesurf": ""}, {"instance_type": "a host name"},
                       {"instance_type": "ip-" + "10-0-0-1"}, {"instance_type": "host1.internal"},
                       {"instance_type": "build.example"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.text(**change)
        for good in ("m8gd.metal-24xl", "c8g.metal-48xl", "g4dn.metal", "m7g.8xlarge", "t4g.large"):
            self.text(instance_type=good)


if __name__ == "__main__":
    unittest.main()
