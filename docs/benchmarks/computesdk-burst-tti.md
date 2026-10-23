# ComputeSDK Burst TTI on fcvm

fcvm measured under ComputeSDK's Burst TTI benchmark, the test Cloudflare cited for its
sandbox start times, plus fcvm's own GPU VM restore distribution. This page gives the
method, the commands and the results. Both runs were on 2026-10-23.

## The benchmark

[ComputeSDK Benchmarks](https://github.com/computesdk/benchmarks) publishes daily sandbox
results for about 30 providers. Its sandbox test, Burst TTI
(`benchmarks/sandbox/tti.bench.ts`, described in its `METHODOLOGY.md`), launches sandboxes
at once and times each one, from the client, from the call that creates it until a first
command (`node -v`) has returned with exit status 0:

```ts
const start = performance.now();
const sandbox = await compute.sandbox.create(options);
await sandbox.runCommand('node -v');
const ttiMs = performance.now() - start;   // destroy is not timed
```

The published daily run uses 100 iterations at concurrency 100, which is one burst of 100,
from a GitHub Actions runner on Namespace's infrastructure, over the internet to each
provider's API. It reports median, p95 and p99 after dropping the fastest and slowest 5
percent of samples (`computeStats` in `benchmarks/src/util/stats.ts`). With 100 samples the
trim leaves 90, so the published p99 is the 95th fastest sample and the published p95 the
91st.

Cloudflare's post of 2026-09-30 ([source](https://blog.cloudflare.com/faster-agent-sandboxes/))
quotes this test. Northflank's post of 2026-05-04
([source](https://northflank.com/blog/gpu-sandboxes)) quotes ComputeSDK's Scale
Invitational, which starts 100,000 concurrent sandboxes. That needs a fleet and is not run
here. The benchmark has no GPU variant.

## What was run

`make bench-computesdk` (`bench/computesdk/run.sh`):

1. Checks out computesdk/benchmarks at commit `9135206e` (2026-10-22), installs it as its CI
   does, and adds one participant, `fcvm`. Its provider (`bench/computesdk/fcvm-provider.ts`)
   is built with `defineProvider` from `@computesdk/provider` 2.1.14, the package and version
   the published providers use. The task, the schedule, the timer and the statistics are the
   harness's, unchanged.
2. Starts `fcvm serve` on the same host. Each sandbox is an `fcvm podman run` of
   `node:22-slim` with 2 vCPUs, 2048 MiB and rootless networking, which restores the image's
   pre-start snapshot (memory from the snapshot file, disk as a btrfs snapshot). A create
   (`POST /v1/sandboxes`) returns once fcvm's health monitor reports the container running.
   `runCommand` (`POST /s/{id}/run/command`) runs `sh -c 'node -v'` in the container over
   vsock.
3. Creates and destroys one sandbox, untimed. That creates the pre-start snapshot, as a
   provider's template exists before its published runs.
4. Runs `bench run benchmarks/sandbox/tti.bench.ts --provider fcvm --iterations N --concurrency C --no-ingest`
   once per burst.

```
make bench-computesdk BURSTS=5 CONCURRENCY=100
make bench-computesdk BURSTS=5 CONCURRENCY=10
make bench-computesdk BURSTS=1 CONCURRENCY=1 ITERATIONS=50
```

Host: one AWS c8gd.metal-24xl (96 vCPUs, Graviton4, 188 GiB), Ubuntu 24.04, host kernel
7.0.0-1013-aws, btrfs RAID 0 over the instance-store NVMe at `/mnt/fcvm-btrfs`, Node.js
24.11.1. fcvm at `f1a68ecd`. No other workload ran on the host.

### Differences from the published runs

- The client runs on the same host as `fcvm serve`, so a TTI here contains no network round
  trip. A published TTI contains at least two round trips over the internet (create, then
  the command), and more for providers whose create polls for readiness.
- Every sandbox in a burst runs on that one host. A provider spreads its burst over a fleet.
- The hardware differs, and the providers do not publish theirs.

## Results

TTI in milliseconds. "harness" columns are the harness's own statistics (5 percent trimmed
from each end), comparable to the published leaderboard. The other columns are the untrimmed
distribution, nearest rank.

| concurrency | sandboxes | ok | harness median | harness p95 | harness p99 | min | p50 | p90 | p95 | p99 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 100 | 5 bursts x 100 | 500/500 | 2765 | 3130 | 3236 | 1958 | 2764 | 3125 | 3270 | 3469 | 3655 |
| 10 | 5 bursts x 10 | 50/50 | 734 | 789 | 794 | 624 | 734 | 786 | 794 | 828 | 828 |
| 1 | 50 in sequence | 50/50 | 655 | 732 | 734 | 548 | 654 | 732 | 734 | 765 | 765 |

The five bursts of 100, one row each:

| burst | harness median | harness p95 | harness p99 | min | max | wall clock (first create to last ready) |
|---|---|---|---|---|---|---|
| 1 | 2669 | 3103 | 3177 | 2041 | 3502 | 3503 |
| 2 | 2827 | 3243 | 3350 | 2109 | 3655 | 3655 |
| 3 | 2814 | 3226 | 3385 | 2116 | 3547 | 3548 |
| 4 | 2783 | 3126 | 3184 | 2081 | 3401 | 3402 |
| 5 | 2615 | 3091 | 3157 | 1958 | 3457 | 3457 |

Deciles of all 500 (ms): 2348, 2459, 2550, 2639, 2764, 2834, 2911, 3008, 3125, 3655.

### Where a burst of 100 spends its time

From one more burst of 100 with fcvm's info log on (harness median 2820 ms, p50 2816 ms),
per sandbox, p50 and p95 in ms:

| phase | p50 | p95 |
|---|---|---|
| create request to Firecracker start (spawn, disk, network, listeners) | 333 | 1570 |
| Firecracker start to restore ACK (snapshot load, guest restore phases) | 622 | 833 |
| ACK to fc-agent's container-ready notification | 52 | 161 |
| container ready to the health monitor publishing Healthy | 1461 | 2147 |
| Healthy to `fcvm serve` returning the create | 7 | 19 |

The largest phase is detection, not restore: without an HTTP health check, fcvm's health
monitor decides that a container is running by spawning `fcvm exec ... podman inspect`, and
under a burst of 100 those execs are slow. At concurrency 1 the whole TTI is 655 ms. Tracked
in #ISSUE.

### Published results, for reference

As published, from different hardware, a different client location and different sandbox
sizes; none of these is a like-for-like comparison with the numbers above. ComputeSDK's
`results/burst_tti/latest.json` at commit `9135206e` (run of 2026-10-20, burst of 100,
harness statistics, ms), a selection:

| provider | median | p95 | p99 | success |
|---|---|---|---|---|
| Vercel | 417 | 539 | 589 | 100% |
| Blaxel | 481 | 511 | 515 | 100% |
| Cloudflare | 658 | 922 | 946 | 100% |
| Daytona | 672 | 943 | 955 | 100% |
| Modal | 1070 | 1207 | 1562 | 100% |
| E2B | 1334 | 1539 | 1571 | 100% |

Of the 29 providers with at least one successful sandbox that day, 10 had a median under
700 ms, 18 under 1,100 ms and 25 under fcvm's 2,765 ms at concurrency 100. Cloudflare's own post quotes 648, 910 and 1,129 ms for median, p95 and
p99. Northflank's post quotes a P99 readiness of 733 ms from the Scale Invitational, a
different test at 100,000 sandboxes.

## GPU VM restores

GPU PENDING
