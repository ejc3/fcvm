#!/usr/bin/env python3
"""entry.sh's Chromium flag block, executed against a stub chromium.

BENCH_RESOLVE_ALL_TO=<ip> is the one-variable knob for the resolver-rule A/B:
entry.sh assembles `--host-resolver-rules=MAP * <ip>` itself, as ONE argv
element. The earlier attempt passed the whole flag through an env var and the
space inside the rule was word-split into two elements (corpus_serve.py,
2026-08-13), which is why the knob carries only the IP and the flag is built
inside the script.

The block under test is lifted out of the shipped entry.sh and run under sh
(the image's /bin/sh is dash) and under bash, with a stub `chromium` on PATH
that records its argv NUL-separated, so an empty element or a split one is
visible. The cwd holds files so an unquoted `*` would glob.

The rule text itself is judged by running it through `rewrite_host` below, a
model of net/base/host_mapping_rules.cc: a rule Chromium cannot parse is
dropped with only a LOG(ERROR), so a flag that reads correctly to a human can
leave the browser resolving through live DNS. Asserting on the mapping the
rule produces catches that; asserting on the flag's spelling does not.

Watched red 2026-08-28 against entry.sh at 13cb9543; the failure text is
quoted on each test.

Run: python3 -m unittest test_entry_flags -v
"""

import fnmatch
import os
import re
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ENTRY = os.path.join(HERE, "entry.sh")
FLAG = "--host-resolver-rules="
SHELLS = ("sh", "bash")


def parse_host_and_port(value):
    """net::ParseHostAndPort, the parser HostMappingRules puts a MAP rule's
    replacement through. Returns (host, port) with the host exactly as the
    hostname component spells it (brackets kept), port None when the
    replacement carries none, or (None, None) when the rule is unparseable
    and AddRuleFromString drops it.

    url::ParseServerInfo splits host from port at the LAST colon, unless that
    colon precedes the last `]`. So `fd00::2` is not an address to this
    parser: it is the host `fd00:` on port 2.
    """
    if not value or "@" in value:
        return None, None
    ipv6_terminator = len(value) if value[0] == "[" else -1
    colon = -1
    for index, char in enumerate(value):
        if char == "]":
            ipv6_terminator = index
        elif char == ":":
            colon = index
    if colon > ipv6_terminator:
        host, port_text = value[:colon], value[colon + 1:]
    else:
        host, port_text = value, ""
    if not host:
        return None, None
    port = None
    if port_text:
        if not port_text.isdigit() or not 0 <= int(port_text) <= 65535:
            return None, None
        port = int(port_text)
    return host, port


def parse_rules(rule_string):
    """net::HostMappingRules::SetRulesFromString. Returns (exclusions, maps).

    Rules are comma or semicolon separated. `EXCLUDE <pattern>` and
    `MAP <pattern> <replacement>` are the only two shapes; anything else is
    logged and dropped, which is why an unparseable rule is silent.
    """
    exclusions, maps = [], []
    for raw in re.split(r"[,;]", rule_string):
        parts = raw.split()
        if len(parts) == 2 and parts[0].lower() == "exclude":
            exclusions.append(parts[1].lower())
        elif len(parts) == 3 and parts[0].lower() == "map":
            host, port = parse_host_and_port(parts[2])
            if host is not None:
                maps.append((parts[1].lower(), host, port))
    return exclusions, maps


def rewrite_host(rule_string, host, port):
    """net::HostMappingRules::RewriteHost: exclusions first, then the first
    matching MAP rule. Returns the (host, port) Chromium would resolve."""
    exclusions, maps = parse_rules(rule_string)
    for pattern in exclusions:
        if fnmatch.fnmatchcase(host.lower(), pattern):
            return host, port
    for pattern, replacement, replacement_port in maps:
        if fnmatch.fnmatchcase(host.lower(), pattern) or fnmatch.fnmatchcase(
                f"{host}:{port}".lower(), pattern):
            return replacement, port if replacement_port is None else replacement_port
    return host, port


def rule_of(argv):
    """The one --host-resolver-rules element of an argv, or None."""
    rules = [a for a in argv or [] if a.startswith(FLAG)]
    assert len(rules) <= 1, f"more than one resolver rule reached chromium: {argv}"
    return rules[0][len(FLAG):] if rules else None


def flag_block() -> str:
    """The shipped bytes from the site-isolation knob through CHROME_PID."""
    with open(ENTRY) as handle:
        src = handle.read()
    match = re.search(r'(SITE_ISO_FLAGS=""\n.*?\nCHROME_PID=\$!)\n', src, re.S)
    assert match, "entry.sh's Chromium flag block is gone"
    return match.group(1)


class EntryResolverRule(unittest.TestCase):
    def _run(self, shell, env_extra, unset=()):
        d = tempfile.mkdtemp(prefix="entry-flags-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        argv_out = os.path.join(d, "chromium-argv")
        stub = os.path.join(binx, "chromium")
        with open(stub, "w") as handle:
            handle.write('#!/bin/sh\nprintf \'%s\\0\' "$@" > "$ARGV_OUT"\n')
        os.chmod(stub, 0o755)
        # Two files in the cwd: an unquoted `*` in the rule would expand to
        # them and the rule would arrive as several elements.
        cwd = os.path.join(d, "cwd")
        os.makedirs(cwd)
        for name in ("a.txt", "b.txt"):
            open(os.path.join(cwd, name), "w").close()
        script = os.path.join(d, "block.sh")
        with open(script, "w") as handle:
            handle.write("set -eu\nCDP_PORT=9222\nCDP_ADDR=0.0.0.0\n"
                         f"{flag_block()}\nwait \"$CHROME_PID\"\n")
        env = dict(os.environ)
        for name in ("BENCH_RESOLVE_ALL_TO", "CB_SITE_ISOLATION",
                     "CHROMIUM_EXTRA_FLAGS", *unset):
            env.pop(name, None)
        env.update(PATH=binx + os.pathsep + env["PATH"], ARGV_OUT=argv_out)
        env.update(env_extra)
        result = subprocess.run([shell, script], cwd=cwd, env=env,
                                capture_output=True, text=True, timeout=30)
        argv = None
        if os.path.exists(argv_out):
            with open(argv_out, "rb") as handle:
                raw = handle.read()
            argv = [] if not raw else raw.decode().split("\0")[:-1]
        return result, argv

    def test_the_rule_is_one_argv_element_before_the_url(self):
        """Red: `AssertionError: 0 != 1 : ['--headless=new', ... 'about:blank']`,
        the argv carried no resolver rule at all."""
        for shell in SHELLS:
            with self.subTest(shell=shell):
                result, argv = self._run(
                    shell, {"BENCH_RESOLVE_ALL_TO": "10.0.2.2"})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIsNotNone(argv, "chromium was never invoked")
                element = [a for a in argv if a.startswith(FLAG)]
                self.assertEqual(len(element), 1, argv)
                self.assertLess(argv.index(element[0]), argv.index("about:blank"),
                                "the rule landed after the URL")
                split = [a for a in argv if a in ("MAP", "EXCLUDE", "*", "10.0.2.2")
                         or a in ("a.txt", "b.txt")]
                self.assertEqual(split, [], f"the rule was word-split or globbed: {argv}")
                self.assertNotIn("", argv, "an empty argv element reached chromium")
                self.assertEqual(rewrite_host(rule_of(argv), "example.com", 443),
                                 ("10.0.2.2", 443),
                                 "the rule does not map a corpus host to the knob")

    def test_unset_leaves_the_argv_unchanged(self):
        """The unset argv is the set argv minus exactly the rule element.

        Red: `AssertionError: '--host-resolver-rules=MAP * 10.0.2.2' not found
        in [...] : the set case emitted no rule, so this comparison proves
        nothing`. Without that precondition the test passed on the unchanged
        script, because both argvs were identical.
        """
        for shell in SHELLS:
            with self.subTest(shell=shell):
                _with, argv_with = self._run(
                    shell, {"BENCH_RESOLVE_ALL_TO": "10.0.2.2"})
                self.assertIsNotNone(rule_of(argv_with),
                                     "the set case emitted no rule, so this "
                                     "comparison proves nothing")
                result, argv = self._run(shell, {})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIsNotNone(argv, "chromium was never invoked")
                self.assertEqual(
                    [a for a in argv if a.startswith(FLAG)], [],
                    f"a resolver rule was emitted with the knob unset: {argv}")
                self.assertNotIn("", argv, "an empty argv element reached chromium")
                self.assertEqual(argv, [a for a in argv_with if not a.startswith(FLAG)],
                                 "the knob changed more than the one element")
                empty, argv_empty = self._run(shell, {"BENCH_RESOLVE_ALL_TO": ""})
                self.assertEqual(empty.returncode, 0, empty.stderr)
                self.assertEqual(argv_empty, argv,
                                 "an empty knob must behave exactly like an unset one")

    def test_a_value_that_is_not_one_ip_token_is_refused(self):
        """Red: `AssertionError: 0 == 0 : chromium was launched with a
        BENCH_RESOLVE_ALL_TO holding a space`; the unchecked value would have
        produced a rule Chromium silently ignores."""
        for shell in SHELLS:
            for bad in ("10.0.2.2 extra", "10.0.2.2,127.0.0.1", "MAP * 10.0.2.2"):
                with self.subTest(shell=shell, value=bad):
                    result, argv = self._run(
                        shell, {"BENCH_RESOLVE_ALL_TO": bad})
                    self.assertNotEqual(
                        result.returncode, 0,
                        f"chromium was launched with a BENCH_RESOLVE_ALL_TO holding "
                        f"{bad!r}: {argv}")
                    self.assertIsNone(argv, "chromium was launched before the refusal")
                    self.assertIn("BENCH_RESOLVE_ALL_TO", result.stderr,
                                  "the refusal does not name the knob")

    def test_a_value_that_is_not_an_ip_literal_is_refused(self):
        """Hex digits, dots and colons alone do not make an address:
        `deadbeef`, `1.2.3.999` and `:::` all reach Chromium as a resolver
        target under a character whitelist, and every request then fails
        far from the knob that caused it. The value must parse as one IPv4
        or IPv6 address; a real IPv6 literal is accepted."""
        for shell in SHELLS:
            for bad in ("deadbeef", "1.2.3.999", ":::", "10.0.2", "1.2.3.4.5", "10.0.2.2."):
                with self.subTest(shell=shell, value=bad):
                    result, argv = self._run(shell, {"BENCH_RESOLVE_ALL_TO": bad})
                    self.assertNotEqual(
                        result.returncode, 0,
                        f"chromium was launched with BENCH_RESOLVE_ALL_TO={bad!r}: {argv}")
                    self.assertIsNone(argv, "chromium was launched before the refusal")
                    self.assertIn("BENCH_RESOLVE_ALL_TO", result.stderr)
            with self.subTest(shell=shell, value="fd00::2"):
                result, argv = self._run(shell, {"BENCH_RESOLVE_ALL_TO": "fd00::2"})
                self.assertEqual(result.returncode, 0, result.stderr)
                host, port = rewrite_host(rule_of(argv), "example.com", 443)
                self.assertEqual((host.strip("[]"), port), ("fd00::2", 443))


    def test_loopback_and_localhost_are_never_mapped(self):
        """The knob maps every host Chromium resolves, and the browser
        resolves its own warmup page: entry.sh navigates
        http://127.0.0.1:$HTTP_PORT/warmup.html before it writes the ready
        marker. With `MAP * 10.0.2.2` and nothing listening on 10.0.2.2:8000
        that navigation fails, `set -e` exits the script, and the container
        never becomes healthy. The rule must exclude the loopback names.

        Red on entry.sh at 96664d74:
        `AssertionError: Tuples differ: ('10.0.2.2', 8000) != ('127.0.0.1', 8000)`
        `: the warmup host is mapped away from the page server`
        """
        for shell in SHELLS:
            with self.subTest(shell=shell):
                _result, argv = self._run(
                    shell, {"BENCH_RESOLVE_ALL_TO": "10.0.2.2"})
                rule = rule_of(argv)
                self.assertIsNotNone(rule, "chromium was launched with no rule")
                self.assertEqual(
                    rewrite_host(rule, "127.0.0.1", 8000), ("127.0.0.1", 8000),
                    "the warmup host is mapped away from the page server")
                for host in ("localhost", "LocalHost", "::1"):
                    self.assertEqual(
                        rewrite_host(rule, host, 8000), (host, 8000),
                        f"{host} is mapped away from the page server")
                self.assertEqual(
                    rewrite_host(rule, "example.com", 443), ("10.0.2.2", 443),
                    "excluding the loopback also stopped the corpus mapping")


    def test_a_scoped_ipv6_address_is_refused(self):
        """`ipaddress.ip_address` accepts a scope id (`fe80::1%eth0`), and no
        Chromium resolver rule can carry one: the zone travels with the
        address only inside a sockaddr. Emitting it produces a rule that
        resolves nothing, so the value is refused at the knob.

        Red on entry.sh at 96664d74: `AssertionError: 0 != 0 : chromium was
        launched with BENCH_RESOLVE_ALL_TO='fe80::1%eth0'`.
        """
        for shell in SHELLS:
            for bad in ("fe80::1%eth0", "fe80::1%1"):
                with self.subTest(shell=shell, value=bad):
                    result, argv = self._run(shell, {"BENCH_RESOLVE_ALL_TO": bad})
                    self.assertNotEqual(
                        result.returncode, 0,
                        f"chromium was launched with BENCH_RESOLVE_ALL_TO={bad!r}: {argv}")
                    self.assertIsNone(argv, "chromium was launched before the refusal")
                    self.assertIn("BENCH_RESOLVE_ALL_TO", result.stderr)

    def test_an_ipv6_target_reaches_chromium_in_the_form_it_parses(self):
        """`MAP * fd00::2` is not the rule it reads as. url::ParseServerInfo
        splits at the last colon, so Chromium stores the host `fd00:` on port
        2 and every request goes somewhere that does not exist, while the
        metadata records a controlled resolver. The replacement must be
        bracketed.

        Red on entry.sh at 96664d74:
        `AssertionError: Tuples differ: ('fd00:', 2) != ('fd00::2', 443)`
        """
        for shell in SHELLS:
            for value, expected in (("fd00::2", "fd00::2"),
                                    ("FD00::2", "fd00::2"),
                                    ("::1", "::1")):
                with self.subTest(shell=shell, value=value):
                    result, argv = self._run(
                        shell, {"BENCH_RESOLVE_ALL_TO": value})
                    self.assertEqual(result.returncode, 0, result.stderr)
                    rule = rule_of(argv)
                    self.assertIsNotNone(rule, "chromium was launched with no rule")
                    host, port = rewrite_host(rule, "example.com", 443)
                    self.assertEqual((host.strip("[]"), port), (expected, 443))


if __name__ == "__main__":
    unittest.main()
