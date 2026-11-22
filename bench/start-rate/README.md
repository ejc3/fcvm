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
sudo BPFTRACE_MAX_MAP_KEYS=150000 bpftrace bench/start-rate/s2fault.bt > $OUT.trace/s2fault.txt &
# wait for "attached" in s2fault.txt, run the cell, then SIGINT bpftrace
python3 bench/start-rate/restore_timeline.py $OUT --trace $OUT.trace
```

- `vcpustat.py` reads `/proc/<vmm>/task/<vcpu>/schedstat` and `stat` of every Firecracker process
  in its first 12 s, every 25 ms (4 ms for a C=1 cell): the vCPU thread's on-CPU time, its
  runqueue wait, and its guest and system ticks.
- `s2fault.bt` counts, per VMM in 100 ms buckets from its first stage-2 fault, the vCPU's stage-2
  faults (`kvm:kvm_guest_fault`) and the time its threads spent in `handle_userfault`. It also
  counts every exec and fork on the host.

The joined table splits the window into `vcpu_run_ms` (on a CPU: guest code, and the host kernel
working for it, stage-2 faults included), `vcpu_wait_ms` (runnable, no CPU), `userfault_ms`
(blocked until the memory server answered) and `rest_ms` (the vCPU thread asleep otherwise: the
guest idle). The tracers cost CPU themselves (one bpftrace probe per stage-2 fault), so rates come
from cells run without them.
