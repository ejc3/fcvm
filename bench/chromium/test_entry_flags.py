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

Watched red 2026-08-28 against entry.sh at 13cb9543; the failure text is
quoted on each test.

Run: python3 -m unittest test_entry_flags -v
"""

import os
import re
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ENTRY = os.path.join(HERE, "entry.sh")
RULE = "--host-resolver-rules=MAP * 10.0.2.2"
SHELLS = ("sh", "bash")


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
                self.assertEqual(argv.count(RULE), 1, argv)
                self.assertLess(argv.index(RULE), argv.index("about:blank"),
                                "the rule landed after the URL")
                split = [a for a in argv if a in ("MAP", "*", "10.0.2.2")
                         or a in ("a.txt", "b.txt")]
                self.assertEqual(split, [], f"the rule was word-split or globbed: {argv}")
                self.assertNotIn("", argv, "an empty argv element reached chromium")

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
                self.assertIn(RULE, argv_with or [],
                              "the set case emitted no rule, so this comparison "
                              "proves nothing")
                result, argv = self._run(shell, {})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIsNotNone(argv, "chromium was never invoked")
                self.assertEqual(
                    [a for a in argv if a.startswith("--host-resolver-rules")], [],
                    f"a resolver rule was emitted with the knob unset: {argv}")
                self.assertNotIn("", argv, "an empty argv element reached chromium")
                self.assertEqual(argv, [a for a in argv_with if a != RULE],
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
                self.assertEqual(
                    [a for a in (argv or []) if a.startswith("--host-resolver-rules")],
                    ["--host-resolver-rules=MAP * fd00::2"])


if __name__ == "__main__":
    unittest.main()
