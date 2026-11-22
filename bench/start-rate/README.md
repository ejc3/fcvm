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
time between the clone's log lines (`VM resume completed`, fc-agent's console lines, the restore
acknowledgement, `exec server ready`) and fc-agent's own phase record from the acknowledgement.

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

- `vcpustat.py` reads `/proc/<vmm>/task/<vcpu>/schedstat` and `stat` of every Firecracker process
  in its first 12 s, every 25 ms (give 0.004 for a C=1 cell): the vCPU thread's on-CPU time, its
  runqueue wait, and its user, guest and system ticks (10 ms each). Samples carry the process's
  start time, so a reused pid is a second process.
- `s2fault.bt` records, per VMM in buckets from its first stage-2 fault (10 ms wide for the first
  second, 100 ms after), the vCPU thread's stage-2 faults on guest memory (`kvm:kvm_guest_fault`),
  the time from each to the vCPU's next entry into the guest (`kvm:kvm_entry`), its device aborts,
  and its own time in `handle_userfault`. It also counts every exec and fork on the host.

The joined table splits the window into `vcpu_run_ms` (on a CPU: guest code, and the host kernel
working for it, stage-2 faults included), `vcpu_wait_ms` (runnable, no CPU), `userfault_ms` (the
vCPU thread blocked until the memory server answered) and `rest_ms` (the vCPU thread asleep
otherwise: the guest idle). `stage2_fault_ms` is the measured fault-to-entry time inside the
window and `us_per_stage2_fault` its mean per fault. A bucket the window ends in counts by the
share of it the window covers. Every row gives `n` against the cell's clones, and the clones
left out are counted by reason (the sampler never saw the VMM; ready came after the sampler's
12 s). The tracers cost CPU themselves (two probes per stage-2 fault), so rates come from cells
run without them.
