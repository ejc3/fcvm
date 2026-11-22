import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import startrate  # noqa: E402


class Summary(unittest.TestCase):
    def write_run(self, d, healthy_at):
        rows = []
        for i, h in enumerate(healthy_at):
            rows.append({
                "i": i, "name": f"c{i}", "spawn_at_ms": 0.0, "popen_ms": 1.0,
                "healthy_at_ms": h, "spawn_to_healthy_ms": h,
                "marks_ms": {"Cloning VM from serve PID": 5.0, "exec server ready": h - 10,
                             startrate.HEALTHY_MARKER: h},
                "error": None, "exit": None,
            })
        (d / "samples.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
        (d / "host.json").write_text(json.dumps({"instance_type": "t", "config": {}}))

    def test_best_window_counts_clones_inside_one_second(self):
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            # 3 within [1000, 2000], then 2 far apart
            self.write_run(d, [1000, 1500, 2000, 4000, 9000])
            s = startrate.summarize(d)
            self.assertEqual(s["best_1s_window"], 3)
            self.assertEqual(s["healthy"], 5)
            self.assertEqual(s["healthy_per_second"][:5], [0, 2, 1, 0, 1])
            self.assertEqual(s["stages_ms"]["agent->healthy"]["p50"], 10.0)
            self.assertEqual(s["stages_ms"]["pre-log"]["p50"], 5.0)


class FailClosed(unittest.TestCase):
    def fake_fcvm(self, d, body):
        p = d / "fcvm"
        p.write_text("#!/bin/sh\n" + body + "\n")
        p.chmod(p.stat().st_mode | stat.S_IEXEC)
        return str(p)

    def test_ls_failure_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "exit 3")
            with self.assertRaises(SystemExit) as e:
                startrate.list_vms(f, False)
            self.assertIn("BLOCKED", str(e.exception))

    def test_ls_garbage_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "echo not-json")
            with self.assertRaises(SystemExit):
                startrate.list_vms(f, False)

    def test_ls_object_blocks(self):
        with tempfile.TemporaryDirectory() as t:
            f = self.fake_fcvm(Path(t), "echo '{}'")
            with self.assertRaises(SystemExit):
                startrate.list_vms(f, False)


if __name__ == "__main__":
    unittest.main()
