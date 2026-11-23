# start-pieces: what one clone start costs, piece by piece

`bench/start-rate` measures whole `fcvm snapshot run` clones reaching healthy per second. This
directory splits that start into its pieces, runs each piece alone N times at concurrency C with
A instances of it already alive, and reports ops/s, latency and CPU per op for each, so each
piece can be measured and changed on its own. Results and the changelog are in `RESULTS.md`.

## What `fcvm snapshot run` does to start one rootless clone

In order, for `fcvm snapshot run --pid <serve>` (UFFD) or `--snapshot <tag>` (File backend) with
`--network rootless`, the default. Line numbers are as of the commit that added this file.

| # | piece | what runs | source | sample program |
|---|---|---|---|---|
| 1 | process start | exec of `fcvm`, a 2-worker tokio runtime (#1995), argument parsing, logging setup | `src/main.rs` | `noop` (the instance process itself) |
| 2 | snapshot config | the per-snapshot lock taken shared, `config.json` read and parsed | `cmd_snapshot_run_inner`, `src/commands/snapshot.rs:3092` | not split out |
| 3 | reclaim | `reclaim_abandoned` scans what killed VMs left behind | `src/commands/snapshot.rs:3214`, `src/reclaim.rs` | not split out |
| 4 | state claim | the name lock, a scan of every VM's state for the name, the first state write; then the loopback IP allocation under its own lock | `StateManager::claim_name` and `allocate_loopback_ip`, `src/state/manager.rs:343,988`; called at `snapshot.rs:3463,3753` | `state` |
| 5 | listeners | volume servers, output listener, status listener, the restore boot-plan and completion listeners on the clone's vsock paths | `snapshot.rs:3570-4320` | not split out |
| 6 | network config | `PastaNetwork::setup`: MAC, addresses, one `ip -6 addr show` fork per process for the host IPv6 | `src/network/pasta.rs:2202,1350` | `rootless-net` stage `config` |
| 7 | namespace holder | `fcvm __netns-agent` unshares its user and network namespaces, then `newuidmap` and `newgidmap`, then the agent confirms it is ready | `spawn_namespace_holder`, `src/commands/common.rs`; `src/network/netns_agent.rs` | `holder`; `rootless-net` stage `holder` |
| 8 | disk | the clone's disk directory as a btrfs subvolume snapshot of the snapshot's `disk/`, or a reflink of `disk.raw` when its extent count is at most 4096 (`REFLINK_MAX_EXTENTS`); runs beside 9 | `DiskManager::create_cow_disk`, `src/storage/disk.rs:587` | `disk` |
| 9 | links | one request to the holder, which makes the VMM's TAP and pasta's (both persistent), `br0` with its MAC and address, and sets the MTUs, over netlink in its own namespace (one request per TAP, four for the bridge); runs beside 8 | `PastaNetwork::build_namespace`, `netns_agent::build`, `src/network/rtnl.rs` | `rootless-net` stage `links` |
| 10 | pasta | pasta spawned in the namespace, where it attaches to its TAP, and waited for through its PID file (ready once pasta has written its PID into it) | `PastaNetwork::post_start`, `start_pasta` | `rootless-net` stage `pasta` |
| 11 | Firecracker spawn | fork/exec of Firecracker with `setns` into the holder's user and net namespaces in `pre_exec` (no `nsenter` hop), then a wait for its API socket | `VmManager::start`, `src/firecracker/vm.rs:258`; `install_namespace_pre_exec`, `src/utils.rs:1092` | `fc-spawn`; `vmm` stage `spawn` |
| 11a | KVM VM | inside Firecracker: `KVM_CREATE_VM`, the memory slot, the vCPU (`KVM_ARM_VCPU_INIT`), the GICv3 | Firecracker's `vmm` crate | `kvm-vm`: the same ioctls with no VMM process, a floor under 11 and 12 |
| 12 | snapshot load | `PUT /snapshot/load` with the network, vsock and drive overrides, `resume_vm: false`. File backend: Firecracker maps `memory.bin` `MAP_PRIVATE`. UFFD: Firecracker connects to the serve's socket and hands over its userfaultfd; the serve may replay the recorded working set | `restore_from_snapshot`, `common.rs:4141`; `src/uffd/server.rs` | `vmm` stage `load` |
| 13 | balloon read | `GET` of the balloon device, when the snapshot has one | `common.rs:4202` | not split out |
| 14 | resume | `PATCH /vm Resumed`; the vCPUs run and fault in guest memory | `common.rs:4264` | `vmm` stage `resume` |
| 15 | MMDS, state save | the restore epoch into MMDS; the state file saved with the network and PIDs | `common.rs:4279,4382` | not split out |
| 16 | guest restore | fc-agent sees the transport reset, fetches the restore document over vsock, pins neighbours, takes the host identity, runs the snapshot network boundary (retires snapshot-time sockets by cookie, opens the gate), steps the clock, restarts chronyd, re-registers exec, publishes | `handle_clone_restore`, `fc-agent/src/restore.rs:388`; `fc-agent/src/snapshot_network.rs` | none: owned by the end-to-end work (see below) |
| 17 | output connect | the host waits for fc-agent's output connection, which gates the health monitor | `wait_for_restored_output`, `snapshot.rs:4709` | none (part of 16) |
| 18 | health check | the monitor polls from 100 ms up; each rootless HTTP probe is one GET from a socket the holder made in its namespace; Healthy is written to the state file, then polls slow to every 10 s | `spawn_health_monitor_full`, `src/health.rs:71`; `check_http_health_holder`, `health.rs` | `health` |
| 19 | steady cost | each held clone: an idle guest (KVM exits, timers), its fcvm process, pasta, the holder, a probe every 10 s | | `vmm` and `health` with ALIVE held, `steady_per_s` |

Pieces 2, 3, 5, 13 and 15 have no sample program yet: at C=1 the end-to-end harness puts
`pre-log` plus `state+listeners` (1 to 5 together with 4) at 17 ms, and none of them forks.

Piece 16 belongs to the end-to-end investigation of the resume-to-agent-ready stage and is not
measured here. The `vmm` piece resumes a real guest from a real snapshot, but no fcvm listener
answers it, so its guest waits for a restore document that never comes; its steady cost is
an idle guest's, not a restored clone's.

## The sample programs

One binary, `start-pieces`, through fcvm's own library code (the `fcvm` crate):

```bash
make bench-start-pieces-build
make bench-start-pieces PIECE=rootless-net COUNT=1000 CONCURRENCY=128 ALIVE=1000 OUT=/mnt/fcvm-btrfs/pieces-out/net-c128-a1000
```

`drive` (the parent) spawns one `instance` child process per op, as each clone is one fcvm
process with the same 2-worker runtime. A child prepares what its piece needs untimed (a holder
for `fc-spawn`; a holder, the rootless network and the disk for `vmm`; a namespace with an HTTP
responder for `health`), says `ready`, runs the piece when told `go`, says `done` with its own
stage times, and then holds what it made until it is told `exit`, when it tears it down and
says `torn`. The driver:

1. makes and starts ALIVE instances (untimed): the "already alive" load;
2. prepares COUNT instances (untimed);
3. samples 3 s of idle: the CPU of the ALIVE and prepared instances, taken off each op;
4. starts the COUNT instances, at most CONCURRENCY in flight (timed);
5. samples 10 s with all ALIVE+COUNT held (`steady_per_s`, `steady_per_instance_per_s`);
6. tears everything down at CONCURRENCY (timed, `teardown`).

| field | meaning |
|---|---|
| `timed.ops_per_s` | ops completed / wall time of phase 4 |
| `timed.latency_ms` | per op, the driver's `go` to `done` (p50, p90, p99, max) |
| `timed.op_ms`, `stages_ms` | the child's own timing of the piece and its stages |
| `timed.per_op.cgroup_cpu_s` | the run's cgroup `usage_usec` delta / ops: every descendant, alive or reaped |
| `timed.cgroup_cpu_s_per_op_net` | the same less the idle CPU, over the window, of everything held before it: the ALIVE instances and the COUNT prepared ones (null without a cgroup of its own) |
| `timed.per_op.host_busy_cpu_s`, `host_cpu_s_per_op_net` | `/proc/stat` busy time / ops, which also counts kernel threads outside the cgroup (netns cleanup, RCU, btrfs commits, kworkers) |
| `timed.per_op.ctxt`, `ipi` | host-wide context switches and IPIs (`/proc/interrupts` IPI rows) per op |

The run needs a cgroup of its own, so the Makefile target starts it with
`systemd-run --user --scope`; `drive` refuses to run in a cgroup that holds other processes.
`COLD=1` times each op from the instance's spawn, so its process start and preparation are
part of the op; `noop` with `COLD=1` is the cost of an instance process. `--disk-shard-dir DIR`
(in `ARGS=`) makes the disk piece reflink from one of the files in DIR instead of the snapshot's
disk. `attribution.sh` runs one cell per piece under `kernel.bt` (rtnl_mutex hold and wait, RCU
syncs and their stacks, netns cleanup, KVM VM create and destroy, copy-on-write and userfaultfd
faults, KVM exits by class, execs); its timings are not measurements. The tracer stays until
the kernel has destroyed the cell's namespaces, which it does after the driver has exited, and
it traces `rtnl_mutex` as a mutex: a probe on `rtnl_lock()` misses every hold taken inside
rtnetlink.c, where that function is inlined. `rtnl.py CELL.bt OPS` prints the hold per op by
holder, with the `rcu_barrier()` time under it apart.
`perftop.sh` writes each cell's top frames, and `table.py` prints the table in `RESULTS.md`.
The `kernel.bt` output of the first table's attribution pass (C=128, 1000 held, one file per
piece; start, steady and teardown in one window) is in `traces/`.
Those dumps were taken with the tracer stopped at the driver's exit and with probes on
`rtnl_lock()`. For the pieces that make namespaces, `traces/rtnl/` holds what `rtnl.py` reads
from complete dumps instead: `rootless-net` before and after changelog step 4 (three runs
each), `holder` and `health`.

`PERF_FREQ=99` adds `perf record -a -g` over the timed phase (`perf.data`), and
`PERF_EVENTS=...` adds `perf stat -a` over the timed and steady phases. Host-wide numbers mean
anything only on a quiet box.

| piece | needs |
|---|---|
| `noop`, `state`, `holder`, `rootless-net`, `health`, `kvm-vm` | nothing |
| `disk` | `SNAPSHOT_DIR=` a snapshot directory (`.../snapshots/<tag>`) |
| `fc-spawn` | `FIRECRACKER=` the Firecracker binary fcvm restores with |
| `tap-attach` | nothing; `tap-floor.sh` runs its campaign |
| `vmm` | both; `UFFD_SOCKET=` a `snapshot serve` socket to load through UFFD |

## tap-attach: what a reused namespace still costs

`tap-attach` is not a piece of today's start. It measures what would be left of piece 9 and 10
if a clone took a namespace that already had its TAP, pasta and bridge: the VMM's attach to
the TAP when it loads the snapshot and its detach when it exits. Each instance prepares a full
rootless network (untimed), a helper process inside the holder's namespaces, and
`--tap-warm-cycles` attach and detach cycles (1 by default), because a reused namespace's TAP
has had a VMM before and a TAP's first attach costs more (it changes the device's offload
features, and the bridge follows). `--tap-no-pasta` prepares the same namespace with no pasta
(`PastaNetwork::isolated`). The timed op is
one clone life's worth: the helper opens `/dev/net/tun` and makes Firecracker's three ioctls
(`TUNSETIFF`, `TUNSETVNETHDRSZ`, `TUNSETOFFLOAD`), stays attached for `--tap-hold-ms` (20 by
default, so the kernel handles the carrier-on before the carrier-off), and closes the
descriptor. The op's stages are the helper's own timing of each call; `attach+detach` is their
sum, and the op's latency also holds the 20 ms.

Each of the four calls takes `rtnl_mutex`, and the two carrier changes are handled afterwards
by the kernel's link watch worker under the same mutex. `tap.bt` attributes the hold time and
the RCU grace periods to each call and to the link events of TAP devices. The driver runs it
over the timed and steady phases when given `PERF_EVENTS=tap ARGS="--perf-cmd
$PWD/bench/start-rate/pieces/tap-bt.sh"`, and `start-pieces tap-summary CELL` turns its two
dumps into per-cycle numbers. A traced cell's timings are not measurements.

```bash
bash bench/start-rate/pieces/tap-floor.sh /mnt/fcvm-btrfs/pieces-out/tap   # every cell holds 2000 pastas
```
