#!/usr/bin/env python3
"""Per-request memory of an fcvm clone, the shared memory server amortized, against a warm host container.

Reads one memory phase's raw `samples.jsonl` (corpus_mem.py, `make
bench-chromium-corpus-extra PHASES=memory`), rebuilds its cells with
corpus_mem.cell_values, and refuses unless the rebuilt cells and the fits
corpus_mem.summarize_memory_fits computes from them equal the `cells` and
`fits` of the `summary.json` beside it. Then, at each N and as N grows:

  resident equivalent (cgroup memory.current, each page charged once)
      clone      (memory server's cgroup + the N clones' cgroups) / N
      container  (the N containers' cgroups) / N
  PSS (secondary)
      clone      (memory server's PSS + the N clones' PSS) / N
      container  (the N containers' PSS) / N
  host free memory (cross-check)
      clone      (memory server's cgroup at the pre sample + fall in host
                  MemFree from the cell's pre sample to its steady samples) / N
      container  (fall in host MemFree) / N
  host available memory (cross-check): the same with MemAvailable

The memory server is started before the first cell, so the host's MemFree
already holds it at every pre sample; its cgroup is the only record of it.

Each figure is a mean over the 14 page blocks (a repetition renders one page
at every N, on both sides), so the clone and container values of a block are
paired. The memory server grows a little with N, so the clone side's slope is
fitted to the clone totals with the server included. The limit as N grows is
the difference of the two sides' least-squares slopes, where every intercept /
N goes to zero. Break-even is the N at which the server's fixed part (the
intercept of the server's own fit over N) / N equals that slope difference.
Intervals are a percentile bootstrap over whole page blocks, as corpus_mem's
fits are, but with both sides resampled together so a difference is paired
(5000 resamples), widened to include the estimate and rounded no finer than
their uncertainty as corpus_mem rounds.

Usage: python3 memory_delta.py <memory dir> [<memory dir> ...] > out.json
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import random
import statistics
import sys

import corpus_mem

RESAMPLES = 5000
CLONE = "fcvm-clone"
CONTAINER = "host-container"
BASES = ("resident", "pss", "host_free", "host_available")


def load_cells(directory):
    """corpus_mem's cell dicts (pre, steady, post) from samples.jsonl."""
    cells = {}
    with open(os.path.join(directory, "samples.jsonl")) as handle:
        for line in handle:
            row = json.loads(line)
            cell = cells.setdefault(
                (row["side"], row["n"], row["rep"]),
                {"side": row["side"], "n": row["n"], "rep": row["rep"],
                 "steady": [], "url_indices": row["url_indices"]},
            )
            if row["url_indices"] != cell["url_indices"]:
                raise SystemExit(f"{directory}: one cell names two page sets")
            if row["phase"] == "steady":
                cell["steady"].append(row)
            elif row["phase"] in ("pre", "post"):
                if row["phase"] in cell:
                    raise SystemExit(f"{directory}: two {row['phase']} samples in one cell")
                cell[row["phase"]] = row
            else:
                raise SystemExit(f"{directory}: unknown phase {row['phase']!r}")
    for key, cell in cells.items():
        if len(cell["steady"]) != 3 or "pre" not in cell or "post" not in cell:
            raise SystemExit(f"{directory}: cell {key} is incomplete")
    return cells


def check_against_summary(directory, cells, summary):
    """The rebuilt cells and fits must be the record's, or nothing here is."""
    recorded = {(c["side"], c["n"], c["rep"]): c for c in summary["cells"]}
    if set(recorded) != set(cells):
        raise SystemExit(f"{directory}: samples.jsonl and summary.json name different cells")
    ordered = []
    for key, cell in cells.items():
        values = corpus_mem.cell_values(cell)
        for name, value in values.items():
            if not math.isclose(value, recorded[key][name], rel_tol=0, abs_tol=1e-9):
                raise SystemExit(f"{directory}: cell {key} {name} {value} != {recorded[key][name]}")
        ordered.append(cell)
    seed = summary["meta"]["schedule_seed"]
    fits = corpus_mem.summarize_memory_fits(ordered, seed)
    if json.loads(json.dumps(fits)) != summary["fits"]:
        raise SystemExit(f"{directory}: fits rebuilt from samples.jsonl differ from summary.json")
    return seed


def per_cell(cell):
    """MiB totals of one cell on each basis, the shared server added on the clone side."""
    values = corpus_mem.cell_values(cell)
    free_fall = (cell["pre"]["mem_free_kb"]
                 - statistics.median(s["mem_free_kb"] for s in cell["steady"])) / 1024
    available_fall = (cell["pre"]["mem_available_kb"]
                      - statistics.median(s["mem_available_kb"] for s in cell["steady"])) / 1024
    out = {"resident": values["cgroup_mib"], "pss": values["pss_mib"],
           "host_free": free_fall, "host_available": available_fall}
    if cell["side"] == CLONE:
        out["serve_cgroup"] = values["serve_cgroup_mib"]
        out["serve_pss"] = values["serve_pss_mib"]
        out["resident_with_serve"] = values["cgroup_mib"] + values["serve_cgroup_mib"]
        out["pss_with_serve"] = values["pss_mib"] + values["serve_pss_mib"]
        # The fall in MemFree already holds whatever the server grew during the
        # cell, so it adds the server as the pre sample found it.
        out["host_free_with_serve"] = free_fall + cell["pre"]["serve_cgroup_kb"] / 1024
        # memory.stat `file`: page cache charged to the clones' cgroups.
        out["page_cache"] = statistics.median(
            s["clone_file_kb"] for s in cell["steady"]) / 1024
        out["host_available_with_serve"] = (available_fall
                                            + cell["pre"]["serve_cgroup_kb"] / 1024)
    else:
        out["resident_with_serve"] = out["resident"]
        out["pss_with_serve"] = out["pss"]
        out["host_free_with_serve"] = out["host_free"]
        out["host_available_with_serve"] = out["host_available"]
    return out


def interval(estimate, draws):
    draws = sorted(draws)
    lower = min(corpus_mem.percentile(draws, 0.025), estimate)
    upper = max(corpus_mem.percentile(draws, 0.975), estimate)
    return lower, upper


def rounded(estimate, draws):
    lower, upper = interval(estimate, draws)
    value, ci, step = corpus_mem.uncertainty_rounding(estimate, lower, upper)
    return {"estimate": value, "ci95": ci, "rounding": step,
            "unrounded": estimate, "unrounded_ci95": [lower, upper]}


def statistics_for(blocks, ns):
    """Every published statistic from one set of page blocks (resampled or not).

    blocks: list of {side: {n: per_cell dict}}.
    """
    out = {}
    for basis in BASES:
        key = basis + "_with_serve"
        for n in ns:
            clone = statistics.mean(b[CLONE][n][key] / n for b in blocks)
            container = statistics.mean(b[CONTAINER][n][key] / n for b in blocks)
            out[(basis, n, "clone")] = clone
            out[(basis, n, "container")] = container
            out[(basis, n, "delta")] = clone - container
        slopes = {}
        # MemFree and MemAvailable falls already hold the server's growth during
        # a cell; the cgroup and PSS totals get it from the server's sample.
        slope_key = basis if basis.startswith("host_") else key
        for side in (CLONE, CONTAINER):
            xs = [n for b in blocks for n in ns]
            ys = [b[side][n][slope_key] for b in blocks for n in ns]
            slope, _intercept = corpus_mem.slope_intercept(xs, ys)
            slopes[side] = slope
        out[(basis, "limit", "clone")] = slopes[CLONE]
        out[(basis, "limit", "container")] = slopes[CONTAINER]
        out[(basis, "limit", "delta")] = slopes[CLONE] - slopes[CONTAINER]
    xs = [n for b in blocks for n in ns]
    for basis, field in (("resident", "serve_cgroup"), ("pss", "serve_pss")):
        out[("serve", basis)] = statistics.mean(b[CLONE][n][field] for b in blocks for n in ns)
        slope, intercept = corpus_mem.slope_intercept(
            xs, [b[CLONE][n][field] for b in blocks for n in ns])
        out[("serve_fixed", basis)] = intercept
        out[("serve_per_clone", basis)] = slope
    for n in ns:
        out[("clone_page_cache", n)] = statistics.mean(
            b[CLONE][n]["page_cache"] / n for b in blocks)
    for basis in ("resident", "pss"):
        serve = out[("serve_fixed", basis)]
        gap = out[(basis, "limit", "delta")]
        out[("break_even", basis)] = serve / gap if gap > 0 else math.inf
    return out


def analyze(directory):
    with open(os.path.join(directory, "summary.json")) as handle:
        summary = json.load(handle)
    cells = load_cells(directory)
    seed = check_against_summary(directory, cells, summary)
    ns = sorted({n for _side, n, _rep in cells})
    reps = sorted({rep for _side, _n, rep in cells})
    blocks = []
    for rep in reps:
        block = {CLONE: {}, CONTAINER: {}}
        for n in ns:
            clone, container = cells[(CLONE, n, rep)], cells[(CONTAINER, n, rep)]
            if clone["url_indices"] != container["url_indices"]:
                raise SystemExit(f"{directory}: N={n} rep={rep} rendered different pages per side")
            block[CLONE][n] = per_cell(clone)
            block[CONTAINER][n] = per_cell(container)
        blocks.append(block)

    observed = statistics_for(blocks, ns)
    material = f"{seed}:memory_delta:{summary['run_id']}".encode()
    rng = random.Random(int.from_bytes(hashlib.sha256(material).digest()[:8], "big"))
    draws = {key: [] for key in observed}
    for _ in range(RESAMPLES):
        sample = [rng.choice(blocks) for _block in blocks]
        for key, value in statistics_for(sample, ns).items():
            draws[key].append(value)

    result = {
        "record": directory,
        "run_id": summary["run_id"],
        "render_op": summary["meta"]["render_op"],
        "container_render": summary["meta"]["container_render"],
        "page_blocks": len(reps),
        "ns": ns,
        "statistic": "mean over page blocks of per-instance MiB; limit is the "
                     "difference of least-squares slopes over N, the clone side's "
                     "fitted with the memory server included",
        "uncertainty": {"method": "page-block bootstrap, both sides resampled together",
                        "resamples": RESAMPLES, "confidence": 0.95},
        "bases": {
            "resident": "cgroup memory.current; clone side adds the memory server's cgroup",
            "pss": "PSS over the same process sets; clone side adds the memory server's PSS",
            "host_free": "fall in host MemFree from the cell's pre sample to the median of "
                         "its steady samples; clone side adds the memory server's cgroup "
                         "at the pre sample",
            "host_available": "the same with host MemAvailable, which leaves out what the "
                              "kernel counts as reclaimable",
        },
        "serve_mean_mib": {
            basis: rounded(observed[("serve", basis)], draws[("serve", basis)])
            for basis in ("resident", "pss")},
        "serve_fit_mib": {
            basis: {"fixed": rounded(observed[("serve_fixed", basis)],
                                     draws[("serve_fixed", basis)]),
                    "per_clone": rounded(observed[("serve_per_clone", basis)],
                                         draws[("serve_per_clone", basis)])}
            for basis in ("resident", "pss")},
    }
    for basis in BASES:
        table = {}
        for n in ns + ["limit"]:
            table[str(n)] = {
                part: rounded(observed[(basis, n, part)], draws[(basis, n, part)])
                for part in ("clone", "container", "delta")
            }
        result[basis] = table
    result["clone_page_cache_mib_per_clone"] = {
        "field": "memory.stat file of the clones' cgroups (clone_file_kb, page cache "
                 "and shmem), per clone",
        **{str(n): rounded(observed[("clone_page_cache", n)],
                           draws[("clone_page_cache", n)]) for n in ns},
    }
    result["break_even_n"] = {}
    for basis in ("resident", "pss"):
        key = ("break_even", basis)
        finite = [d for d in draws[key] if math.isfinite(d)]
        entry = {"definition": "memory server's fitted fixed part / (clone slope with "
                               "the server - container slope)",
                 "resamples_with_no_positive_gap": len(draws[key]) - len(finite)}
        if math.isfinite(observed[key]) and len(finite) == len(draws[key]):
            entry.update(rounded(observed[key], draws[key]))
        else:
            entry["estimate"] = observed[key] if math.isfinite(observed[key]) else None
            entry["ci95"] = None
            entry["note"] = "the slope gap is not positive in every resample, so no interval"
        result["break_even_n"][basis] = entry
    return result


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    json.dump([analyze(d.rstrip("/")) for d in argv[1:]], sys.stdout, indent=1)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
