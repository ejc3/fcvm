# Adversarial review ledger — chromium shared-nothing bench

Two runs are on record. **Quote only from the second.**

| run | date | verdict |
|---|---|---|
| `20260807-full` | 2026-08-07 | comparative numbers **retracted** on methodology (six defects) |
| `results/20260808-corrected` | 2026-08-08 | six defects corrected; numbers below are the current record |

The 2026-08-08 run: 426 matrix requests / 0 failures, R=12 per cell, 3 reps per density cell,
5 bursts per throughput cell, one seeded interleaved schedule (`seed=20260808`),
`RUST_LOG=fcvm=debug`, per-clone cgroup accounting, load sampled every 5 s. Binary built from a
pristine `origin/main` @ `63f0d375` checkout and pinned via `FCVM=` so nothing under test could
change mid-run (sha256 in `hostinfo.json`).

---

## Previously REFUTED claims — status after the corrected run

| # | claim | status now |
|---|---|---|
| 1 | "fcvm marginal memory beats a warm container pool (129 vs 151 MiB)" | **SUPPORTED, but much smaller than claimed, and only for some backends.** On a matched cgroup basis: file-backed **143.5 ± 0.4** vs pool **156.5 ± 5.0** MiB/req — an 8% win, not a comfortable one. UFFD copy-mode **loses** (257.8 ± 1.1). UFFD `minor` wins clearly (132.5 ± 1.0 at 4K; **34.7 ± 0.4** with hugepages). |
| 2 | any egress-mode *ordering* | **STILL NOT SUPPORTED.** The confound is gone (drift term −68.8 ± 51.9 ms/h, n.s.) and within-run SE is now 8.4 ms, but run-to-run shifts reach 52 ms and the IPv6 modes swap rank. Only "the two IPv4 rootless modes are fastest" reproduces; total spread ~110 ms of ~800 ms. |
| 3 | "16 clones sustain 5.5–6.3 req/s" | **SUPERSEDED.** With the burst as the experimental unit (5 bursts/cell): file-backed N=16 = **6.44 rps (CI 6.37–6.70)**, hugepage-minor = **7.39 (7.16–7.67)**. But the burst *understates* capacity — sustained file-backed reaches **7.26 rps** with 462/462 completed. **Do not quote burst figures as throughput.** |
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

- **`--uffd-mode minor`, first measurement.** With 2 MiB hugepages: **34.7 ± 0.4 MiB per
  concurrent request** (MemAvailable basis 29.1 ± 1.1) — 4.5x better than the warm container pool
  and 7.4x better than UFFD copy-mode. At 4K: 132.5 ± 1.0.
- **`minor` buys memory, not rate.** At a sustained 8 rps the 4K `minor` cell needed 165 s to
  drain 459 requests (2.78 rps achieved, p50 1224 ms, 1 request timed out) while file-backed held
  7.26 rps with 462/462 complete. The serve process is the bottleneck.
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

## Harness bugs found and fixed while producing this run

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
