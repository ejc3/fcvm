#!/usr/bin/env python3
"""renders_agent_check.py without a host: its messages against the ones the Rust side
serialises and parses (renders_agent_protocol.jsonl, pinned by the agent's
`the_host_checks_recorded_messages_are_this_builds`), its reading of an agent's output
against a stand-in agent process, its arguments, and renders_bench.sh's agent command line.
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import renders_agent_check as check  # noqa: E402

with open(os.path.join(HERE, "renders_agent_protocol.jsonl")) as handle:
    RECORDED = [json.loads(line) for line in handle if line.strip()]
HELLO, REPORT, ORDERS = RECORDED[0], RECORDED[1], RECORDED[2:]

STAND_IN = textwrap.dedent('''
    import json, sys
    hello, report, early = sys.argv[1], sys.argv[2], sys.argv[3] == "early"
    print(hello, flush=True)
    if early:
        print(report, flush=True)
    for line in sys.stdin:
        msg = json.loads(line)
        if msg.get("type") == "renders":
            print(report, flush=True)
''')


class Messages(unittest.TestCase):
    def test_the_orders_are_the_ones_the_agent_parses(self):
        self.assertEqual(json.loads(check.order(True)), ORDERS[0])
        self.assertEqual(json.loads(check.order(True, check.OVERLAY)), ORDERS[1])
        self.assertEqual(json.loads(check.order(False)), ORDERS[2])

    def test_the_agents_hello_is_read_as_it_is_sent(self):
        self.assertEqual(check.hello_problems(HELLO, 2), [])
        self.assertIn("1 pages, not 2", " ".join(check.hello_problems(
            {**HELLO, "renders": {**HELLO["renders"], "pages": HELLO["renders"]["pages"][:1]}}, 2)))
        self.assertEqual(check.hello_problems({**HELLO, "renders": None}, 2), ["the hello carries no renders"])
        no_kernel = {**HELLO, "renders": {**HELLO["renders"], "build": {"viewport": "1280x720"}}}
        self.assertIn("kernel", " ".join(check.hello_problems(no_kernel, 2)))
        self.assertTrue(check.hello_problems(REPORT, 2))

    def test_a_report_with_renders_on_each_side(self):
        self.assertTrue(check.is_report(REPORT))
        self.assertFalse(check.is_report(HELLO))
        self.assertTrue(check.rendered(REPORT, ["fcvm", "kitesurf"]))
        none = json.loads(json.dumps(REPORT))
        none["sides"]["kitesurf"]["done"] = 0
        self.assertFalse(check.rendered(none, ["fcvm", "kitesurf"]))
        self.assertTrue(check.rendered(none, ["fcvm"]))
        self.assertFalse(check.rendered({"type": "renders", "at": 1}, ["fcvm"]))

    def test_the_injected_files(self):
        band = '<div id="fcvm-overlay">agent check</div>'
        style = "<style>html{filter:invert(1)}</style>"
        self.assertEqual(check.inject_problems(style + band, band), [])
        self.assertEqual(check.inject_problems(band, band), ["the fcvm side's file holds no transform"])
        self.assertEqual(check.inject_problems(style + band, style + band),
                         ["the kitesurf side's file holds the Chromium-only transform"])
        self.assertEqual(len(check.inject_problems("", "")), 3)


class StandInAgent(unittest.TestCase):
    def agent(self, early=False):
        command = [sys.executable, "-c", STAND_IN, json.dumps(HELLO), json.dumps(REPORT),
                   "early" if early else "late"]
        agent = check.Agent(command, subprocess.DEVNULL)
        self.addCleanup(lambda: agent.close(5) or agent.proc.kill())
        return agent

    def test_no_report_before_the_first_order_and_one_after(self):
        agent = self.agent()
        self.assertEqual(agent.next(lambda m: True, 10, "the hello"), HELLO)
        self.assertIsNone(agent.quiet(check.is_report, 0.3))
        agent.send(check.order(True))
        self.assertEqual(agent.next(check.is_report, 10, "a report"), REPORT)
        self.assertTrue(agent.alive())
        self.assertTrue(agent.close(10))
        self.assertFalse(agent.alive())

    def test_an_early_report_is_caught(self):
        agent = self.agent(early=True)
        agent.next(lambda m: True, 10, "the hello")
        self.assertEqual(agent.quiet(check.is_report, 5), REPORT)

    def test_drained_messages_are_not_seen_by_a_later_wait(self):
        agent = self.agent(early=True)
        agent.next(lambda m: True, 10, "the hello")
        agent.next(check.is_report, 10, "the early report")
        agent.send(check.order(True))
        agent.next(check.is_report, 10, "a report")
        agent.drain()
        with self.assertRaises(TimeoutError):
            agent.next(check.is_report, 0.3, "nothing")

    def test_an_agent_that_exits_ends_a_wait(self):
        agent = check.Agent([sys.executable, "-c", "print('{\"type\": \"hello\"}')"], subprocess.DEVNULL)
        self.addCleanup(agent.close, 5)
        agent.next(lambda m: True, 10, "the hello")
        with self.assertRaises(RuntimeError):
            agent.next(check.is_report, 10, "a report")


class Arguments(unittest.TestCase):
    BASE = ["--unit", "u", "--status", "s", "--teardown", "t", "--run-dir", "r", "--pages", "14"]

    def test_the_command_follows_the_double_dash(self):
        opts = check.parse(self.BASE + ["--", "sudo", "systemd-run", "--pipe", "fcvm"])
        self.assertEqual(opts.command, ["sudo", "systemd-run", "--pipe", "fcvm"])
        self.assertEqual(opts.cases, "running,held,kill")
        self.assertIn("running:TERM", opts.stages)
        self.assertFalse(opts.engines)

    def test_no_command_or_an_unknown_case_is_an_error(self):
        with open(os.devnull, "w") as null:
            stderr, sys.stderr = sys.stderr, null
            try:
                with self.assertRaises(SystemExit):
                    check.parse(self.BASE)
                with self.assertRaises(SystemExit):
                    check.parse(self.BASE + ["--cases", "running,golden", "--", "x"])
            finally:
                sys.stderr = stderr


class Launcher(unittest.TestCase):
    """renders_bench.sh's agent command line: the module's options, each behind
    `--renders-arg=`, beside the agent's own harmless ones."""

    def functions(self):
        with open(os.path.join(HERE, "renders_bench.sh")) as handle:
            text = handle.read()
        picked = []
        for name in ("module_opts", "agent_args"):
            start = text.index(f"\n{name}() {{")
            picked.append(text[start:text.index("\n}\n", start) + 3])
        return "".join(picked)

    def test_the_agent_gets_the_modules_options(self):
        result = subprocess.run(["bash", "-n", os.path.join(HERE, "renders_bench.sh")])
        self.assertEqual(result.returncode, 0)
        with tempfile.TemporaryDirectory() as tmp:
            env = {**os.environ, "OUT": f"{tmp}/out", "RUN_DIR": f"{tmp}/run", "STATUS": f"{tmp}/s.json",
                   "HERE": HERE, "CORPUS": f"{tmp}/corpus", "URLS": "https://a.test/,https://b.test/",
                   "TAG": "t", "MAX_ENGINES": "36", "LEVELS": "16,36", "SECONDS_RUN": "60"}
            script = self.functions() + "\nagent_args\n"
            out = subprocess.run(["bash", "-c", script], env=env, capture_output=True, check=True).stdout
        args = out.decode().split("\0")[:-1]
        self.assertEqual(args[:3], ["fleet", "agent", "--stdio"])
        self.assertIn("--renders", args)
        renders = [a[len("--renders-arg="):] for a in args if a.startswith("--renders-arg=")]
        self.assertEqual(renders[:2], ["--out", f"{tmp}/out"])
        self.assertIn("https://a.test/,https://b.test/", renders)
        self.assertIn("--max-engines", renders)
        # the benchmark's own options are not the agent's
        self.assertNotIn("--levels", renders)
        self.assertNotIn("--seconds", renders)
        own = [a for a in args if not a.startswith("--renders-arg=")]
        for flag in ("--host", "--image", "--base-port", "--sysfs-root"):
            self.assertIn(flag, own)


if __name__ == "__main__":
    unittest.main()
