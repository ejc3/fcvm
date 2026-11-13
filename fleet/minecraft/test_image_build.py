"""The tile image's build and the caller's proxy (Makefile PODMAN_BUILD_PROXY,
scripts/podman-build-proxy-args.sh): with a proxy on the host's loopback the build runs on the
host's network, where that loopback is reachable, and keeps the proxy; any other proxy, and none,
leave podman's defaults. The proxy is never dropped."""

import os
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROXY_VARS = ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY")


def podman_build_line(**proxy: str) -> str:
    env = {k: v for k, v in os.environ.items() if k not in PROXY_VARS}
    env.update(proxy)
    out = subprocess.run(["make", "-s", "-n", "-C", str(ROOT), "minecraft-image"], env=env,
                         capture_output=True, text=True, check=True, timeout=60).stdout
    joined = out.replace("\\\n", " ")
    [line] = [l for l in joined.splitlines() if " build " in l and "fleet/minecraft/Containerfile" in l]
    return line


class ProxyTests(unittest.TestCase):
    def test_a_loopback_proxy_is_reached_on_the_hosts_network_and_kept(self):
        for name, value in (("https_proxy", "http://user:secret@127.0.0.1:46147"),
                            ("HTTPS_PROXY", "http://[::1]:3128"),
                            ("http_proxy", "http://localhost:8080")):
            with self.subTest(name=name):
                args = podman_build_line(**{name: value}).split()
                self.assertIn("--network=host", args)
                # the proxy reaches the build: nothing turns podman's proxy passing off
                self.assertFalse(any(a.startswith("--http-proxy") for a in args), args)

    def test_another_proxy_and_none_are_left_to_podman(self):
        for proxy in ({"https_proxy": "http://proxy.example:3128"}, {}):
            with self.subTest(proxy=proxy):
                args = podman_build_line(**proxy).split()
                self.assertNotIn("--network=host", args)
                self.assertFalse(any(a.startswith("--http-proxy") for a in args), args)


if __name__ == "__main__":
    unittest.main()
