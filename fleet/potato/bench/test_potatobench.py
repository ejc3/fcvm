"""Unit tests for potatobench.py's bookkeeping: python3 -m unittest fleet/potato/bench/test_potatobench.py"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import potatobench as pb  # noqa: E402

ZERO = "00" * 32


class Bookkeeping(unittest.TestCase):
    def test_every_cycle_has_its_own_round(self):
        # a player visited twice must not see the same (round, seq) and take it for a repeat
        balls = [pb.cycle_ball(i, "[a]:1", "[s]:2") for i in range(8)]
        self.assertEqual(len({(b["round"], b["seq"]) for b in balls}), 8)

    def test_a_catchs_hold_comes_from_its_seq(self):
        # 1000 ms shrinking a tenth a lap in a ring of 2: seq 2 opens lap 1
        self.assertEqual([pb.hold_at(1000, 100, 2, 2, s) for s in range(5)], [1000, 1000, 900, 900, 810])
        self.assertEqual(pb.hold_at(10, 500, 4, 2, 10), 4, "never below the floor")

    def test_a_catch_must_carry_the_stepped_chain(self):
        good = {"round": 1, "seq": 0, "colour": 4, "counter": 1, "chain": pb.chain_step(ZERO, 1, 0, 4)}
        self.assertTrue(pb.catch_ok(3, 0, ZERO, good))
        self.assertFalse(pb.catch_ok(3, 0, ZERO, dict(good, chain="11" * 32)), "the right colour and counter, a wrong chain")
        self.assertFalse(pb.catch_ok(3, 0, ZERO, dict(good, colour=5)))

    def test_an_answer_is_decoded(self):
        self.assertTrue(pb.reply_ok(b'{"type":"ok"}\n'))
        self.assertFalse(pb.reply_ok(b'{"type":"refused","reason":"ok"}'))
        self.assertFalse(pb.reply_ok(b"ok"))


    def test_a_round_that_ends_without_a_drop_keeps_its_record(self):
        # a ring of 2 at 1000 ms: catches 0 to 2 close lap 0, catches 0 to 3 close lap 1 (900 ms)
        def summary(seqs, after=(), failed=(), **done):
            def c(s):
                return {"type": "caught", "round": 1, "size": 2, "seq": s, "proof_ok": s not in failed}
            rows = [c(s) for s in seqs]
            rows.append({"type": "size_done", "size": 2, "start_hold_ms": 1000, **done})
            rows += [c(s) for s in after]
            with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as f:
                f.write("".join(json.dumps(r) + "\n" for r in rows))
            try:
                out = subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "ladder_summary.py"), f.name],
                                     check=True, capture_output=True, text=True).stdout
            finally:
                os.unlink(f.name)
            return json.loads(out)["record_hold_ms"]
        self.assertEqual(summary([0, 1, 2], reason="timeout"), 1000)
        self.assertEqual(summary([0, 1, 2, 3], reason="timeout"), 900)
        self.assertIsNone(summary([0], reason="timeout"), "no lap completed")
        self.assertEqual(summary([0, 1, 2, 3], reason="dropped", lap=2), 900, "a drop names its lap")
        self.assertEqual(summary([0, 1, 2], after=[3], reason="timeout"), 1000, "a catch after the round ended")
        self.assertEqual(summary([0, 1, 3, 2], failed=[2], reason="colour"), 1000, "seq 3 is past a failed catch")
        self.assertEqual(summary([0, 1, 3], reason="timeout"), 1000, "seq 3 is past a missing catch")

    def test_a_burst_compares_only_trials_whose_sleeps_are_its_own(self):
        # each trial sleeps the VMs the trial before woke: with two arms, trials 0 and 1 are
        # arm 0, 2 and 3 arm 1; trial 2 sleeps arm 0's VMs, so only 1 and 3 are compared
        self.assertEqual([pb.burst_arm(i, 2) for i in range(5)],
                         [(0, False), (0, True), (1, False), (1, True), (0, False)])

class Hello(unittest.TestCase):
    def test_a_player_whose_fcvm_exits_fails_at_once_with_its_status(self):
        # a player whose fcvm exits before it says hello (a refused preflight) must not be
        # waited for until the hello's timeout, which is 600 s for a boot
        import asyncio
        import time

        async def run(out):
            os.makedirs(os.path.join(out, "logs"))
            p = pb.Player(0, "2001:db8::/80", out)
            with open(p.logpath("run"), "w") as f:
                f.write("2026-01-01T00:00:00.000000Z ERROR fcvm: Error: routed mode preflight check failed\n")
            p.proc = await asyncio.create_subprocess_exec("sh", "-c", "exit 3")
            t = time.monotonic()
            with self.assertRaises(RuntimeError) as e:
                await p.hello(5)
            self.assertLess(time.monotonic() - t, 4)
            self.assertIn("status 3", str(e.exception))
            self.assertIn("preflight check failed", str(e.exception))

        with tempfile.TemporaryDirectory() as out:
            asyncio.run(run(out))

    def test_a_wake_whose_fcvm_exits_leaves_no_probe_running(self):
        import asyncio

        async def run(out):
            os.makedirs(os.path.join(out, "logs"))
            p = pb.Player(0, "2001:db8::/80", out)
            with self.assertRaises(RuntimeError):
                await p.wake(env=dict(os.environ, FCVM_BIN="false"))
            return [t for t in asyncio.all_tasks() if t is not asyncio.current_task() and not t.done()]

        with tempfile.TemporaryDirectory() as out:
            self.assertEqual(asyncio.run(run(out)), [])

    def test_a_hello_that_arrives_is_returned(self):
        import asyncio

        async def run(out):
            p = pb.Player(0, "2001:db8::/80", out)
            p.proc = await asyncio.create_subprocess_exec("sleep", "30")
            p.hellos.put_nowait((1.0, {"colour": 2}))
            try:
                self.assertEqual(await p.hello(5), (1.0, {"colour": 2}))
            finally:
                p.proc.kill()
                await p.proc.wait()

        with tempfile.TemporaryDirectory() as out:
            asyncio.run(run(out))


class LadderWakeFailure(unittest.TestCase):
    """A round whose first wakes fail ends at once, says why, and sends no ball to wait for."""

    def game(self):
        from types import SimpleNamespace
        a = SimpleNamespace(hold=100, shrink=100, min_hold=10, flight=100, round_secs=5, margin=150,
                            first_wake_ms=1500)
        return pb.Game(SimpleNamespace(players=[]), a, open(os.devnull, "w"))

    def test_a_failed_first_wake_puts_its_error_in_the_round_result(self):
        import asyncio
        from types import SimpleNamespace

        async def refuse(**_):
            raise RuntimeError("potato-0: fcvm exited with status 1 before it said hello")

        p = SimpleNamespace(k=0, phase="asleep", colour=0, counter=0, chain=ZERO, wake=refuse,
                            addr="2001:db8::1000", ball_addr=lambda: "[2001:db8::1000]:7070")
        res = asyncio.run(self.game().play([p], 1))
        self.assertEqual(res["reason"], "wake_failed")
        self.assertIn("status 1", res["error"])

    def test_a_round_that_sent_no_ball_does_not_wait_for_it_to_land(self):
        import asyncio
        import time

        async def run():
            g = self.game()
            g.round = {"id": 1, "n": 4, "hold": 100, "tasks": set(), "landed": asyncio.Event(), "ball_sent": False}
            t = time.monotonic()
            await asyncio.wait_for(g.quiesce([]), 5)
            return time.monotonic() - t

        self.assertLess(asyncio.run(run()), 2)


if __name__ == "__main__":
    unittest.main()
