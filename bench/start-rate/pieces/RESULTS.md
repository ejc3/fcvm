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
