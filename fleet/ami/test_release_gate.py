#!/usr/bin/env python3
"""Tests of fleet/ami/release_gate.py and the workflow that runs it (fleet-release-gate.yml)."""
import hashlib
import subprocess
import importlib.util
import json
import os
import pathlib
import re
import sys
import tempfile
import io
import unittest
from contextlib import redirect_stdout
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
import release_gate  # noqa: E402

WORKFLOW = (ROOT / ".github/workflows/fleet-release-gate.yml").read_text()


class Lambda:
    """`aws lambda invoke` that records each payload and answers from a list."""

    def __init__(self, *answers):
        self.answers, self.payloads = list(answers), []

    def __call__(self, argv, **_):
        assert argv[:5] == ["aws", "lambda", "invoke", "--function-name", release_gate.FUNCTION], argv
        self.payloads.append(json.loads(argv[argv.index("--payload") + 1]))
        pathlib.Path(argv[-1]).write_text(json.dumps(self.answers.pop(0)))
        return mock.Mock(returncode=0, stdout=json.dumps({"StatusCode": 200}))


class GateTests(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(os.environ, {"RELEASE_GATE": release_gate.FUNCTION}, clear=False)
        patcher.start()
        self.addCleanup(patcher.stop)

    # Placeholder instance ids: the gate's raw answer names the release's hosts and tile instances.
    HOSTS = ["i-0example0host00a", "i-0example0host00b"]
    TILES = ["i-0example0tile00a"]

    @staticmethod
    def digest(instance):
        # The attribution digest both sides compute (fleet/coord/stage-smoke.py, release_gate.py):
        # instance ids are not printed in this public repository's logs.
        return hashlib.sha256(b"fcvm-release-gate\0" + instance.encode()).hexdigest()

    # A status as ejc3/aws#862's gate returns it: the live receipt and the placement serving it.
    def status(self, **changes):
        placement = {"current": True, "settled": True, "release_hosts": list(self.HOSTS),
                     "release_tiles": list(self.TILES), "digest": "d" * 64, "changed_kinds": ["host", "tile"]}
        placement.update(changes.pop("placement", {}))
        value = {"generation": 15, "build_id": "fcvm-fleet-image-factory:example-build", "source_sha": "a" * 40,
                 "verified": False, "rollback": False, "placement": placement}
        value.update(changes)
        return value

    def attribution(self, hosts=None, instances=None, ok=True, mode="wall"):
        """What the smoke records of each probe: the digests of the host and tile instances the
        coordinator reported serving it."""
        hosts = self.HOSTS[:1] if hosts is None else hosts
        return {"mode": mode, "tile": "fcvm:1", "ok": ok,
                "verdict": "pass" if ok else "image_failure",
                "hosts": [self.digest(h) for h in hosts], "instances": [self.digest(i) for i in (instances or [])]}

    def summary(self, name="v4", failed=0, attribution=None):
        if attribution is None:
            attribution = ([self.attribution(), self.attribution(hosts=self.HOSTS[1:2]),
                            self.attribution(hosts=[], instances=self.TILES)])
            attribution += [self.attribution(mode=mode) for mode in release_gate.MODES if mode != "wall"]
            attribution += [self.attribution(ok=False) for _ in range(failed)]
        return {"family": name, "modes": ["wall", "phones", "windows", "minecraft", "potato"],
                "probes": len(attribution), "failed": failed, "attribution": attribution}

    SUMMARY = {"family": "v4", "modes": ["wall", "phones", "windows", "minecraft", "potato"], "probes": 26, "failed": 0}

    def family(self, name, failed=0, attribution=None):
        return json.dumps(self.summary(name, failed, attribution))

    def redacted(self, status):
        """The status as the status job hands it on: release instance ids replaced by digests."""
        value = json.loads(json.dumps(status))
        for key in ("release_hosts", "release_tiles"):
            if isinstance(value.get("placement"), dict) and isinstance(value["placement"].get(key), list):
                value["placement"][key] = [self.digest(i) for i in value["placement"][key]]
        return value

    def decide(self, gate, before=None, v4=None, v6=None, agents=None):
        before = self.redacted(before if before is not None else self.status())
        argv = ["decide", "--before", json.dumps(before),
                "--v4", self.family("v4") if v4 is None else v4, "--v6", self.family("v6") if v6 is None else v6]
        if agents is not None:
            argv += ["--agents", agents]
        return release_gate.main(argv, run=gate)

    def agents(self, instances=None, observed_at=1_794_780_000_000, since=1_794_770_000_000):
        """The coordinator's record of registered agents, as stage-smoke.py --agents prints it."""
        names = self.HOSTS if instances is None else instances
        return json.dumps({"since": since, "registered": sorted(self.digest(i) for i in names),
                           "observed_at": observed_at})

    def test_probes_that_decide_are_never_overruled_by_an_empty_agent_list(self):
        # A coordinator restart clears its registered agents until they reconnect (Codex on
        # #1925): an empty list next to complete, attributed probes is that, not a broken image.
        gate = Lambda(self.status(), {"status": "verified"})
        self.assertEqual(self.decide(gate, agents=self.agents(instances=[])), 0)
        self.assertEqual([p["action"] for p in gate.payloads], ["status", "verify"])
        gate = Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}})
        self.assertEqual(self.decide(gate, v4=self.family("v4", failed=1), agents=self.agents(instances=[])), 0)
        self.assertEqual(gate.payloads[1]["evidence"]["kind"], "image_failure")

    def test_release_hosts_without_an_agent_roll_back(self):
        # A host image that boots but never starts its agent: the smoke reaches no tile, so it
        # reports no probe summary, and the coordinator lists none of the release's hosts.
        gate = Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}})
        self.assertEqual(self.decide(gate, v4="", v6="", agents=self.agents(instances=[])), 0)
        self.assertEqual(gate.payloads[1], {
            "action": "rollback",
            "receipt": {"generation": 15, "build_id": "fcvm-fleet-image-factory:example-build"},
            "placement": "d" * 64,
            "evidence": {"kind": "no_agent", "hosts": sorted(self.digest(h) for h in self.HOSTS),
                         "observed_at": 1_794_780_000_000, "coordinator_since": 1_794_770_000_000}})
        # An agent of a host outside the release does not count for it.
        gate = Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}})
        self.assertEqual(self.decide(gate, v4="", v6="", agents=self.agents(instances=["i-0example0other0a"])), 0)
        self.assertEqual(gate.payloads[1]["evidence"]["kind"], "no_agent")

    def test_an_agent_on_any_release_host_leaves_the_probes_to_decide(self):
        gate = Lambda(self.status(), {"status": "verified"})
        self.assertEqual(self.decide(gate, agents=self.agents(instances=self.HOSTS[1:])), 0)
        self.assertEqual([p["action"] for p in gate.payloads], ["status", "verify"])
        gate = Lambda(self.status())
        self.assertEqual(self.decide(gate, v4="", v6="", agents=self.agents()), 0)
        self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_no_agent_decides_nothing_unless_the_release_changed_the_host_image_and_its_hosts_held_still(self):
        none = self.agents(instances=[])
        for after in (self.status(placement={"changed_kinds": ["tile"]}),
                      self.status(placement={"digest": "e" * 64, "release_hosts": self.HOSTS[:1]}),
                      self.status(placement={"digest": "e" * 64, "release_hosts": self.HOSTS + ["i-0example0host00c"]}),
                      self.status(placement={"settled": False}), self.status(generation=16)):
            with self.subTest(after=after["placement"]):
                gate = Lambda(after)
                before = self.status(placement={"changed_kinds": after["placement"]["changed_kinds"]})
                self.assertEqual(self.decide(gate, before=before, v4="", v6="", agents=none), 0)
                self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_no_agent_rolls_back_when_only_tiles_moved_during_the_probes(self):
        # Staging, 2026-11-18 (generation 21): the release host never registered, and AWS
        # interrupted one Spot tile during the 27-minute smoke. The placement digest moved, so the
        # run decided nothing and the rollback waited for an operator. The no-agent evidence names
        # only the release's hosts and their registrations, so a tile replacement does not touch
        # it: the claim is bound to the placement the agents were read against, which the gate
        # checks again under its lock.
        moved = self.status(placement={"digest": "e" * 64, "release_tiles": ["i-0example0tile00b"]})
        gate = Lambda(moved, {"status": "rolled_back", "pointer": {}, "hosts": {}})
        self.assertEqual(self.decide(gate, v4="", v6="", agents=self.agents(instances=[])), 0)
        self.assertEqual(gate.payloads[1]["action"], "rollback")
        self.assertEqual(gate.payloads[1]["placement"], "e" * 64)
        self.assertEqual(gate.payloads[1]["evidence"]["hosts"], sorted(self.digest(h) for h in self.HOSTS))
        # Probes still decide only on a placement that held still: a pass or a failure measured
        # on tiles that have since moved is not laid on the new placement.
        for v4 in (self.family("v4"), self.family("v4", failed=2)):
            with self.subTest(v4=v4):
                gate = Lambda(moved)
                self.assertEqual(self.decide(gate, v4=v4, agents=self.agents(instances=[])), 0)
                self.assertEqual(gate.payloads, [{"action": "status"}])
        # An agent on a release host still leaves the moved placement undecided.
        gate = Lambda(moved)
        self.assertEqual(self.decide(gate, v4="", v6="", agents=self.agents()), 0)
        self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_a_gate_call_the_cli_could_not_complete_is_repeated(self):
        # Staging, 2026-11-18 (generation 19): both families reported six attributed image
        # failures and no inconclusive probe, and `aws lambda invoke` exited 254. The verdict was
        # lost with no retry and no reason; the rollback came two runs and 6.5 minutes later.
        # Every gate action is safe to repeat: status reads, verify records the same receipt, and
        # the gate answers a repeated rollback of a committed receipt with the committed answer.
        throttled = ("\nAn error occurred (TooManyRequestsException) when calling the Invoke operation "
                     "(reached max retries: 4): Rate Exceeded.\n")
        for failures in (1, len(release_gate.INVOKE_WAITS)):
            with self.subTest(failures=failures):
                inner = Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}})
                calls = []

                def gate(argv, **kw):
                    calls.append(json.loads(argv[argv.index("--payload") + 1])["action"])
                    if calls.count("rollback") and calls.count("rollback") <= failures:
                        return mock.Mock(returncode=254, stdout="", stderr=throttled)
                    return inner(argv, **kw)
                err = io.StringIO()
                with mock.patch.object(release_gate.time, "sleep") as sleep, \
                        mock.patch.object(sys, "stderr", err), redirect_stdout(io.StringIO()):
                    self.assertEqual(self.decide(gate, v4=self.family("v4", failed=1)), 0)
                self.assertEqual(calls, ["status"] + ["rollback"] * (failures + 1))
                self.assertEqual([c.args[0] for c in sleep.call_args_list], list(release_gate.INVOKE_WAITS[:failures]))
                self.assertIn("TooManyRequestsException", err.getvalue())
                self.assertNotIn("Rate Exceeded", err.getvalue())
        # A call that never completes stops after the last wait and fails the job.
        failing = mock.Mock(return_value=mock.Mock(returncode=254, stdout="", stderr=throttled))
        with mock.patch.object(release_gate.time, "sleep"), mock.patch.object(sys, "stderr", io.StringIO()):
            with self.assertRaisesRegex(SystemExit, r"exit 254, TooManyRequestsException"):
                release_gate.invoke({"action": "status"}, failing)
        self.assertEqual(failing.call_count, len(release_gate.INVOKE_WAITS) + 1)

    def test_a_gate_call_waits_for_the_function_rather_than_invoking_it_again(self):
        # The gate function runs up to 900 s with one reserved execution (ejc3/aws
        # fcvm-fleet-ami-pipeline.tf). The CLI's default 60 s read timeout makes the CLI invoke it
        # again while a rollback is still running, which the reserved execution refuses.
        gate = Lambda(self.status())
        argv = []
        release_gate.invoke({"action": "status"}, lambda a, **kw: (argv.extend(a), gate(a, **kw))[1])
        self.assertGreater(int(argv[argv.index("--cli-read-timeout") + 1]), 900)

    def test_a_missing_or_malformed_agent_snapshot_decides_nothing_without_probes(self):
        good = {"since": 2, "registered": [], "observed_at": 3}
        for raw in ("", "not json", json.dumps(dict(good, registered=["x"])), json.dumps(dict(good, since="then")),
                    json.dumps({"registered": [], "observed_at": 3}), json.dumps(dict(good, since=0)),
                    json.dumps(dict(good, observed_at=1)), json.dumps(dict(good, extra=1)),
                    json.dumps({"agents": [], "observed_at": 3, "watched_ms": 120000})):
            with self.subTest(raw=raw):
                gate = Lambda(self.status())
                self.assertEqual(self.decide(gate, v4="", v6="", agents=raw), 0)
                self.assertNotIn("rollback", [p.get("action") for p in gate.payloads])

    def test_the_decision_reads_the_coordinators_agents_after_the_smoke(self):
        decide = WORKFLOW.split("\n  decide:\n", 1)[1]
        self.assertIn("python3 -B fleet/coord/stage-smoke.py --agents", decide)
        # The snapshot is selected as JSON (jq), not by matching its text, and the workflow's own
        # filter passes what stage-smoke.py --agents prints (Codex on #1925: a filter on a key the
        # producer no longer writes passed nothing).
        self.assertNotIn("grep", decide)
        selector = re.search(r"\| (jq -cR '[^']+') \|", decide)
        self.assertIsNotNone(selector, decide)
        spec = importlib.util.spec_from_file_location("smoke", ROOT / "fleet/coord/stage-smoke.py")
        smoke = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(smoke)
        line = smoke.agents_line({"fleet": {"agents": {"since": 1, "hosts": [{"name": self.HOSTS[0]}]}}}, 2)
        printed = "\n".join(["step text", json.dumps({"status": "failed"}), json.dumps(line)]) + "\n"
        out = subprocess.run(["bash", "-c", selector.group(1)], input=printed, capture_output=True, text=True, check=True)
        self.assertEqual(release_gate.agent_snapshot(out.stdout.strip().splitlines()[-1]), line)
        self.assertIn('--agents "$AGENTS"', decide)
        for secret in ("CF_ACCESS_CLIENT_ID: ${{ secrets.CF_ACCESS_CLIENT_ID }}",
                       "CF_ACCESS_CLIENT_SECRET: ${{ secrets.CF_ACCESS_CLIENT_SECRET }}",
                       "STAGING_API_URL: ${{ vars.STAGING_API_URL }}"):
            self.assertIn(secret, decide)

    def test_a_pass_on_both_families_verifies_the_exact_receipt(self):
        gate = Lambda(self.status(), {"status": "verified", "generation": 15})
        self.assertEqual(self.decide(gate), 0)
        self.assertEqual(gate.payloads[0], {"action": "status"})
        self.assertEqual(gate.payloads[1], {"action": "verify",
                                            "receipt": {"generation": 15, "build_id": "fcvm-fleet-image-factory:example-build"},
                                            "placement": "d" * 64})

    def test_a_probe_failure_on_a_stable_current_placement_rolls_the_receipt_back(self):
        gate = Lambda(self.status(), {"status": "rolled_back", "pointer": {"generation": 16}, "hosts": {"withdrawn_running": 1}})
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(self.decide(gate, v6=self.family("v6", failed=1)), 0)
        self.assertEqual(gate.payloads[1], {"action": "rollback",
                                            "receipt": {"generation": 15, "build_id": "fcvm-fleet-image-factory:example-build"},
                                            "placement": "d" * 64, "evidence": {
                                                "kind": "image_failure", "family": "v6",
                                                "hosts": [self.digest(self.HOSTS[0])], "instances": [],
                                            }})
        # The pointer moving back and the hosts already running are reported apart.
        self.assertIn("release pointer: rolled back", out.getvalue())
        self.assertIn("running hosts:", out.getvalue())

    def test_host_only_release_ignores_failure_from_unchanged_ec2_tile(self):
        prior = self.status(placement={"changed_kinds": ["host"]})
        summary = self.summary("v6", failed=1)
        summary["attribution"][-1] = self.attribution(hosts=[], instances=self.TILES, ok=False)
        gate = Lambda(prior, {"status": "rolled_back"})
        self.assertEqual(self.decide(gate, before=prior, v6=json.dumps(summary)), 0)
        self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_release_chooses_failure_from_the_changed_image_not_a_prior_unrelated_failure(self):
        for changed_kind, first, second in (
            ("host", self.attribution(hosts=[], instances=self.TILES, ok=False), self.attribution(ok=False)),
            ("tile", self.attribution(ok=False), self.attribution(hosts=[], instances=self.TILES, ok=False)),
        ):
            with self.subTest(changed_kind=changed_kind):
                prior = self.status(placement={"changed_kinds": [changed_kind]})
                summary = self.summary("v6", failed=2)
                summary["attribution"][-2:] = [first, second]
                gate = Lambda(prior, {"status": "rolled_back"})
                self.assertEqual(self.decide(gate, before=prior, v6=json.dumps(summary)), 0)
                self.assertEqual(gate.payloads[-1]["action"], "rollback")
                self.assertEqual(gate.payloads[-1]["evidence"], {
                    "kind": "image_failure", "family": "v6", "hosts": second["hosts"], "instances": second["instances"],
                })

    def test_neither_image_changed_does_not_make_a_failure_a_pass_or_a_rollback(self):
        prior = self.status(placement={"changed_kinds": []})
        gate = Lambda(prior, {"status": "rolled_back"})
        self.assertEqual(self.decide(gate, before=prior, v6=self.family("v6", failed=1)), 0)
        self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_no_or_malformed_changed_kinds_cannot_ready_a_release(self):
        for value in (None, "host", ["no-such-kind"], ["host", "host"], [["host"]]):
            with self.subTest(value=value):
                prior = self.status(placement={"changed_kinds": value})
                gate = Lambda()
                self.assertEqual(self.decide(gate, before=prior), 0)
                self.assertEqual(gate.payloads, [])
        legacy = self.status()
        del legacy["placement"]["changed_kinds"]
        gate = Lambda()
        self.assertEqual(self.decide(gate, before=legacy), 0)
        self.assertEqual(gate.payloads, [])

    def test_an_attributed_transport_failure_does_not_roll_back(self):
        # This now happens after the coordinator supplied a known release host. A socket
        # timeout is still no answer from the application on that image.
        summary = self.summary("v6", failed=1)
        summary["attribution"][-1]["verdict"] = "inconclusive"
        gate = Lambda(self.status(), {"status": "rolled_back"})
        self.assertEqual(self.decide(gate, v6=json.dumps(summary)), 0)
        self.assertNotIn("rollback", [p["action"] for p in gate.payloads])
        self.assertNotIn("verify", [p["action"] for p in gate.payloads])

    def test_an_inconclusive_probe_blocks_even_with_another_positive_failure(self):
        summary = self.summary("v6", failed=2)
        summary["attribution"][-1]["verdict"] = "inconclusive"
        gate = Lambda(self.status(), {"status": "rolled_back"})
        self.assertEqual(self.decide(gate, v6=json.dumps(summary)), 0)
        self.assertNotIn("rollback", [p["action"] for p in gate.payloads])

    def test_missing_or_inconsistent_verdicts_do_not_trigger_gate_actions(self):
        def omit_verdict(s):
            del s["attribution"][-1]["verdict"]

        cases = (
            omit_verdict,
            lambda s: s.update(family="v4"),
            lambda s: s.update(failed=0),
            lambda s: s["attribution"][-1].update(verdict="pass"),
            lambda s: s["attribution"][-1].update(mode="aggregate"),
            lambda s: s["attribution"][-1].update(mode="potato"),
            lambda s: s["attribution"][-1].update(instances=[self.digest(self.TILES[0])]),
            lambda s: s["attribution"][-1].update(hosts=[self.digest(h) for h in self.HOSTS]),
            lambda s: s["attribution"][0].update(ok=1),
            lambda s: [e.update(mode="wall") for e in s["attribution"]],
        )
        for tamper in cases:
            with self.subTest(tamper=tamper):
                summary = self.summary("v6", failed=1)
                tamper(summary)
                gate = Lambda(self.status(), {"status": "rolled_back"})
                self.assertEqual(self.decide(gate, v6=json.dumps(summary)), 0)
                self.assertTrue(all(p["action"] == "status" for p in gate.payloads))

    def test_a_runner_or_auth_failure_decides_nothing(self):
        # A family whose job did not run, or did not get as far as its probes (setup, credentials,
        # runner, an unreachable coordinator), reports no summary.
        for v4, v6 in (("", self.family("v6")), (self.family("v4"), ""), ("", ""), ("not json", self.family("v6")),
                       (json.dumps(dict(self.SUMMARY, probes=0)), self.family("v6")),
                       (json.dumps(dict(self.SUMMARY, modes=["wall"])), self.family("v6"))):
            with self.subTest(v4=v4, v6=v6):
                gate = Lambda(self.status())
                self.assertEqual(self.decide(gate, v4=v4, v6=v6), 0)
                self.assertNotIn("verify", [p.get("action") for p in gate.payloads])
                self.assertNotIn("rollback", [p.get("action") for p in gate.payloads])

    def test_a_placement_that_changed_during_the_probe_decides_nothing(self):
        for after in (self.status(placement={"digest": "e" * 64}), self.status(generation=16),
                      self.status(build_id="fcvm-fleet-image-factory:other"),
                      self.status(placement={"changed_kinds": ["tile"]})):
            with self.subTest(after=after):
                gate = Lambda(after)
                self.assertEqual(self.decide(gate, v4=self.family("v4", failed=3)), 0)
                self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_a_mixed_unsettled_or_unobservable_placement_decides_nothing(self):
        for placement in ({"current": False}, {"settled": False}, {"release_hosts": []}, {"digest": None}):
            for which in ("before", "after"):
                with self.subTest(placement=placement, which=which):
                    bad = self.status(placement=placement)
                    gate = Lambda(bad if which == "after" else self.status())
                    self.assertEqual(self.decide(gate, before=bad if which == "before" else None,
                                                 v4=self.family("v4", failed=2)), 0)
                    self.assertEqual(gate.payloads, [] if which == "before" else [{"action": "status"}])
        # A status from a gate without the placement fields (before ejc3/aws#862's receipt patch).
        legacy = {"generation": 15, "source_sha": "a" * 40, "verified": False, "rollback": False, "current": True}
        gate = Lambda()
        self.assertEqual(self.decide(gate, before=legacy, v4=self.family("v4", failed=2)), 0)
        self.assertEqual(gate.payloads, [])

    def test_a_refused_rollback_fails_the_job(self):
        for reason in ("the live release has no previous release to roll back to", "the previous release was not verified"):
            with self.subTest(reason=reason):
                gate = Lambda(self.status(), {"status": "refused", "reason": reason})
                with self.assertRaisesRegex(SystemExit, reason):
                    self.decide(gate, v4=self.family("v4", failed=1))

    def test_status_writes_whether_the_live_release_is_ready_to_smoke(self):
        for status, ready in ((self.status(), "true"), (self.status(verified=True), "false"),
                              (self.status(placement={"settled": False}), "false"),
                              (self.status(placement={"current": False}), "false")):
            with self.subTest(status=status), tempfile.NamedTemporaryFile("r") as out, \
                    mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}):
                self.assertEqual(release_gate.main(["status"], run=Lambda(status)), 0)
                lines = dict(line.split("=", 1) for line in out.read().splitlines())
                self.assertEqual(lines["ready"], ready)
                self.assertEqual(json.loads(lines["status"]), self.redacted(status))

    def test_status_never_prints_or_hands_on_a_release_instance_id(self):
        with tempfile.NamedTemporaryFile("r") as out, mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}):
            printed = io.StringIO()
            with redirect_stdout(printed):
                self.assertEqual(release_gate.main(["status"], run=Lambda(self.status())), 0)
            handed = out.read()
        for instance in self.HOSTS + self.TILES:
            self.assertNotIn(instance, printed.getvalue())
            self.assertNotIn(instance, handed)

    def test_a_probe_on_a_host_outside_the_release_gives_no_verdict(self):
        # The placement digest stayed the same, but the coordinator said this probe's tile ran on
        # a host that is not on the release's host image, or on a tile instance not on its tile image.
        for failed in (0, 1):
            for stray in (self.attribution(hosts=["i-0example0other0a"]),
                          self.attribution(hosts=[], instances=["i-0example0other0b"]),
                          self.attribution(hosts=self.HOSTS + ["i-0example0other0a"])):
                with self.subTest(failed=failed, stray=stray):
                    summary = self.summary("v6", failed)
                    summary["attribution"].append(stray)
                    summary["probes"] += 1
                    gate = Lambda(self.status())
                    self.assertEqual(self.decide(gate, v6=json.dumps(summary)), 0)
                    self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_a_probe_without_host_attribution_gives_no_verdict(self):
        bare = self.summary("v4")
        del bare["attribution"]
        empty = self.summary("v4")
        empty["attribution"].append(self.attribution(hosts=[], instances=[]))
        empty["probes"] += 1
        short = self.summary("v4")
        short["probes"] += 1
        not_a_digest = self.summary("v4")
        not_a_digest["attribution"][0]["hosts"] = [self.HOSTS[0]]
        for summary in (bare, empty, short, not_a_digest):
            with self.subTest(summary=summary):
                gate = Lambda(self.status())
                self.assertEqual(self.decide(gate, v4=json.dumps(summary)), 0)
                self.assertEqual(gate.payloads, [{"action": "status"}])

    def test_every_probe_on_a_release_host_with_a_pass_verifies(self):
        gate = Lambda(self.status(), {"status": "verified"})
        self.assertEqual(self.decide(gate, v4=self.family("v4"), v6=self.family("v6")), 0)
        self.assertEqual([p["action"] for p in gate.payloads], ["status", "verify"])

    def test_every_probe_on_a_release_host_with_a_fail_rolls_back(self):
        gate = Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}})
        self.assertEqual(self.decide(gate, v4=self.family("v4", failed=2), v6=self.family("v6")), 0)
        self.assertEqual([p["action"] for p in gate.payloads], ["status", "rollback"])

    def test_a_family_summary_is_read_from_the_smoke_lines(self):
        lines = [json.dumps({"mode": "wall", "tile": "fcvm:1", "ok": True}), json.dumps(dict(self.SUMMARY, failed=2)),
                 json.dumps({"status": "failed", "step": "viewer path"})]
        with tempfile.NamedTemporaryFile("w", delete=False) as smoke:
            smoke.write("\n".join(lines) + "\n")
        with tempfile.NamedTemporaryFile("r") as out, mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}):
            self.assertEqual(release_gate.main(["summary", smoke.name]), 0)
            self.assertEqual(json.loads(out.read().split("=", 1)[1]), dict(self.SUMMARY, failed=2))
        with open(smoke.name, "w") as handle:
            handle.write(json.dumps({"status": "failed", "step": "/live"}) + "\n")
        with tempfile.NamedTemporaryFile("r") as out, mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}):
            self.assertEqual(release_gate.main(["summary", smoke.name]), 0)
            self.assertEqual(out.read(), "summary=\n")
        os.unlink(smoke.name)

    def test_only_the_gate_function_is_invoked(self):
        with mock.patch.dict(os.environ, {"RELEASE_GATE": "fcvm-fleet-ami-publish"}):
            with self.assertRaisesRegex(SystemExit, "must name"):
                release_gate.main(["status"], run=Lambda())

    def test_a_promotion_starts_the_gate_at_once_and_the_schedule_is_the_fallback(self):
        # Whatever records a promotion starts the gate directly (repository_dispatch from the
        # publisher, or `gh workflow run` by whoever saw the new receipt), so a bad release does not
        # wait for the schedule; the schedule only catches a promotion nobody announced.
        on = WORKFLOW.split("\non:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("  repository_dispatch:\n    types: [fleet-release-promoted]", on)
        self.assertIn("  workflow_dispatch:", on)
        self.assertIn("    - cron: '*/5 * * * *'", on)

    def test_the_smoke_runs_only_while_the_release_is_ready_and_its_verdicts_reach_the_decision(self):
        smoke = WORKFLOW.split("\n  smoke:\n", 1)[1].split("\n  decide:\n", 1)[0]
        self.assertIn("if: needs.status.outputs.ready == 'true'", smoke)
        decide = WORKFLOW.split("\n  decide:\n", 1)[1]
        self.assertIn("if: ${{ !cancelled() && needs.status.outputs.ready == 'true' }}", decide)
        for line in ("BEFORE: ${{ needs.status.outputs.status }}", "V4: ${{ needs.smoke.outputs.v4 }}",
                     "V6: ${{ needs.smoke.outputs.v6 }}",
                     'release_gate.py decide --before "$BEFORE" --v4 "$V4" --v6 "$V6"'):
            self.assertIn(line, decide)
        viewer = (ROOT / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        call = viewer.split("  workflow_call:\n", 1)[1].split("\n  schedule:", 1)[0]
        self.assertIn("v4:\n        value: ${{ jobs.ipv4.outputs.summary }}", call)
        self.assertIn("v6:\n        value: ${{ jobs.ipv6.outputs.summary }}", call)
        for job in ("ipv4", "ipv6"):
            body = viewer.split(f"\n  {job}:\n", 1)[1].split("\n  ipv6:\n" if job == "ipv4" else "\n  report:\n", 1)[0]
            self.assertIn("summary: ${{ steps.verdict.outputs.summary }}", body)
            self.assertIn("id: verdict", body)
            self.assertIn("if: ${{ always() }}", body)

    def test_the_gates_smoke_replaces_no_tile(self):
        # The smoke's relaunch launches a new EC2 instance for a tile and terminates the old one,
        # after the viewer path ran. That changes the placement captured before the probes, so a
        # smoke with relaunches could never reach a verdict. The gate's call turns them off, and
        # only the gate's: a deploy's and the daily smoke still relaunch.
        smoke = WORKFLOW.split("\n  smoke:\n", 1)[1].split("\n  decide:\n", 1)[0]
        self.assertIn("    with:\n      relaunch: false\n", smoke)
        viewer = (ROOT / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        call = viewer.split("  workflow_call:\n", 1)[1].split("\n  schedule:", 1)[0]
        self.assertIn("    inputs:\n", call)
        self.assertIn("      relaunch:\n        type: boolean\n        default: true\n", call)
        ipv4 = viewer.split("\n  ipv4:\n", 1)[1].split("\n  ipv6:\n", 1)[0]
        ipv6 = viewer.split("\n  ipv6:\n", 1)[1].split("\n  report:\n", 1)[0]
        self.assertIn("RELAUNCH: ${{ inputs.relaunch }}", ipv4)
        # Unset on the schedule and on a manual run: those relaunch.
        self.assertIn('relaunch=--relaunch; [ "$RELAUNCH" = false ] && relaunch=', ipv4)
        self.assertNotIn(" --relaunch ", ipv4)
        self.assertNotIn("--relaunch", ipv6)
        for name in ("fleet-coordinator-deploy.yml", "fleet-web-deploy.yml"):
            self.assertNotIn("relaunch: false", (ROOT / ".github/workflows" / name).read_text(), name)

    def test_the_smoke_call_grants_what_the_smoke_workflow_needs(self):
        # A called workflow cannot raise its caller's permissions: the smoke's report job opens an
        # issue, so the calling job grants issues: write, as the deploy workflows that call it do.
        smoke = WORKFLOW.split("\n  smoke:\n", 1)[1].split("\n  decide:\n", 1)[0]
        self.assertIn("    permissions:\n      contents: read\n      issues: write\n", smoke)

    def outputs_of(self, argv, gate):
        with tempfile.NamedTemporaryFile("r") as out, mock.patch.dict(os.environ, {"GITHUB_OUTPUT": out.name}), \
                redirect_stdout(io.StringIO()):
            self.assertEqual(release_gate.main(argv, run=gate), 0)
            return dict(line.split("=", 1) for line in out.read().splitlines())

    def test_status_says_whether_an_unverified_release_is_still_settling(self):
        # Staging, 2026-11-18: generation 21 was current at 09:00:23 and settled at 09:05:23,
        # but nothing looks before the schedule's next run, which came 8 to 37 minutes apart
        # (median 9.3) over 2026-11-15 to 2026-11-18. `waiting` lets the workflow look again soon.
        for status, unverified, waiting in (
                (self.status(), "true", "false"),
                (self.status(placement={"settled": False}), "true", "true"),
                (self.status(placement={"current": False}), "true", "true"),
                (self.status(placement={"release_hosts": []}), "true", "false"),
                (self.status(verified=True), "false", "false"),
                (self.status(verified=True, placement={"settled": False}), "false", "false")):
            with self.subTest(status=status):
                lines = self.outputs_of(["status"], Lambda(status))
                self.assertEqual((lines["unverified"], lines["waiting"]), (unverified, waiting))
                self.assertEqual(lines["generation"], "15" if unverified == "true" else "")

    def test_decide_says_whether_it_reached_a_verdict(self):
        for gate, v4, verdict in ((Lambda(self.status(), {"status": "verified"}), self.family("v4"), "verified"),
                                  (Lambda(self.status(), {"status": "rolled_back", "pointer": {}, "hosts": {}}),
                                   self.family("v4", failed=1), "rolled_back"),
                                  (Lambda(self.status(placement={"digest": "e" * 64})), self.family("v4"), "none")):
            with self.subTest(verdict=verdict):
                argv = ["decide", "--before", json.dumps(self.redacted(self.status())), "--v4", v4,
                        "--v6", self.family("v6")]
                self.assertEqual(self.outputs_of(argv, gate)["verdict"], verdict)

    class Gh:
        """`gh` that lists the gate's dispatched runs from `titles` and records each dispatch."""

        def __init__(self, titles=(), list_status=0):
            self.titles, self.list_status, self.calls = list(titles), list_status, []

        def __call__(self, argv, **_):
            self.calls.append(argv)
            if argv[:3] == ["gh", "run", "list"]:
                return mock.Mock(returncode=self.list_status, stderr="",
                                 stdout=json.dumps([{"displayTitle": t} for t in self.titles]))
            assert argv[:3] == ["gh", "workflow", "run"], argv
            return mock.Mock(returncode=0, stdout="", stderr="")

    def again(self, gh, generation="21", since="", now=10_000_000):
        with mock.patch.object(release_gate.time, "time", return_value=now), \
                mock.patch.object(release_gate.time, "sleep") as sleep, redirect_stdout(io.StringIO()):
            self.assertEqual(release_gate.main(["again", "--generation", generation, "--since", since], run=gh), 0)
        return sleep, [c for c in gh.calls if c[:3] == ["gh", "workflow", "run"]]

    def test_an_unsettled_or_undecided_release_is_looked_at_again_for_two_hours(self):
        now = 10_000_000
        title = "Fleet image release gate: generation {} since {}".format
        # The first look starts the clock; a later run carries it, and so does any earlier run of
        # the same generation in the history, so the schedule's own runs cannot restart it.
        for titles, since, first in (([], "", now), ([], str(now - 600), now - 600),
                                     ([title(21, now - 3000), title(20, now - 9000), "Fleet image release gate"],
                                      str(now - 600), now - 3000),
                                     ([title(21, now + 50)], "", now), ([], "not a time", now)):
            with self.subTest(titles=titles, since=since):
                sleep, dispatched = self.again(self.Gh(titles), since=since)
                self.assertEqual(dispatched, [["gh", "workflow", "run", "fleet-release-gate.yml", "--ref", "main",
                                               "-f", "generation=21", "-f", f"since={first}"]])
                self.assertEqual([c.args[0] for c in sleep.call_args_list], [release_gate.AGAIN_DELAY])
        # Two hours after the gate first saw the generation, only the schedule looks.
        for titles, since in (([title(21, now - 7200)], ""), ([], str(now - 7201))):
            with self.subTest(titles=titles, since=since):
                sleep, dispatched = self.again(self.Gh(titles), since=since)
                self.assertEqual(dispatched, [])
                sleep.assert_not_called()
        # No generation, or a history it cannot read: no dispatch (the bound cannot be checked).
        self.assertEqual(self.again(self.Gh(), generation="")[1], [])
        with self.assertRaisesRegex(SystemExit, "listing"):
            self.again(self.Gh(list_status=1))

    def test_the_workflow_looks_again_only_at_an_unverified_release_and_only_that_job_may_dispatch(self):
        on = WORKFLOW.split("\non:\n", 1)[1].split("\n\n", 1)[0]
        self.assertIn("  workflow_dispatch:\n    inputs:\n", on)
        self.assertIn("      generation:\n        type: string\n", on)
        self.assertIn("      since:\n        type: string\n", on)
        # The title is the history the bound reads (release_gate.py TITLE).
        name = re.search(r"^run-name: \"\$\{\{ inputs\.generation && format\('([^']+)', inputs\.generation, "
                         r"inputs\.since\) \|\| '([^']+)' \}\}\"$", WORKFLOW, re.M)
        self.assertIsNotNone(name)
        self.assertRegex(name.group(1).format(21, 1234), release_gate.TITLE)
        self.assertEqual(name.group(2), "Fleet image release gate")
        again = WORKFLOW.split("\n  again:\n", 1)[1]
        self.assertIn("needs: [status, smoke, decide]", again)
        for condition in ("!cancelled()", "needs.status.outputs.unverified == 'true'",
                          "needs.status.outputs.waiting == 'true'", "needs.decide.outputs.verdict == 'none'"):
            self.assertIn(condition, again)
        self.assertIn("    permissions:\n      actions: write\n", again)
        self.assertEqual(WORKFLOW.count("actions: write"), 1)
        self.assertIn("GH_TOKEN: ${{ github.token }}", again)
        self.assertIn('release_gate.py again --generation "$GENERATION" --since "$SINCE"', again)
        status = WORKFLOW.split("\n  status:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        for key in ("generation", "unverified", "waiting"):
            self.assertIn(f"{key}: ${{{{ steps.gate.outputs.{key} }}}}", status)
        decide = WORKFLOW.split("\n  decide:\n", 1)[1].split("\n  again:\n", 1)[0]
        self.assertIn("verdict: ${{ steps.decide.outputs.verdict }}", decide)
        self.assertIn("id: decide", decide)

    def test_the_smoke_installs_a_pinned_ffmpeg_not_from_apt(self):
        # Staging, 2026-11-18: apt on the hosted runner fetched ffmpeg at 75.7 kB/s for 13m50s
        # in the gate's first smoke of generation 19, between a ready release and its verdict.
        viewer = (ROOT / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        for job, script in (("ipv4", "fleet/coord/install-ffmpeg.sh"), ("ipv6", "./fcvm/fleet/coord/install-ffmpeg.sh")):
            body = viewer.split(f"\n  {job}:\n", 1)[1].split("\n  ipv6:\n" if job == "ipv4" else "\n  report:\n", 1)[0]
            self.assertNotIn("apt-get", body)
            install = f'run: {script} "$RUNNER_TEMP/ffmpeg" >> "$GITHUB_PATH"'
            self.assertIn(install, body)
            self.assertLess(body.index(install), body.index("stage-smoke.py --family"))

    def test_install_ffmpeg_refuses_a_download_that_does_not_match_its_checksum(self):
        script = ROOT / "fleet/coord/install-ffmpeg.sh"
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            # A download that is a working ffmpeg with an H.264 decoder, but not the pinned one.
            fake = tmp / "fake-ffmpeg"
            fake.write_text("#!/bin/sh\necho ' V....D h264                 H.264'\n")
            bin_ = tmp / "bin"
            bin_.mkdir()
            (bin_ / "curl").write_text('#!/bin/sh\nwhile [ "$1" != -o ]; do shift; done\n'
                                       f'gzip -c {fake} > "$2"\n')
            (bin_ / "uname").write_text("#!/bin/sh\necho \"$FAKE_ARCH\"\n")
            for name in ("curl", "uname"):
                (bin_ / name).chmod(0o755)
            for arch in ("x86_64", "aarch64", "riscv64"):
                with self.subTest(arch=arch):
                    out = tmp / arch
                    env = dict(os.environ, PATH=f"{bin_}:{os.environ['PATH']}", FAKE_ARCH=arch)
                    result = subprocess.run(["bash", str(script), str(out)], env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 1, result)
                    self.assertIn("pinned", result.stderr)
                    self.assertFalse((out / "ffmpeg").exists())
                    self.assertEqual(result.stdout, "")

    def test_the_gates_smoke_never_queues_behind_another_smoke(self):
        # Staging, 2026-11-18: generation 19's first gate smoke was ready at 07:59:36 and its v4
        # job waited 2m11s for an operator's standalone smoke holding the shared job group.
        smoke = WORKFLOW.split("\n  smoke:\n", 1)[1].split("\n  decide:\n", 1)[0]
        self.assertIn("    with:\n      relaunch: false\n      group: fleet-release-gate-smoke\n", smoke)
        viewer = (ROOT / ".github/workflows/fleet-viewer-smoke.yml").read_text()
        call = viewer.split("  workflow_call:\n", 1)[1].split("\n  schedule:", 1)[0]
        self.assertIn("      group:\n        type: string\n        default: fleet-viewer-smoke\n", call)
        for job in ("ipv4", "ipv6"):
            body = viewer.split(f"\n  {job}:\n", 1)[1].split("\n  ipv6:\n" if job == "ipv4" else "\n  report:\n", 1)[0]
            self.assertIn("    concurrency:\n      group: ${{ inputs.group || 'fleet-viewer-smoke' }}\n", body)
        # Every other caller, the schedule and a manual run keep sharing one group.
        for name in ("fleet-coordinator-deploy.yml", "fleet-web-deploy.yml"):
            call = (ROOT / ".github/workflows" / name).read_text().split(
                "uses: ./.github/workflows/fleet-viewer-smoke.yml", 1)[1]
            call = re.split(r"\n  \S", call, maxsplit=1)[0]
            self.assertIn("secrets: inherit", call)
            self.assertNotIn("group:", call, name)

    def test_the_status_run_is_cheap(self):
        # It runs every 5 minutes and mostly finds the live generation verified: a GitHub-hosted
        # runner, a short limit, and a checkout of the one file it runs.
        status = WORKFLOW.split("\n  status:\n", 1)[1].split("\n  smoke:\n", 1)[0]
        self.assertIn("runs-on: ubuntu-24.04", status)
        self.assertIn("timeout-minutes: 5", status)
        self.assertIn("sparse-checkout: fleet/ami/release_gate.py", status)
        self.assertIn("sparse-checkout-cone-mode: false", status)


if __name__ == "__main__":
    unittest.main()
