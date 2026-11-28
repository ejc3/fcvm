#!/usr/bin/env python3
"""The pages-mode page's reference figures, generated from one campaign's summary.

    renders_reference.py            write fleet/web/core/renders-reference.gen.ts
    renders_reference.py --check    exit 1 unless that file is what the summary gives

The summary is bench/chromium/results/renders-*-summary.json (renders_campaign.py); there
is one in the tree, and the page shows one campaign. No figure is typed: each comes from a
regime of the summary, named by its label, and the file carries the summary's path and
sha256. A campaign, or a run the summary cites, that holds a WITHDRAWN marker gives no file. The regime labelled `held` (both sides held at the limits the module's own search
found) is the page's table and must be there.
"""

import argparse
import contextlib
import glob
import hashlib
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import campaign_summary  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
TARGET = os.path.join(REPO, "fleet", "web", "core", "renders-reference.gen.ts")
FCVM_KERNEL = re.compile(r"^\d+\.\d+\.\d+-fcvm-[0-9a-f]{12}$")
SIDES = ("fcvm", "kitesurf")
# analysis field -> the page's name
FIGURES = (("in_flight_median", "inFlight"), ("renders_per_s", "rendersPerS"), ("page_mean_ms", "pageMeanMs"),
           ("cpu_ms_per_render", "cpuMsPerRender"), ("cores_busy", "cpusBusy"), ("cpu_pressure", "cpuPressure"),
           ("renders", "renders"), ("failed", "failed"))


class Refused(Exception):
    pass


def summary_path():
    found = sorted(glob.glob(os.path.join(HERE, "results", "renders-*-summary.json")))
    if len(found) != 1:
        raise Refused(f"want one renders-*-summary.json under bench/chromium/results, found {len(found)}")
    return found[0]


def reference(summary, path, digest):
    """The page's reference as a plain value. Raises Refused."""
    if summary.get("schema") != 1:
        raise Refused(f"summary schema {summary.get('schema')!r}")
    if not FCVM_KERNEL.fullmatch(summary.get("kernel") or ""):
        raise Refused(f"kernel {summary.get('kernel')!r} is not fcvm's host kernel")
    regimes = {}
    for row in summary.get("regimes") or []:
        sides = {}
        for side in SIDES:
            values = (row.get("sides") or {}).get(side) or {}
            if not values.get("renders"):
                continue  # the side did not run in this regime
            sides[side] = {page: values.get(field) for field, page in FIGURES}
            sides[side]["limit"] = values.get("limit")
            sides[side]["pageMedianMs"] = values.get("page_median_ms") or {}
            sides[side]["setup"] = values.get("setup") or {}
        regimes[row["label"]] = {
            "run": row["run"], "ended": row.get("ended"), "windowS": (row.get("window") or {}).get("seconds"),
            "fixedLevels": row.get("fixed_levels"), "sides": sides,
            "summarySha256": ((row.get("files") or {}).get("summary.json") or {}).get("sha256"),
        }
    held = regimes.get("held")
    if not held or set(held["sides"]) != set(SIDES):
        raise Refused("the summary has no `held` regime with both sides")
    levels = held["fixedLevels"]
    if not (isinstance(levels, list) and len(levels) == 2 and all(type(n) is int and n > 0 for n in levels)
            and all(held["sides"][side]["inFlight"] == level for side, level in zip(SIDES, levels))):
        raise Refused(f"the `held` regime is not a run held at fixed levels on both sides: {levels!r}")
    for side in SIDES:
        row = held["sides"][side]
        if row["failed"] is None or not row["rendersPerS"] or row["pageMeanMs"] is None:
            raise Refused(f"the `held` regime has no figures for {side}")
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", held["ended"] or ""):
        raise Refused(f"the `held` regime's `ended` is {held['ended']!r}")
    for key in ("campaign", "instance_type", "source_revision", "kitesurf_commit"):
        if not isinstance(summary.get(key), str) or not summary[key]:
            raise Refused(f"the summary has no `{key}`")
    return {
        "campaign": summary["campaign"], "date": (held["ended"] or "")[:10],
        "instanceType": summary["instance_type"], "kernel": summary["kernel"],
        "sourceRevision": summary["source_revision"], "kitesurfCommit": summary["kitesurf_commit"],
        "summary": os.path.relpath(path, REPO), "summarySha256": digest, "regimes": regimes,
    }


@contextlib.contextmanager
def checked(path=None):
    """The reference file's text, while the campaign's directories are held: yields the text, and
    the caller writes or compares it before leaving. Raises Refused when the campaign, or a run the
    summary cites, is withdrawn (a WITHDRAWN marker, as campaign_summary.py reads it) or its
    directory is gone. The shared locks are the reader's side of the withdrawal protocol: a marker
    is either there before the summary is read, or published after this file."""
    path = path or summary_path()
    with open(path, "rb") as handle:
        raw = handle.read()
    summary = json.loads(raw)
    campaign_dir = path[:-len("-summary.json")] if path.endswith("-summary.json") else None
    runs = [row.get("run") for row in summary.get("regimes") or []] if isinstance(summary, dict) else []
    if campaign_dir is None or not all(isinstance(r, str) and os.path.basename(r) == r and r not in ("", ".", "..")
                                       for r in runs):
        raise Refused(f"{path}: not a campaign's summary beside its runs' directory")
    cited = [campaign_dir] + sorted({os.path.join(campaign_dir, run) for run in runs})
    missing = [d for d in cited if not os.path.isdir(d)]
    if missing:
        raise Refused(f"cited by the summary and absent: {', '.join(os.path.relpath(d, REPO) for d in missing)}")
    with campaign_summary.shared_run_directory_locks(cited) as lock_errors:
        if lock_errors:
            raise Refused("; ".join(lock_errors))
        for directory in cited:
            marker = os.path.join(directory, campaign_summary.WITHDRAWN_MARKER)
            if os.path.lexists(marker):
                raise Refused(f"{os.path.relpath(directory, REPO)} is withdrawn: "
                              f"{campaign_summary.withdrawal_reason(marker)}")
        value = reference(summary, path, hashlib.sha256(raw).hexdigest())
        yield ("// Generated by bench/chromium/renders_reference.py from " + value["summary"] + ": do not edit.\n"
               "// Pages mode's recorded figures: one campaign, one boot, one kernel. `regimes.held` is the\n"
               "// page's table (both sides held at the limits the module's search found).\n\n"
               "export const RENDERS_REFERENCE = " + json.dumps(value, indent=2, sort_keys=True) + " as const;\n")


def generate(path=None):
    with checked(path) as text:
        return text


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--check", action="store_true")
    opts = p.parse_args(argv)
    try:
        with checked() as text:
            if opts.check:
                try:
                    with open(TARGET) as handle:
                        now = handle.read()
                except OSError:
                    now = None
                if now != text:
                    print("fleet/web/core/renders-reference.gen.ts is not what the summary gives: "
                          "run python3 bench/chromium/renders_reference.py", file=sys.stderr)
                    return 1
                return 0
            with open(TARGET, "w") as handle:
                handle.write(text)
    except Refused as refused:
        print(f"REFUSED: {refused}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
