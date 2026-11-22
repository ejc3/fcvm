# Restore: where the time goes between resume and fc-agent ready

One session on one c8g.metal-48xl (192 cores, kernel 7.0.0-1013-aws), fcvm at 4f67a0fbd, store on
btrfs in RAM. Guest: nginx:alpine, 1 vCPU, 256 MiB, rootless network, snapshot taken when the
baseline first reported healthy. Every cell is `make bench-start-rate-run`; the traced cells also
ran `vcpustat.py` and `s2fault.bt` (README, "Restore timeline"). Times are p50 over the cell's
clones unless a row says otherwise. UFFD cells ran after the memory server had recorded a working
set (the first clones of a new snapshot take 173 ms from resume to ready, all demand faults).

## One clone at a time (C=1, N=30)

Steps, from the harness's receive time of the clone's log lines (ms):

| step | File | UFFD |
|---|---|---|
| VM resume completed to `detected restore-epoch` | 13.9 | 5.0 |
| to `handling restore` | 0.9 | 0.9 |
| to tcp-cleanup begin (attribute drop, exec rebind request, neighbour flush and pin, host identity, ARP) | 11.4 | 6.1 |
| tcp-cleanup (gate verify, UDP rule, receive barrier, egress open, 25 socket destroys) | 3.2 | 3.0 |
| to the clock step | 5.8 | 6.2 |
| to `snapshot network published` (exec and egress waits, thaw, ingress open) | 1.2 | 1.2 |
| to `restore phases complete` | 0.5 | 0.5 |
| to the host's acknowledgement | 3.3 | 1.7 |
| to `exec server ready` | 0.1 | 0.1 |
| **resume to ready** | **40.4** | **24.2** |
| ready to healthy | 6.0 | 5.9 |
| ready to `chronyd restarted` | 45.7 | 35.7 |
| ready to `journald restarted` | 71.6 | 59.7 |

The same window from the host (per clone):

| | File | UFFD |
|---|---|---|
| vCPU thread on a CPU (ms; the sampler adds up to 4 ms at each end) | 44.5 | 27.6 |
| vCPU thread runnable, waiting for a CPU (ms) | 0.0 | 0.0 |
| blocked in a userfault (ms) | 0.0 | 0.7 |
| stage-2 faults on guest memory, first 100 ms | 18,488 | 14,501 |
| device aborts (console, virtio), first 100 ms | 4,434 | 4,767 |
| userfaults, first 100 ms | 0 | 30 |
| scheduler ticks of the window: guest / host kernel / VMM user (mean ms) | 5.7 / 28.7 / 0.3 | 5.7 / 11.7 / 0.3 |

The vCPU is on a CPU for the whole window and the guest is never idle in it. About 6 ms is guest
code. The rest is the host kernel mapping the 14 to 18 thousand guest pages the restore touches:
a clone starts with an empty stage-2 table, so every page the guest reads or writes for the first
time is one fault, on both backends, whether or not the page is already in host memory.

The gate is not a process spawn: all 30 restores of each cell verified the gate, added the UDP
rule and opened both halves over nfnetlink (`tcp_gate_netlink`): 0.2 ms to verify, 0.2 ms for
the UDP rule, 3.1 ms for the egress commit and 0.1 ms to publish on File (0.2, 0.1, 0.9 and 0.1
on UFFD). A restore forks about 12 times in the guest, counted from
`/proc/stat` before a snapshot and after a clone's restore: chronyd and its daemon fork, the
`chronyc` polls that wait for it, `systemctl restart systemd-journald`, and journald.

## After ready

Per clone, the 2 s that follow the first healthy check:

| | File | UFFD |
|---|---|---|
| vCPU thread on a CPU (ms) | 210 | 205 |
| VMM main thread on a CPU (ms) | 26 | 25 |

That is five times the restore itself. The guest takes about 1,000 stage-2 faults and device
aborts every 100 ms for one second after the restore, then drops to 60: fc-agent's restore-epoch
watcher reads the vsock transport reset after it has already dispatched the restore (29 or 30 of 30
clones in every cell), and the reset opens a one-second window of metadata fetches 2 ms apart.
The chronyd start and the journald restart run in the same two seconds.

## Ramp: 128 starting at once, to 1000 alive

| | File | UFFD |
|---|---|---|
| clones healthy per second, whole ramp (untraced cell) | 32.2 | 19.3 |
| resume to ready, p50 / p90 / p99 (ms, untraced cell) | 2,862 / 3,442 / 3,952 | 326 / 1,175 / 5,741 |
| CPU-seconds per start, all processes / Firecracker (pidstat) | 4.78 / 4.62 | 7.29 / 6.81 |
| traced cell: resume to ready (ms) | 2,575 | 288 |
| vCPU thread on a CPU (ms) | 2,441 | 267 |
| scheduler ticks of the window: guest / host kernel (ms) | 130 / 2,290 | 40 / 210 |
| vCPU thread runnable, waiting for a CPU, p50 / p90 (ms) | 105 / 397 | 39 / 265 |
| blocked in a userfault, p50 / p90 (ms) | 0 / 0 | 0 / 38 |
| stage-2 faults on guest memory in the window, p50 / p90 | 67,627 / 124,989 | 15,025 / 27,551 |
| vCPU thread on a CPU in the 2 s after healthy (ms) | 1,036 | 1,760 |

On the File backend the window is 2.5 s of host kernel time in the vCPU thread: 68 thousand
stage-2 faults at about 34 µs each, where one clone alone takes 18 thousand at 1.5 µs. The clones
fault the same pages of one memory file in the same order. Waiting for a CPU is 4% of the window
and guest code 5%. On UFFD the faults cost about 10 µs each (#2004 measures NUMA balancing in that
path). On both backends the clone then spends one to two CPU-seconds in the 2 s after it is
healthy, in the fetch window and the two daemon restarts, on a host whose 192 cores are all busy.

## A snapshot can carry a compaction pass

The cells above the ramp are from a snapshot whose clones take 40 ms (File). A second snapshot
made by the same binary takes 67 ms: its clones take 26,013 stage-2 faults in the first 100 ms
where the other's take 18,488, and the extra ones have the guest's `copy_page`,
`isolate_migratepages_block` and `try_to_migrate_one` as their program counters. The guest's
kcompactd was in a proactive compaction pass when the snapshot was taken, and every clone runs
the pass on. Of 8 snapshots taken one after another, 4 held one (52 to 64 ms) and 4 did not (34
to 39 ms). The ramp cells above are from a snapshot that held one; the same ramp on one that did
not: File 35.2 clones/s and 1,414 ms from resume to ready, UFFD 29.6 clones/s and 63 ms.
