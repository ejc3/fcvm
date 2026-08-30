# Memory and CPU: fcvm clones against host containers, corpus run of 2026-08-30

Every figure here is recomputed from the raw records in this directory by
`recompute_memory_cpu.py`, which sits beside this file and prints each number
this document quotes, including the ones only used in prose. Nothing is carried
over from an earlier write-up; four of its claims are corrected at the end.

"What the run design limits" comes before the tables because two properties of
the run bound what any of them is evidence for. The arithmetic itself reproduces
exactly; the confounds were simply not disclosed before.

## Convention

- **p50 is `statistics.median`.** `bench/chromium/reqanalyze.py`, which
  publishes the wall-clock arm's medians, computes every one of them with
  `statistics.median` (lines 139 and 145 for a median and its bootstrap CI,
  1682 and 1687 for a difference of medians). The sibling record
  `corpusextra-hostcdp-20260830-172413/hostcdp-cpu2/summary.json` carries
  `"p50_convention": "statistics.median"` because the memory run's
  `harness/resummarize.py` recomputed that summary under the same convention
  after the fact; the measuring script did not write the field at the time.
  Memory cells have three reps each, so their p50 is the middle of three, and
  the min and max are printed beside every one.
- **A mean is called a mean and is only compared against a mean.** Where a
  record holds a total and a count but no distribution, a mean is the only
  statistic that exists.

## What was measured

`memory/run.json` fixes the cell: snapshot `cb-req-corpus`, image
`localhost/chromium-bench-req` (`334fc21f7c8c...`), `uffd_mode` `minor`,
`uffd_prefetch` `on`, N in {1, 2, 4, 8}, 3 reps per cell, a 14-URL corpus,
`loadavg1_at_start` 0.97, host kernel `6.17.0-1019-aws`, aarch64.

`memory/run.json`'s own basis string:

> cgroup memory.current and PSS summed over EXACTLY that cgroup's process set,
> on both sides: an fcvm clone's leaf cgroup holds fcvm, firecracker, the
> namespace holder and pasta; a container's cgroup is podman's own.
> MemAvailable delta from a quiesced pre-sample is recorded beside them as an
> attribution-free check.

Every instance renders one corpus page before it is sampled.

## What the run design limits

Two properties of the run bound every across-N reading below. Both come from the
records, and neither was stated in the earlier write-up.

**The N axis mixes instance count with workload.**
`harness/corpus_mem.py` gives instance `i` of a cell the url
`urls[i % len(urls)]` for `i in range(n)` (lines 290 and 397, the same rule on
both sides). So the cell at N renders `urls[0..N-1]`, a different mix at every N:

| N | urls rendered | mean host warm p50 of that set |
|---|---|---|
| 1 | `example.com` | 137.3 ms |
| 2 | + `news.ycombinator.com` | 156.8 ms |
| 4 | + both cloudflare docs sites | 238.7 ms |
| 8 | + wikipedia, MDN, **`elmundo.es`**, **`rtp.pt`** | **654.3 ms** |

(per-URL times from `corpusextra-hostcdp-20260830-172413/hostcdp-cpu2/summary.json`,
`per_url[].p50_ms`.) The two heaviest pages in the corpus enter at the 4-to-8
step. They are the same two the CPU section below names as the outliers, at
6887.4 ms and 5236.6 ms of CPU against 980.7 ms for `example.com`. So every
statement of the form "cost per instance rises with N" or "the marginal cost
crosses at the last step" is confounded with a 2.7x heavier page set, and this
run cannot separate the two.

The per-N comparison **between the sides** is not affected: both sides use the
same index-to-url rule, so at each N the two sides render the same set.

**The two sides are blocked in time, not interleaved.** Sorting
`memory/samples.jsonl` by `ts` gives exactly one side switch: all twelve fcvm
cells run at t = 0.0 to 234.1 s, all twelve container cells at t = 238.2 to
497.3 s. The three reps of a cell therefore all sit inside that side's block, so
the `[min-max]` ranges below measure within-block variation only and cannot
separate a side effect from drift across the run.
`bench/chromium/AGENTS.md` names this failure as its methodology defect 2
("Interleave, never block"), and `reqbench.py` interleaves its arms per request
for exactly this reason. This run does not.

## Memory, per instance, as recorded

MiB per instance, p50 over the 3 reps, `[min-max]` beside it. From
`memory/summary.json`, `cells[]`, divided by `instances_counted`.

### cgroup `memory.current`

| N | fcvm clone | host container | fcvm minus container | lower |
|---|---|---|---|---|
| 1 | 231.5 `[225.5-238.0]` | 315.6 `[314.5-317.2]` | -84.1 (-26.6%) | fcvm |
| 2 | 232.6 `[229.8-235.2]` | 312.7 `[310.3-313.2]` | -80.1 (-25.6%) | fcvm |
| 4 | 272.1 `[267.1-277.2]` | 325.8 `[325.7-325.8]` | -53.7 (-16.5%) | fcvm |
| 8 | 398.8 `[396.5-403.3]` | 381.3 `[381.2-387.9]` | **+17.5 (+4.6%)** | **container** |

### PSS over the same process set

| N | fcvm clone | host container | fcvm minus container | lower |
|---|---|---|---|---|
| 1 | 579.1 `[566.6-584.2]` | 526.6 `[525.1-528.2]` | **+52.6 (+10.0%)** | **container** |
| 2 | 358.3 `[357.6-365.8]` | 407.6 `[407.3-408.5]` | -49.3 (-12.1%) | fcvm |
| 4 | 280.3 `[275.7-286.4]` | 362.8 `[362.6-362.8]` | -82.5 (-22.7%) | fcvm |
| 8 | 327.7 `[321.5-329.1]` | 388.2 `[387.9-394.6]` | -60.5 (-15.6%) | fcvm |

### MemAvailable delta

Signed difference only. This column does not vote on which side is lower; see
"The MemAvailable column" below.

| N | fcvm clone | host container | fcvm minus container |
|---|---|---|---|
| 1 | 130.1 `[98.4-167.9]` | 296.8 `[275.8-396.6]` | -166.7 |
| 2 | 162.1 `[147.0-165.9]` | 335.5 `[278.9-365.3]` | -173.5 |
| 4 | 190.3 `[177.3-192.0]` | 345.0 `[344.5-345.3]` | -154.6 |
| 8 | 292.0 `[284.2-300.1]` | 402.7 `[387.9-408.7]` | -110.7 |

In every cell the two sides' `[min-max]` ranges are disjoint, so each row's
ordering holds across all three reps of this run. Given the blocked design that
is a statement about three consecutive samples within one block, not about three
independent draws.

## The shared UFFD serve is excluded from the attributed bases too

The clone's leaf cgroup holds fcvm, firecracker, the namespace holder and pasta.
The UFFD serve that every clone restores from is a separate cgroup, so
`cells[].cgroup_mib` and `cells[].pss_mib` both exclude it, and the MemAvailable
baseline excludes it as well. `memory/summary.json` records it per cell:

| N | `serve_cgroup_mib` p50 | `serve_pss_mib` p50 |
|---|---|---|
| 1 | 950.1 | 9.6 |
| 2 | 953.2 | 9.2 |
| 4 | 958.1 | 9.9 |
| 8 | 969.4 | 13.0 |

The container side has no counterpart. The CPU section below charges fcvm for
the shared serve (`+110.7 ms` per request), so the memory section has to show
the same charge rather than only mention it:

| N | cgroup: fcvm bare | + serve/N | fcvm + serve | container | lower with serve |
|---|---|---|---|---|---|
| 1 | 231.5 | 950.1 | 1181.7 | 315.6 | container |
| 2 | 232.6 | 476.6 | 709.2 | 312.7 | container |
| 4 | 272.1 | 239.5 | 511.7 | 325.8 | container |
| 8 | 398.8 | 121.2 | 520.0 | 381.3 | container |

| N | PSS: fcvm bare | + serve/N | fcvm + serve | container | lower with serve |
|---|---|---|---|---|---|
| 1 | 579.1 | 9.6 | 588.7 | 526.6 | container |
| 2 | 358.3 | 4.6 | 362.9 | 407.6 | fcvm |
| 4 | 280.3 | 2.5 | 282.8 | 362.8 | fcvm |
| 8 | 327.7 | 1.6 | 329.3 | 388.2 | fcvm |

Counting the serve, the container is cheaper at every N on the cgroup basis. The
PSS orderings do not move, because PSS charges the serve 9.2 to 13.0 MiB where
`memory.current` charges it 950.1 to 969.4 MiB. The two bases disagree about the
same component by two orders of magnitude: the serve's cgroup charge is
overwhelmingly page cache from mapping the snapshot memory image, and its PSS is
what the serve process privately holds. These records do not settle which of
those a machine should be said to pay for, so they do not settle whether the
serve belongs in the per-instance number. What they do settle is that the answer
decides the cgroup ordering at every N, and that leaving it out is a choice, not
a neutral default.

## Where the bases agree and where they do not

| N | cgroup | PSS | attributed bases |
|---|---|---|---|
| **as recorded, serve excluded** | | | |
| 1 | fcvm | container | **disagree** |
| 2 | fcvm | fcvm | agree |
| 4 | fcvm | fcvm | agree |
| 8 | container | fcvm | **disagree** |
| **with the serve amortised over N** | | | |
| 1 | container | container | agree |
| 2 | container | fcvm | **disagree** |
| 4 | container | fcvm | **disagree** |
| 8 | container | fcvm | **disagree** |

The two attributed bases agree on an fcvm advantage at exactly two of the eight
(N, accounting) combinations, and only when the shared serve is left out. There
is no accounting under which both attributed bases put the clone lower at every
N.

Why the two bases can part company is visible in the tables. At N=1 the clone's
PSS (579.1) is 2.50 times its own cgroup charge (231.5): PSS counts every page a
process maps, divided by the number of processes mapping it, regardless of which
cgroup is charged, and the serve's ~950 MiB mapping is in that set. Per-instance
PSS then falls to 358.3 at N=2 and 280.3 at N=4 while the cgroup charge moves
from 231.5 to 272.1, which is what a shared component divided among more mappers
looks like. But the series does not keep falling: at N=8 fcvm PSS per instance
turns back up to 327.7, and so does the container's (526.6, 407.6, 362.8,
**388.2**). The division reading is supported over N in {1, 2, 4} and reverses at
the fourth point, in a cell whose workload is also 2.7x heavier. These records do
not decompose PSS by mapping, so it stays an inference from the shape of a series
with a confounded x-axis.

## Marginal cost of one more instance

Totals in MiB (p50 over reps), then the marginal cost of each added instance
between adjacent cells. The bracket is the envelope from the rep extremes,
`(min(later) - max(earlier))` to `(max(later) - min(earlier))`, per added
instance.

| basis | side | N=1 | N=2 | N=4 | N=8 | 1→2 | 2→4 | 4→8 |
|---|---|---|---|---|---|---|---|---|
| cgroup | fcvm | 231.5 | 465.2 | 1088.5 | 3190.4 | 233.7 `[221.5,245.0]` | 311.6 `[299.0,324.6]` | **525.5 `[515.9,539.5]`** |
| cgroup | container | 315.6 | 625.4 | 1303.2 | 3050.1 | 309.9 `[303.4,311.9]` | 338.9 `[338.2,341.4]` | **436.7 `[436.5,450.2]`** |
| PSS | fcvm | 579.1 | 716.6 | 1121.2 | 2621.3 | 137.5 `[131.0,165.1]` | 202.3 `[185.6,215.1]` | 375.0 `[356.7,382.6]` |
| PSS | container | 526.6 | 815.1 | 1451.0 | 3105.3 | 288.6 `[286.4,291.8]` | 317.9 `[316.7,318.4]` | 413.6 `[412.9,426.5]` |
| MemAvailable | fcvm | 130.1 | 324.1 | 761.4 | 2336.2 | 194.0 `[126.1,233.3]` | 218.6 `[188.7,237.0]` | 393.7 `[376.3,422.9]` |
| MemAvailable | container | 296.8 | 671.0 | 1380.0 | 3221.7 | 374.2 `[161.2,454.7]` | 354.5 `[323.7,411.7]` | 460.4 `[430.5,473.0]` |

Every ordering in the table survives its envelope. On PSS the added clone costs
less than the added container at all three steps, with the gap narrowing. On
cgroup the added clone costs less at the first two steps and more at the last,
525.5 against 436.7. That crossing is a different claim from the N=8 row of the
cgroup table: the row says the fcvm total exceeds the container total at N=8,
the crossing says the fcvm increment exceeds the container increment over 4 to 8.
Neither implies the other, and here both happen to hold. Both statements share
the workload confound, since the 4-to-8 step is also where the two heaviest pages
enter.

`memory/summary.json`'s `fits` block reports a least-squares slope and intercept
per series. The slope is a poor summary of any of these six series, not just the
ones with a negative intercept: recomputing residuals from `fits[].points` shows
all six carry the same curvature signature, mean residual positive at N=1,
negative at N=2 and N=4, positive at N=8. R² runs 0.9725 to 0.9945, and the
**lowest** of the six is fcvm PSS (0.9725), whose intercept is positive. Use the
stepwise marginals above for every basis.

## The MemAvailable column, and why it cannot referee

`run.json` calls this basis "attribution-free". It is free of per-process
attribution. It is not neutral between the two sides.

From `memory/samples.jsonl`, phase `pre`:

- All 12 fcvm pre-samples are taken with the UFFD serve already running:
  `serve_procs` 1, `clones` 0, `serve_cgroup_kb` between 969,348 and 989,940 kB
  (946.6 to 966.7 MiB, p50 948.4 MiB), `serve_pss_kb` 11.3 to 18.8 MiB.
- All 12 container pre-samples have `pool_containers` 0 and `pool_cgroup_kb` 0.

The delta is `pre` MemAvailable minus `steady` MemAvailable
(`harness/corpus_mem.py`, `avail_delta`). Whatever the serve's residency costs
MemAvailable is charged before the window opens and subtracted back out of the
fcvm delta; the container side has no comparable pre-charged component. Two more
facts from the records, both against reading an ordering out of this column:

- It is the noisiest of the three at low N. Within-cell spread (max minus min
  over 3 reps, per instance) at N=1 is 120.8 MiB on the container side against a
  p50 of 296.8, and 69.5 MiB on the fcvm side against a p50 of 130.1. The cgroup
  and PSS spreads in the same cells are 2.7 and 3.1 MiB (container) and 12.5 and
  17.7 MiB (fcvm).
- It disagrees with PSS by different amounts on the two sides. At N=1, PSS minus
  MemAvailable is +449.0 MiB on the fcvm side and +229.7 MiB on the container
  side. The 219.3 MiB difference between those two gaps exceeds the largest
  side-to-side difference anywhere in the tables above (173.5 MiB).

What these records do not show is the serve costing MemAvailable much: the two
sides' pre-sample MemAvailable levels differ by 183.1 MiB (fcvm higher, p50
380591.4 MiB against 380408.3 MiB, on a box holding about 371.7 GiB available).
That is consistent with most of the serve's cgroup charge being reclaimable page
cache that MemAvailable still counts as available. It is not evidence either
way, because MemAvailable with the serve absent was never sampled.

The column stays in the record. No conclusion in this document rests on it.

## CPU time

From `memory/cputime.json`. The two sides' bases, quoted from their own records:

- fcvm, `cputime.json.fcvm.basis`: *"leaf cgroup usage_usec over one whole clone
  lifecycle (spawn, restore, render, teardown)"*. n=42.
- host, `cputime.json.host.basis`: *"container cgroup usage_usec differenced
  across the renders, divided by the renders; includes the container's idle cost
  between them"*. n=42.

Both sides ran the same schedule, `urls[i % 14]` for i in 0..41, three passes of
the 14-URL corpus in the same order (`harness/corpus_mem.py`, `run_cputime`). So
unlike the memory cells, the CPU arms are matched on workload.

| quantity | value | source |
|---|---|---|
| fcvm per-lifecycle CPU, p50 (`statistics.median`) | **1486.0 ms** | recomputed from `cputime.json.fcvm.records[].cpu_ms` |
| fcvm per-lifecycle CPU, mean | **2149.0 ms** | `cputime.json.fcvm.per_request_cpu_ms_mean` |
| fcvm min / max | 979.6 / 6898.7 ms | `cputime.json.fcvm.min`, `.max` |
| UFFD serve, per request | 110.7 ms | `cputime.json.fcvm.serve_cpu_ms_per_request` |
| host per-render CPU, mean | **733.1 ms** | `cputime.json.host.total_cpu_ms` 30788.8 / `n` 42 |
| host per-render CPU, median | not derivable | `cputime.json.host` holds a total and a count, no `records[]` |

The only like-for-like ratio these records support is mean against mean:

```
2149.0 / 733.1 = 2.93x                      clone lifecycle vs warm container render
(2149.0 + 110.7) / 733.1 = 3.08x            same, counting the shared UFFD serve
```

The fcvm distribution is right-skewed (p50 1486.0, mean 2149.0, max 6898.7,
driven by `elmundo.es` at a 6887.4 ms per-URL median and `rtp.pt` at 5236.6 ms).
A median would summarise it better and cannot be used here, because the host side
has no median to put it against.

Neither 2.93x nor 3.08x is a render-cost ratio, and the two known asymmetries
push it in opposite directions:

- The host denominator includes the container's idle time between renders (its
  own basis string says so), which inflates it. Removing it would raise the
  ratio.
- The host excludes container startup entirely (two warmup renders run before
  the differenced window opens), while every fcvm number includes a fresh spawn,
  snapshot restore and teardown. Removing those would lower the ratio.

What is measured is one cold clone per request against one warm container reused
across 42 requests, which is what `run_cputime`'s docstring in
`harness/corpus_mem.py` says it is after: *"which is what a warm pool actually
pays"*.

## Conclusions the records support

1. **Memory, per instance, with the shared serve left out of both attributed
   bases as the harness records them:** the clone is cheaper than the container
   at N=2 and N=4 on both (cgroup -25.6% and -16.5%, PSS -12.1% and -22.7%), and
   the bases disagree at both endpoints. PSS puts the clone dearer at N=1 (579.1
   against 526.6, +10.0%); cgroup puts it dearer at N=8 (398.8 against 381.3,
   +4.6%). No basis puts the clone lower at every N.
2. **Memory, per instance, charging fcvm for the shared UFFD serve** the way the
   CPU figures do: the container is cheaper at every N on the cgroup basis
   (1181.7, 709.2, 511.7, 520.0 against 315.6, 312.7, 325.8, 381.3), and the PSS
   orderings are unchanged because PSS charges the serve 9.2 to 13.0 MiB rather
   than 950.1 to 969.4 MiB. Which of those two the serve "costs" is not settled
   by these records, and the answer decides the cgroup result.
3. **Memory, marginal:** on PSS each added clone costs less than each added
   container at all three steps, 137.5 against 288.6, 202.3 against 317.9, 375.0
   against 413.6 MiB, every ordering surviving its rep envelope. On cgroup the
   last step reverses, 525.5 against 436.7.
4. **Both across-N readings above are confounded** with the page set, which gets
   2.7x heavier from N=4 to N=8, in the same step where the cgroup marginal
   crosses. The per-N side comparison is not.
5. **CPU:** a full fcvm clone lifecycle costs a mean 2149.0 ms per request
   against a warm container's mean 733.1 ms per render, 2.93x, or 3.08x counting
   the shared serve. That compares a cold clone per request against a reused warm
   container, not two renders.

The short version: on this run, at N=2 and N=4, a clone's own cgroup and PSS are
below a container's; that advantage does not hold at both ends of the range on
both bases, it does not survive charging fcvm for the serve on the cgroup basis,
and the across-N shape cannot be read as scaling because the workload changes
with N.

## What this measurement does not license

- **Reading N as a scale axis.** Cell N renders `urls[0..N-1]`, so an across-N
  change mixes instance count with a workload 2.7x heavier at N=8 than at N=4.
- **Separating a side effect from wall-clock drift.** All fcvm cells ran in one
  block (t=0.0 to 234.1 s) and all container cells in the next (t=238.2 to
  497.3 s). One side switch in the whole run.
- **A per-instance memory number that is neutral on the shared serve.** Both
  attributed bases exclude it; including it flips the cgroup result at every N;
  the CPU figures include the analogous charge.
- **Any statistical confidence.** Three reps per memory cell, all inside one
  block. The `[min-max]` ranges are the entire evidence of spread.
- **Any CPU-time conversion to a published figure.** `run_cputime`'s docstring in
  `harness/corpus_mem.py`: *"It is still measured on a different machine from any
  such figure, so it licenses no conversion."*
- **A render-cost comparison in either direction.** The fcvm CPU number is a
  whole clone lifecycle; the host CPU number is renders plus idle with startup
  excluded.
- **A median-against-median CPU ratio.** `cputime.json.host` has no per-render
  records. Anyone who wants one has to re-measure with the host cgroup read
  between renders.
- **A per-URL CPU comparison.** Only the fcvm side records per-URL CPU; the
  per-URL numbers quoted above are one-sided descriptions of that distribution.
- **Any claim that one memory basis is the correct one.** The attributed bases
  order the sides differently, and nothing recorded here breaks the tie.
- **Any ordering claim from MemAvailable.** Asymmetric baseline, largest spread
  of the three at low N, and a side-dependent offset from PSS of 219.3 MiB at
  N=1.
- **Extrapolation past N=8**, or to another snapshot, image, guest memory size,
  UFFD mode, or prefetch setting. `run.json` fixes each to one value.
- **A density claim of the form "N clones cost only what they faulted".** These
  records contain no fault counts.

## Corrections to the earlier write-up

Four claims are withdrawn. Each was that write-up contradicting its own data, not
a measurement problem. The write-up itself is not in this repository; the
sentences below are reproduced from the review that flagged them, so only the
corrected values are checkable against the records.

**1. "This third basis agrees with PSS in ordering at every N and puts fcvm
lower than the container throughout."**

False. At N=1 PSS ranks the clone **higher** (579.1 against the container's
526.6) while MemAvailable ranks it lower (130.1 against 296.8). The same
disagreement holds under the mean that write-up used: 576.6 against 526.6 on
PSS, 132.1 against 323.1 on MemAvailable. The corrected statement is the
agreement table above.

**2. "MemAvailable measured on a quiesced box ... as an attribution-free third
view", "consistent with the serve's page cache being counted once for the
machine rather than once per instance."**

Counted **zero** times, not once: the serve is resident at 946.6 to 966.7 MiB in
every fcvm pre-sample, so it is inside the baseline the delta is taken from. The
same test applies to the two attributed bases, which the write-up did not check:
the serve is outside the summed leaf cgroup as well, at 950.1 to 969.4 MiB by
`memory.current` and 9.2 to 13.0 MiB by PSS. Including it flips the cgroup
ordering at every N. That correction is in "The shared UFFD serve" section, and
Conclusion 2 states it.

**3. CPU: a median compared against a mean.**

The earlier text set "an fcvm clone costs 1643.5 ms of CPU at the median"
against "a warm host container costs 733.1 ms of CPU per render", implying
2.24x. The 733.1 is `total_cpu_ms / n`, a mean by `cputime.json.host.basis`'s own
words. Corrected, mean against mean: **2149.0 / 733.1 = 2.93x**, or **3.08x**
with the serve's 110.7 ms per request.

**4. CPU: a non-median reported as a median.**

1643.5 is `sorted[21]` of 42, the upper middle element:
`harness/corpus_mem.py` computes the field as `round(vals[len(vals) // 2], 1)`
on an already-sorted list, which is not a median for even n. The sorted
neighbours are 1328.5 and 1643.5, so `statistics.median` is **1486.0**. The
wall-clock arm publishes medians through `reqanalyze.py`'s `statistics.median`,
and this figure departed from that silently. The mislabelled field is still in
`memory/cputime.json` as `per_request_cpu_ms_p50: 1643.5`; the record is left as
it was recorded. `corpus_mem.py` has since been landed and now computes that
field with `statistics.median`, so later runs write 1486.0 for this case.

Two ratios that should not be quoted, for the record: 1643.5/733.1 = 2.24x
(mislabelled median against a mean) and 1486.0/733.1 = 2.03x (true median against
a mean). Neither is like against like.

## Provenance

`provenance.json` records `git_head` `55756858`, and beside it
`"git_dirty": " M bench/chromium/hostcdp.sh; M bench/chromium/report.py;"`, so
the head alone does not pin what ran. It also carries the sha256 of `fcvm` and of
each script.

`harness/` holds frozen copies of the scripts, with `SHA256SUMS` and `GIT_HEAD`.
It is not the complete set: `corpus_mem.py` renders through `cdpdrive.py`
(`CDPDRIVE = os.path.join(HERE, "cdpdrive.py")`), and `cdpdrive.py`, `render.py`
and `corpus_serve.py` are absent from `harness/`. They are hashed in
`provenance.json` and are byte-identical to `origin/main`, so they remain
recoverable.

Of the frozen scripts, `hostcdp.sh` and `test_hostcdp_corpus.py` have been sent
to the tree on branch `hostcdp-corpus-urls` (PR #892, merged into `main` as
`8b237e45` at 2026-08-30T19:50:53Z, two and a half minutes after this file was
written; the sentence here said "open, not merged" and was corrected when the
run was landed). The branch's first
commit carries `hostcdp.sh` byte-identical to the frozen copy, hashing to the
`hostcdp.sh` value in `provenance.json`
(`7ea77ec7e25759d9dcd955a968a71eff9c44dbd83acbbd63e982e8c973ba1d5c`); later
commits on that branch change it further in response to review, and its
`test_hostcdp_corpus.py` is a superset of the frozen copy. `corpus_mem.py`,
`compare.py`, `corpus_extra.sh`, `resummarize.py` and the `report.py` change in
`harness/report.py.diff` (walking the cgroup subtree, without which rootless
podman's nested container cgroup returns an empty pid list and the container
side's PSS silently comes back 0) existed only in this frozen copy when this was
written, and have since been landed under `bench/chromium/`. The landed copies
carry fixes this frozen copy does not; `harness/SHA256SUMS` names the bytes that
produced the numbers above, and those are the copies here.

`recompute_memory_cpu.py` was written after the run and is not in `SHA256SUMS` or
`provenance.json`. It reads the records and writes nothing.

## Reproducing every number

```
python3 bench/chromium/results/corpusextra-memory-20260830-181830/recompute_memory_cpu.py
```

It reads `memory/summary.json`, `memory/samples.jsonl`, `memory/cputime.json`,
`memory/run.json` and the sibling hostcdp `summary.json`, and prints every
figure this document quotes: the per-instance tables, the same tables with the
serve amortised, the ordering agreement under both accountings, the totals and
marginals with their envelopes and fit residuals, the per-cell URL sets, the
time-blocking of the two sides, the pre-sample facts, the within-cell spreads,
the between-basis gaps, the percentages and ratios used in prose, and the CPU
figures including the ratios that should not be quoted.
