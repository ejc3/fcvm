#!/usr/bin/env python3
"""Every bench module a Containerfile ships must bring what it imports.

The bench images COPY an explicit list of files. A module added to that list
that imports a SIBLING module not on the list dies at import time inside the
container -- and dies quietly, because entry.sh backgrounds the health checker,
so `set -eu` never fires. The container comes up, touches its ready marker, and
looks healthy while the thing that reports health is not running.

That is not hypothetical. Splitting cdp_health.py into a probe plus a shared
health_loop.py added `import health_loop` at module scope and updated only
Containerfile.webkit-bench's COPY list. Reproduced by staging exactly the set
Containerfile.chromium-bench copies:

    ModuleNotFoundError: No module named 'health_loop'

Nothing caught it. No test in bench/chromium reads a Containerfile, and CI runs
only `unittest discover` over this directory, so the image was verified by
nobody until it was built and run.

Run: python3 -m unittest test_containerfiles -v
"""

import ast
import os
import re
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
CONTAINERFILES = ("Containerfile.chromium-bench", "Containerfile.webkit-bench")


def copied_bench_files(containerfile: str) -> set[str]:
    """Basenames of bench/chromium/* files the image COPYs.

    Line continuations matter: the COPY lists span several lines with trailing
    backslashes, and reading them line-by-line finds only the first entry.
    """
    with open(os.path.join(REPO, containerfile)) as handle:
        text = handle.read()
    text = re.sub(r"\\\s*\n", " ", text)  # join continuations
    copied = set()
    for line in text.splitlines():
        if not line.strip().upper().startswith("COPY"):
            continue
        for token in re.findall(r"bench/chromium/([\w.]+)", line):
            copied.add(token)
    return copied


def local_imports(module_basename: str) -> set[str]:
    """Top-level imports of this module that resolve to a sibling .py here.

    Only module-scope imports: an import inside a function fails at call time,
    which is a different (and louder) failure than one at import time.
    """
    path = os.path.join(HERE, module_basename)
    with open(path) as handle:
        tree = ast.parse(handle.read(), filename=path)
    names = set()
    for node in tree.body:  # body only -- module scope
        if isinstance(node, ast.Import):
            names.update(a.name.split(".")[0] for a in node.names)
        elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
            names.add(node.module.split(".")[0])
    return {n for n in names if os.path.exists(os.path.join(HERE, n + ".py"))}


class ContainerfileImports(unittest.TestCase):
    def test_every_copied_module_brings_what_it_imports(self):
        """A shipped module whose sibling import is missing dies at import.

        Checked for each Containerfile independently, because the two images
        ship different sets and the failure is per-image.
        """
        for containerfile in CONTAINERFILES:
            with self.subTest(containerfile=containerfile):
                copied = copied_bench_files(containerfile)
                self.assertTrue(
                    copied,
                    f"{containerfile} COPYs no bench/chromium files; either the parser "
                    "broke or the image no longer ships the harness",
                )
                for module in sorted(m for m in copied if m.endswith(".py")):
                    for needed in sorted(local_imports(module)):
                        self.assertIn(
                            needed + ".py",
                            copied,
                            f"{containerfile} copies {module}, which imports {needed} at "
                            f"module scope, but does not copy {needed}.py. The container "
                            f"fails with ModuleNotFoundError on first import -- and "
                            f"quietly, because entry.sh backgrounds it.",
                        )

    def test_the_copy_parser_reads_continuations(self):
        """Guard the guard.

        The COPY lists wrap across lines with trailing backslashes. A parser
        that reads line-by-line sees only the first path on each COPY and
        reports a nearly empty set -- which would make the test above pass by
        finding nothing to check. This pins that the parser sees a realistic
        number of files.
        """
        for containerfile in CONTAINERFILES:
            with self.subTest(containerfile=containerfile):
                copied = copied_bench_files(containerfile)
                self.assertGreaterEqual(
                    len(copied),
                    3,
                    f"only {sorted(copied)} parsed out of {containerfile}; the COPY "
                    "parser is not following line continuations, so the import check "
                    "above is inspecting almost nothing",
                )


if __name__ == "__main__":
    unittest.main()
