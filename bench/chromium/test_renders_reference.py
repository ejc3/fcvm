"""renders_reference.py: the page's reference file is what the committed summary gives."""

import copy
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_reference  # noqa: E402

SIDE = {"in_flight_median": 15, "renders_per_s": 23.5, "page_mean_ms": 634.0, "cpu_ms_per_render": 1439.7,
        "cores_busy": 33.9, "cpu_pressure": 0.13, "renders": 7069, "failed": 0, "limit": None,
        "page_median_ms": {"example.com": 230.1}, "setup": {"vcpus": "4"}}
SUMMARY = {
    "schema": 1, "campaign": "renders-test", "kernel": "7.3.3-fcvm-0123456789ab", "source_revision": "a" * 40,
    "kitesurf_commit": "b" * 40, "instance_type": "m8gd.metal-24xl", "boot": "0" * 16,
    "regimes": [
        {"label": "held", "run": "c-held", "ended": "2026-01-02T03:04:05Z", "window": {"seconds": 300.0},
         "fixed_levels": [15, 38], "sides": {"fcvm": SIDE, "kitesurf": dict(SIDE, renders_per_s=31.6, in_flight_median=38)},
         "files": {"summary.json": {"sha256": "c" * 64, "size": 1}}},
        {"label": "alone-fcvm", "run": "c-alone", "ended": "2026-01-02T04:00:00Z", "window": {"seconds": 240.0},
         "fixed_levels": [15, 0], "sides": {"fcvm": SIDE, "kitesurf": {"renders": 0, "failed": 0}}, "files": {}},
    ],
}


class Reference(unittest.TestCase):
    def test_the_committed_file_is_what_the_committed_summary_gives(self):
        self.assertEqual(renders_reference.main(["--check"]), 0)

    def test_every_figure_comes_from_a_named_regime_of_the_summary(self):
        ref = renders_reference.reference(SUMMARY, os.path.join(renders_reference.REPO, "x", "s.json"), "d" * 64)
        self.assertEqual((ref["campaign"], ref["date"], ref["instanceType"], ref["kernel"]),
                         ("renders-test", "2026-01-02", "m8gd.metal-24xl", "7.3.3-fcvm-0123456789ab"))
        self.assertEqual((ref["sourceRevision"], ref["kitesurfCommit"]), ("a" * 40, "b" * 40))
        self.assertEqual((ref["summary"], ref["summarySha256"]), (os.path.join("x", "s.json"), "d" * 64))
        held = ref["regimes"]["held"]
        self.assertEqual((held["run"], held["windowS"], held["fixedLevels"], held["summarySha256"]),
                         ("c-held", 300.0, [15, 38], "c" * 64))
        self.assertEqual(held["sides"]["fcvm"], {
            "inFlight": 15, "rendersPerS": 23.5, "pageMeanMs": 634.0, "cpuMsPerRender": 1439.7, "cpusBusy": 33.9,
            "cpuPressure": 0.13, "renders": 7069, "failed": 0, "limit": None,
            "pageMedianMs": {"example.com": 230.1}, "setup": {"vcpus": "4"}})
        self.assertEqual(held["sides"]["kitesurf"]["rendersPerS"], 31.6)
        # a side that did not run in a regime has no figures there
        self.assertEqual(sorted(ref["regimes"]["alone-fcvm"]["sides"]), ["fcvm"])

    def refused(self, change):
        summary = copy.deepcopy(SUMMARY)
        change(summary)
        with self.assertRaises(renders_reference.Refused) as caught:
            renders_reference.reference(summary, os.path.join(renders_reference.REPO, "s.json"), "d" * 64)
        return str(caught.exception)

    def test_a_summary_from_another_kernel_gives_no_reference(self):
        for kernel in ("7.0.0-1018-aws", "7.3.3", ""):
            with self.subTest(kernel=kernel):
                self.assertIn("is not fcvm's host kernel", self.refused(lambda s, k=kernel: s.update(kernel=k)))

    def test_a_summary_without_both_sides_held_gives_no_reference(self):
        self.assertIn("no `held` regime", self.refused(lambda s: s["regimes"].pop(0)))
        self.assertIn("no `held` regime",
                      self.refused(lambda s: s["regimes"][0]["sides"].update(kitesurf={"renders": 0})))
        self.assertIn("schema", self.refused(lambda s: s.update(schema=2)))

    def test_a_summary_missing_what_the_page_names_gives_no_reference(self):
        for key in ("instance_type", "campaign", "source_revision", "kitesurf_commit"):
            with self.subTest(key=key):
                self.assertIn(key, self.refused(lambda s, k=key: s.pop(k)))
        self.assertIn("is not fcvm's host kernel", self.refused(lambda s: s.update(kernel=s["kernel"] + "\n")))
        self.assertIn("ended", self.refused(lambda s: s["regimes"][0].update(ended="")))
        # a side that completed nothing has no table row
        self.assertIn("no figures", self.refused(
            lambda s: s["regimes"][0]["sides"].update(fcvm=dict(SIDE, failed=7069, renders_per_s=0.0, page_mean_ms=None))))

    def test_a_withdrawn_run_or_campaign_gives_no_reference(self):
        """A WITHDRAWN marker in the campaign's directory, or in the directory of a run the summary cites,
        withdraws the reference: nothing is written and --check fails."""
        with tempfile.TemporaryDirectory() as tmp:
            summary = os.path.join(tmp, "renders-test-summary.json")
            with open(summary, "w") as handle:
                json.dump(SUMMARY, handle)
            for run in ("c-held", "c-alone"):
                os.makedirs(os.path.join(tmp, "renders-test", run))
            text = renders_reference.generate(summary)
            self.assertIn("export const RENDERS_REFERENCE", text)
            for where in (("renders-test", "c-alone"), ("renders-test",)):
                with self.subTest(where=where):
                    marker = os.path.join(tmp, *where, "WITHDRAWN")
                    with open(marker, "w") as handle:
                        handle.write("measured on the wrong bundle\n")
                    with self.assertRaises(renders_reference.Refused) as caught:
                        renders_reference.generate(summary)
                    self.assertIn("measured on the wrong bundle", str(caught.exception))
                    os.remove(marker)
            # a cited run whose directory is gone cannot be checked for a marker
            os.rmdir(os.path.join(tmp, "renders-test", "c-alone"))
            with self.assertRaises(renders_reference.Refused):
                renders_reference.generate(summary)

    def test_the_held_regime_is_a_run_held_at_fixed_levels(self):
        """A search run labelled `held` is not the page's table: the regime must say the level each side
        was held at, and each side's renders in flight must have been that level."""
        for levels in (None, [15, 0], [0, 38], [15], "15,38", [15, 38, 1], [True, 38]):
            with self.subTest(levels=levels):
                self.assertIn("held at fixed levels",
                              self.refused(lambda s, v=levels: s["regimes"][0].update(fixed_levels=v)))
        self.assertIn("held at fixed levels", self.refused(
            lambda s: s["regimes"][0]["sides"]["kitesurf"].update(in_flight_median=37)))

    def test_the_reference_is_text_a_typescript_file_can_hold(self):
        text = renders_reference.generate()
        self.assertTrue(text.startswith("// Generated by bench/chromium/renders_reference.py from bench/chromium/results/renders-"))
        self.assertIn("export const RENDERS_REFERENCE = {", text)
        self.assertTrue(text.endswith("} as const;\n"))


if __name__ == "__main__":
    unittest.main()
