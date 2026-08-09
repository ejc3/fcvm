# Adversarial review ledger — chromium shared-nothing bench

Three runs are on record. **Quote only from `20260808-corrected`.**

| run | date | verdict |
|---|---|---|
| `20260807-full` | 2026-08-07 | comparative numbers **retracted** on methodology (six defects) |
| `results/20260808-corrected` | 2026-08-08 | six defects corrected; numbers below are the current record |
| `reqbench` CDP A/B | 2026-08-08 | **WITHDRAWN IN FULL** — see "The CDP-path A/B" below. Harness defects invalidate every figure it produced. |

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

**The earlier CDP-path availability A/B is withdrawn.** Its arms were not
comparable, so its `file 1/60, UFFD 6/60, exec 0/66` counts must not be used to
attribute failures to a transport component or to describe current behavior.

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
| 1 | "fcvm marginal memory beats a warm container pool (129 vs 151 MiB)" | **SUPPORTED, but much smaller than claimed, and only for some backends.** On a matched cgroup basis: file-backed **143.5 ± 0.4** vs pool **156.5 ± 5.0** MiB/req — an 8% win, not a comfortable one. UFFD copy-mode **loses** (257.8 ± 1.1). UFFD `minor` wins clearly (132.5 ± 1.0 at 4K; **34.7 ± 0.4** with hugepages). |
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

- **`--uffd-mode minor`, first measurement.** With 2 MiB hugepages: **34.7 ± 0.4 MiB per
  concurrent request** (MemAvailable basis 29.1 ± 1.1) — 4.5x better than the warm container pool
  and 7.4x better than UFFD copy-mode. At 4K: 132.5 ± 1.0.
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
