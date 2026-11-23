# start-pieces results

One c8g.metal-48xl (192 Graviton4 cores, 377 GiB), host kernel 7.0.0-1013-aws,
`/mnt/fcvm-btrfs` a btrfs image on tmpfs, one boot, 2026-11-22. Snapshot: `bench-start-rate-prep`
of nginx:alpine, 1 vCPU, 256 MiB, rootless; its `disk/disk.raw` is a 12.8 GB sparse file of 854
extents, so clones reflink it (`REFLINK_MAX_EXTENTS` is 4096). Each cell is one
`make bench-start-pieces` run (`matrix.sh`); raw `summary.json` files are not committed.

The budget: 1000 clones/s on 192 cores, less about 60 cores for 1000 idle clones, leaves
about 0.13 CPU-s per clone start for all pieces together.

## The table

ops/s at each concurrency C with 0 / 1000 instances of the piece already held. CPU-s per op is
host-wide (`/proc/stat`, kernel threads included) less the held set's idle CPU, at C=128 with
1000 held. The ceiling is the lower of the best rate measured with 1000 held (with none held
for vmm, which has no such cells) and 132 cores divided by the CPU per op; rows are ranked by
it (`table.py`). Revision: main at f3f155fa for the rows marked 1, main at
6faf3305 (#2000 merged) for the rows marked 3.

| piece | ceiling/s | C=1 | C=32 | C=128 | C=512 | CPU-s/op | p50 ms, C=1 / C=128+1000 | top frames (self time, C=128) |
|---|---|---|---|---|---|---|---|---|
| state, before #2000 (1) | 15 | 162 / 9 | 87 / 15 | 36 / 12 | 36 / 12 | 0.157 | 6.2 / 9905 | user code reading every state file |
| vmm, File backend (1) | 123 | 119 / - | 730 / - | 166 / - | 239 / - | 1.07 (a0) | 8.2 / - | guest time dominates (see below) |
| rootless-net (3) | 134 | 43 / 36 | 340 / 134 | 205 / 70 | 187 / 76 | 0.177 | 23 / 1754 | `ip` mutex_spin_on_owner 11-16% (rtnl_mutex), osq_lock; pasta chroot_fs_refs, rwsem_spin_on_owner; `ip` do_one_broadcast |
| disk (3) | 156 | 148 / 123 | 156 / 156 | 146 / 146 | 98 / 87 | 0.069 | 6.5 / 855 | btrfs_fiemap 39% of all samples: wait_extent_bit, btrfs_is_data_extent_shared, find_parent_nodes |
| holder (1) | 2590 | 109 / 109 | 3024 / 2590 | 3062 / 2295 | 2841 / 2013 | 0.018 | 9.1 / 44 | `unshare` mutex_spin_on_owner, osq_lock; newuidmap and newgidmap execs |
| state, after #2000 (3) | 3409 | 1616 / 1634 | 3472 / 3409 | 3404 / 3379 | 3195 / 3194 | 0.0009 | 0.6 / 37 | |
| health probe (1) | 5095 | 231 / 231 | 3823 / 4251 | 5077 / 5095 | 4700 / 4834 | 0.018 | 4.3 / 23 | `curl` ptep_clear_flush 50%, change_protection_range 11% |
| fc-spawn (1) | 7725 | 269 / 274 | 2847 / 3018 | 7841 / 7618 | 7663 / 7725 | 0.0074 | 4.0 / 15 | firecracker and the instance: ptep_clear_flush, memset |
| process start, noop cold (3) | 14843 | 718 / 794 | 5424 / 11195 | 8158 / 13945 | 8391 / 14843 | 0.0020 | 1.0 / 6.8 | |
| bare KVM VM, kvm-vm (3) | 25637 | 3776 / 3023 | 34193 / 25637 | 33457 / 23597 | 33686 / 19098 | 0.0041 | 0.3 / 5.0 | |

How each piece scales:

- **rootless-net** degrades with C and with alive namespaces. CPU per op rises from 0.022 s at
  C=1 to 0.090 s at C=128 with none held, and to 0.177 s at C=128 and 0.39 s at C=512 with
  1000 held. Under `kernel.bt` (C=128, 1000 held, 2000 namespace lives from their start to the
  end of the kernel's cleanup, three runs on a second boot of the same instance type on
  2026-11-23, at the commit before the netlink builder of changelog step 4) `rtnl_mutex` was held 25.7 ms per namespace life (24.1 to 26.6) and waited
  for 2.2 s. Of the hold:
  - 14.4 ms is one `rcu_barrier()` per namespace in the netns cleanup worker (`cleanup_net`
    -> `br_dev_uninit` -> `br_multicast_dev_del`): every bridge destroyed waits for the RCU
    callbacks of all 192 CPUs under the mutex, so a batch of 1000 dying namespaces is one
    hold of about 15 s. Linux 25ae123db10b removed that barrier (in 6.18.60; not in 7.0.y,
    which this host runs); the fcvm host kernels carry the removal (#1685).
  - 7.3 ms is `ip` and the namespace's creation (35 holds per namespace: loopback, the TAP,
    the bridge and every link change), 2.3 ms pasta (it makes and removes its TAP), 1.1 ms
    the rest of the cleanup worker.
  - Sampled on CPU under the mutex (7.6 ms per namespace), 3.3 ms is
    `wireless_nlevent_flush` (2.3 to 3.9): the wireless extensions' netdev notifier
    (`CONFIG_WEXT_CORE`) walks every network namespace on the host on every netdev event
    of any device. Another 1.6 ms is `do_one_broadcast`: every rtnetlink notification walks
    every NETLINK_ROUTE multicast listener on the host, and each pasta binds one
    (`nl_neigh_notify_init`, `RTMGRP_NEIGH`). Both grow with the namespaces alive.

  pasta also ran 9 expedited RCU grace periods per namespace, 3 of them under the mutex at
  its exit (`__tun_detach` -> `br_del_if` -> `nbp_vlan_flush`, `del_nbp`), plus `do_umount`
  and `put_mnt_ns`. Each
  namespace costs about 15 execs (unshare, sleep, newuidmap, newgidmap, nsenter and true, then
  nsenter, bash and ip twice, pasta, nsenter and ip per device poll).
- **disk** is serialized: about 150 ops/s at any C. Most of its CPU was FIEMAP of the
  snapshot's disk in every clone; the rest is the reflink itself, `cp` per
  clone and the source inode's extent lock.
- **vmm** includes the restored guest's own running: the File backend, dirty tracking on, took
  16,079 stage-2 faults (`kvm:kvm_guest_fault`) and 821 host page faults per clone at C=1, and
  43,825 stage-2 faults per clone at C=128. Its guests wait for a restore document no fcvm
  serves, so this is not a restored clone's steady cost.
- **health**: one `nsenter -U -n -- curl` probe is 4.4 ms of CPU at C=1 and 18 ms at C=128, half
  of it curl's dynamic loading (`ptep_clear_flush` on 192 CPUs). At 1000 clones polled every 10 s
  that is 100 probes/s, 1.8 cores.
- **fc-spawn** already enters the holder's namespaces with `setns` in `pre_exec`, no `nsenter`
  hop; the spawn is 2.3 ms of CPU at C=1 and 7.4 ms at C=128.
- **bare KVM VM**: `KVM_CREATE_VM` plus a memory slot, one vCPU and a GICv3 is 0.7 ms of CPU at
  C=1 and 2.4 to 5.8 ms at C=128 to 512; destroying one (`kvm_destroy_vm`) took 4 to 16 ms
  under `kernel.bt`. At 19,000 to 34,000 per second it is far from limiting. The x86-only PM
  notifier chain does not exist on arm64. No x86 metal flavor is in `pbox flavors` other than
  kvm-gpu-96.
- **process start**: an instance process (a small Rust binary, 2-worker tokio runtime) is 1 to
  2 ms of CPU. The first campaign capped it at 600/s; that was the driver forking from a
  192-thread process, fixed in the driver (posix_spawn, 8 workers) before these rows.

## tap-attach: what a reused namespace still costs

Not a row of the table above: `tap-attach` is the network work that would be left on a clone's
start and stop if its namespace, pasta and bridge were reused (README, "tap-attach"). One
on-demand c8g.metal-48xl, host kernel 7.0.0-1013-aws, one boot, 2026-11-22, this harness at the
commit that added this section. Every cell holds its namespaces for the whole run; an op is
one attach (Firecracker's three ioctls), 20 ms, and the close. The hold columns are
microseconds of `rtnl_mutex` hold per cycle from the cell's traced twin (`tap.bt`): the
helper's own calls, and the two link events the kernel's link watch worker handles for the
carrier going up and down. `1 s / hold` is how many cycles fit in a second of that mutex. The
cycles/s and wall columns are from the untraced cell, a burst of 200 to 1000 ops; a carrier-off
is handled up to a second after its close, so a burst can run ahead of its link events and a
sustained rate cannot. Rows marked "first attach" time a TAP's first cycle; the others made
one cycle while preparing.

| namespaces | C | cycles/s | attach+detach wall ms, p50 / p99 | TUNSETIFF | TUNSETOFFLOAD | close (its grace period) | carrier-on | carrier-off | hold per cycle, us | 1 s / hold |
|---|---|---|---|---|---|---|---|---|---|---|
| 2000 with pasta | 1 | 44 | 0.90 / 7.3 | 478 | 1 | 692 (264) | 363 | 268 | 1802 | 555 |
| 2000 with pasta | 32 | 857 | 13.80 / 40.5 | 436 | 1 | 487 (126) | 269 | 288 | 1481 | 675 |
| 2000 with pasta | 128 | 923 | 114.56 / 197.8 | 419 | 1 | 515 (114) | 271 | 289 | 1497 | 668 |
| 2000 with pasta | 512 (undercounted) | 752 | 560.41 / 1207.7 | 313 | 1 | 403 (118) | 269 | 297 | 1284 | 779 |
| 2000, no pasta | 1 | 46 | 0.39 / 5.5 | 221 | 0 | 212 (68) | 211 | 148 | 792 | 1263 |
| 2000, no pasta | 32 | 987 | 10.27 / 25.3 | 217 | 0 | 229 (34) | 246 | 145 | 836 | 1196 |
| 2000, no pasta | 128 | 2051 | 38.87 / 80.0 | 210 | 1 | 230 (31) | 300 | 147 | 887 | 1127 |
| 2000, no pasta | 512 (undercounted) | 2139 | 176.99 / 391.8 | 167 | 1 | 204 (33) | 282 | 143 | 797 | 1254 |
| 200 with pasta | 1 | 47 | 0.23 / 0.3 | 116 | 0 | 112 (34) | 82 | 102 | 413 | 2422 |
| 2000 with pasta, first attach | 1 | 44 | 1.08 / 12.6 | 451 | 294 | 686 (372) | 432 | 283 | 2147 | 466 |
| 2000 with pasta, first attach | 128 | 727 | 148.86 / 304.2 | 380 | 289 | 435 (105) | 359 | 281 | 1744 | 573 |
| 2000, no pasta, first attach | 1 | 46 | 0.57 / 1.7 | 228 | 409 | 171 (30) | 210 | 146 | 1165 | 859 |
| 2000, no pasta, first attach | 128 | 1067 | 93.70 / 191.5 | 264 | 253 | 277 (34) | 179 | 142 | 1117 | 896 |

Two sets of rows are not measurements of a clone life, found in review after the campaign:

- **The C=512 rows undercount.** `tap.bt` sums a hold from a return probe on `rtnl_lock`, and
  the kernel keeps a fixed number of return-probe instances (twice the CPUs, 384 here). With
  512 helpers asleep in `rtnl_lock` at once the rest are dropped: the dumps hold 0.77 to 0.95
  holds per call at C=512 and 1.00 in every other cell, so those rows' hold columns are short
  by up to a quarter. `tap-summary` now refuses such a cell.
- **The no-pasta rows count the wrong link events.** With no pasta the TAP is the bridge's
  only port. The worker handled the TAP's carrier-on after the helper's 20 ms were over in all
  but 1% of cycles, so those rows' carrier-on column is the average of one or two events, and
  the bridge's own carrier events (0.13 to 1.8 seen per cycle, 75 to 82 us each) are in no
  column. Every hold of the mutex on the host during those cells, per cycle, was 1.43 ms at
  C=1, 0.82 at C=32 and 0.59 at C=128, against the 0.79 to 0.89 the rows say. In the rows with
  pasta the same total agrees with the row (1.79, 1.34 and 1.50 ms at C=1, 32 and 128).
  `tap-summary` now reports both.

Both are measured again, with a tracer that drops nothing and a helper that stays attached
past the link watch delay, in the campaign of the PR that adds the namespace pool.

- **A reused namespace costs 1.5 to 1.8 ms of `rtnl_mutex` per clone life with 2000 pastas
  alive** (C=1 to 128), which caps attach and detach at 555 to 675 per second. The untraced
  bursts ran at 857 and 923 cycles/s at C=32 and 128, above that cap, because a burst runs
  ahead of its link events: the helper's own calls hold 0.92 to 0.94 ms per cycle, which
  alone would allow about 1070.
- **35 to 38% of it is not the VMM's own calls.** `TUNSETIFF` holds 0.42 to 0.48 ms and the
  close 0.49 to 0.69 ms (0.11 to 0.26 ms of that an expedited RCU grace period under the
  mutex, `__tun_detach` -> `synchronize_net`); each of the two carrier changes is then handled
  by the link watch worker for another 0.27 to 0.36 ms. `TUNSETVNETHDRSZ` holds under a
  microsecond.
- **`TUNSETOFFLOAD` is free after a TAP's first attach.** The first one changes the device's
  features and the bridge above it follows (0.25 to 0.41 ms); later ones set what is already set.
- **The hold grows with what is alive on the host.** 200 namespaces with pasta hold 0.41 ms
  per cycle where 2000 hold 1.80 (C=1). Each pasta binds a NETLINK_ROUTE socket to a multicast
  group (its neighbour listener, in the host's namespace), and every link notification walks
  every such socket on the host under the mutex: `do_one_broadcast` is 8.0% of all samples
  under the helper and 2.0% under the link watch worker, and `mutex_spin_on_owner`, the
  waiting, 21.9% (`PERF_FREQ=99`, `perf record -a -g` over the timed phase of the untraced
  C=128 cell with 1000 held, top frames by `perftop.sh`). How much of the hold is pasta's
  listener is not established: the no-pasta rows that were to show it are the ones above.
- **The attach and detach have to go.** With pasta a cycle holds 1.5 ms or more, and without
  it the host still held the mutex 0.59 to 1.43 ms per cycle. The design's bound for keeping
  them was 0.7 ms, so the pool has to keep the TAP attached and hand the VMM the descriptor.

## Changelog

Each step is a same-session A/B with the piece's own program.

### 1. state: #2000 (index names and PIDs)

Not this work; measured here as a check. `state`, before (f3f155fa) and after (6faf3305):
C=128 with 1000 held, 12.4 to 3,379 ops/s, 0.157 to 0.0009 CPU-s per op.

### 2. idle clone: fc-agent's 50 ms MMDS poll (branch fc-agent-idle-poll)

An idle clone's fc-agent fetched MMDS every 50 ms to notice a restore. With the output
connection established, the vsock transport reset a restore raises now wakes the watcher, and
the poll is a 10 s backstop. Not a piece program: 1000 File-backend clones of a snapshot made by
each build (`bench-start-rate-prep`, `bench-start-rate-run COUNT=1000 CONCURRENCY=64 HOLD=120`),
then 60 s of host busy CPU and `perf stat -a` from 30 s after the last clone was healthy. The
main arm's fcvm carried the disk-extents-once change, which no idle clone runs.

| arm | idle cores per 1000 clones | KVM exits /VM/s | exits to Firecracker /VM/s | context switches /VM/s |
|---|---|---|---|---|
| main | 26.78 | 1116 | 442 | 785 |
| fc-agent fix | 9.30 | 183 | 26 | 307 |
| main | 34.50 | 1362 | 436 | 775 |
| fc-agent fix | 11.66 | 217 | 26 | 307 |


Every arm's 1000 clones were healthy; resume to agent-ready p50 was 868 and 949 ms on main,
933 and 924 ms with the change, so restores still find the reset.

### 3. disk: count a snapshot's disk extents once (branch disk-extents-once)

Every clone counted the snapshot disk's extents with FIEMAP to choose between a reflink and a
subvolume snapshot; on btrfs FIEMAP walks each extent's back references, one per clone. The
count is now recorded on the file (`user.fcvm.extents`) the first time.

| cell | before ops/s | after ops/s | before CPU-s/op | after CPU-s/op |
|---|---|---|---|---|
| C=1, 0 held | 148 | 234 | 0.0073 | 0.0056 |
| C=128, 0 held | 146 | 241 | 0.078 | 0.0146 |
| C=512, 0 held | 98 | 174 | 0.96 | 0.021 |
| C=128, 1000 held | 146 | 235 | 0.069 | 0.016 |
| C=512, 1000 held | 87 | 250 | 1.15 | 0.013 |
| C=128, 1000 held, A B A B | 145.7, 142.3 | 239.4, 243.5 | 0.088, 0.090 | 0.0144, 0.0148 |

The rate is still near 240/s. What is left, measured with `--disk-shard-dir` (the clone
reflinks one of K reflinked copies of the disk, chosen by PID, which skips the extent count):
C=128, 0 held, one copy 232 ops/s, 16 copies 366 ops/s, 0.014 and 0.016 CPU-s per op. One
source file serializes part of it; the rest is not attributed yet.

### 4. rootless-net: the holder builds the links over netlink (#2025)

One c8g.metal-48xl, host kernel 7.0.0-1013-aws, one boot, 2026-11-22 to 23. Before: the parent
commit, where two `nsenter bash ip -batch` scripts make the links and pasta makes its TAP.
After: the holder makes every link over netlink, both TAPs are persistent, pasta attaches to
its TAP. Three runs of each cell, the two binaries in turn; mean (lowest to highest).

1000 held, untraced:

| C | | setups/s | setup p50 ms | CPU-s per setup (host, net) | instances torn down/s | from the first teardown until the kernel's cleanup is idle, s |
|---|---|---|---|---|---|---|
| 128 | before | 68 (63 to 72) | 1746 (1719 to 1769) | 0.204 (0.170 to 0.271) | 134 (130 to 139) | 30.2 (29.4 to 31.0) |
| 128 | after | 89 (74 to 111) | 1395 (1085 to 1624) | 0.087 (0.066 to 0.108) | 124 (115 to 129) | 28.9 (28.5 to 29.4) |
| 512 | before | 53 (40 to 77) | 6738 (5627 to 7374) | 0.815 (0.235 to 1.221) | 593 (586 to 600) | 24.7 (24.4 to 25.3) |
| 512 | after | 86 (75 to 106) | 5352 (4250 to 5969) | 0.092 (0.076 to 0.102) | 762 (121, 124 and 2042) | 23.5 (23.1 to 24.0) |

The last column is the cell's teardown wall time plus the time `matrix.sh` then waits for the
netns cleanup worker to go idle (1 s steps).

Traced (`attribution.sh`, C=128, 1000 held, 2000 namespace lives; `rtnl.py`, summaries in
`traces/rtnl/`):

| per namespace life | before | after |
|---|---|---|
| `rtnl_mutex` hold, ms | 25.70 (24.07 to 26.64) | 26.20 (24.98 to 27.26) |
| of it `rcu_barrier()` in the netns cleanup worker, one per bridge | 14.38 (14.14 to 14.58) | 15.48 (14.88 to 16.13) |
| all but that | 11.32 (9.94 to 12.07) | 10.72 (10.11 to 11.13) |
| the builder: `ip`, or the holder | 7.35 (6.12 to 8.01), 35 holds | 7.64 (7.04 to 8.03), 13 holds |
| pasta | 2.30 (2.14 to 2.40) | 0.88 (0.85 to 0.93) |
| netns cleanup worker, less its barrier | 1.09 (1.07 to 1.11) | 1.52 (1.49 to 1.56) |
| link watch worker | 0.22 (0.19 to 0.23) | 0.48 (0.45 to 0.49) |
| IPv6 address worker | 0.36 (0.35 to 0.37) | 0.19 (0.19 to 0.20) |
| on CPU under the mutex, ms | 7.64 (6.46 to 8.27) | 7.73 (7.15 to 8.11) |
| of it `wireless_nlevent_flush` | 3.30 (2.27 to 3.91) | 3.55 (2.98 to 3.92) |
| of it `do_one_broadcast` | 1.60 (1.56 to 1.67) | 1.41 (1.36 to 1.48) |
| expedited grace periods | 14.1 | 12.7 |
| wait for the mutex, s | 2.21 (1.98 to 2.34) | 2.40 (2.12 to 2.68) |

- The hold per namespace life did not change: the two ranges overlap with and without the
  barrier. The work on pasta's TAP moved. pasta no longer makes and removes a device (2.30 to
  0.88 ms); the holder makes it, the link watch worker handles the two carrier changes of
  pasta's attach and exit, and the cleanup worker unregisters it.
- CPU per setup fell to 0.43 of what it was at C=128 and to 0.11 at C=512. Setups per second
  are bound by the mutex, not by CPU: 63 to 72 against 74 to 111 at C=128, 40 to 77 against 75
  to 106 at C=512.
- Instances torn down per second is not a rate of namespaces destroyed. The kernel destroys
  them afterwards, a batch under one hold, and 2000 of them take 23 to 31 s from the first
  teardown whichever binary made them (2000 barriers of 14 ms are 29 s). What differs is how
  much of that the instances wait for: pasta's exit needs the mutex to detach its TAP, and it
  either gets in before the cleanup worker's first batch (2042/s, then 23 s of cleanup) or
  queues behind the batches (121/s, then 7 s).
- Three other ways to make the TAPs were measured the same way, in a build that could switch
  each on (three traced runs each, hold without the barrier): `IFF_NO_CARRIER` at creation, 10.54 ms (9.42 to 11.43) against 10.72
  in the same round; pasta making its own TAP and the holder bridging it afterwards, 11.38
  (11.22 to 11.59); `RTM_NEWLINK` of kind `tun` is refused by the kernel ("tun/tap creation
  via rtnetlink is not supported"). None is cheaper, so the TAPs stay as they are.
- The VMM's attach and detach on a prepared namespace, the tap-attach floor (C=128, 2000
  namespaces with pasta, one traced cell each): 1538 us of hold per cycle before, 1426 after.
