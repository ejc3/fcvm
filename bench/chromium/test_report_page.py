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
        from html.parser import HTMLParser

        page = wrap_page.wrap(read(REPORT))
        parents = {}

        class Landmarks(HTMLParser):
            VOID = {"meta", "wbr", "br", "img", "link"}

            def __init__(self):
                super().__init__()
                self.stack = []

            def handle_starttag(self, tag, attrs):
                if tag in self.VOID:
                    return
                if tag in ("header", "main", "footer"):
                    parents.setdefault(tag, []).append(self.stack[-1] if self.stack else None)
                self.stack.append(tag)

            def handle_endtag(self, tag):
                if tag not in self.VOID and self.stack and self.stack[-1] == tag:
                    self.stack.pop()

        Landmarks().feed(page)
        self.assertEqual(parents, {"header": ["body"], "main": ["body"], "footer": ["body"]},
                         "header, main and footer must each be a single child of <body>")
        body = page[page.index("<body>"):]
        self.assertLess(body.index("<header"), body.index("<main>"))
        self.assertLess(body.index("</main>"), body.index("<footer"))

    def test_a_fragment_without_main_is_refused(self):
        """There is nothing to put in <body> without a <main>."""
        with self.assertRaisesRegex(ValueError, "no <main>"):
            wrap_page.wrap("<title>x</title>\n<style></style>\n<p>no main</p>\n")

    def test_a_main_that_would_land_in_the_head_is_refused(self):
        """RED BEFORE THE FIX: after the split moved to </style>, a <main>
        that only appears in the style, or before it, passed the whole-fragment
        check, and the page was published with no <main> in its body."""
        for fragment in ("<title>x</title>\n<style>/* <main> */</style>\n<p>body</p>\n",
                         "<title>x</title>\n<main>\n<style></style>\n<p>body</p></main>\n"):
            with self.assertRaises(ValueError, msg=fragment):
                wrap_page.wrap(fragment)

    def test_a_fragment_without_a_style_is_refused(self):
        """The head ends at </style>; without one there is no head to end."""
        with self.assertRaisesRegex(ValueError, "no </style>"):
            wrap_page.wrap("<title>x</title>\n<main></main>\n")

    def test_a_crlf_fragment_is_wrapped(self):
        """A </style> followed by CRLF still ends the head."""
        page = wrap_page.wrap("<title>x</title>\r\n<style></style>\r\n<main></main>\r\n")
        self.assertIn("</style>\r\n</head>\n<body>\n<main>", page)

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
    text = html_mod.unescape(re.sub(r"<[^>]+>", " ", fragment)).replace(" ", " ")
    return re.sub(r"\s+", " ", text).strip()


def section(page, anchor):
    """The HTML from <h2 id=anchor> up to the next <h2."""
    start = page.index(f'<h2 id="{anchor}">')
    end = page.find("<h2", start + 1)
    return page[start:end if end != -1 else len(page)]


def css_rules(css):
    """(media, selectors, declarations) for every rule, in source order.
    media is the @media prelude, or None at top level. Comments are dropped."""
    css = re.sub(r"/\*.*?\*/", "", css, flags=re.S)
    rules = []
    head = re.compile(r"\s*([^{}]+?)\s*\{")
    i = 0
    while True:
        m = head.match(css, i)
        if not m:
            break
        prelude = " ".join(m.group(1).split())
        if prelude.startswith("@media"):
            depth, j = 1, m.end()
            while depth:
                depth += {"{": 1, "}": -1}.get(css[j], 0)
                j += 1
            for sel, body in re.findall(r"([^{}]+?)\s*\{([^{}]*)\}", css[m.end():j - 1]):
                rules.append((prelude, [" ".join(s.split()) for s in sel.split(",")], body))
            i = j
        else:
            end = css.index("}", m.end())
            rules.append((None, [" ".join(s.split()) for s in prelude.split(",")], css[m.end():end]))
            i = end + 1
    return rules


def declaration(body, prop):
    """The value of `prop` in a declaration block, or None."""
    m = re.search(r"(?:^|;)\s*" + re.escape(prop) + r"\s*:\s*([^;]+)", body)
    return m.group(1).strip() if m else None


def applies_at(media, width):
    """Whether a rule's @media prelude (or None) applies at a viewport width,
    for the prelude shapes this page uses."""
    if media is None:
        return True
    if "prefers-color-scheme" in media:
        return False
    for kind, px in re.findall(r"(min|max)-width:\s*(\d+)px", media):
        if (kind == "max" and width > int(px)) or (kind == "min" and width < int(px)):
            return False
    return True


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


HEX = re.compile(r"#(?:[0-9a-fA-F]{6}|[0-9a-fA-F]{3})(?![0-9a-fA-F])")


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
        title = self.page.index('<p class="tbl-title">Every row Cloudflare published</p>')
        table = re.search(r"<table\b.*?</table>", self.page[title:], re.S).group(0)
        rows = []
        for tr in re.findall(r"<tr>(.*?)</tr>", table, re.S):
            cells = dict(re.findall(r'<td data-label="([^"]+)">(.*?)</td>', tr, re.S))
            if cells:
                rows.append({k: plain(v) for k, v in cells.items()})
        keys = ("Metric", "Kitesurf", "Cloudflare Chromium", "Kitesurf, relative")
        got = [tuple(r.get(k, "").lower() if k == "Metric" else r.get(k) for k in keys)
               for r in rows[:len(PUBLISHED)]]
        want = [(m.lower(), k, c, r) for m, k, c, r in PUBLISHED]
        self.assertEqual(got, want, "the table must open with Cloudflare's rows, in order, cell for cell")
        extra = [r["Metric"] for r in rows[len(PUBLISHED):]]
        self.assertEqual(extra, ["Web-platform tests"], "rows after Cloudflare's six")

    def test_the_zero_failure_count_carries_its_interval(self):
        """RED BEFORE THE FIX: the headline tile said "0 failed requests in the
        four published corpus runs" and the sample-gate bullet "Zero failures in
        every published cell" with no interval. REVIEW.md requires the interval
        whenever the count supports a reliability claim; each corpus run's
        failure_rate_ci is [0, 0.0159] over 230 attempts.

        Every zero-failure count must carry an exact interval, and one about
        the corpus runs must carry theirs. "zero-failure gate" names the gate
        and is not a count."""
        cited = set(re.findall(r"results/(reqbench-[0-9-]+-corpus[a-z0-9-]*)", self.page))
        self.assertEqual(cited, set(self.CORPUS_RUNS), "the page must cite the four published corpus runs")
        bounds = set()
        for run in self.CORPUS_RUNS:
            with open(os.path.join(HERE, "results", run, "analysis.json")) as f:
                arms = json.load(f)["arms"]
            for name in ("cdp", "noop"):
                self.assertEqual((arms[name]["attempted"], arms[name]["failed"]), (230, 0), (run, name))
                bounds.add(f"{arms[name]['failure_rate_ci'][1] * 100:.2f}%")
        self.assertEqual(len(bounds), 1, f"the runs' intervals differ: {bounds}; quote each")
        bound = bounds.pop()
        units = re.findall(r"<(?:li|p|td)\b[^>]*>(.*?)</(?:li|p|td)>", self.page, re.S)
        units += re.findall(r'<div><span class="num">(.*?)</div>', self.page, re.S)
        claim = re.compile(r"(?i)\b(?:no|zero|0)\s[^.]*\bfail")
        interval = re.compile(r"(?i)\binterval (?:of )?0 to (?:at most )?\d+(?:\.\d+)?%")
        checked = 0
        for unit in units:
            text = plain(unit)
            if not claim.search(text):
                continue
            checked += 1
            self.assertRegex(text, interval, f"a zero-failure count without its interval: {text[:160]!r}")
            if "corpus" in text:
                self.assertIn(bound, text, f"a corpus zero-failure count without {bound}: {text[:160]!r}")
                counts = re.findall(r"\b0 (?:of|failures in|in) (?:the [\w-]+ arm[\u2019']s )?(\d[\d,]*)", text)
                self.assertTrue(counts, f"a corpus zero-failure count without its attempts: {text[:160]!r}")
                self.assertEqual(set(counts), {"230"}, f"attempt counts {counts} in: {text[:160]!r}")
        self.assertGreaterEqual(checked, 2, "the tile and the sample-gate bullet were not both checked")

    CORPUS_RUNS = (
        "reqbench-20260830-171007-corpus",
        "reqbench-20260902-023115-corpus-c2",
        "reqbench-20260902-025115-corpus-c4",
        "reqbench-20260902-031115-corpus-c8",
    )

    # The Firecracker branch rootfs-config.toml selects at each fcvm source
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
        rootfs-config.toml selects at their source revisions; only the fixture
        runs used bump-vsock-max-connections.

        Each subsection names exactly the branch its runs used, and no other
        part of the page names a branch."""
        fc = section(self.page, "firecracker")
        parts = re.split(r"<h3>([^<]+)</h3>", fc)
        subsections = {parts[i].strip(): parts[i + 1] for i in range(1, len(parts), 2)}
        self.assertEqual(sorted(subsections), ["Corpus runs", "Fixture runs"])
        branches = set(self.FIRECRACKER_BRANCH.values())
        expected = {"Corpus runs": set(), "Fixture runs": set()}
        runs = sorted(set(re.findall(r"results/(reqbench-[A-Za-z0-9-]+)", self.page)))
        self.assertTrue(runs, "the page cites no reqbench run; the check is vacuous")
        for run in runs:
            with open(os.path.join(HERE, "results", run, "analysis.json")) as f:
                revision = json.load(f)["cell"]["source_revision"]
            self.assertIn(revision, self.FIRECRACKER_BRANCH,
                          f"{run}: add the Firecracker pin at {revision}")
            where = "Corpus runs" if "-corpus" in run else "Fixture runs"
            expected[where].add(self.FIRECRACKER_BRANCH[revision])
        for where, want in expected.items():
            named = {b for b in branches if b in subsections[where]}
            self.assertEqual(named, want, f"{where} must name exactly the branch its runs used")
        rest = self.page.replace(fc, "")
        self.assertEqual({b for b in branches if b in rest}, set(),
                         "a branch is named outside Firecracker builds, where it is not tied to runs")


class PhoneLayout(unittest.TestCase):
    """Source-level checks of the page's layout on a phone. No browser runs in
    CI, so each check models the one quantity that broke."""

    def setUp(self):
        self.page = read(REPORT)
        self.css = self.page[self.page.index("<style>") + len("<style>"):self.page.index("</style>")]
        self.rules = css_rules(self.css)

    def variables(self, dark):
        """Custom properties of one colour scheme."""
        values = {}
        for media, selectors, body in self.rules:
            if selectors == [":root"] and (media is None or (dark and "prefers-color-scheme: dark" in media)):
                values.update(re.findall(r"(--[\w-]+)\s*:\s*([^;]+)", body))
        return {k: v.strip() for k, v in values.items()}

    def test_step_badges_meet_text_contrast_in_both_schemes(self):
        """RED BEFORE THE FIX: the flow's step numbers were 13px white text on
        the chart blue, 4.42:1 in light mode and 3.64:1 in dark, under the
        4.5:1 WCAG AA minimum for text of that size. Every numbered badge is
        checked, including the untimed step's."""
        badges = [(sel, body) for media, sels, body in self.rules if media is None
                  for sel in sels if sel.startswith(".flow li") and sel.endswith("::before")]
        self.assertGreaterEqual(len(badges), 2, "the badge rules were not found")
        base = dict(badges)[".flow li::before"]
        for dark in (False, True):
            names = self.variables(dark)
            for selector, body in badges:
                colours = []
                for prop in ("color", "background"):
                    value = declaration(body, prop) or declaration(base, prop)
                    var = re.fullmatch(r"var\((--[\w-]+)\)", value)
                    value = names[var.group(1)] if var else value
                    m = HEX.fullmatch(value)
                    self.assertTrue(m, f"{selector} {prop}: {value!r} is not an opaque hex colour")
                    colours.append(value)
                self.assertGreaterEqual(contrast(*colours), 4.5,
                                        f"{selector}, {'dark' if dark else 'light'}: {colours}")

    WORD_BREAKING = re.compile(r"overflow-wrap\s*:\s*anywhere|word-break\s*:\s*break-(?:all|word)")
    MAY_BREAK_WORDS = {"code", ".r .l", "table.kv td", "table.stack td"}

    def test_only_stacked_and_key_value_tables_break_inside_words(self):
        """RED BEFORE THE FIX: a phone rule gave every non-numeric td
        overflow-wrap: anywhere, which let the auto table layout squeeze plain
        data tables until stage and site names broke mid-word ('Restor/e',
        'theguard/ian.com') instead of scrolling. The property is inherited, so
        every rule that breaks words is checked, whatever its selector."""
        for media, selectors, body in self.rules:
            if self.WORD_BREAKING.search(body):
                for selector in selectors:
                    self.assertIn(selector, self.MAY_BREAK_WORDS,
                                  f"{selector!r} ({media or 'all widths'}) breaks words")
        for table in re.findall(r"<table>(.*?)</table>", self.page, re.S):
            self.assertNotIn("<code>", table, "code in a plain data table would break words")

    def test_stacked_tables_stack_wherever_their_columns_split_code(self):
        """RED BEFORE THE FIX: stacked tables returned to columns above 640 px,
        and from 641 to 687 px their columns were narrower than the code they
        hold, so paths broke between letters ('arms.cdp.blocki|ng_ms')."""
        stacking = [media for media, sels, body in self.rules
                    if "table.stack thead" in sels and declaration(body, "display") == "none"]
        self.assertEqual(len(stacking), 1, "the stacking rule was not found")
        width = int(re.search(r"max-width:\s*(\d+)px", stacking[0]).group(1))
        self.assertGreaterEqual(width, 720, "stacked tables must stack up to 720 px")

    # Chart rows at 320 px: 16 px page gutters, a 1 px figure border and 12 px
    # figure padding each side leave a 262 px track. Rows inside a .grp keep a
    # 5.5rem label column and an 8 px gap beside the track. A value label
    # starts 8 px after the bar or whisker end and may run into the figure
    # padding, not past the border. 7.2 px is the widest average advance of
    # 13 px system-ui text with tabular digits.
    TRACK = {"row": 320 - 2 * (16 + 1 + 12), "grp": 320 - 2 * (16 + 1 + 12) - 88 - 8}
    PADDING = 12
    CHAR_PX = 7.2

    def chart_rows(self):
        """(inside a .grp, --end percent, value label) for every chart row."""
        from html.parser import HTMLParser

        rows = []

        class Rows(HTMLParser):
            def __init__(self):
                super().__init__()
                self.stack = []
                self.row = None

            def handle_starttag(self, tag, attrs):
                if tag in ("wbr", "br", "meta", "img"):
                    return
                classes = (dict(attrs).get("class") or "").split()
                self.stack.append((tag, classes))
                if "r" in classes and tag == "div":
                    in_grp = any("grp" in c for _, c in self.stack[:-1])
                    self.row = {"grp": in_grp, "depth": len(self.stack), "end": None, "label": None}
                elif self.row is not None and "t" in classes:
                    self.row["end"] = float(re.search(r"--end:([\d.]+)%", dict(attrs)["style"]).group(1))
                elif self.row is not None and "v" in classes:
                    self.row["label"] = ""

            def handle_endtag(self, tag):
                if tag in ("wbr", "br", "meta", "img"):
                    return
                if self.row is not None and len(self.stack) == self.row["depth"]:
                    rows.append((self.row["grp"], self.row["end"], self.row["label"]))
                    self.row = None
                elif self.row is not None and self.stack and "v" in self.stack[-1][1]:
                    self.row["label"] = self.row["label"].strip()
                self.stack.pop()

            def handle_data(self, data):
                if self.row is not None and self.stack and "v" in self.stack[-1][1]:
                    self.row["label"] += data

        parser = Rows()
        parser.feed(self.page[self.page.index("</style>") + len("</style>"):])
        return rows

    def test_chart_value_labels_fit_a_320px_phone(self):
        """RED BEFORE THE FIX: the fault chart's '12,543 (2.3 µs each)' label
        started at 64.51% of the track and ran past the figure border and the
        viewport at 320 px (scrollWidth 326 against 320)."""
        rows = self.chart_rows()
        self.assertEqual(len(rows), len(re.findall(r'<div class="r"[ >]', self.page)),
                         "a chart row was not modelled")
        for in_grp, end, label in rows:
            self.assertIsNotNone(end, "a chart row has no --end")
            self.assertTrue(label, "a chart row has no value label")
            track = self.TRACK["grp" if in_grp else "row"]
            right = end / 100 * track + 8 + len(label) * self.CHAR_PX
            self.assertLessEqual(right, track + self.PADDING,
                                 f"{label!r} at {end}% runs {right - track - self.PADDING:.0f} px past the chart")

    # Advance widths of 13 px Liberation Sans, the sans-serif Chromium falls
    # back to on Linux. Calibrated against Chromium at 320 px on 03a0c0e5: the
    # model gives the per-site table 274.3 px where Chromium measured 273, and
    # the stage table 337.2 where it measured 301, so it errs wide.
    @staticmethod
    def advance(token, bold):
        width = 0.0
        for ch in token:
            if ch.isdigit():
                width += 7.23
            elif ch in ".,:;/'’":
                width += 3.62
            elif ch in "-–":
                width += 4.33
            elif ch.isupper():
                width += 8.67
            else:
                width += 6.5
        return width * (1.1 if bold else 1.0)

    def padding_at(self, selector, width, side):
        """Horizontal padding of `selector` at a viewport width, from the rules
        that apply there, last one winning."""
        value = None
        for media, sels, body in self.rules:
            if selector in sels and applies_at(media, width):
                shorthand = declaration(body, "padding")
                if shorthand:
                    parts = shorthand.split()
                    value = parts[1] if len(parts) > 1 else parts[0]
                longhand = declaration(body, f"padding-{side}")
                if longhand:
                    value = longhand
        return float(value.rstrip("px")) if value else 0.0

    def test_plain_tables_fit_a_320px_phone(self):
        """RED BEFORE THE FIX: at 320 px the per-site and stage-median tables
        were wider than their scroll containers, and the clip edge fell on the
        decimal point, so 894.9 read as 894 with nothing to show the table
        continued."""
        cell_pad = self.padding_at("th", 320, "left")
        details_pad = self.padding_at("details", 320, "left")
        checked = 0
        for m in re.finditer(r"<table>(.*?)</table>", self.page, re.S):
            before = self.page[:m.start()]
            in_details = before.rfind("<details") > before.rfind("</details>")
            columns = {}
            for row in re.findall(r"<tr>(.*?)</tr>", m.group(1), re.S):
                for i, (tag, cell) in enumerate(re.findall(r"<(th|td)[^>]*>(.*?)</\1>", row, re.S)):
                    text = html_mod.unescape(re.sub(r"<(?!wbr>)[^>]+>", "", cell)).replace("<wbr>", " ")
                    # Line-break opportunities: spaces, <wbr>, after an en dash,
                    # and after a hyphen not followed by a digit (UAX #14).
                    text = re.sub(r"-(?=\D)", "- ", text.replace("–", "– "))
                    widest = max((self.advance(t, tag == "th") for t in text.split()), default=0.0)
                    columns[i] = max(columns.get(i, 0.0), widest)
            need = sum(c + 2 * cell_pad for c in columns.values()) + len(columns) + 1
            room = 320 - 2 * 16 - (2 * (1 + details_pad) if in_details else 0)
            checked += 1
            self.assertLessEqual(need, room, f"a {len(columns)}-column table needs {need:.0f} px of {room}")
        self.assertGreaterEqual(checked, 3, "too few plain tables found; the check is vacuous")

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
        codes = re.findall(r"<code\b[^>]*>(.*?)</code>", flow)
        self.assertTrue(codes, "no code in the request flow; the check is vacuous")
        for code in codes:
            for piece in code.split("<wbr>"):
                self.assertLessEqual(len(html_mod.unescape(piece)), self.FLOW_CODE_CHARS,
                                     f"{code!r} needs a <wbr> to fit a {self.FLOW_CODE_CHARS}-character card")


if __name__ == "__main__":
    unittest.main()
