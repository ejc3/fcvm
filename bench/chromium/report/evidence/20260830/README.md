# August 30 exploratory records

These are historical experiments, not publication-qualified comparisons. They do
not support the report's 549.4 ms headline or replace its missing older probes.
The originals remain untouched under `/home/ubuntu/fcvm/bench/chromium/results/`.

## Retained inputs

| directory | retained | status |
|---|---|---|
| `corpusextra-hostcdp-20260830-172413` | `run.json`, `summary.json` and `hostcdp.jsonl` for the CPU-limited and unrestricted host arms | descriptive host runs only; separately timed, with no current run-id/metadata-hash binding |
| `corpusextra-memory-20260830-173915` | `memory/run.json`, `samples.jsonl`, `phase.log` | incomplete; the log ends with `BLOCKED: cputime container never became ready`; no completion summary |
| `corpusextra-memory-20260830-181830` | memory metadata, summary, raw samples, per-request CPU samples, analysis and its read-only reducer | completed exploratory measurement, not a controlled comparison |

Each directory retains its original `provenance.json`. The later memory run's
archived `harness/` holds its measurement source, the
host driver, summary reducer, captured Git revision and local diffs. These are
provenance, not another supported harness. The captured revision is
`55756858d46347f00bac3f66f1f5cacf3411bbb8`; the capture records modifications to
`hostcdp.sh` and `report.py`. Its original `SHA256SUMS` also names omitted
files; it records the original capture, not an inventory of this subset.
No claim is made that the Git revision alone reconstructs the executed tree.

Replay access/DNS logs, per-clone logs and duplicate harness files are not copied.
No retained conclusion depends on them. In particular, this archive does not
claim DNS-verified replay, a current publication-gate pass, or a complete runtime
seal. The original directories still hold the omitted files.

## What the later run cannot establish

- All fcvm cells precede all container cells. The arms are blocked in time, so
  within-block ranges cannot distinguish a side effect from drift.
- Each concurrency uses the first N URLs, so increasing N also changes the page
  mix. This is not a controlled concurrency or density ladder.
- The clone cgroup excludes the shared UFFD serve. Adding it changes the cgroup
  totals; cgroup and PSS still disagree about the same memory. Neither is a
  validated claim that fcvm uses less memory overall.
- Host-global `MemAvailable` is an unattributed diagnostic, not a ranking basis.
- The earlier memory attempt is incomplete and is never pooled with the later
  samples. Retaining it records the failure, not a successful replication.

## Recompute, without starting VMs

The archived reducer reads the retained memory/CPU records and the sibling
host summary. From the repository root:

```bash
python3 bench/chromium/report/evidence/20260830/corpusextra-memory-20260830-181830/recompute_memory_cpu.py
```

It prints the arithmetic in `memory-cpu-analysis.md`, including the blocked
schedule and accounting differences. Reproducing that arithmetic does not
remove the design limitations above. The analysis is retained to prevent those
limitations from being rediscovered or its provisional rankings being quoted
without them. The raw JSONL and per-request CPU arrays permit independent
reduction; the host summaries use `statistics.median` after resummarization.
The original analysis is copied unchanged; its paths and PR states describe
August 30. Use the command above for this archive's location.
