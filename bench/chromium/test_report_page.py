#!/usr/bin/env python3
"""The benchmark report is published by GitHub Pages from its fragment source.

report/shared-nothing-renders.html carries no document skeleton; report/wrap_page.py
adds one, and .github/workflows/pages.yml runs it at deploy time. These tests pin
both halves, so the published page cannot silently stop tracking the source.
"""

import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
REPORT = os.path.join(HERE, "report", "shared-nothing-renders.html")
PAGES_WORKFLOW = os.path.join(REPO, ".github", "workflows", "pages.yml")

sys.path.insert(0, os.path.join(HERE, "report"))

import wrap_page  # noqa: E402


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


class WrappedReport(unittest.TestCase):
    def test_the_fragment_becomes_one_complete_document(self):
        """The wrapper adds only the document skeleton around the fragment."""
        fragment = read(REPORT)
        page = wrap_page.wrap(fragment)
        self.assertTrue(page.startswith("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n"))
        self.assertTrue(page.endswith("</main>\n</body>\n</html>\n"))
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
                         ["<title>Shared-Nothing Renders</title>"])
        for tag in ("<!DOCTYPE", "<html", "<head", "<body"):
            self.assertEqual(page.count(tag), 1, tag)
        # Removing the skeleton the wrapper adds gives back the fragment,
        # so nothing else was added, dropped or reordered.
        stripped = (page.removeprefix(wrap_page.PREAMBLE)
                    .replace("</head>\n<body>\n", "", 1)
                    .removesuffix("</body>\n</html>\n"))
        self.assertEqual(stripped, fragment.rstrip("\n") + "\n")

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


if __name__ == "__main__":
    unittest.main()
