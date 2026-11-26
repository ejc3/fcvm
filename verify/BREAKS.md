# Breaks: where a vCPU's wake-up is lost, per kernel and VMM

`lake exe breaks` searches every rule book and configuration breadth-first from `init`, visits
every reachable state, and tests every property of `WakeVerify/Properties.lean` on it. For each
kind of break it prints a shortest run that reaches it, step by step with the source each step
models. A kind is the property, the wait site (queue head or queue node), and what happened to the
kick. The runs named below are copied from that output into `WakeVerify/Breaks.lean`, where Lean's
kernel replays each one and evaluates the property (`by decide`) on every `lake build`. That a run
is a shortest one of its kind is the search's output; it is not proved.

The model was first checked against what was already known: it returns #2125 as a shortest run to
a hang on v7.3.3 (15 steps), it returns #2131 on every kernel (16 steps), and on v6.14 without
snapshots it finds nothing, which `v614_sound_without_snapshots` then proves.

## The run

| Command | Searches | States visited | Wall time | Peak memory |
|---|---|---|---|---|
| `JOBS=7 ./search-expected.sh full` | 76 (6 rule books x 12 configurations, 4 restore orders) | 449,763,133 | 7 min 53 s as seven processes side by side (one a rule book, and one for the restore orders); about 45 min of CPU in all | 3.3 GB a process |

Run on 2026-11-26 on a shared arm64 dev box. One process for all 76 searches takes the sum and
was not timed. No search hit its state limit. What every search printed (its states, each kind
with its count and the length of its shortest run, the audit's numbers) is committed as
`WakeVerify/search.expected`, and `search-expected.sh` fails on any difference from it, in either
direction: `quick` (19 searches) in `make verify` and in CI, `full` (all 76) in `make
verify-search` and the dispatch job. The largest (v7.3.3 with host page faults in the lock, call and
idle paths) has 8,905,216 states. Each search is followed by the audit (README.md, "Audited"):
under the design no vCPU is ever unable to run again, in any configuration; under the other rule
books a search that has such states has between 532 and 32,928 of them, and every dead vCPU has a
flag of its own there or ahead. Every process exits 0.

## The table

Rule books across, configurations down. **C**: a completion cleared the kick (#2125). **S**: the
snapshot does not hold the kick (#2131). **W**: the restore's wake-everyone completion cleared the
restore kick (#2132). **M**: a write-back of `mp_state` cleared the kick (outside the design).
"none": no reachable state breaks any property, and the audit finds no vCPU that can never run
again. The first half is a theorem for the design (below the table); the second is the audit's
result on the searched configurations, and is not a theorem for any column.

Each letter stands for the hang (`NoLostKick` with interrupts off, at a queue head and at a queue
node), the missing waker (`HasWaker`), and the stuck cross-CPU call that follows (`NoStuckCall`).
A letter does **not** always include the milder case of a lock taken with interrupts on, where the
sleeper is woken by its next interrupt:

- **C** with interrupts on happens only through a synthetic halt (a fault in kernel mode, or any
  fault in a guest booted `no-kvmapf`). A completion whose "page not present" was injected raises
  the page-ready interrupt, which a HLT with interrupts on does not block on.
- **S** with interrupts on happens only in a guest booted `no-kvmapf`. With async page faults on,
  the restore's own completion raises that interrupt in the new VM.
- **W** never happens with interrupts on, for the same reason.

| Configuration | v6.14 | v7.3.3 | v6.14 + kick | v7.3.3 + kick | v7.3.3 + #2130 | **Design**: v7.3.3 + #2130 + kick |
|---|---|---|---|---|---|---|
| fcvm (2 vCPUs, PV spinlocks, async PF, userfaultfd, snapshots, devices) | S | C S | none | C W | S | **none** |
| the same, no snapshot taken | none | C | none | C | none | **none** |
| the same, guest booted `no-kvmapf` | S | C S | none | C | S | **none** |
| the same, with host page faults in the lock, call and idle paths too | S | C S | none | C W | S | **none** |
| the same, and NMIs arrive | S | C S | none | C W | S | **none** |
| with the idle timer, no host page faults | S | C S | none | W | S | **none** |
| with the idle timer and host page faults, no snapshot taken | none | C | none | C | none | **none** |
| with a nested guest (nested VM-exit) | S | C S | none | C W | S | **none** |
| with Xen `SCHEDOP_poll` | S | C S | none | C W | S | **none** |
| with CPU hotplug (INIT, SIPI, vCPU reset) | S | C S | none | C W | S | **none** |
| with SEV AP reset hold and SNP AP creation | S | C S | none | C W | S | **none** |
| outside the design: the VMM writes `mp_state` back on a paused VM | S | C M S | none | C M W | M S | M |

Every configuration has devices and a tick (`Cfg.rescue`), and, unless it says otherwise, host
page faults in ordinary code only. For the design's column, "no reachable state breaks any
property" is also a theorem for every row but the last (`design_noLostKick`, `design_hasWaker` and
the others in `Proofs.lean`), without the searches' bounds. "No vCPU is ever dead" is not: it is
what the audit found in these twelve searches of the design, and nothing more (README.md,
"Audited").

Under v7.3.3 as it is (second column) the search also reports the wake-everyone completion
clearing a kick. That is C, not W: there is no restore kick in that rule book. A VM restored twice
in a row can have its page-ready slot occupied at the second restore, so the completion waits on
the done list until the guest acknowledges, and a real kick sent meanwhile is cleared when it is
at last handled (`wakeAllToday`, 34 steps).

## What the first version reported and this one does not

The first version of the model (the pull request's first two commits) had no page-ready interrupt
and no page-ready slot. Its search reported twelve kinds in the fcvm configuration under v7.3.3;
four of them do not happen, and the same forms under the other rule books go with them:

| Withdrawn kind (all `NoLostKick` with interrupts on, at a queue head and at a queue node) | Rule books | Why it does not happen |
|---|---|---|
| a page-ready completion cleared the kick | v7.3.3, v7.3.3 + kick | the completion of an injected fault raises the page-ready interrupt, and a HLT with interrupts on returns for it |
| the snapshot does not hold the kick, guest with async page faults | v6.14, v7.3.3, v7.3.3 + #2130 | the restore's wake-everyone completion raises the same interrupt in the new VM |
| the restore's wake-everyone completion cleared the restore kick | v7.3.3 + kick | the same completion raises it |

Of the 17 runs the first version recorded, one is withdrawn for this reason: `pageReadyIrqsOn`.
`patchedNoKick` is the same run under the name `snapshotPatched`. `msrsFirst` is the same run in
a guest booted `no-kvmapf` (finding 6 says why it needs that). `synthHalt` is a shorter run, now
in the fcvm configuration. The other 13 are unchanged, and 28 were added, for 44.

Every hang of the first version is still found: each "interrupts off" kind and each `NoStuckCall`
kind it reported, under every rule book, is in this search's output. With interrupts off the
page-ready vector is masked, so the interrupt changes none of them. What the hangs gained is a
condition (the page-ready slot free in the snapshot, findings 2 and 3).

## Findings

Severity is for fcvm on x86_64: **P0** a guest hangs for ever in fcvm's configuration, **P1** the
same in a configuration fcvm supports but does not build the host kernel for, **P2** not reachable
under fcvm, a constraint on future changes.

| # | Finding | Variants | Severity | Reachable under fcvm | Runs in `Breaks.lean` | Issue |
|---|---|---|---|---|---|---|
| 1 | A completion clears a pending kick | v6.15 and later without #2130 | P0 | yes | `pageReady`, `synthHalt`, `wakeAllToday`, each with `Node`, `Call`, `NodeCall`; `synthHaltIrqsOn`, `synthHaltNodeIrqsOn`; the twelve `...NoApf`; `noWaker` | #2125 |
| 2 | A kick pending for a runnable vCPU is not in a snapshot | every kernel | P0 | yes, when the vCPU's page-ready slot is free in the snapshot | `snapshot`, `snapshotNode`, `snapshotCall`, `snapshotNodeCall`; the four `...614`; `snapshotPatched` | #2131 |
| 3 | The restore kick is cleared on the restored vCPU's first entry | v6.15 and later without #2130, with the #2131 kick | P0 on an unpatched host: the fix for 2 does nothing there for a vCPU whose slot is free. P1 for hosts fcvm does not build the kernel for | yes | `wakeAll`, `wakeAllNode`, `wakeAllCall`, `wakeAllNodeCall` | #2132: found by this search, then reproduced on hardware (#2135) |
| 4 | A VMM that reads `mp_state` and writes it back on a live VM clears a pending kick | v6.15 and later, with or without #2130 | P2 | no: Firecracker writes `mp_state` only into a new VM | `writeBack` | none (REPAIRS.md, 3) |
| 5 | A restore kick sent before `KVM_SET_MP_STATE` is cleared by it | v6.15 and later | P2 | no: #2135 sends it last, and its restore tests fail with the kick moved ahead | `kickFirst` | none (REPAIRS.md, 2) |
| 6 | `MSR_IA32_TSC_DEADLINE` restored before `KVM_SET_LAPIC` is dropped | every kernel | P2 | no: Firecracker restores it last | `msrsFirst` | none |
| 7 | Without `KVM_SET_LAPIC` a pending IPI is not restored | every kernel | P2 | no | `noLapic` | none |

Recorded in `Breaks.lean`: every kind the search reports under v7.3.3 as it is, in the fcvm
configuration and in its `no-kvmapf` variant; every kind under v6.14; the four W kinds; and one
run for each of the other findings. Kinds found in the other configurations (nested, Xen, hotplug,
SEV, the idle timer) are the same kinds reached in a larger state space and are not recorded
again.

### 1. A completion clears a pending kick (#2125)

`kvm_arch_async_page_present()` ends with `kvm_set_mp_state(vcpu, KVM_MP_STATE_RUNNABLE)` (v7.3.3
x86.c:10603-10604), and since v6.15 the helper clears `pv_unhalted` (x86.h:143-148). A completion
that is handled between a kick and the kicked vCPU's HLT erases the kick. `Run.pageReady`, 15
steps:

```
 1-3   vCPU0  spin_lock_irqsave: takes the lock, starts to unlock
 4     vCPU1  host page fault in user mode: "page not present" injected, fault in flight
 5-8   vCPU1  spin_lock_irqsave: queue head, xchg(&lock->locked, _Q_SLOW_VAL)
 9     vCPU0  try_cmpxchg_release fails: unlock slow path
10     vCPU1  kvm_wait: reads _Q_SLOW_VAL, the HLT is next (interrupts off)
11     vCPU0  smp_store_release(&lock->locked, 0)
12     vCPU0  pv_kick: vCPU1 pv_unhalted = 1
13     vCPU1  the async page fault worker finishes: done list, KVM_REQ_APF_READY
14     vCPU1  re-enters: kvm_check_async_pf_completion -> kvm_arch_async_page_present:
              pv_unhalted = 0; token written, page-ready interrupt raised (masked: IF = 0)
15     vCPU1  HLT: nothing it can take, mp_state = HALTED, interrupts off
```

- `pageReadyNode` (23 steps): the same at the other wait site, a queue node that `pv_kick_node`
  hashed while it was between its read of `pn->state` and its HLT.
- `pageReadyCall` (17 steps), `pageReadyNodeCall` (25): vCPU0 then starts a cross-CPU call to
  vCPU1 and waits in `csd_lock_wait` for ever. This is the soft lockup of #2125: one vCPU in
  `smp_call_function_many_cond`, the other halted in `kvm_wait`.
- `synthHalt` (17 steps): the fault is one KVM cannot inject (kernel mode), so KVM halts the vCPU
  itself (`apf.halted`). A device interrupt ends that halt early, its handler takes the lock with
  interrupts off while the fault is still in flight, and the completion clears the kick through
  the same function. In a guest booted `no-kvmapf` every fault is of this kind
  (`synthHaltNoApf`), which is why `no-kvmapf` did not avoid the hang in #2130's measurements.
- `synthHaltIrqsOn` (17 steps): the same with a lock taken with interrupts on. The completion of
  a synthetic halt raises no interrupt, so the HLT blocks and the vCPU sleeps until its next
  interrupt. Not a hang; a stall. This is the only "interrupts on" form of C. The first version of
  this model also reported one after an injected fault, which the code does not produce: the
  page-ready interrupt is pending and the HLT returns. The faithfulness review found that.
- `wakeAllToday` (34 steps): the completion is the restore's, in a VM restored twice (above).

v6.14 is free of it: `Today.v614` on `Run.pageReady` keeps the kick (`v614_survives_pageReady`),
the search finds nothing without snapshots, and `v614_sound_without_snapshots` proves it.

Real, and reachable in fcvm's configuration. Fix: the kernel patch, merged for the x86_64 host
kernel (#2130) and for the x86 guest kernels that run KVM themselves (#2146) (REPAIRS.md, 1). A
host kernel from v6.15 on that fcvm does not build still has it, which is why #2125 is open.

### 2. A kick pending for a runnable vCPU is not in a snapshot (#2131)

`KVM_GET_MP_STATE` folds the kick in only for a halted vCPU (v7.3.3 x86.c:9025-9030). For a vCPU
that is still runnable, between `kvm_wait`'s read and its HLT, no ioctl returns it. `Run.snapshot`,
16 steps: steps 1 to 11 as above without the page fault (vCPU1 is at its HLT with
`pv_unhalted = 1`), then

```
12     the VMM pauses the VM
13     snapshot, restore into a new VM (KVM_SET_MP_STATE, KVM_SET_LAPIC, KVM_SET_MSRS):
       vCPU1 pv_unhalted = 0
14     the VMM resumes the VM
15     vCPU1 enters the guest (the wake-everyone completion runs; there is no kick to clear)
16     vCPU1  HLT: mp_state = HALTED, interrupts off
```

The same run breaks v6.14 (`snapshot614`) and v7.3.3 with the #2130 patch (`snapshotPatched`): it
is independent of #2125.

**The condition.** In a guest with async page faults on, the hang needs the vCPU's page-ready slot
(`apf_reason.token`) to be zero in the snapshot's memory. If the guest had a page-ready
notification it had not yet acknowledged when the snapshot was taken, the restore's completion
cannot be dequeued (v7.3.3 x86.c:10618-10624), stays on the done list, and keeps
`kvm_vcpu_has_events()` true: the HLT does not block, the waiter reads the lock byte again and
takes the lock. So every restore of a snapshot with the kick pending and the slot free hangs the
same way; with the slot occupied, none does. In a guest booted `no-kvmapf` there is no completion
and no condition.

Real, and reachable in fcvm's configuration. Fix: the restore kick in the Firecracker fork, #2135,
merged; #2131 is closed (REPAIRS.md, 2).

### 3. The restore kick is cleared on the restored vCPU's first entry (#2132)

Found by the search before it was seen anywhere, in the rule book that has the #2131 fix and not
the #2130 patch, and since reproduced on hardware (below). Restoring the
guest's `MSR_KVM_ASYNC_PF_EN` makes `kvm_pv_enable_async_pf()` queue the wake-everyone completion
(v7.3.3 msrs.c:1384-1417, virt/kvm/async_pf.c:216-241); the function does not ask whether the
write came from the host. The restore kick is recorded after it. The vCPU's first entry handles
the completion through `kvm_arch_async_page_present()`, which clears the kick before the guest
runs one instruction. `Run.wakeAll`, 16 steps: steps 1 to 12 as in 2, then

```
13     snapshot, restore: KVM_SET_MP_STATE, KVM_SET_LAPIC, KVM_SET_MSRS (completion queued),
       the restore kick: vCPU1 pv_unhalted = 1
14     the VMM resumes the VM
15     vCPU1 enters the guest: kvm_check_async_pf_completion ->
       kvm_arch_async_page_present: pv_unhalted = 0
16     vCPU1  HLT: mp_state = HALTED, interrupts off
```

What holds beyond this one run (`Proofs.lean`):

- `restore_queues_completion`: in a guest with async page faults on, a restore in Firecracker's
  order leaves the completion on every vCPU's done list, and the slot as the snapshot had it.
- `v733_entry_clears_kick`: on v7.3.3 as released, a vCPU that enters the guest with a completion
  queued and its slot free has no kick afterwards, whatever it had.
- `v733Patched_entry_keeps_kick`: with the #2130 patch it keeps it.

So on an unpatched v6.15+ host the restore kick is cleared for every vCPU whose slot was free in
the snapshot, which is the case in which #2131 hangs (the condition in 2). For those vCPUs the
#2131 fix depends on the #2130 patch. A guest booted `no-kvmapf` queues no completion, and the
kick holds there (the `no-kvmapf` row has no **W**).

The run is not the shortest hang in that rule book: the 15-step run of #2125 breaks it too
(`breaks_pageReady_with_kick`). It is the shortest of its own kind.

It also means a test of the restore kick that does not set `MSR_KVM_ASYNC_PF_EN` on the vCPU
passes on an unpatched kernel without exercising this path.

Real. Reachable under fcvm on a host without the patch: the x64 runners before the patched image,
and any user's host at v6.15 or later.

**Reproduced on hardware** by the fix's own pull request, #2135. The Firecracker fork's test
`test_the_restore_kick_survives_the_page_ready_wakeup_of_the_restore` sets a vCPU up for async
page faults as Linux does, stops it before a HLT with interrupts off, kicks it, saves it and
restores it with the restore kick. #2135 reports, on one x86_64 bare-metal host with a
distribution's 7.0.0 KVM (which has the v6.15 change and not the patch): 12 of the module's 13
tests passed and this one failed on each of the three pinned fork commits, the restored vCPU
still in `KVM_RUN` at the 10 s bound (`StillInKvmRun { blocked: 9.9966636s, .. }`); with `kvm.ko`
and `kvm-intel.ko` rebuilt from the same source with the one change of the #2130 patch, 13
passed. That is steps 13 to 16 above. #2132 is closed for every kernel fcvm builds; a stock
kernel from v6.15 on still clears the restore kick.

Checked on the kernel's own functions by `make verify-harness`, step by step, from the call the
MSR write makes to the halt: `kvm_pv_enable_async_pf` (cut from v7.3.3 msrs.c) queues the
completion through `kvm_async_pf_wakeup_all`; the kick sets `pv_unhalted`;
`kvm_check_async_pf_completion` finds the slot free
(`kvm_arch_can_dequeue_async_page_present`) and runs `kvm_arch_async_page_present`, after which
`pv_unhalted` is clear; `__kvm_emulate_halt` then halts the vCPU and `vcpu_block` sleeps. With the
patch applied the same sequence leaves the vCPU running. Two links of the chain are read from the
source and not run in the harness: that Firecracker's `KVM_SET_MSRS` of `MSR_KVM_ASYNC_PF_EN`
reaches `kvm_pv_enable_async_pf` (the ioctl's dispatch is not cut) and does so before the kick,
and that `vcpu_enter_guest` handles `KVM_REQ_APF_READY` before the first guest instruction (v7.3.3
x86.c:8216-8217). Both are run on real KVM by #2135's test above. Fixed where the patch and the
kick are both present (REPAIRS.md, 2 and 4).

### 4. Writing `mp_state` back on a live VM clears a pending kick

On v6.15 and later `KVM_SET_MP_STATE` with RUNNABLE clears `pv_unhalted` (v7.3.3 x86.c:9072). A VMM
that synchronises vCPU state in place (reads `mp_state` of a paused VM and writes the same value
back) erases a kick pending for a runnable vCPU. `Run.writeBack`, 16 steps, is `Run.snapshot` with
the restore replaced by that write. It breaks the designed system too, which is why the design's
theorems carry the hypothesis that no such write happens (`Sound.noSet`): the proof does not close
without it.

Real in KVM, not reachable under fcvm: Firecracker calls `KVM_SET_MP_STATE` only on the vCPUs of a
new VM (arch/x86_64/vcpu.rs:713-715 at 2a33289b55b7, from `build_microvm_from_snapshot`). A
constraint for the fork (REPAIRS.md, 3).

### 5 to 7. Restore orders

Searched with orders no VMM here uses, to see which orders the design depends on:

- `kickFirst` (16 steps): the kick before `KVM_SET_MP_STATE` is cleared by it on v6.15 and later.
  On v6.14 the order does not matter (`Order.kickFirst614` is clean).
- `msrsFirst` (5 steps), in a guest booted `no-kvmapf`: vCPU0 programs its timer and halts in the
  idle loop; the restore writes `MSR_IA32_TSC_DEADLINE` before `KVM_SET_LAPIC`,
  `kvm_set_lapic_tscdeadline_msr` drops it because the timer is not in deadline mode yet (v7.3.3
  lapic.c:2749-2759), and the vCPU sleeps with no timer until some other interrupt arrives. It is
  a lost timer, not a vCPU that never runs again (the audit counts none dead). In a guest with
  async page faults on, the restore's completion wakes the vCPU once and the idle loop programs
  the timer again, so the model shows no sleep there at all. Firecracker's order is right, and
  its comment says why (arch/x86_64/vcpu.rs:703-704).
- `noLapic` (4 steps): a call-function interrupt pending in the IRR at the snapshot is lost if the
  LAPIC is not restored, and the caller waits for ever.

## What the search decided about the other paths

The review of #2130 listed the other places in v7.3.3 that clear `pv_unhalted` or write `mp_state`
and judged each by reading. They are transitions of the model, and the search decides them:

| Path | In the model | Result |
|---|---|---|
| Nested VM-exit (vmx/nested.c:5188) | `l2Enter`, `l2Hlt`, `l2Exit` | no new kind of break in any rule book; the design is proved with it on. A vCPU in a nested guest is not in `kvm_wait`, so the kick it clears is one nobody is owed |
| `KVM_SET_MP_STATE` | at restore (`clone`), and in place (`setMpSame`) | at restore: finding 5. In place: finding 4 |
| INIT, SIPI, vCPU reset (lapic.c:3617-3633, x86.c:9554) | `offline`, `init`, `sipi` | no new kind; proved for the design. INIT only at an offline CPU, which is what Linux does and what the proof needs |
| SEV AP reset hold, SNP AP creation (svm/sev.c:4129-4146) | `sevHold`, `snpCreate` | no new kind; proved for the design |
| Xen `SCHEDOP_poll` (xen.c:1581-1592) | `xenPoll`, `xenWake`, and the poll's return on any wake or pause | no new kind; proved for the design |
| `__kvm_emulate_halt` (x86.c:8693-8712) | every HLT | consumes a kick only when it leaves the vCPU running: `KeepsKick.halt_kick`, true of all three kernels |
| `vcpu_block` after a synthetic halt (x86.c:8625-8638) | `hostRun`, `nmi`, `extIrq` | the RUNNABLE arm leaves the kick alone: `KeepsKick.unhalt_runnable`. The kick is then cleared by the completion (finding 1, `synthHalt`), not here |

## The other questions

- **The source VM across pause, snapshot, resume.** Nothing is lost for it. In the model a
  snapshot reads the source and changes nothing but where the vCPU threads are, which is what
  Firecracker does: `KVM_GET_MP_STATE` and the other reads, no write (arch/x86_64/vcpu.rs:591-650).
  The `pause` then `resume` runs of every rule book are in every search; v6.14 and the design are
  clean with them, and the design is proved. The audit does not accept a restore as a rescue, so
  a source VM that hung would be counted.
- **A restore whose first step is `KVM_SET_MP_STATE` to RUNNABLE on v6.15 and later.** A vCPU that
  was halted with a kick pending is reported RUNNABLE and restored RUNNABLE. It does not execute
  its HLT again: `kvm_emulate_halt` moves RIP past the HLT before the vCPU halts (v7.3.3
  x86.c:8720-8728), so the restored vCPU continues after it, which is the wake-up the kick was. The
  helper's clearing of `pv_unhalted` on the new vCPU clears nothing. In the model this is
  `restoreVcpu` on a vCPU at `hdWoke` or `ndWoke`; the invariant's `afterHlt` line is the reason
  the proof goes through.
- **An interrupt or timer pending at the snapshot for a halted vCPU.** Kept. The IRR travels in
  `KVM_GET_LAPIC` and `kvm_vcpu_has_events` reads it when the restored vCPU first runs, whatever
  order `mp_state` and the LAPIC were set in (`NoStuckCall`'s second half, proved). The timer needs
  `KVM_SET_LAPIC` before its MSR (finding 6), which is Firecracker's order (`NoLostTimer`, proved).
  A timer that had expired and was not yet injected keeps its deadline in the MSR (it is zeroed at
  injection, v7.3.3 lapic.c:2040-2051), so it expires again in the new VM.
- **arm64.** None of these mechanisms exist there: README.md, "arm64".

## What each theorem rests on

`Axioms.lean` sweeps everything declared in the library's modules and fails on any axiom beyond
`propext`, `Quot.sound` and `Classical.choice`. `lake build` elaborates that file only when
something it imports changed; `check-sweep.sh` elaborates it on every run of `make verify` and of
the CI job, and `check-imports.sh` fails when a file of the library is not among its imports. It
also prints, and pins, the axioms of the theorems about the design:

```
design_noLostKick, design_noHang, design_hasWaker, design_noMutualSleep, design_noStuckCall,
design_mutualExclusion, design_noOrphanHalt, design_noLostTimer, design_noStrandedNode,
reachable_inv:                                                    [propext, Quot.sound]
design_sound, v614Kick_sound, v614_sound_without_snapshots,
v733Patched_sound_without_snapshots, v733_not_keepsKick, hlt_rechecks, unlock_slow_kicks,
restore_queues_completion, v733_entry_clears_kick, v733Patched_entry_keeps_kick,
allLabels_complete, replay_reachable, design_survives, breaks_*:  [propext]
```
