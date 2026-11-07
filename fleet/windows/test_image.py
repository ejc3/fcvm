#!/usr/bin/env python3
"""Tests for the Windows image build's inputs and helpers (no Windows, no QEMU needed).

    python3 -B fleet/windows/test_image.py
"""
import os
import re
import sys
import unittest
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import image_tool  # noqa: E402

NS = "{urn:schemas-microsoft-com:unattend}"


def read(*p):
    with open(os.path.join(*p), encoding="utf-8") as f:
        return f.read()


class Render(unittest.TestCase):
    def test_template_renders_to_xml_with_the_index_and_an_escaped_password(self):
        out = image_tool.render(read(HERE, "autounattend.xml.in"), "4", "a<b&c", "s>x")
        root = ET.fromstring(out)
        values = [e.text for e in root.iter(NS + "Value")]
        self.assertIn("4", values)
        self.assertIn("a<b&c", values)
        self.assertIn("s>x", values)
        names = [e.text for e in root.iter(NS + "Name")]
        self.assertIn("sandbox", names)
        groups = [e.text for e in root.iter(NS + "Group")]
        self.assertEqual(groups, ["Users;Remote Desktop Users"], "sandbox must be in no administrative group")
        computer = [e.text for e in root.iter(NS + "ComputerName")]
        self.assertNotIn("sandbox", [c.lower() for c in computer], "a user name may not equal the computer name")
        self.assertNotIn("@", "".join(v for v in values if v))

    def test_a_placeholder_twice_or_missing_is_refused(self):
        t = read(HERE, "autounattend.xml.in")
        with self.assertRaisesRegex(ValueError, "@ADMIN_PASSWORD@ appears 2 times"):
            image_tool.render(t + "@ADMIN_PASSWORD@", "4", "x", "y")
        with self.assertRaisesRegex(ValueError, "@IMAGE_INDEX@ appears 0 times"):
            image_tool.render(t.replace("@IMAGE_INDEX@", "4"), "4", "x", "y")
        with self.assertRaisesRegex(ValueError, "not a number"):
            image_tool.render(t, "4; rm", "x", "y")
        with self.assertRaisesRegex(ValueError, "must differ"):
            image_tool.render(t, "4", "same", "same")


class SetupLog(unittest.TestCase):
    SAMPLE = (
        "Activating Windows(R), ServerDatacenterEval edition ...\r\n"
        "Product activated successfully.\r\n"
        "Windows(R), ServerDatacenterEval edition:\r\n"
        "    Timebased activation will expire 5/6/2027 5:45:50 PM\r\n"
    )

    def test_the_log_between_the_markers_is_kept_and_the_rest_dropped(self):
        com2 = ("boot noise\n" + image_tool.LOG_START + "\n﻿" + self.SAMPLE + "\n" + image_tool.LOG_END + "\ntrailing").encode()
        log = image_tool.setup_log(com2)
        self.assertIn("Product activated successfully.", log)
        self.assertNotIn("boot noise", log)
        self.assertNotIn("trailing", log)
        self.assertNotIn("\r", log)

    def test_no_markers_is_an_error(self):
        with self.assertRaisesRegex(ValueError, "no setup log"):
            image_tool.setup_log(b"only boot noise")

    def test_the_expiry_is_read_as_utc_on_a_24_hour_clock(self):
        self.assertEqual(image_tool.expiry(self.SAMPLE), "2027-05-06T17:45:50+00:00")
        self.assertEqual(image_tool.expiry("will expire 12/1/2027 12:05:00 AM"), "2027-12-01T00:05:00+00:00")
        self.assertEqual(image_tool.expiry("will expire 12/1/2027 12:05:00 PM"), "2027-12-01T12:05:00+00:00")
        with self.assertRaisesRegex(ValueError, "no evaluation expiry"):
            image_tool.expiry("License Status: Licensed")


class Build(unittest.TestCase):
    def test_setup_complete_never_reboots_the_machine_itself(self):
        setup = read(HERE, "fcvm-setup.ps1")
        # SetupComplete's own steps: the script without the finish script it writes out
        body = re.sub(r"@'.*?'@", "", setup, flags=re.S)
        self.assertNotRegex(body, r"(?m)^\s*(shutdown|Restart-Computer)\b")
        self.assertIn("New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(2)", setup)

    def test_the_build_is_private_and_caches_inputs_by_checksum(self):
        s = read(HERE, "build-image.sh")
        self.assertRegex(s, r"\numask 077\n")
        self.assertNotIn("umask 022", s)
        self.assertIn('f=$SRC/$2-$3', s)
        self.assertIn('OUT_ROOT=$(realpath -m', s)


class Pins(unittest.TestCase):
    def test_every_download_is_pinned_by_sha256(self):
        s = read(HERE, "build-image.sh")
        for name in ("ISO", "VIRTIO", "FW"):
            self.assertRegex(s, rf"\n{name}_URL='https://[^']+'\n")
            self.assertRegex(s, rf"\n{name}_SHA256=[0-9a-f]{{64}}\n")

    def test_no_key_or_password_is_in_the_repository(self):
        for f in os.listdir(HERE):
            if not f.endswith((".in", ".ps1", ".cmd", ".sh", ".xml")):
                continue
            s = read(HERE, f)
            self.assertNotRegex(s, r"<ProductKey>|<Key>[A-Z0-9]{5}-", f)
            for m in re.finditer(r"<Value>([^<]*)</Value>", s):
                self.assertRegex(m.group(1), r"^@[A-Z_]+@$", f"{f}: a literal value in the template")


class AgentAddresses(unittest.TestCase):
    """The guest agent hardcodes fcvm's rootless addressing (src/network/pasta.rs); a change
    there must change the agent, or every sandbox loses its network after a restore."""

    def rust_const(self, name):
        m = re.search(rf'const {name}: &str = "([^"]+)";', read(ROOT, "src", "network", "pasta.rs"))
        self.assertIsNotNone(m, name)
        return m.group(1)

    def test_the_agent_uses_fcvms_guest_gateway_and_namespace_addresses(self):
        agent = read(HERE, "fcvm-agent.ps1")
        self.assertIn(f"-IPAddress {self.rust_const('GUEST_IP')} -PrefixLength 24 -DefaultGateway {self.rust_const('GUEST_GATEWAY')}", agent)
        self.assertIn(f"ConnectAsync('{self.rust_const('NAMESPACE_IP')}'", agent)
        mac = self.rust_const("NAMESPACE_MAC").replace(":", "-")
        self.assertIn(f"-IPAddress {self.rust_const('NAMESPACE_IP')} -LinkLayerAddress {mac} -State Permanent", agent)

    def test_the_agent_acknowledges_every_answer_with_its_generation_and_time(self):
        agent = read(HERE, "fcvm-agent.ps1")
        self.assertIn("ConnectAsync('10.0.2.1', 4600)", agent)
        self.assertIn('"ack $gen $([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())`n"', agent)

    def test_the_agent_retries_the_network_until_the_neighbour_is_pinned(self):
        agent = read(HERE, "fcvm-agent.ps1")
        self.assertIn("do { Net; Start-Sleep -Seconds 1 } until (Get-NetNeighbor -IPAddress 10.0.2.1 | Where-Object State -eq 'Permanent')", agent)
        self.assertNotRegex(agent, r"(?m)^Net$", "a single Net at start gives up when the NIC is not up yet")

    def test_the_agent_parses_only_a_whole_line_with_a_plausible_time(self):
        agent = read(HERE, "fcvm-agent.ps1")
        self.assertIn("$r.ReadLine()", agent)
        m = re.search(r"-match '([^']+)'", agent)
        self.assertIsNotNone(m)
        line = re.compile(m.group(1))
        self.assertTrue(line.match("vm-1a2b 1794074699656"))
        # a partial read, a short time, trailing junk: none may set the clock
        for bad in ("vm-1a2b 1794", "vm-1a2b", "vm-1a2b 1794074699656 x", "a b 1794074699656", ""):
            self.assertFalse(line.match(bad), bad)

    def test_the_guest_firewall_lets_the_agent_reach_the_namespace(self):
        setup = read(HERE, "fcvm-setup.ps1")
        self.assertIn("Get-NetFirewallRule -Direction Outbound -Action Allow -Enabled True | Disable-NetFirewallRule", setup)
        self.assertIn(f"-Direction Outbound -RemoteAddress {self.rust_const('NAMESPACE_IP')} -Action Allow", setup)
        self.assertIn("-DefaultOutboundAction Block", setup)


if __name__ == "__main__":
    unittest.main(verbosity=2)
