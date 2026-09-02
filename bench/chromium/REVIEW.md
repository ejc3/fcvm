# Adversarial review ledger — chromium shared-nothing bench

**Quote only from sealed runs that passed their gates and were never withdrawn.**
When this ledger was written (2026-08-08) three runs existed and the only quotable
one was `20260808-corrected`; every later gated reqbench run (noop drift canary
inside its CI band, generation and runtime-bundle seals verified) joins the record
under the same rule. Withdrawn runs stay unquotable forever — the table below
records why.

| run | date | verdict |
|---|---|---|
| `20260807-full` | 2026-08-07 | comparative numbers **retracted** on methodology (six defects) |
| `results/20260808-corrected` | 2026-08-08 | six defects corrected; numbers below are the current record |
| `reqbench` CDP A/B | 2026-08-08 | **WITHDRAWN IN FULL** — see "The CDP-path A/B" below. Harness defects invalidate every figure it produced. |
| `results/reqbench-20260816-*-corpus` (14 runs), `results/reqbench-20260814-042319-uffd`, `results/cpuprobe-20260816` | 2026-08-14 to 2026-08-16 | **WITHDRAWN IN FULL** on 2026-09-02, live-DNS contamination; see "The 2026-08-16 corpus series" below. Each directory carries a `WITHDRAWN` marker with its evidence and stays in the tree. `results/campaign-20260816-summary.json` is a hand-written index of these cells, not a record. |
| `results/reqbench-20260830-171007-corpus` | 2026-08-30 | DNS-verified corpus record (`dns-evidence.json` verdict clean); 712.6 ms [610.5, 808.5] cdp p50 at 2 vCPU is the current corpus headline. No 4 or 8 vCPU cell is verified. |

---

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
  80bfe10d, for the withdrawn series; 6.17.0-1019-aws, boot 291f8bad, for the
  verified run), on their own goldens and binaries, so no stage difference is
  attributed to the DNS fix. The withdrawn 982.9 is 27.5% above the verified
  712.6 across that host change; how much of the gap is DNS is not measured.

| withdrawn figure | why it cannot stand |
|---|---|
| corpus mix p50 **695.7 ms [560.9, 747.1]** at 4 vCPU (`results/reqbench-20260816-123529-corpus/analysis.json`), the report's headline and its Kitesurf comparison cell | Rendered under live DNS: `dns_ms` hits at 149-158 ms, `ttfb_ms` p99 279 ms and max 3.6 s, elmundo at 31 s. Replaced by 712.6 ms [610.5, 808.5] at 2 vCPU from `results/reqbench-20260830-171007-corpus/analysis.json`. No DNS-verified 4 vCPU cell exists; do not substitute a prediction. |
| the vCPU ladder **982.9 / 695.7 / 647.2 ms** at 2/4/8, its steps (287 ms, 48 ms), its 66 ms noise floor (916.9, 951.2, 982.9) and "4 vCPU is the operating point" | All ten runs contaminated. 982.9 was also the maximum of the five uffd/minor 2 vCPU cells (916.9-982.9, mean 949.0), inflating the 2->4 step from 253 to 287 ms; the eight 2 vCPU cells across uffd/minor, uffd/copy and file backends and four fcvm binaries span 878.5-982.9 (mean 938.2). What survives is a direction only: on the six URLs whose screenshots are byte-identical between each rung and the verified run (example.com and the five TodoMVC pages), the withdrawn medians were 680.9 / 498.7 / 494.9 ms, still fetched live (example.com ttfb 46.6 / 51.0 / 48.3 ms against 0.6 verified), against 547.5 ms for the verified run on the same six URLs at 2 vCPU: 4 vCPU is faster than 2, and the size of the knee is unmeasured. |
| the probed trio **966.2 / 685.5 / 660.5 ms** and its per-render peak-core captions (`results/cpuprobe-20260816/*.json`), the "probe overhead −1.5%" comparison (685.5 vs 695.7), and the 1630 -> 1885 -> 2340 core-ms totals | Source runs `-152156`, `-145649`, `-154831` are contaminated; the samples were taken while renders waited on live DNS, and a difference between two live-network runs says nothing about the probe. The censoring shape at 2 and 4 vCPU is kept as a lead, not a measurement. |
| "the guest is CPU-starved, not slow: `Page.enable` 6.8 vs 3.9 ms, TCP connect is 0.1 ms throughout" | `stages.tcp_ms` is cdpdrive.py's own TCP connect to the WebSocket endpoint, the harness reaching the guest's forwarded CDP port on 127.0.0.2:9222 over loopback (`cdpdrive.py` line 16). It is not the page's connection to any origin and cannot exclude network effects. The page-side timings in the same records (`render.nav`) show live DNS. Conclusion withdrawn; a CPU-starvation reading needs a verified ladder with `render.nav` quoted beside `Page.enable`. |
| elmundo **31,046 ms** "unresolved, guest-specific stall", its 114 ms contribution to the mix and the 581.8 ms elmundo-excluded figure | The cause was the untested item on that list: DNS. Third-party chains the replay leaves unanswered went to the live internet and waited for real timeouts. Verified value 3,842 ms at 2 vCPU. |
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
(`reqanalyze.clopper_pearson(0, 200)` -> [0.000%, 1.828%]); n=200 is where the
zero-failure upper bound first falls below 2%, which is the first point at which
"we do not drop requests" is a defensible statement rather than a hope. Today the
observed rates are ~1 in 60 (file) and ~1 in 10 (UFFD), so this gate fails loudly.

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
| 1 | "fcvm marginal memory beats a warm container pool (129 vs 151 MiB)" | **SUPPORTED, but much smaller than claimed, and only for some backends.** On a matched cgroup basis: file-backed **143.5 ± 0.4** vs pool **156.5 ± 5.0** MiB/req — an 8% win, not a comfortable one. UFFD copy-mode **loses** (257.8 ± 1.1). UFFD `minor` wins clearly at 4K (132.5 ± 1.0). **The hugepage cell is NOT on the matched basis and must not be quoted as a win:** 34.7 ± 0.4 counts only non-hugetlb memory, because this host's cgroup2 mounts without `memory_hugetlb_accounting` (no hugetlb controller at all) and the pool was pre-allocated before the sample, so neither basis can see the guest's 2 MiB pages. On the pool-consumption basis that can, hugepage-minor costs **553-611 MiB per concurrent clone**, i.e. it LOSES to 4K minor on memory and buys render latency instead. Load during the density phases that produced every number in this row (continuous record: corrected.json load.by_phase dens1-dens16): median 0.56-0.64, p90 2.21, max 6.32 on 64 cores. |
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
  including it. On the pool-consumption basis, hugepage-minor is **553-611 MiB per concurrent
  clone** — 4x WORSE than 4K minor on memory, and it is bought for render latency, not density.
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
- **`minor` sustained-throughput ceiling** was observed but not root-caused (single serve process
  suspected; not proven).
- **Hugepage `minor` at 4K-equivalent page counts** — the hugepage win conflates page size with
  sharing mode; a 4K `minor` vs 2 MiB `minor` decomposition would separate them.
- **Teardown** (175 ms, post-artifact) was not moved off the response path; the AGENTS.md
  reclaim measurements suggest it converts to throughput cost rather than disappearing.

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
