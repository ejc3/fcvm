#!/usr/bin/env python3
"""The campaign's sealing, provenance and preflight properties, pinned.

A campaign runs for hours and produces records that outlive it. Every property
here failed silently before it was fixed: a contaminated run directory, a
provenance ref pointing somewhere else, a corpus missing 13 of 14 URLs, a log
filter that removed the records the analysis needs. None of them raises; each
produces a run that looks finished and is not trustworthy.

Two styles, both cheap. The Makefile properties are STRUCTURAL -- the same
approach as MakefileBenchGraph in test_reqbench.py, which pins the bench
dependency graph. The shell properties are BEHAVIOURAL: the block under test is
lifted out of the shipped script and executed, so the test runs the same bytes
the campaign runs rather than a paraphrase of them.

Run: python3 -m unittest test_campaign -v
"""

import http.server
import os
import re
import subprocess
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
MAKEFILE = os.path.join(REPO, "Makefile")
CAMPAIGN = os.path.join(HERE, "corpus_campaign.sh")


def makefile() -> str:
    with open(MAKEFILE) as handle:
        return handle.read()


def campaign() -> str:
    with open(CAMPAIGN) as handle:
        return handle.read()


class RunPackaging(unittest.TestCase):
    """One run, one directory, one stamp -- decided once."""

    def test_the_campaign_stamp_is_simply_expanded(self):
        """`?=`/`=` with $(shell date) RE-RUNS the shell on every reference.

        A campaign that crosses a second boundary between two references then
        creates one directory and writes later artifacts into another. checkmake
        flags exactly this (timestampexpanded), and it defeats the entire point
        of packaging a run: the contamination it prevents is the contamination
        it would introduce.
        """
        match = re.search(r"^CORPUS_STAMP\s*(\??:?=)", makefile(), re.M)
        self.assertIsNotNone(match, "CORPUS_STAMP is gone from the Makefile")
        self.assertEqual(
            match.group(1), ":=",
            "CORPUS_STAMP is recursively expanded, so $(shell date) re-runs on "
            "every reference and one run can straddle two stamps",
        )

    def test_the_run_directory_is_reserved_and_not_reused(self):
        """`mkdir -p` accepts an existing directory.

        Two campaigns under one stamp interleave their records, and nothing in a
        record says which run wrote it -- so it cannot be untangled afterwards.
        The reservation must FAIL on collision.
        """
        body = makefile()
        self.assertRegex(
            body, r'@mkdir "\$\(CORPUS_RUN_DIR\)" \|\|',
            "the run directory is not reserved with a failing plain mkdir",
        )
        self.assertNotRegex(
            body, r'@mkdir -p "\$\(CORPUS_RUN_DIR\)"\s*$',
            "mkdir -p on the run directory silently reuses an existing campaign",
        )


class Provenance(unittest.TestCase):
    """A record is only reproducible while the commit it cites is reachable."""

    def test_the_revision_pin_cannot_move_or_be_skipped(self):
        """`git branch -f ... || true` fails both ways.

        `-f` MOVES a ref an earlier record may already cite, and `|| true`
        continues when the pin fails -- so SOURCE_REF can name a ref that has
        since moved, or no local ref at all. Either way the record cites a
        commit nobody can recover, which is the one thing writing SOURCE_REF
        exists to prevent.
        """
        pin = re.search(r"git -C \"\$\(CURDIR\)\" branch[^\n]*", makefile())
        self.assertIsNotNone(pin, "the revision pin is gone")
        self.assertNotIn("branch -f", pin.group(0),
                         "the pin can move a ref an earlier record cites")
        self.assertNotIn("|| true", pin.group(0),
                         "a failed pin is swallowed, so SOURCE_REF can name nothing")


class Teardown(unittest.TestCase):
    def test_a_failed_dnsmasq_restore_fails_the_target(self):
        """The campaign takes 127.0.0.1:53 from dnsmasq.

        Suppressing the restart error let bench-stop print "clean" over a box
        left with no DNS resolution -- the exact failure a teardown target
        exists to prevent.
        """
        stop = makefile().split("bench-stop:", 1)[1].split("\n.PHONY")[0]
        block = stop.split("restoring host services", 1)[1]
        self.assertNotRegex(
            block, r"@-sudo systemctl start dnsmasq[^\n]*\|\| true",
            "a failed dnsmasq restart is suppressed, so teardown reports clean "
            "on a box with no DNS",
        )
        self.assertIn("exit 1", block,
                      "teardown cannot fail when it leaves the host broken")


class LogFilter(unittest.TestCase):
    """The one variable the analysis cannot do without."""

    def _run_filter_block(self, env):
        """Execute the shipped RUST_LOG block and report what it exports."""
        block = re.search(
            r"(if \[ -n \"\$\{RUST_LOG:-\}\".*?export RUST_LOG=\"fcvm=debug\")",
            campaign(), re.S)
        self.assertIsNotNone(block, "the RUST_LOG block is gone from the campaign")
        script = block.group(1) + '\nprintf "%s" "$RUST_LOG"\n'
        result = subprocess.run(["bash", "-c", script], env={**os.environ, **env},
                                capture_output=True, text=True, timeout=30)
        return result

    def test_an_inherited_log_filter_is_overridden_not_honoured(self):
        """`${RUST_LOG:-fcvm=debug}` let an ambient RUST_LOG=info through.

        The stage attribution the analysis relies on exists only in fcvm=debug
        output, and a missing stage looks exactly like a stage that took no
        time -- so the loss is invisible downstream. An ambient RUST_LOG is
        precisely what a developer's shell already has set.
        """
        result = self._run_filter_block({"RUST_LOG": "info"})
        self.assertEqual(result.stdout, "fcvm=debug",
                         f"the campaign ran with RUST_LOG={result.stdout!r}, "
                         "which carries no stage records")
        self.assertIn("overriding inherited", result.stderr,
                      "the override is silent; a reader would not know their "
                      "RUST_LOG was discarded")

    def test_an_absent_log_filter_still_gets_the_required_one(self):
        env = {k: v for k, v in os.environ.items() if k != "RUST_LOG"}
        result = subprocess.run(
            ["bash", "-c", re.search(
                r"(if \[ -n \"\$\{RUST_LOG:-\}\".*?export RUST_LOG=\"fcvm=debug\")",
                campaign(), re.S).group(1) + '\nprintf "%s" "$RUST_LOG"\n'],
            env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.stdout, "fcvm=debug")


class PartialCorpus(unittest.TestCase):
    """A replay miss renders an error page, which is a fast plausible number."""

    class OnlyOne(http.server.BaseHTTPRequestHandler):
        served = "/served"

        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path == self.served:
                self.send_response(200)
            else:
                self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()

    @staticmethod
    def retarget(probe: str, port: int) -> str:
        """Point the shipped probe at a local stub, changing only the address.

        --resolve maps a host to an ADDRESS, not to a port, so it cannot reach a
        stub on an ephemeral port; --connect-to can. Everything else -- the
        loop, the host extraction, the status classification, the accumulation
        and the refusal -- is the shipped text, because that is what is under
        test. Substituting only the address mapping is what keeps this a test of
        corpus_campaign.sh rather than of a paraphrase of it.

        The first version of this substitution reused --resolve and every
        request returned 000, so the negative case passed for the wrong reason:
        it refused a corpus because NOTHING was reachable. The positive control
        below is what exposed that, which is why it exists.
        """
        probe = probe.replace(
            '--resolve "$host:443:127.0.0.1"',
            f'--connect-to "$host:80:127.0.0.1:{port}"')
        return probe.replace("https://", "http://").replace("curl -sk", "curl -s")

    def test_a_corpus_missing_a_url_blocks_the_campaign(self):
        """The preflight probed ONE url to detect startup and stopped there.

        A corpus holding only blog.cloudflare.com passed it, and the other 13
        URLs failed inside the MEASURED run -- where the failure is not an error
        but a render of an error page. This drives the shipped probe loop
        against a server that serves one of two URLs and requires it to refuse.
        """
        block = re.search(r"(missing=\"\"\n.*?\nfi)\n", campaign(), re.S)
        self.assertIsNotNone(block, "the corpus completeness probe is gone")

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.OnlyOne)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        port = server.server_port

        probe = self.retarget(block.group(1), port)

        with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as script:
            script.write(f'set -u\nURLS="http://x/served,http://x/absent"\n{probe}\n'
                         'echo COMPLETE\n')
            path = script.name
        self.addCleanup(os.unlink, path)

        result = subprocess.run(["bash", path], capture_output=True, text=True,
                                timeout=60)
        self.assertEqual(result.returncode, 3,
                         "a corpus missing a URL was accepted; those renders "
                         f"would be error pages\n{result.stdout}{result.stderr}")
        self.assertNotIn("COMPLETE", result.stdout)
        self.assertIn("/absent", result.stderr,
                      "the refusal does not name the URL that is missing")
        self.assertNotIn("/served", result.stderr,
                         "the URL that IS served was also reported missing, so "
                         "this refused because nothing was reachable rather "
                         "than because the corpus is partial")

    def test_a_complete_corpus_passes(self):
        """The guard must not refuse a corpus that IS complete."""
        block = re.search(r"(missing=\"\"\n.*?\nfi)\n", campaign(), re.S)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.OnlyOne)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        probe = self.retarget(block.group(1), server.server_port)
        with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as script:
            script.write(f'set -u\nURLS="http://x/served"\n{probe}\necho COMPLETE\n')
            path = script.name
        self.addCleanup(os.unlink, path)
        result = subprocess.run(["bash", path], capture_output=True, text=True,
                                timeout=60)
        self.assertIn("COMPLETE", result.stdout,
                      f"a complete corpus was refused\n{result.stderr}")


if __name__ == "__main__":
    unittest.main()
