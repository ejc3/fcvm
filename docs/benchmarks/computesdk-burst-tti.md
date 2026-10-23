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
24.11.1, fcvm at `6edb22bb` (this branch). No other workload ran on the host.

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
| 100 | 5 bursts x 100 | 500/500 | 2866 | 3272 | 3361 | 2032 | 2866 | 3261 | 3467 | 3733 | 4162 |
| 10 | 5 bursts x 10 | 50/50 | 719 | 776 | 792 | 596 | 716 | 773 | 792 | 836 | 836 |
| 1 | 50 in sequence | 50/50 | 669 | 734 | 745 | 580 | 666 | 732 | 745 | 761 | 761 |

The five bursts of 100, one row each:

| burst | harness median | harness p95 | harness p99 | min | max | wall clock (first create to last ready) |
|---|---|---|---|---|---|---|
| 1 | 2879 | 3262 | 3315 | 2032 | 3489 | 3489 |
| 2 | 2804 | 3135 | 3227 | 2043 | 3645 | 3645 |
| 3 | 2894 | 3360 | 3515 | 2224 | 3733 | 3734 |
| 4 | 2844 | 3308 | 3665 | 2063 | 4162 | 4162 |
| 5 | 2910 | 3275 | 3454 | 2091 | 4047 | 4047 |

Deciles of all 500 (ms): 2369, 2505, 2645, 2776, 2866, 2917, 3007, 3094, 3261, 4162.

An earlier set of five bursts of 100 on the same host, one commit before (`f1a68ecd`, which
differs only in how sandboxes are torn down), gave a harness median of 2765 ms, p95 3130 and
p99 3236.

### Where a burst of 100 spends its time

From one more burst of 100 with fcvm's info log on (harness median 2875 ms, p50 2873 ms),
per sandbox (101 with the warm-up), p50 and p95 in ms:

| phase | p50 | p95 |
|---|---|---|
| create request to Firecracker start (spawn, disk, network, listeners) | 285 | 1544 |
| Firecracker start to restore ACK (snapshot load, guest restore phases) | 641 | 808 |
| ACK to fc-agent's container-ready notification | 53 | 119 |
| container ready to `fcvm serve` returning the create | 1449 | 2371 |

Almost all of the last phase is fcvm's health monitor, not `fcvm serve`: serve reads the
state directory every 10 ms, and pairing the k-th "VM healthy" log line with the k-th
returned create puts serve's share at a median of 7 ms (the health monitor's line names no
VM, so that pairing is by rank, not by sandbox). Without an HTTP health check the monitor
decides that a container is running by spawning `fcvm exec ... podman inspect`, and under a
burst of 100 those execs are slow. At concurrency 1 the whole TTI is 666 ms. Tracked in
#1161.

### With the container start notification (#1161)

Since #1161 fc-agent runs podman with `--sdnotify=conmon` and tells the host when podman
reports the container running, and the health monitor takes that as the running signal
instead of spawning `fcvm exec ... podman inspect`. Both arms below were measured on one
c8gd.metal-24xl on 2026-10-23 with the commands above, alternating per configuration: before
at `12e0f0ca`, after at `e8ecb178`. Nothing else ran on the host. Each measurement started
only once the 1-minute load average was at most 1.0 (0.82 to 1.00 at the eight starts), and
`pidstat` found no process using 1% of a CPU in the 5 s before any of them. During the runs
it found only the benchmark's own processes (firecracker, fcvm, pasta, node). TTI in ms,
harness statistics, then untrimmed p99 and max.

| concurrency | sandboxes | before median | before p95 | before p99 | after median | after p95 | after p99 | before / after untrimmed p99 | before / after max |
|---|---|---|---|---|---|---|---|---|---|
| 100 | 5 bursts x 100 | 3017 | 3520 | 3591 | 2125 | 2356 | 2381 | 3875 / 2444 | 3929 / 2501 |
| 10 | 5 bursts x 10 | 716 | 770 | 779 | 579 | 636 | 637 | 795 / 638 | 795 / 638 |
| 1 | 50 in sequence | 683 | 765 | 777 | 509 | 558 | 576 | 805 / 587 | 805 / 587 |

One more burst of 100 creates each way, without the command, timed per sandbox on the host
clock: the request, the status listener's container-ready file, podman's `State.StartedAt`
in the guest (moved to the host clock by an offset measured afterwards over exec, round
trip 10 ms at the median), the restore's `ack_ms` from the clone's restored record, and the
create's return. p50 / p95 ms:

| interval | before | after |
|---|---|---|
| create request to return | 2826 / 3445 | 1738 / 1970 |
| restore start to guest ACK | 705 / 1088 | 609 / 762 |
| podman started the container to the create's return | 1032 / 1666 | 29 / 196 |

Before, a health check under that burst was one or two execs, each 569 ms at the median
(10 sandboxes probed right after their start: `fcvm exec --vm -- true` 81 ms, `podman
inspect` 415 ms of it inside the guest, starting the `fcvm` process 13 ms), and the next
check waited as long as the last one took. In the before run, where fc-agent still sent its
notification when it spawned `podman run`, podman took a median of 588 ms (p95 963) from
that spawn to the container running. This change does not touch that time, and it is now
the largest part of a create after the restore.

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

Of the 29 providers with at least one successful sandbox that day, 10 had a median under 700
ms, 18 under 1,100 ms and 25 under fcvm's 2,866 ms at concurrency 100. Cloudflare's own post
quotes 648, 910 and 1,129 ms for median, p95 and p99. Northflank's post quotes a P99
readiness of 733 ms from the Scale Invitational, a different test at 100,000 sandboxes.

## GPU VM restores

`make bench-gpu-restore N=104` (`bench/gpu-restore/run.sh`) runs, one after another,

```
fcvm podman run --name gpu-<i> --kernel-profile gpu --gpu <sbdf> ubuntu:24.04 nvidia-smi -L
```

rotating over the host's vfio-pci bound T4s. One untimed run first creates the gpu profile's
pre-start snapshot of `ubuntu:24.04`, so each timed run is a cache hit: it restores that
snapshot with the GPU attached through VFIO, the guest rescans the bus, loads the NVIDIA
modules and starts `nvidia-persistenced`, then the container runs `nvidia-smi -L` and exits
and the VM is torn down. Per run it records:

- **wall**: `fcvm podman run` process start to exit, all of the above.
- **ack_ms**: from the clone's restored record, restore start to the guest's ACK, which
  comes after the GPU attach.
- **gpu_ms**: the guest's GPU attach phase (rescan, modules, device nodes,
  `nvidia-persistenced`, `nvidia-smi -L`, CDI spec).

Host: one AWS g4dn.metal (x86_64, 96 vCPUs, 8 NVIDIA T4, 377 GiB), on-demand, used by this
run alone; Ubuntu 24.04, host kernel 7.0.0-1014-aws; fcvm at `12e0f0ca` (this branch, built on
the host with `make build` and `make setup-gpu`). `make bench-gpu-restore N=104`: 104 runs, 13
on each of the 8 GPUs, all exited 0, all restored with the `file` memory backend.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max |
|---|---|---|---|---|---|---|---|
| wall | 104 | 3256 | 3310 | 3343 | 3356 | 3375 | 3377 |
| ack_ms | 104 | 2316 | 2351 | 2374 | 2377 | 2391 | 2396 |
| gpu_ms | 104 | 1289 | 1339 | 1373 | 1383 | 1398 | 1404 |

The host was idle around the run. Captured just before it: 1-minute load 0.49 (0.84 after
the 5 s `pidstat` that followed, the start gate being at most 1.0), no process other than
`pidstat` itself above 1% CPU, no `fcvm` or Firecracker process, and all 8 T4s bound to
vfio-pci with no process holding any of their `/dev/vfio` groups. Captured just after: the
same, with a 1-minute load of 1.08 left by the run's own VMs (one at a time) and no
`/dev/vfio` group held.

DESIGN.md's measurements of 2026-10-22, before `nvidia-persistenced` was started during the
attach, had gpu_ms of 3,028 to 3,337 ms over five restores and a cache-hit run of 6.7 s.
