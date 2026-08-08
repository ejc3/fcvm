# Chromium shared-nothing render benchmark

Measures the cost of a **shared-nothing, per-request Chromium render** on fcvm:
every HTTP-like "request" restores a fresh clone from a golden snapshot of a
warm headless Chromium (renderer, JIT, network service, raster/encode all hot),
drives one CDP render (screenshot + DOM dump), and destroys the clone. Nothing
is shared between requests except the immutable snapshot.

Axes:

- **Egress path** — all six distinct paths a clone can use to reach the outside
  world (`rootless-proxy`, `rootless-pasta`, `rootless-proxy6`,
  `rootless-pasta6`, `bridged`, `routed`), against a host-served fixture site,
  plus an in-guest control arm that renders the same bytes with no external
  network.
- **Memory restore** — `uffd` (snapshot serve + lazy UFFDIO_COPY) vs `file`
  (MAP_PRIVATE page-cache sharing), each at 4K and 2MB hugetlbfs pages
  (file x huge degrades to an implicit per-clone UFFD server — Firecracker
  rejects the File backend for hugepage snapshots — and is reported as such).
- **Baselines** — host-native podman cold and warm-pool renders (the physics
  floor), and fcvm cold boot (no snapshot).

Outputs per request: `artifact` latency (t(RENDER_OK) − t0, i.e. when a reply
could have been sent) and `total` (including destroy), decomposed into
restore / exec / egress-ready / in-guest Navigation Timing / screenshot stages.
Fan-out phases add burst latency and marginal memory per concurrent request.

## Files

| file | role |
|---|---|
| `../../Containerfile.chromium-bench` | the golden image: Debian chromium + python3 driver/pageserver, warm-point `/ready` health endpoint, and the `ENV VK_ICD_FILENAMES` pre-seed (see below) |
| `entry.sh` | container entry: pageserver → warm Chromium via CDP → touch ready-file → hold. Carries the full write-up of the ANGLE setenv/getenv crash workaround |
| `pageserver.py` | in-guest fixture server (`Cache-Control: no-store`, `/ready` gate for `--health-check` golden snapshots) |
| `render.py` | per-request CDP driver (stdlib-only WebSocket client); prints one machine-parsable `RENDER_OK` line with per-phase timings |
| `bench.sh` | host-side harness: golden snapshots, egress matrix, fan-out, baselines (see phases below) |
| `hostserver.py` | host-side "simulated external site": dual-stack bind, optional self-signed TLS, same `pages/` bytes as the image |
| `report.py` | `sample` (host memory + per-clone PSS one-liner) and `finalize` (requests/samples → `raw.json` + `report.md`) |
| `gen_images.py` | regenerates the deterministic PNG fixtures in `pages/` (stdlib only) |
| `pages/` | fixture site: `minimal` / `medium` / `heavy` / `warmup` HTML + CSS + JS + 4 PNGs, byte-identical whether served in-guest or from the host |
| `upstream/` | ready-to-file upstream report + patch for the ANGLE `setenv(VK_ICD_FILENAMES)` vs `getenv()` startup SIGSEGV (see `upstream/ANGLE-setenv-race.md`) |

## Build the golden image

```bash
# repo root context (.dockerignore excludes target/)
# --format docker is LOAD-BEARING: podman's default OCI format drops the image's
# HEALTHCHECK with only a warning, and fcvm treats a MISSING healthcheck as a
# pass — so the golden snapshot would fire on a COLD browser.
podman build --format docker -t localhost/chromium-bench -f Containerfile.chromium-bench .

# ...and verify it survived — this FAILS (exit 1) if the OCI format dropped it.
# It must FAIL, not print a warning: fcvm treats a MISSING healthcheck as a PASS
# (src/health.rs AND-logic), so a dropped HEALTHCHECK means the golden snapshot
# fires on a COLD browser and silently inflates page load, screenshot, artifact
# and total for every restore in the run. `bench.sh` — the harness
# `make bench-chromium` actually runs — has no build step and no healthcheck
# check, so on that route this line is the ENTIRE verification.
podman image inspect localhost/chromium-bench --format '{{json .HealthCheck}}' \
  | grep -q cdp_health || { echo 'FATAL: image has no HEALTHCHECK (OCI format drop?)'; exit 1; }

# host smoke test, no VM:
podman run -d --name cb localhost/chromium-bench
podman logs -f cb          # wait for CHROMIUM_BENCH_READY
podman exec cb python3 /opt/bench/render.py http://127.0.0.1:8000/medium.html \
    --out-prefix /tmp/medium
```

The image pre-seeds `VK_ICD_FILENAMES` in its environment (and `entry.sh`
re-exports it): without it, ANGLE's in-process-GPU `setenv()` races glibc's
`getenv()` in the async fontconfig init and Chromium SIGSEGVs at ~7% under
launch concurrency. The full analysis, measurements, and the upstream report
live in `entry.sh`'s comment block and `upstream/ANGLE-setenv-race.md`. Do not
remove the pre-seed until the upstream bug is fixed in the shipped Chromium.

## Run

```bash
make bench-chromium          # everything: build fcvm, then bench.sh run
# or directly:
bench/chromium/bench.sh run
```

Phases (each runnable alone; `phase1`/`phase3`/`phase4`/`phase5` re-run
`phase0` first; reuse one results dir across invocations with
`RESULTS=bench/chromium/results/<stamp>`; there is no phase 2):

| command | what it does |
|---|---|
| `bench.sh phase0` | probe mode availability (sudo, host IPv6, hugepage pool), write `hostinfo.json` / `availability.json`, sync the image into root podman for sudo modes |
| `bench.sh phase1` | boot + warm golden VMs and take the golden snapshots (rootless, REDIRECT-flushed "noredir" for the pasta arms, bridged/routed, hugepages) |
| `bench.sh phase3` | per-request matrix: every egress mode x every fixture page, UFFD arm + file-backed arm + in-guest control |
| `bench.sh phase4` | fan-out: burst latency and sustained-rate memory density over the 2x2 {uffd,file} x {4K,huge} matrix |
| `bench.sh phase5` | baselines: host podman cold/warm, fcvm cold boot, warm host-native pool contrast |
| `bench.sh phase6` | `report.py finalize` → `raw.json` + `report.md` in the results dir |

Env knobs (see `bench.sh` header for the full list): `R` (reps, default 12),
`R_CONTROL`, `R_COLD`, `REBUILD=1` (rebuild the image), `SKIP_SUDO=1`,
`FANOUT_MODE`, `BURST_NS`, `SUST_RATES`, `SUST_SECS`, `HUGEPAGE_POOL`.
Run the harness with `RUST_LOG=fcvm=debug` in the environment when the run is
meant to be analyzed for stage attribution (serve/restore logs land in
`results/<stamp>/logs/`).

## Results conventions

Raw run output goes to `results/<timestamp>/` — **git-ignored** (see
`.gitignore`): `hostinfo.json`, `availability.json`, `requests/*.log`
(timestamped harness lines, filenames encode `phase__mode__arm__url__rN`),
`samples/*.jsonl`, `logs/`, then `raw.json` + `report.md` from phase6.

A publishable run follows the editor-loop-bench style: commit a curated
`summary.md` (the writeup), `charts/*.svg` (rendered from `raw.json`), the
bench json itself, and a `REVIEW.md` recording the adversarial-review verdicts
against the numbers — what survived, what was refuted, and what a rerun must
change. Never publish numbers whose review verdicts were refuted; see
`REVIEW.md` in this directory for the current state.

## Current status

The **2026-08-08 corrected run** is the current record: `results/20260808-corrected/`
(`summary.md`, `charts/*.svg`, `corrected.json`). It fixes all six methodology defects that
sank the first run — matched per-clone cgroup accounting plus an independent whole-machine
`MemAvailable` basis, one seeded interleaved schedule with two control arms, the burst as the
experimental unit with bootstrap CIs, `RUST_LOG=fcvm=debug` stage attribution, slopes reported
with intercepts and req/GiB at concrete N, and uncertainty on every figure.

Headlines: artifact **730 ms** (95% CI 708-741) end to end against a host-native warm floor of
**218 ms** (202-229); `--uffd-mode minor` with hugepages at **34.7 +/- 0.4 MiB per concurrent
request**; routed's 1 s first-egress stall gone (3.0 ms on every mode). Two previously published
Chromium figures are **refuted** by this run: JPEG q80 is -8.3% per request (not -21%), and
site-isolation-off saves 3.6% on PSS (not 23% - that number was an RSS artifact).

`REVIEW.md` is the ledger of what holds, what was refuted, and what remains unmeasured. Read it
before quoting anything from this directory.

### Running it reproducibly

Pin the binary: this is a shared box, and a concurrent workload rebuilding
`target/release/fcvm` from its own uncommitted changes will silently swap the thing under test.

```bash
git worktree add /tmp/pristine --detach origin/main && (cd /tmp/pristine && make build)
FCVM=/tmp/pristine/target/release/fcvm RESULTS=bench/chromium/results/<stamp> \
  bench/chromium/bench.sh run
python3 bench/chromium/analyze.py bench/chromium/results/<stamp>
python3 bench/chromium/charts.py  bench/chromium/results/<stamp>
```

`hostinfo.json` records the binary sha256, git commit, image id, and the load average at start;
`samples/loadavg.jsonl` records load every 5 s so every phase can be reported with the
contention it actually ran under.
