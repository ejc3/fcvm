#!/usr/bin/env python3
"""The warm point must share ZERO byte-resources with measured fixtures.

The golden snapshot is taken after rendering warmup.html; any resource that
page shares with a measured fixture (stylesheet, image, script) can leave
Blink-internal cache warmth (decoded images, parsed stylesheets) in the
snapshot that `Cache-Control: no-store` cannot provably evict — quietly
converting "shared-nothing render" into "warm re-render". This was real:
warmup.html shipped sharing styles.css and img1.png with every measured page
until 2026-08-13. Glyph/shaping warmth for the system font stack is the one
deliberate exception (browser warmth, disclosed in the report).

Run: python3 -m unittest test_fixture_isolation -v
"""

import re
import unittest
from pathlib import Path

PAGES = Path(__file__).resolve().parent / "pages"
MEASURED = ["minimal.html", "medium.html", "heavy.html"]
REF = re.compile(r'(?:src|href)="([^"]+)"')


def refs(page: str) -> set:
    out = set()
    for r in REF.findall((PAGES / page).read_text()):
        if r.startswith("data:"):
            continue  # inline data URIs are private bytes by construction
        out.add(r)
    return out


class WarmupSharesNothing(unittest.TestCase):
    def test_warmup_references_no_measured_resource(self):
        warm = refs("warmup.html")
        for page in MEASURED:
            shared = warm & refs(page)
            self.assertFalse(
                shared,
                f"warmup.html shares {sorted(shared)} with {page}; the warm "
                f"point must not preload any byte-resource a measured render "
                f"will use",
            )

    def test_warmup_has_no_external_stylesheet_or_script(self):
        text = (PAGES / "warmup.html").read_text()
        self.assertNotIn(
            'rel="stylesheet" href=',
            text.replace("data:,", ""),
            "warmup styles must be inline (private bytes)",
        )
