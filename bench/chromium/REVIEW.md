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
