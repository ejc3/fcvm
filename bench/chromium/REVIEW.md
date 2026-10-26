# Adversarial review ledger — chromium shared-nothing bench

**Quote only from sealed runs that passed their gates and were never withdrawn.**
When this ledger was written (2026-08-08) three runs existed and the only quotable
one was `20260808-corrected`; every later reqbench run joins the record under the
same rule once reqanalyze marks it publishable (every gate in its analysis.json
passes: at least 200 measured requests per CDP arm, zero failures over every attempt
including warmups, the no-render arm's first-half to second-half shift with a 95%
confidence interval inside ±10 ms, and the stall and teardown gates), its generation
and runtime-bundle seals verify, and, for a corpus run, dns-evidence.json records a
clean verdict. The no-render arm stops when the forwarded port accepts, so the drift
gate says nothing about the render stages. Withdrawn runs stay unquotable forever;
the table below records why.

| run | date | verdict |
|---|---|---|
| `20260807-full` | 2026-08-07 | comparative numbers **retracted** on methodology (six defects) |
| `results/20260808-corrected` | 2026-08-08 | six defects corrected; numbers below are the current record |
| `reqbench` CDP A/B | 2026-08-08 | **WITHDRAWN IN FULL** — see "The CDP-path A/B" below. Harness defects invalidate every figure it produced. |
| `results/reqbench-20260816-*-corpus` (14 runs), `results/reqbench-20260814-042319-uffd`, `results/cpuprobe-20260816` | 2026-08-14 to 2026-08-16 | **WITHDRAWN IN FULL** on 2026-09-02, live-DNS contamination; see "The 2026-08-16 corpus series" below. Each directory carries a `WITHDRAWN` marker with its evidence and stays in the tree. `results/campaign-20260816-summary.json` is a hand-written index of these cells, not a record. |
| `results/reqbench-20260830-171007-corpus` | 2026-08-30 | DNS-verified corpus record (`dns-evidence.json` verdict clean); 712.6 ms [610.5, 808.5] cdp p50 at 2 vCPU. Still verified; the headline moved to the 4 vCPU rung of the ladder below. |
| `results/reqbench-20260902-023115-corpus-c2`, `results/reqbench-20260902-025115-corpus-c4`, `results/reqbench-20260902-031115-corpus-c8` | 2026-09-02 | DNS-verified 2/4/8 vCPU ladder, one golden per rung on one runtime bundle (each `dns-evidence.json` verdict clean, first_mismatch null, diag violations none); cdp p50 770.3 [596.2, 807.8], 549.4 [467.9, 632.4] and 580.2 [520.8, 645.9] ms, n=202 each. 549.4 at 4 vCPU is the current corpus headline. Each rung ran once: from 2 to 4 vCPUs the median fell 220.9 ms, the 4 and 8 vCPU medians each sit inside the other's interval, and the stage where 8 loses (the wait for the target, +53.1 ms in the mean) moves in 50 ms poll steps, so these runs do not show that 4 is faster than 8 or locate a knee. Indexed by `results/campaign-20260902-box2-ladder-summary.json`. |
| `results/scale-ceiling-20261026/` | 2026-10-26 | Open-loop corpus throughput with the replay server's listener measured (#1159): the default replay's single accept thread, which runs the TLS handshake, and its listen backlog of 5 are what limited the s13 run; section "The corpus open-loop ceiling is the replay server's listener" below. |
| `results/reqbench-20261022-corpus-*` (serial runs), `results/reqbench-20261022-corpus-s13/scale-*`, `results/corpusextra-2026102[23]-*` | 2026-10-22 to 10-23 | The corpus campaign the report now quotes, on one c8g.metal-48xl (spot; reclaimed once, resumed on a replacement): serial runs at 2, 4 and 8 vCPUs, prefetch pairs, the poll-interval and copy-mode A/B/As, memory with many clones (screenshot and HTML), host Chromium controls, and an open-loop throughput run. Every serial run DNS-verified clean with 0 failures in 230 attempts per arm. Records carry `host_boot_sha256` (#1153). Harness defects found on the way are items 1 to 19 of "Harness bugs the 2026-10-22 campaign's first runs exposed" below. |

---

## Report closeout, 2026-09-07

The report now publishes the verified corpus latency result without quantitative
claims from the missing network-mode, concurrency/memory, memory-frontier,
ablation, CoW census or WebKit probes. Their tables and repeated conclusions are
removed. The withdrawn CPU-demand series supplies no chart or queueing
attribution. The Kitesurf table leaves fcvm CPU and memory unestablished rather
than substituting fixture VMM CPU or unsupported clone-memory figures.

Useful August 30 exploratory records are retained under
`report/evidence/20260830/`, including raw reduction inputs and the incomplete
first memory attempt's failure log. They remain unqualified for publication:
blocked arms, a changing URL mix and unreconciled memory accounting prevent a
controlled comparison. See that directory's README for provenance and the
read-only recomputation command.

The report source and checklist are reconciled. External publication is still
pending because this session has no Artifact publisher and the public wrapper
does not expose a report body to compare. Follow-up benchmark campaigns are not
required to publish the scoped latency result.

The historical sections below retain their original experiment context. The
unretained hugepage pool-consumption numbers are no longer current evidence.

## The 2026-08-16 corpus series: WITHDRAWN, do not quote

Every corpus run recorded before fcvm `90733b854e` (2026-08-29, "network: stop
pasta claiming the guest's port 53 when --dns names the gateway") resolved its
hostnames on the live internet. pasta claimed the guest's port 53 and redirected
it to the host's own resolver, so the `--dns 10.0.2.2` baked into the corpus
golden never reached the replay server; `scripts/probe-pasta-dns-gateway.sh`
reproduces the redirect with the pinned pasta binary and no VM (with `-D none`
the query reaches 10.0.2.2, without it port 53 lands on the host resolver).
`git merge-base --is-ancestor 90733b854e <source_revision>` fails for every
run in the series (e4e4df8d, efee2208, 45b64a4f, f167c726, a7275a4e, 8f0a01e8,
and 50d343f8 for the 2026-08-14 CX run) and succeeds for the 2026-08-30
record's 55756858.

The raw records are intact in `/home/ubuntu/src/fcvm-main/bench/chromium/results/`,
the worktree the runs executed in: each directory's `reqbench.jsonl` hashes to
the sha256 its committed `analysis.json` records under
`analysis_identity.inputs`, so the series can be re-analyzed without
re-running. Re-analysis does not rehabilitate it (the current reqanalyze exits 5
on the 4 vCPU record: the meta assigns no complete cell and the stall gate
fails on 19 elmundo load events over 15 s), and `campaign_summary.py` refuses
both a marked run and the shape itself: hostname URLs with no recorded
resolver (`guest_dns` null, no `BENCH_RESOLVE_ALL_TO`, no `dns-evidence.json`).
The marker is `WITHDRAWN` in each directory; it is never removed, and the
directory is kept so the marker and its evidence stay readable. Three further
2026-08-16 attempts exist only in that worktree and were never committed, so
they carry no marker: `reqbench-20260816-120712-corpus` and `-144601-corpus`
(aborted, no reqbench.jsonl) and `-144802-corpus` (a partial run at
source_revision b67e28b9 with the same signature: example.com ttfb 47.2 ms,
`dns_ms` up to 144 ms). Do not quote them from the worktree.

Evidence, from the runs' own `reqbench.jsonl` (paths under fcvm-main):

- `render.nav.dns_ms` nonzero in 8 of 460 render records of
  `reqbench-20260816-123529-corpus` (4 vCPU), seven between 149 and 158 ms;
  26 of 460 in `-121054-corpus` (2 vCPU), max 141.5 ms; 52 of 460 in
  `reqbench-20260814-042319-uffd`, max 186.2 ms. The verified run: 2 of 230,
  at 4.6 and 4.9 ms.
- `render.nav.ttfb_ms` maxima 1.0 to 7.4 s across the series (the replay
  server sits on the host loopback); 15.0 ms in the verified run, below the
  withdrawn 2 vCPU run's median of 16.2 ms.
- news.ycombinator.com rendered 11 distinct screenshot hashes in 15 renders in
  each of the 2 and 4 vCPU runs over a frozen corpus; 1 in the verified run.
- www.elmundo.es cdp median 30,912 ms at 2 vCPU and 31,046 at 4 (n=14 each),
  unmoved by a vCPU doubling; 3,842 ms [3,714, 3,879] in the verified run.
- Controls at matched 2 vCPU (`-121054-corpus` vs the verified run): noop 41.0
  vs 44.8 ms, resolve 155.8 vs 154.3, connect_total 173.0 vs 163.8, screenshot
  82.8 vs 80.3, navigate 598.4 vs 409.1. The two runs were measured on
  different hosts (`cell.host_kernel_release` 7.0.14-fcvm-cd6cd2b4b52e, boot
  hash acac488d, for the withdrawn series; 6.17.0-1019-aws, boot hash 34dd09df, for the
  verified run), on their own goldens and binaries, so no stage difference is
  attributed to the DNS fix. The withdrawn 982.9 is 37.9% above the verified
  712.6 across that host change; how much of the gap is DNS is not measured.

| withdrawn figure | why it cannot stand |
|---|---|
| corpus mix p50 **695.7 ms [560.9, 747.1]** at 4 vCPU (`results/reqbench-20260816-123529-corpus/analysis.json`), the report's headline and its Kitesurf comparison cell | Rendered under live DNS: `dns_ms` hits at 149-158 ms, `ttfb_ms` p99 279 ms and max 3.6 s, elmundo at 31 s. Replaced first by 712.6 ms [610.5, 808.5] at 2 vCPU from `results/reqbench-20260830-171007-corpus/analysis.json`, and since 2026-09-02 by the DNS-verified 4 vCPU cell 549.4 ms [467.9, 632.4] from `results/reqbench-20260902-025115-corpus-c4/analysis.json`, which is the headline. The withdrawn 695.7 is not evidence for it and is not used to size anything. |
| the vCPU ladder **982.9 / 695.7 / 647.2 ms** at 2/4/8, its steps (287 ms, 48 ms), its 66 ms noise floor (916.9, 951.2, 982.9) and "4 vCPU is the operating point" | All ten runs contaminated. 982.9 was also the maximum of the five uffd/minor 2 vCPU cells (916.9-982.9, mean 949.0), inflating the 2->4 step from 253 to 287 ms; the eight 2 vCPU cells across uffd/minor, uffd/copy and file backends and four fcvm binaries span 878.5-982.9 (mean 938.2). Superseded by the DNS-verified 2/4/8 ladder of 2026-09-02, 770.3 / 549.4 / 580.2 ms (`results/reqbench-20260902-023115-corpus-c2/analysis.json`, `results/reqbench-20260902-025115-corpus-c4/analysis.json`, `results/reqbench-20260902-031115-corpus-c8/analysis.json`, indexed by `results/campaign-20260902-box2-ladder-summary.json`). Each of its rungs ran once: the median fell 220.9 ms from 2 to 4 vCPUs, and the 4 and 8 vCPU medians each sit inside the other's interval, so it does not locate a knee or show that 4 is faster than 8. The six-URL direction argument that stood here in the interim, built from these withdrawn rungs because no verified ladder existed, is removed from the report and is not quotable either. Nothing from the 2026-08-16 series is used to size the step. |
| the probed trio **966.2 / 685.5 / 660.5 ms** and its per-render peak-core captions (`results/cpuprobe-20260816/*.json`), the "probe overhead −1.5%" comparison (685.5 vs 695.7), and the 1630 -> 1885 -> 2340 core-ms totals | Source runs `-152156`, `-145649`, `-154831` are contaminated; the samples were taken while renders waited on live DNS, and a difference between two live-network runs says nothing about the probe. The censoring shape at 2 and 4 vCPU is kept as a lead, not a measurement. |
| "the guest is CPU-starved, not slow: `Page.enable` 6.8 vs 3.9 ms, TCP connect is 0.1 ms throughout" | `stages.tcp_ms` is cdpdrive.py's own TCP connect to the WebSocket endpoint, the harness connecting over loopback to 127.0.0.2:9222, pasta's host-side listener for the CDP port forwarded to the guest, which accepts on the host without the guest, so the connect contains no guest round trip (`cdpdrive.py` line 16). It is not the page's connection to any origin and cannot exclude network effects. The page-side timings in the same records (`render.nav`) show live DNS. Conclusion withdrawn. The 2026-09-02 verified ladder supplies the `render.nav` the re-derivation needed and does not support it: `dns_ms` is zero in all 230 render records of every rung, and `Page.enable` runs 2.5 / 2.8 / 3.8 ms at 2 / 4 / 8 vCPU, rising as cores are added, with the two verified 2 vCPU runs 1.9 ms apart against 1.4 ms across the ladder. The stage does not discriminate; no CPU-starvation claim stands. |
| elmundo **31,046 ms** "unresolved, guest-specific stall", its 114 ms contribution to the mix and the 581.8 ms elmundo-excluded figure | The cause was the untested item on that list: DNS. pasta sent the guest's lookups to the host's resolver, so elmundo's third-party hosts resolved on the live internet, those requests never reached the replay server, and the page waited on them. The replay server leaves nothing unanswered: it returns an empty 404 for an uncaptured GET, HEAD or POST and 204 for any OPTIONS. Verified value 3,842 ms in the 2026-08-30 2 vCPU run. |
| CX, **615.1 ms** at 2 vCPU (`results/reqbench-20260814-042319-uffd/analysis.json`), the report's original corpus run | Same class: source_revision 50d343f8 predates the fix, `dns_ms` nonzero in 52 of 460 records, no resolver evidence. |

The guard is `test_reqbench.DocLint`: `test_every_corpus_record_cited_by_the_report_is_dns_verified_or_withdrawn` (a cited corpus record loads clean through `campaign_summary.load_cell` or is cited as withdrawn; every committed corpus record is one or the other; a cited campaign index is `campaign_summary` output whose hashes match the committed bytes) and `test_every_corpus_figure_in_the_ladder_is_a_verified_headline` (every figure in a vCPU or cdp-headline table equals a median, lo or hi of a verified record at its printed precision).

---

## The CDP-path A/B (`reqbench.py`) — WITHDRAWN, do not quote

This run measured the host-driven CDP request path against the `exec` path and
reported `exec 565 ms -> cdp 384 ms`, `PART 1 = -180.5 ms CI [-235.5, -176.7]`,
a per-child teardown breakdown, and `reclaim CPU 0.00 ms`. **None of it may be
quoted.** Each figure below is withdrawn for a stated, specific reason, and the
harness defect that caused it is fixed in this PR with a regression test.

| withdrawn figure | why it cannot stand |
|---|---|
| `cdp` 384 ms, `cdp-fast` 372 ms, `PART 1 -180.5 ms` | **Success-conditioned with an arm-correlated censoring rate that was never reported.** `reqanalyze` computed every median over `ok` records only and emitted ONE global `n_failed` — there was no per-arm denominator anywhere in its output. The CDP arms dropped requests with `WsClosed` (file **1/60 = 1.7%**, CP 95% CI [0.04%, 8.9%]; UFFD **6/60 = 10.0%**, CI [3.8%, 20.5%]) while `exec` dropped **0/66** (CI [0%, 5.4%]). A ~5.25 s transport drop truncates the right tail, so the arm with 10% censoring has its median pulled down six times harder than the arm with 1.7% — in the same direction as the reported effect. |
| `reclaim CPU 0.00 ms [0.00, 0.00]` | **Three independent defects, all in the same number.** (a) `/proc/<pid>/stat` is quantized to one jiffy = 10 ms here, so a sub-tick reclaim reports a hard `0.0` — zero with zero uncertainty, defect 6 exactly. (b) The analyzer POOLED all children into one list and discarded the name, so the median of {firecracker, holder, pasta} is the middle child, not the straggler. (c) The per-child sampler ran SEQUENTIALLY, so a child that exited while an earlier one was still being sampled recorded `null`, not a bound. The honest restatement of what was measured is **"below /proc tick resolution (< 20 ms) for every child sampled"** — which is not the same claim. |
| machine-cost `+610.4 ms, CI [+575.9, +620.3]` per request | **The ambient baseline was measured while the harness held 100% of one core.** The control window was `while time.monotonic()-t0 < 0.05: pass`. Measured on this box, that reports `control_busy_cores` **1.20–1.40** where a sleeping window over the same ambient load reports **0.00–0.40** — roughly one core of the sampler's own spin, then multiplied by the whole reclaim window and subtracted. The control window was also taken BEFORE the kill, so it additionally contains the still-running VM's own CPU, which the reclaim window does not. Systematic over-subtraction in a value published as "ambient subtracted". |
| per-child teardown: **pasta 704 ms**, firecracker 19.7 ms, holder 0.2 ms | **Measured on a tree without pasta's pdeathsig.** `46dbb789` (on main, and now under this branch) arms `PR_SET_PDEATHSIG(SIGKILL)` on pasta via `pre_exec`. The measurement — and the conclusion drawn from it, "the pdeathsig guarantee does not cover pasta" — describe a tree that no longer exists. Both are withdrawn. The replacement claim is in AGENTS.md: kernel-enforced for the VMM and the holder unconditionally, and for pasta **while fcvm runs as root** (`cred_cap_issubset` holds only under a capability loss). |
| "Early response converts teardown from latency into throughput cost" — REFUTED | The refutation rested on the two rows above (the machine-cost figure and the pasta straggler). Both are withdrawn, so the refutation is withdrawn with them. The premise returns to **untested**, not to supported. |
| `n` for every CDP arm | The harness aborted the whole run on any per-rep exception (`run_exec_request` had a bare `proc.wait(timeout=...)`), and a dropped rep produced no record. So the denominators are not auditable from the artifact. |

**Availability gate for the re-run.** Per the two-sided Clopper-Pearson convention
used throughout this file: at least **200 CDP requests per backend at 0 failures**
before any CDP latency figure is quoted. At 0/200 the CP upper bound is 1.8%
(`reqanalyze.clopper_pearson(0, 200)` -> [0.000%, 1.828%]); the zero-failure
upper bound first falls below 2% at n=183 (1.996%), and 200 is the round number past
it; below 2% is where "we do not drop requests" becomes a defensible statement rather
than a hope. On 2026-08-08 the observed rates were ~1 in 60 (file) and ~1 in 10
(UFFD), so the gate failed. reqanalyze now enforces it (at least 200 measured requests
per CDP arm, zero failures over every attempt including warmups); each of the four
published corpus runs passed it at 0/230 attempts per arm, [0, 1.59%].

*(This said **1.5%** and attributed the gate to "AGENTS.md's amplification
discipline". 1.5% is the ONE-sided bound, `1 - 0.05**(1/200) = 1.487%`, i.e. 22%
tighter than the data supports and inconsistent with the convention this file
declares four paragraphs below; and AGENTS.md's only amplification passage is the
ANGLE parity trap — it contains no availability gate, so the attribution was
invented. Six of this file's seven bounds reproduce exactly against
`reqanalyze.clopper_pearson`; this was the sole outlier, and
`DocLint.test_every_binomial_bound_matches_reqanalyze_clopper_pearson` now
recomputes every one of them.)*

**Root cause of the observed `WsClosed` records: undetermined.** `render.py`'s
`_recv_until` discarded any bytes the peer coalesced past the `\r\n\r\n` of the
101 response, which desyncs the very next frame header and surfaces as exactly
`WsClosed("connection closed mid-frame")`. That is fixed in this PR — but it is a
**variable removed, not a diagnosis**: Chromium is not expected to push before the
first command, and the hypothesis was never confirmed against a failing record.
That withdrawn run used a `socat` relay; the current direct-DNAT path does not.

**"0 failures" is not a 0% failure rate.** Every success count in this file is
exact-binomial bounded, not a guarantee: **0/426 is [0, 0.86%]**, **0/462 is
[0, 0.80%]**, and the single timeout at the `minor`-4K cell is **1/459 = 0.22%
[0.006%, 1.21%]** (Clopper-Pearson, 95%, two-sided; `reqanalyze.clopper_pearson`
computes these). Quote the interval whenever the count is used to support a
claim about reliability.

The 2026-08-08 run: 426 matrix requests / 0 failures, R=12 per cell, 3 reps per density cell,
5 bursts per throughput cell, one seeded interleaved schedule (`seed=20260808`),
`RUST_LOG=fcvm=debug`, per-clone cgroup accounting, load sampled every 5 s. Binary built from a
pristine `origin/main` @ `63f0d375` checkout and pinned via `FCVM=` so nothing under test could
change mid-run (sha256 in `hostinfo.json`).

---

## Previously REFUTED claims — status after the corrected run

| # | claim | status now |
|---|---|---|
| 1 | "fcvm marginal memory beats a warm container pool (129 vs 151 MiB)" | **SUPPORTED, but much smaller than claimed, and only for some backends.** On a matched cgroup basis: file-backed **143.5 ± 0.4** vs pool **156.5 ± 5.0** MiB/req, an 8% win and not a comfortable one. UFFD copy-mode **loses** (257.8 ± 1.1). UFFD `minor` wins clearly at 4K (132.5 ± 1.0). **The hugepage cell is NOT on the matched basis and must not be quoted as a win:** 34.7 ± 0.4 counts only non-hugetlb memory, because this host's cgroup2 mounts without `memory_hugetlb_accounting` (no hugetlb controller at all) and the pool was pre-allocated before the sample, so neither basis can see the guest's 2 MiB pages. The 553-611 MiB per concurrent clone once quoted here, on a pool-consumption basis that can see those pages, came from a later report table, not from this run; no committed record holds it and the 2026-09-07 closeout took it out of current evidence, so the hugepage cell's per-clone memory cost is not established. Load during the density phases that produced every number in this row (continuous record: corrected.json load.by_phase dens1-dens16): median 0.56-0.64, p90 2.21, max 6.32 on 64 cores. |
| 2 | any egress-mode *ordering* | **STILL NOT SUPPORTED.** The confound is gone (drift term −68.8 ± 51.9 ms/h, n.s.) and within-run SE is now 8.4 ms, but run-to-run shifts reach 52 ms and the IPv6 modes swap rank. Only "the two IPv4 rootless modes are fastest" reproduces; total spread ~110 ms of ~800 ms. |
| 3 | "16 clones sustain 5.5–6.3 req/s" | **SUPERSEDED.** Throughput, file-backed N=16, sustained phase: **7.3 rps** — 462 completions in 63.6 s, a **SINGLE** sustained window per cell, so n=1 window and no run-to-run rate interval is derivable from the data as collected (the burst cells are replicated 5x and do carry CIs). all 462 launched requests completed — 0/462 incomplete, CP 95% [0, 0.80%]. *Burst figures are NOT throughput and must not be quoted as such* (see the binding requirement below); for completeness the burst cell, with the burst as the experimental unit (5 bursts/cell), came out at 6.4 req/s measured **within** the burst window (CI 6.4–6.7) file-backed and 7.4 (7.2–7.7) hugepage-minor — i.e. the burst *understates* capacity, which is why leading with it was the original defect. |
| 4 | "7.9 vs 5.3 req/GB" (slope without intercept) | **REPLACED.** Slopes now carry intercepts and SEs and req/GiB is quoted at concrete N. At N=8: hugepage-minor 27.7, minor-4K 7.5, file-4K 7.0, pool 5.5, copy-4K 3.9. |
| 5 | ~70 ms of the file-vs-UFFD gap unattributable at `RUST_LOG=info` | **RESOLVED.** At debug the exec retry ladder logged **0.0 ms cumulative retry wait in all 426 requests** (max 0.0) — the quantization is absent, not merely smaller. |

## Previously SURVIVING claims — re-checked

| claim | status |
|---|---|
| restore primitive is milliseconds; the per-request cost is lifecycle, not the primitive | **HOLDS.** Restore stage 52.9 ms (52.0–54.0); the hypervisor is ~6% of the 890.6 ms request. |
| routed egress stalls ~1 s on first connection after restore | **FIXED AND CONFIRMED.** First-egress is 3.0 ms on every mode including routed (was ~1002 ms). |
| JPEG q80 screenshots −21% per request | **REFUTED AS STATED.** Screenshot *stage* −28.8% (−52.9 ms, CI −63.8…−44.8) — real and worth keeping — but the whole-request effect is **−8.3%** (−65.8 ms), not −21%. The screenshot is only ~18% of the request. |
| `--disable-site-isolation-trials` −23% Chromium RSS | **REFUTED AS A MEMORY WIN.** RSS does fall 20.5%, but RSS counts shared pages once per process and the change removes 2 of 12 processes. On **PSS** the true saving is **3.6% (−14.1 MiB, CI −17.3…−12.8)**, not −192 MiB. |
| `VK_ICD_FILENAMES` pre-seed prevents the ANGLE setenv crash | **UNCHALLENGED HERE.** 426/426 renders succeeded with the pre-seed in place; this run did not re-run the race amplifier, so the original amplified evidence still carries the claim. |

## Newly measured in this run

- **`--uffd-mode minor`, first measurement.** At 4K: **132.5 ± 1.0 MiB per concurrent request**,
  the figure that stands (density phases ran at median load 0.56-0.64, p90 2.21 on 64 cores;
  continuous record in corrected.json load.by_phase). With 2 MiB hugepages the run recorded 34.7 ± 0.4 (MemAvailable basis
  29.1 ± 1.1), and that figure **counts only non-hugetlb memory**: cgroup2 here mounts without
  `memory_hugetlb_accounting` and carries no hugetlb controller, so `memory.current` cannot see
  2 MiB pages, and the pool was pre-allocated before the sample, so MemAvailable cannot move for
  them either. The "4.5x better than the warm container pool, 7.4x better than copy-mode"
  reading that accompanied it compared a number excluding the guest's RAM against numbers
  including it. The 553-611 MiB per concurrent clone once quoted here on a
  pool-consumption basis was not measured in this run: it came from a later report table, no
  committed record holds it, and the 2026-09-07 closeout took it out of current evidence, so
  this cell's per-clone memory cost is not established.
- **`minor` buys memory, not rate.** At a sustained 8 rps the 4K `minor` cell needed 165 s to
  drain 459 requests (2.8 rps achieved, p50 1224 ms, 1 request timed out — 1/459 = 0.22%
  [0.006%, 1.21%]) while file-backed held 7.3 rps with 462/462 complete. Both rates come from a
  SINGLE window per cell, so neither carries a run-to-run interval; the 2.6x gap is far larger
  than any plausible one-window spread, which is why the *direction* is quoted and the third
  significant figure is not. The serve process is the bottleneck.
- **End-to-end request, measured end to end in one run** (not composed): artifact **730.0 ms**
  (708.4–741.0), total **890.6 ms** (869.6–929.0).
- **The guest-side exec cost is attributed by measurement.** An interleaved shell-only control
  (`noop-sh`, 254.1 ms) against the Python control (`noop`, 280.5 ms) puts Python startup at
  **26.4 ms** and leaves **~173 ms** on fcvm's `--exec` podman entry — the largest addressable
  item in the request.
- **CDP handshake is a real per-request cost:** 16.7 ms (16.4–16.9) under this design.
- **Host-native floor, same run, same driver:** warm container `medium` **217.6 ms** (202–229),
  so fcvm's shared-nothing overhead is **512 ms**, of which only ~84 ms is isolation.
- **Two memory bases agree.** cgroup `memory.current` and whole-machine `MemAvailable` delta
  agree within 5–10 MiB on every cell — the cross-check that makes the density numbers usable.
  PSS deliberately disagrees (it divides shared pages); its large intercept *is* the shared
  snapshot.

## Still unmeasured / open

- **Egress-mode ordering** needs repeated *independent runs*, not more reps within one run —
  run-to-run variance dominates within-run SE by ~6x.
- **The ANGLE race** was not re-amplified in this run.
- **The corpus open-loop throughput ceiling** is located in the replay server's listener
  (next section, #1159). Open: where the ceiling lies once the listener is out of the way
  (4 guest vCPUs: the handshake-in-handler runs went to 4.2 per restore path, the static
  page to 5.6; beyond that the harness's own limits end the runs first, see the section below), and which stage is
  the next limit.
- **Hugepage `minor` at 4K-equivalent page counts** — the hugepage win conflates page size with
  sharing mode; a 4K `minor` vs 2 MiB `minor` decomposition would separate them.
- **Teardown** (175 ms, post-artifact) was not moved off the response path in this run.
  reqbench.py has since moved it after the answer: `blocking_ms` ends when the CDP driver
  returns and excludes teardown (median 63.7 ms at 4 vCPUs in
  `results/reqbench-20260902-025115-corpus-c4`), and `wall_ms` includes it. The AGENTS.md
  reclaim measurements suggest the moved teardown converts to throughput cost rather than
  disappearing. Since 2026-10-22 the fast arms' `request_cpu_ms` includes the CPU each clone
  process spent freeing its memory at exit (`reclaim`); kernel-thread work such as deleting
  the clone's disk is still in no figure.

## Harness bugs found in `reqbench.py` / `reqanalyze.py` / `reqbench.sh` (2026-08-08)

Found by adversarial review of the CDP A/B, all fixed in this PR, each with a test
that was watched fail first (`bench/chromium/test_reqbench.py`, and the clone
flavour in `tests/test_signal_cleanup.rs`):

1. **A vacuous leak assertion.** `test_bench_fast_teardown_leaks_nothing_clone`
   held the namespace holder as an `Option` and asserted
   `!holder.is_some_and(running)` — `!false` when discovery returns `None`.
   Discovery pgreps for the literal `sleep infinity`, so an argv drift silently
   turned the assertion into nothing while still reading as coverage. It also
   never looked for pasta at all. Both are now hard-fail-on-absent.
2. **The teardown reaped the state file and data dir of a VM it had just failed
   to kill** — `all_gone` was computed, recorded, and then ignored.
3. **The CPU-accounting control window was a busy-spin** (see the withdrawal
   table above), and the pre-kill CPU baseline was sampled BEFORE that window, so
   the delta also absorbed ~50 ms of the live VM's ordinary CPU.
4. **Per-child CPU sampling was sequential**, so a child that exited while an
   earlier one was still being sampled recorded `null`.
5. **`find_state` was an unthrottled `os.listdir` + `json.load` loop** — 400 ms of
   harness CPU for a 400 ms wait, 100% of a core, at the instant a 2-vCPU clone
   was restoring on the same box. Now inotify, matching what fcvm itself does.
6. **The exec arm's `proc.wait(timeout=…)` was unguarded**, so one slow rep raised
   out of `main()` and orphaned a live clone into the next run.
7. **`cmd_verify` always exited 0** — all three hops were `|| echo`, so the gate
   documented as "do this first" could not fail, and `all` proceeded to measure.
8. **`reqbench.sh` had no `trap`**, and two of three phases never captured `$!`,
   so every error path leaked a VM or a serve under `set -e`.
9. **`--ws-url` was passed verbatim to every clone**, naming one fixed host-side
   address for a whole run in which every clone has its own.
10. **The target-id stability probe inspected one clone** under a heading that
    asked a cross-clone question, and `cdpdrive.py --print-target` — the flag its
    own docstring offers for checking that claim — did not exist.

## Harness bugs found and fixed while producing the 20260808-corrected run

These were live defects in the harness, not in fcvm:

1. **Every request failed with a `SyntaxError`** — `build_driver` already returns a complete
   `python3 -c '…'` command and `req()` wrapped it in another one. Fixed; 0/426 failures after.
2. **The MINOR arm silently measured a COPY server.** `start_serve` reused a serve by snapshot tag
   regardless of uffd mode, and `stop_serve` returned after a fixed `sleep 1` while the old server
   was still draining. Fixed: serves are keyed by (tag, mode), teardown waits for the process and
   its state entry to disappear, and a serve this run did not start is never killed (shared box).
3. **`exec_handshake_ms`/`exec_spawn_ms` were always `None`** — the generic
   `DEBUG fcvm::commands::exec` branch preceded the ACK/GO branches in the same `elif` chain and
   swallowed them. Fixed; the exec stage now splits.
4. **Phase 5 aborted under `set -u`** — `native-$f__${label}` parses `$f__` as a variable name.
   Fixed by bracing `${f}`.
5. **A bare `wait` hung phase 5 for 20+ minutes** — it waited on the (infinite) load sampler as
   well as the podman job. Fixed to wait on the specific PID.

## Harness bugs the 2026-10-22 campaign's first runs exposed

Each was fixed before any figure the report quotes was measured, with a test
that fails on the commit before the fix (`test_run_facts.py` names them).
Records of the runs that hit them are not committed.

1. **Every fast-arm CPU figure was a lower bound.** fcvm's children die by
   pdeathsig after the SIGKILL; init reaped them before the 200 us sampler
   read their final CPU from the zombie. The harness is now their subreaper
   and collects each pinned child after the read (`d8986477`).
2. **Every fast-arm CPU figure was withheld.** The reducer looked for the key
   `firecracker`; fcvm's content-addressed binaries show as
   `firecracker-def` and `pasta-<hash>`. Children are read by role, and the
   scale probe and faultbench match the prefix (`ec5def10`).
3. **The memory phase refused every golden reqbench made** after the scale
   files joined reqbench's sealed bundle; corpus_extra hashed the old list
   (`ec5def10`).
4. **The memory phase's `podman ps` timed out at 48 containers**: the image
   healthcheck, run per container by systemd timers, held podman's locks
   (7.7 to 23.6 s per `ps`). Pool containers run without it (`05b6ce9b`).
5. **pasta's PSS was unreadable** to the owning user (pasta is
   non-dumpable); the sampler reads it through `sudo -n cat` (`a3530101`).
6. **The measured run refused itself after the before-run verify**, whose
   two clones lifted the 1-minute load over the run's 2.0 gate; the campaign
   settles again before the run (`ff9aef9e`).
7. **A memory read refused on fcvm's own health checks**, which run in each
   clone's cgroup every 10 s; a read whose process set moved, or one of
   whose processes exited mid-read, is repeated, and a zombie counts as zero
   (`cc1e0eed`, `c17cbe49`, `9043868d`).
8. **The scale run recorded the host name**; it records the instance type
   (`33936a18`).

Later in the campaign, each with a test that fails on the commit before
the fix:

9. **fc-agent's restore phases reached no record.** fcvm prints them in its
   one-line restored record, which only the uncommitted per-request clone
   logs held. Requests now record it (`restore_ack`), and `restore_ack.py`
   reads it out of an earlier run's clone logs into `restore-ack.jsonl`
   (`19b9c21c`).
10. **The memory phase could not render with HTML extraction**, so
    Cloudflare's HTML memory row had no fcvm figure (`MEM_OP=html`,
    `a92ead87`).
11. **The memory phase removed each pool container with podman's 10 s
    SIGTERM grace**, serially; one removal on a host loaded by other jobs
    took 72 s and failed the phase after 110 of 140 cells. Containers are
    removed with `--time=0` (`e5b0abfa`).
12. **The replay reset beacon connections** (#1137): it answered an
    uncaptured POST without reading the body, and the close with the body
    unread was a RST, which the diag refused as a request with no remote
    address. Four diags in a row failed on it on one host while another
    project's jobs loaded it. PR #1138 reads the body first, but no run here
    uses it: with bodies read, the first diag on it failed on a different
    request, an iframe on www.elmundo.es that the page itself canceled
    (`net::ERR_ABORTED`) before it named an address, in two of three renders.
    Reading bodies changes how that page behaves, so the campaign kept the
    replay it started with. The diag instead counts a traced row with no
    address as the replay's when the replay's own access log shows it
    answered that request inside the render's window (`ffab8ecd`, with the
    request target matched as sent in `1775f3c3`); the runs measured after
    those commits record them as their `source_revision`.

13. **The memory phase refused itself on a state file another clone was
    deleting**: it lists the state directory and reads each file, and an
    exiting clone's fcvm removes its own; the read raised and the phase
    stopped after 101 of its 140 cells. A file deleted between the listing
    and the read is skipped (`ef0fb672`).

14. **The corpus scale run refused itself twice before measuring
    anything.** Its clean-tree check counted the run's own results, which
    the corpus campaign writes under `bench/chromium/results/` before the
    scale phase, as uncommitted harness (`ed2fbe51`); and it reads three
    monotonic stamps from each request record that `run_cdp_request` never
    set, so it refused every request (`4d13d954`). The corpus scale path had
    not run end to end before; its unit tests stubbed the request function.
    Its first complete attempt then failed at 25.2 requests per second:
    each in-flight request holds an inotify instance, and the host's limit
    of 128 refused most of that burst with EMFILE. The host's
    `fs.inotify.max_user_instances` was raised to 8192 and the run repeated
    at 1.4 to 11.2 requests per second; the harness does not check the limit
    before it starts.

15. **The scale run's clones ran outside their backend cgroups and carried
    no fault counters.** reqscale hands `run_cdp_request` a cgroup guard and
    a per-request Firecracker fault probe, and it used neither: every clone
    of the 2026-10-22 attempts ran in the driver's cgroup (the FILE cgroup's
    `cpu.stat` read 0 while the driver's read 739 s), and no request carried
    `firecracker_process_faults_ready_to_artifact`, which the scale analyzer
    requires of every row, so no attempt could have been analyzed. Both are
    wired in `0bf1f978`.

    Those two attempts, at `4d13d954`, also stopped at 2.8 requests per
    second per restore path (5.6 in all). Of each 420-request burst at that
    rate, 37 and then 168 requests timed out, 9 and 17 failed navigation
    with a reset, and 3 and 1 were reported as teardown failures over a lock
    file that was already gone (`left=[]`; item 17); at 1.4 both completed all 210. The harness refuses a run on any failed
    request, so the throughput run was repeated at lower rates. Their
    `requests.jsonl` (each failure's `error`), `status.json` and
    `bursts.jsonl` are kept under `results/reqbench-20261022-corpus-s7/` and
    `-s7-r2/`; `error_details.failed` in `status.json` holds the totals, 49
    and 186.

16. **Wiring the fault probe exposed a race in it.** It lists fcvm's
    children and opens each one's `/proc` directory, and fcvm's short-lived
    helpers exit in between; at `0bf1f978` only 7 and 12 of 84 requests per
    path completed at 1.4 req/s (from the campaign's step log; that
    attempt's records were not kept). The probe now skips a child that is gone
    and still requires exactly one Firecracker (`1dc6ae42`). Two attempts
    before that were refused at startup because their rates broke the
    schedule's rules, which the harness checks only once the campaign has
    built its golden: each 15 s ramp must hold a whole number of requests
    and each 60 s window whole cycles of the 14 pages, so the rates must be
    multiples of 7/15 per second.

17. **The fast teardown counted a file someone else had already removed as
    a failure.** `reap_disk` checks that the clone's state and lock exist,
    then removes them; under concurrent clones another fcvm's stale-state
    cleanup can delete them in between, and the ENOENT became
    `SurvivedTeardown` with nothing left on disk. One request of 2,015 at
    `1dc6ae42` did that, and the harness refused the run (stamp `s12`; that
    attempt's records were not kept, and its repeat `s12-r2` below completed
    without it). A removal that
    finds the file gone now counts as done (`c5352b1b`).

18. **The scale analyzer could not read what the harness records.** The
    first corpus scale run to complete every burst with no failed request
    (stamp `s12-r2`, at `1dc6ae42`) was refused at analysis: fcvm writes
    each snapshot port mapping as an object and the analyzer took only
    strings such as `"9222:9222"`, its fixtures' shape; and the cgroup audit
    names Firecracker by its comm, `firecracker-def`, where the analyzer
    looked for `firecracker`. Both are fixed in `fcbeec9e`. With the
    analysis refused, that run's DNS evidence was bound to no run id and
    read unclean, so the run was repeated rather than re-bound by hand
    (`results/reqbench-20261022-corpus-s12-r2/`: `bursts.jsonl`, every
    burst passed, and `dns-evidence.json` with its `reason`). The
    harness's tests had covered each side against its own fixtures; nothing
    ran a real scale record through the analyzer before this campaign.

19. **The WebKit golden could not record its browser version.** The golden
    records the browser build by running a command in its image, and for
    WebKit that was `WebKitWebDriver --version`; WebKitWebDriver has no such
    flag (it prints its usage to stderr and exits 1), so the golden refused
    itself and no WebKit run could start. The version now comes from the
    `webkit2gtk-driver` package (`873d12c3`). This came in with the
    `browser_version` record itself (`5b94b566`); no WebKit run had used it.

Also seen and not a harness defect: an unattended package upgrade ran
during one serial run (its DNS-owner sample read `dnsmasq=unknown`, so the
run was refused as unclean and rerun); the hosts' apt timers were disabled
for the rest of the campaign. One 96-clone memory cell failed when one
clone's published DevTools port reset every connect for 30 s while fcvm's
own probe reached the guest (#1135); the published memory fit stops at 48
clones. Two spot hosts were terminated mid-run by the parallel-box idle
watchdog, which reads a measured run of one request at a time on a
192-vCPU host as idle; their unfinished runs are not records.

## The corpus open-loop ceiling is the replay server's listener (#1159)

`results/scale-ceiling-20261026/` (`summary.json`, `commands.txt`, the listener logs). c8g.metal-48xl,
on demand, 4 guest vCPUs unless stated, FILE and UFFD interleaved, the s13 gates, seed 20261022.
`corpus_serve.py --listen-log` was added for these runs; it records, once a second, what happens
before the replay's handler thread starts timing (`duration_ms` in the access log starts there).

- **Default replay (listen backlog 5, TLS handshake inside the one accept thread).** At 1.40 per
  path the accept thread was inside `accept()` 43% of the time on average (2.8 ms per connection, 46,392
  connections in the 300 s of scoring windows, 52 per page load) and the kernel dropped 13,869
  connections for a full accept queue (`ListenOverflows`). At 1.87 it was 62% busy (3.0 ms), 26,105 drops,
  median navigation 1,072 ms against 370 ms at 1.40, 364 of 1,120 requests over 2 s, and the run failed the
  `departure` gate (1.78 completed per second) as s13 did. The first run on this tree offered 2.33 and 2.8:
  both failed (2.8: 244 of 336 requests failed, the accept thread spent up to 11.2 s inside one
  `accept()`, 15,182 drops; navigation median 33 s). A dropped SYN is retried after 1 s, so the 5 s p95
  that s13 recorded at every rate comes partly from here.
- **`--backlog 1024` alone**: 0 drops, median navigation 267 to 306 ms from 1.40 to 2.33 per path. In the
  second run (`backlog-1024-b`, the only one with an analysis) 1.40 and 1.87 pass every gate and 2.33 is
  recorded as a failed rate (one failed request among 1,400, a teardown leaving a lock file); the first run
  was stopped at burst 12 of 18 by the sampler fault fixed in this PR, so its windows are 240 s at 1.40 and
  180 s at 1.87 and 2.33 and it has no analysis. The accept thread is then 37% to 61% busy at 2.4 to 2.5 ms per connection, which puts
  its own limit near 3.8 per path (1 / 2.44 ms = 410 connections per second, 53 per page load, two paths).
- **`--tls-handshake-in-handler`** (the default backlog): the accept thread is 0.5% to 2.4% busy (0.03 to
  0.05 ms per connection), handler time per request 6 to 9 ms (mean), and 1.40, 1.87, 2.33 and 2.8 all pass
  (median navigation 262, 290, 307, 281 ms). In a second run set 3.27, 3.73 and 4.20 had 0, 3 and 1 failed
  scored requests among 1,960, 2,240 and 2,520 (a failed request fails its rate under the strict policy,
  and that run's analysis was refused for a 16.2% host-control drift), median navigation 322, 346 and
  392 ms, 38, 75 and 244 dropped connections. At 2 guest vCPUs the same setting had 4 failed of
  2,520 at 4.2 and none of 1,680 at 2.8, and failed 2,295 of 3,360 requests at 5.6 (267 drops).
  Its 1.40 cell lost 62 requests in one burst (b0011) to the per-request Firecracker probe
  ("firecracker-def pid N is outside run cgroup", one line per thread of one clone), which is not a
  replay effect and is open.
- **Replay out of the path** (one page the guest serves itself, `SCALE_URL_OVERRIDE`): 5.6 per path
  completed 3,357 of 3,360 requests (median 502 ms against 423 ms at 1.40; the 3 failures were the harness's
  Firecracker-pid probe, not the host).
- **Above about 4.2 per path the failures are the harness's own limits, not a host measurement.** Every
  failed request at 5.6 with 2 vCPUs (2,021 "firecracker-def pid N is outside run cgroup", 976
  `inotify_init1: Too many open files`) and at 8.4 and 6.5 with the page or the replay was one of those two
  or a descriptor-exhaustion error. `reqbench.py` opens one inotify instance per request in flight, and
  Ubuntu's `fs.inotify.max_user_instances` is 128. With it at 65,536 the static page again held 5.6
  (669 ok, 3 failed) and at 8.4 completed 609 and failed 399 requests, 804 of the 805 failed requests at that
  rate being the cgroup-membership probe (median 1.1 s); at 11.2 the driver ran out of file
  descriptors. So the host's own ceiling is not established above 5.6 per path (static page) or 4.2 (corpus
  with the handshake fix), and a rerun of the corpus arm with the sysctl raised stopped at its first burst
  on a process left in a request's cgroup ("inter-burst cgroup membership is not quiescent").

What this does not show: what limits the corpus above 4.2 per path once the harness's inotify, descriptor and
cgroup-probe limits are out of the way (the accept thread is idle; at most 57 handler threads were
running at 4.2 with 4 vCPUs), whether the Python replay or the host side of pasta is
the next limit, and what a page's navigation costs apart from the listener (with the listener fixed, 476 of 6,300
scored requests in the 2.8-per-path run set took over 2 s navigation, and 460 of them are all 460
requests for www.elmundo.es; the 5 s p95 of s13 is that page plus the listener's dropped connections). Every figure above is the 5 scored bursts of one
run (300 s of scoring windows per rate), except the `base-first` rows, which are 60 s windows of the first
attempt, and the `backlog-1024-a` rows (240 and 180 s); none has a repeat except the default-replay 1.40 (median navigation 362.7 and 370.0 ms in two runs, 355.4 in s13).

The earlier s13 record (`reqbench-20261022-corpus-s13`) used the default replay, so its 1.40 and 1.87
results are this table's first rows. The replay options default to the old behavior; the 2026-10-22 serial
records are unchanged by this work.

## Binding requirements for any future run

Unchanged from the previous ledger, plus:

- **Pin the binary.** Build from a known tree and pass `FCVM=`; record its sha256. A concurrent
  workload rebuilding `target/release/fcvm` from its own uncommitted changes silently swaps the
  thing under test.
- **Record load continuously** (`samples/loadavg.jsonl`) and report it per phase. On a 64-core box
  quote load as utilization, not as a raw number.
- **Do not quote a burst figure as throughput** — report the sustained phase separately and
  explain any disagreement.
- **Use PSS, never RSS**, for any claim about a change that alters process count.
