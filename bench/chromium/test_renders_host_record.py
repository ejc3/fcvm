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
        self.assertIn("root=PARTUUID=elided ro console=ttyS0 kvm-arm.mode=nested\n", text)
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

    def test_every_partition_and_filesystem_id_is_elided(self):
        for cmdline in ("root=UUID=0b1c2d3e-0000-4000-8000-000000000000 ro",
                        "root=PARTUUID=x resume=UUID=y", "root=LABEL=cloudimg-rootfs rd.luks.uuid=UUID=z"):
            with self.subTest(cmdline=cmdline):
                line = self.text(cmdline=cmdline).splitlines()[1]
                self.assertNotRegex(line, r"UUID=(?!elided)")

    def test_what_is_not_a_commit_or_an_instance_type_is_refused(self):
        for change in ({"source": "abc123"}, {"kitesurf": ""}, {"instance_type": "a host name"},
                       {"instance_type": "ip-10-0-0-1"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.text(**change)


if __name__ == "__main__":
    unittest.main()
