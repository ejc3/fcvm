#!/usr/bin/env python3
"""The benchmark report is published by GitHub Pages from its fragment source.

report/shared-nothing-renders.html carries no document skeleton; report/wrap_page.py
adds one, and .github/workflows/pages.yml runs it at deploy time. These tests pin
both halves, so the published page cannot silently stop tracking the source.
"""

import html as html_mod
import json
import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
REPORT = os.path.join(HERE, "report", "shared-nothing-renders.html")
PAGES_WORKFLOW = os.path.join(REPO, ".github", "workflows", "pages.yml")

sys.path.insert(0, os.path.join(HERE, "report"))
sys.path.insert(0, HERE)

import wrap_page  # noqa: E402
from test_report_kitesurf import PUBLISHED  # noqa: E402


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


class WrappedReport(unittest.TestCase):
    def test_the_fragment_becomes_one_complete_document(self):
        """The wrapper adds only the document skeleton around the fragment."""
        fragment = read(REPORT)
        page = wrap_page.wrap(fragment)
        self.assertTrue(page.startswith("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n"))
        self.assertTrue(page.endswith("</footer>\n</body>\n</html>\n"))
        head = page[:page.index("</head>")]
        body = page[page.index("<body>"):]
        self.assertEqual(
            re.findall(r"<(title|style|main|meta)\b", head),
            ["meta", "meta", "title", "style"],
            "the head holds the fragment's title and style and nothing else",
        )
        self.assertEqual(body.count("<main>"), 1)
        self.assertNotIn("<style>", body)
        # The SVG charts carry their own <title> children; the document title
        # is the one in the head.
        self.assertEqual(re.findall(r"<title>[^<]*</title>", head),
                         ["<title>One microVM per page: Chromium on Firecracker snapshots</title>"])
        # Match whole tag names: the page body has a <header> element.
        for tag in ("!DOCTYPE", "html", "head", "body"):
            self.assertEqual(len(re.findall(rf"<{tag}[\s>]", page)), 1, tag)
        # Removing the skeleton the wrapper adds gives back the fragment,
        # so nothing else was added, dropped or reordered.
        stripped = (page.removeprefix(wrap_page.PREAMBLE)
                    .replace("</head>\n<body>\n", "", 1)
                    .removesuffix("</body>\n</html>\n"))
        self.assertEqual(stripped, fragment.rstrip("\n") + "\n")

    def test_the_header_and_footer_are_page_landmarks(self):
        """RED BEFORE THE FIX: wrap_page split the fragment at <main>, so the
        breadcrumb header and the source footer sat inside <main>. The page had
        no banner or contentinfo landmark, and a screen reader announced both as
        part of the article."""
        page = wrap_page.wrap(read(REPORT))
        body = page[page.index("<body>"):]
        header = body.index('<header class="site-header">')
        main_open, main_close = body.index("<main>"), body.index("</main>")
        footer = body.index('<footer class="site-footer">')
        self.assertLess(header, main_open, "the header must precede <main>")
        self.assertLess(main_close, footer, "the footer must follow </main>")

    def test_a_fragment_without_main_is_refused(self):
        """There is nothing to put in <body> without a <main>."""
        with self.assertRaisesRegex(ValueError, "no <main>"):
            wrap_page.wrap("<title>x</title>\n<style></style>\n<p>no main</p>\n")

    def test_the_pages_workflow_publishes_the_wrapped_report(self):
        """RED BEFORE THE FIX: pages.yml uploaded docs/ alone and triggered
        only on docs/**, so the report was never on the site and an edit to
        the source did not redeploy."""
        workflow = read(PAGES_WORKFLOW)
        self.assertIn("bench/chromium/report/wrap_page.py", workflow)
        self.assertIn("bench/chromium/report/shared-nothing-renders.html", workflow)
        paths = workflow[workflow.index("paths:"):workflow.index("workflow_dispatch")]
        for trigger in ("'docs/**'",
                        "'bench/chromium/report/shared-nothing-renders.html'",
                        "'bench/chromium/report/wrap_page.py'"):
            self.assertIn(trigger, paths, f"a change to {trigger} must redeploy")
        self.assertRegex(
            workflow, r"> *_site/shared-nothing-renders\.html",
            "the wrapped report is written into the uploaded site directory",
        )
        self.assertRegex(workflow, r"path: *_site\b", "the site directory is what is uploaded")

    def test_the_deliverable_contract_names_github_pages(self):
        """Codex on #1012: AGENTS.md required publishing through the Artifact tool,
        which this PR retires. The contract names the Pages path instead.

        RED BEFORE THE FIX: deliverable 6 read "Publish an artifact (HTML via the
        Artifact tool)" and named no Pages URL."""
        agents = read(os.path.join(HERE, "AGENTS.md"))
        start = agents.index("6. **Publish")
        deliverable = agents[start:agents.index("7. **", start)]
        self.assertIn("https://ejc3.github.io/fcvm/shared-nothing-renders.html", deliverable)
        self.assertIn("report/wrap_page.py", deliverable)
        self.assertNotIn("Artifact tool", deliverable)

    def test_the_docs_index_links_the_report(self):
        """The published page is reachable from the docs index."""
        index = read(os.path.join(REPO, "docs", "index.html"))
        self.assertIn('href="shared-nothing-renders.html"', index)


def plain(fragment):
    """Text of an HTML fragment with tags dropped and whitespace collapsed."""
    text = html_mod.unescape(re.sub(r"<[^>]+>", " ", fragment)).replace("\u00a0", " ")
    return re.sub(r"\s+", " ", text).strip()


def section(page, anchor):
    """The HTML from <h2 id=anchor> up to the next <h2."""
    start = page.index(f'<h2 id="{anchor}">')
    end = page.find("<h2", start + 1)
    return page[start:end if end != -1 else len(page)]


def css_block(css, selector):
    """The declarations of the first rule whose selector is exactly `selector`."""
    m = re.search(r"(?m)^\s*" + re.escape(selector) + r"\s*\{([^}]*)\}", css)
    if not m:
        raise AssertionError(f"no CSS rule for {selector!r}")
    return m.group(1)


def luminance(hex_colour):
    """WCAG relative luminance of #rgb or #rrggbb."""
    h = hex_colour.lstrip("#")
    if len(h) == 3:
        h = "".join(c * 2 for c in h)
    channels = [int(h[i:i + 2], 16) / 255 for i in (0, 2, 4)]
    lin = [c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4 for c in channels]
    return 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]


def contrast(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


class ReportContent(unittest.TestCase):
    """What the page states about its sources, checked against those sources."""

    def setUp(self):
        self.page = read(REPORT)

    def test_every_published_cloudflare_row_is_quoted_cell_for_cell(self):
        """RED BEFORE THE FIX: the table titled "Every row Cloudflare published"
        printed the CPU rows as "380 core-ms" where Cloudflare prints "380 ms",
        and dropped the published relative column (3.1x less CPU ... 1.8x
        slower), which is where their CPU and memory against wall-time trade is
        stated."""
        rows = []
        for tr in re.findall(r"<tr>(.*?)</tr>", self.page, re.S):
            cells = dict(re.findall(r'<td data-label="([^"]+)">(.*?)</td>', tr, re.S))
            if "Kitesurf, relative" in cells or "Kitesurf" in cells:
                rows.append({k: plain(v) for k, v in cells.items()})
        for metric, kite, chrom, rel in PUBLISHED:
            want = {"Metric": metric.lower(), "Kitesurf": kite, "Cloudflare Chromium": chrom,
                    "Kitesurf, relative": rel}
            got = [{k: (r.get(k, "").lower() if k == "Metric" else r.get(k)) for k in want} for r in rows]
            self.assertIn(want, got, f"Cloudflare's row {metric!r} is not quoted cell for cell")

    def test_the_zero_failure_count_carries_its_interval(self):
        """RED BEFORE THE FIX: the headline tile said "0 failed requests in the
        four published corpus runs" and the sample-gate bullet "Zero failures in
        every published cell" with no interval. REVIEW.md requires the interval
        whenever the count supports a reliability claim; each corpus run's
        failure_rate_ci is [0, 0.0159] over 230 attempts."""
        runs = sorted(set(re.findall(r"results/(reqbench-[0-9-]+-corpus[a-z0-9-]*)", self.page)))
        self.assertTrue(runs, "the page cites no corpus run; the check is vacuous")
        bounds = set()
        for run in runs:
            with open(os.path.join(HERE, "results", run, "analysis.json")) as f:
                arm = json.load(f)["arms"]["cdp"]
            self.assertEqual(arm["failed"], 0, run)
            bounds.add(f"{arm['failure_rate_ci'][1] * 100:.2f}%")
        self.assertEqual(len(bounds), 1, f"the runs' intervals differ: {bounds}; quote each")
        bound = bounds.pop()
        units = re.findall(r"<(?:li|p|td)\b[^>]*>(.*?)</(?:li|p|td)>", self.page, re.S)
        units += re.findall(r'<div><span class="num">(.*?)</div>', self.page, re.S)
        claim = re.compile(r"(?i)\b(?:0|zero) (?:failures?|failed)\b")
        checked = 0
        for unit in units:
            text = plain(unit)
            if claim.search(text) and "corpus" in text:
                checked += 1
                self.assertIn(bound, text, f"a zero-failure claim without its interval: {text[:160]!r}")
        self.assertTrue(checked, "no zero-failure claim about the corpus runs was checked")

    # The Firecracker branch rootfs-config.toml pins at each fcvm source
    # revision a cited run records (cell.source_revision). Derived with
    #   git show <rev>:rootfs-config.toml | sed -n '/^\[firecracker\]/,/^\[/p'
    # A run at a revision missing here fails the test until its pin is added.
    FIRECRACKER_BRANCH = {
        "55756858d46347f00bac3f66f1f5cacf3411bbb8": "agent/uffd-minor",
        "1e9e9b70937ccc6317046ea22ba41ef0acc454d0": "agent/uffd-minor",
        "46ed7eef965e22881ad8a4fd2bed4708e47dedb5": "bump-vsock-max-connections",
        "0594e96a443ce34345dfd08664c08f88e52120a8": "bump-vsock-max-connections",
        "f7d546902bb0d5e6ee2fc6b54ed7d66e997af9c9": "bump-vsock-max-connections",
        "b72e516aa4bed8f677700d07c8a241c5b30e738f": "bump-vsock-max-connections",
        "97074abdabf2be2af6f188499032e4c128385568": "bump-vsock-max-connections",
        "5893de232a23a0ce00f52816e239ea85173a7c20": "bump-vsock-max-connections",
    }

    def test_each_cited_run_is_attributed_to_the_firecracker_it_ran(self):
        """RED BEFORE THE FIX: the page described one Firecracker build,
        branch bump-vsock-max-connections, for every figure. The corpus runs
        behind the headline were built from agent/uffd-minor, which
        rootfs-config.toml pins at their source revisions; only the fixture
        runs used bump-vsock-max-connections."""
        fc = section(self.page, "firecracker")
        parts = re.split(r"<h3>([^<]+)</h3>", fc)
        subsections = {parts[i].strip(): parts[i + 1] for i in range(1, len(parts), 2)}
        self.assertEqual(sorted(subsections), ["Corpus runs", "Fixture runs"])
        runs = sorted(set(re.findall(r"results/(reqbench-[A-Za-z0-9-]+)", self.page)))
        self.assertTrue(runs, "the page cites no reqbench run; the check is vacuous")
        for run in runs:
            with open(os.path.join(HERE, "results", run, "analysis.json")) as f:
                revision = json.load(f)["cell"]["source_revision"]
            self.assertIn(revision, self.FIRECRACKER_BRANCH,
                          f"{run}: add the Firecracker pin at {revision}")
            branch = self.FIRECRACKER_BRANCH[revision]
            where = "Corpus runs" if "-corpus" in run else "Fixture runs"
            other = "Fixture runs" if where == "Corpus runs" else "Corpus runs"
            self.assertIn(branch, subsections[where], f"{run} ran {branch}; {where} must name it")
            self.assertNotIn(f"Branch {branch}", subsections[other],
                             f"{other} is not built from {branch}")


class PhoneLayout(unittest.TestCase):
    """Source-level checks of the page's layout on a 320 px phone. No browser
    runs in CI, so each check models the one quantity that broke."""

    def setUp(self):
        self.page = read(REPORT)
        self.css = self.page[self.page.index("<style>"):self.page.index("</style>")]

    def test_step_badges_meet_text_contrast_in_both_schemes(self):
        """RED BEFORE THE FIX: the flow's step numbers were 13px white text on
        the chart blue, 4.42:1 in light mode and 3.64:1 in dark, under the
        4.5:1 WCAG AA minimum for text of that size."""
        rule = css_block(self.css, ".flow li::before")
        fg = re.search(r"(?<![-\w])color:\s*(#[0-9a-fA-F]{3,6})", rule).group(1)
        bg = re.search(r"background:\s*var\((--[\w-]+)\)", rule).group(1)
        light = self.css[:self.css.index("@media (prefers-color-scheme: dark)")]
        dark = self.css[self.css.index("@media (prefers-color-scheme: dark)"):]
        for scheme, block in (("light", light), ("dark", dark)):
            value = re.search(re.escape(bg) + r":\s*(#[0-9a-fA-F]{6})", block).group(1)
            self.assertGreaterEqual(contrast(fg, value), 4.5, f"{scheme}: {fg} on {value}")

    def test_only_stacked_and_key_value_tables_break_inside_words(self):
        """RED BEFORE THE FIX: a phone rule gave every non-numeric td
        overflow-wrap: anywhere, which let the auto table layout squeeze plain
        data tables until stage and site names broke mid-word ('Restor/e',
        'theguard/ian.com') instead of scrolling."""
        for selectors, body in re.findall(r"([^{}]+)\{([^{}]*)\}", self.css):
            if "overflow-wrap: anywhere" not in body:
                continue
            for selector in selectors.split(","):
                selector = selector.strip()
                if re.search(r"(?:^|[\s>+~])td\b", selector):
                    self.assertRegex(selector, r"^table\.(?:stack|kv)\b",
                                     f"{selector!r} lets a plain data table break words")

    # Track width of a chart row at 320 px: 16 px page gutters, a 1 px figure
    # border and 12 px figure padding each side. Rows inside a .grp keep a
    # 5.5rem label column and an 8 px gap beside the track. A value label
    # starts 8 px after the bar or whisker end and may run into the figure
    # padding, not past the border. 7.2 px is the widest average advance of
    # 13 px system-ui text with tabular digits.
    TRACK = {"row": 320 - 2 * (16 + 1 + 12), "grp": 320 - 2 * (16 + 1 + 12) - 88 - 8}
    PADDING = 12
    CHAR_PX = 7.2

    def test_chart_value_labels_fit_a_320px_phone(self):
        """RED BEFORE THE FIX: the fault chart's '12,543 (2.3 µs each)' label
        started at 64.51% of the track and ran past the figure border and the
        viewport at 320 px (scrollWidth 326 against 320)."""
        checked = 0
        in_grp = False
        for line in self.page.splitlines():
            if line.startswith('<div class="grp">'):
                in_grp = True
            elif line == "</div>" and in_grp:
                in_grp = False
            if not line.startswith('<div class="r"'):
                continue
            end = float(re.search(r"--end:([\d.]+)%", line).group(1))
            label = plain(re.search(r'<span class="v">(.*?)</span>', line).group(1))
            track = self.TRACK["grp" if in_grp else "row"]
            right = end / 100 * track + 8 + len(label) * self.CHAR_PX
            checked += 1
            self.assertLessEqual(right, track + self.PADDING,
                                 f"{label!r} at {end}% runs {right - track - self.PADDING:.0f} px past the chart")
        self.assertGreater(checked, 20, "too few chart rows found; the check is vacuous")

    # The narrowest request-flow card: a 13rem grid column less 62 px of
    # padding and 2 px of border. Code is 85% of 14 px monospace, about
    # 7.14 px a character, plus 0.4em padding each side.
    FLOW_CODE_CHARS = int((13 * 16 - 62 - 2 - 2 * 0.4 * 11.9) / 7.14)

    def test_code_in_the_request_flow_fits_its_card(self):
        """RED BEFORE THE FIX: 'Page.captureScreenshot' is wider than the
        request-flow card, and code breaks anywhere, so it rendered as
        'Page.captureScreensho' with a lone 't' at 768 to 1280 px."""
        flow = self.page[self.page.index('<ol class="flow">'):]
        flow = flow[:flow.index("</ol>")]
        codes = re.findall(r"<code>(.*?)</code>", flow)
        self.assertTrue(codes, "no code in the request flow; the check is vacuous")
        for code in codes:
            for piece in code.split("<wbr>"):
                self.assertLessEqual(len(html_mod.unescape(piece)), self.FLOW_CODE_CHARS,
                                     f"{code!r} needs a <wbr> to fit a {self.FLOW_CODE_CHARS}-character card")


if __name__ == "__main__":
    unittest.main()
