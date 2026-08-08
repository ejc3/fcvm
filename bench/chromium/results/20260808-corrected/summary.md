# A shared-nothing Chromium render on fcvm — measured, with every prior defect corrected

Every HTTP-like "request" here restores a **fresh microVM** from a golden snapshot of a warm
headless Chromium, drives one CDP render (page load + screenshot + DOM dump), and destroys the
clone. Nothing is shared between requests except the immutable snapshot.

This run replaces the retracted 2026-08-07 numbers. All six methodology defects from
[`REVIEW.md`](../../REVIEW.md) are corrected here, and several previously-published figures are
**refuted by this run** — those are called out explicitly rather than quietly dropped.

**Machine:** 64-core aarch64 (Neoverse, `cpu part 0xd40`), 125.5 GiB RAM, kernel
`6.18.3-fcvm-8ee6c35df0e1`. **Versions:** fcvm built from `origin/main` @ `63f0d375`
(`fcvm` sha256 `02180e29…`), golden image `724e1a6478ca`, guest Chromium 151.0.7922.71 (Debian
bookworm arm64). Full provenance in [`hostinfo.json`](hostinfo.json); raw statistics in
[`corrected.json`](corrected.json); generated tables in [`tables.md`](tables.md).

**VM shape:** 2 vCPU, 2048 MiB. **Reps:** R=12 per matrix cell, 3 reps per density cell,
5 bursts per throughput cell. **426 matrix requests, 0 failures.**

---

## The one idea

A shared-nothing request costs what it costs for two reasons that scale differently, and
**isolation is no longer the expensive one**:

- **O(isolation)** — what a private microVM costs you: snapshot restore, network attach, first
  egress. Measured **84 ms** (53 + 28 + 3.5), flat, and independent of the work being done.
- **O(launch + work)** — starting a process *inside* the guest (**225 ms**, almost all of it
  podman's exec entry, not virtualization) plus the render itself (**356 ms**: CDP handshake,
  page load, screenshot), which a host-native container pays too.

The hypervisor is 6% of the wall clock. The two things that dominate — a container exec path and
Chromium's own render — are exactly the two things that have nothing to do with the VM boundary.
The same split governs memory: a fixed shared snapshot paid once, plus a marginal per-clone cost
that ranges **35 -> 258 MiB** depending purely on which memory backend you pick.

---

## Per-request cost: where the wall clock goes

`rootless-proxy` / UFFD 4K / `medium` page / JPEG q80, n=12, medians with 95% bootstrap CIs.
(`corrected.json` -> `primary_cell.stages`)

![stage decomposition](charts/stage-decomposition.svg)

| stage | ms (95% CI) | what it is |
|---|---|---|
| restore (clone) | **52.9** (52.0–54.0) | fcvm clone spawn -> guest resumed |
| fcvm exec handshake | **28.0** (27.0–29.5) | restore -> GO sent (fcvm's own path) |
| guest command start | **224.5** (217.0–232.0) | GO -> driver's first output (podman exec entry) |
| first egress | **3.5** (3.0–4.0) | in-guest TCP connect to the host fixture site |
| CDP handshake | **16.7** (16.4–16.9) | `/json/list` + TCP + RFC 6455 upgrade — **per request** |
| page load | **204.0** (196.6–207.3) | Navigation Timing, in-guest |
| screenshot | **133.8** (120.8–144.2) | `Page.captureScreenshot`, JPEG q80 |
| DOM dump | **1.8** (1.6–2.1) | `DOM.getDocument` + outerHTML |
| **artifact — a reply could be sent** | **730.0** (708.4–741.0) | t0 -> screenshot in hand |
| teardown (post-artifact) | **175.1** (150.4–194.9) | after the artifact already exists |
| **total** | **890.6** (869.6–929.0) | t0 -> clone gone |

**Exec waiting is now fully attributed (defect 4).** With `RUST_LOG=fcvm=debug`, the exec client's
retry ladder logged **0.0 ms of cumulative retry wait in all 426 requests** (max 0.0). The old
100 ms quantization is not merely smaller — it is *absent*, because the client connects on the
first attempt every time. The remaining 224.5 ms "guest command start" is attributed by
**measurement, not assertion**, using a shell-only control interleaved into the same schedule:

| control (interleaved, n=12) | artifact ms | isolates |
|---|---|---|
| `noop-sh` — shell only, no interpreter | **254.1** | fcvm exec path + podman entry |
| `noop` — Python driver, no egress, no render | **280.5** | the above + interpreter startup |
| difference | **26.4** | Python interpreter startup |

So of the 224.5 ms: **~26 ms is Python**, and the remaining **~173 ms is fcvm's `--exec` podman
entry** — a known, unfixed container-exec cost, larger here than the ~94–100 ms previously
scavenged from logs. It is the single biggest addressable item in the request.

### Against a real host-native baseline

Same page set, same screenshot format, same driver, measured in the same run and interleaved
(`corrected.json` -> `host_baseline`).

| configuration | artifact ms (95% CI) | n |
|---|---|---|
| host-native warm container, `medium` | **217.6** (202–229) | 24 |
| host-native warm container, `minimal` | 174.1 (169–175) | 12 |
| host-native warm container, `heavy` | 1381.4 (1357–1397) | 12 |
| **fcvm shared-nothing clone, `medium`** | **730.0** (708–741) | 12 |
| host-native **cold** container, `medium` | 1269.9 (1268–1272) | 3 |
| fcvm **cold boot** (no snapshot), `medium` | 5287.7 (5286–5290) | 2 |

**fcvm's shared-nothing overhead over a warm host-native container is 512 ms** (730.0 − 217.6),
of which only ~84 ms is isolation. A shared-nothing clone is **1.7x faster than starting a fresh
container** (1270 ms) and **7.2x faster than a cold VM boot**, while giving stronger isolation
than the warm pool it is compared against.

---

## Memory density per concurrent request

**Defect 1 (matched basis) and defect 5 (slope AND intercept) both apply here.** Every clone's
*entire* process set — the `fcvm` supervisor, `firecracker`, `pasta`, and the `unshare` holder
(5 processes per clone) — is confined to one leaf cgroup before exec, so the fcvm side and the
container-pool side are measured over the same kind of process set. Three independent bases are
reported and reconciled. N = 1, 2, 4, 8, 16; 3 reps each; 15 fitted points per cell; **0 samples
dropped for contamination**.

![memory density](charts/memory-density.svg)

| backend | cgroup `memory.current` MiB/req | MemAvailable delta MiB/req | intercept MiB (cgroup) | req/GiB @ N=8 | @ N=16 |
|---|---|---|---|---|---|
| **UFFD `minor` + 2 MiB hugepages** | **34.7 ± 0.4** | 29.1 ± 1.1 | 18 ± 3 | **27.7** | **28.6** |
| UFFD `minor`, 4K | **132.5 ± 1.0** | 123.5 ± 1.5 | 35 ± 8 | 7.5 | 7.6 |
| file-backed MAP_PRIVATE, 4K | **143.5 ± 0.4** | 138.8 ± 1.1 | 17 ± 3 | 7.0 | 7.1 |
| UFFD `MISSING`+`COPY`, 4K | **257.8 ± 1.1** | 255.0 ± 5.0 | 30 ± 9 | 3.9 | 3.9 |
| *host-native warm container pool* | *156.5 ± 5.0* | *150.6 ± 8.1* | *250 ± 46* | *5.5* | *6.0* |

All fits r2 >= 0.995 over N=1–16. Uncertainties are OLS standard errors.

**Reconciling the bases.** cgroup `memory.current` and the whole-machine `MemAvailable` delta —
two completely independent instruments — agree within 5–10 MiB on every cell. That agreement is
the reason these numbers can be trusted at all. PSS disagrees *by design*: it splits shared pages
across mappers, so it reports a smaller slope and a large intercept (file-4K: slope 110.8 ± 0.3
with a **411 MiB** intercept — that intercept *is* the shared snapshot, paid once). For
hugepages+`minor` the split is extreme (PSS slope 4.3 MiB/req vs cgroup 34.7) precisely because
almost every page really is shared. **Use the cgroup/MemAvailable figures for capacity planning;
the PSS slope answers a different question.**

**`--uffd-mode minor` is the headline, and this is its first measurement.** With hugepages it
costs **34.7 ± 0.4 MiB per concurrent request** — **4.5x better than the warm container pool**
and **7.4x better than UFFD copy-mode**. At 4K it is still the best 4K option (132.5 vs 143.5
file-backed vs 257.8 copy).

**On beating the container pool.** The retracted claim ("129 MiB/req beats a pool at 151") is
*directionally* reproduced on a properly matched basis but the margin is much smaller than
claimed: file-backed is **143.5 ± 0.4** vs pool **156.5 ± 5.0** — a 13 ± 5 MiB (8%) win, not a
comfortable one. UFFD copy-mode **loses** to the pool outright (257.8 vs 156.5). Only the `minor`
modes win convincingly. Note also the pool's very different intercept (250 ± 46 MiB vs ~20 MiB),
which is why req/GiB is quoted at concrete N rather than as `1024/slope`.

---

## Egress mode comparison — and why no ordering is published

All six modes were drawn **request-by-request from one seeded shuffle** (seed `20260808`), with
all serves running concurrently, plus two control arms in the same stream. Mean schedule position
per mode was 182–237 against a uniform expectation of 207 — no mode ran systematically early or
late.

![egress modes](charts/egress-modes.svg)

**The routed 1-second stall is gone.** First-egress-after-restore is **3.0 ms** on *every* mode,
routed included (it was ~1002 ms). That fix is confirmed on merged main.

**Drift is measured and near zero.** The joint model's time term is **−68.8 ± 51.9 ms/hour**
(1.3 sigma — not significant), and the pure-orchestration `noop` control shows no trend. The
confound that invalidated the previous comparison is gone.

**But the ordering still does not reproduce, and I will not publish one.** Comparing this run's
mode contrasts against the earlier (contended) run:

| mode (vs `bridged`) | clean run | earlier run | shift |
|---|---|---|---|
| rootless-proxy | −67.7 ± 8.4 | −16.1 ± 36.5 | −51.6 |
| rootless-pasta | −24.2 ± 8.4 | −1.9 ± 36.6 | −22.3 |
| rootless-proxy6 | −7.4 ± 8.4 | +42.2 ± 36.5 | −49.5 |
| routed | +24.1 ± 8.4 | +29.1 ± 36.7 | −5.0 |
| rootless-pasta6 | +40.9 ± 8.4 | +16.3 ± 36.5 | +24.7 |

Within this run the contrasts look significant (SE 8.4 ms), but **run-to-run shifts reach 52 ms —
six times the within-run SE**, and the IPv6 modes swap places. The within-run CI therefore
understates the true variability of a mode effect. What survives: **the two IPv4 rootless modes
(`proxy`, `pasta`) are the fastest two in both runs**, and the entire spread is ~110 ms out of
~800 ms (14%). Anything finer is not supported.

*(Incidentally, the earlier run's residual SE is 4.3x larger — 36.5 vs 8.4 ms. That is a
quantitative fingerprint of the contention it was measured under.)*

---

## Throughput: bursts and sustained rate disagree, and the disagreement is the result

**The burst is the experimental unit** (defect 3): 5 bursts per cell, median with bootstrap CI
over bursts; clones within a burst are pseudoreplicates and are *not* counted as n.

![burst throughput](charts/burst-throughput.svg)

| cell | N=4 | N=8 | N=16 |
|---|---|---|---|
| UFFD minor + hugepages | 2.13 (2.04–2.15) | 4.04 (3.90–4.05) | **7.39 (7.16–7.67)** |
| file-backed 4K | 1.75 (1.60–1.80) | 3.50 (3.19–3.54) | 6.44 (6.37–6.70) |
| UFFD copy 4K | 1.23 (1.21–1.24) | 2.35 (2.30–2.37) | 4.31 (4.17–4.35) |
| UFFD minor 4K | 1.14 (1.13–1.43) | 2.19 (2.15–2.23) | 4.09 (4.00–4.13) |

Sustained phase, reported **separately** (60 s at each target rate):

| cell | target | launched | completed | achieved rps | p50 latency |
|---|---|---|---|---|---|
| file-backed 4K | 8 rps | 462 | **462** | **7.26** | 659 ms |
| file-backed 4K | 4 rps | 234 | 234 | 3.79 | 615 ms |
| UFFD minor 4K | 4 rps | 236 | 236 | 3.72 | 1098 ms |
| UFFD minor 4K | 8 rps | 459 | 458 | **2.78** | 1224 ms |

**The two phases disagree, and here is why.** For file-backed, sustained (7.26 rps) *exceeds* the
N=16 burst (6.44 rps). A burst forces 16 simultaneous cold restores that contend with each other;
a sustained stream pipelines them. **The burst number therefore understates capacity and should
not be quoted as throughput** — which is exactly the error the previous report made in leading
with a burst figure its own sustained phase contradicted.

**UFFD `minor` saturates under sustained load.** At 8 rps it completed 458 requests but took
165 s to drain them (2.78 rps achieved) with latency climbing to 1224 ms, and one request hit the
120 s timeout. Its excellent *density* does not come with matching *sustained throughput* at 4K:
the single serve process becomes the bottleneck. **`minor` buys memory, not rate.**

---

## Two Chromium settings, re-measured on merged main

Both were interleaved into the same schedule rather than inherited from the old run.

| change | metric | measured here | previously claimed |
|---|---|---|---|
| JPEG q80 vs PNG | screenshot stage | **−28.8%** (−52.9 ms, CI −63.8…−44.8) | −40% |
| JPEG q80 vs PNG | whole request (artifact) | **−8.3%** (−65.8 ms, CI −83.1…−42.6) | −21% |
| JPEG q80 vs PNG | artifact bytes | −52.5% (130 050 -> 61 765) | — |
| site-isolation off | Chromium **PSS** | **−3.6%** (−14.1 MiB, CI −17.3…−12.8) | −23% (RSS) |
| site-isolation off | Chromium RSS | −20.5% (−192 MiB) | −23% |
| site-isolation off | renderer processes | 12 -> 10 | fewer |

**Two corrections.** JPEG is worth keeping (it is free and it is real), but **the −21% per-request
claim is not reproduced — it is −8.3%**, because the screenshot is only ~18% of the request.

**The site-isolation memory claim was an accounting artifact.** RSS counts every shared page once
per process, so removing 2 of 12 processes "saves" 20.5% on RSS while the true saving on PSS is
**3.6% — 14 MiB, not 192 MiB**. This is precisely the trap `AGENTS.md` warns about, and the
previous −23% figure should not be used.

---

## Contention: recorded, not asserted

This is a shared box. Load average is sampled every 5 s for the whole run
(`samples/loadavg.jsonl`) and every phase is reported with the load it actually ran under
(`corrected.json` -> `load`). On **64 cores**, load 2.0 is ~3% utilization.

| phase | load1 median | p90 | max |
|---|---|---|---|
| matrix (p3) | 2.04 | 5.58 | 8.87 |
| density cells | 0.56–0.66 | ~2.2 | 6.32 |
| burst cells | 0.65–0.66 | 2.23 | 6.32 |
| sustained cells | 4.75–7.80 | 9.2–14.4 | 19.9 |

The sustained rows are **self-load** — that phase generates the concurrency it measures. For the
matrix, a regression of request latency on the load at each request's timestamp gives
**−27.9 ± 25.7 ms per load unit: not significant**, so contention did not measurably drive the
latencies reported here.

**Provenance caveat, stated plainly.** An earlier run of this same harness was contaminated by
concurrent workloads on this box and by a foreign rebuild of `target/release/fcvm` carrying
uncommitted changes to `src/uffd/server.rs`. That run is **discarded, not published**. Every
number above comes from the single run in this directory, executed with a binary built from a
pristine `origin/main` checkout (sha256 recorded in `hostinfo.json`) so that nothing under test
could change mid-benchmark.

---

## Reproducing

```bash
# build a binary from a known tree, then pin the run to it
git worktree add /tmp/pristine --detach origin/main && (cd /tmp/pristine && make build)
FCVM=/tmp/pristine/target/release/fcvm \
RESULTS=bench/chromium/results/<stamp> \
  bench/chromium/bench.sh run
python3 bench/chromium/analyze.py bench/chromium/results/<stamp>
python3 bench/chromium/charts.py  bench/chromium/results/<stamp>
```

`RUST_LOG=fcvm=debug` is the harness default — stage attribution depends on it.
