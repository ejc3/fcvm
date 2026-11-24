# start-pieces results

One m8gd.metal-24xl (96 Graviton4 cores, 377 GiB), host kernel Ubuntu's 7.0.0-1018-aws (it has the
fix that keeps namespace cleanup from holding RTNL through one `rcu_barrier()` per bridge),
`/mnt/fcvm-btrfs` btrfs on the instance-store NVMe RAID0 (`/dev/md127`), one boot, 2026-11-24, main
at 9d7b6f94f. Snapshot: `bench-start-rate-prep` of nginx:alpine, 1 vCPU, 256 MiB, rootless. Each
cell is one `make bench-start-pieces` run through `matrix.sh` (default rungs `1:100 32:400 128:1000
512:1000`, alive 0 and 1000; `vmm` alive 0 only, with `SNAPSHOT_DIR` and `FIRECRACKER` set;
`noop` with `COLD=1`); raw `summary.json` files are not committed.

## The table

ops/s at each concurrency C with 0 / 1000 instances of the piece already held, from
`python3 bench/start-rate/pieces/table.py ROOT --cores 91`. CPU-s per op is at C=128 with 1000
held, less the held set's idle CPU. The ceiling is the lower of the best rate measured with 1000
held (with none held for vmm) and 91 cores divided by the CPU per op: this host's 96 cores less
5 for 1000 idle clones, measured ("What 1000 idle clones cost" below). Rows are ranked by the ceiling.

| piece | ceiling/s | ops/s C=1 a0/a1000 | ops/s C=32 a0/a1000 | ops/s C=128 a0/a1000 | ops/s C=512 a0/a1000 | CPU-s/op cgroup (C=128) | CPU-s/op host | p50/p99 ms C=1 a0 | p50/p99 ms C=128 a1000 | held instance, cores | ctxt/op | IPI/op | top 3 frames |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rootless-net | 176 | 74 / 60 | 501 / 176 | 298 / 134 | 272 / 128 | 0.0533 | 0.0524 | 13.0 / 16.0 | 924.6 / 1573.0 | 0.0001 | 397 | 298 |  |
| vmm | 747 | 127 / - | 922 / - | 764 / - | 239 / - | 0.1209 | 0.1219 | 8.0 / 9.2 | - / - | 0.0442 | 174 | 106 |  |
| disk | 1822 | 296 / 304 | 572 / 470 | 1098 / 1131 | 1255 / 1822 | 0.0075 | 0.0077 | 3.4 / 3.5 | 112.6 / 166.1 | 0.0000 | 86 | 40 |  |
| holder | 3283 | 367 / 360 | 5045 / 3283 | 4126 / 2836 | 3824 / 2789 | 0.0163 | 0.0164 | 2.7 / 3.1 | 34.8 / 91.1 | 0.0000 | 59 | 28 |  |
| state | 3525 | 1696 / 1745 | 3486 / 3525 | 3520 / 3518 | 3298 / 3293 | 0.0007 | 0.0006 | 0.6 / 0.7 | 35.9 / 62.0 | 0.0000 | 70 | 35 |  |
| fc-spawn | 6961 | 212 / 215 | 3412 / 3611 | 6977 / 6961 | 6891 / 6781 | 0.0101 | 0.0100 | 5.0 / 5.4 | 17.4 / 34.3 | 0.0000 | 46 | 98 |  |
| noop cold | 10985 | 761 / 827 | 4245 / 9193 | 8070 / 10034 | 7998 / 10985 | 0.0016 | 0.0016 | 1.0 / 12.0 | 9.7 / 37.4 | 0.0000 | 21 | 10 |  |
| kvm-vm | 42842 | 4378 / 4414 | 36800 / 42058 | 40523 / 42842 | 35938 / 37395 | 0.0017 | 0.0016 | 0.2 / 0.3 | 2.8 / 5.9 | 0.0000 | 15 | 8 |  |
| health | 280979 | 5611 / 5600 | 145550 / 147043 | 279604 / 264502 | 294292 / 280979 | 0.0003 | 0.0002 | 0.2 / 0.2 | 0.4 / 0.8 | 0.0000 | 21 | 7 |  |

## What 1000 idle clones cost

1000 clones of the same snapshot held for 150 s after the last was healthy, measured by the
run's own `pidstat` (2 s samples, every process's CPU, vCPU threads included in Firecracker's):
the host's processes used 2.8 to 4.4 cores with File clones and 4.6 to 5.6 cores with UFFD
clones, Firecracker 2.0 to 3.5 and 3.6 to 5.5 of that, fcvm's processes under 1. Host memory
available fell by 69 GiB for the 1000 UFFD clones, about 69 MiB each. One run per backend on a
c8gd.metal-24xl (96 Graviton4 cores, 188 GiB), Ubuntu's 7.0.0-1018-aws, btrfs on the NVMe RAID0,
2026-11-24, main at 53624b25b: the same core count and kernel as the table's host, a different
instance type.

## The namespace pool

One session on a c8gd.metal-24xl (96 Graviton4 cores, 188 GiB), Ubuntu's 7.0.0-1018-aws, btrfs on
the instance-store NVMe RAID0 (`/dev/md127`), 2026-11-24, main at 53624b25b; the same snapshot
recipe as above. The pooled network piece is `netpool.sh` (claims from a running `fcvm netpool
serve` of 2000 namespaces, with 1000 leases held), in the table's format:

| piece | ceiling/s | ops/s C=1 a0/a1000 | ops/s C=32 a0/a1000 | ops/s C=128 a0/a1000 | ops/s C=512 a0/a1000 | CPU-s/op cgroup (C=128) | CPU-s/op host | p50/p99 ms C=1 a0 | p50/p99 ms C=128 a1000 | held instance, cores | ctxt/op | IPI/op | top 3 frames |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| netpool | 35969 | - / 1937 | - / 35969 | - / 33176 | - / 32252 | 0.0003 | 0.0003 | - / - | 3.7 / 4.8 | 0.0000 | 30 | 15 |  |

A claim is not the pool's limit: every released namespace is reset before it is granted again,
and with 1000 leases held the pool reset 3,706 to 3,937 namespaces a second (mean 243 to 265 ms
each at C=32 to 512), so a pool sustains about 3,700 starts a second in steady state. The pool of
2000 took 21 s to build and 151 s to destroy.

The composed start with the pool warm (`fcvm netpool serve --slots 2200`, ready before the
first clone) against the same start without it, same session and box. 1000 clones of
nginx:alpine, 1 vCPU, 256 MiB, rootless, held 10 s; one snapshot per backend for each arm, the
C=128 cell first on it, then C=512 on the same snapshot. Every pooled clone logged that it ran in
a pooled namespace. Host CPU per start is `/proc/stat` busy time from the run's launch to the
last healthy clone plus 3 s, divided by 1000, so it over-counts (the window is longer than the
starts); "host CPU, peak" is the highest 2 s `pidstat` sample.

| backend | C | pool | rate/s | s2h p50 / p99 ms | resume to agent p50 / p99 ms | store reads per clone, MiB | most requests in flight | host CPU-s per start | host CPU, peak cores |
|---|---|---|---|---|---|---|---|---|---|
| File | 128 | no | 138.2 | 781 / 1943 | 30 / 1093 | 9.78 | 52 | 0.609 | 83 |
| File | 128 | yes | 207.1 | 478 / 1414 | 66 / 444 | 3.91 | 52 | 0.502 | 83 |
| File | 512 | no | 136.6 | 2321 / 3517 | 30 / 200 | 9.76 | 45 | 0.408 | 58 |
| File | 512 | yes | 212.6 | 1280 / 2770 | 42 / 141 | 3.91 | 49 | 0.342 | 46 |
| UFFD | 128 | no | 73.4 | 1653 / 2103 | 1426 / 1686 | 9.29 | 18 | 1.056 | 65 |
| UFFD | 128 | yes | 76.0 | 1610 / 2080 | 1352 / 1538 | 9.04 | 52 | 1.091 | 62 |
| UFFD | 512 | no | 126.6 | 3300 / 5368 | 31 / 150 | 9.22 | 23 | 0.546 | 56 |
| UFFD | 512 | yes | 165.9 | 1954 / 4696 | 68 / 240 | 9.00 | 50 | 0.608 | 60 |

The pool raises File starts from 137 to 213 a second and UFFD's from 127 to 166 at C=512. The UFFD
C=128 cells are the first on a fresh snapshot, so their clones record the working set and wait
on demand faults (resume to agent 1.4 s); the C=512 cells replay it. The pooled start's ceiling
on this host is what these cells reached, 213 a second (File) and 166 (UFFD) at C=512, and host
CPU does not bound it: the busiest 2 s sample of the pooled File C=512 cell used 46 of 96 cores.
At the over-counted CPU per start, the ceiling per core is at least 2.9 pooled File starts a
second and 1.6 pooled UFFD starts. What does bound it is not identified by these cells.

## The vmm piece, broken down

The `vmm` piece (spawn Firecracker in a prepared namespace, load the snapshot, resume) per stage,
p50 / p99 ms, with none held. File is the table's run above (m8gd.metal-24xl, main 9d7b6f94f);
UFFD is a second run in the pool session (c8gd.metal-24xl, main 53624b25b), its snapshot served by
`fcvm snapshot serve` with a recorded working set. Same kernel and store.

| backend | C | ops/s | Firecracker spawn | snapshot load | resume |
|---|---|---|---|---|---|
| File | 1 | 127 | 4.5 / 4.6 | 3.9 / 4.4 | 0.1 / 0.2 |
| File | 32 | 922 | 12.2 / 31.1 | 10.7 / 27.8 | 3.3 / 11.7 |
| File | 128 | 764 | 24.5 / 159.6 | 42.8 / 351.9 | 6.1 / 40.2 |
| File | 512 | 239 | 44.1 / 85.9 | 84.7 / 3925.0 | 6.5 / 35.8 |
| UFFD | 1 | 116 | 3.3 / 5.1 | 4.0 / 25.6 | 0.1 / 0.2 |
| UFFD | 32 | 632 | 4.9 / 12.1 | 43.1 / 99.0 | 0.2 / 2.3 |
| UFFD | 128 | 646 | 5.3 / 26.4 | 161.0 / 501.1 | 0.2 / 1.5 |
| UFFD | 512 | 875 | 25.1 / 71.6 | 360.9 / 1006.2 | 0.3 / 2.9 |

On UFFD the snapshot load includes the memory server's handshake, and replay of the recorded set
runs after it, beside the guest. From the server's log of the same run, p50 / p99:

| C | handshake, ms | replay, ms | pages replayed | demand faults during replay | demand faults in the VM's life |
|---|---|---|---|---|---|
| 1 | 0.1 / 0.1 | (no set yet) | 0 | - | 5986 / 6093 |
| 32 | 0.1 / 22.2 | 13 / 92 | 6356 / 6372 | 33 / 278 | 43 / 280 |
| 128 | 0.6 / 23.7 | 204 / 956 | 6714 / 6783 | 85 / 1005 | 85 / 1005 |
| 512 | 18.3 / 74.7 | 1311 / 1814 | 7101 / 7663 | 579 / 1597 | 579 / 1597 |

The handshake is the time from the server accepting the connection to receiving the
userfaultfd; replay populates about 25 MiB per clone, and at C=512 a replay takes 1.3 s because
1000 replays share the host. The C=1 clones ran before any set was recorded, so each faulted its
6000 pages on demand. The guest's first faults after resume, as stage-2 faults and userfaults
with their time, are in `../RESTORE.md` ("One clone at a time").

## Earlier entries

This file's earlier per-piece notes, the tap-attach study and the changelog's before and after
figures were measured on Ubuntu's 7.0.0-1013-aws, whose namespace cleanup holds RTNL through one
`rcu_barrier()` per dying bridge (fixed upstream in 6.18.60 and 7.2), so they are not kept. The
changes they measured are in main: #2000 (state: index names and PIDs), fc-agent's idle MMDS poll,
counting a snapshot's disk extents once, #2014 (reflink while one of 8 slots is free, else a
subvolume snapshot), #2025 (the holder builds the links over netlink), #2035 (the write set), and
the namespace pool. The table above measures main with all of them.
