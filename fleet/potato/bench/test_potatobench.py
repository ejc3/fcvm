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

if __name__ == "__main__":
    unittest.main()
