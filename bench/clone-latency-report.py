#!/usr/bin/env python3
"""Summarise a bench/clone-latency.sh run.

Reads /tmp/fcvm-clone-latency-<label>/ and prints p50/min/max of the measured
spawn->exec-ready totals plus a stage breakdown parsed from each clone's
RUST_LOG=fcvm=debug timeline.

Stages are anchored on log lines emitted by `fcvm snapshot run`. Each stage is
[first line matching `start`, first line matching `end`), so the stages tile the
clone's own timeline and sum to roughly the in-process total (the difference vs
the wall-clock total is process exec + runtime init before the first log line,
reported separately as `pre-log`).
"""

import re
import statistics
import sys
from pathlib import Path

TS = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z")

# (stage name, substring that opens it, substring that closes it)
STAGES = [
    ("resolve-fc", "Cloning VM from serve PID", "connecting to memory server"),
    ("listeners+state", "connecting to memory server", "spawning namespace holder"),
    ("netns+pasta", "spawning namespace holder", "pasta + bridge setup complete"),
    ("snapshot-load", "loading snapshot with UFFD", "VM resume completed"),
    ("resume->ready", "VM resume completed", "exec server ready"),
]


def parse_ts(line):
    m = TS.match(line)
    if not m:
        return None
    text = m.group(1)
    date, _, clock = text.partition("T")
    y, mo, d = (int(x) for x in date.split("-"))
    hh, mm, rest = clock.split(":")
    sec = float(rest)
    # Day-relative seconds are enough: a clone never spans midnight.
    return ((int(hh) * 60) + int(mm)) * 60 + sec + (d * 86400)


def first_ts(lines, needle):
    for line in lines:
        if needle in line:
            t = parse_ts(line)
            if t is not None:
                return t
    return None


def main():
    out = Path(sys.argv[1])
    totals_file = out / "totals.txt"
    if not totals_file.exists():
        print(f"no totals.txt in {out}", file=sys.stderr)
        return 1
    totals = [int(x) for x in totals_file.read_text().split() if x.strip()]

    rows = []
    for log in sorted(out.glob("clone-*.log")):
        lines = log.read_text(errors="replace").splitlines()
        stamps = [t for t in (parse_ts(line) for line in lines) if t is not None]
        if not stamps:
            continue
        row = {"name": log.stem}
        first = stamps[0]
        ready = first_ts(lines, "exec server ready")
        row["in-process"] = (ready - first) * 1000 if ready else None
        for name, start, end in STAGES:
            a, b = first_ts(lines, start), first_ts(lines, end)
            row[name] = (b - a) * 1000 if (a is not None and b is not None) else None
        rows.append(row)

    def pct(vals, q):
        vals = sorted(vals)
        if not vals:
            return float("nan")
        k = max(0, min(len(vals) - 1, int(round((len(vals) - 1) * q))))
        return vals[k]

    print("=" * 68)
    print(f"clone spawn -> exec-ready   (n={len(totals)})   [{out.name}]")
    print("=" * 68)
    print(f"  p50 {pct(totals, 0.5):7.1f} ms")
    print(f"  min {min(totals):7.1f} ms")
    print(f"  max {max(totals):7.1f} ms")
    print(f"  p90 {pct(totals, 0.9):7.1f} ms")
    print(f"  mean{statistics.mean(totals):7.1f} ms")
    print()

    names = ["in-process"] + [s[0] for s in STAGES]
    print("stage breakdown (median of per-clone deltas, ms)")
    print(f"  {'stage':<18} {'p50':>8} {'min':>8} {'max':>8}")
    for name in names:
        vals = [r[name] for r in rows if r.get(name) is not None]
        if not vals:
            print(f"  {name:<18} {'n/a':>8}")
            continue
        print(
            f"  {name:<18} {pct(vals, 0.5):8.1f} {min(vals):8.1f} {max(vals):8.1f}"
        )

    inproc = [r["in-process"] for r in rows if r.get("in-process") is not None]
    if inproc and totals:
        print()
        print(
            f"  pre-log (exec+init) {pct(totals, 0.5) - pct(inproc, 0.5):6.1f} ms"
            "   (wall-clock p50 minus in-process p50)"
        )

    # Machine-readable dump for cross-run diffs.
    tsv = out / "samples.tsv"
    with tsv.open("w") as fh:
        fh.write("\t".join(["clone", "total_ms"] + names) + "\n")
        for i, r in enumerate(rows):
            total = totals[i] if i < len(totals) else ""
            cells = [r["name"], str(total)]
            cells += [
                ("" if r.get(n) is None else f"{r[n]:.1f}") for n in names
            ]
            fh.write("\t".join(cells) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
