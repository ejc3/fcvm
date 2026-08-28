#!/usr/bin/env python3
"""HOP D of `reqbench.sh verify`: the baked resolver, proven inside a RESTORED clone.

The corpus campaign bakes GUEST_DNS=10.0.2.2 into the golden so every corpus
hostname resolves to the pasta gateway and lands on the host replay server.
HOPs A-C prove the render path by IP; none of them proves that a restored
clone still resolves through the baked server. A clone whose resolv.conf came
back pointing at a real resolver would render the live site and record a
plausible number. HOP D asks the clone itself, and the golden records the
resolver it asked for so the two can be compared later.

Driven the way HugepageGuards drives reqbench.sh: the script is sourced with a
stub $FCVM on PATH and a fixture config.json, so no VM, sudo or podman is
involved. The stub answers `snapshot serve`, `snapshot run`, `ls --json` and
every `exec` form cmd_verify uses, and publishes/removes a state file the way
a real clone does, so the ordered teardown at the end of cmd_verify runs for
real.

Run: python3 -m unittest test_verify_dns -v
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SH = os.path.join(HERE, "reqbench.sh")
TAG = "tag-under-test"
HOSTS = "example.com,blog.cloudflare.com"
URLS = "https://example.com/,https://blog.cloudflare.com/"
RUN_ID = "0" * 32

# Stands in for fcvm. Every exec is appended to $STUB_EXEC_LOG as
# "<view> <argv...>" so a test can prove which questions HOP D asked the clone.
FCVM_STUB = r'''#!/bin/bash
echo "$*" >> "$STUB_ARGV"
case "$1 $2" in
  "snapshot serve")
      # Simulates the snapshot directory vanishing after the hugepage check
      # (the only reader before HOP D) so HOP D meets an unreadable config.
      [ "${STUB_DROP_CONFIG:-0}" = 1 ] && rm -f "$FCVM_DATA_DIR/snapshots/$TAG/config.json"
      echo "Serve PID: $$"; echo "Waiting for VMs"; exec sleep 30 ;;
  "snapshot run")
      name=""; prev=""
      for a in "$@"; do [ "$prev" = --name ] && name="$a"; prev="$a"; done
      vm_id="vm-$(printf '%s' "$name" | sha256sum | cut -c1-32)"
      state="$STATE_DIR/$vm_id.json"
      read -r st < /proc/$$/stat; st=${st##*) }; read -ra f <<< "$st"
      printf '{"vm_id":"%s","name":"%s","pid":%s,"pid_start_time":%s,"config":{"network":{"loopback_ip":"127.0.0.1"}}}\n' \
          "$vm_id" "$name" "$$" "${f[19]}" > "$state"
      sleep 30 & sl=$!
      trap 'kill $sl 2>/dev/null; rm -f "$state"; exit 0' TERM
      wait $sl; rm -f "$state"; exit 0 ;;
  "ls --json")
      pid=""; [ "${3:-}" = --pid ] && pid="$4"
      "$STUB_PYTHON" - "$STATE_DIR" "$pid" <<'PY'
import glob, json, os, sys
rows = [json.load(open(p)) for p in glob.glob(os.path.join(sys.argv[1], "vm-*.json"))]
if sys.argv[2]:
    rows = [r for r in rows if str(r["pid"]) == sys.argv[2]]
print(json.dumps(rows))
PY
      ;;
  "exec --pid")
      shift 3; view="$1"; shift; shift   # exec --pid P <view> -- argv...
      line="$view $*"; printf '%s\n' "${line//$'\n'/ }" >> "$STUB_EXEC_LOG"   # one line per exec
      case "$view $*" in
        "--vm cat /etc/resolv.conf") printf '%b' "$STUB_RESOLV_VM"; exit "${STUB_RESOLV_RC:-0}" ;;
        "-c cat /etc/resolv.conf") printf '%b' "$STUB_RESOLV_CONTAINER"; exit "${STUB_RESOLV_RC:-0}" ;;
        "-c python3 /opt/bench/"*) exit 0 ;;
        "-c python3 -c "*gethostbyname*) echo "$STUB_ANSWER" ;;
        "-c python3 -c "*urllib*) echo "$STUB_URL_STATUS" ;;
        *) echo "stub fcvm: unexpected exec: $view $*" >&2; exit 97 ;;
      esac ;;
  *) exit 0 ;;
esac
'''

# Stands in for the HOST python3. HOP B feeds a script on stdin ("-") and HOP C
# and target_id run cdpdrive.py; both are outside HOP D and answered trivially.
# Everything else (the -c one-liners that parse `fcvm ls --json`) runs the real
# interpreter, so the harness's own state parsing is exercised unchanged.
PYTHON_STUB = '''#!/bin/bash
case "$1" in
  *cdpdrive.py)
      for a in "$@"; do [ "$a" = --print-target ] && {{ echo TARGET-1; exit 0; }}; done
      exit 0 ;;
  -) cat >/dev/null; exit 0 ;;
esac
exec {python} "$@"
'''


def write_exec(path, body):
    with open(path, "w") as handle:
        handle.write(body)
    os.chmod(path, 0o755)


def read_if_exists(path, default=""):
    if not os.path.exists(path):
        return default
    with open(path) as handle:
        return handle.read()


class VerifyDnsHop(unittest.TestCase):
    """cmd_verify end to end against the stub, with HOP D's inputs varied."""

    def _fixture(self, d, dns_server="10.0.2.2", config=True, hosts=HOSTS,
                 urls=URLS, answer="10.0.2.2", resolv_vm="nameserver 10.0.2.2\n",
                 resolv_container="nameserver 10.0.2.2\n", url_status="200",
                 drop_config=False, resolv_rc=0):
        data = os.path.join(d, "data")
        state_dir = os.path.join(data, "state")
        snap = os.path.join(data, "snapshots", TAG)
        os.makedirs(state_dir)
        os.makedirs(snap)
        if config:
            network = {"guest_ip": "10.0.2.100"}
            if dns_server is not None:
                network["dns_server"] = dns_server
            with open(os.path.join(snap, "config.json"), "w") as handle:
                json.dump({
                    "generation_id": "12345678-1234-4234-8234-123456789abc",
                    "metadata": {"hugepages": False, "memory_mib": 1024,
                                 "network_config": network},
                }, handle)
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        write_exec(os.path.join(binx, "python3"),
                   PYTHON_STUB.format(python=sys.executable))
        fcvm = os.path.join(d, "fcvm")
        write_exec(fcvm, FCVM_STUB)
        write_exec(os.path.join(d, "fc-agent"), "#!/bin/bash\nexit 0\n")
        env = dict(os.environ)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            TAG=TAG,
            STATE_DIR=state_dir,
            RESULTS=os.path.join(d, "results"),
            RUNID=RUN_ID,
            FCVM=fcvm,
            FC_AGENT=os.path.join(d, "fc-agent"),
            VERIFY_DNS_HOSTS=hosts,
            VERIFY_DNS_ANSWER="10.0.2.2",
            VERIFY_DNS_URLS=urls,
            STUB_PYTHON=sys.executable,
            STUB_ARGV=os.path.join(d, "argv.log"),
            STUB_EXEC_LOG=os.path.join(d, "exec.log"),
            STUB_ANSWER=answer,
            STUB_RESOLV_VM=resolv_vm,
            STUB_RESOLV_CONTAINER=resolv_container,
            STUB_URL_STATUS=url_status,
            STUB_DROP_CONFIG="1" if drop_config else "0",
            STUB_RESOLV_RC=str(resolv_rc),
        )
        return env, state_dir

    def _verify(self, env):
        return subprocess.run(
            ["bash", "-c", f'source "{SH}" && cmd_verify'],
            env=env, capture_output=True, text=True, timeout=180)

    def _evidence(self, env):
        path = os.path.join(env["RESULTS"], "verify-dns.json")
        self.assertTrue(os.path.exists(path), f"no {path} was written")
        with open(path) as handle:
            return json.load(handle)

    def test_a_clone_answering_the_real_address_fails_hop_d(self):
        """The failure the hop exists for: the guest resolves example.com to
        its real address, so a render would fetch the live site, not the
        replay. Every other hop passes; only HOP D can see it."""
        with tempfile.TemporaryDirectory() as d:
            env, state_dir = self._fixture(d, answer="93.184.216.34")
            result = self._verify(env)
            self.assertNotEqual(
                result.returncode, 0,
                "verify passed a clone that resolves the corpus to the live "
                f"internet\n{result.stdout}{result.stderr}")
            self.assertIn("HOP D FAILED", result.stderr)
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], False)
            self.assertEqual(evidence["hosts"]["example.com"],
                             {"answer": "93.184.216.34", "ok": False})
            # The hop failed and the ordered teardown still ran: both clone
            # state files are gone, exactly as after a passing verify.
            self.assertEqual(os.listdir(state_dir), [],
                             "a failed HOP D skipped the clone teardown")

    def test_a_clone_resolving_through_the_baked_resolver_passes(self):
        with tempfile.TemporaryDirectory() as d:
            env, state_dir = self._fixture(
                d, resolv_vm="search .\\nnameserver 10.0.2.2\\n",
                resolv_container="nameserver 10.0.2.2\\nsearch .\\n")
            result = self._verify(env)
            self.assertEqual(result.returncode, 0,
                             f"{result.stdout}\n{result.stderr[-2500:]}")
            self.assertIn("HOP D", result.stdout)
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], True)
            self.assertEqual(evidence["dns_server"], "10.0.2.2")
            self.assertIn("nameserver 10.0.2.2", evidence["resolv_conf_vm"])
            self.assertIn("nameserver 10.0.2.2", evidence["resolv_conf_container"])
            for host in HOSTS.split(","):
                self.assertEqual(evidence["hosts"][host],
                                 {"answer": "10.0.2.2", "ok": True})
            for url in URLS.split(","):
                self.assertEqual(evidence["urls"][url], {"status": 200, "ok": True})
            self.assertTrue(evidence["timestamp"])
            execs = read_if_exists(env["STUB_EXEC_LOG"]).splitlines()
            self.assertIn("--vm cat /etc/resolv.conf", execs,
                          "HOP D never read the VM's resolv.conf")
            self.assertIn("-c cat /etc/resolv.conf", execs,
                          "HOP D never read the container's resolv.conf")
            for host in HOSTS.split(","):
                self.assertTrue(
                    any("gethostbyname" in line and line.endswith(" " + host)
                        for line in execs),
                    f"HOP D never resolved {host} inside the container:\n"
                    + "\n".join(execs))
            for url in URLS.split(","):
                self.assertTrue(
                    any("urllib" in line and line.endswith(" " + url)
                        for line in execs),
                    f"HOP D never fetched {url} inside the container")
            self.assertEqual(os.listdir(state_dir), [])

    def test_a_container_view_pointing_elsewhere_fails(self):
        """podman rewrites resolv.conf for the container; the VM's copy
        being right proves nothing about what Chromium reads."""
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, resolv_container="nameserver 10.0.2.3\\n")
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            self.assertIn("container", result.stderr)
            self.assertIs(self._evidence(env)["passed"], False)

    def test_a_replay_status_outside_2xx_3xx_fails(self):
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, url_status="404")
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], False)
            self.assertEqual(evidence["urls"]["https://example.com/"],
                             {"status": 404, "ok": False})

    def test_hosts_without_a_baked_resolver_fail(self):
        """A golden made without GUEST_DNS carries no resolver to verify
        against; a corpus verify of it must refuse, not vacuously pass."""
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, dns_server=None)
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            self.assertIn("baked resolver", result.stderr)
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], False)
            self.assertIsNone(evidence["dns_server"])
            self.assertNotIn("gethostbyname", read_if_exists(env["STUB_EXEC_LOG"]),
                             "HOP D resolved hosts with no resolver to hold them to")

    def test_no_hosts_records_the_resolver_and_passes(self):
        """The default (medium.html) golden has nothing to assert; HOP D
        records what the snapshot says and stays out of the way."""
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, dns_server=None, hosts="", urls="")
            result = self._verify(env)
            self.assertEqual(result.returncode, 0,
                             f"{result.stdout}\n{result.stderr[-2500:]}")
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], True)
            self.assertIsNone(evidence["dns_server"])
            self.assertEqual(evidence["hosts"], {})
            self.assertEqual(evidence["urls"], {})

    def test_a_failed_resolver_read_fails_even_with_plausible_output(self):
        """`fcvm exec` that prints the wanted line and exits non-zero is
        an exec that did not complete (a torn-down clone, a container that
        is not the one Chromium runs in); its stdout proves nothing. The
        hop kept the stdout and discarded the exit status.

        RED BEFORE THE FIX: AssertionError: 0 == 0 (verify passed) and
        evidence passed=true.
        """
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, resolv_rc=3)
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0,
                                "verify passed on a resolver read that exited 3\n"
                                + result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            self.assertIn("exited 3", result.stderr)
            self.assertIs(self._evidence(env)["passed"], False)

    def test_urls_alone_are_still_verified(self):
        """VERIFY_DNS_URLS without VERIFY_DNS_HOSTS ran zero checks and
        recorded passed=true; the URL fetches are assertions in their own
        right and need the baked resolver just the same.

        RED BEFORE THE FIX: AssertionError: 0 == 0 (a 404 through the
        resolver under test passed) and evidence urls == {}.
        """
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, hosts="", url_status="404")
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], False)
            self.assertEqual(evidence["urls"]["https://example.com/"],
                             {"status": 404, "ok": False})
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(d, hosts="")
            result = self._verify(env)
            self.assertEqual(result.returncode, 0,
                             f"{result.stdout}\n{result.stderr[-2500:]}")
            evidence = self._evidence(env)
            self.assertIs(evidence["passed"], True)
            self.assertEqual(evidence["hosts"], {})
            for url in URLS.split(","):
                self.assertEqual(evidence["urls"][url], {"status": 200, "ok": True})
        with tempfile.TemporaryDirectory() as d:
            # URLs alone, with no baked resolver to fetch through: refuse.
            env, _ = self._fixture(d, hosts="", dns_server=None)
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("baked resolver", result.stderr)

    def test_a_second_nameserver_fails(self):
        """glibc walks the nameserver list: a resolv.conf that names the
        baked resolver AND a public one renders the live site the moment
        the replay server misses a query. One matching line was enough.

        RED BEFORE THE FIX: AssertionError: 0 == 0 (verify passed with
        8.8.8.8 as a fallback resolver).
        """
        with tempfile.TemporaryDirectory() as d:
            env, _ = self._fixture(
                d, resolv_container="nameserver 10.0.2.2\\nnameserver 8.8.8.8\\n")
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D FAILED", result.stderr)
            self.assertIn("8.8.8.8", result.stderr)
            self.assertIs(self._evidence(env)["passed"], False)

    def test_an_unreadable_config_blocks_hop_d(self):
        """Fail closed: a hop that could not read the snapshot has no basis
        for a verdict, and must say BLOCKED rather than FAILED or nothing."""
        with tempfile.TemporaryDirectory() as d:
            env, state_dir = self._fixture(d, drop_config=True)
            result = self._verify(env)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("HOP D BLOCKED", result.stderr)
            self.assertFalse(
                os.path.exists(os.path.join(env["RESULTS"], "verify-dns.json")),
                "a blocked hop wrote evidence for checks it never ran")
            self.assertEqual(os.listdir(state_dir), [],
                             "a blocked HOP D skipped the clone teardown")


class GoldenGuestDns(unittest.TestCase):
    """The golden records the resolver it asked for, and checks it landed.

    `podman prepare --dns` is a request; metadata.network_config.dns_server in
    the installed snapshot is what fc-agent actually wrote to resolv.conf. The
    provenance carries GUEST_DNS as `guest_dns` so a later reader can compare
    the request against every verify's evidence, and the golden refuses a
    snapshot whose recorded resolver is not the one requested.
    """

    def _golden(self, d, guest_dns, baked):
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        os.makedirs(os.path.join(d, "state"))
        network = {} if baked is None else {"dns_server": baked}
        config = json.dumps({
            "generation_id": "12345678-1234-4234-8234-123456789abc",
            "created_at": "2026-08-09T00:00:00Z",
            "vm_id": "vm-11111111111111111111111111111111",
            "metadata": {
                "image": "localhost/chromium-bench-req",
                "image_disk_path": "/image-cache/%s.storage-v2.img" % ("a" * 64),
                "network_config": network,
            },
        })
        fcvm = os.path.join(d, "fcvm")
        write_exec(fcvm, f'''#!/bin/bash
case "$1 $2" in
  "podman prepare")
      mkdir -p "$DATA_ROOT/snapshots/$TAG"
      : > "$DATA_ROOT/snapshots/$TAG.lock"
      printf '%s\\n' {config!r} > "$DATA_ROOT/snapshots/$TAG/config.json"
      digest=$(sha256sum "$DATA_ROOT/snapshots/$TAG/config.json" | cut -d" " -f1)
      printf '{{"status":"prepared","generation_id":"%s","config_digest":"%s"}}\\n' \\
          "12345678-1234-4234-8234-123456789abc" "$digest"
      ;;
esac
''')
        write_exec(os.path.join(d, "fc-agent"), "#!/bin/bash\nexit 0\n")
        write_exec(os.path.join(binx, "podman"), '''#!/bin/bash
if [ "$1 $2" = "image inspect" ]; then
    echo '[{"Digest":"sha256:%s","Id":"%s"}]'
    exit 0
fi
exit 1
''' % ("a" * 64, "b" * 64))
        env = dict(os.environ)
        env.pop("GUEST_DNS", None)
        env.update(
            PATH=binx + os.pathsep + env["PATH"],
            RESULTS=os.path.join(d, "results"),
            STATE_DIR=os.path.join(d, "state"),
            DATA_ROOT=d,
            ALLOW_BUSY="1",
            RUNID=RUN_ID,
            FCVM=fcvm,
            FC_AGENT=os.path.join(d, "fc-agent"),
        )
        if guest_dns is not None:
            env["GUEST_DNS"] = guest_dns
        result = subprocess.run([SH, "golden"], env=env, capture_output=True,
                                text=True, timeout=120)
        provenance = os.path.join(d, "snapshots", "cb-req-golden",
                                  "reqbench-provenance.json")
        return result, provenance

    def test_guest_dns_is_recorded_when_the_snapshot_baked_it(self):
        with tempfile.TemporaryDirectory() as d:
            result, provenance = self._golden(d, "10.0.2.2", "10.0.2.2")
            self.assertEqual(result.returncode, 0, result.stderr[-2000:])
            with open(provenance) as handle:
                self.assertEqual(json.load(handle)["guest_dns"], "10.0.2.2")

    def test_a_snapshot_baked_with_another_resolver_fails_the_golden(self):
        """A --dns the guest ignored is exactly the silent contamination the
        corpus campaign cannot afford: the golden must not be installed with
        provenance claiming a resolver its resolv.conf does not have."""
        with tempfile.TemporaryDirectory() as d:
            result, provenance = self._golden(d, "10.0.2.2", "10.0.2.3")
            self.assertNotEqual(result.returncode, 0,
                                "golden accepted a snapshot whose resolver is "
                                f"not the one requested\n{result.stdout}")
            self.assertIn("10.0.2.2", result.stderr)
            self.assertIn("10.0.2.3", result.stderr)
            self.assertFalse(os.path.exists(provenance),
                             "provenance was written for a refused golden")

    def test_an_unset_guest_dns_records_null(self):
        """fcvm fills dns_server from the host resolver when --dns is not
        given, so null means "not requested", never "no resolver"."""
        with tempfile.TemporaryDirectory() as d:
            result, provenance = self._golden(d, None, "10.0.2.3")
            self.assertEqual(result.returncode, 0, result.stderr[-2000:])
            with open(provenance) as handle:
                record = json.load(handle)
            self.assertIn("guest_dns", record)
            self.assertIsNone(record["guest_dns"])


if __name__ == "__main__":
    unittest.main()
