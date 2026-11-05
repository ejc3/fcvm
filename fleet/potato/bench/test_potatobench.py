"""Unit tests for potatobench.py's bookkeeping: python3 -m unittest fleet/potato/bench/test_potatobench.py"""
import os
import sys
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


if __name__ == "__main__":
    unittest.main()
