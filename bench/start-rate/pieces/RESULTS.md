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
`python3 bench/start-rate/pieces/table.py ROOT --cores 66`. CPU-s per op is at C=128 with 1000
held, less the held set's idle CPU. The ceiling is the lower of the best rate measured with 1000
held (with none held for vmm) and 66 cores divided by the CPU per op; 66 is this host's 96 cores
less 30 set aside for 1000 idle clones, an assumption, not a measurement. Rows are ranked by the
ceiling.

| piece | ceiling/s | ops/s C=1 a0/a1000 | ops/s C=32 a0/a1000 | ops/s C=128 a0/a1000 | ops/s C=512 a0/a1000 | CPU-s/op cgroup (C=128) | CPU-s/op host | p50/p99 ms C=1 a0 | p50/p99 ms C=128 a1000 | held instance, cores | ctxt/op | IPI/op | top 3 frames |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rootless-net | 176 | 74 / 60 | 501 / 176 | 298 / 134 | 272 / 128 | 0.0533 | 0.0524 | 13.0 / 16.0 | 924.6 / 1573.0 | 0.0001 | 397 | 298 |  |
| vmm | 542 | 127 / - | 922 / - | 764 / - | 239 / - | 0.1209 | 0.1219 | 8.0 / 9.2 | - / - | 0.0442 | 174 | 106 |  |
| disk | 1822 | 296 / 304 | 572 / 470 | 1098 / 1131 | 1255 / 1822 | 0.0075 | 0.0077 | 3.4 / 3.5 | 112.6 / 166.1 | 0.0000 | 86 | 40 |  |
| holder | 3283 | 367 / 360 | 5045 / 3283 | 4126 / 2836 | 3824 / 2789 | 0.0163 | 0.0164 | 2.7 / 3.1 | 34.8 / 91.1 | 0.0000 | 59 | 28 |  |
| state | 3525 | 1696 / 1745 | 3486 / 3525 | 3520 / 3518 | 3298 / 3293 | 0.0007 | 0.0006 | 0.6 / 0.7 | 35.9 / 62.0 | 0.0000 | 70 | 35 |  |
| fc-spawn | 6541 | 212 / 215 | 3412 / 3611 | 6977 / 6961 | 6891 / 6781 | 0.0101 | 0.0100 | 5.0 / 5.4 | 17.4 / 34.3 | 0.0000 | 46 | 98 |  |
| noop cold | 10985 | 761 / 827 | 4245 / 9193 | 8070 / 10034 | 7998 / 10985 | 0.0016 | 0.0016 | 1.0 / 12.0 | 9.7 / 37.4 | 0.0000 | 21 | 10 |  |
| kvm-vm | 39786 | 4378 / 4414 | 36800 / 42058 | 40523 / 42842 | 35938 / 37395 | 0.0017 | 0.0016 | 0.2 / 0.3 | 2.8 / 5.9 | 0.0000 | 15 | 8 |  |
| health | 230411 | 5611 / 5600 | 145550 / 147043 | 279604 / 264502 | 294292 / 280979 | 0.0003 | 0.0002 | 0.2 / 0.2 | 0.4 / 0.8 | 0.0000 | 21 | 7 |  |

## Earlier entries

This file's earlier per-piece notes, the tap-attach study and the changelog's before and after
figures were measured on Ubuntu's 7.0.0-1013-aws, whose namespace cleanup holds RTNL through one
`rcu_barrier()` per dying bridge (fixed upstream in 6.18.60 and 7.2), so they are not kept. The
changes they measured are in main: #2000 (state: index names and PIDs), fc-agent's idle MMDS poll,
counting a snapshot's disk extents once, #2014 (reflink while one of 8 slots is free, else a
subvolume snapshot), #2025 (the holder builds the links over netlink), #2035 (the write set), and
the namespace pool. The table above measures main with all of them.
