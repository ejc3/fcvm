# Race gym: design

Status: v1 built (section 8). Code references are to `main` at `65122f8c`.

The race gym runs several fcvm actors (processes and VMs) against one shared store, stops
each actor at named failpoints, and releases them in an enumerated order. A schedule that
breaks an oracle is written as a replay file, which reruns the same interleaving and becomes
a named case in `tests/test_lifecycle_interleave.rs`.

The two harnesses we have each cover part of this. `tests/test_fuzz_chaos.rs` drives one VM
per seed with a seeded op list, so the op order repeats and the timing does not. The cases in
`tests/test_lifecycle_interleave.rs` pin one interleaving each, written by hand. Neither runs
two fcvm processes against one content key, and neither searches interleavings.

The first acceptance test: four `fcvm podman run --gpu` launches on an image with no cached
snapshot, where one restore failed because `rootfs.raw` was missing. The gym must turn that
failure red on `main`, from an enumerated schedule rather than from timing.

## 1. Actors

An actor is one fcvm command line that the controller starts, plus the processes it spawns.
Each actor gets its own failpoint spec and an actor id (section 3).

| Actor | Command | Notes |
|---|---|---|
| cold run | `podman run` on a key with no snapshot | cold boot, pre-start snapshot create, and on NV2 or `--gpu` the throwaway teardown and the restore (`podman/mod.rs:2681`) |
| warm run | `podman run` on a key with a snapshot | startup or pre-start lookup, restore, possible `invalidate_unusable_snapshot` |
| create | `snapshot create --pid` | generation locks, VM lock, pause, save, `atomic_replace_dir` |
| serve | `snapshot serve` | holds the generation shared for its lifetime, binds the uffd socket, publishes the working set |
| clone | `snapshot run --pid` / `--snapshot` | shared generation lease, clone disk, mount redirect, LoadSnapshot |
| teardown | SIGTERM to any VM actor | `cleanup_vm_inner` (`common.rs:1836`) |
| prepare | `podman prepare` | disposable source, install, verify, publish (`podman/mod.rs:3190`) |
| prune | `snapshots prune` | unlocked list, then exclusive delete |
| kill | SIGKILL to one hop: sudo, test binary, fcvm, firecracker, holder, pasta | issued by the controller at a park, not a process of its own |
| reader | `fcvm ls --json`, `exec --pid` | unlocked readers of the state dir |
| gpu scrub | `fcvm gpu scrub` vs a `--gpu` launch | the scrub landed in #1158; the launch lease is #1164, not merged |
| nested builder | `podman run` inside an L1 that maps `/mnt/fcvm-btrfs` | image cache and setup store across a fuse-pipe map, where `flock` does not reach |

## 2. Shared resources

The rows below list what each actor touches, how it is protected today, and where nothing
protects it. The gym's failpoints sit at the create, use and cleanup steps in these rows.

| Resource | Create / use / cleanup | Protection today | Unprotected |
|---|---|---|---|
| `snapshots/<key>/` | `create_snapshot_core` builds `<key>.creating`, `atomic_replace_dir` (`common.rs:3745`) renames old aside and new in; restores read it; `delete_snapshot*` removes it | `<key>.lock` flock: creators and deleters exclusive, restores shared | between the two renames `<key>` is absent; prune lists `.creating` and `.old` and reads configs unlocked (`snapshots.rs:355`) |
| restore lease | `cmd_snapshot_run_inner` takes `<key>.lock` shared (`snapshot.rs:1838`), drops it after `restore_from_snapshot` (`:2793`) | shared flock | after the drop the clone depends on open inodes only |
| source VM dir `vm-disks/<source_vm_id>` | created by the source run; a restore of its snapshot records it with its owner, leases it (`storage/runtime_dir.rs`), creates it as an empty mountpoint if missing, binds the clone dir over it in a private mount namespace, then LoadSnapshot opens `…/disks/rootfs.raw`; the source's teardown removes it; a mountpoint the restore made is kept leased while the clone lives and removed by the clone's teardown, or by the next launch after a SIGKILL (`reclaim.rs`) | shared flock by restores until the load, exclusive by the teardown | none found |
| clone disk `vm-disks/<clone>/disks/` | `create_cow_disk`: btrfs subvolume snapshot of `<key>/disk/`, rename to `rootfs.raw` (`disk.rs:238`), else reflink | inside the restore lease | none found |
| `vm-disks/<id>/snapshot.lock` | taken by creators and `fcvm balloon` | exclusive flock | never taken by restore or teardown, and it lives inside the dir teardown removes |
| working-set sidecar | tmp, fsync, rename under shared `<key>.lock` and the sidecar lock (`working_set.rs:673`) | flock + identity recheck | tmp leaks on SIGKILL |
| uffd socket | unlink then bind (`server.rs:1106`), removed on Drop | pid + start time in the name | leaks on SIGKILL |
| state files | `save_state` writes `<id>.json.tmp` and renames under `<id>.json.lock`; `update_state` is locked read-modify-write; `claim_name` under `names.lock`; loopback IPs under `loopback-ip.lock` | flock, rename | the PID-collision sweep in `save_state` lists and judges liveness before any lock (`manager.rs:171`); `cleanup_stale_state` from `load_state_by_pid` runs under no global lock; `fcvm ls` judges liveness by `/proc/<pid>` alone |
| image cache | archive and storage image built under UUID temps and renamed; per-digest flock | rename is safe across VMs, the flock is not | removal of an invalid archive (`podman/mod.rs:1055`) relies on the flock alone |
| setup store | rootfs, initrd and kernel downloads use fixed temp names under a store flock (`setup/mod.rs:179`) | flock | two L1 VMs on one map share the fixed temp names |
| network | netns names settle on `ip netns add` EEXIST; bridged subnet under `bridged-subnet.lock`; routed IPv6 lease flock | mixed | bridged DNAT and FORWARD rules unlocked; clones share one host route |
| process tree | pdeathsig on firecracker, holder, pasta; `root-test-runner.sh` for the sudo hop | kernel; the vm dir (with pasta's PID file), a restore's mountpoint and image-cache temps are recorded with an owner whose flock the kernel releases, and the next launch reclaims them (`reclaim.rs`) | on fcvm SIGKILL the bridged netns and rules have no reclaimer (#1184) |

## 3. Control: park and release

**A new host action, `park`.** `FCVM_FAILPOINT=*:park:<dir>` parks every point (a named
entry, `<point>:park:<dir>`, parks one). A hit writes `<dir>/<actor>-<pid>/<seq>.reached`
with the point name, pid and a per-process hit count (`restore.pre_load#1`), then waits for
`<seq>.go`. `<actor>` is `FCVM_RACE_ACTOR`. The controller also sets `FCVM_RACE_PARENT` to
its own pid, and a process whose parent is another one drops its parks: the `fcvm exec`
that a health check spawns inherits the environment and would otherwise park on a timer.
The marker lines on stderr stay as they are. `park` is host-only, like `block_until_file`,
and keeps its 300 s cap. `failpoint::HOST_POINTS` and `GUEST_POINTS` list every point, and
`failpoint/tests/registry.rs` checks that every `failpoint::hit*` call in `src/` and
`fc-agent/src/` names a registered point and every registered point has a call, so a
deleted point cannot leave a case that passes without reaching its window.

**One step.** The controller releases one parked hit and waits for that actor's next
event, which is one of:

- `parked(p#k)`: it reached its next point;
- `exited(code)`: its process tree is gone;
- `waiting(lock)`: it is waiting on a lock another actor holds.

Lock waits are made visible by a note (a record that does not park) in the lock helpers:
`acquire_snapshot_dir_lock`, `lock_vm_snapshots`, the state manager's per-VM lock and its
global locks, the image-cache flock and the setup store lock. Most of these poll `try_lock`
every 100 ms, so the kernel cannot tell the controller that an actor is blocked. The note
says which lock it is. A step that produces none of the three events within the step
deadline (120 s) is reported as `UNMODELED WAIT` with the actor's thread stacks. That is a
defect in the gym (an instrumented helper is missing), and the schedule fails rather than
passes.

**Inside one actor.** Async tasks in one fcvm process park independently: the health
monitor can sit at `health.pre_persist_healthy` while the main task sits at
`restore.pre_resume`. The schedule names the hit it releases, not the actor. Work between
two points (the guest, Firecracker, tokio tasks with no point) is not controlled. The
schedule is deterministic at named points, and the gym makes no stronger claim. Guest points
in fc-agent cannot park, because the controller cannot create files in the guest. They stay
`sleep`/`burn` only.

**Points added for v1**, at the steps in section 2: `cache.miss`, `snapshot.create.locked`,
`snapshot.install.pre_rename`, `snapshot.install.post_rename`, `teardown.pre_kill`,
`teardown.pre_remove_data_dir`, `restore.locked`, `restore.post_clone_disk`,
`restore.pre_spawn_vmm`, `restore.pre_load`, `restore.unlocked`. The existing host points stay:
`cache.pre_ack`, `snapshot.pre_pause`, `snapshot.post_save_pre_resume`,
`restore.post_network_pre_resume`, `restore.pre_resume`, `health.pre_persist_healthy`.
An unarmed hit stays one static lookup.

## 4. Exploration

**Discovery.** One run of the scenario in lock-step (release each actor one step in turn)
records every actor's trace: the ordered hits it passes. For the cold-run scenario with
`--gpu` the winner of the generation lock passes 22 hits and the loser 15.
A discovery failure is reported with the schedules' failures and has its own replay
(`<scenario>-discovery.json`). A control or outcome failure there, which cuts an actor's
trace short, ends the gym before any schedule is derived from it.

**Phase 1: every pair of points (depth 2).** A pair schedule is `(a@i, b@j, first)`: run
lock-step until actor `a` is parked at its `i`-th hit and actor `b` at its `j`-th (an actor
at its target waits for the other); release `first` and run it until it exits or waits;
then release the other; then finish lock-step. If an actor exits or waits before its
target, the schedule is recorded as infeasible and costs only its prefix. Lock-step in the
prefix means every actor has passed its early points before either target is reached, so a
cold-cache prefix has both actors miss the cache.

Cost: `2·k_a·k_b` schedules per ordered pair of actor roles. For N identical actors, one
pair is enough, with `N = 2`: `2 × 22 × 15 = 660` schedules for the cold-run scenario, of
which 254 were infeasible (an actor exited, or waited on a lock the held actor owns, before
reaching its target). The other `N − 2` actors, when present, run lock-step with the prefix.

**Phase 2: seeded random walk (depth d).** PCT (Burckhardt et al., ASPLOS 2010): give each
actor a seeded random priority, always release the highest-priority parked hit, and at
`d − 1` seeded step indices drop the running actor below all others. With `n` actors and
`K` total steps, one run finds a given depth-`d` bug with probability at least
`1 / (n · K^(d−1))`. For `n = 4`, `K ≈ 55`, `d = 3` that bound is 1 in 12,100 per run, so the
walk is for breadth after Phase 1, not a guarantee. `SCHEDULES=` sets the run count
(default 200), and a run is identified by `(scenario, seed)`. Kills enter here: at each
step, with a seeded probability, the controller SIGKILLs one hop of one actor instead of
releasing a hit.

**Isolation between schedules.** Schedules run in parallel on one box. Each schedule salts
its content key with a per-schedule container argument (`… sleep 3600 # gym-<n>`), which
the key already hashes and the guest ignores, so actors in a schedule share one key and
schedules share none. Shared global state (state dir, loopback IP lock, image cache) is
still shared across schedules, as it is in production. A failure is reported only after a
serial replay of its schedule reproduces it.

## 5. Oracles

Checked at every park, while every actor is stopped, and again after all actors exit:

1. **Outcome.** Each actor meets its scenario's expectation within its deadline. For N cold
   runs that is "all healthy". For kill and invalidation scenarios it is "healthy, or exited
   non-zero with an error line naming the cause". A hang fails.
2. **Artifacts.** The salted key's generation is complete: `config.json` parses and names
   the key, `memory.bin` and `vmstate.bin` exist at their recorded sizes, `disk/disk.raw`
   exists. A fresh restore of the key after the schedule comes up healthy. Artifacts built
   offline and stored clean are checked by `e2fsck -fn` (`.storage-v2.img`, `layer2-*.raw`)
   or by their content hash (kernel, initrd, pmem entries). A snapshot's `disk.raw` is taken
   from a running guest and needs journal recovery, so `e2fsck -n` cannot judge it, and the
   restore check stands in for it.
3. **Leaks.** The controller sets `PR_SET_CHILD_SUBREAPER`, so anything an actor orphans
   reparents to it. After the schedule: no descendant process; no state file, `vm-disks/<id>`,
   data dir, uffd socket or `.creating`/`.old` directory that names a schedule actor's VM;
   no mount under the store in `/proc/self/mountinfo`; for bridged actors, no netns, veth
   or iptables rule naming a VM id. A recreated `vm-disks/<source_vm_id>` counts as a leak.
4. **State consistency.** Every state file names a pid whose `/proc/<pid>/stat` start time
   matches `pid_start_time`; a state reading `healthy` has a live VMM; `fcvm ls --json` and
   the files agree.
5. **Source untouched.** Every restore leaves the snapshot's `config.json` and `vmstate.bin`
   bytes, the `(len, mtime, ino)` of `memory.bin` and the `disk/` contents unchanged. When a
   scenario keeps a live source, the guest's routes, neighbours and addresses are compared
   across the schedule, as `snapshot_leaves_the_source_network_untouched` does.

Kill scenarios break oracle 3 today: section 2 lists what leaks on SIGKILL with no
reclaimer. Those scenarios stay out of CI until the leaks are fixed, and each leak the gym
finds gets an issue.

## 6. Output and replay

A failing schedule is written as `race-gym/<scenario>-<id>.json`:

```json
{ "format": 1, "scenario": "cold-run", "fcvm_rev": "<sha>", "tier": "firecracker",
  "params": { "actors": 2, "image": "alpine:latest", "kernel_profile": "nested" },
  "schedule": { "kind": "pair", "a": 0, "i": 9, "b": 1, "j": 12, "first": 0 },
  "steps": [ { "release": "1:cache.miss#1", "event": { "parked": "cache.pre_ack#1" } },
             { "release": "0:teardown.pre_remove_data_dir#1", "event": { "exited": 0 } } ],
  "failed": [ { "oracle": "outcome", "actor": 1, "detail": "<the actor's error line>" } ] }
```

`make race-gym REPLAY=<file>` releases the recorded hits in the recorded order and checks
that each step produces its recorded event. A different event stops the run with
`REPLAY DIVERGED at step n`, which is a failure. A replay that cannot reach its window must not
pass. A fix that adds synchronization in the window usually keeps every step's event and
turns the oracle green. A fix that changes an event (a release that now waits on the new
lock) is re-recorded with `RERECORD=1`, which runs the replay's schedule again and writes a
new file; the diff of the replay file shows the changed step for review.

A replay holds no host values: GPUs come from `RACE_GYM_GPUS` on the host that replays it.

`make race-gym REPLAY=<file> EMIT=<name>` copies the file to
`tests/race_gym/cases/<name>.json` and appends a thin case to
`tests/test_lifecycle_interleave.rs`:

```rust
#[cfg(feature = "race-gym")]
#[tokio::test]
async fn test_lifecycle_interleave_gym_<name>() -> Result<()> {
    race_gym::replay_case(include_str!("race_gym/cases/<name>.json")).await
}
```

`make race-gym-cases` runs them. They sit behind the `race-gym` feature until the fc-mock
tier can run them in CI (section 7): a `cold-run` replay on the Firecracker tier needs a
host with the GPUs or the NV2 profile it was recorded with.

The case fails while the bug exists and passes once it is fixed, so it is the red test the
repo's defect rule asks for. The replay names points, not line numbers, so it survives
unrelated edits.

## 7. Cost model

Measured on one g4dn.metal (96 vCPU, 8 T4 on vfio-pci), `cold-run` with
`--kernel-profile gpu`, one `--gpu` per actor, a `localhost/` image:

| Run | Schedules | Per schedule (p10 / p50 / p90) | Wall |
|---|---|---|---|
| discovery, 2 actors lock-step | 1 | 11.8 to 12.0 s | 12 s |
| Phase 1, every pair, 2 actors, 4 at a time | 660 | 11.9 / 13.3 / 14.4 s | 2,360 s |
| Phase 2, PCT depth 3, 4 actors, 2 at a time | 40 | 23.5 / 25.0 / 27.0 s | 926 s |
| one replay of a pair schedule | 1 | 10.3 to 10.6 s | 10.5 s |

Per schedule includes the end oracles, so also the fresh restore of the key. Wall includes
discovery and the serial replay that confirms one schedule per failure signature. The guest
must not pull its image from a registry: a first run with `alpine:latest` hit Docker Hub's
unauthenticated rate limit after about 100 cold boots and failed every later schedule.
`RACE_GYM_IMAGE` should name a `localhost/` image, which fcvm exports once into the image
cache. Build one with the invoking user's podman (one registry pull, then none):

```bash
printf 'FROM docker.io/library/alpine:latest\n' | podman build -t localhost/race-gym-alpine:1 -
```

`RACE_GYM_IMAGE` also replaces the image a replay recorded, so
`RACE_GYM_IMAGE=localhost/race-gym-alpine:1 make race-gym REPLAY=<file>` and the emitted
cases (`make race-gym-cases`) run on a host that builds the image under any name. The emitted
cold-run case records `localhost/race-gym-alpine:1`, so without the variable the host needs
that tag.

Not measured: the arm64 nested tier and fc-mock. A pbox kvm-96 (c8g.metal-24xl) boots a
stock kernel without `kvm-arm.mode=nested`, so `--kernel-profile nested` needs the fcvm host
kernel installed first; the GPU host reaches the same converge path through `--gpu`.

fc-mock answers `/snapshot/create` and `/snapshot/load` with 200 and touches no files
(`fc-mock/src/api_server.rs:224`), and it never sends the guest's `cache-ready`. To be useful
here it must write a `vmstate.bin` that embeds each drive's `path_on_host` and a sparse
`memory.bin`, open the embedded drive paths on load inside its mount namespace and fail as
Firecracker does when one is missing, and send `cache-ready` and the restore ACK. That is the
next stacked PR, and the CI lane depends on it. The converge path runs only on NV2 profiles
(arm64 `nested`) and `--gpu` (`podman/mod.rs:2681`), so the fc-mock lane needs an arm64
runner with the nested profile's kernel; the existing `fc-mock` job runs on `ubuntu-latest`
(x86_64).

CI budget: Phase 1 on fc-mock, capped at 15 minutes, once that tier exists. Phase 2 and the
Firecracker and GPU tiers run from `make race-gym` on a dedicated metal host, by hand or
nightly, never in the PR gate. Phase 1 of `cold-run` on the GPU host costs 40 minutes.

## 8. v1 scope

Built:

1. `failpoint`: the `park` action, `note`, `FCVM_RACE_PARENT`, the `HOST_POINTS` and
   `GUEST_POINTS` registry with its source-level test, the 11 points in section 3, and
   lock-wait notes in the snapshot, per-VM snapshot, state, image-cache and setup-store locks.
2. `tests/race_gym/`: the controller (spawn, park, release, events, subreaper), pair
   explorer, PCT walk, replay, and the `cold-run` scenario with oracles 1 to 4.
3. `tests/test_race_gym_controller.rs`: the controller against shell actors with a planted
   check-then-act race, no VM. The pair search must find it, its replay must fail the same
   way 5 times out of 5, a tampered replay must diverge, and a silent actor must fail as an
   unmodeled wait. It runs in the unit suite.
4. `make race-gym`, `make race-gym REPLAY=`, `RERECORD=1`, `EMIT= PIN=`, and
   `make race-gym-cases`, behind the `race-gym` cargo feature, so no suite runs them.

Acceptance, on `main` plus the gym (GPU host above): Phase 1 of `cold-run` failed one schedule
of 660 with the reported error, schedule 348: actor 0 held at
`teardown.pre_remove_data_dir#1`, actor 1 at `restore.pre_load#1`, actor 0 released first.
Actor 1's LoadSnapshot fails with `No such file or directory ... vm-disks/vm-<source>/disks/rootfs.raw`.
Its replay failed the same way 10 times out of 10. Every one of the 660 pair schedules and 40
walks also failed the leak oracle on the converge path's orphaned source directory (#1181).

With the fix for that race (#1188, merged locally for the check) the same 660 pair schedules
gave no outcome failure. Re-recorded with the fix, schedule 348 shows the change at one step:
releasing actor 0 at `teardown.pre_remove_data_dir#1` now records a wait on the lock of
`vm-disks/vm-<source>`, actor 1 loads, and actor 0 continues. That re-recorded file is the case
`test_lifecycle_interleave_gym_cold_run_restore_after_source_teardown`, pinned to the outcome
oracle: with #1188 it passes (13.4 s); on `main` it fails with `REPLAY DIVERGED at step 20`.

The same run found one defect in the gym itself (schedule 288, an unmodeled wait that did not
reproduce alone): an actor with two parked hits at once, fcvm's health monitor and its main
task, lost the second hit's park when the first was released, and the controller waited for an
event the actor had already produced. Fixed, and pinned by
`race_gym_a_second_parked_hit_of_the_released_actor_is_its_event`.

Later, each its own stacked PR: the fc-mock snapshot model and the CI lane; oracle 5;
kill, prune, serve and prepare scenarios; the nested builder; `gpu scrub` vs a launch, with
and without the #1164 lease.
