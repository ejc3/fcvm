# Report source of record

`shared-nothing-renders.html` is the source for the published benchmark report:
https://claude.ai/code/artifact/bc59e62a-a8b3-49f1-b759-878061c94a2e

## Why this exists

The report lived ONLY as an artifact URL. `git ls-files bench/` returned 919
files, including the entire harness, and no source for the document that quotes
it. The only other copy was a tool-result cache in a session scratchpad. That is
the same exposure that already cost the measurements: the probe-set directories
the report cites by name were destroyed with an instance-store wipe on
2026-08-15 and do not exist on any filesystem, worktree, or scratchpad.

`bench/chromium/AGENTS.md` already required a source of record. This is it.

## Publishing

The file is the artifact BODY: a `<title>`, a `<style>`, and a `<main>`. It
carries no `<!doctype>`, `<html>`, `<head>` or `<body>` — the publisher wraps it.

Edit here, then republish to the SAME artifact so the URL is stable:

    Artifact(file_path="bench/chromium/report/shared-nothing-renders.html",
             url="https://claude.ai/code/artifact/bc59e62a-a8b3-49f1-b759-878061c94a2e",
             favicon="🔬",
             description="Per-request-isolated Chromium in Firecracker microVMs, measured on Cloudflare's 14-URL corpus")

`favicon` is REQUIRED — a publish without it is rejected, and the incantation
above is meant to be pasted. Keep the same emoji across redeploys: readers find
the tab by its icon. `description` is the gallery subtitle; omitting it on a
redeploy drops the one already there.

Publishing WITHOUT `url=` creates a second artifact. That already happened once:
`6fe16829-6ac7-4603-ad6e-3ed7a83c5a9e` holds the same 15 sections and differs
only in a cache-busting `<base href>`. It is stale the moment this file changes,
and it is public.

## Publication rule: corpus only

**Published numbers come from the Cloudflare 14-URL corpus mix, and nothing
else.** `medium.html` is a synthetic fixture: 1.4 KB of HTML, a 14-rule
stylesheet, 806 B of JS and four generated PNGs, served with `Cache-Control:
no-store` so every render re-fetches and re-compiles. That determinism is
exactly what makes it a good MICRO-BENCHMARK for optimisation work, where the
question is whether a configuration change moved a number. It is not a workload
anyone runs, so it must not carry a published figure.

What this rule costs the current document, so nobody rediscovers it:

- The verdict box's second headline, `348.7 ms direct-CDP p50`, is a fixture
  number. Not publishable as a headline.
- The whole "Fixture latency ladder, direct CDP" section is fixture-based:
  RB, AB, PF, HM, HK, NC, FG. Keep it as optimisation evidence, clearly marked,
  or cut it.
- "Where the isolation premium lives" decomposes a fixture render against the
  host container. Same treatment.
- "Three network modes, priced" is fixture-based AND has no surviving record.

That leaves the corpus mix (615.1 ms [499.5, 719.0], n=202, 14 URLs cycled
uniformly) as the publishable headline, and it is the one figure with both a
surviving sealed record and a real workload behind it. See "The 615.1 ms
headline does not reproduce" below: it remains the only publishable WORKLOAD,
but the figure itself is dated rather than current.

Regeneration follows the same rule: anything intended for publication is a
corpus run, not a fixture run.

## Known corrections outstanding

Recorded here so an editor does not have to rediscover them. None are stylistic;
each is a claim the evidence no longer supports:

1. Six sections quote records that no longer exist: "Three network modes,
   priced", "Concurrency and memory amortization", "The memory frontier",
   "Ablating the floor", "A second engine: WebKit", and the Kitesurf memory/CPU
   rows. The three network-mode run ids it cites (f0023333, b87bb625, 62962574)
   appear nowhere, and all 23 surviving analysis.json files record
   network_mode "rootless", so no non-rootless record exists to re-derive.
2. Provenance says the probe sets are "kept alongside the run index". They are
   not kept. That sentence is false as published.
3. "Open measurements" still lists network-mode A/B/C as open while a full
   section prices it. Do NOT resolve that by deleting the clause: it is the last
   in-document hedge on the section with zero retrievable evidence. Demote the
   section instead.
4. The five headline runs are not mutually seal-comparable: five bundles, five
   revisions, three snapshot tags. Only the -huge pair shares a seal, so any
   cross-run subtraction crosses a seal boundary and should say so.

## Corrections applied

Kept as a record of what was wrong, because a corrections file that only ever
grows is a file nobody acts on.

The 34.7 MiB/clone huge-minor cell was filed here as a correction to
`shared-nothing-renders.html`, which never published it. It lives in
`bench/chromium/README.md` and `bench/chromium/REVIEW.md` — the second being the
file `bench/chromium/README.md` tells readers to "read before quoting anything
from this directory". Filing it against the wrong document left the overclaim
standing in the two files a reader is pointed at. Both are now corrected in
place: 34.7 counts only non-hugetlb memory, since cgroup2 on this host mounts
without `memory_hugetlb_accounting` and carries no hugetlb controller (verified:
`mount | grep cgroup2` shows `rw,nosuid,nodev,noexec,relatime,nsdelegate,memory_recursiveprot`),
and the pool was pre-allocated before the sample, so MemAvailable cannot move
for those pages either. On the pool-consumption basis, hugepage-minor is
553-611 MiB per concurrent clone against 133-146 at 4K — it loses on memory and
buys render latency.

Eleven further defects were found by an adversarial review of this branch and
fixed in the same pass:

- "Neither mode's render or memory regresses with concurrency at 1 GiB" was
  refuted by the table directly above it: density render goes 476, 461, 448,
  then 767 ms at N=32. Now stated as the regression it is.
- The Bottom line called hugepage per-clone memory "unmeasured and the next
  experiment" while two sections measure and publish it.
- "124-298 MiB per concurrent clone at 4 K pages": 298 appears nowhere in the
  document; the tables give 133-146 at 1 GiB and 108-203 across both guest sizes.
- WebKit's clean N=1 marginal (147) was compared against Chromium's 124, an
  ablation floor probe. The matched clean cell is 146, so the engines are at
  parity.
- Upstream PR #5696 was cited for two different fixes. It is the dump_dirty PR
  (verified against the firecracker repo); the vsock muxer change was never
  filed upstream at all, and now says so.
- "Plus five commits ... Ranked by relevance:" listed four bullets. The branch is
  six commits ahead, five non-merge; the serial-interrupt bullet covers two of
  them (fix plus regression test).
- "On 4K pages copy is 14% faster than minor" inverted the base: 532.5 vs 607.9
  makes minor 14% slower and copy 12% faster. Restated to match the body.
- "Across all 370 LD clones" cannot be reconciled with the rung list (nine rungs,
  199 clones per fill mode) and the record is gone, so the unverifiable total is
  no longer asserted.
- Kitesurf's Chromium column was called "their isolated Chromium" in one place
  and a warm pool in three others; the headline 1.8x ratio depends on which.
- "0 failures anywhere" is scoped to the runs in that table; a bridged attempt
  was refused by the zero-failure gate.
- The run-id legend omitted NM, and the publish incantation omitted the required
  `favicon` argument, and the section count said 15 against an actual 14. (The
  2026-08-16 reproduction section brings it back to 15; the count above is
  current.)

## The 615.1 ms headline does not reproduce

Regenerated 2026-08-16, same host, same 14 URLs, same knobs: seven gated corpus runs,
all publishable, zero failures. 2 vCPUs measures **951.2 ms**, 4 measures 706.3,
8 measures 654.5. The guest is CPU-starved rather than slow -- one `Page.enable`
round trip costs 15.4 ms on 2 vCPUs and 3.4 ms on 4 -- and the host-container
control moved only +9.5% because it gets all 64 cores.

Ruled out with the run that did it: working-set replay, the memory backend,
the firecracker binary (including rebuilding the sealed run's own branch), port
forwarding, the restore path, metric drift, host CPU, guest CPU, guest kernel.
Not established: that the container image is the only remaining difference.

Consequence for this file's publication rule: the corpus mix is still the only
publishable workload, but 615.1 is now a DATED measurement under a partly
unrecorded configuration, not a current figure. Every latency number in the
report comes from that same 2026-08-13/14 campaign and inherits the caveat.

Records: `results/reqbench-20260816-*-corpus/analysis.json`,
`results/campaign-20260816-summary.json`. Procedure: `../corpus_campaign.sh`,
which did not exist before this reproduction -- regenerating the benchmark
required reverse-engineering the invocation out of a sealed `analysis.json`.
