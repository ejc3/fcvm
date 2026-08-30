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

Every numeric assertion is made against one parsed table row, never against the
flattened block. Checking membership and first-occurrence order over all the block's
numbers is satisfied by a table whose pairs have traded rows, so the report can
attribute both pairs to the wrong metrics with the suite green. Each row is compared
whole, label and both cells and direction together, and each row's direction word and
multiplier are recomputed from that row's own two values.
"""

import datetime
import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import report  # noqa: E402

BLOCK = report.KITESURF_CONTEXT

# Cloudflare's table, as published: one tuple per row, cell for cell. The row is
# the unit of truth here. A label divorced from its two values says nothing, so
# nothing below is asserted except against the row it was published on.
HEADER = ["metric", "Kitesurf", "Chromium (warm pool)", "Kitesurf, relative"]
PUBLISHED = [
    ("CPU, screenshot", "380 ms", "1,173 ms", "3.1x less CPU"),
    ("CPU, HTML extraction", "229 ms", "877 ms", "3.8x less CPU"),
    ("memory, screenshot", "57.8 MiB", "271.0 MiB", "4.7x less memory"),
    ("memory, HTML extraction", "39.4 MiB", "273.7 MiB", "7.0x less memory"),
    ("wall, screenshot", "1,148 ms", "637 ms", "1.8x slower"),
    ("wall, HTML extraction", "820 ms", "472 ms", "1.7x slower"),
]

# Cloudflare rounds its own multipliers loosely: 273.7 / 39.4 = 6.95 is published
# as "7.0x", the widest gap in the table at 0.053.
RATIO_TOLERANCE = 0.06


def numbers_in(text):
    """Every number in the text, commas stripped, as floats."""
    return [float(n.replace(",", "")) for n in re.findall(r"\d[\d,]*(?:\.\d+)?", text)]


def one_number_in(cell):
    """The single number a value cell carries, or None if it does not carry one."""
    nums = numbers_in(cell)
    return nums[0] if len(nums) == 1 else None


def table_rows(text):
    """The Markdown table's rows, each a list of stripped cells, separators dropped."""
    rows = []
    for line in text.splitlines():
        line = line.strip()
        if not (line.startswith("|") and line.endswith("|")):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if all(set(c) <= set("-: ") and c for c in cells):
            continue
        rows.append(cells)
    return rows


ROWS = table_rows(BLOCK)

# The day the six rows above were last read off Cloudflare's page.
VERIFIED = "2026-08-30"


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

    def test_the_table_is_the_six_published_rows_under_the_published_headers(self):
        """Column order is what binds a value to an engine, so pin the header."""
        self.assertTrue(ROWS, "no Markdown table found in the comparator block")
        self.assertEqual(ROWS[0], HEADER, "comparator table headers changed")
        self.assertEqual(
            [r[0] for r in ROWS[1:]],
            [row[0] for row in PUBLISHED],
            "comparator table does not carry the six published rows, in order",
        )

    def test_each_row_binds_its_metric_to_both_values_and_its_direction(self):
        """A row is asserted whole: label, Kitesurf cell, Chromium cell, direction.

        Checking the numbers against the flattened block instead would let a pair
        move to another metric's row, or two pairs trade rows, with every value
        still present and every comparison still true. The report would then
        attribute the numbers to the wrong metrics and pass.
        """
        by_label = {}
        for row in ROWS[1:]:
            by_label.setdefault(row[0], []).append(row)
        for expected in PUBLISHED:
            label = expected[0]
            with self.subTest(metric=label):
                rows = by_label.get(label, [])
                self.assertEqual(len(rows), 1, f"{label}: expected exactly one row")
                self.assertEqual(
                    tuple(rows[0]), expected, f"{label}: row does not match the source"
                )

    def test_each_rows_direction_word_agrees_with_that_rows_own_two_values(self):
        """The direction is read off the row that states it, not off a constant."""
        for row in ROWS[1:]:
            label, kitesurf_cell, chromium_cell, relative = row
            with self.subTest(metric=label):
                kitesurf = one_number_in(kitesurf_cell)
                chromium = one_number_in(chromium_cell)
                self.assertIsNotNone(kitesurf, f"{label}: Kitesurf cell has no value")
                self.assertIsNotNone(chromium, f"{label}: Chromium cell has no value")
                if "less" in relative:
                    self.assertLess(
                        kitesurf,
                        chromium,
                        f"{label}: row says {relative!r} while Kitesurf spends more",
                    )
                elif "slower" in relative:
                    self.assertGreater(
                        kitesurf,
                        chromium,
                        f"{label}: row says {relative!r} while Kitesurf spends less",
                    )
                else:
                    self.fail(f"{label}: {relative!r} states no direction")

    def test_each_rows_multiplier_matches_that_rows_own_two_values(self):
        """The stated Nx must be the ratio of the two cells beside it."""
        for row in ROWS[1:]:
            label, kitesurf_cell, chromium_cell, relative = row
            with self.subTest(metric=label):
                kitesurf = one_number_in(kitesurf_cell)
                chromium = one_number_in(chromium_cell)
                self.assertIsNotNone(kitesurf, f"{label}: Kitesurf cell has no value")
                self.assertIsNotNone(chromium, f"{label}: Chromium cell has no value")
                m = re.match(r"([\d.]+)x\b", relative)
                self.assertIsNotNone(m, f"{label}: {relative!r} states no multiplier")
                stated = float(m.group(1))
                actual = max(kitesurf, chromium) / min(kitesurf, chromium)
                self.assertLessEqual(
                    abs(stated - actual),
                    RATIO_TOLERANCE,
                    f"{label}: row claims {stated}x but its own cells "
                    f"({kitesurf_cell}, {chromium_cell}) are {actual:.2f}x apart",
                )

    def test_both_workloads_are_present(self):
        """The HTML extraction row is the second data point; screenshots alone is one."""
        self.assertRegex(BLOCK, r"(?i)screenshot")
        self.assertRegex(BLOCK, r"(?i)HTML extraction")

    def test_records_the_source_and_the_verification_date(self):
        """The date is pinned, and the block reads it from the same constant.

        A date-shaped regex accepts any year and impossible calendar days, so it
        cannot tell a real re-check from a typo.
        """
        self.assertIn("developers.cloudflare.com/browser-run/kitesurf", BLOCK)
        self.assertEqual(
            report.KITESURF_VERIFIED,
            VERIFIED,
            "the recorded verification date moved; re-check the source, then pin it here",
        )
        try:
            datetime.date.fromisoformat(report.KITESURF_VERIFIED)
        except ValueError as exc:
            self.fail(f"verification date is not a calendar date: {exc}")
        self.assertIn(
            f"verified on {report.KITESURF_VERIFIED} against",
            BLOCK,
            "the block's prose and KITESURF_VERIFIED have drifted apart",
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
