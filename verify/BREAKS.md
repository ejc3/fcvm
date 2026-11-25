# Breaks: where a vCPU's wake-up is lost, per kernel and VMM

`lake exe breaks` searches every rule book and configuration breadth-first from `init`, visits
every reachable state, and tests every property of `WakeVerify/Properties.lean` on it. For each
kind of break it prints the shortest run that reaches it, step by step with the source each step
models. A kind is the property, the wait site (queue head or queue node), and what happened to the
kick. Every run below is copied from that output into `WakeVerify/Breaks.lean`, where Lean's kernel
replays it and evaluates the property (`by decide`) on every `lake build`.

The model was first checked against what was already known: it returns #2125 as the shortest run
to a hang on v7.3.3 (15 steps), it returns #2131 on every kernel (16 steps), and on v6.14 without
snapshots it finds nothing, which `v614_sound_without_snapshots` then proves.

## The run

| Command | Searches | States visited | Wall time | Peak memory |
|---|---|---|---|---|
| `lake exe breaks` | 70 (6 rule books x 11 configurations, 4 restore orders) | 150,736,883 | 9 min 43 s | 1.8 GB |

No search hit its state limit. The largest (v7.3.3 with the idle timer) has 10,603,548 states.

## The table

Rule books across, configurations down. **C**: a completion cleared the kick (#2125). **S**: the
snapshot does not hold the kick (#2131). **W**: the restore's wake-everyone completion cleared the
restore kick (#2132, new). **M**: a write-back of `mp_state` cleared the kick (outside the design).
Every letter is a `NoLostKick` break with interrupts off (the hang) and with them on, at both wait
sites, and the `NoStuckCall` break that follows from it. "none": no reachable state breaks any
property.

| Configuration | v6.14 | v7.3.3 | v6.14 + kick | v7.3.3 + kick | v7.3.3 + #2130 | **Design**: v7.3.3 + #2130 + kick |
|---|---|---|---|---|---|---|
| fcvm (2 vCPUs, PV spinlocks, async PF, userfaultfd, snapshots) | S | C S | none | C W | S | **none** |
| the same, no snapshot taken | none | C | none | C | none | **none** |
| the same, guest booted `no-kvmapf` | S | C S | none | C | S | **none** |
| the same, interrupts from devices and the tick arrive | S | C S | none | C W | S | **none** |
| the same, and NMIs arrive | S | C S | none | C W | S | **none** |
| with the idle timer | S | C S | none | C W | S | **none** |
| with a nested guest (nested VM-exit) | S | C S | none | C W | S | **none** |
| with Xen `SCHEDOP_poll` | S | C S | none | C W | S | **none** |
| with CPU hotplug (INIT, SIPI, vCPU reset) | S | C S | none | C W | S | **none** |
| with SEV AP reset hold and SNP AP creation | S | C S | none | C W | S | **none** |
| outside the design: the VMM writes `mp_state` back on a paused VM | S | C M S | none | C M W | M S | M |

The design's column is also a theorem for every row but the last (`design_noLostKick` and the
others in `Proofs.lean`), for runs of any length.

## Findings

Severity is for fcvm on x86_64: **P0** a guest hangs for ever in fcvm's configuration, **P1** the
same in a configuration fcvm supports but does not build the host kernel for, **P2** not reachable
under fcvm, a constraint on future changes.

| # | Finding | Variants | Severity | Reachable under fcvm | Runs in `Breaks.lean` | Issue |
|---|---|---|---|---|---|---|
| 1 | A page-ready completion clears a pending kick | v6.15 and later without #2130 | P0 | yes | `pageReady`, `pageReadyNode`, `pageReadyCall`, `pageReadyIrqsOn`, `synthHalt` | #2125 |
| 2 | A kick pending for a runnable vCPU is not in a snapshot | every kernel | P0 | yes | `snapshot`, `snapshotNode`, `snapshotCall`, `snapshot614`, `patchedNoKick` | #2131 |
| 3 | The restore kick is cleared on the restored vCPU's first entry | v6.15 and later without #2130, with the #2131 kick | P0 on an unpatched host: the fix for 2 does nothing there. P1 for hosts fcvm does not build the kernel for | yes | `wakeAll`, `wakeAllNode`, `wakeAllCall` | **#2132 (new)** |
| 4 | A VMM that reads `mp_state` and writes it back on a live VM clears a pending kick | v6.15 and later, with or without #2130 | P2 | no: Firecracker writes `mp_state` only into a new VM | `writeBack` | none (REPAIRS.md, 3) |
| 5 | A restore kick sent before `KVM_SET_MP_STATE` is cleared by it | v6.15 and later | P2 | no: the #2131 change sends it last | `kickFirst` | none (REPAIRS.md, 2) |
| 6 | `MSR_IA32_TSC_DEADLINE` restored before `KVM_SET_LAPIC` is dropped | every kernel | P2 | no: Firecracker restores it last | `msrsFirst` | none |
| 7 | Without `KVM_SET_LAPIC` a pending IPI is not restored | every kernel | P2 | no | `noLapic` | none |

### 1. A page-ready completion clears a pending kick (#2125)

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
              pv_unhalted = 0
15     vCPU1  HLT: nothing pending, mp_state = HALTED, interrupts off
```

- `pageReadyNode` (23 steps): the same at the other wait site, a queue node that `pv_kick_node`
  hashed while it was between its read of `pn->state` and its HLT.
- `pageReadyCall` (17 steps): vCPU0 then starts a cross-CPU call to vCPU1 and waits in
  `csd_lock_wait` for ever. This is the soft lockup of #2125: one vCPU in
  `smp_call_function_many_cond`, the other halted in `kvm_wait`.
- `pageReadyIrqsOn` (15 steps): with a lock taken with interrupts on, the sleeper is woken by its
  next interrupt. Not a hang; a stall until the next tick or IPI.
- `synthHalt` (21 steps), guest booted `no-kvmapf`: KVM halts the faulting vCPU itself
  (`apf.halted`), an IPI ends that halt early, the vCPU then takes the lock path with interrupts
  off while its fault is still in flight, and the completion clears the kick through the same
  function. This is why `no-kvmapf` did not avoid the hang in #2130's measurements.

v6.14 is free of it: `Today.v614` on `Run.pageReady` keeps the kick (`v614_survives_pageReady`),
the search finds nothing without snapshots, and `v614_sound_without_snapshots` proves it.

Real, and reachable in fcvm's configuration. Fix: host kernel, #2130, merged (REPAIRS.md, 1).

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

The same run breaks v6.14 (`snapshot614`) and v7.3.3 with the #2130 patch (`patchedNoKick`): it is
independent of #2125. Every restore of that snapshot hangs the same way.

Real, and reachable in fcvm's configuration. Fix: Firecracker fork, the restore kick (REPAIRS.md,
2).

### 3. The restore kick is cleared on the restored vCPU's first entry (#2132, new)

Found by the search, in the rule book that has the #2131 fix and not the #2130 patch. Restoring the
guest's `MSR_KVM_ASYNC_PF_EN` makes `kvm_pv_enable_async_pf()` queue the wake-everyone completion
(v7.3.3 msrs.c:1384-1417, virt/kvm/async_pf.c:216-241). The restore kick is recorded after it. The
vCPU's first entry handles the completion through `kvm_arch_async_page_present()`, which clears the
kick before the guest runs one instruction. `Run.wakeAll`, 16 steps: steps 1 to 12 as in 2, then

```
13     snapshot, restore: KVM_SET_MP_STATE, KVM_SET_LAPIC, KVM_SET_MSRS (completion queued),
       the restore kick: vCPU1 pv_unhalted = 1
14     the VMM resumes the VM
15     vCPU1 enters the guest: kvm_check_async_pf_completion ->
       kvm_arch_async_page_present: pv_unhalted = 0
16     vCPU1  HLT: mp_state = HALTED, interrupts off
```

So the #2131 fix depends on the #2130 patch on every kernel from v6.15: without the patch it
changes nothing for a guest that has async page faults enabled (the `no-kvmapf` row has no **W**:
no completion is queued and the kick holds). It also means a test of the restore kick that does not
set `MSR_KVM_ASYNC_PF_EN` on the vCPU passes on an unpatched kernel without exercising this path.

Real. Reachable under fcvm on a host without the patch: the x64 runners before the patched image,
and any user's host at v6.15 or later. Not yet reproduced on hardware; the two kernel functions
involved, cut from v7.3.3, behave as the model says (`make verify-harness`). Fix: land the patch
and the kick together, and see REPAIRS.md, 2 and 4.

### 4. Writing `mp_state` back on a live VM clears a pending kick

On v6.15 and later `KVM_SET_MP_STATE` with RUNNABLE clears `pv_unhalted` (v7.3.3 x86.c:9072). A VMM
that synchronises vCPU state in place (reads `mp_state` of a paused VM and writes the same value
back) erases a kick pending for a runnable vCPU. `Run.writeBack`, 16 steps, is `Run.snapshot` with
the restore replaced by that write. It breaks the designed system too, which is why the design's
theorems carry the hypothesis that no such write happens (`Sound.noSet`): the proof does not close
without it.

Real in KVM, not reachable under fcvm: Firecracker calls `KVM_SET_MP_STATE` only on the vCPUs of a
new VM (arch/x86_64/vcpu.rs:709-711, from `build_microvm_from_snapshot`). A constraint for the fork
(REPAIRS.md, 3).

### 5 to 7. Restore orders

Searched with orders no VMM here uses, to see which orders the design depends on:

- `kickFirst` (16 steps): the kick before `KVM_SET_MP_STATE` is cleared by it on v6.15 and later.
  On v6.14 the order does not matter (`Order.kickFirst614` is clean).
- `msrsFirst` (5 steps): vCPU0 programs its timer and halts in the idle loop; the restore writes
  `MSR_IA32_TSC_DEADLINE` before `KVM_SET_LAPIC`, `kvm_set_lapic_tscdeadline_msr` drops it because
  the timer is not in deadline mode yet (v7.3.3 lapic.c:2749-2759), and the vCPU sleeps for a timer
  it does not have. Firecracker's order is right, and its comment says why
  (arch/x86_64/vcpu.rs:703-704).
- `noLapic` (4 steps): a call-function interrupt pending in the IRR at the snapshot is lost if the
  LAPIC is not restored.

## What the search decided about the other paths

The review of #2130 listed the other places in v7.3.3 that clear `pv_unhalted` or write `mp_state`
and judged each by reading. They are transitions of the model, and the search decides them:

| Path | In the model | Result |
|---|---|---|
| Nested VM-exit (vmx/nested.c:5188) | `l2Enter`, `l2Hlt`, `l2Exit` | no new kind of break in any rule book; the design is proved with it on. A vCPU in a nested guest is not in `kvm_wait`, so the kick it clears is one nobody is owed |
| `KVM_SET_MP_STATE` | at restore (`clone`), and in place (`setMpSame`) | at restore: finding 5. In place: finding 4 |
| INIT, SIPI, vCPU reset (lapic.c:3617-3633, x86.c:9554) | `offline`, `init`, `sipi` | no new kind; proved for the design. Linux sends INIT to an offline CPU only |
| SEV AP reset hold, SNP AP creation (svm/sev.c:4129-4146) | `sevHold`, `snpCreate` | no new kind; proved for the design |
| Xen `SCHEDOP_poll` (xen.c:1581-1592) | `xenPoll`, `xenWake` | no new kind; proved for the design |
| `__kvm_emulate_halt` (x86.c:8693-8712) | every HLT | consumes a kick only when it leaves the vCPU running: `KeepsKick.halt_kick`, true of all three kernels |
| `vcpu_block` after a synthetic halt (x86.c:8625-8638) | `hostRun`, `nmi`, `extIrq` | the RUNNABLE arm leaves the kick alone: `KeepsKick.unhalt_runnable`. The kick is then cleared by the completion (finding 1, `synthHalt`), not here |

## The other questions

- **The source VM across pause, snapshot, resume.** Nothing is lost for it. In the model a
  snapshot reads the source and changes nothing but where the vCPU threads are, which is what
  Firecracker does: `KVM_GET_MP_STATE` and the other reads, no write (arch/x86_64/vcpu.rs:591-650).
  The `pause` then `resume` runs of every rule book are in every search; v6.14 and the design are
  clean with them, and the design is proved.
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

`Axioms.lean` prints, and checks on every build:

```
design_noLostKick, design_noHang, design_noStuckCall, design_mutualExclusion,
design_noOrphanHalt, design_noLostTimer, design_noStrandedNode:   [propext, Quot.sound]
design_sound, v614Kick_sound, v614_sound_without_snapshots,
v733Patched_sound_without_snapshots, v733_not_keepsKick, hlt_rechecks,
design_survives, breaks_*:                                        [propext]
```
