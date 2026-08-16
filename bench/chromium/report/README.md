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
             url="https://claude.ai/code/artifact/bc59e62a-a8b3-49f1-b759-878061c94a2e")

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
surviving sealed record and a real workload behind it.

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
4. The huge-minor memory cell (34.7 MiB/clone) is not a memory measurement.
   This host has no memory_hugetlb_accounting, so the cgroup cannot see hugetlb
   pages at all, and the pool was pre-allocated before the sample so MemAvailable
   cannot move either. The N=16 cell demanded 32 GiB of hugepages and published
   34.7 MiB/clone. It is defensible only as "non-hugetlb memory".
5. The five headline runs are not mutually seal-comparable: five bundles, five
   revisions, three snapshot tags. Only the -huge pair shares a seal, so any
   cross-run subtraction crosses a seal boundary and should say so.
