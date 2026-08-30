#!/usr/bin/env python3
"""The published-comparator block must state the Kitesurf trade in the right direction.

Cloudflare publishes six benchmark rows for Kitesurf against a warm Chromium pool
(developers.cloudflare.com/browser-run/kitesurf/, verified 2026-08-30, same table at
blog.cloudflare.com/kitesurf/). Kitesurf uses 3.1-3.8x less CPU and 4.7-7.0x less
memory, and is 1.7-1.8x SLOWER on wall time. An earlier revision of KITESURF_CONTEXT
claimed a "1.7-1.8x wall-clock advantage" for the lighter engine, inverting the one
axis this report also measures.

The block is prose, so these assertions pin what a prose defect actually inverts: the
per-row numbers bound to the engine they belong to, and the direction words that
readers take away. A column swap or a re-inverted summary fails here.
"""

import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import report  # noqa: E402

BLOCK = report.KITESURF_CONTEXT

# Cloudflare's table, as published. (metric, kitesurf, chromium, kitesurf_is_lower)
PUBLISHED = [
    ("CPU screenshot", 380.0, 1173.0, True),
    ("CPU HTML extraction", 229.0, 877.0, True),
    ("memory screenshot", 57.8, 271.0, True),
    ("memory HTML extraction", 39.4, 273.7, True),
    ("wall screenshot", 1148.0, 637.0, False),
    ("wall HTML extraction", 820.0, 472.0, False),
]


def numbers_in(text):
    """Every number in the text, commas stripped, as floats."""
    return [float(n.replace(",", "")) for n in re.findall(r"\d[\d,]*(?:\.\d+)?", text)]


class ComparatorDirection(unittest.TestCase):
    def test_no_wall_clock_advantage_for_the_lighter_engine(self):
        """Kitesurf must never be credited with a wall-time win. This is the defect."""
        forbidden = [
            r"wall[- ]clock advantage",
            r"kitesurf[^.]*\b(?:faster|advantage)\b",
            r"\b(?:faster|advantage)\b[^.]*lighter engine",
        ]
        for pat in forbidden:
            with self.subTest(pattern=pat):
                m = re.search(pat, BLOCK, re.I | re.S)
                self.assertIsNone(
                    m,
                    f"comparator credits the lighter engine with a wall-time win: "
                    f"{m.group(0)!r}" if m else "",
                )

    def test_names_both_the_cpu_and_the_wall_direction(self):
        """Both directions must be stated, not just the flattering one."""
        self.assertRegex(
            BLOCK, r"(?i)less CPU", "comparator does not name the CPU direction"
        )
        self.assertRegex(
            BLOCK, r"(?i)slower", "comparator does not name the wall-time direction"
        )
        self.assertRegex(
            BLOCK,
            r"(?i)chromium[^.]*\b(?:wins|faster)\b",
            "comparator does not say Chromium is the faster of the two on wall clock",
        )

    def test_every_published_row_is_quoted_with_its_engines_in_order(self):
        """Each row's pair must appear with the numbers bound to the right engine.

        Catches a column swap, which is the numeric form of the same inversion.
        """
        nums = numbers_in(BLOCK)
        for metric, kitesurf, chromium, kitesurf_lower in PUBLISHED:
            with self.subTest(metric=metric):
                self.assertIn(kitesurf, nums, f"{metric}: Kitesurf value missing")
                self.assertIn(chromium, nums, f"{metric}: Chromium value missing")
                i, j = nums.index(kitesurf), nums.index(chromium)
                self.assertLess(i, j, f"{metric}: engines quoted out of column order")
                self.assertEqual(
                    kitesurf < chromium,
                    kitesurf_lower,
                    f"{metric}: published direction contradicted",
                )

    def test_both_workloads_are_present(self):
        """The HTML extraction row is the second data point; screenshots alone is one."""
        self.assertRegex(BLOCK, r"(?i)screenshot")
        self.assertRegex(BLOCK, r"(?i)HTML extraction")

    def test_records_the_source_and_the_verification_date(self):
        self.assertIn("developers.cloudflare.com/browser-run/kitesurf", BLOCK)
        self.assertRegex(
            BLOCK, r"20\d\d-\d\d-\d\d", "no verification date recorded for the quote"
        )

    def test_states_the_corpus_is_shared(self):
        """corpus_mirror.sh mirrors their public corpus, so the workload axis is close."""
        self.assertIn("corpus_mirror.sh", BLOCK)
        self.assertIn("kitesurf.cloudflare.app/corpus.txt", BLOCK)

    def test_does_not_claim_hardware_the_source_never_states(self):
        """Neither the docs page nor the blog post names the measurement hardware."""
        self.assertNotRegex(
            BLOCK,
            r"(?i)\b(?:EPYC|Xeon|AMD|Intel)\b",
            "comparator attributes hardware to Cloudflare that their page never states",
        )


if __name__ == "__main__":
    unittest.main()
