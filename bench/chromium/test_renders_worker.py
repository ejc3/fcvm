#!/usr/bin/env python3
"""renders_worker.py's line protocol, with stand-ins for the two render calls.

The worker imports reqbench and cdpdrive from the directory --bench names, so the
tests give it a directory holding small modules with the same entry points.
"""

import io
import json
import os
import sys
import tempfile
import textwrap
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import renders_worker  # noqa: E402

STAND_IN_REQBENCH = textwrap.dedent('''
    calls = []
    def become_child_subreaper():
        calls.append("subreaper")
    class Interrupted(BaseException):
        pass
    def run_cdp_request(args, rep, fast):
        calls.append((args.url, rep, fast, args.viewport, args.spawn_prefix[-2]))
        if "explode" in args.url:
            raise RuntimeError("no clone")
        if "interrupt" in args.url:
            raise KeyboardInterrupt()
        rec = {"ok": "fail" not in args.url, "blocking_ms": 12.5, "log": None,
               "render": {"image_sha256": "abc", "image_bytes": 7, "viewport": args.viewport,
                          "width": 1280, "height": 720}}
        if not rec["ok"]:
            rec["error"] = "render failed"
            rec["failure_stage"] = "navigate"
        args.on_answer(rec)
        rec["teardown"] = {"teardown_total_ms": 70.0,
                           "lifetime_cpu_ms_by_child": {"fcvm": 100.0, "firecracker": 900.0},
                           "reaped_children_cpu_ms_by_child": {"pasta": 5.0},
                           "per_child_cpu": {"firecracker": {"reclaim_cpu_ms": 20.0}}}
        return rec
''')

STAND_IN_CDPDRIVE = textwrap.dedent('''
    def parse_viewport(text):
        if text and "x" not in text:
            raise ValueError(f"--viewport wants WIDTHxHEIGHT, got {text!r}")
    def drive(ns):
        if "explode" in ns.url:
            raise OSError("refused")
        return {"ok": True, "image_sha256": "def", "image_bytes": 9, "width": 1280, "height": 720,
                "viewport": ns.viewport, "stages": {"navigate_ms": 3.0}}
''')


def run(argv, jobs):
    stdin = io.StringIO("".join(json.dumps(j) + "\n" for j in jobs))
    stdout = io.StringIO()
    status = renders_worker.main(argv, stdin=stdin, stdout=stdout)
    return status, [json.loads(line) for line in stdout.getvalue().splitlines()]


class WorkerProtocol(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        for name, text in (("reqbench.py", STAND_IN_REQBENCH), ("cdpdrive.py", STAND_IN_CDPDRIVE)):
            with open(os.path.join(self.dir.name, name), "w") as handle:
                handle.write(text)
        # The real modules, if another test file loaded them, come back afterwards as the
        # same objects.
        saved = {name: sys.modules.pop(name, None) for name in ("reqbench", "cdpdrive")}
        path = list(sys.path)

        def restore():
            sys.path[:] = path
            for name, module in saved.items():
                sys.modules.pop(name, None)
                if module is not None:
                    sys.modules[name] = module

        self.addCleanup(restore)

    def fcvm(self, jobs):
        return run(["--side", "fcvm", "--index", "3", "--bench", self.dir.name,
                    "--viewport", "1280x720", "--run-id", "r", "--out-dir", self.dir.name,
                    "--fcvm", "/bin/true", "--serve-pid", "42", "--data-root", self.dir.name,
                    "--cgroup-procs", "/sys/fs/cgroup/x/cgroup.procs"], jobs)

    def test_an_fcvm_job_is_answered_before_its_row(self):
        status, lines = self.fcvm([
            {"seq": 1, "page": 0, "url": "https://a.test/", "level": 4},
            {"seq": 2, "page": 1, "url": "https://fail.test/", "level": 5},
        ])
        self.assertEqual(status, 0)
        self.assertEqual(lines[0]["ready"], True)
        self.assertEqual([("answered" in row, row.get("seq")) for row in lines[1:]],
                         [(True, 1), (False, 1), (True, 2), (False, 2)])
        answered, row = lines[1], lines[2]
        self.assertEqual((answered["ok"], answered["ms"]), (True, 12.5))
        self.assertEqual((row["ok"], row["ms"], row["page"], row["level"]), (True, 12.5, 0, 4))
        self.assertEqual((row["sha256"], row["bytes"]), ("abc", 7))
        # the clone's processes, those it reaped, and the memory the kernel freed for it
        self.assertEqual(row["cpu_ms"], 1025.0)
        self.assertEqual(row["detail"]["viewport"], "1280x720")
        # the image's own size, so a run can show both sides drew one viewport
        self.assertEqual((row["detail"]["width"], row["detail"]["height"]), (1280, 720))
        self.assertEqual(row["detail"]["teardown_ms"], 70.0)
        failed = lines[4]
        self.assertEqual((failed["ok"], failed["error"], failed["stage"]),
                         (False, "render failed", "navigate"))
        self.assertEqual(lines[3]["ok"], False)
        import reqbench
        self.assertEqual(reqbench.calls[0], "subreaper")
        # the clone joins the side's cgroup through the guard, and the viewport reaches it
        self.assertEqual(reqbench.calls[1],
                         ("https://a.test/", 0, True, "1280x720", "/sys/fs/cgroup/x/cgroup.procs"))

    def test_a_request_that_raises_is_a_failed_row_and_the_worker_goes_on(self):
        status, lines = self.fcvm([
            {"seq": 1, "page": 0, "url": "https://explode.test/", "level": 1},
            {"seq": 2, "page": 1, "url": "https://a.test/", "level": 1},
        ])
        self.assertEqual(status, 0)
        rows = [row for row in lines[1:] if "answered" not in row]
        self.assertEqual([row["ok"] for row in rows], [False, True])
        self.assertIn("RuntimeError: no clone", rows[0]["error"])
        self.assertGreaterEqual(rows[0]["ms"], 0.0)
        self.assertNotIn(True, ["answered" in row and row["seq"] == 1 for row in lines])

    def test_an_interrupt_ends_the_worker_after_its_row(self):
        status, lines = self.fcvm([
            {"seq": 1, "page": 0, "url": "https://interrupt.test/", "level": 1},
            {"seq": 2, "page": 1, "url": "https://a.test/", "level": 1},
        ])
        self.assertEqual(status, 1)
        self.assertEqual([row.get("seq") for row in lines[1:]], [1])
        self.assertEqual(lines[1]["ok"], False)

    def test_a_kitesurf_job_is_one_row_against_the_engine_it_names(self):
        status, lines = run(["--side", "kitesurf", "--index", "0", "--bench", self.dir.name,
                             "--viewport", "1280x720"], [
            {"seq": 7, "page": 2, "url": "https://a.test/", "level": 30, "address": "127.0.0.1:28802"},
            {"seq": 8, "page": 3, "url": "https://explode.test/", "level": 30, "address": "127.0.0.1:28802"},
        ])
        self.assertEqual(status, 0)
        self.assertEqual(len(lines), 3)
        row = lines[1]
        self.assertEqual((row["seq"], row["ok"], row["sha256"], row["cpu_ms"]), (7, True, "def", None))
        self.assertEqual(row["detail"]["address"], "127.0.0.1:28802")
        self.assertEqual(row["detail"]["viewport"], "1280x720")
        self.assertEqual((row["detail"]["width"], row["detail"]["height"]), (1280, 720))
        self.assertGreaterEqual(row["ms"], 0.0)
        self.assertEqual((lines[2]["ok"], lines[2]["error"]), (False, "OSError: refused"))

    def test_a_malformed_viewport_ends_the_worker_before_it_is_ready(self):
        """The module learns at start, from a worker that exits, and no render is tried."""
        for side in (["--side", "kitesurf"],
                     ["--side", "fcvm", "--run-id", "r", "--out-dir", self.dir.name,
                      "--fcvm", "/bin/true", "--serve-pid", "42", "--data-root", self.dir.name,
                      "--cgroup-procs", "/sys/fs/cgroup/x/cgroup.procs"]):
            stdout = io.StringIO()
            with self.assertRaises(SystemExit):
                renders_worker.main(side + ["--index", "0", "--bench", self.dir.name,
                                            "--viewport", "1280-720"],
                                    stdin=io.StringIO(""), stdout=stdout)
            self.assertEqual(stdout.getvalue(), "")

    def test_the_fcvm_side_refuses_to_start_without_its_arguments(self):
        with self.assertRaises(SystemExit):
            run(["--side", "fcvm", "--index", "0", "--bench", self.dir.name], [])


if __name__ == "__main__":
    unittest.main()
