#!/usr/bin/env python3
"""The committed replay corpus carries no address of the host that captured it.

Some responses echo the client's address back: three ad geo lookups in the
elmundo.es capture held the capture host's public IPv4 address as
"client_ip" until it was replaced with a documentation address. Two checks
keep it from coming back:

  - every dotted-quad IPv4 address in corpus-live/ (bodies, index.json,
    live.txt; not the screenshots) is in a range nobody owns publicly or is
    named in ALLOWED with the reason it is there;
  - corpus_capture.write_capture replaces the address it is given as the
    host's own in every file it stores.

Run: python3 -m unittest test_corpus_addresses -v
"""

import ipaddress
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import corpus_capture  # noqa: E402

CORPUS = HERE / "corpus-live"

ALLOWED_NETWORKS = [ipaddress.IPv4Network(n) for n in (
    "127.0.0.0/8",      # loopback
    "10.0.0.0/8",       # RFC 1918, holds the guest (10.0.2.100) and its gateway (10.0.2.2)
    "172.16.0.0/12",    # RFC 1918
    "192.168.0.0/16",   # RFC 1918
    "192.0.2.0/24",     # RFC 5737 documentation ranges
    "198.51.100.0/24",
    "203.0.113.0/24",
)]

# Dotted quads that third-party pages ship, by reason. None is an address of
# the capture host.
ALLOWED = {
    "Cloudflare's public resolver, named in the Cloudflare pages' text and links": (
        "1.1.1.1",),
    "Fastly CDN address hard-coded in rtp.pt's player script (getCDNIP)": (
        "151.101.134.192",),
    "network prefixes in elmundo.es's own IP-range check script": (
        "193.110.128.0", "212.80.128.0"),
    "Chrome client-hint brand version 'Not=A?Brand;99.0.0.0' in ad request URLs": (
        "99.0.0.0",),
    "MediaWiki Parsoid version string 0.24.0.0-alpha18": (
        "0.24.0.0",),
    "a run of vendor ids in a consent script's list ('1~7.12.35.62.66...')": (
        "7.12.35.62",),
    "SVG path coordinates that read as dotted quads": (
        "1.1.1.3", "1.1.1.4", "1.1.4.2", "1.1.5.1", "1.2.5.1", "1.2.8.6", "1.4.1.2",
        "1.4.2.4", "1.4.8.4", "1.7.76.73", "1.8.8.5", "1.9.2.3", "1.9.5.2", "1.12.33.7",
        "1.15.24.55", "1.16.39.99", "1.29.63.68", "1.31.23.54", "1.31.55.61",
        "1.41.82.49", "1.48.44.49", "1.58.53.55", "1.64.5.68", "1.73.44.62",
        "1.73.48.5", "1.77.45.63", "1.78.53.61", "1.139.23.116", "2.1.3.9",
        "2.12.77.57", "2.14.64.89", "2.19.45.5", "2.67.76.87", "2.134.252.18",
        "2.208.145.125", "3.2.1.2", "3.31.87.94", "4.4.11.96", "4.6.5.6", "5.4.1.2",
        "8.25.34.95", "20.28.8.8"),
}
ALLOWED_ADDRESSES = {a for addresses in ALLOWED.values() for a in addresses}

DOTTED_QUAD = re.compile(rb"(?<![0-9.])((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?![0-9])")


def disallowed_addresses(root: Path):
    """(path, byte offset) of every IPv4 address outside the allowed ranges and list."""
    found = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.suffix == ".png":
            continue
        for match in DOTTED_QUAD.finditer(path.read_bytes()):
            text = match.group(1).decode()
            try:
                address = ipaddress.IPv4Address(text)
            except ValueError:
                continue
            if text in ALLOWED_ADDRESSES or any(address in n for n in ALLOWED_NETWORKS):
                continue
            found.append((str(path.relative_to(root)), match.start(1)))
    return found


class CommittedCorpus(unittest.TestCase):
    def test_every_ipv4_address_is_allowed(self):
        # The message names where, not what: a CI log is as public as the corpus.
        found = disallowed_addresses(CORPUS)
        self.assertEqual(found, [], "IPv4 addresses outside the allowlist, as (file, offset); "
                         "replace a capture host's address or add a third-party one to ALLOWED")


class CaptureScrub(unittest.TestCase):
    OWN = "34.201.17.86"       # made up; plays the capture host's public address
    OWN_ANY_FORM = re.compile(rb"(?<![0-9])34[.-]201[.-]17[.-]86(?![0-9])")

    def test_capture_replaces_own_address_in_every_stored_file(self):
        body = (b'{"client_ip": "34.201.17.86", "rdns": "ec2-34-201-17-86.example", '
                b'"other": "134.201.17.86"}')
        resources = [
            ("https://ads.example/geo?ip=34.201.17.86", body, 200, "application/json"),
            ("https://site.example/", b"<html>ok</html>", 200, "text/html"),
        ]
        redirects = {"https://site.example/r?ip=34.201.17.86":
                     {"status": 302, "location": "https://site.example/?from=34.201.17.86"}}
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp)
            corpus_capture.write_capture(dest, "https://site.example/", resources, redirects,
                                         "your address is 34.201.17.86", b"\x89PNG", [self.OWN])
            stored = {p: p.read_bytes() for p in dest.rglob("*") if p.is_file()}
            for path, data in stored.items():
                self.assertIsNone(self.OWN_ANY_FORM.search(data), path)
            index = json.loads((dest / "index.json").read_text())
            entry = index["resources"]["https://ads.example/geo?ip=198.51.100.1"]
            scrubbed = (dest / entry["file"]).read_bytes()
            self.assertEqual(scrubbed, b'{"client_ip": "198.51.100.1", "rdns": '
                             b'"ec2-198-51-100-1.example", "other": "134.201.17.86"}')
            self.assertEqual(len(scrubbed), len(body))
            self.assertEqual(entry["bytes"], len(scrubbed))
            self.assertEqual(disallowed_addresses(dest), [(entry["file"], scrubbed.index(b"134."))])

    def test_capture_refuses_without_an_own_address(self):
        def fails():
            raise OSError("no route")
        with self.assertRaises(RuntimeError):
            corpus_capture.own_public_addresses([], probes=(fails, lambda: "10.0.2.2"))
        self.assertEqual(
            corpus_capture.own_public_addresses([], probes=(fails, lambda: self.OWN + "\n")),
            [self.OWN])


if __name__ == "__main__":
    unittest.main()
