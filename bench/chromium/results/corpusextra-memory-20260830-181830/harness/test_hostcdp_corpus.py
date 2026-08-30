#!/usr/bin/env python3
"""hostcdp.sh runs the corpus schedule, not one URL repeated.

The VM arm cycles its URL list with reqbench.url_for_rep -- urls[rep % len(urls)],
rep counted from 0 across warmup and measured reps alike. A host control that
rendered ONE page while the VM arm rendered fourteen would not be a control for
it, so these pin the cycle, the per-record url, the run.json fields, and the
two-full-cycles warmup floor.

Driven with a stub podman and a python3 shim that answers for cdpdrive.py and
appends the URL it was handed, so the schedule is checked with no container and
no browser. ALLOW_BUSY=1 passes the quiet-box gate.

Watched red against the unpatched hostcdp.sh: the cycle test failed with every
rep recording https://a.example/ (the whole spec passed through as one URL),
the run.json test with KeyError 'urls', and the warmup floor test with exit 0
where 2 was required.

Run: python3 -m unittest test_hostcdp_corpus -v
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SH = os.environ.get("HOSTCDP_SH", os.path.join(HERE, "hostcdp.sh"))
URLS = ["https://a.example/", "https://b.example/", "https://c.example/"]


def write_exec(path, body):
    with open(path, "w") as handle:
        handle.write(body)
    os.chmod(path, 0o755)


class HostCdpCorpusSchedule(unittest.TestCase):
    def _run(self, url_spec, reps, warmup):
        d = tempfile.mkdtemp(prefix="hostcdp-corpus-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d], check=True))
        binx = os.path.join(d, "bin")
        os.makedirs(binx)
        seen = os.path.join(d, "cdpdrive-urls")
        write_exec(os.path.join(binx, "podman"), '''#!/bin/bash
case "$1" in
  run) echo 0123456789ab ;;
  inspect) echo sha256:''' + "c" * 64 + ''' ;;
esac
exit 0
''')
        write_exec(os.path.join(binx, "python3"), f'''#!/bin/bash
case "${{1:-}}" in
  *cdpdrive.py) printf '%s\\n' "$3" >> {seen}; echo '{{"stub": true}}'; exit 0 ;;
esac
exec {sys.executable} "$@"
''')
        env = dict(os.environ)
        env.update({
            "PATH": binx + os.pathsep + env["PATH"],
            "ALLOW_BUSY": "1",
            "RESULTS": os.path.join(d, "results"),
            "URL": url_spec,
            "REPS": str(reps),
            "WARMUP": str(warmup),
        })
        proc = subprocess.run(["bash", SH], env=env, capture_output=True, text=True)
        urls = []
        if os.path.exists(seen):
            urls = open(seen).read().split()
        run_json = os.path.join(d, "results", "run.json")
        meta = json.load(open(run_json)) if os.path.exists(run_json) else None
        return proc, urls, meta, d

    def test_reps_cycle_the_list_the_way_the_vm_arm_does(self):
        """Red: every rep recorded the whole comma-separated spec as one URL."""
        reps = 8
        proc, urls, _, _ = self._run(",".join(URLS), reps, 6)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        self.assertEqual(urls, [URLS[rep % len(URLS)] for rep in range(reps)])

    def test_each_record_carries_the_url_it_rendered(self):
        """Red: records had no url field, so per-URL medians were underivable."""
        proc, _, _, d = self._run(",".join(URLS), 9, 6)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        rows = [json.loads(line) for line in open(os.path.join(d, "results", "hostcdp.jsonl"))]
        self.assertEqual([r["url"] for r in rows],
                         [URLS[rep % len(URLS)] for rep in range(9)])

    def test_run_json_records_the_parsed_corpus(self):
        """Red: KeyError 'urls' -- the record could not say what was rendered."""
        _, _, meta, _ = self._run(",".join(URLS) + " , ", 6, 6)
        self.assertEqual(meta["urls"], URLS)
        self.assertEqual(meta["url_count"], 3)

    def test_a_single_url_is_unchanged(self):
        """One URL keeps today's contract: a one-element list, every rep on it."""
        proc, urls, meta, _ = self._run(URLS[0], 3, 1)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        self.assertEqual(urls, [URLS[0]] * 3)
        self.assertEqual(meta["urls"], [URLS[0]])
        self.assertEqual(meta["url_count"], 1)

    def test_all_warmup_refuses_instead_of_dying_in_the_summary(self):
        """Red: IndexError out of the summary after every rep had already run."""
        proc, _, _, _ = self._run(URLS[0], 3, 3)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("all 3 reps were warmup", proc.stderr)

    def test_multi_url_refuses_less_than_two_full_cycles_of_warmup(self):
        """Red: exit 0 -- a cold host arm would have been compared to a warm VM arm."""
        proc, urls, meta, _ = self._run(",".join(URLS), 9, 5)
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertIn("two full cycles", proc.stderr)
        self.assertEqual(urls, [])
        self.assertIsNone(meta)


if __name__ == "__main__":
    unittest.main()
