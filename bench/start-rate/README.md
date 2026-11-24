# start-rate: snapshot clones reaching healthy per second

Measures how many `fcvm snapshot run` clones of one small snapshot reach healthy per second on one
host, and where each clone's time goes.

A clone counts when its own fcvm process logs `VM healthy, switching to ... polling`, which the health
monitor writes after it has stored `health_status=healthy` in the clone's state file. The snapshot
records an HTTP health check (`prep` boots the workload with `--health-check http://localhost/`), so
healthy means the clone's network is up, fc-agent is connected and the workload answered HTTP through
the clone's own network path. After the last clone, `run` reads `fcvm ls --json` and exits non-zero
unless every clone its log calls healthy is healthy in its state file too, and when any clone failed.
`fcvm ls` failing, or `pidstat` missing, blocks the run before it starts.

## Running

```bash
make bench-start-rate-prep OUT=/mnt/fcvm-btrfs/sr/prep NETWORK=rootless UFFD=1   # baseline, snapshot, serve
make bench-start-rate-run PREP=/mnt/fcvm-btrfs/sr/prep/prep.json OUT=/mnt/fcvm-btrfs/sr/run-c128 \
    COUNT=1000 CONCURRENCY=128
make bench-start-rate-teardown PREP=/mnt/fcvm-btrfs/sr/prep/prep.json
```

- `CONCURRENCY` is the most clones spawned and not yet healthy at once; a new clone starts as soon as
  one turns healthy or fails. Healthy clones stay up until the end, so `COUNT` clones are alive when
  the last one turns healthy.
- With `UFFD=1` clones restore through `snapshot serve` (`--pid`); without it through the File backend
  (`--snapshot`).
- `NETWORK=bridged` or `routed` runs fcvm under sudo; routed may need `IPV6_PREFIX=`.
- `CPU=` and `MEM=` size the baseline VM (default 1 vCPU, 256 MiB); `IMAGE=` defaults to nginx:alpine.
- `LAUNCHERS=` is the number of harness threads spawning clones (default 4); `popen_ms` in the summary
  shows whether spawning itself limits the rate.

## The store

Measure with `/mnt/fcvm-btrfs` on local NVMe: a pbox `kvm-96` with `--local-nvme`, btrfs on the
instance-store RAID0 (`/dev/md127`). A btrfs image on a loop device, even one whose file is in tmpfs,
caps the store at the loop device's request queue (128 requests). Each clone's `rootfs.raw` is an
inode of its own, so no clone finds another's reads of its disk in the page cache.

To compare stores, read the device's requests in flight (`/sys/block/<dev>/inflight`) and its read
await (`iostat -x 1`) during a run, and each clone's Firecracker `read_bytes` from
`/proc/<pid>/io`. Figures for both stores follow, measured on a host kernel with the RTNL fix
(upstream 6.18.60 and 7.2): the pboxes' stock 7.0 kernel holds RTNL in its namespace cleanup
worker, so start numbers measured on it are not published.

`host.json` records the store's device, its backing file and that file's filesystem, and the
`nr_requests` of the device, or of each member when it has members (an md RAID0).


## Churn: starts and stops at one rate

`run` measures a ramp: every clone stays up, so nothing is torn down while clones start and
no namespace is used twice. `churn` measures a rate a host can hold: each clone runs
`LIFETIME` seconds after it is healthy (uniform in `LIFETIME` plus or minus 20%, so the
clones a burst started together do not stop together for the whole run) and is then stopped, and a new one starts while fewer
than `ALIVE` are up or starting, at most `CONCURRENCY` starting at once. It runs rootless
clones in a namespace pool that is already running (`fcvm netpool serve`, in the same
`FCVM_DATA_DIR`), with `--publish none`, because a clone with a published port is not pooled.

```bash
fcvm netpool serve --slots 2000 &
make bench-start-rate-prep OUT=/mnt/fcvm-btrfs/sr/prep NETWORK=rootless UFFD=1
make bench-start-rate-churn PREP=/mnt/fcvm-btrfs/sr/prep/prep.json OUT=/mnt/fcvm-btrfs/sr/churn \
    ALIVE=1000 LIFETIME=1 CONCURRENCY=128 DURATION=120
```

`ALIVE` is what the run asks for and the rate is what comes out: the loop is closed, and a
host that cannot start clones fast enough holds fewer than `ALIVE` up. The summary prints the
clones up and starting beside `ALIVE`.

The run warms up until every start of the last 2 s (longer for a run that starts fewer than
ten clones a second) took a namespace another clone had before (`generation` 2 or more in the
clone's "running in a pooled namespace" line) and a lifetime has passed, measures a window of
`DURATION` seconds between two samples of the pool's counters, then stops everything.
`summary.json` has `ok`, one entry of `conditions` for each line below, the window's figures
under `observed`, and the same figures under `sustained` only when every condition holds
(`null` otherwise; the first line the run prints then starts with `NOT SUSTAINED`). It also
has `failures` (in the window, still starting at its end, in warm-up), the pool samples'
coverage, what was left after the drain, and the harness's own cost (`driver`: its CPU over
the window and the spawn call's time; near one core, the harness is the limit, not the host).
The run exits 1 unless every condition holds (`churn_verdict`, a test per condition in
`test_startrate.py`). `report DIR` judges a churn directory again from its records.

| condition | what it refuses |
|---|---|
| window | a window under 60 s or under 10 lifetimes (`churn` refuses to start with such a `DURATION`) |
| starts equal stops | starts and stops in the window more than 2% apart: a ramp, or a backlog of stops |
| no failed clone | a clone that failed or exited before its stop in the window, and a clone still starting at the window's end after ten times the window's p99 (or `--timeout`, which must be under `DURATION`), whatever became of it after the end |
| alive count flat | an alive count whose first and last tenth of the window differ by over 2% of `ALIVE` (one clone for a small run), or that swings by over 20% of it (5 clones for a small run), or whose samples do not cover the window (one within 2 s of each edge, none over 3 s apart) |
| alive count accounted for | clones up plus starting more than 5% from `ALIVE`, or an alive count more than 10% from the start rate times the time a clone stayed up |
| every start in a recycled namespace | a start in a namespace no clone had before, or in one the clone built for itself |
| pool counters cover the window | no sample of the pool's counters answered within 2 s before the window or asked within 2 s after it, or two samples over 3 s apart (a poll that fails or takes over 2 s is counted and leaves a gap) |
| free slots flat | a pool whose ready namespaces run down over the window (2% of its slots) |
| no namespace built or destroyed | a build or a destroy between the samples around the window |
| slots cover the resets | a pool with fewer slots than the alive clones, plus the rate times the window's reset p99, plus `CONCURRENCY` |
| warm-up ended | starts that were not all recycled within `--warmup-limit` (600 s) |
| window completed | a run that stopped before the window's end |
| state files agree | a running clone the logs call healthy that `fcvm ls` does not |
| nothing left after the drain | a namespace still lent or being reset, or a clone of the run still in `fcvm ls`, after every clone was stopped |

A start counts as in `run`: the clone's own log says its health monitor saw the VM healthy.
A stop counts when the clone's fcvm process has exited. Records: `samples.jsonl` (per clone:
spawn, healthy, stop, exit, generation), `alive.tsv` (every 0.5 s), `pool.jsonl` (the pool's
counters every second), `host.tsv`.

## Records (OUT)

| file | what |
|---|---|
| `host.json` | instance type, kernel, CPUs, memory, the store's filesystem, device, backing file and its filesystem, and queue depth, source revision, run config |
| `samples.jsonl` | one row per clone: spawn time, time to healthy, the receive time of each stage marker, error |
| `clones/<name>.log` | the clone's own output, each line prefixed with ms since its spawn |
| `host.tsv` | once a second: load average, running and blocked tasks, MemAvailable |
| `pidstat.log` | `pidstat -u -h` per process every 2 s |
| `summary.json` | rate (overall, best 1 s window, best 5 s window, per second), latency percentiles, stage p50/p99, verification |

`python3 bench/start-rate/startrate.py report DIR...` prints summaries again. Stage times are the
harness's receive times of the marker lines, so they include pipe and scheduling delay on a loaded host.

## Restore timeline: resume to fc-agent ready

`restore_timeline.py` reads one `run` directory and prints, per step of the guest's restore, the
time between the clone's log lines (`VM resume completed`, fc-agent's lines, the restore
acknowledgement, `exec server ready`) and fc-agent's own phase record from the acknowledgement.
fc-agent sends a restore's progress lines on the output connection after the acknowledgement, each
with `logged=`, the guest's clock when fc-agent's console writer thread took the line off its pipe,
and the steps use that time. Lines the thread took in one read share a time, so a step between two
of them can read 0. A held line that went to the serial console carries how long it was held
(`held_ms=`) and is placed the same way.

```bash
python3 bench/start-rate/restore_timeline.py /mnt/fcvm-btrfs/sr/run-c128
```

Two host-side records, taken while `run` runs, split each clone's resume to ready window by where
its vCPU thread was:

```bash
python3 bench/start-rate/vcpustat.py $OUT.trace/vcpustat.jsonl 0.025 12 &     # SIGTERM to stop
sudo BPFTRACE_MAX_MAP_KEYS=300000 bpftrace bench/start-rate/s2fault.bt > $OUT.trace/s2fault.txt &
# wait for "attached" in s2fault.txt, run the cell, then SIGINT bpftrace
python3 bench/start-rate/restore_timeline.py $OUT --trace $OUT.trace
```

- `s2fault.bt` records, per VMM pid, the time of its vCPU thread's first stage-2 fault
  (`kvm:kvm_guest_fault`) and the thread's on-CPU, runqueue-wait and guest time at that moment.
  In buckets from that fault (10 ms wide for the first second, 100 ms after) it records the
  stage-2 faults on guest memory, the time from each to the vCPU's next entry into the guest
  (`kvm:kvm_entry`), the device aborts, and the thread's time in `handle_userfault`. It also
  counts every exec and fork on the host. Its maps are keyed by pid, so `restore_timeline.py`
  leaves out clones of one run that had the same VMM pid.
- `vcpustat.py` reads `/proc/<vmm>/task/<vcpu>/schedstat` and `stat` of every Firecracker process
  in its first 12 s, every 25 ms (give 0.004 for a C=1 cell; a pass over many VMMs takes longer
  than the period): the vCPU thread's on-CPU time, its runqueue wait and its guest time (whole
  10 ms ticks). Each sample is stamped at its own read and carries the process's start time, so
  a reused pid is a second process.
  Once a second it writes a clock anchor, the wall clock against `CLOCK_MONOTONIC`, which is the
  clock of `s2fault.bt`'s times; without anchors `restore_timeline.py --trace` refuses.

The window runs between two wall-clock stamps of the clone's fcvm process, and the buckets count
from the vCPU's first stage-2 fault, so each clone's buckets are shifted by the time between its
resume stamp and that fault (`first_fault_after_resume_ms`, a row of its own; negative when the
vCPU faulted before fcvm stamped the resume). A bucket the window starts or ends in counts by the
share of it the window covers.

The joined table splits the window into `vcpu_run_ms` (on a CPU: guest code, and the host kernel
working for it, stage-2 faults included), `vcpu_wait_ms` (runnable, no CPU), `userfault_ms` (the
vCPU thread blocked until the memory server answered) and `rest_ms` (the time before the first
fault, and the vCPU thread asleep otherwise: the guest idle). `stage2_fault_ms` is the measured
fault-to-entry time inside the window and `us_per_stage2_fault` its mean per fault.

- On-CPU and waiting time run from the later of the resume stamp and the first fault, where
  `s2fault.bt` read the thread's counters, to the ready stamp, read off the line between the two
  records around it. The kernel adds a running thread's time to those counters at its ticks
  (1 ms apart at `CONFIG_HZ=1000`), so each end is up to a tick behind.
- `guest_ms` is the kernel's tick accounting of guest time, which `/proc` gives in whole 10 ms.
  Against the exact value at the first fault it reads about 5 ms low per clone, so read its mean
  over a cell, and not at all for a window of a few ticks. `outside_guest_ms` is `vcpu_run_ms`
  less `guest_ms`: the host kernel and the VMM's own code on the vCPU thread.
- `userfault_ms` is the thread's time inside `handle_userfault`, which runs to the thread's
  return on a CPU after the memory server's wake. `vcpu_wait_ms` counts that wait for a CPU too,
  so `rest_ms` goes negative by as much.
- The rows from `s2fault.txt` need no sample, so every clone the tracer saw is in them. A clone
  is left out of the on-CPU rows when the sampler never saw its VMM or stopped sampling it before
  ready, and the report counts each reason. It also counts the clones with no sample inside the
  window, whose on-CPU time is read off the line from the first fault to a sample after ready.

The tracers cost CPU themselves (two probes per stage-2 fault), so rates come from cells run
without them.

## Device accesses per clone, by device

`mmio.bt` counts each restored guest's device (MMIO) aborts in the second after its vCPU's first
stage-2 fault, by the page of the guest address, and how many of them KVM returned to the VMM for:

```bash
sudo BPFTRACE_MAX_MAP_KEYS=100000 bpftrace bench/start-rate/mmio.bt > $OUT.trace/mmio.txt &
# wait for "attached" in mmio.txt, run the cell, then SIGINT bpftrace
```

A page is one device. In a guest of the same snapshot, `/proc/iomem` (as root) names the serial
console and each `virtio_mmio` device by address, and `/sys/bus/virtio/devices/virtioN/device`
gives each one's kind (1 net, 2 block, 5 balloon, 19 vsock). A byte written to the serial console
is one abort that returns to the VMM, plus the driver's status reads around each burst.

## Fault path: time per kernel function

`perf` and bpftrace `profile` samples cannot show what a vCPU thread does inside a stage-2 fault
on arm64: KVM reports every sample taken while a vCPU is loaded as guest state, and the kernel
stack comes back empty. `faultpath.py` times the fault path's functions from entry to return
instead, and the lock waits of the vCPU threads by stack:

```bash
sudo python3 bench/start-rate/faultpath.py program > $OUT.trace/fault.bt   # probes this kernel allows
sudo BPFTRACE_MAX_MAP_KEYS=100000 bpftrace $OUT.trace/fault.bt > $OUT.trace/fault.txt &
# wait for "attached" in fault.txt, run the cell, then SIGINT bpftrace
python3 bench/start-rate/faultpath.py report $OUT.trace/fault.txt
```

- Function times are inclusive (a function's time holds its callees'), on the vCPU threads of the
  VMMs whose pid is a multiple of 8 (`--one-in`), from attach to SIGINT: everything those vCPUs
  did in that time, not one clone's resume to ready window.
- `us/fault` divides a function's total time by the same threads' stage-2 faults on guest memory.
- A row `X inside Y` counts only the calls of X made while Y was running on the thread
  (`INSIDE` in the script), so it reads as a part of Y's row. The plain row of X is all its calls.
- Lock waits (`lock:contention_begin` to `contention_end`) are of every vCPU thread, summed per
  stack of seven frames.
