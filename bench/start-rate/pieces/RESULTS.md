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
  1000 held. Under `kernel.bt` (C=128, 1000
  held, start and teardown in one window) rtnl_mutex was held 21.1 s and waited for 2,280 s in
  total, 1.14 s of wait per namespace. Of the hold, 14.4 s was the netns cleanup worker
  (`cleanup_net`, which tears down a batch of dying namespaces under one hold: one batch held it
  8 to 16 s), 4.2 s pasta (2 holds per pasta) and 2.1 s `ip` (4 holds per namespace). pasta also
  ran 9 expedited RCU grace periods per namespace, 3 of them under rtnl at its exit
  (`__tun_detach` -> `br_del_if` -> `nbp_vlan_flush`, `del_nbp`), plus `do_umount` and
  `put_mnt_ns`. `do_one_broadcast` under `ip` walks every rtnetlink multicast listener in every
  namespace, and each pasta binds one (`nl_neigh_notify_init`, `RTMGRP_NEIGH`), so the hold
  grows with the namespaces alive. Each
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

Two pairs, run in this order. Host load over each 60 s window (`host.tsv`): `load1` still
carries the clone starts that ended 30 s before the window, so the runnable count is the
better guide.

| pair | arm | idle cores per 1000 clones | KVM exits /VM/s | exits to Firecracker /VM/s | context switches /VM/s | load1 mean | runnable mean |
|---|---|---|---|---|---|---|---|
| 1 | main | 26.78 | 1116 | 442 | 785 | 34.4 | 26.4 |
| 1 | fc-agent fix | 9.30 | 183 | 26 | 307 | 79.5 | 5.9 |
| 2 | main | 34.50 | 1362 | 436 | 775 | 119.4 | 29.1 |
| 2 | fc-agent fix | 11.66 | 217 | 26 | 307 | 129.6 | 5.8 |

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

### 4. disk: reflink while one of 8 slots is free, else a subvolume snapshot (#2014)

The rest is the source inode's lock: every clone of a snapshot reflinks the same file, and
`btrfs_remap_file_range` holds that inode's lock (`lock_two_nondirectories`) for the whole
reflink. Under bpftrace, 1000 clones at C=128 spent 514.6 s in reflinks, 510.3 s of it waiting
for that lock. A clone now reflinks while it can take one of the snapshot's 8 reflink slots
(`flock` without waiting) and takes a subvolume snapshot of the snapshot's `disk/` when all 8 are
held. A subvolume snapshot takes no such lock.

Same session, c8g.metal-48xl, stock host kernel 7.0.0-1013-aws, the store a btrfs loop file in
RAM, an 863-extent disk, N=1000. Two rounds, in the order main, this PR, always-subvolume (this
PR with `FCVM_REFLINK_MAX_EXTENTS=0`), then again. Both rates are listed; p50 is the mean of the two.

| C | held | main clones/s | this PR clones/s | always-subvolume clones/s | p50 ms main / PR / subvolume |
|---|---|---|---|---|---|
| 1 | 0 | 287, 285 | 286, 286 | 128, 140 | 3.5 / 3.5 / 7.4 |
| 1 | 1000 | 245, 283 | 285, 286 | 100, 109 | 3.8 / 3.5 / 9.3 |
| 8 | 0 | 237, 236 | 237, 238 | 561, 640 | 47.4 / 47.3 / 12.7 |
| 8 | 1000 | 139, 236 | 232, 237 | 281, 353 | 54.5 / 47.1 / 24.7 |
| 128 | 0 | 238, 235 | 1689, 1263 | 1316, 1287 | 533.7 / 85.7 / 82.1 |
| 128 | 1000 | 166, 234 | 1174, 1000 | 1393, 1193 | 659.1 / 110.9 / 100.6 |
| 512 | 0 | 253, 256 | 1977, 1641 | 1943, 1779 | 1947.6 / 269.4 / 228.7 |
| 512 | 1000 | 256, 258 | 1936, 1635 | 1782, 2081 | 1933.9 / 242.7 / 274.9 |

Teardown ran at 144 to 377 clones/s on main, 171 to 1319 with this PR and 702 to 1308 always
subvolume. Up to 8 at once the PR is main (reflinks); always-subvolume is faster at C=8 on this
quiet host, but the fleet measured the subvolume commit at 193 ms p50 and 1.8 s p99 while it was
taking snapshots, against a C=8 reflink p50 of 47 ms here.

### 5. vmm: stage-2 faults per clone, and the write set (#2035)

**Where a clone's stage-2 aborts come from.** Real clones (`bench-start-rate-run`, rootless, 20
at C=1 and 256 at C=128, all held at the end), one snapshot, fcvm built from 9d6b094 (#2014's first draft, on
main of 2026-11-22) with Firecracker 3057dcb7, c8g.metal-48xl, stock host kernel 7.0.0-1013-aws. bpftrace
on `kvm_guest_fault` (every abort, with its kind and guest page), `io_mem_abort` (MMIO) and
`kvm_handle_guest_abort` (on-CPU time: entry to return less time switched out). Every value is a
mean per clone; on-CPU us per abort is the on-CPU ms divided by the aborts. At C=1 every abort's
guest page was kept, so RAM aborts split exactly: RAM = aborts - MMIO, distinct pages = read first
+ write first, repeats = RAM - distinct. At C=128 the tracer dropped most per-page lines, so only
the totals and the copy-on-write count (permission faults on a write, all RAM) are given.

| restore | C | aborts | MMIO | RAM | read first | write first | repeats | of them copy-on-write | on-CPU ms | on-CPU us / abort |
|---|---|---|---|---|---|---|---|---|---|---|
| File | 1 | 36,804 | 14,018 | 22,786 | 13,052 | 4,615 | 5,119 | 5,041 | 137 | 3.7 |
| File | 128 | 47,347 | 8,290 | | | | | 7,216 | 474 | 10.0 |
| UFFD copy | 1 | 34,844 | 12,188 | 22,656 | 12,680 | 4,472 | 5,504 | 4,568 | 65 | 1.9 |
| UFFD copy | 128 | 44,661 | 8,235 | | | | | 6,963 | 454 | 10.2 |
| UFFD minor | 1 | 45,290 | 13,714 | 31,577 | 12,529 | 3,896 | 15,152 | 6,367 | 180 | 4.0 |
| UFFD minor | 128 | 73,600 | 7,706 | | | | | 7,201 | 2,451 | 33.3 |

Every stage-2 mapping was 4 KiB. UFFD minor mode is the slow one under concurrency: at C=128 it
takes 73,600 aborts per clone at 33.3 us each, 2.5 s of handler time per clone, against 10 us
for File and UFFD copy; that is not attributed further here.

**The write set (#2035)** removes the copy-on-write from a File clone's restore. End-to-end
(`startrate.py` to healthy, rootless, File backend), to 1000 clones alive, main (878fc71e)
against #2035, three fresh snapshots per arm, same session, stock host kernel 7.0.0-1013-aws:
30.0-35.3 against 38.6-59.3 clones/s at C=128 and 28.5-34.8 against 34.5-58.3 at C=512. Measured
there the same way as above, but as the median per clone over 1000, the handler's on-CPU time
was 2.7-4.2 s per clone on main against 60-81 ms with #2035, and anonymous memory per clone
36.5-49.6 against 38.4-49.1 MB. The table is in #2035. The C=128 row above (474 ms) is the
smaller run: 256 clones, an older head, one snapshot.

**MMIO aborts by device**, from `kvm_mmio` (each guest read and write, kernel-handled notifies
included), mean per clone:

| head | clones | net | serial | block | vsock | rng | total |
|---|---|---|---|---|---|---|---|
| #2024's head (bd898093) | 10 at C=1, held 5 s | 11,349 | 4,392 | 655 | 53 | 8 | 16,457 |
| main (878fc71e) | 5 at C=1, held 10 s | 234 | 4,270 | 444 | 56 | 12 | 5,016 |

On #2024's head the net device's aborts were 4,313 interrupt-status reads, 4,313 interrupt
acknowledgements and 2,723 queue notifies (the notifies are ioeventfds, handled in the kernel).
On main they are under 250; which change between the two heads removed that traffic is not
identified here. What remains is the console: on main, 2,975 transmit-register writes per clone in
the first second, one exit to Firecracker per byte, which are fc-agent's 37 restore lines (about
2.7 KB) on ttyS0. These MMIO counts are from different runs than the table's MMIO column.
