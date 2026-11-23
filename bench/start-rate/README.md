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

## Records (OUT)

| file | what |
|---|---|
| `host.json` | instance type, kernel, CPUs, memory, the store's filesystem and backing, source revision, run config |
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
with the time the guest logged it (`logged=`), and the steps use that time. A held line that went
to the serial console carries how long it was held (`held_ms=`) and is placed the same way.

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
