"""renders_campaign.py: one summary for the regimes of one boot, or nothing."""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import renders_campaign  # noqa: E402

KERNEL = "7.3.3-fcvm-0123456789ab"
SOURCE = "a" * 40
KITESURF = "b" * 40


def write_run(root, run, *, kernel=KERNEL, summary_kernel=None, source=SOURCE, kitesurf=KITESURF,
              instance="m8gd.metal-24xl", boot="0123456789abcdef", left=None, levels=((3, 5), (4, 6)),
              record=None, cmdline="quiet root=PARTUUID=elided"):
    """A run of 20 samples a second apart: each side at its first level, then its second
    from the tenth sample; one render a side a second, 100 ms and 200 ms."""
    out = os.path.join(root, run)
    os.makedirs(out)
    start = 1_000_000
    with open(os.path.join(out, "samples.jsonl"), "w") as samples, \
            open(os.path.join(out, "renders.jsonl"), "w") as renders:
        for i in range(20):
            at = start + i * 1000
            side = lambda n, first, second: {  # noqa: E731
                "level": first if i < 10 else second, "usage_usec": i * n * 1_000_000,
                "pressure_usec": i * 10_000, "pool_waits": 0}
            samples.write(json.dumps({
                "at": at, "recycles": i // 10,
                "fcvm": side(2, *levels[0]), "kitesurf": side(3, *levels[1]),
                "harness": {"usage_usec": i * 100_000, "pressure_usec": i * 1000, "parts": {},
                            "replay_ms": {"fcvm": 0.5, "kitesurf": 0.7}},
            }) + "\n")
            for name, ms in (("fcvm", 100.0), ("kitesurf", 200.0)):
                renders.write(json.dumps({
                    "at": at, "side": name, "page": f"page-{i % 2}", "ok": True, "ms": ms + i % 2,
                    "detail": {"width": 1280, "height": 720}}) + "\n")
    with open(os.path.join(out, "summary.json"), "w") as handle:
        json.dump({"module": {"kernel": summary_kernel or kernel, "fixed_levels": None, "engine_restarts": 0},
                   "sides": {"fcvm": {"setup": {"vcpus": "4"}, "search": {"limit": levels[0][1]}},
                             "kitesurf": {"setup": {"engines": "48"}, "search": {"limit": levels[1][1]}}}}, handle)
    with open(os.path.join(out, "teardown.json"), "w") as handle:
        json.dump(left or {"cgroups_left": [], "resources_left": 0, "run_dir_left": False}, handle)
    with open(os.path.join(out, "host-record.txt"), "w") as handle:
        handle.write(record if record is not None else (
            f"kernel: {kernel}\ncmdline: {cmdline}\nsource: {source}\nkitesurf: {kitesurf}\n"
            f"host: {instance}  boot: {boot}\narguments: MAX_FCVM=40\nended: 2026-01-01T00:00:00Z\n"))
    return out


class Summary(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = self.tmp.name

    def test_a_campaign_names_one_kernel_commit_host_and_boot_and_each_regimes_figures(self):
        write_run(self.root, "a")
        write_run(self.root, "b", levels=((5, 5), (6, 6)))
        s = renders_campaign.summarize(self.root, "c", [("search", "a", 0.0, 0.0), ("held", "b", 5.0, 0.0)])
        self.assertEqual((s["kernel"], s["source_revision"], s["kitesurf_commit"], s["instance_type"], s["boot"]),
                         (KERNEL, SOURCE, KITESURF, "m8gd.metal-24xl", "0123456789abcdef"))
        search, held = s["regimes"]
        self.assertEqual((search["label"], search["run"], held["label"]), ("search", "a", "held"))
        fcvm = search["sides"]["fcvm"]
        self.assertEqual(fcvm["level_changes"], [[0, 3], [10, 5]])
        self.assertEqual(search["sides"]["kitesurf"]["level_changes"], [[0, 4], [10, 6]])
        self.assertEqual((fcvm["limit"], fcvm["setup"]), (5, {"vcpus": "4"}))
        self.assertEqual((fcvm["failed"], fcvm["renders"]), (0, 20))
        # 2 CPU-seconds a second over the 19 s window: two CPUs busy, 38 s of CPU over 20 renders
        self.assertEqual((fcvm["cores_busy"], fcvm["cpu_ms_per_render"]), (2.0, 1900.0))
        self.assertEqual(held["window"], {"seconds": 14.0, "skip_s": 5.0, "last_s": 0.0})
        self.assertEqual(sorted(search["files"]), sorted(renders_campaign.RECORD_FILES))
        size = os.path.getsize(os.path.join(self.root, "a", "renders.jsonl"))
        self.assertEqual(search["files"]["renders.jsonl"]["size"], size)
        self.assertRegex(search["files"]["renders.jsonl"]["sha256"], r"^[0-9a-f]{64}$")

    def refused(self, regimes):
        with self.assertRaises(renders_campaign.Refused) as caught:
            renders_campaign.summarize(self.root, "c", regimes)
        return str(caught.exception)

    def test_runs_of_two_boots_kernels_commits_or_hosts_are_not_one_campaign(self):
        write_run(self.root, "a")
        for run, change, named in (
            ("boot", {"boot": "f" * 16}, "boot"),
            ("kernel", {"kernel": "7.3.3-fcvm-ba9876543210"}, "kernel"),
            ("source", {"source": "c" * 40}, "source"),
            ("kitesurf", {"kitesurf": "d" * 40}, "kitesurf"),
            ("host", {"instance": "c8gd.metal-24xl"}, "instance_type"),
        ):
            with self.subTest(run=run):
                write_run(self.root, run, **change)
                text = self.refused([("one", "a", 0.0, 0.0), ("two", run, 0.0, 0.0)])
                self.assertIn(f"not the campaign's {named}", text)

    def test_a_kernel_that_is_not_fcvms_own_is_refused(self):
        for kernel in ("7.0.0-1018-aws", "7.3.3", "7.3.3-fcvm", ""):
            with self.subTest(kernel=kernel):
                write_run(self.root, f"k{kernel}", kernel=kernel)
                self.assertIn("is not fcvm's host kernel", self.refused([("one", f"k{kernel}", 0.0, 0.0)]))

    def test_a_run_whose_summary_names_another_kernel_is_refused(self):
        write_run(self.root, "a", summary_kernel="7.3.3-fcvm-ffffffffffff")
        self.assertIn("summary.json names kernel", self.refused([("one", "a", 0.0, 0.0)]))

    def test_a_run_whose_teardown_left_something_is_refused(self):
        for i, left in enumerate(({"cgroups_left": ["x"], "resources_left": 0, "run_dir_left": False},
                                  {"cgroups_left": [], "resources_left": 2, "run_dir_left": False},
                                  {"cgroups_left": [], "resources_left": 0, "run_dir_left": True})):
            with self.subTest(left=left):
                write_run(self.root, f"t{i}", left=left)
                self.assertIn("the teardown left something", self.refused([("one", f"t{i}", 0.0, 0.0)]))

    def test_a_teardown_that_does_not_say_nothing_was_left_is_refused(self):
        """Absent keys are not "nothing left": the reaper's result must say so."""
        for i, left in enumerate(({}, {"cgroups_left": None, "resources_left": 0, "run_dir_left": False},
                                  {"error": "the reaper died"}, [],
                                  {"cgroups_left": [], "resources_left": "0", "run_dir_left": False},
                                  {"cgroups_left": [], "resources_left": 0})):
            with self.subTest(left=left):
                out = write_run(self.root, f"q{i}")
                with open(os.path.join(out, "teardown.json"), "w") as handle:
                    json.dump(left, handle)
                self.assertIn("does not say nothing was left", self.refused([("one", f"q{i}", 0.0, 0.0)]))

    def test_a_host_record_must_say_each_thing_once_and_hold_no_partition_id(self):
        good = (f"kernel: {KERNEL}\ncmdline: quiet root=PARTUUID=elided\nsource: {SOURCE}\nkitesurf: {KITESURF}\n"
                "host: m8gd.metal-24xl  boot: 0123456789abcdef\narguments: MAX_FCVM=40\nended: 2026-01-01T00:00:00Z\n")
        write_run(self.root, "good", record=good)
        renders_campaign.summarize(self.root, "c", [("one", "good", 0.0, 0.0)])
        for name, record, text in (
            ("twice", good.replace(f"kernel: {KERNEL}\n", f"kernel: 7.0.0-1018-aws\nkernel: {KERNEL}\n"), "says `kernel` twice"),
            ("noend", good.replace("ended: 2026-01-01T00:00:00Z\n", ""), "no `ended`"),
            ("noargs", good.replace("arguments: MAX_FCVM=40\n", ""), "no `arguments`"),
            ("badend", good.replace("2026-01-01T00:00:00Z", "yesterday"), "`ended`"),
            ("uuid", good.replace("PARTUUID=elided", "PARTUUID=4a97390b-26c9-47aa-b101-000000000000"), "partition"),
            ("uuid2", good.replace("root=PARTUUID=elided", "root=UUID=0b1c2d3e-0000-4000-8000-000000000000"), "partition"),
            ("nohost", good.replace("host: m8gd.metal-24xl  boot: 0123456789abcdef\n", "host: m8gd.metal-24xl\n"), "no `host: TYPE  boot: HASH`"),
            ("commit", good.replace(SOURCE, "abc123"), "is not a commit"),
        ):
            with self.subTest(name=name):
                write_run(self.root, name, record=record)
                self.assertIn(text, self.refused([("one", name, 0.0, 0.0)]))

    def test_a_run_is_a_directory_of_the_campaign_and_a_window_holds_samples(self):
        write_run(self.root, "a")
        for run, text in ((os.path.join(self.root, "a"), "names a directory of the campaign"),
                          ("./a", "names a directory of the campaign"), ("a/", "names a directory of the campaign"),
                          ("..", "names a directory of the campaign"), ("absent", "absent")):
            with self.subTest(run=run):
                self.assertIn(text, self.refused([("one", run, 0.0, 0.0)]))
        self.assertIn("fewer than two samples", self.refused([("one", "a", 100.0, 0.0)]))
        self.assertIn("window", self.refused([("one", "a", -1.0, 0.0)]))
        os.remove(os.path.join(self.root, "a", "teardown.json"))
        self.assertIn("teardown.json", self.refused([("one", "a", 0.0, 0.0)]))
        out = os.path.join(self.root, "summary.json")
        self.assertEqual(renders_campaign.main([self.root, "--name", "c", "--regime", "one=a", "--out", out]), 2)
        self.assertFalse(os.path.exists(out))

    def test_no_regime_or_a_label_twice_is_refused(self):
        write_run(self.root, "a")
        self.assertIn("no regime", self.refused([]))
        self.assertIn("used twice", self.refused([("one", "a", 0.0, 0.0), ("one", "a", 5.0, 0.0)]))

    def test_the_command_writes_nothing_when_it_refuses(self):
        write_run(self.root, "a", kernel="7.0.0-1018-aws")
        out = os.path.join(self.root, "summary.json")
        self.assertEqual(renders_campaign.main([self.root, "--name", "c", "--regime", "one=a", "--out", out]), 2)
        self.assertFalse(os.path.exists(out))

    def test_the_command_writes_the_same_bytes_twice(self):
        write_run(self.root, "a")
        outs = [os.path.join(self.root, f"s{i}.json") for i in (1, 2)]
        for out in outs:
            self.assertEqual(renders_campaign.main(
                [self.root, "--name", "c", "--regime", "one=a,skip=2", "--regime", "end=a,last=5", "--out", out]), 0)
        with open(outs[0], "rb") as a, open(outs[1], "rb") as b:
            first = a.read()
            self.assertEqual(first, b.read())
        doc = json.loads(first)
        self.assertEqual([r["window"]["last_s"] for r in doc["regimes"]], [0.0, 5.0])
        self.assertEqual(doc["regimes"][0]["window"]["skip_s"], 2.0)

    def test_a_regime_argument_is_label_run_and_a_window(self):
        self.assertEqual(renders_campaign.parse_regime("held=c-held-a,skip=60"), ("held", "c-held-a", 60.0, 0.0))
        self.assertEqual(renders_campaign.parse_regime("s=run,last=300"), ("s", "run", 30.0, 300.0))
        for bad in ("run", "=run", "a=", "a=run,first=3", "a=run,skip=soon"):
            with self.subTest(bad=bad), self.assertRaises(Exception):
                renders_campaign.parse_regime(bad)


if __name__ == "__main__":
    unittest.main()
