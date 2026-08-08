# Adversarial review — chromium shared-nothing bench, run 20260807-full

Verdicts from the adversarial review of the first full run (R=6, shared dev
box, 2906 parsed request logs, 0 render failures). The run's raw output is not
committed (`results/` is git-ignored) and its **comparative** numbers must not
be quoted: several were refuted on methodology. The rerun requirements at the
bottom are binding for the next full run.

## REFUTED claims

These looked like findings; the review showed the measurement cannot support
them.

1. **"fcvm clone marginal memory (~193 MiB uffd-4k / ~129 MiB file-4k) vs
   ~151 MiB per warm host-native Chromium container" — basis mismatch.**
   The fcvm slope sums only the firecracker process PSS
   (`/proc/<pid>/smaps_rollup`), excluding the fcvm supervisor, serve process
   amortization, pasta/proxy helpers, and host page-cache attribution, while
   the host-native pool contrast measures the full container. The two columns
   are not the same quantity; the density comparison is unsupported until both
   sides use matched full-cgroup accounting.

2. **Egress-mode ranking — confounded by sequential ordering.** Each mode's
   R reps ran as one contiguous block on a shared dev box with intermittent
   concurrent workloads. Time-varying background load is therefore aliased
   onto the mode factor. The ~50-100 ms spreads between the non-routed modes
   are within this confound and must not be ranked. (The routed arm's ~1 s
   stall is orders larger and survives — see below.)

3. **Burst fan-out numbers — n=1 per cell.** Every burst size (N=4/8/16 per
   memory cell) ran exactly once. No variance estimate exists; per-cell burst
   p50/p95 and req/s are anecdotes, not measurements.

4. **exec-stage attribution — 100 ms retry quantization.** The exec stage
   (~160 ms in all cells) is measured through a retry loop with 100 ms
   granularity, so the exec vs restore split is quantization-ambiguous. Stage
   decomposition needs `RUST_LOG=fcvm=debug` serve/restore logs to attribute
   time to the actual sub-steps.

## SURVIVED claims

Verified independent of the confounds above (effect direction and magnitude
robust, or reproduced under a dedicated amplifier):

1. **The restore primitive itself is ~5 ms.** Firecracker's snapshot-load is
   milliseconds; the 600-700 ms "restore" stage observed per request is
   harness- and process-lifecycle overhead around the primitive, not the
   primitive. This is what makes the per-request shared-nothing model worth
   pursuing at all.

2. **Routed egress: first connection after restore stalls ~1 s.** In-guest
   TCP probe retry loop while IPv6 DAD / NDP proxy state reconverges after
   restore; the render itself is fast once connectivity converges. The effect
   (~1,050 ms on http, ~980 ms on https at n=6 each) is an order larger than
   the ordering confound. **Fixed on the `routed-restore-ndp` branch**; rerun
   after that merges to confirm routed joins the other kernel paths.

3. **JPEG q80 screenshots: −21% artifact latency** vs PNG on the screenshot
   stage. Direction and size held across pages.

4. **`--disable-site-isolation-trials`: −23% Chromium RSS.** Fewer renderer
   processes in the warm image; the delta is far above sampling noise.

5. **`--single-process`: 0 crashes in 936 launches with the
   `ENV VK_ICD_FILENAMES` pre-seed.** Without the pre-seed, ANGLE's
   `setenv()` races glibc `getenv()` in the async fontconfig init:
   25/336 = 7.4% natural crash rate, 15/15 under the LD_PRELOAD race
   amplifier; with it, 0/336 natural and 0/60 amplified (Wilson 95% upper
   bound 1.1%). See `entry.sh` and `upstream/ANGLE-setenv-race.md`.

## Binding rerun requirements

The comparative memory / egress / throughput numbers may only be published
from a run that has all of:

- **Matched cgroup accounting** — both fcvm clones and host-native containers
  measured as full-cgroup memory (same accounting basis on both sides), not
  firecracker-only PSS vs whole container.
- **Interleaved mode ordering** — egress modes drawn round-robin (or
  randomized) per rep, never one mode per contiguous block.
- **Repeated bursts** — every burst cell run enough times for a variance
  estimate (≥5), reported with spread.
- **`RUST_LOG=fcvm=debug`** on serve/restore so stage attribution comes from
  logs, not from a 100 ms-quantized retry loop.

---

# Run 2 — request-optimized path (CDP-direct + fast teardown) vs exec path

**2026-08-08, aarch64 Graviton, 64 cores / 125 GiB, kernel 6.18.3-fcvm, podman
4.9.3, Chromium 151.0.7922.71, fcvm 0.1.0.** Golden `cb-req-golden` (1 GiB,
rootless, warm Chromium). Raw: `results/reqbench-medfile/reqbench.jsonl`
(file-backed, headline) and `results/reqbench-med/reqbench.jsonl` (UFFD).
`results/` is gitignored; the harness and this ledger are the record.

Method actually used: 4 arms (`exec`, `cdp`, `cdp-fast`, `noop`) **interleaved
request-by-request from seed 20260808**, n=30 per arm after 3 discarded warmups,
`RUST_LOG=fcvm=debug`, medians with 20k-sample percentile-bootstrap CIs.
Contention: box quiescent, loadavg1 median 0.8 (min 0.4, max 2.1) for the
headline run. The drift control moved 0.4 ms, 95% CI [−11.8, +9.0] — **no
significant drift**, so the interleaved deltas are usable.

## The one idea

The exec path spends most of a request **starting a process to ask the
question**, not answering it. A warm Chromium already speaks a request protocol
(CDP); the win is deleting the per-request interpreter, not speeding it up.
Everything below follows from that.

## SURVIVED

1. **Deleting the per-request guest interpreter is the whole win: −181 ms of
   caller latency (file-backed, medium).** exec 565 ms [564, 614] → cdp 384 ms
   [376, 389], n=30 each; paired delta **−180.5 ms, 95% CI [−235.5, −176.7]**.
   The exec arm's own decomposition says where it goes: **exec handshake 8.79 ms
   [8.51, 8.96]** plus **209 ms [202, 216] of guest Python startup** (residual
   between the exec GO and render.py's first timestamp — labelled a residual
   because no fcvm log can see inside the guest). Both are deleted outright.

2. **The exec arm reproduces the published baseline.** 565 ms [564, 614] here vs
   the 573 ms file-backed medium figure this work is measured against. The
   harness is measuring the same thing.

3. **Chromium's in-guest TCP listener survives snapshot restore, and the CDP
   target id is stable across clones.** **1 distinct target id across 113
   successful clones** (54 UFFD + 59 file-backed) — it is frozen into the
   snapshot. `port_wait_ms` = **0.12 ms [0.12, 0.12]**: the forwarded port
   answers essentially instantly after restore. No reconnect logic, no
   re-listen, no vsock transport-reset problem — because the request path uses
   no vsock at all.

4. **The CDP-direct path costs LESS memory per clone, not more.** Peak
   firecracker PSS: exec **499.5 MiB** (n=6) vs cdp **464.6 MiB** (n=6) —
   ~35 MiB cheaper, because no Python interpreter is spawned in the guest. The
   one process this design adds, socat, is **2.0 MiB PSS / 4.1 MiB RSS**
   (0.59% of container PSS).

5. **Golden snapshot size is unchanged — exactly.** `memory.bin` is `--mem`
   bytes by construction and `vmstate.bin` is byte-identical (11539 B) between
   the exec-path golden and the CDP golden. The relay adds **0 bytes**.

6. **Firecracker teardown is ~3x cheaper than the synthetic extrapolation.**
   Per-child pidfd timing from one SIGKILL, n=6: **firecracker 19.7 ms**
   (16.4–23.3), namespace holder **0.2 ms**. Reclaim CPU for a file-backed clone
   is **0.00 ms [0.00, 0.00]**. The AGENTS.md synthetic table predicted 63.5 ms
   wall / 110 ms CPU per GiB — it was built from dirty anonymous pages, and its
   own caveat predicted this. Firecracker's guest memory is MAP_PRIVATE
   file-backed and mostly clean, so it unmaps far cheaper.

## REFUTED

1. **"Early response converts teardown from latency into throughput cost" — as
   applied to the SIGKILL fast path, REFUTED.** The caller saves nothing
   measurable (cdp → cdp-fast blocking **−12.5 ms, CI [−19.7, +3.0]**, crosses
   zero) while the machine pays **+610.4 ms, CI [+575.9, +620.3]** of extra
   wall time per request. It converts 0 ms of latency into 610 ms of extra VM
   lifetime.

   Cause, measured not guessed: **pasta takes 704 ms [700–707] to exit** after
   fcvm is SIGKILLed, versus 63 ms for fcvm's own SIGTERM cleanup. pasta is a
   child of fcvm but is *not* reaped by the pdeathsig fan-out on this path;
   fcvm's explicit cleanup (`src/network/pasta.rs:976`) is what normally kills
   it promptly, and SIGKILL skips exactly that. The "one signal, kernel-enforced,
   cannot leak" property holds for firecracker (19.7 ms) and the holder (0.2 ms)
   — it does **not** extend to pasta.

   The weaker claim DOES survive: moving the response ahead of teardown at all
   (exec → cdp) takes fcvm's ~72 ms of awaited teardown off the caller's path at
   ~0 reclaim CPU. Report that, not the SIGKILL variant.

2. **"The resident-server design would have a vsock transport-reset problem to
   solve" — moot, and the premise it was traded against was wrong.** No vsock is
   involved either way in the request path. See the AGENTS.md correction: the
   trade was justified against a `--forward-localhost` behaviour that does not
   exist.

## NEW DEFECT FOUND — must be fixed before this path ships

**The host-driven CDP path drops requests.** `WsClosed` (connection closed
mid-frame / during handshake), failing at ~5.25 s:

| Backend | cdp-arm requests | failures | rate |
|---|---|---|---|
| file | 60 | 1 | **1.7%** |
| UFFD | 60 | 6 | **10.0%** |

Root cause **not determined**. Candidates not yet separated: socat's per-connection
`fork` child, pasta's connection handling under restore, Chromium closing the
DevTools socket. The exec arm had **0** failures in 66 requests. This is a
reliability regression of the new path and no latency number should be quoted
without it.

## Numbers, file-backed medium, n=30/arm (headline table)

| | exec | cdp | cdp-fast | noop (control) |
|---|---|---|---|---|
| caller-blocking p50 | **565** [564, 614] | **384** [376, 389] | **372** [367, 383] | 63.5 [59.1, 68.1] |
| wall to VM gone | 565 [564, 614] | 455 [450, 488] | 1065 [1062, 1072] | 1070 [1064, 1074] |
| teardown reap wall | (inside fcvm) | 63 [63, 113] | 634 [628, 652] | 1006 [1004, 1007] |
| reclaim CPU | — | — | 0.00 [0.00, 0.00] | — |

Stage decomposition (medians, ms):

| exec arm | | cdp arm | |
|---|---|---|---|
| spawn→holder ready | 13.31 [13.25, 13.36] | spawn→CDP ready | ~64 (blocking − render) |
| holder→pasta ready | 29.8 [28.5, 32.5] | port_wait | 0.12 [0.12, 0.12] |
| pasta→resume done | 17.6 [17.2, 19.0] | resolve (`/json/list`) | 16.2 [15.8, 18.8] |
| resume→exec start | 0.59 [0.58, 0.60] | tcp | 0.07 |
| exec handshake | 8.79 [8.51, 8.96] | ws upgrade | 9.6 [8.1, 10.6] |
| **guest python up (residual)** | **209 [202, 216]** | enable | 4.99 [4.53, 5.44] |
| render.py total | 217 [212, 226] | connect_total | 31.0 [30.1, 31.8] |
| — connect | 13.00 [12.80, 13.20] | navigate | 192.0 [188.0, 196.4] |
| — navigate | 119.6 [117.9, 122.0] | screenshot | 91 [84, 98] |
| — screenshot | 85 [79, 88] | render total | 319.8 [315.7, 322.5] |

**The tradeoff, stated honestly:** the CDP path deletes 218 ms (handshake +
interpreter) but makes the render conversation itself **+103 ms** more expensive
(320 vs 217), because every CDP message now crosses socat + pasta instead of
guest loopback. Net −181 ms. `resolve_ms` (16.2 ms) is removable — the target id
is provably stable, so `--ws-url` can skip `/json/list` entirely. Untested.

## Still UNMEASURED

- `--ws-url` prewiring (predicted −16 ms; target-id stability is proven, the
  saving is not).
- Failure root cause above.
- Concurrency: every number here is n=1 clone at a time. No throughput or
  density claim is made, and none may be derived from these medians.
- Only `medium.html`. minimal/heavy not run in this pass.
- `test_bench_fast_teardown_leaks_nothing_clone` still has not been executed.
