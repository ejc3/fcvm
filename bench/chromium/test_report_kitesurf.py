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

import argparse
import datetime
import json
import os
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import report  # noqa: E402

# bench.sh's own schedule: three host-served fixtures over http plus one over
# https. Every standard `make bench-chromium` report is written for these.
FIXTURE_PAGES = ["heavy", "medium", "medium-https", "minimal"]
CORPUS_PAGES = sorted(report.KITESURF_CORPUS)

FIXTURE_BLOCK = report.kitesurf_context(FIXTURE_PAGES)
CORPUS_BLOCK = report.kitesurf_context(CORPUS_PAGES)

# The six quoted rows, the summary and the source are the same whichever
# workload the run measured, so the assertions below hold against either block
# and BLOCK stands for both. WorkloadComparability asserts what differs.
BLOCK = FIXTURE_BLOCK

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

    def test_names_the_corpus_the_quoted_rows_were_measured_over(self):
        """Whatever this run rendered, the reader is told what THEY rendered."""
        for name, block in (("fixture", FIXTURE_BLOCK), ("corpus", CORPUS_BLOCK)):
            with self.subTest(workload=name):
                self.assertIn("corpus_mirror.sh", block)
                self.assertIn("kitesurf.cloudflare.app/corpus.txt", block)

    def test_the_quoted_rows_do_not_depend_on_what_this_run_rendered(self):
        """Only the workload paragraph varies; the six rows are theirs either way.

        Every other assertion in this class is made against one block, so this
        is what entitles them to speak for both.
        """
        self.assertEqual(table_rows(FIXTURE_BLOCK), table_rows(CORPUS_BLOCK))
        for shared in ("Their summary:", f"verified on {report.KITESURF_VERIFIED}",
                       "Not comparable: everything else."):
            with self.subTest(text=shared):
                self.assertIn(shared, FIXTURE_BLOCK)
                self.assertIn(shared, CORPUS_BLOCK)

    def test_the_corpus_is_the_one_corpus_mirror_actually_mirrors(self):
        """The claim is that OUR mirror holds THEIR pages, so the two lists must agree.

        report.KITESURF_CORPUS decides which run is corpus-backed and
        corpus_mirror.sh decides which pages exist to render. If they drift, the
        block credits a run with a corpus it did not render.
        """
        script = open(os.path.join(HERE, "corpus_mirror.sh")).read()
        block = re.search(r"^URLS=\(\n(.*?)^\)$", script, re.M | re.S)
        self.assertIsNotNone(block, "corpus_mirror.sh no longer declares a URLS array")
        mirrored = re.findall(r'"([^"]+)"', block.group(1))
        self.assertEqual(
            sorted(mirrored),
            sorted(report.KITESURF_CORPUS.values()),
            "report.KITESURF_CORPUS and corpus_mirror.sh name different pages",
        )

    def test_does_not_claim_hardware_the_source_never_states(self):
        """Neither the docs page nor the blog post names the measurement hardware."""
        self.assertNotRegex(
            BLOCK,
            r"(?i)\b(?:EPYC|Xeon|AMD|Intel)\b",
            "comparator attributes hardware to Cloudflare that their page never states",
        )


class WorkloadComparability(unittest.TestCase):
    """The shared-corpus claim belongs to a run that rendered the shared corpus.

    `report.py finalize` writes one report per run, and bench.sh's own schedule
    renders three host-served fixtures (minimal.html, medium.html, heavy.html)
    and never the corpus. A block that states the workload is comparable no
    matter what the run rendered puts a false claim in every standard
    `make bench-chromium` report, which is the same defect as the inverted wall
    row: prose that contradicts the records beneath it.

    Both directions are asserted here, through `cmd_finalize` rather than
    against the block alone, because the defect was in the wiring: the block was
    correct for a corpus run and appended unconditionally.
    """

    def finalize(self, pages, render_ok=True):
        """report.md for a run whose request records name exactly `pages`."""
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        os.makedirs(os.path.join(d, "requests"))
        os.makedirs(os.path.join(d, "samples"))
        with open(os.path.join(d, "hostinfo.json"), "w") as f:
            json.dump({"uname": "Linux test", "cpu_model": "Neoverse-V1", "nproc": 64,
                       "mem_total_kb": 128 * 1024 * 1024, "vm_cpu": 2,
                       "vm_mem_mib": 2048, "reps": 1,
                       "contention_note": "synthetic fixture"}, f)
        with open(os.path.join(d, "availability.json"), "w") as f:
            json.dump({"modes": {"rootless-proxy": {"available": "yes", "reason": ""}},
                       "hugepages": {"available": "no", "reason": "off", "pool_pages": 0},
                       "file_huge_cell": "skipped"}, f)
        # Every run also carries the two orchestration controls, which render no
        # page; a corpus run must stay corpus-backed with them present.
        for label in list(pages) + ["noop", "noopsh"]:
            name = f"p3__rootless-proxy__uffd-4k__{label}__r1.log"
            outcome = "RENDER_OK" if render_ok else "RENDER_FAIL nav timeout"
            with open(os.path.join(d, "requests", name), "w") as f:
                f.write(f"1000.0 BENCH_T0\n1001.0 {outcome}\n1002.0 BENCH_EXIT rc=0\n")
        report.cmd_finalize(argparse.Namespace(results_dir=d))
        with open(os.path.join(d, "report.md")) as f:
            md = f.read()
        # The comparator block alone: the assertions are about what it claims,
        # and the surrounding tables would bury the failure message.
        head = md.index("### Published comparator")
        tail = md.index("\n## ", head)
        return md[head:tail]

    def test_a_fixture_run_does_not_claim_the_workload_is_comparable(self):
        """bench.sh renders its own fixtures, so its report may not claim a shared corpus."""
        md = self.finalize(["minimal", "medium", "heavy", "medium-https"])
        self.assertIn(
            "Not comparable: the workload",
            md,
            "a report over host-served fixtures does not say its workload is "
            "incomparable to the corpus the quoted rows were measured over",
        )
        self.assertNotIn(
            "same page list",
            md,
            "a report over host-served fixtures claims both sides rendered the "
            "same page list",
        )
        for page in ("minimal", "medium", "heavy"):
            self.assertIn(
                page, md, f"the block does not name {page}, the page this run rendered"
            )

    def test_a_corpus_run_states_the_shared_corpus(self):
        """The corpus point is the strongest half of the comparison where it holds."""
        md = self.finalize(CORPUS_PAGES)
        self.assertIn(
            "Comparable: the workload",
            md,
            "a report over the mirrored corpus does not claim the workload it shares",
        )
        self.assertIn(
            "same page list",
            md,
            "a report over the mirrored corpus does not state that both sides "
            "rendered the same page list",
        )
        self.assertRegex(
            md,
            r"(?i)this run's page list is the 14-URL corpus",
            "the shared-corpus claim is not bound to what this run rendered, so "
            "it says nothing about the report it sits in",
        )
        self.assertNotIn("Not comparable: the workload", md)

    def test_a_page_whose_renders_all_failed_is_not_called_rendered(self):
        """A page a run scheduled is part of its workload whether the render
        succeeded or not, and the failures section below already counts what
        failed. What the block may not do is tell the reader the run RENDERED a
        page whose every record is a RENDER_FAIL: that is the same shape of
        false statement as the workload claim this class exists for.
        """
        md = self.finalize(["minimal", "medium", "heavy"], render_ok=False)
        self.assertNotRegex(
            md,
            r"(?i)\bthis run rendered\b",
            "the block credits the run with rendering pages its own records show failing",
        )
        self.assertIn(
            "heavy, medium, minimal",
            md,
            "a failed render does not take a page out of the run's page list",
        )

    def test_a_corpus_run_with_a_failed_page_still_states_the_shared_corpus(self):
        """What makes the workloads comparable is the page list, not the success rate.

        Dropping a page whose renders failed would take a 14-URL corpus run down
        to 13 pages and silently withdraw a claim that still holds.
        """
        md = self.finalize(CORPUS_PAGES, render_ok=False)
        self.assertIn(
            "same page list",
            md,
            "a failed render withdrew a shared-corpus claim that still holds",
        )

    def test_a_run_whose_records_name_no_page_claims_nothing(self):
        """A results dir with no request record is not a corpus run either."""
        md = self.finalize([])
        self.assertIn("Not comparable: the workload", md)
        self.assertIn("name no page at all", md)
        self.assertNotIn("same page list", md)

    def test_a_run_whose_records_name_no_page_claims_nothing(self):
        """A results dir with no request record is not a corpus run either."""
        md = self.finalize([])
        self.assertIn("Not comparable: the workload", md)
        self.assertIn("name no page at all", md)
        self.assertNotIn("same page list", md)


if __name__ == "__main__":
    unittest.main()
