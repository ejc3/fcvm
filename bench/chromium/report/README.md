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
`6fe16829-6ac7-4603-ad6e-3ed7a83c5a9e` holds the same sections and differs
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

That leaves the corpus mix as the publishable headline: 549.4 ms [467.9, 632.4],
n=202, 14 URLs cycled uniformly, 4 guest vCPUs, from
`results/reqbench-20260902-025115-corpus-c4` (DNS-verified: `dns-evidence.json`
verdict clean, first_mismatch null, diag violations none). Four corpus cells now
have a shipped record, a real workload and a verified resolver: the 2026-09-02
ladder at 2, 4 and 8 vCPU, and `results/reqbench-20260830-171007-corpus` at
712.6 ms [610.5, 808.5] on 2 vCPU. The headline is the 4 vCPU cell because the
ladder puts the knee between 2 and 4 (see "Corpus latency by guest vCPU count"
below), so 4 is the operating point; the 2 vCPU figures are quoted beside it
wherever it appears, because most of the rest of this report is 2 vCPU work.
The 695.7 ms at 4 vCPU that stood here before came from
`results/reqbench-20260816-123529-corpus`, which is withdrawn.

Regeneration follows the same rule: anything intended for publication is a
corpus run, not a fixture run.

## Known corrections outstanding

Recorded here so an editor does not have to rediscover them. None are stylistic;
each is a claim the evidence no longer supports:

1. Six sections quote records that no longer exist: "Three network modes,
   priced", "Concurrency and memory amortization", "The memory frontier",
   "Ablating the floor", "A second engine: WebKit", and the Kitesurf memory/CPU
   rows. The three network-mode run ids it cites (f0023333, b87bb625, 62962574)
   appear nowhere, and every surviving analysis.json (23 at audit time; the 14
   curated ones are committed under results/) records network_mode "rootless",
   so no non-rootless record exists to re-derive.
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

The 2026-08-16 corpus series was withdrawn on 2026-09-02 for live-DNS
contamination (the section below has the evidence). The headline moved from
695.7 ms at 4 vCPU to the DNS-verified 712.6 ms at 2 vCPU, the vCPU ladder,
its steps and its noise floor were withdrawn, the CPU-demand section lost its
latency captions, the elmundo note now names its cause, and the "TCP connect is
0.1 ms" network exclusion was withdrawn because the stage it quoted never
measured the page's network. `test_reqbench.DocLint` now fails if a doc cites
a withdrawn corpus record without saying so, or quotes a corpus figure that is
not a median, lo or hi of a DNS-verified record.

The headline is now the 4 vCPU 549.4 ms of the DNS-verified 2/4/8 vCPU ladder
measured on 2026-09-02, not the 2 vCPU 712.6 ms that replaced the withdrawn
figure first. Both cells are verified and both are quoted. This file previously
said 4 and 8 vCPU were "not measured on the fixed tree"; the ladder measures
them, and the rows that called the knee unmeasured are corrected.

## Corpus latency by guest vCPU count

A DNS-verified 2/4/8 vCPU ladder exists, measured 2026-09-02; it is below,
under "The verified ladder". The 2026-08-16 ladder that used to stand here is
withdrawn and stays withdrawn. Every run in that series (the ten gated
campaign runs `results/reqbench-20260816-*-corpus`, the 2 vCPU rerun
`results/reqbench-20260816-134130-corpus`, the three cpuprobe source runs, and
the earlier `results/reqbench-20260814-042319-uffd`) has a source_revision that
does not contain fcvm `90733b854e`. Before that commit pasta claimed the guest's
port 53 and redirected it to the host's own resolver, so the guest resolved the
corpus hostnames on the live internet instead of at the replay server;
`scripts/probe-pasta-dns-gateway.sh` reproduces the redirect with the pinned
pasta binary and no VM. Each directory carries a `WITHDRAWN` marker with the
per-run evidence; `campaign_summary.py` refuses a marked run, and refuses the
shape itself (hostname URLs with no recorded resolver) whether or not a marker
is present. The raw records survive in
`/home/ubuntu/src/fcvm-main/bench/chromium/results/`, the worktree the runs
executed in, byte-identical to the sha256 each analysis.json records under
`analysis_identity.inputs`, so the series can be re-analyzed without re-running.
Re-analysis does not rehabilitate it: the current reqanalyze exits 5 on every
record because the meta assigns no complete cell and the record names no
resolver; on the 2026-08-16 runs the 15 s stall gate also fails, on 16 to 22
elmundo load events per run, while the 2026-08-14 run has none over 15 s and
passes it.

What the records show, read from the same reqbench.jsonl files the figures
came from:

- `render.nav.dns_ms` (Chrome Navigation Timing) is nonzero in 8 of 460 render
  records of the withdrawn 4 vCPU run, seven of them between 149 and 158 ms.
  The verified 2026-08-30 run has 2 nonzero of 230, at 4.6 and 4.9 ms, and the
  three ladder runs have none of 230 each, with `render.nav.ttfb_ms` maxima of
  15.1, 2.5 and 2.2 ms at 2, 4 and 8 vCPU.
- `render.nav.ttfb_ms`: the verified run's maximum (15.0 ms) is below the
  withdrawn 2 vCPU run's median (16.2 ms); the withdrawn runs' maxima are
  1.0 to 7.4 s against a replay server on the host loopback.
- The corpus is frozen (`corpus-live/`, one commit), yet news.ycombinator.com
  produced 11 distinct screenshot hashes across 15 renders in each of the
  withdrawn 2 and 4 vCPU runs, against 1 across 15 in the verified run.
- www.elmundo.es: 30,912 ms median at 2 vCPU and 31,046 at 4 (cdp arm, n=14
  each), unmoved by a vCPU doubling; 3,842 ms in the verified run.
- Controls at matched 2 vCPU (withdrawn `reqbench-20260816-121054-corpus`
  against the verified run): noop 41.0 vs 44.8 ms (the verified run is slower),
  resolve 155.8 vs 154.3, connect_total 173.0 vs 163.8, screenshot 82.8 vs
  80.3, navigate 598.4 vs 409.1. The two runs were measured on different
  hosts: the withdrawn series on this box (`cell.host_kernel_release`
  7.0.14-fcvm-cd6cd2b4b52e, boot 80bfe10d), the verified run on a
  6.17.0-1019-aws host (boot 291f8bad), each on its own golden and fcvm
  binary. So neither the navigate change nor the 3.8 ms noop shift is
  attributed to the DNS fix. The withdrawn 982.9 is 37.9% above the
  verified 712.6 across that host change; how much of the gap is DNS is
  not measured.

### The verified ladder

The ladder was re-run on a source_revision that contains `90733b854e`
(`1e9e9b70937c`): three rungs measured back to back in one session, one runtime
bundle (`b1194fed78b8`), one golden per rung because the guest vCPU count is
baked into the snapshot. Every rung passed its gates: 202 measured non-warmup
cdp requests at zero failures, `dns-evidence.json` verdict clean with
first_mismatch null, diag violations none, teardown failures none.

| guest vCPUs | cdp p50 | CI | noop | record |
|---|---|---|---|---|
| 2 | 770.3 ms | [596.2, 807.8] | 44.9 ms | `results/reqbench-20260902-023115-corpus-c2/analysis.json` |
| 4 | 549.4 ms | [467.9, 632.4] | 45.1 ms | `results/reqbench-20260902-025115-corpus-c4/analysis.json` |
| 8 | 580.2 ms | [520.8, 645.9] | 45.8 ms | `results/reqbench-20260902-031115-corpus-c8/analysis.json` |

Index: `results/campaign-20260902-box2-ladder-summary.json`, which carries the
per-cell seals, DNS verdicts and load evidence.

2 to 4 vCPU is a step of 220.9 ms: 549.4 is 28.7% below 770.3, and all 14 URLs
are faster at 4 than at 2. 4 to 8 does not separate. Each rung's median sits
inside the other's interval, and 8 vCPU is slower than 4 on 11 of the 14 URLs,
faster only on the three heaviest (elmundo 2,907.2 against 2,945.2, rtp.pt
1,945.8 against 2,195.0, theguardian 894.9 against 913.7). So the knee is
between 2 and 4, and 4 vCPU is the operating point.

Two limits on that reading. The intervals are within-run: 202 requests of one
run, not run-to-run variance. The 2 and 4 intervals overlap between 596.2 and
632.4, so the step is read from the medians and the per-URL sweep, not from
disjoint intervals; neither median falls inside the other's interval, which is
what separates that pair from 4 against 8. And the ladder prices latency only:
twice the vCPUs per render halves how many renders a fixed core budget holds
concurrently, and no run here measures that.

The 4-to-8 difference is not in the render. Stage medians, cdp arm:

| rung | resolve | upgrade | Page.enable | navigate | screenshot | cdp p50 |
|---|---|---|---|---|---|---|
| 2 vCPU | 152.9 | 6.1 | 2.5 | 414.1 | 81.4 | 770.3 ms |
| 4 vCPU | 153.1 | 2.6 | 2.8 | 301.0 | 67.1 | 549.4 ms |
| 8 vCPU | 204.6 | 3.1 | 3.8 | 267.2 | 64.1 | 580.2 ms |

navigate and screenshot both keep falling from 4 to 8. What rises is `resolve`,
by 51.5 ms, and that stage is a poll, not a lookup. cdpdrive.py's
`resolve_target` GETs `http://{cdp_host}/json/list` until the answer carries a
page target, sleeping `RESOLVE_RETRY_S = 0.05` between attempts
(`cdpdrive.py:107,184`). `cdp_host` is `127.0.0.2:9222` in every record, an IP
literal, so nothing is name-resolved; the GET crosses the same forwarded
loopback path as `tcp_ms` and Chromium inside the guest answers it.

So the stage advances in 50 ms steps. `render.resolve_attempts` has median 4, 4
and 5 at 2, 4 and 8 vCPU, and pooled over the four verified runs the median
`resolve_ms` is 104.1 ms at 3 attempts, 153.6 at 4, 204.6 at 5 and 254.4 at 6.
Subtracting the sleeps leaves 3.1, 3.2 and 4.5 ms of non-sleep time per rung,
so the stage is almost entirely waiting. The 51.5 ms is one extra poll: at
8 vCPU the endpoint was not ready at the attempt that served the 4 vCPU rung.
The whole distribution shifts, not just the median, so this is not a boundary
crossing: mean attempts are 3.58, 3.97 and 4.95. By how much the 8 vCPU guest
was later is not measured. The stage resolves nothing finer than its own 50 ms
quantum, and it spans host and guest with no field separating the wait from the
request.

The box was not equally quiet at every rung. `load_max_1min` is 2.62 at
2 vCPU, 18.3 at 4 and 16.97 at 8, against 2.87 in the 2026-08-30 run, and the
per-request samples in the campaign index have medians 2.14, 4.41 and 4.19. The
quiet-box gate is checked at run start, and the noop arm, which restores, boots
and tears down without rendering, is the drift canary the analyzer rejects a
run on: it reads 44.9, 45.1 and 45.8 ms across the three rungs, so the extra
load did not move the lifecycle baseline.

The two verified 2 vCPU cells agree: this ladder's 770.3 [596.2, 807.8] and
the 2026-08-30 run's 712.6 [610.5, 808.5], each median inside the other's
interval, measured on different goldens, fcvm binaries and host boots
(`cell.host_boot_id` 291f8bad and 21ffa582). Both ran on kernel
6.17.0-1019-aws on aarch64. The ladder's `hostinfo.json` names its machine
(box parallel-box-2, instance i-0b8def825d4e9bcc2) and the 2026-08-30 run has
no `hostinfo.json`, so whether that is one box rebooted or two is not
established. That agreement is the cross-check that the two runs measure the
same thing.

The direction argument that stood here, derived from the six URLs whose
screenshots are byte-identical between each withdrawn rung and the 2026-08-30
run, is superseded by the ladder above and removed. No figure from the
withdrawn series is used to size the step.

Two things about the withdrawn table, so nobody restores it from memory:

- Its 2 vCPU rung, 982.9 ms, was the MAXIMUM of the five withdrawn uffd/minor
  2 vCPU cells in the series (916.9 to 982.9 ms, mean 949.0), which by itself
  inflated the 2->4 step from 253 ms to 287 ms; the eight withdrawn 2 vCPU
  cells across uffd/minor, uffd/copy and file backends and four fcvm binaries
  span 878.5 to 982.9 ms (mean 938.2).
- Its "the guest is CPU-starved rather than slow" argument rested on
  `Page.enable` costing 6.8 ms on 2 vCPUs against 3.9 on 4 while "TCP connect
  is 0.1 ms throughout". That 0.1 ms is `stages.tcp_ms`, which cdpdrive.py
  defines as its own TCP connect to the WebSocket endpoint: the harness on the
  host reaching the guest's forwarded CDP port on 127.0.0.2:9222 over loopback.
  It is not the page's connection to any origin and cannot rule network
  effects in or out. The page-side timings in the same records
  (`render.nav.dns_ms`, `render.nav.ttfb_ms`, above) show the renders waiting
  on live DNS. The conclusion is withdrawn, and the verified ladder above does
  not re-derive it. `Page.enable` there runs 2.5 ms at 2 vCPU, 2.8 at 4 and 3.8
  at 8, slower as cores are added, and the two verified 2 vCPU runs differ by
  more (2.5 against 4.3 ms, a 1.9 ms gap) than the ladder spans from 2 to 8
  (1.4 ms). The section makes no claim about why 2 vCPU is slower than 4.

**elmundo was waiting on live DNS.** The 31,046 ms median this file called an
unresolved guest-specific stall, and the "untested: DNS through the wildcard
override" hedge under it, were the defect above: elmundo's third-party request
chains, which the replay leaves unanswered, went out to the live internet and
waited for real timeouts. The DNS-verified run measures elmundo at 3,842 ms
median [3,714, 3,879], n=14, on 2 vCPUs, one screenshot form across all 14
renders. The 114 ms it used to add to the mix median and the 581.8 ms
elmundo-excluded figure are withdrawn with the run that produced them. The
remaining 1.5 s over the 2.36 s host-container load (2026-08-17 probe,
unretained) is not decomposed.

Records: `results/reqbench-20260902-023115-corpus-c2/analysis.json`,
`results/reqbench-20260902-025115-corpus-c4/analysis.json`,
`results/reqbench-20260902-031115-corpus-c8/analysis.json`,
`results/campaign-20260902-box2-ladder-summary.json`,
`results/reqbench-20260830-171007-corpus/analysis.json`,
`results/campaign-20260830-box2-summary.json`. Withdrawn, kept in the tree
with their markers: `results/reqbench-20260816-*-corpus/WITHDRAWN`,
`results/reqbench-20260814-042319-uffd/WITHDRAWN`,
`results/cpuprobe-20260816/WITHDRAWN`; `results/campaign-20260816-summary.json`
is a hand-written index of those cells, not a record, and is not cited.
Ledger: `../REVIEW.md`. Procedure: `../corpus_campaign.sh`.
