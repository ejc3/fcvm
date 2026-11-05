#!/usr/bin/env python3
"""The report cites exactly one campaign, and nothing it says can be removed by
measuring more.

The owner's rules for report/shared-nothing-renders.html (2026-11-04):

1. Every fcvm number comes from one campaign: one fcvm commit, one Chromium
   build, one Firecracker build, one image build and one runtime bundle per
   engine, on boxes of one instance type, each box on one boot. The page
   names the campaign by citing one index, results/campaign-<id>-summary.json,
   and every record it cites is a directory of that campaign
   (reqbench-<id>-..., corpusextra-<id>-...). The records themselves must
   agree on the source revision, the instance type, the Firecracker binary,
   and per engine the image, the browser build and the runtime bundle, so a
   record of another campaign renamed into the prefix still fails. The boots
   the records name are exactly as many as the boxes Setup states
   (id="box-count"), and every A/B/A sequence ran on one boot.
2. No dates beyond the one that says when the campaign ran: one date, or the
   first and last day of a campaign that ran past midnight, in the Setup
   table's cell with id="campaign-date". The only other dates
   allowed are those of Cloudflare's published figures, in their source lines
   (class "cf-src"). With one campaign there is nothing to tell apart by date.
3. No removable caveats. The Limits section holds only limits more measurement
   cannot remove, each named in ALLOWED_LIMITS, and no sentence anywhere says
   a configuration was not run, that intervals overlap, or that bars do not
   order what they show.

Run against another page: REPORT_PAGE=path python3 -m unittest test_report_one_campaign
"""

import json
import os
import re
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
REPORT = os.environ.get("REPORT_PAGE") or os.path.join(HERE, "report", "shared-nothing-renders.html")

# Matched against the page with its <wbr> line-break hints removed: a long
# record name carries them between its parts.
INDEX = re.compile(r"results/campaign-([0-9a-f]{8})-summary\.json")
# A record name ends in a letter or digit; a trailing "." or "-" is the sentence's.
CITED = re.compile(r"results/([A-Za-z0-9](?:[A-Za-z0-9_.\-]*[A-Za-z0-9])?)")
# A full date: ISO, a compact stamp as record directories carry, or a month
# with a day in either order. A bare year ("the March 2023 counter-offset
# series") is not a date of a measurement.
MONTH = r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec)[a-z]*\.?"
DATE = re.compile(
    r"\b20\d\d-[01]\d-[0-3]\d\b"
    r"|\b20\d\d[01]\d[0-3]\d\b"
    rf"|\b{MONTH} [0-3]?\d\b"
    rf"|\b[0-3]?\d {MONTH}\b"
)
# The Limits items allowed to stand: a limit that measurement cannot remove.
ALLOWED_LIMITS = ("Cloudflare's figures come from a setup we cannot run",)
# Caveat language about this campaign's own measurements: each states
# something more runs would settle.
REMOVABLE = re.compile(
    r"\b(?:was|were|is|are|has|have|had) not (?:been )?(?:run|measured|repeated)\b"
    r"|\bnot (?:yet )?(?:run|measured|repeated)\b"
    r"|\bintervals? (?:of [^.]{0,40})?overlap"
    r"|\b(?:do|does|did) not (?:order|rank|separate)\b"
    r"|\bone run each\b|\bonly one run\b|\ba third run\b"
    r"|\bnot (?:priced|isolated|attributed)\b"
    r"|\bdiffer(?:s)? from them in several settings\b",
    re.I,
)


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def plain(fragment):
    import html
    text = html.unescape(re.sub(r"<[^>]+>", " ", fragment.replace("<wbr>", "")))
    return re.sub(r"\s+", " ", text.replace(" ", " ")).strip()


def without_elements(page, pattern):
    """The page with every element whose opening tag matches pattern removed,
    and the removed elements' HTML."""
    removed = []

    def cut(match):
        removed.append(match.group(0))
        return " "

    # Elements of these kinds do not nest in the page, so a lazy match to the
    # first closing tag of the same name is the element.
    regex = re.compile(rf"<(p|td|li|span)\b[^>]*{pattern}[^>]*>.*?</\1>", re.S)
    return regex.sub(cut, page), removed


def record_identity(name):
    """(kind, identity) of one cited results directory, read from its records.

    The fields every record kind writes for the box, the build and the code:
    a serial run's reqbench.jsonl meta line; a corpus-extra phase's run.json;
    a scale run's provenance.json."""
    base = os.path.join(RESULTS, name)
    meta_path = os.path.join(base, "reqbench.jsonl")
    if os.path.isfile(meta_path):
        with open(meta_path) as handle:
            meta = json.loads(handle.readline())
        firecracker = meta.get("firecracker") or {}
        return "serial", {
            "source_revision": meta.get("source_revision"),
            "host_boot_sha256": meta.get("host_boot_sha256"),
            "instance_type": (meta.get("host") or {}).get("instance_type"),
            "firecracker_sha256": firecracker.get("sha256"),
            "engine": meta.get("engine"),
            "image_id": meta.get("image_id"),
            "browser_version": meta.get("browser_version"),
            "runtime_bundle_sha256": meta.get("runtime_bundle_sha256"),
        }
    runs = []
    for root, _dirs, files in os.walk(base):
        for leaf in ("run.json", "provenance.json"):
            if leaf in files:
                runs.append(os.path.join(root, leaf))
    identities = []
    for path in sorted(runs):
        with open(path) as handle:
            run = json.load(handle)
        host = run.get("host") or {}
        if "source_revision" not in run:
            continue
        identities.append({
            "source_revision": run.get("source_revision"),
            "host_boot_sha256": run.get("host_boot_sha256") or host.get("host_boot_sha256"),
            "instance_type": host.get("instance_type"),
            "image_id": run.get("image_id"),
        })
    if not identities:
        return None, None
    return "extra", identities


class OneCampaign(unittest.TestCase):

    def setUp(self):
        self.page = read(REPORT).replace("<wbr>", "")

    def campaign(self):
        ids = set(INDEX.findall(self.page))
        self.assertEqual(len(ids), 1, f"the page must cite exactly one campaign index, found {sorted(ids)}")
        return ids.pop()

    def test_every_cited_record_belongs_to_the_one_campaign(self):
        cid = self.campaign()
        prefixes = (f"reqbench-{cid}-", f"corpusextra-{cid}-", f"campaign-{cid}-")
        cited = sorted(set(CITED.findall(self.page)))
        self.assertTrue(cited, "the page cites no record; the check is vacuous")
        foreign = [name for name in cited if not name.startswith(prefixes)]
        self.assertEqual(foreign, [], f"records outside campaign {cid}")

    def test_the_cited_records_share_one_commit_boot_and_build(self):
        """A record renamed into the campaign's prefix still names its own
        commit and boot; every one must name the same."""
        cid = self.campaign()
        cited = sorted({n for n in CITED.findall(self.page)
                        if n.startswith((f"reqbench-{cid}-", f"corpusextra-{cid}-"))})
        self.assertTrue(cited, "the page cites no campaign record")
        seen = {"source_revision": set(), "instance_type": set(), "firecracker_sha256": set()}
        boots = set()
        boot_of = {}
        per_engine = {}
        extras = []
        for name in cited:
            kind, identity = record_identity(name)
            self.assertIsNotNone(kind, f"{name}: no record names its commit and boot")
            rows = identity if kind == "extra" else [identity]
            for row in rows:
                # A host-control record names its boot but not the instance
                # type; its boot must then be one a serial record names, with
                # that record's type (checked below).
                needed = ("source_revision", "host_boot_sha256") + (
                    ("instance_type",) if kind == "serial" else ())
                for field in needed:
                    self.assertTrue(row.get(field), f"{name}: the record does not name its {field}")
                seen["source_revision"].add(row["source_revision"])
                if row.get("instance_type"):
                    seen["instance_type"].add(row["instance_type"])
                boots.add(row["host_boot_sha256"])
                boot_of[name] = row["host_boot_sha256"]
            if kind == "serial":
                self.assertTrue(identity["firecracker_sha256"], f"{name}: no Firecracker sha256")
                seen["firecracker_sha256"].add(identity["firecracker_sha256"])
                engine = per_engine.setdefault(identity["engine"], {
                    "image_id": set(), "browser_version": set(), "runtime_bundle_sha256": set()})
                for field in engine:
                    engine[field].add(identity[field])
            else:
                extras.extend((name, row) for row in rows)
        serial_boots = {boot_of[n] for n in boot_of if n.startswith(f"reqbench-{cid}-")
                        and record_identity(n)[0] == "serial"}
        for name, row in extras:
            if not row.get("instance_type"):
                self.assertIn(row["host_boot_sha256"], serial_boots,
                              f"{name}: names no instance type and a boot no serial record names")
        for field, values in seen.items():
            self.assertEqual(len(values), 1, f"the cited records name {len(values)} values of {field}: {sorted(values)}")
        count = re.search(r'<span id="box-count">(\d+)</span>', self.page)
        self.assertIsNotNone(count, 'Setup must state the number of boxes in <span id="box-count">')
        self.assertEqual(len(boots), int(count.group(1)),
                         f"the records name {len(boots)} boots, Setup states {count.group(1)} boxes")
        sequences = {}
        for name, boot in boot_of.items():
            m = re.match(rf"reqbench-{cid}-((?:cm|pf)\d+)-(?:a1|b|a2)$", name)
            if m:
                sequences.setdefault(m.group(1), set()).add(boot)
        for seq, seq_boots in sequences.items():
            self.assertEqual(len(seq_boots), 1, f"A/B/A {seq} spans {len(seq_boots)} boots; every arm must run on one box")
        revision = next(iter(seen["source_revision"]))
        self.assertTrue(revision.startswith(cid), f"campaign {cid} is not the commit its records name ({revision})")
        for engine, fields in per_engine.items():
            for field, values in fields.items():
                self.assertEqual(len(values), 1, f"{engine}: {len(values)} values of {field}: {sorted(values)}")
        chromium_image = next(iter(per_engine.get("chromium", {}).get("image_id", {None})))
        for name, row in extras:
            if row.get("image_id") is not None:
                self.assertEqual(row["image_id"], chromium_image, f"{name}: another Chromium image build")

    def test_the_campaign_date_appears_once_in_setup_and_nowhere_else(self):
        rest, cells = without_elements(self.page, r'id="campaign-date"')
        self.assertEqual(len(cells), 1, "the Setup table must carry one cell with id=\"campaign-date\"")
        dates = DATE.findall(plain(cells[0]))
        self.assertIn(len(dates), (1, 2), "the campaign-date cell must hold one date, or a first and last day")
        rest, _cloudflare = without_elements(rest, r'class="[^"]*\bcf-src\b')
        stray = sorted(set(DATE.findall(plain(rest))))
        self.assertEqual(stray, [], "dates outside the campaign-date cell and Cloudflare's source lines")

    def test_limits_hold_only_what_measurement_cannot_remove(self):
        start = self.page.index('<h2 id="limits">')
        end = self.page.find("<h2", start + 1)
        limits = self.page[start:end]
        items = re.findall(r"<li>\s*<strong>(.*?)</strong>", limits, re.S)
        self.assertTrue(items, "the Limits section lists no item")
        for item in items:
            text = plain(item).rstrip(".").replace("\u2019", "'")
            self.assertIn(text, ALLOWED_LIMITS, "a Limits item that more measurement could remove")
        found = sorted({m.group(0) for m in REMOVABLE.finditer(plain(self.page))})
        self.assertEqual(found, [], "caveat language more measurement would remove")


if __name__ == "__main__":
    unittest.main()
