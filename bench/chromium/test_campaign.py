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

import hashlib
import http.server
import json
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


def read_if_exists(path, default=""):
    if not os.path.exists(path):
        return default
    with open(path) as handle:
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
        for var in ("CORPUS_STAMP", "CORPUS_RUN_DIR"):
            match = re.search(rf"^{var}\s*(\??:?=)", makefile(), re.M)
            self.assertIsNotNone(match, f"{var} is gone from the Makefile")
            self.assertEqual(
                match.group(1), ":=",
                f"{var} is recursively expanded; moving $(shell date) into any "
                "recursively expanded variable re-runs it per reference, and one "
                "run can straddle two stamps",
            )

    def test_the_run_directory_is_reserved_and_not_reused(self):
        """`mkdir -p` accepts an existing directory.

        Two campaigns under one stamp interleave their records, and nothing in a
        record says which run wrote it -- so it cannot be untangled afterwards.
        The reservation must FAIL on collision.
        """
        body = makefile()
        # Behavioural, not structural: run the reservation recipe against a
        # pre-created directory and require make itself to fail. The structural
        # version regexed for `@mkdir ... ||` and never looked past the `||`,
        # so `exit 1` -> `exit 0` survived it -- the recipe still PRINTED
        # "REFUSING" and then proceeded into the existing directory, which is
        # the exact contamination the reservation exists to prevent.
        block = re.search(
            r'(@mkdir "\$\(CORPUS_RUN_DIR\)" \|\| \{ \\\n.*?\})', body, re.S)
        self.assertIsNotNone(block, "the reservation block is gone")
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = os.path.join(tmp, "corpus-x")
            os.mkdir(run_dir)  # the collision
            recipe = block.group(1).replace("$(CORPUS_RUN_DIR)", run_dir)
            mk = os.path.join(tmp, "mk")
            with open(mk, "w") as handle:
                handle.write("reserve:\n\t" + recipe + "\n")
            result = subprocess.run(["make", "-f", mk, "reserve"],
                                    capture_output=True, text=True, timeout=60)
            self.assertNotEqual(
                result.returncode, 0,
                "the reservation printed its refusal and PROCEEDED into an "
                "existing campaign directory\n" + result.stdout + result.stderr)
            self.assertIn("REFUSING", result.stdout + result.stderr)
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
        # Join continuations FIRST: make folds backslash-newline into one shell
        # command, so `|| true` on the next physical line is semantically on
        # this one -- and a single-line regex cannot see it.
        joined = re.sub(r"\\\n", " ", makefile())
        pin = re.search(r"git -C \"\$\(CURDIR\)\" branch[^\n]*", joined)
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
        # And the recipe line carrying that exit must not be prefixed `-`:
        # make then ignores the failure and the target exits 0 over a box with
        # no DNS, which is the same fail-open with one more character.
        retry = re.search(r"^\t(@?-?)for i in", block, re.M)
        self.assertIsNotNone(retry, "the dnsmasq retry loop is gone")
        self.assertNotIn("-", retry.group(1),
                         "the dnsmasq retry recipe is `-` prefixed, so make "
                         "ignores its exit 1 and teardown reports clean anyway")


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


class LocalProbesIgnoreProxies(unittest.TestCase):
    """The replay probes talk to 127.0.0.1 and nothing else.

    A proxy in the environment (a bench host behind an egress proxy, a shell
    with http_proxy exported) sends curl to the proxy instead of the replay
    server unless the URL's host is in no_proxy, and the corpus hosts are not.
    A working proxy then fetches the LIVE site, so an incomplete replay passes
    the completeness probe on live status codes; an unreachable proxy rejects a
    replay that is complete. Neither reads as a proxy problem: the first is a
    plausible campaign, the second is `BLOCKED: HTTPS replay returned '000'`.

    Both probes run here against a local stub with every proxy variable set to
    an address nothing listens on (127.0.0.1:9). curl reads http_proxy in lower
    case only (the CGI convention) and https_proxy in either case; all four are
    set so the retargeted http:// probe sees what the shipped https:// one
    would. A control runs the same probe with --noproxy removed and requires it
    to fail, so a pass cannot come from an environment curl never saw. dig
    honours no proxy variable and needs nothing.

    Watched red 2026-08-28 against corpus_campaign.sh at 13cb9543; the failure
    text is quoted on each test.
    """

    DEAD_PROXY = "http://127.0.0.1:9"
    READINESS = re.compile(
        r'(answer=""\ncode=""\nfor _ in \$\(seq 1 100\); do\n.*?\ndone\n'
        r'\[ "\$answer" = "10\.0\.2\.2" \][^\n]*\n\[ "\$code" = "200" \][^\n]*\n)', re.S)

    def _env(self) -> dict:
        env = {k: v for k, v in os.environ.items()
               if k.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
        for name in ("http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"):
            env[name] = self.DEAD_PROXY
        return env

    def _stub(self, served: str) -> int:
        class Stub(PartialCorpus.OnlyOne):
            pass

        Stub.served = served
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server.server_port

    def _run(self, script: str, env: dict):
        with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as handle:
            handle.write(script)
            path = handle.name
        self.addCleanup(os.unlink, path)
        return subprocess.run(["bash", path], env=env, capture_output=True,
                              text=True, timeout=60)

    def _control(self, script: str, env: dict):
        """The same probe with --noproxy removed must go to the dead proxy."""
        stripped = script.replace("--noproxy '*' ", "")
        control = self._run(stripped, env)
        self.assertEqual(control.returncode, 3,
                         "the control probe reached the stub through the proxy "
                         "environment, so a pass here would prove nothing\n"
                         + control.stdout + control.stderr)
        return control

    def test_the_completeness_probe_reaches_the_replay_server_through_a_proxy_environment(self):
        """Red: exit 3, `BLOCKED: the corpus does not serve every configured
        URL:` naming `000  http://x/served`."""
        block = re.search(r"(missing=\"\"\n.*?\nfi)\n", campaign(), re.S)
        self.assertIsNotNone(block, "the corpus completeness probe is gone")
        probe = PartialCorpus.retarget(block.group(1), self._stub("/served"))
        script = f'set -u\nURLS="http://x/served"\n{probe}\necho COMPLETE\n'
        env = self._env()
        control = self._control(script, env)
        self.assertIn("000  http://x/served", control.stderr)
        result = self._run(script, env)
        self.assertEqual(result.returncode, 0,
                         "a complete corpus was refused because curl went to the "
                         f"proxy instead of 127.0.0.1\n{result.stdout}{result.stderr}")
        self.assertIn("COMPLETE", result.stdout)

    def test_the_readiness_probe_reaches_the_replay_server_through_a_proxy_environment(self):
        """Red: exit 3, `BLOCKED: HTTPS replay returned '000' for blog.cloudflare.com`."""
        block = self.READINESS.search(campaign())
        self.assertIsNotNone(block, "the replay readiness probe is gone")
        port = self._stub("/")
        probe = (block.group(1)
                 .replace("--resolve 'blog.cloudflare.com:443:127.0.0.1' "
                          "https://blog.cloudflare.com/",
                          f"--connect-to 'blog.cloudflare.com:80:127.0.0.1:{port}' "
                          "http://blog.cloudflare.com/")
                 .replace("curl -sk", "curl -s")
                 # The shipped loop polls 100 times at 0.2 s for a server that
                 # is still binding; the stub is up before the probe starts, so
                 # two rounds bound the failing case at under a second.
                 .replace("seq 1 100", "seq 1 2"))
        for applied in ("--connect-to", "http://blog.cloudflare.com/", "seq 1 2"):
            self.assertIn(applied, probe, "the retarget did not apply; the probe "
                                          "would hit 127.0.0.1:443, not the stub")
        self.assertNotIn("-sk", probe)
        with tempfile.TemporaryDirectory() as tmp:
            binx = os.path.join(tmp, "bin")
            logs = os.path.join(tmp, "logs")
            os.makedirs(binx)
            os.makedirs(logs)
            for name, body in (("dig", "#!/bin/bash\necho 10.0.2.2\n"),
                               ("sudo", DnsBrackets.FAKE_SUDO)):
                path = os.path.join(binx, name)
                with open(path, "w") as handle:
                    handle.write(body)
                os.chmod(path, 0o755)
            open(os.path.join(logs, "corpus_serve.log"), "w").close()
            env = self._env()
            env["PATH"] = binx + os.pathsep + env["PATH"]
            script = (f'set -euo pipefail\nLOGDIR="{logs}"\nSERVE_PID=$$\n'
                      f'{probe}\necho READY\n')
            control = self._control(script, env)
            self.assertIn("returned '000'", control.stderr)
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0,
                             "a replay server that is up was reported down because "
                             f"curl went to the proxy instead of 127.0.0.1\n{result.stderr}")
            self.assertIn("READY", result.stdout)


class DnsBrackets(unittest.TestCase):
    """Resolver evidence around the measured run: verify on each side, sample between.

    The replay wiring (guest resolv.conf -> pasta gateway -> corpus_serve on
    127.0.0.1:53) is only evidence if it held for the WHOLE measured run. A
    dnsmasq restart, a server leaked from an earlier campaign, or a golden that
    ignored --dns would hand the guest a different resolver, and nothing in a
    record would say so. Three brackets close that: reqbench's HOP D verify
    before and after the run (and once more on golden reuse), a sampler naming
    the owner of 127.0.0.1:53 every DNS_SAMPLE_INTERVAL seconds while the run
    is in flight, and dns-evidence.json tying them to the replay server's logs.

    Behavioural where the block can run without a VM (the helper functions are
    lifted out of the shipped script and executed against fakes on PATH, as
    PartialCorpus does), structural for the ordering of the calls in the main
    flow.
    """

    HELPERS = re.compile(
        r"(engine_target\(\) \{\n.*?\ncampaign_fail\(\) \{\n.*?\n\}\n)", re.S)
    GOLDEN = re.compile(
        r"(GUEST_DNS=10\.0\.2\.2[^\n]*\\\n[^\n]*engine_target golden\)[^\n]*)")
    URL_LINE = re.compile(r'^URLS="([^"]+)"$', re.M)

    FAKE_MAKE = """#!/bin/bash
env > "$MAKE_ENV_DUMP"
echo "$*" > "$MAKE_ARGV"
[ -z "${MAKE_VERIFY_JSON:-}" ] || printf '%s\\n' "$MAKE_VERIFY_JSON" > "$RESULTS/verify-dns.json"
[ -z "${MAKE_DIAG_JSON:-}" ] || { mkdir -p "$RESULTS/diag"; printf '%s\\n' "$MAKE_DIAG_JSON" > "$RESULTS/diag/summary.json"; }
exit "${MAKE_RC:-0}"
"""
    # Decoy :53 listeners with OTHER pids, as on the bench host itself:
    # systemd-resolved on 127.0.0.53 and dnsmasq on the bridge addresses. Only
    # the 127.0.0.1:53 line names the replay server, and its pid changes from
    # the SS_CHANGE_AT-th call on.
    FAKE_SS = """#!/bin/bash
n=$(cat "$SS_CALLS" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$SS_CALLS"
if [ -n "${SS_FAIL_AT:-}" ] && [ "$n" -ge "$SS_FAIL_AT" ]; then exit 1; fi
pid="$SS_OWNER"
if [ -n "${SS_CHANGE_AT:-}" ] && [ "$n" -ge "$SS_CHANGE_AT" ]; then pid="$SS_OTHER"; fi
cat <<SSOUT
State  Recv-Q Send-Q Local Address:Port Peer Address:PortProcess
UNCONN 0      0          127.0.0.53%lo:53        0.0.0.0:*    users:(("systemd-resolve",pid=17853,fd=14))
UNCONN 0      0          192.168.94.37:53        0.0.0.0:*    users:(("dnsmasq",pid=1363,fd=55))
UNCONN 0      0              127.0.0.1:53        0.0.0.0:*    users:(("python3",pid=$pid,fd=7))
SSOUT
"""
    FAKE_SYSTEMCTL = """#!/bin/bash
quiet=0; args=()
for a in "$@"; do if [ "$a" = --quiet ]; then quiet=1; else args+=("$a"); fi; done
[ "${args[0]:-}" = is-active ] || exit 1
[ "$quiet" = 1 ] || echo "$FAKE_DNSMASQ_STATE"
[ "$FAKE_DNSMASQ_STATE" = active ] && exit 0 || exit 3
"""
    FAKE_SUDO = '#!/bin/bash\n[ "${1:-}" = -n ] && shift\nexec "$@"\n'

    @staticmethod
    def _hosts_of(urls: str) -> list:
        hosts = []
        for url in urls.split(","):
            host = url.split("://", 1)[1].split("/", 1)[0]
            if host not in hosts:
                hosts.append(host)
        return hosts

    def _bracket_evidence(self, corpus_urls: str, **overrides) -> str:
        """verify-dns.json as HOP D writes it for a passing corpus clone."""
        evidence = {
            "dns_server": "10.0.2.2",
            "resolv_conf_vm": "nameserver 10.0.2.2\n",
            "resolv_conf_container": "nameserver 10.0.2.2\n",
            "hosts": {h: {"answer": "10.0.2.2", "ok": True}
                      for h in self._hosts_of(corpus_urls)},
            "urls": {u: {"status": 200, "ok": True} for u in corpus_urls.split(",")},
            "proxies_disabled": True,
            "timestamp": "2026-08-28T00:00:00Z",
            "passed": True,
        }
        evidence.update(overrides)
        return json.dumps(evidence)

    def _helpers(self) -> str:
        block = self.HELPERS.search(campaign())
        self.assertIsNotNone(block, "the resolver evidence helpers are gone "
                                    "from the campaign (engine_target .. campaign_fail)")
        return block.group(1)

    def _urls(self) -> str:
        line = self.URL_LINE.search(campaign())
        self.assertIsNotNone(line, "the corpus URL list is gone")
        return line.group(1)

    def _fakes(self, tmp, dnsmasq="inactive"):
        """Fake make/ss/systemctl/sudo on PATH; each records what it saw."""
        binx = os.path.join(tmp, "bin")
        os.makedirs(binx, exist_ok=True)
        for name, body in (("make", self.FAKE_MAKE), ("ss", self.FAKE_SS),
                           ("systemctl", self.FAKE_SYSTEMCTL), ("sudo", self.FAKE_SUDO)):
            path = os.path.join(binx, name)
            with open(path, "w") as handle:
                handle.write(body)
            os.chmod(path, 0o755)
        results = os.path.join(tmp, "results")
        logs = os.path.join(tmp, "logs")
        os.makedirs(results, exist_ok=True)
        os.makedirs(logs, exist_ok=True)
        # /proc/loadavg's shape, at a value the real box will not show, so a
        # sampler that read the real file instead of LOADAVG_FILE is caught.
        loadavg = os.path.join(tmp, "loadavg")
        self._set_load(loadavg, "0.42")
        env = dict(os.environ)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            MAKE_ENV_DUMP=os.path.join(tmp, "make-env"),
            MAKE_ARGV=os.path.join(tmp, "make-argv"),
            SS_CALLS=os.path.join(tmp, "ss-calls"),
            SS_OWNER="4242", SS_OTHER="999",
            FAKE_DNSMASQ_STATE=dnsmasq,
            LOADAVG_FILE=loadavg,
            RESULTS=results, LOGDIR=logs, REPO=tmp, TAG="cb-req-corpus",
            ENGINE="chromium",
        )
        return env, results

    @staticmethod
    def _set_load(loadavg, load1):
        """Write, then rename over: the sampler reads this file by name every
        DNS_SAMPLE_INTERVAL, and a truncate followed by a write leaves it a
        window in which the load column is empty."""
        with open(loadavg + ".new", "w") as handle:
            handle.write(f"{load1} 0.30 0.20 1/100 1234\n")
        os.replace(loadavg + ".new", loadavg)

    def _make_env(self, env):
        seen = {}
        with open(env["MAKE_ENV_DUMP"]) as handle:
            for line in handle.read().splitlines():
                key, _, value = line.partition("=")
                seen[key] = value
        return seen

    def _run(self, script, env, timeout=60):
        return subprocess.run(["bash", "-c", script], env=env,
                              capture_output=True, text=True, timeout=timeout)

    def test_verify_carries_the_corpus_resolver_knobs_and_keeps_its_evidence(self):
        """run_verify must hand reqbench every corpus host and URL, the replay
        answer, and THIS run's RESULTS, and keep the bracket's evidence under
        its own name, since reqbench overwrites verify-dns.json."""
        urls = self._urls()
        hosts = []
        for url in urls.split(","):
            host = url.split("://", 1)[1].split("/", 1)[0]
            if host not in hosts:
                hosts.append(host)
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            env["MAKE_VERIFY_JSON"] = self._bracket_evidence(urls)
            script = (f'set -euo pipefail\nsay() {{ :; }}\nURLS="{urls}"\n{self._helpers()}\n'
                      'CORPUS_HOSTS=$(corpus_hosts)\nrun_verify pre\necho VERIFIED\n')
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("VERIFIED", result.stdout)
            seen = self._make_env(env)
            self.assertEqual(seen.get("VERIFY_DNS_HOSTS"), ",".join(hosts),
                             "verify was not told the corpus hosts")
            self.assertEqual(seen.get("VERIFY_DNS_ANSWER"), "10.0.2.2")
            self.assertEqual(seen.get("VERIFY_DNS_URLS"), urls)
            self.assertEqual(seen.get("RESULTS"), results,
                             "verify's logs land outside the run directory")
            self.assertEqual(seen.get("TAG"), "cb-req-corpus")
            with open(env["MAKE_ARGV"]) as handle:
                self.assertIn("bench-chromium-request-verify", handle.read())
            with open(os.path.join(results, "verify-dns-pre.json")) as handle:
                self.assertEqual(json.load(handle), json.loads(env["MAKE_VERIFY_JSON"]))

    def test_verify_refuses_failed_or_missing_resolver_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            base = (f'set -euo pipefail\nsay() {{ :; }}\nURLS="{self._urls()}"\n{self._helpers()}\n'
                    'CORPUS_HOSTS=$(corpus_hosts)\n')
            # verify's make exits 0 but HOP D recorded passed=false.
            env["MAKE_VERIFY_JSON"] = self._bracket_evidence(self._urls(), passed=False)
            result = self._run(base + "run_verify before-run\necho VERIFIED\n", env)
            self.assertNotEqual(result.returncode, 0, "passed=false was accepted")
            self.assertNotIn("VERIFIED", result.stdout)
            self.assertTrue(os.path.exists(os.path.join(results, "verify-dns-before-run.json")),
                            "the failed bracket's evidence was not kept")
            # verify wrote nothing at all (HOP D never ran).
            env.pop("MAKE_VERIFY_JSON")
            result = self._run(base + "run_verify after-run\necho VERIFIED\n", env)
            self.assertNotEqual(result.returncode, 0,
                                "a verify with no HOP D evidence was accepted")
            self.assertIn("verify-dns.json", result.stderr)
            # verify's make itself failed.
            env["MAKE_RC"] = "1"
            env["MAKE_VERIFY_JSON"] = self._bracket_evidence(self._urls())
            result = self._run(base + "run_verify pre\necho VERIFIED\n", env)
            self.assertNotEqual(result.returncode, 0, "a failed verify make was accepted")
            self.assertNotIn("VERIFIED", result.stdout)

    def test_a_bracket_that_asserted_nothing_is_refused(self):
        """`passed: true` is what HOP D writes when it was given nothing to
        check. run_verify accepted it, so a bracket with an empty host list
        (or a resolver other than the replay's, or a host it never asked
        about) read as clean.

        RED BEFORE THE FIX: `{"passed": true}` and the other three shapes
        all printed VERIFIED with exit 0.
        """
        urls = self._urls()
        hosts = self._hosts_of(urls)
        partial = json.loads(self._bracket_evidence(urls))
        del partial["hosts"][hosts[-1]]
        cases = {
            "asserted nothing": '{"passed": true}',
            "another resolver": self._bracket_evidence(urls, dns_server="10.0.2.3"),
            "a host never asked": json.dumps(partial),
            "a url that failed": self._bracket_evidence(
                urls, urls={u: {"status": 404, "ok": False} for u in urls.split(",")}),
            # HOP D's URL probe records that it ran with proxies disabled;
            # a bracket that does not say so may have fetched through the
            # proxy fc-agent injects into the exec, live site and all.
            "proxies not disabled": self._bracket_evidence(urls, proxies_disabled=False),
            "proxy handling unrecorded": self._bracket_evidence(urls, proxies_disabled=None),
        }
        for label, evidence in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                env, _results = self._fakes(tmp)
                env["MAKE_VERIFY_JSON"] = evidence
                script = (f'set -euo pipefail\nsay() {{ :; }}\nURLS="{urls}"\n'
                          f'{self._helpers()}\nCORPUS_HOSTS=$(corpus_hosts)\n'
                          'run_verify pre\necho VERIFIED\n')
                result = self._run(script, env)
                self.assertNotEqual(result.returncode, 0,
                                    f"{label}: accepted\n{result.stdout}")
                self.assertNotIn("VERIFIED", result.stdout)

    def test_a_stale_read_only_bracket_copy_cannot_stand_in(self):
        """run_verify is called as `run_verify pre || campaign_fail ...`, and
        bash turns errexit off inside a function invoked that way. The copy
        into verify-dns-<stage>.json was unchecked, so when it failed the
        `jq -e` that follows validated whatever copy was already there.

        RED BEFORE THE FIX: VERIFIED printed, exit 0, over a bracket whose
        fresh evidence says passed=false.
        """
        urls = self._urls()
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            stale = os.path.join(results, "verify-dns-pre.json")
            with open(stale, "w") as handle:
                handle.write(self._bracket_evidence(urls))
            os.chmod(stale, 0o444)
            env["MAKE_VERIFY_JSON"] = self._bracket_evidence(urls, passed=False)
            script = (f'set -euo pipefail\nsay() {{ :; }}\nURLS="{urls}"\n'
                      f'{self._helpers()}\nCORPUS_HOSTS=$(corpus_hosts)\n'
                      'run_verify pre || { echo REFUSED; exit 7; }\necho VERIFIED\n')
            try:
                result = self._run(script, env)
            finally:
                if os.path.exists(stale):
                    os.chmod(stale, 0o644)
            self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
            self.assertNotIn("VERIFIED", result.stdout)
            with open(stale) as handle:
                self.assertIs(json.load(handle)["passed"], False,
                              "the stale copy survived under the stage name")

    def test_the_run_sub_make_receives_the_stall_limit(self):
        """reqanalyze's stall gate is armed by STALL_MAX_MS, and
        campaign_summary refuses an analysis whose gate was never armed;
        the campaign passed nothing, so every run it produced was
        un-indexable by construction.

        RED BEFORE THE FIX: no STALL_MAX_MS default line in the campaign,
        and the run's make saw no STALL_MAX_MS.
        """
        body = campaign()
        default = re.search(r'^STALL_MAX_MS="\$\{STALL_MAX_MS:-(\d+)\}"$', body, re.M)
        self.assertIsNotNone(default, "the campaign sets no STALL_MAX_MS default")
        self.assertEqual(default.group(1), "15000")
        block = re.search(r'(TAG="\$TAG" URL="\$URLS" BACKEND=.*?\|\| run_rc=\$\?)',
                          body, re.S)
        self.assertIsNotNone(block, "the measured run invocation is gone")
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            env.pop("STALL_MAX_MS", None)
            script = ('set -euo pipefail\nsay() { :; }\n'
                      f'URLS="{self._urls()}"\nBACKEND=uffd\nUFFD_MODE=minor\n'
                      'UFFD_PREFETCH=on\nARMS=noop,cdp\nREPS=1\nWARMUP=1\n'
                      f'{default.group(0)}\n{self._helpers()}\nrun_rc=0\n'
                      f'{block.group(1)}\necho "run_rc=$run_rc"\n')
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            seen = self._make_env(env)
            self.assertEqual(seen.get("STALL_MAX_MS"), "15000")
            self.assertEqual(seen.get("RESULTS"), results)
            with open(env["MAKE_ARGV"]) as handle:
                self.assertIn("bench-chromium-request-run", handle.read())

    # The rm may continue over backslash-newlines; the block runs whole.
    START_CLEANUP = re.compile(
        r'(mkdir -p "\$RESULTS"\n(?:#[^\n]*\n)*rm -f (?:[^\n]*\\\n)*[^\n]*\n)')

    def _run_start_cleanup(self, results: str):
        """Execute the block after `mkdir -p "$RESULTS"` that clears an
        earlier campaign's evidence, against a pre-filled RESULTS."""
        block = self.START_CLEANUP.search(campaign())
        self.assertIsNotNone(block, "the campaign does not clear stale evidence at start")
        return subprocess.run(
            ["bash", "-c", f'set -euo pipefail\nRESULTS="{results}"\n{block.group(1)}'],
            capture_output=True, text=True, timeout=30)

    def test_stale_evidence_from_an_earlier_campaign_is_cleared_at_start(self):
        """An explicit RESULTS can be reused. A clean dns-evidence.json left
        by an earlier campaign, beside a run this campaign never finished,
        would be indexed as if it were this run's.

        RED BEFORE THE FIX: the block after `mkdir -p "$RESULTS"` removed
        nothing.
        """
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "results")
            os.makedirs(results)
            stale = ["dns-evidence.json", "verify-dns-after-run.json",
                     "verify-dns.json", "dns-owner.log", "corpus-serve.status",
                     "diag/summary.json"]
            os.makedirs(os.path.join(results, "diag"))
            for name in stale:
                with open(os.path.join(results, name), "w") as handle:
                    handle.write('{"verdict": "clean", "passed": true}\n')
            bundle = os.path.join(results, "runtime", "c" * 64, "MANIFEST.sha256")
            os.makedirs(os.path.dirname(bundle))
            with open(bundle, "w") as handle:
                handle.write("0" * 64 + "  fcvm\n")
            result = self._run_start_cleanup(results)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in stale:
                self.assertFalse(os.path.exists(os.path.join(results, name)),
                                 f"stale {name} survived the campaign start")
            self.assertTrue(os.path.exists(bundle),
                            "the start wiped more than the evidence")

    def test_stale_replay_logs_from_an_earlier_campaign_are_cleared_at_start(self):
        """corpus_serve opens both replay logs for append (JsonlLog), so a
        reused RESULTS carries an earlier attempt's queries and requests into
        the logs this campaign hashes into dns-evidence.json as its own. The
        server writes them from its first query, so they go with the rest of
        the stale evidence, before it starts.

        RED BEFORE THE FIX: `stale corpus-dns.log survived the campaign start`.
        """
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "results")
            os.makedirs(results)
            for name in self.REPLAY_LOGS:
                with open(os.path.join(results, name), "w") as handle:
                    handle.write('{"ts": 0, "peer": "earlier attempt"}\n')
            result = self._run_start_cleanup(results)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in self.REPLAY_LOGS:
                self.assertFalse(os.path.exists(os.path.join(results, name)),
                                 f"stale {name} survived the campaign start")

    def test_a_stale_run_record_from_an_earlier_attempt_is_cleared_at_start(self):
        """reqbench.py opens reqbench.jsonl for append, so a retry into a
        reused RESULTS appends a second run_id and reqanalyze emits a pooled
        multi-run analysis with no top-level cell, which campaign_summary
        refuses. A retry that fails before its own analysis leaves the
        earlier attempt's analysis.json beside this attempt's fresh DNS
        evidence. Both go at start. The content-addressed runtime bundles
        and the phase logs are not this attempt's record and stay.

        RED BEFORE THE FIX: `stale reqbench.jsonl survived the campaign start`.
        """
        with tempfile.TemporaryDirectory() as tmp:
            results = os.path.join(tmp, "results")
            os.makedirs(os.path.join(results, "logs"))
            stale = {
                "reqbench.jsonl": '{"kind": "meta", "run_id": "earlier"}\n',
                "analysis.json": '{"publishable": true, "run_id": "earlier"}\n',
            }
            for name, text in stale.items():
                with open(os.path.join(results, name), "w") as handle:
                    handle.write(text)
            kept = [os.path.join(results, "runtime", "c" * 64, "MANIFEST.sha256"),
                    os.path.join(results, "logs", "golden.log")]
            os.makedirs(os.path.dirname(kept[0]))
            for path in kept:
                with open(path, "w") as handle:
                    handle.write("earlier attempt\n")
            result = self._run_start_cleanup(results)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in stale:
                self.assertFalse(os.path.exists(os.path.join(results, name)),
                                 f"stale {name} survived the campaign start")
            for path in kept:
                self.assertTrue(os.path.exists(path),
                                f"the start wiped {os.path.relpath(path, results)}")

    def test_the_golden_sub_make_receives_results_and_the_baked_resolver(self):
        block = self.GOLDEN.search(campaign())
        self.assertIsNotNone(block, "the golden invocation is gone or no longer "
                                    "names engine_target golden")
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            script = f'set -euo pipefail\nsay() {{ :; }}\n{self._helpers()}\n{block.group(1)}\n'
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            seen = self._make_env(env)
            self.assertEqual(seen.get("RESULTS"), results,
                             "the golden's logs land outside the run directory")
            self.assertEqual(seen.get("GUEST_DNS"), "10.0.2.2")
            with open(env["MAKE_ARGV"]) as handle:
                self.assertIn("bench-chromium-request-golden", handle.read())

    def test_every_phase_verifies_on_both_sides_of_the_measured_run(self):
        """Ordering in the main flow, which cannot run without a VM."""
        body = campaign()
        phases = re.search(r'if \[ "\$PHASE" = all \]; then\n(.*?)\nelse\n(.*?)\nfi\n',
                           body, re.S)
        self.assertIsNotNone(phases, "the PHASE branch is gone")
        self.assertIn("run_verify pre", phases.group(1))
        self.assertIn("run_verify pre", phases.group(2),
                      "PHASE=run reuses the golden without verifying its resolver")
        quiet = body.index('say "box quiet')
        before = body.index("run_verify before-run")
        sampler_on = body.index("start_dns_sampler\n", before)
        run = body.index("engine_target run)")
        # From `run` on: the exit trap calls stop_dns_sampler too, earlier in
        # the file.
        sampler_off = body.index("stop_dns_sampler\n", run)
        after = body.index("run_verify after-run")
        evidence = body.index("write_dns_evidence ", after)
        self.assertLess(quiet, before, "before-run verify precedes the settle wait")
        self.assertLess(before, sampler_on, "sampler starts before the before-run verify")
        self.assertLess(sampler_on, run, "sampler starts after the measured run")
        self.assertLess(run, sampler_off, "sampler stops before the measured run")
        self.assertLess(sampler_off, after, "after-run verify runs under the sampler")
        self.assertLess(after, evidence, "evidence is written before the after-run verify")
        # The evidence hashes the replay server's logs; a server still
        # serving appends after the hash. It stops first, and the exit trap
        # finds it already gone.
        serve_off = body.index("stop_corpus_serve\n", after)
        self.assertLess(serve_off, evidence,
                        "the replay logs are hashed while corpus_serve still writes them")
        call = body[evidence:body.index("\n", evidence)]
        self.assertIn("|| verdict=unclean", call,
                      "an evidence write that fails must not leave verdict=clean: " + call)
        cleanup = body.split("cleanup() {", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("stop_dns_sampler", cleanup,
                      "a campaign that dies mid-run leaks the sampler")
        self.assertIn("stop_corpus_serve", cleanup,
                      "a campaign that dies mid-run leaks the replay server")

    BRACKETS = ("pre", "before-run", "after-run")
    REPLAY_LOGS = ("corpus-dns.log", "corpus-access.log")

    def _leave_files(self, results, brackets=BRACKETS, logs=REPLAY_LOGS,
                     bracket_passed=True, serve_status="0"):
        """What the campaign leaves in $RESULTS before write_dns_evidence:
        the bracket files, the replay logs and corpus-serve.status, the exit
        status the server's wrapper writes once it is gone (None: no file)."""
        for stage in brackets:
            with open(os.path.join(results, f"verify-dns-{stage}.json"), "w") as handle:
                handle.write(json.dumps({"passed": bracket_passed}) + "\n")
        for name in logs:
            with open(os.path.join(results, name), "w") as handle:
                handle.write(name + "\n")
        if serve_status is not None:
            with open(os.path.join(results, "corpus-serve.status"), "w") as handle:
                handle.write(serve_status + "\n")

    # Waits for what the assertion needs, never for a duration. A fixed
    # sleep here assumed a sample rate the box has to deliver: at 0.3 s per
    # `ss` call the 0.6 s this used to sleep buys one sample, the
    # SS_CHANGE_AT-th call never happens, and the owner-change case reports
    # clean (Codex, 2026-08-28). Each helper ends the script with a message
    # and a non-zero status when its deadline passes, so a wait that will
    # never finish fails the test loudly instead of hanging to the
    # subprocess timeout.
    SYNC = """
sync_deadline() { echo $((SECONDS + 30)); }
sync_expired() {
    [ "$SECONDS" -lt "$1" ] && return 1
    echo "TIMEOUT after ${2}: $3" >&2
    exit 97
}
wait_rows() {
    # $1 = rows dns-owner.log must carry before the script goes on.
    local deadline; deadline=$(sync_deadline)
    while [ "$(wc -l <"$RESULTS/dns-owner.log" 2>/dev/null || echo 0)" -lt "$1" ]; do
        sync_expired "$deadline" 30s "dns-owner.log never reached $1 rows"
        sleep 0.02
    done
}
wait_last_load() {
    # $1 = the load1 value the newest sample must carry.
    local deadline; deadline=$(sync_deadline)
    while [ "$(sed -n 's/.* load1=//p' "$RESULTS/dns-owner.log" 2>/dev/null | tail -n 1)" != "$1" ]; do
        sync_expired "$deadline" 30s "no sample carried load1=$1"
        sleep 0.02
    done
}
wait_sampler_gone() {
    # The sampler ended on its own. `kill -0` cannot see this (a dead,
    # unreaped child still answers it) and `wait` would consume the status
    # stop_dns_sampler reads, so this watches the process state: Z once it
    # has exited and is waiting to be reaped.
    local deadline state; deadline=$(sync_deadline)
    while :; do
        state=$(sed -e 's/^.*) //' -e 's/ .*//' "/proc/$SAMPLER_PID/stat" 2>/dev/null || true)
        case "${state:-gone}" in gone | Z) return 0 ;; esac
        sync_expired "$deadline" 30s "the sampler is still running"
        sleep 0.02
    done
}
"""

    def _sample(self, env, results, verdict_in="clean", rows=1, mid_run="",
                files=True):
        """Sample until dns-owner.log carries `rows` rows, run `mid_run`,
        stop, write the evidence. `rows` is what the caller's assertions
        need, and mid_run has the SYNC helpers to wait for anything it
        causes."""
        if files:
            self._leave_files(results)
        script = (f'set -u\n{self._helpers()}\n{self.SYNC}\n'
                  f'RESULTS="{results}"\nSERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=yes\n'
                  'DNS_SAMPLE_INTERVAL=0.05\nstart_dns_sampler\n'
                  f'wait_rows {rows}\n{mid_run}\nstop_dns_sampler\n'
                  f'write_dns_evidence {verdict_in}\n')
        result = self._run(script, env)
        self.assertEqual(result.returncode, 0, result.stderr)
        with open(os.path.join(results, "dns-evidence.json")) as handle:
            return json.load(handle), result

    def test_a_port_owner_change_mid_run_is_unclean(self):
        """The owner changes from the third sample on, so the case only
        exists once three samples are in the log.

        RED WITH A SLOW SAMPLER (0.3 s per `ss` call, the fixed 0.6 s sleep
        this used to wait): "127.0.0.1:53 changed hands and the verdict is
        clean", samples 1.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            env["SS_CHANGE_AT"] = "3"
            evidence, _ = self._sample(env, results, rows=3)
            self.assertEqual(evidence["verdict"], "unclean",
                             f"127.0.0.1:53 changed hands and the verdict is clean: {evidence}")
            self.assertGreaterEqual(evidence["samples"], 3)
            self.assertIsNotNone(evidence["first_mismatch"])
            self.assertIn("owner_pid=999", evidence["first_mismatch"])
            with open(os.path.join(results, "dns-owner.log")) as handle:
                lines = handle.read().splitlines()
            self.assertEqual(lines[2], evidence["first_mismatch"])
            self.assertRegex(lines[0], r"^\S+ owner_pid=4242 dnsmasq=inactive load1=0\.42$")

    def test_a_steady_owner_with_dnsmasq_down_is_clean(self):
        """The clean case, over the same three samples.

        RED WITH A SLOW SAMPLER: AssertionError: 1 not greater than or equal
        to 3.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            evidence, _ = self._sample(env, results, rows=3)
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertEqual(evidence["serve_pid"], 4242)
            self.assertIs(evidence["dnsmasq_was_active_before"], True)
            self.assertIs(evidence["dnsmasq_active_after_restore"], False)
            self.assertEqual(evidence["dnsmasq_state_after_restore"], "inactive")
            self.assertIs(evidence["sampler_alive_at_stop"], True)
            self.assertGreaterEqual(evidence["samples"], 3)
            self.assertIsNone(evidence["first_mismatch"])
            self.assertEqual(evidence["verify_files"],
                             [os.path.join(results, f"verify-dns-{s}.json")
                              for s in self.BRACKETS])
            for key, name in (("corpus_dns_log_sha256", "corpus-dns.log"),
                              ("corpus_access_log_sha256", "corpus-access.log")):
                self.assertEqual(evidence[key],
                                 hashlib.sha256((name + "\n").encode()).hexdigest())

    def test_the_evidence_pins_each_bracket_by_hash(self):
        """The bracket files are the only record that a restored clone
        resolved the corpus through the replay server, and nothing else
        hashes them. The verdict records each bracket's sha256 so a reader of
        the run directory can tell the file the verdict read from one edited
        after it.

        RED BEFORE THE FIX: KeyError: 'verify_file_sha256'.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            evidence, _ = self._sample(env, results)
            want = {}
            for stage in self.BRACKETS:
                name = f"verify-dns-{stage}.json"
                with open(os.path.join(results, name), "rb") as handle:
                    want[name] = hashlib.sha256(handle.read()).hexdigest()
            self.assertEqual(evidence["verify_file_sha256"], want)

    def test_a_missing_or_failed_bracket_is_unclean(self):
        """Three brackets run; a verdict built from fewer is a verdict over a
        run whose resolver went unproven on one side.

        RED BEFORE THE FIX: verdict clean with two brackets, and clean with a
        bracket that recorded passed=false.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results, brackets=("pre", "before-run"))
            evidence, _ = self._sample(env, results, files=False)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIn("after-run", evidence["reason"])
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results, bracket_passed=False)
            evidence, _ = self._sample(env, results, files=False)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIn("pre", evidence["reason"])

    def test_a_missing_or_empty_replay_log_is_unclean(self):
        """The replay server's own logs are the other half of the evidence;
        a null hash was recorded without lowering the verdict.

        RED BEFORE THE FIX: verdict clean with corpus_dns_log_sha256 null.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results, logs=("corpus-access.log",))
            evidence, _ = self._sample(env, results, files=False)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIsNone(evidence["corpus_dns_log_sha256"])
            self.assertIn("corpus-dns.log", evidence["reason"])
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results)
            open(os.path.join(results, "corpus-access.log"), "w").close()
            evidence, _ = self._sample(env, results, files=False)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIn("corpus-access.log", evidence["reason"])

    def test_a_sampler_that_died_mid_run_is_unclean(self):
        """One clean sample followed by a dead sampler was accepted: the
        evidence required samples > 0 and no mismatch, nothing about the
        sampler covering the run. Its exit status now says whether the
        stop is what ended it; a sample that cannot be taken (sudo -n
        refused, ss missing) ends it on its own, with or without errexit.

        RED BEFORE THE FIX: verdict clean, and KeyError: 'sampler_alive_at_stop'.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            env["SS_FAIL_AT"] = "3"
            evidence, _ = self._sample(env, results, rows=2,
                                       mid_run="wait_sampler_gone\n")
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIs(evidence["sampler_alive_at_stop"], False)
            self.assertGreaterEqual(evidence["samples"], 2)
            self.assertIsNone(evidence["first_mismatch"])
            with open(os.path.join(results, "dns-owner.log")) as handle:
                lines = handle.read().splitlines()
            self.assertEqual(len(lines), 2, "the sampler kept going after a failed sample")
            self.assertIn("sampler", evidence["reason"])

    def test_an_unknown_dnsmasq_state_after_the_run_is_unclean(self):
        """`systemctl is-active --quiet` exits non-zero for failed,
        activating and unknown alike, and all of them read as inactive.

        RED BEFORE THE FIX: verdict clean with dnsmasq state unknown.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp, dnsmasq="unknown")
            evidence, _ = self._sample(env, results)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertEqual(evidence["dnsmasq_state_after_restore"], "unknown")
            self.assertIs(evidence["dnsmasq_active_after_restore"], False)

    def test_an_unwritable_evidence_file_is_an_unclean_verdict(self):
        """write_dns_evidence printed its verdict after the write, whatever
        the write did; a failed jq or redirect still yielded `clean` and a
        campaign exit of 0 with no evidence on disk.

        The write is failed by a jq or an mv on PATH that exits 1, one per
        subtest, so both halves of `jq ... >tmp || ! mv tmp out` are covered.
        The first version made $RESULTS read-only instead, and root writes
        into a 0o555 directory: under sudo the write succeeded and this test
        failed with `0 == 0 : a failed evidence write returned 0` (Codex,
        2026-08-28). A stub on PATH fails the same way for every uid.

        RED BEFORE THE FIX: stdout `clean`, exit 0.
        """
        for tool in ("jq", "mv"):
            with self.subTest(tool), tempfile.TemporaryDirectory() as tmp:
                env, results = self._fakes(tmp)
                self._leave_files(results)
                with open(os.path.join(results, "dns-owner.log"), "w") as handle:
                    handle.write("2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive\n")
                stub = os.path.join(tmp, "bin", tool)
                with open(stub, "w") as handle:
                    handle.write("#!/bin/bash\nexit 1\n")
                os.chmod(stub, 0o755)
                script = (f'set -u\n{self._helpers()}\nRESULTS="{results}"\n'
                          'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=yes\nSAMPLER_ALIVE_AT_STOP=yes\n'
                          'write_dns_evidence clean\n')
                result = self._run(script, env)
                self.assertNotEqual(result.returncode, 0,
                                    f"a failed {tool} in the evidence write returned 0")
                self.assertEqual(result.stdout.strip(), "unclean")
                self.assertIn("cannot write", result.stderr,
                              f"the failed {tool} was not what made the verdict unclean")
                self.assertEqual(sorted(os.listdir(results)),
                                 sorted(["dns-owner.log", "corpus-dns.log", "corpus-access.log",
                                         "corpus-serve.status"]
                                        + [f"verify-dns-{s}.json" for s in self.BRACKETS]),
                                 "a partial evidence file or temp file was left behind")

    def test_every_sample_carries_the_one_minute_load(self):
        """The quiet gate reads the load once, before the run, and nothing
        recorded it while clones were being measured, so a build or a stray
        process that arrived mid-run left no evidence in the record. Each
        sample now ends in load1=<first field of LOADAVG_FILE>, read from
        /proc/loadavg by default, and the evidence reports the maximum over
        the samples as load_max_1min with load_samples counting them.

        RED BEFORE THE FIX: AssertionError: Regex didn't match:
        '^\\S+ owner_pid=4242 dnsmasq=inactive load1=0\\.42$' not found in
        '2026-08-28T..Z owner_pid=4242 dnsmasq=inactive', then
        KeyError: 'load_max_1min'.

        The mid-run swap must be atomic, or the test fails for a reason it
        is not about (CodeRabbit, 2026-08-28). RED WITH THE SWAP WRITTEN IN
        PLACE, over the same interleaving: `AssertionError: 97 != 0 :
        TIMEOUT after 30s: dns-owner.log never reached 4 rows`, because the
        sample taken while the file was truncated read an empty load1 and
        the sampler ended. The shipped campaign has no such write: it
        truncates dns-owner.log before the sampler exists, and both files
        another process reads, dns-evidence.json and corpus-serve.status,
        are written to a temp file and renamed.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            loadavg = env["LOADAVG_FILE"]
            # The swap writes a second file and renames it over the first,
            # and a sample is taken while both exist. In place, the sample
            # landing between the truncate and the bytes reads an empty
            # load1, which ends the sampler.
            raise_load = (f'printf "1.87 0.30 0.20 1/100 1234\\n" > "{loadavg}.new"\n'
                          f'wait_rows 4\nmv "{loadavg}.new" "{loadavg}"\n'
                          'wait_last_load 1.87\n')
            evidence, _ = self._sample(env, results, rows=3, mid_run=raise_load)
            with open(os.path.join(results, "dns-owner.log")) as handle:
                lines = handle.read().splitlines()
            self.assertGreaterEqual(len(lines), 4)
            self.assertRegex(lines[0], r"^\S+ owner_pid=4242 dnsmasq=inactive load1=0\.42$")
            self.assertRegex(lines[-1], r"^\S+ owner_pid=4242 dnsmasq=inactive load1=1\.87$")
            for line in lines:
                self.assertRegex(line, r" load1=(0\.42|1\.87)$")
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertEqual(evidence["load_max_1min"], 1.87)
            self.assertEqual(evidence["load_samples"], len(lines))
            self.assertEqual(evidence["samples"], len(lines))

    def test_a_sample_whose_load_cannot_be_read_ends_the_sampler(self):
        """A load column that silently went missing would read as a quiet
        box. An unreadable LOADAVG_FILE fails the sample, which ends the
        sampler, which the evidence records as unclean.

        RED BEFORE THE FIX: AssertionError: 'unclean' != 'clean'.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            cut_load = f'rm -f "{env["LOADAVG_FILE"]}"\nwait_sampler_gone\n'
            evidence, _ = self._sample(env, results, mid_run=cut_load)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertIs(evidence["sampler_alive_at_stop"], False)
            self.assertIn("sampler", evidence["reason"])
            with open(os.path.join(results, "dns-owner.log")) as handle:
                lines = handle.read().splitlines()
            for line in lines:
                self.assertRegex(line, r" load1=0\.42$")

    def test_the_evidence_reports_the_maximum_load_over_the_samples(self):
        """write_dns_evidence over a fixture owner log: the maximum of the
        load1 column as a number, and the count of samples carrying one. An
        owner log from before the column existed yields null and 0 rather
        than a verdict change; the sampler, not the evidence, is what
        guarantees the column is present.

        RED BEFORE THE FIX: KeyError: 'load_max_1min'.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results)
            with open(os.path.join(results, "dns-owner.log"), "w") as handle:
                handle.write(
                    "2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive load1=0.31\n"
                    "2026-08-28T00:00:10Z owner_pid=4242 dnsmasq=inactive load1=1.87\n"
                    "2026-08-28T00:00:20Z owner_pid=4242 dnsmasq=inactive load1=0.90\n")
            script = (f'set -u\n{self._helpers()}\nRESULTS="{results}"\n'
                      'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=yes\nSAMPLER_ALIVE_AT_STOP=yes\n'
                      'write_dns_evidence clean\n')
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(os.path.join(results, "dns-evidence.json")) as handle:
                evidence = json.load(handle)
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertEqual(evidence["load_max_1min"], 1.87)
            self.assertIsInstance(evidence["load_max_1min"], float)
            self.assertEqual(evidence["load_samples"], 3)
            self.assertEqual(evidence["samples"], 3)
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            self._leave_files(results)
            with open(os.path.join(results, "dns-owner.log"), "w") as handle:
                handle.write("2026-08-28T00:00:00Z owner_pid=4242 dnsmasq=inactive\n" * 2)
            script = (f'set -u\n{self._helpers()}\nRESULTS="{results}"\n'
                      'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=yes\nSAMPLER_ALIVE_AT_STOP=yes\n'
                      'write_dns_evidence clean\n')
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(os.path.join(results, "dns-evidence.json")) as handle:
                evidence = json.load(handle)
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertIsNone(evidence["load_max_1min"])
            self.assertEqual(evidence["load_samples"], 0)
            self.assertEqual(evidence["samples"], 2)

    def test_a_bracket_failure_cannot_be_lifted_by_clean_samples(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            evidence, _ = self._sample(env, results, verdict_in="unclean")
            self.assertEqual(evidence["verdict"], "unclean")
            self.assertIsNone(evidence["first_mismatch"])

    def test_dnsmasq_active_after_the_run_is_unclean(self):
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp, dnsmasq="active")
            evidence, _ = self._sample(env, results)
            self.assertEqual(evidence["verdict"], "unclean")
            self.assertIs(evidence["dnsmasq_active_after_restore"], True)

    def test_no_samples_is_unclean(self):
        """An empty owner log is absence of evidence, not evidence of a
        clean run."""
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            script = (f'set -u\n{self._helpers()}\nRESULTS="{results}"\n'
                      'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=no\nwrite_dns_evidence clean\n')
            result = self._run(script, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(os.path.join(results, "dns-evidence.json")) as handle:
                evidence = json.load(handle)
            self.assertEqual(evidence["verdict"], "unclean")
            self.assertEqual(evidence["samples"], 0)
            self.assertIs(evidence["dnsmasq_was_active_before"], False)

    def test_a_replay_server_that_did_not_exit_zero_is_unclean(self):
        """corpus_serve exits 1 when a log line could not be written, after
        the response bytes were already sent (007b726d, 181fcbbb). Its exit
        status was discarded by `sudo -b`: stop_corpus_serve polled liveness
        only, and the evidence hashed the truncated log as clean. The wrapper
        now writes the status to $RESULTS/corpus-serve.status once the server
        is gone, and a clean verdict needs that file to say 0; a missing or
        non-zero status is unclean with the reason, and the evidence carries
        it as corpus_serve_exit_status.

        RED BEFORE THE FIX: verdict clean with corpus-serve.status holding 1,
        clean with no status file, and KeyError: 'corpus_serve_exit_status'.
        """
        cases = {"1": 1, "137": 137, None: None}
        for status, recorded in cases.items():
            with self.subTest(status=status), tempfile.TemporaryDirectory() as tmp:
                env, results = self._fakes(tmp)
                self._leave_files(results, serve_status=status)
                script = (f'set -u\n{self._helpers()}\nRESULTS="{results}"\n'
                          'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=yes\nSAMPLER_ALIVE_AT_STOP=yes\n'
                          'printf "%s owner_pid=4242 dnsmasq=inactive load1=0.42\\n" '
                          '2026-08-28T00:00:00Z 2026-08-28T00:00:10Z > "$RESULTS/dns-owner.log"\n'
                          'write_dns_evidence clean\n')
                result = self._run(script, env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), "unclean",
                                 f"corpus_serve exit status {status!r} was hashed as clean")
                with open(os.path.join(results, "dns-evidence.json")) as handle:
                    evidence = json.load(handle)
                self.assertEqual(evidence["verdict"], "unclean", evidence)
                self.assertEqual(evidence["corpus_serve_exit_status"], recorded)
                self.assertIn("corpus_serve", evidence["reason"])
                if status is not None:
                    self.assertIn(status, evidence["reason"])
                else:
                    self.assertIn("corpus-serve.status", evidence["reason"])
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            evidence, _ = self._sample(env, results)
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertEqual(evidence["corpus_serve_exit_status"], 0)

    STOP_SERVE = re.compile(r"(stop_corpus_serve\(\) \{\n.*?\n\}\n)", re.S)
    # The launch: the pidfile name through the liveness check on the pid it
    # published. The readiness probes that follow need the real server.
    START_SERVE = re.compile(
        r'(SERVE_PIDFILE="\$LOGDIR/corpus_serve\.pid"\n.*?\n'
        r'sudo kill -0 "\$SERVE_PID" 2>/dev/null \|\| \{ echo "BLOCKED: corpus_serve pid[^\n]*\n)',
        re.S)
    # Stands in for corpus_serve.py under the shipped launch line: the same
    # startup line, its own pid where the test can read it, and on SIGTERM
    # the exit status the test chose, as the real server exits 1 after a
    # log line it could not write.
    FAKE_SERVE = """import os, signal, sys, time
signal.signal(signal.SIGTERM,
              lambda *_: sys.exit(int(os.environ.get("FAKE_SERVE_EXIT", "0"))))
print("loaded 3 urls", flush=True)
with open(os.environ["FAKE_SERVE_PIDOUT"], "w") as handle:
    handle.write(f"{os.getpid()}\\n")
while True:
    time.sleep(0.05)
"""
    FAKE_SUDO_B = ('#!/bin/bash\n[ "${1:-}" = -n ] && shift\n'
                   'if [ "${1:-}" = -b ]; then shift; "$@" & exit 0; fi\nexec "$@"\n')

    def _serve_harness(self, tmp, exit_status):
        env, results = self._fakes(tmp)
        for name, body in (("sudo", self.FAKE_SUDO_B),):
            path = os.path.join(tmp, "bin", name)
            with open(path, "w") as handle:
                handle.write(body)
            os.chmod(path, 0o755)
        serve_dir = os.path.join(tmp, "bench", "chromium")
        os.makedirs(serve_dir)
        with open(os.path.join(serve_dir, "corpus_serve.py"), "w") as handle:
            handle.write(self.FAKE_SERVE)
        env.update(FAKE_SERVE_EXIT=str(exit_status),
                   FAKE_SERVE_PIDOUT=os.path.join(tmp, "serve-pid"))
        start = self.START_SERVE.search(campaign())
        self.assertIsNotNone(start, "the corpus_serve launch block is gone")
        stop = self.STOP_SERVE.search(campaign())
        self.assertIsNotNone(stop, "stop_corpus_serve is gone")
        # One owner sample naming the server that was started, and a live
        # sampler and inactive dnsmasq, so the exit status is the only thing
        # that can lower the verdict. The stop waits for the stand-in to have
        # installed its handler (it writes its pid after), as the campaign's
        # readiness probes do for the real server.
        script = (f'set -euo pipefail\nsay() {{ :; }}\n{self._helpers()}\n'
                  f'SERVE_PID=""\n{stop.group(1)}\n{start.group(1)}\n'
                  'echo "started $SERVE_PID"\n'
                  'for _ in $(seq 1 100); do [ -s "$FAKE_SERVE_PIDOUT" ] && break; sleep 0.05; done\n'
                  'printf "%s owner_pid=%s dnsmasq=inactive load1=0.42\\n" '
                  '2026-08-28T00:00:00Z "$SERVE_PID" > "$RESULTS/dns-owner.log"\n'
                  'SAMPLER_ALIVE_AT_STOP=yes\nDNSMASQ_WAS_ACTIVE=yes\n'
                  'stop_corpus_serve\necho "stopped $SERVE_PID"\n'
                  'write_dns_evidence clean\n')
        return env, results, script

    def test_the_replay_server_exit_status_is_recorded_when_it_stops(self):
        """The shipped launch line and stop_corpus_serve, run against a
        stand-in server under a sudo that honours -b. The pidfile must name
        the server itself (the sampler matches that pid against the owner of
        127.0.0.1:53), the stop must wait for the status the wrapper writes
        once the server is gone, and the evidence must carry it: 3 is
        unclean with the reason, 0 is clean.

        RED BEFORE THE FIX: no corpus-serve.status was written at all, so
        `MISSING != '3'`, and write_dns_evidence printed clean over it.
        """
        with tempfile.TemporaryDirectory() as tmp:
            env, results, script = self._serve_harness(tmp, 3)
            self._leave_files(results, serve_status=None)
            result = self._run(script, env, timeout=120)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            with open(os.path.join(tmp, "logs", "corpus_serve.pid")) as handle:
                published = handle.read().strip()
            with open(env["FAKE_SERVE_PIDOUT"]) as handle:
                self.assertEqual(published, handle.read().strip(),
                                 "the pidfile names the wrapper, not the server")
            self.assertIn(f"started {published}", result.stdout)
            status_path = os.path.join(results, "corpus-serve.status")
            self.assertEqual(read_if_exists(status_path, "MISSING").strip(), "3",
                             "the server's exit status was not recorded when it stopped")
            self.assertEqual(result.stdout.strip().splitlines()[-1], "unclean")
            with open(os.path.join(results, "dns-evidence.json")) as handle:
                evidence = json.load(handle)
            self.assertEqual(evidence["verdict"], "unclean", evidence)
            self.assertEqual(evidence["corpus_serve_exit_status"], 3)
            self.assertIn("corpus_serve", evidence["reason"])
        with tempfile.TemporaryDirectory() as tmp:
            env, results, script = self._serve_harness(tmp, 0)
            self._leave_files(results, serve_status=None)
            result = self._run(script, env, timeout=120)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(read_if_exists(
                os.path.join(results, "corpus-serve.status"), "MISSING").strip(), "0")
            self.assertEqual(result.stdout.strip().splitlines()[-1], "clean")
            with open(os.path.join(results, "dns-evidence.json")) as handle:
                evidence = json.load(handle)
            self.assertEqual(evidence["verdict"], "clean", evidence)
            self.assertEqual(evidence["corpus_serve_exit_status"], 0)

    def test_the_replay_server_logs_into_the_run_directory(self):
        """corpus_serve's DNS and access logs are the other half of the
        evidence: the sha256 in dns-evidence.json must name files that live
        with the run, not in a /tmp the next campaign overwrites."""
        serve = re.search(r"sudo -b sh -c '([^']*)'(.*?)\n\n", campaign(), re.S)
        self.assertIsNotNone(serve, "the corpus_serve launch line is gone")
        inner, args = serve.groups()
        self.assertIn("--dns-log", inner)
        self.assertIn("--access-log", inner)
        self.assertIn('"$RESULTS/corpus-dns.log"', args)
        self.assertIn('"$RESULTS/corpus-access.log"', args)


class DiagPhase(unittest.TestCase):
    """The diag runs on the golden's clones after its verify and before anything
    is measured: one traced render per corpus URL and rep, inside a restored
    clone, holding every remote IP to the replay's, every name to resolved,
    every load event under DIAG_MAX_LOAD_MS. A failed diag ends the campaign
    before the measured run; DIAG_ONLY=1 ends it after the diag whatever the
    result, for the throwaway golden round.

    Borrows DnsBrackets' fakes and helper lifting, as HugepageGuardsRound2
    borrows _bash in test_reqbench.py.
    """

    HELPERS = DnsBrackets.HELPERS
    URL_LINE = DnsBrackets.URL_LINE
    FAKE_MAKE = DnsBrackets.FAKE_MAKE
    FAKE_SS = DnsBrackets.FAKE_SS
    FAKE_SYSTEMCTL = DnsBrackets.FAKE_SYSTEMCTL
    FAKE_SUDO = DnsBrackets.FAKE_SUDO
    _fakes = DnsBrackets._fakes
    _set_load = staticmethod(DnsBrackets._set_load)
    _make_env = DnsBrackets._make_env
    _run = DnsBrackets._run
    _helpers = DnsBrackets._helpers
    _urls = DnsBrackets._urls

    DIAG_BLOCK = re.compile(
        r'(run_diag \|\| campaign_fail[^\n]*\n'
        r'if \[ "\$DIAG_ONLY" = 1 \]; then\n.*?\nfi\n)', re.S)

    def _diag_summary(self, urls, passed=True):
        return json.dumps({
            "engine": "chromium", "tag": "cb-req-corpus", "passed": passed,
            "urls": {u: {"reps": 3, "renders_ok": 3, "max_load_ms": 812.5}
                     for u in urls.split(",")},
            "violations": [] if passed else [
                {"url": urls.split(",")[0], "rep": 1, "kind": "remote_ip",
                 "detail": "93.184.216.34 served 1 request(s)"}],
            "limits": {"expect_ips": ["10.0.2.2"], "max_load_ms": 15000},
        })

    def _knob_defaults(self):
        body = campaign()
        limit = re.search(r'^DIAG_MAX_LOAD_MS="\$\{DIAG_MAX_LOAD_MS:-(\d+)\}"$', body, re.M)
        self.assertIsNotNone(limit, "the campaign sets no DIAG_MAX_LOAD_MS default")
        self.assertEqual(limit.group(1), "15000")
        only = re.search(r'^DIAG_ONLY="\$\{DIAG_ONLY:-0\}"$', body, re.M)
        self.assertIsNotNone(only, "the campaign has no DIAG_ONLY knob")
        return limit.group(0) + "\n" + only.group(0) + "\n"

    def _prelude(self, urls):
        return ('set -euo pipefail\nsay() { echo "=== $*"; }\n'
                f'URLS="{urls}"\nBACKEND=uffd\nUFFD_MODE=minor\nUFFD_PREFETCH=on\n'
                'SERVE_PID=4242\nDNSMASQ_WAS_ACTIVE=no\nSAMPLER_ALIVE_AT_STOP=""\n'
                'DNS_SAMPLE_INTERVAL=10\n'
                + self._knob_defaults() + self._helpers() + "\n")

    def test_run_diag_hands_the_corpus_and_the_replay_answer_to_the_diag_target(self):
        """Every corpus URL, the replay's answer as the only expected remote
        IP, the 15 s limit, this run's RESULTS and the run's backend knobs,
        to the engine's diag target; the summary it leaves must say passed.

        Watched red 2026-08-28 at 55d6fb7d: `bash: line N: run_diag: command
        not found` (the helpers block has no run_diag) after the knob
        defaults were found missing.
        """
        urls = self._urls()
        with tempfile.TemporaryDirectory() as tmp:
            env, results = self._fakes(tmp)
            for key in ("DIAG_MAX_LOAD_MS", "DIAG_ONLY", "DIAG_REPS"):
                env.pop(key, None)
            env["MAKE_DIAG_JSON"] = self._diag_summary(urls)
            result = self._run(self._prelude(urls) + "run_diag\necho DIAGNOSED\n", env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("DIAGNOSED", result.stdout)
            seen = self._make_env(env)
            self.assertEqual(seen.get("DIAG_URLS"), urls, "diag was not told the corpus")
            self.assertEqual(seen.get("DIAG_EXPECT_IPS"), "10.0.2.2")
            self.assertEqual(seen.get("DIAG_MAX_LOAD_MS"), "15000")
            self.assertEqual(seen.get("RESULTS"), results,
                             "the diag's records land outside the run directory")
            self.assertEqual(seen.get("TAG"), "cb-req-corpus")
            self.assertEqual(seen.get("ENGINE"), "chromium")
            for knob, want in (("BACKEND", "uffd"), ("UFFD_MODE", "minor"),
                               ("UFFD_PREFETCH", "on")):
                self.assertEqual(seen.get(knob), want,
                                 f"the diag does not run on the measured run's {knob}")
            with open(env["MAKE_ARGV"]) as handle:
                self.assertIn("bench-chromium-request-diag", handle.read())

    def test_the_webkit_diag_is_not_asked_for_addresses_it_cannot_see(self):
        """WebKit renders carry no network trace, and reqbench refuses a
        DIAG_EXPECT_IPS it cannot enforce; the campaign passes the
        expectation to the Chromium diag only, and the verify brackets keep
        the resolver evidence for both engines."""
        urls = self._urls()
        with tempfile.TemporaryDirectory() as tmp:
            env, _results = self._fakes(tmp)
            env["ENGINE"] = "webkit"
            for key in ("DIAG_MAX_LOAD_MS", "DIAG_ONLY", "DIAG_REPS", "DIAG_EXPECT_IPS"):
                env.pop(key, None)
            env["MAKE_DIAG_JSON"] = self._diag_summary(urls)
            result = self._run(self._prelude(urls) + "run_diag\necho DIAGNOSED\n", env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            seen = self._make_env(env)
            self.assertFalse(seen.get("DIAG_EXPECT_IPS"),
                             "the webkit diag was handed an IP expectation it cannot check")
            self.assertEqual(seen.get("DIAG_URLS"), urls)
            self.assertEqual(seen.get("DIAG_MAX_LOAD_MS"), "15000")
            with open(env["MAKE_ARGV"]) as handle:
                self.assertIn("bench-webkit-request-diag", handle.read())

    def test_a_failed_or_missing_diag_refuses_to_measure(self):
        urls = self._urls()
        cases = {
            "make failed": {"MAKE_RC": "1", "MAKE_DIAG_JSON": self._diag_summary(urls)},
            "summary says failed": {"MAKE_DIAG_JSON": self._diag_summary(urls, passed=False)},
            "no summary": {},
        }
        for label, extra in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                env, results = self._fakes(tmp)
                for key in ("DIAG_MAX_LOAD_MS", "DIAG_ONLY", "DIAG_REPS"):
                    env.pop(key, None)
                env.update(extra)
                result = self._run(self._prelude(urls) + "run_diag\necho DIAGNOSED\n", env)
                self.assertNotEqual(result.returncode, 0, f"{label}: accepted\n{result.stdout}")
                self.assertNotIn("DIAGNOSED", result.stdout)
                self.assertIn("diag", result.stderr, label)

    def test_diag_only_stops_after_the_diag_and_a_failed_diag_stops_before_measuring(self):
        """The block between the golden's verify and the settle wait, run
        with the fake make: DIAG_ONLY=1 exits 0 without reaching what
        follows; a failed diag exits non-zero without reaching it; the
        default reaches it.
        """
        urls = self._urls()
        block = self.DIAG_BLOCK.search(campaign())
        self.assertIsNotNone(block, "the diag call and the DIAG_ONLY stop are gone")
        cases = (
            ("DIAG_ONLY=1", {"DIAG_ONLY": "1"}, True, 0, False),
            ("default", {}, True, 0, True),
            ("diag failed", {"DIAG_ONLY": "1", "MAKE_RC": "1"}, False, 1, False),
        )
        for label, extra, diag_ok, want_rc, measured in cases:
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                env, results = self._fakes(tmp)
                for key in ("DIAG_MAX_LOAD_MS", "DIAG_ONLY", "DIAG_REPS"):
                    env.pop(key, None)
                env["MAKE_DIAG_JSON"] = self._diag_summary(urls, passed=diag_ok)
                env.update(extra)
                script = self._prelude(urls) + block.group(1) + "echo MEASURED\n"
                result = self._run(script, env)
                self.assertEqual(result.returncode, want_rc,
                                 f"{label}: {result.stdout}{result.stderr}")
                self.assertEqual("MEASURED" in result.stdout, measured,
                                 f"{label}: {result.stdout}{result.stderr}")
                if label == "DIAG_ONLY=1":
                    self.assertIn("DIAG_ONLY", result.stdout,
                                  "the stop does not say why the campaign ended")

    def test_the_diag_follows_the_golden_verify_in_both_phases_and_precedes_the_settle(self):
        """Ordering in the main flow, which cannot run without a VM."""
        body = campaign()
        phases = re.search(r'if \[ "\$PHASE" = all \]; then\n(.*?)\nelse\n(.*?)\nfi\n',
                           body, re.S)
        self.assertIsNotNone(phases, "the PHASE branch is gone")
        for branch in (phases.group(1), phases.group(2)):
            self.assertIn("run_verify pre", branch)
            self.assertNotIn("run_diag", branch,
                             "the diag is called inside one phase branch; it runs once, "
                             "after whichever golden the phase settled on")
        self.assertEqual(body.count("run_diag ||"), 1)
        diag = body.index("run_diag ||")
        self.assertLess(phases.end(), diag, "the diag runs before the golden's verify")
        stop = body.index('if [ "$DIAG_ONLY" = 1 ]; then', diag)
        settle = body.index("settle_deadline=")
        self.assertLess(stop, settle, "the DIAG_ONLY stop comes after the settle wait")
        self.assertLess(diag, body.index("run_verify before-run"))
        self.assertLess(diag, body.index("engine_target run)"))
        self.assertIn("campaign_fail", body[diag:body.index("\n", diag)],
                      "a failed diag does not end the campaign")


if __name__ == "__main__":
    unittest.main()
