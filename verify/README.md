# verify: how a guest vCPU's wake-up is lost, in Lean

A small Lean 4 model of the path a paravirtual spinlock kick takes from one guest vCPU to another
on x86_64, through the guest kernel, host KVM and the VMM, with rule books for the code as it was
before the repairs (`Today.lean`, every rule citing file, line and release) and for the repaired
system (`Design.lean`), which is what fcvm builds and pins now. It follows the shape of AWS's
Cedar models
([How We Built Cedar](https://arxiv.org/abs/2407.01688)): plain data, one function per step,
properties as decidable propositions, an exhaustive search for counterexamples against what
exists, and proofs for what is designed.

It was written after two hangs of restored clones (#2125, #2131), to find every way the same
wake-up can be lost, not only those two.

- `BREAKS.md`: every break the search found, per kernel and VMM variant, with its trace, what it
  is in the real system, and a severity.
- `REPAIRS.md`: what each break needs changed, where, and the wording proposed.

The model was read against its sources by an independent reviewer, and its proofs and properties
by another. What each found and what changed is in "The reviews" at the end.

## Where the repairs are

As of 2026-11-25 both repairs of `Design.lean` are merged, and the model's third finding has been
seen on hardware.

| Repair | Change | State |
|---|---|---|
| 1 | #2130: the x86_64 host kernel keeps a pending kick across a page-ready completion (`kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch`) | merged; the x64 runners boot it (kernel release `7.3.3-fcvm-2a778d7a8b2d`) since 10:33 UTC on 2026-11-25 |
| 1 | #2146: the same patch in the x86 guest kernels that run KVM themselves (nested, phone, the mountinfo pair) | merged |
| 2 | #2135: after a snapshot load the Firecracker fork kicks each vCPU saved RUNNABLE in a guest that was offered `KVM_FEATURE_PV_UNHALT`; `rootfs-config.toml` pins that fork (2a33289b55b7 for the default profile) | merged |
| 3 (`REPAIRS.md`) | the VMM never writes `mp_state` to a live VM | the comment at the fork's one call is in #2135; the assertion and the test are not written |

Issues: #2131 and #2132 are closed. #2125 is open: Linux from v6.15 on still clears the kick
(the patch is not upstream as of v7.3.3), so a host whose kernel fcvm does not build has it.

Which rule book a host runs under the fork fcvm pins now:

| Host kernel | Rule book | Breaks (`BREAKS.md`) |
|---|---|---|
| fcvm's x86_64 host kernel (v7.3.3 with the patch) | `Design.rules` | none: proved |
| v6.15 or later without the patch, a distribution's included | `Partial.v733Kick` | 1 (#2125) and 3 (#2132) |
| v6.14 or earlier | `Partial.v614Kick` | none: proved (`v614Kick_sound`) |

`Today.v614` and `Today.v733` are the same kernels under the fork before #2135.

In CI since: all four x64 runners moved to the image with the host patch at 10:33 UTC on
2026-11-25. Of the x64 VM lanes started after that, ten had finished by 15:11 UTC, each with no
`soft lockup` line (among them #2135's own lane and #2146's), where three lanes showed the hang
on the image before it earlier the same day (#2125 has two of them, #2135 the third). Read again
for this file: 14 x64 root lanes started between 11:08 and 15:55 UTC name that kernel in their
`pidstat.log` header, and none has `soft lockup` in its job log or its test-log artifact.

## Build

```sh
make verify           # the build and every check the CI job runs (below)
make verify-search    # all 76 searches against WakeVerify/search.expected (JOBS=7: side by side)
make verify-harness   # the model's kernel rules against the kernel's own functions (needs cc, network)
```

`make verify` runs, in this order, what `.github/workflows/verify.yml` runs:

| Step | What it checks | Cost |
|---|---|---|
| `check-imports.sh` | every file of the library is imported by `Axioms.lean`; `lakefile.toml` builds the library whole (`defaultTargets`, `globs`); the executables' sources declare no theorem or axiom | under 1 s |
| `lake build` | every proof and every recorded break, in Lean's kernel | 3 min 37 s from cold, 1.5 GB |
| `check-sweep.sh` | the axiom sweep, elaborated again whatever the build directory holds; `warningAsError` in `lakefile.toml` | 1 s |
| `test-search.sh` | the search program's own gates: 15 cases | 24 s, 0.7 GB |
| `harness/test-run.sh` | the harness against the kernel's functions, and the harness script's own gates: 8 cases | 15 s after the first fetch |
| `search-expected.sh quick` | the 19 small searches against `WakeVerify/search.expected` | about 2 min, 1.2 GB |

The toolchain is pinned in `lean-toolchain` (`leanprover/lean4:v4.35.0`; elan installs it on first
use). Lean's core only: no Mathlib, no other package. The costs are from a shared arm64 dev box
(Neoverse) with the toolchain installed, one run each: they say what to expect, and are not
benchmarks. All of `make verify` on a built directory took 2 min 48 s and 1.6 GB. `make verify-search` is 76 searches and 449.8 million states: about 45 min of CPU and
3.3 GB in one process, or 7 min 53 s with `JOBS=7` (one process a rule book, 3.3 GB each).

## The files

| File | What it holds |
|---|---|
| `WakeVerify/Spec.lean` | `State` (two vCPUs, one lock, the VMM's view), the guest paths as one step per instruction (`advance`, `guest`), the host's steps (`hostRun`, `host`), the VMM's (`pause`, `resume`, `clone`), `step`, `replay`, `Reachable`. `Kernel` and `Rules` are the rule books' type; `Cfg` is what exists around the guest |
| `WakeVerify/Today.lean` | `V614.kernel`, `V733.kernel`: what each release does at every place that writes `mp_state` or the kick. `firecracker`: the fork's restore order before #2135. `Today.v614`, `Today.v733` |
| `WakeVerify/Design.lean` | `V733Patched.kernel` (#2130), `firecrackerWithKick` (#2135), `Design.rules`; the partial combinations and the restore orders the search also runs |
| `WakeVerify/Properties.lean` | The properties, decidable on a state |
| `WakeVerify/Breaks.lean` | Runs the search found, replayed and checked by the kernel (`by decide`) |
| `WakeVerify/Lemmas.lean` | `KeepsKick` (what the proofs need of a kernel), the invariant `PInv`, and that every step keeps it |
| `WakeVerify/Proofs.lean` | `Sound`, `reachable_inv`, the `sound_*` and `design_*` theorems, which kernels are sound, and the smaller facts the documents cite |
| `WakeVerify/Axioms.lean` | Fails the build if anything declared in a module of the library rests on more than `propext`, `Quot.sound`, `Classical.choice`, or if a module declares an axiom (a sweep by module, so a private theorem or one outside the namespace is looked at too), and pins the axioms of the cited theorems |
| `Search.lean` | `lake exe breaks`: breadth-first search, kinds of break, shortest runs, the audit |
| `WakeVerify/search.expected`, `search-expected.sh` | what every search prints (states, kinds with their counts and shortest runs, the audit's numbers), and its comparison with a run |
| `HostOps.lean`, `harness/` | `lake exe hostops` and the C harness: the kernel's own functions against the rule books. `harness/operations.expected` pins how many operations each kernel is compared on |
| `check-imports.sh`, `check-sweep.sh` | what `lake build` cannot check about itself |
| `test-search.sh`, `harness/test-run.sh` | tests of the search program's gates and of the harness script's |

## The properties

| | Property | v6.14 | v7.3.3 | Design |
|---|---|---|---|---|
| P1 | **NoLostKick**: no vCPU sleeps in `kvm_wait`, the lock byte it waits on released, with nothing pending that can wake it. `NoHang` is the case with interrupts off, where only an NMI helps | broken (#2131) | broken (#2125, #2131) | proved |
| P1 | **HasWaker**: every vCPU asleep in `kvm_wait` has a kick recorded or about to be sent, or the other vCPU ahead of it in the lock's path: the owner, or the queue head that hands over to it. This one does not wait for the lock byte to be released | broken | broken | proved |
| P1 | **NoMutualSleep**: the two vCPUs are never both asleep in `kvm_wait` with no kick recorded for either | holds | holds | proved |
| P2 | **NoStuckCall**: no vCPU waits in a cross-CPU call for a vCPU asleep in `kvm_wait` with interrupts off and no kick recorded, and no queued call is without its interrupt | broken | broken | proved |
| P3 | **MutualExclusion**: whatever wakes a vCPU (a kick, a completion, an NMI, an interrupt meant for something else), it never enters the critical section beside the owner. With `hlt_rechecks`: the step after every HLT in the modelled paths goes back to the read | holds | holds | proved |
| | **NoOrphanHalt**: a synthetic halt has its completion coming | holds | holds | proved |
| | **NoLostTimer**: a vCPU that halts for its timer still has one after a restore, or something else pending | holds | holds | proved |
| | **NoStrandedNode**: the guest's queue protocol never leaves a node asleep that was handed the queue head | holds | holds | proved |

"holds" is the exhaustive search finding no reachable state that breaks it. "proved" is a theorem
(`Proofs.lean`). The six kernel and VMM combinations are in `BREAKS.md`.

## What is proved, what is audited, what is checked, what is assumed

### Proved

By Lean's kernel, with no `sorry`, on the axioms `propext` and `Quot.sound` only.

**What the theorems say.** Each `design_*` theorem has the form: for every configuration `C` with
`C.userSetMp = false` and every state `s` with `Reachable Design.rules C s`, the state `s` does
not have a certain shape. There are nine, one for each property above:

| Theorem | The state it shows unreachable |
|---|---|
| `design_noLostKick` | a vCPU halted after the HLT of `kvm_wait`, the lock byte it waits on no longer `_Q_SLOW_VAL` (a queue node: hashed as well), no kick recorded, the other vCPU not at its kick, no completion in flight or done, and no interrupt or timer it can take |
| `design_noHang` | the same with interrupts off (a corollary) |
| `design_hasWaker` | a vCPU halted after that HLT with no kick recorded, where the other vCPU is not at its kick, does not own the lock, and (for a node not yet hashed) is not the queue head. The lock byte may still be `_Q_SLOW_VAL` |
| `design_noMutualSleep` | both vCPUs halted after that HLT with no kick recorded for either (a corollary of `design_hasWaker`) |
| `design_noStuckCall` | a vCPU in `csd_lock_wait` whose target is halted after that HLT with interrupts off and no kick recorded; and a queued call whose interrupt is not pending |
| `design_mutualExclusion` | both vCPUs own the lock |
| `design_noOrphanHalt` | a synthetic halt with no completion in flight or done |
| `design_noLostTimer` | a vCPU halted in the idle loop for a timer that is off, with no kick, completion or interrupt pending |
| `design_noStrandedNode` | a queue node halted after it was handed the queue head and `pv_kick_node` has run, unhashed, with nothing pending |

The configurations are every value of `Cfg` with that one field false: a nested guest, Xen's poll,
CPU hotplug and the SEV paths on or off, host page faults anywhere interrupts are on, the idle
timer, NMIs. A restored VM can be snapshotted and restored again, any number of times. The proof
is an inductive invariant (`PInv` in `Lemmas.lean`, `reachable_inv`), so it has no bound on the
length of a run. The bounds that remain are the model's: two vCPUs, one lock.

**What they do not say.** They are statements about single states. No theorem here says "no vCPU
sleeps for ever", and none says that a vCPU which has a waker is in the end woken. That is a
statement about every future of a run (the other vCPU does take its steps, the host does run its
thread), and it is **not proved**. Two proved facts come close: while a sleeper's lock byte is
still `_Q_SLOW_VAL`, the other vCPU owns the lock (`design_hasWaker`, from the invariant's
`lockHeld` line), and an owner's unlock of a `_Q_SLOW_VAL` byte goes through the slow path, whose
last step is the kick (`unlock_slow_kicks`). That no vCPU of the design ends up unable to run is
what the audit below checks, on the finite model.

Also proved:

- The same nine for any rule book and configuration that is `Sound` (`sound_*`): a kernel that
  keeps a recorded kick (`KeepsKick`), a restore in Firecracker's order that ends with the kick or
  no snapshot at all, and no write of `mp_state` to a live VM. `v614Kick_sound` (v6.14 with the
  restore kick), `v614_sound_without_snapshots` (v6.14 has no #2125) and
  `v733Patched_sound_without_snapshots` are instances.
- `v733_not_keepsKick`: v7.3.3 as released fails the one line of `KeepsKick` that the #2130 patch
  restores.
- `restore_queues_completion`, `v733_entry_clears_kick`, `v733Patched_entry_keeps_kick`: #2132 as
  a statement about every restore, not one run (BREAKS.md, 3).
- `hlt_rechecks`: the step after every HLT of the modelled guest paths goes back to the read.
- `allActs_complete`, `allLabels_complete`: the list of labels the search iterates over has every
  label. `allLabels_fit_edge_encoding`: there are at most 64, which the search's graph encoding
  needs. `replay_reachable`: the end of a recorded run is a `Reachable` state.
- Every run in `Breaks.lean`: possible, and breaking its property, by `decide`. The design on
  seven of those 44 runs (`design_survives`): possible, and fine. `design_reaches_a_due_sleeper`: the design
  does reach a state where a vCPU sleeps with its lock byte released, so the theorems are not
  empty of cases.

**Not proved, searched only** (the search is exhaustive on the finite model, its program is
compiled Lean and is not verified): every "holds" in the properties table for v6.14 and v7.3.3;
that the rule books other than the design have no kind of break beyond those in BREAKS.md; that
each recorded run is a shortest one of its kind; and the restore orders of BREAKS.md, 5 to 7.

**The hypothesis every `design_*` theorem carries: the VMM never writes `mp_state` to a live VM.**
`KVM_SET_MP_STATE` with RUNNABLE clears a pending kick on v6.15 and later, with or without the
#2130 patch, so a VMM that reads `mp_state` of a VM that has run and writes it back can erase a
wake-up (`breaks_writeBack`, on the designed system). The proof would not close without assuming
this never happens (`Sound.noSet`, `C.userSetMp = false`). It is true of the pinned Firecracker
fork, which issues `KVM_SET_MP_STATE` once, in `KvmVcpu::restore_state`
(src/vmm/src/arch/x86_64/vcpu.rs:713-715 at 2a33289b55b7), called only from
`KvmVm::restore_kvm_state` on the new vCPUs of `build_microvm_from_snapshot`
(arch/x86_64/vm.rs:264-266, builder.rs:499). Since #2135 a comment at the call says so. Nothing
in the fork enforces it: `REPAIRS.md`, 3 is the repair that would.

**The theorems are about a VM whose whole history ran under the rule book.** A VM that hung on an
unpatched host and is then snapshotted and restored on a repaired one starts from a state
`Reachable` does not contain, and stays hung.

### Audited (exhaustive on the finite model, not proved)

`Search.lean`, `audit`. For every rule book and configuration, without using the properties, the
search computes the states in which some vCPU can never execute another instruction: however the
other vCPU, the host and the VMM go on, short of a snapshot restore or an NMI, its position never
changes in a step of a vCPU. (A restore is excluded because a new VM is not a rescue of the old
one; with it included, a hung source VM counted as alive. The VMM's pause does not count as
progress.) Then:

- under the design there must be no such state, in any configuration;
- under every rule book, each such vCPU must be flagged by a property of its own in that state
  (its own lost kick, missing waker, stuck call, orphan halt, lost timer or stranded node), or
  have that flag ahead of it. "Ahead" means a state in which the vCPU is flagged can be reached
  from this one over the audit's steps; it does not mean every continuation reaches one. In the
  searches it is one case: a vCPU waiting in a cross-CPU call for a vCPU that has lost its kick
  and has not executed its HLT yet.

A dead vCPU with neither, or any dead vCPU under the design, fails the run (exit status 1), and so
does a search that stops at its state limit, which is not audited. In the run recorded in
BREAKS.md the rule books other than the design had between 532 and 32,928 such states in each
search that had any, every dead vCPU explained, and the design had none. Every configuration lets
interrupts from devices and the tick arrive (`Cfg.rescue`), so that a vCPU asleep with interrupts
on is not counted dead.

Also checked by the search and not proved: that today's rule books have no kind of break other
than those listed, and that each recorded run is a shortest one of its kind. The search program is
compiled Lean and is not itself verified.

**Bounds of the searches** (the theorems have none of these):

| Bound | Where |
|---|---|
| Host page faults in ordinary code only (`kernelFaults := false`) | every configuration but "with host page faults in the lock, call and idle paths" |
| The idle timer without host page faults, or with them and without snapshots | the two idle-timer configurations. With all three the space is about 40 million states (some 20 GB), and was not searched |
| Locks taken with interrupts off only (`plainLocks := false`) | the two idle-timer configurations |

Snapshots and restores are not bounded: a restored VM can be snapshotted and restored again.

### Checked against the kernel's own code

`harness/`, `make verify-harness`: C that compiles functions cut verbatim from kernel source
(fetched from the `gregkh/linux` tags and checksum-pinned in `harness/SHA256SUMS`, not kept here)
with stubs for what they call (`harness/stubs.h`), drives them with the operations `lake exe
hostops` prints, and compares each resulting state with what the rule book computes.

**Which kernels.** Three builds of host KVM: v6.14, v7.3.3, and v7.3.3 with the repository's own
`kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch`
applied. Each is compared with its rule book (`V614.kernel`, `V733.kernel`,
`V733Patched.kernel`). No other release is run: not the releases between v6.14 and v7.3.3, not a
distribution's kernel, and not the guest kernels that carry the same patch since #2146 (6.18.60,
6.16.1).

**Which functions**, 21 in v7.3.3 and 19 in v6.14 (which has no `kvm_set_mp_state` and one
function for the async page fault enable test):

| File | Functions cut |
|---|---|
| arch/x86/kvm/x86.c | `kvm_vcpu_running`, `kvm_vcpu_has_events`, `kvm_arch_vcpu_runnable`, `vcpu_block`, `__kvm_emulate_halt`, `kvm_arch_vcpu_ioctl_get_mpstate`, `kvm_arch_vcpu_ioctl_set_mpstate`, `apf_put_user_ready`, `apf_pageready_slot_free`, `kvm_arch_async_page_present`, `kvm_arch_async_page_present_queued`, `kvm_arch_can_dequeue_async_page_present` |
| arch/x86/kvm/x86.h (v7.3.3) | `kvm_set_mp_state`, `__kvm_pv_async_pf_enabled`, `kvm_pv_async_pf_enabled` (in x86.c in v6.14) |
| arch/x86/kvm/lapic.c | `__apic_accept_irq`, `kvm_apic_set_irq` |
| virt/kvm/async_pf.c | `kvm_flush_and_free_async_pf_work`, `kvm_check_async_pf_completion`, `kvm_async_pf_wakeup_all` |
| arch/x86/kvm/msrs.c (v7.3.3; x86.c in v6.14) | `kvm_pv_enable_async_pf` |

**On which inputs.** Every value of (`mp_state`, `pv_unhalted`, `apf.halted`), with a completion
queued or not, interrupts on or off, an interrupt pending or not, the page-ready slot free or
occupied; then every recorded run of `Breaks.lean` whose rule book has that kernel, step by step.
The output must equal what `Today.lean` and `Design.lean` compute: 2,867, 4,804 and 2,951
operations agree for v6.14, v7.3.3 and v7.3.3 with the patch. The harness proves it can fail by
running v7.3.3's functions against the patched rule book: 55 operations differ, as they must.
The counts are pinned in `harness/operations.expected`: a run that compares a different number
exits 1. Before it compares anything `run.sh` checks that every run of `Breaks.lean` is in
`recordedRuns`, the hand-kept list `hostops` replays.

A step that cannot be carried out ends the run as BLOCKED with exit status 2, never as agreement
and never as a disagreement (exit 1). `harness/test-run.sh` injects five such failures through a
stand-in tool on PATH and checks each: `hostops` failing after ten rows, the self-test's `diff`
exiting 2 and exiting 0, a kernel's own `diff` exiting 2, the count of differing operations
unreadable, and the compiler failing. It does not inject a crash of the compiled functions, a
failed fetch or a failed `sed` or `paste`; those are checked in the script and not tested.

**What it does not run.**

- Six of the twelve kernel rules: `nestedExit`, `xenBlock`, `xenWake`, `initAp`, `sipi`,
  `snpReset` (vmx/nested.c, svm/nested.c, xen.c, the INIT and SIPI arms of lapic.c, svm/sev.c).
  Each is one or two assignments, read from the cited lines.
- Code around the cut functions, stood in for by hand where a run needs it (`harness.c` lists
  each): `vcpu_enter_guest` itself, the async page fault worker, the guest's interrupt handler,
  the LAPIC's own state. The stubs make the INIT, SMM, nested, Xen and Hyper-V arms of
  `kvm_vcpu_has_events` constant false.
- The guest. No function of Linux 6.18.60 (the PV qspinlock paths, `kvm_wait`, the cross-CPU
  call, the idle loop) is compiled or run: `Spec.lean`'s guest steps rest on the faithfulness
  review alone.
- The VMM. No Firecracker code is run here. The fork's own test in #2135 runs the restore on
  real KVM (BREAKS.md, 3).

### Assumed

Each abstraction, why it does not hide a lost wake-up, and whether the proofs need it.

| Assumption | Why it is safe | Needed by the proofs |
|---|---|---|
| The VMM never writes `mp_state` to a live VM (`Sound.noSet`) | Not safe to drop: see above. True of the pinned fork by reading its one call site; unenforced there | yes |
| The restore is exactly Firecracker's order with the kick last (`firecrackerWithKick`) | It is what #2135 shipped (`KvmVm::restore_kvm_state`). Other orders are searched, not proved (BREAKS.md, 5 to 7) | yes |
| The restore kick reaches every vCPU saved RUNNABLE | The fork gives none to a guest that was not offered `KVM_FEATURE_PV_UNHALT`, to a vCPU whose APIC id is 0xff, or to one with no enabled local APIC (`pv_kick_target`, `kick_runnable_vcpus`). The first has no paravirtual spinlocks and so no `kvm_wait`; the other two cannot be kicked by their own guest either. The model's guest always has paravirtual spinlocks and reachable APICs | yes |
| INIT arrives only at an offline CPU | Linux sends it to nothing else. An INIT at a vCPU asleep in `kvm_wait` resets it inside the lock path, and the invariant does not hold across that | yes |
| Each KVM function is one atomic step | Every clear of the kick runs on the vCPU's own thread or under its mutex, and the only write from another thread sets it. `vcpu_block` reads the kick more than once (v7.3.3 x86.c:8579, 8606): a kick that lands between two reads only makes the vCPU runnable sooner. The async page fault worker writes none of `mp_state`, `pv_unhalted`, `apf.halted` | yes |
| `kvm_vcpu_block`'s sleep and wake protocol (rcuwait, `kvm_vcpu_kick`, halt polling) is correct | A blocked vCPU is modelled as running again exactly when `kvm_arch_vcpu_runnable` is true. A lost host-side wake of the vCPU thread is a different defect, not followed here | yes |
| Interrupt injection is correct: a pending, unmasked interrupt reaches a guest that runs | `KVM_REQ_EVENT`, the interrupt window and posted interrupts are not modelled. Every writer in the model raises `KVM_REQ_EVENT` in the source (lapic.c:1454, 3347; x86.c:9086) | yes |
| A host page fault taken with interrupts off is invisible | `kvm_can_do_async_pf` refuses it (x86.c:10543-10547), so the vCPU thread waits in the fault itself and no KVM state changes | yes |
| "Page not present" is injected only in ordinary code | The guest does not set `KVM_ASYNC_PF_SEND_ALWAYS`, so KVM injects in user mode only (x86.c:10513-10515); a fault anywhere else with interrupts on is a synthetic halt, which the model has at every such position | no |
| One outstanding async page fault a vCPU | Completions are independent, and one is enough to clear a kick. With the slot occupied a second one waits on the done list, which is modelled | no |
| A repeated fault halts again only after a synthetic first fault | The code does it after either kind (mmu/mmu.c:4672-4675). A halt the model lacks is a wake-up it has | no |
| A guest starts nested entry, Xen poll and CPU offlining only from ordinary code | Linux does. These paths are in the model so that the search, not judgement, says they clear no kick that is owed | yes |
| Devices and the tick are interrupts that may arrive or not (`Cfg.rescue`) | They can only wake a vCPU. No property depends on one arriving | no |
| A snapshot holds what `Saved` holds | Two things the code does beyond it only add wake-ups: `KVM_GET_MP_STATE` runs `kvm_apic_accept_events` first (x86.c:9020), and Firecracker replaces a zero TSC deadline with the current TSC (arch/x86_64/vcpu.rs:417-448) | no |

## What is modelled

Two vCPUs and one PV qspinlock: the lock byte, both wait sites (`pv_wait_head_or_lock` and
`pv_wait_node`), `pv_kick_node`, lock stealing, the unlock fast and slow paths, with locks taken
with interrupts off and on. A cross-CPU call that waits for its target. The idle loop, with and
without its timer. Per vCPU in KVM: `mp_state`, `pv_unhalted`, `apf.halted`, one async page fault
in flight or done (page-not-present injected, synthetic halt, or the restore's wake-everyone), the
guest's page-ready slot and the page-ready interrupt, the call-function interrupt in the IRR, the
APIC timer, and where the vCPU thread is. The VMM: pause, snapshot and restore into a new VM in
the rule book's ioctl order, resume, any number of times.

### Each transition and its source

Guest, Linux v6.18.60 (`pvq.h` is kernel/locking/qspinlock_paravirt.h, `kvm.c` is
arch/x86/kernel/kvm.c):

| Transition (`Pc`, `Act`) | Source |
|---|---|
| `lkEnter`: steal the lock, become queue head, or queue behind the head | `pv_hybrid_queued_unfair_trylock` pvq.h:81-99; `xchg_tail` kernel/locking/qspinlock.c:260-290 |
| `hdTry`, `hdTry2`: trylock in the spin loop, pending bit set | pvq.h:429-435 |
| `hdHash`: `xchg(&lock->locked, _Q_SLOW_VAL)` | pvq.h:437-463 |
| `hdChk`, `ndChk`: `kvm_wait` turns interrupts off and reads | kvm.c:1092-1113; called at pvq.h:466 and pvq.h:322 |
| `hdHlt`, `ndHlt`: `halt()` or `safe_halt()` | kvm.c:1104, 1110 |
| `ndSpin`, `ndSet`, `ndRe`, `ndCas`: `pv_wait_node` | pvq.h:298-305, 317, 319, 330 |
| `hoSet`: `arch_mcs_spin_unlock_contended(&next->locked)` | kernel/locking/qspinlock.c:370 |
| `hoKick`: `pv_kick_node` | kernel/locking/qspinlock.c:371; pvq.h:357-389 |
| `ulTry`: `try_cmpxchg_release(&lock->locked, &locked, 0)` | pvq.h:543-556 |
| `ulRel`: `pv_unhash`, `smp_store_release(&lock->locked, 0)` | pvq.h:523-529 |
| `ulKick`: `pv_kick` to `kvm_kick_cpu`, `KVM_HC_KICK_CPU` | pvq.h:539; kvm.c:1081-1088 |
| `ccSend`, `ccWait`, `irq`: cross-CPU call | kernel/smp.c:829-859, 877, 461-464. A target polling in the idle loop gets `need_resched` and no interrupt (smp.c:115-120); it then runs the queue, which the model does not tell apart from taking the interrupt |
| `idChk`, `idHlt`: idle loop, interrupts off, re-check, `safe_halt` | kernel/sched/idle.c:298-330, 116; arch/x86/kernel/process.c:765-768 |
| `idArm`: the tick is stopped and the next event programmed | kernel/sched/idle.c:226 |
| `irqApf`: the page-ready handler clears the slot and acknowledges | kvm.c:317-334 |

Host KVM (paths under arch/x86/kvm/; the v6.14 line, then the v7.3.3 line):

| Rule (`Kernel` field, or step) | v6.14 | v7.3.3 |
|---|---|---|
| `halt`: `__kvm_emulate_halt` | x86.c:11290-11310 | x86.c:8693-8712 |
| `Vcpu.runnable`: `kvm_arch_vcpu_runnable` | x86.c:11107-11166 | x86.c:8516-8572 |
| `hostRun`: one iteration of `vcpu_run`; `vcpu_block` | x86.c:11169-11237 | x86.c:8643-8691, 8575-8640 |
| `unhalt`: the switch that ends `vcpu_block` | x86.c:11220-11235 | x86.c:8625-8638 |
| `present`: tail of `kvm_arch_async_page_present` | x86.c:13476-13477 | x86.c:10603-10604 |
| completion dequeued on entry, when the page-ready slot is free | x86.c:10849-10850 | x86.c:8216-8217, 10618-10624; virt/kvm/async_pf.c:153-174. A second caller is the guest's write of `MSR_KVM_ASYNC_PF_ACK` (msrs.c:1695-1707): the model's `hostRun` of a vCPU in guest mode, at any position, covers it |
| `Vcpu.present`: token written to the slot, page-ready interrupt raised (not for a synthetic halt) | x86.c:13469-13474 | x86.c:10596-10601 |
| `Vcpu.events`: done list, or an interrupt the guest can take | x86.c:11115-11116, 11146-11147 | x86.c:8524-8525, 8552-8553 |
| `kick`: `APIC_DM_REMRD` | lapic.c:1351-1356 | lapic.c:1451-1456; hypercall x86.c:7383-7389 |
| `getMpState`: `KVM_GET_MP_STATE` | x86.c:11774-11779 | x86.c:9025-9030 |
| `setMpState`: `KVM_SET_MP_STATE` | x86.c:11827 | x86.c:9072; helper x86.h:143-148 |
| `faultPv`, `faultSynth`, `refault` | | x86.c:10533-10579, 8147-8152; mmu/mmu.c:4670-4678 |
| `apfDone`: `async_pf_execute` | | virt/kvm/async_pf.c:45-97; x86.c:10607-10616 |
| wake-everyone on `MSR_KVM_ASYNC_PF_EN` | x86.c:3550 | msrs.c:1384-1417; virt/kvm/async_pf.c:216-241 |
| `nestedExit` | vmx/nested.c:5073-5074; svm/nested.c:996-997 | vmx/nested.c:5187-5188; svm/nested.c:1340-1341 |
| `xenBlock`, `xenWake`; `Vcpu.wake` and `Vcpu.pause` at a polling vCPU: the poll returns runnable whatever ended it | xen.c:1483, 1494 | xen.c:1581-1592 |
| `initAp`, `sipi` | lapic.c:3397-3413; x86.c:12440-12442 | lapic.c:3617-3633; x86.c:9607-9609 |
| `snpReset` | svm/sev.c:3856-3858, 3895-3897 | svm/sev.c:4129-4146 |
| `tmrFire`, `tmrInject`, deadline write | | lapic.c:2053-2089, 3203-3211, 2749-2759; x86.c:8670-8671 |

VMM, ejc3/firecracker (paths under src/vmm/src/). The restore without the kick is the fork at
3e203733cb91, which `rootfs-config.toml` pinned until #2135; the kick is the fork at 2a33289b55b7,
the pin since. Between the two, `lib.rs` and `vstate/vcpu.rs` are unchanged and
`arch/x86_64/vcpu.rs` gained four comment lines at 709.

| Step | Source |
|---|---|
| `pause`, `resume` | lib.rs:463-484; vstate/vcpu.rs:240-345 |
| `save`: `KVM_GET_MP_STATE` first, then the LAPIC, the MSRs, the vCPU events | arch/x86_64/vcpu.rs:591-650 |
| `clone`: new vCPUs (the boot processor runnable, the other UNINITIALIZED: v7.3.3 x86.c:9376-9379), then `firecracker` order | at 3e203733cb91: builder.rs:434-494; arch/x86_64/vcpu.rs:684-750 |
| `MSR_IA32_TSC_DEADLINE` restored last | arch/x86_64/vcpu.rs:40-47, 514-517 |
| `Ioctl.kick`: after every vCPU's state and the VM's, `KVM_SIGNAL_MSI` in remote-read mode to each vCPU saved RUNNABLE | at 2a33289b55b7: `restore_kvm_state` arch/x86_64/vm.rs:256-269, `kick_runnable_vcpus` :299-326, `pv_kick_target` :109-128; called from builder.rs:499 |

fcvm's own restore loads the snapshot with `resume_vm: false` (`src/commands/common.rs:4315-4319`),
patches the root drive and resumes; it issues no KVM state ioctl of its own, so it is the `clone`
then `resume` of the model. Its userfaultfd server is the source of the host page faults
(`faultPv`, `faultSynth`), which may start wherever the guest has interrupts on and complete at any
later time, also while the VM is paused.

## What is not modelled

- **More than two vCPUs, more than one lock.** The kick is one flag per vCPU and the invariant is
  per vCPU, so a third vCPU adds interleavings, not a new way to clear a kick; that argument is not
  a proof.
- **Device interrupts as obligations.** A virtio interrupt that a snapshot must carry is the
  in-kernel irqchip's state (`KVM_GET_IRQCHIP`), which Firecracker saves; it is not in the model.
- **The guest task that waits for a page-ready token.** Only the vCPU-level effect of a completion
  is modelled. The restore's wake-everyone completion is what wakes those tasks in a new VM.
- **`KVM_REQ_EVENT`, the interrupt window, posted interrupts, halt polling.**
- **A vCPU waiting in `KVM_MP_STATE_UNINITIALIZED`.** A new application processor is created in
  that state and every modelled restore overwrites it.
- **What a kick does to a vCPU that waits for INIT or SIPI.** #2135 observed it on KVM: with a
  kick recorded, `KVM_RUN` of an UNINITIALIZED vCPU returns `EAGAIN` and of an INIT-received one
  spins in `vcpu_run()`. It is why the fork kicks RUNNABLE vCPUs only. The model's restore kicks
  the same set, so the model never puts a kick on such a vCPU through a restore, and it has no
  transition for either effect.
- **Userspace irqchip, `KVM_X86_DISABLE_EXITS_HLT`, SMM, the TDX and SEV-ES register paths.**
- **`__set_sregs`'s reset special case** (regs.c:629-633): it makes the boot processor runnable
  only at the real-mode reset vector, which a restored Linux guest is never at.
- **Time.** No timeouts, no watchdog NMI (which is what un-sticks a hung guest that has
  `softlockup_all_cpu_backtrace` set).
- **arm64.** See below.

## arm64

The mechanisms of this model do not exist on arm64 KVM, which is why the arm64 lanes show neither
hang:

- **The halt is WFI, and it is not in `mp_state`.** `kvm_handle_wfx` calls `kvm_vcpu_wfi` and then
  moves the PC past the instruction (arch/arm64/kvm/handle_exit.c:130-178, arm.c:1065-1089). A
  vCPU thread taken out of KVM_RUN while in WFI returns with the PC already advanced, so a snapshot
  holds a vCPU that has woken: a spurious wake-up, which the idle loop tolerates. arm64's
  `mp_state` has only STOPPED and SUSPENDED (arm.c:779-805), both PSCI states.
- **There is no paravirtual kick.** arm64 uses plain queued spinlocks
  (arch/arm64/Kconfig:95, `ARCH_USE_QUEUED_SPINLOCKS`, and no `PARAVIRT_SPINLOCKS`); its only
  paravirtual interface is stolen time (arch/arm64/kernel/paravirt.c:27-46). A waiter spins, it
  never halts on a lock.
- **There are no async page faults.** arch/arm64/kvm/Kconfig (lines 20-39 at v7.3.3) does not
  select `KVM_ASYNC_PF`. A fault that needs I/O is taken by the vCPU thread itself.
- **A pending interrupt is the vGIC's state**, which `kvm_arch_vcpu_runnable` reads (arm.c:849-859)
  and Firecracker saves and restores through the vGIC device.

## CI

`.github/workflows/verify.yml`, job `lean` (check name "Lean"), on a hosted runner, on every pull
request that changes `verify/**` or the kernel patch the harness applies (no base-branch filter,
so stacked pull requests are checked) and on every push to main. It runs the six steps of `make
verify` (the table under "Build"), capped at 15 minutes. elan comes from a checksum-pinned release
file, the toolchain is the one `lean-toolchain` names, and both are cached with the build
directory. It is a workflow of its own and is not part of `Summary`.
`tests/test_ci_workflow_coverage.rs` (`the_lean_workflow_checks_verify_on_stacked_pull_requests_and_on_main`)
pins its triggers and each of the six lines of its build step.

A second job, `search`, runs all 76 searches against `search.expected` when the workflow is
dispatched by hand (`gh workflow run verify.yml --ref <branch>`): run it after a change to
`Spec.lean`, `Today.lean`, `Design.lean`, `Properties.lean` or `Search.lean`.

**Why the sweep has a step of its own.** CI restores `verify/.lake` from an earlier run, and `lake
build` elaborates `Axioms.lean`, and so its sweep, only when something that file imports changed.
A new module that nothing imports, holding an `axiom` and a theorem resting on it, built with exit
0 on such a directory. Now `check-imports.sh` refuses the file before the build (it is not among
`Axioms.lean`'s imports), and `check-sweep.sh` elaborates `Axioms.lean` after it on every run,
where the sweep refuses it too. A library dropped from `defaultTargets`, or given a list of named
modules in place of its glob, is refused by `check-imports.sh`: with either, a `sorry` in
`Proofs.lean` built with exit 0.

What fails `lake build`, each observed by breaking the model one way in a copy of this directory
(exit status 1 each time, and 0 for the unchanged copy):

| Broken | How | `lake build` |
|---|---|---|
| a proof | `V733Patched.present` made to clear the kick (REPAIR 1 undone) | fails in `Proofs.lean` (`v733Patched_keepsKick`: unsolved goal `present_keeps`; `v733Patched_entry_keeps_kick`) and in `Breaks.lean` (`design_survives`) |
| an axiom beyond the allowed ones | `breaks_kickFirst` proved with `native_decide` | fails in `Axioms.lean`: "theorems resting on more than Lean's own axioms: ... Wake.breaks_kickFirst depends on [Wake.breaks_kickFirst._native.native_decide.ax_1_1]" |
| a declared axiom | `axiom every_run_is_fine : ∀ s : State, NoHang s` added to `Proofs.lean` | fails in `Axioms.lean`: "Wake.every_run_is_fine is an axiom" |
| a `sorry` in a proof | the proof of `sound_noHang` replaced by `sorry` | fails in `Proofs.lean`: "declaration uses `sorry`", an error because warnings are (`lakefile.toml`) |
| a `sorry` in a recorded break | `breaks_snapshotNodeCall` proved by `sorry` | fails in `Breaks.lean`: "declaration uses `sorry`" |
| a recorded break | the last step of `Run.pageReady` removed | fails in `Breaks.lean`: "`decide` proved that the proposition ... is false" |
| a run under the design that stops being possible | `design_survives` asked about `Run.pageReady` in a configuration without locks taken with interrupts off | fails in `Breaks.lean`: "`decide` proved that the proposition `after (replay Design.rules ...) NoLostKick ∧ ...` is false" (`after` is false for a run that cannot be replayed) |

What the other steps fail on, each observed the same way:

| Step | Broken | Result |
|---|---|---|
| `check-imports.sh` | a module nothing imports; `WakeVerify` out of `defaultTargets`; seven named modules for the glob | exit 1, naming it (`lake build` alone: exit 0 each time) |
| `check-sweep.sh` | the module nothing imports, on a built directory | exit 1: "WakeVerify.Extra is not imported by WakeVerify.Axioms" |
| `check-sweep.sh` | `warningAsError` removed and a definition Lean warns about | exit 1 (`lake build` alone: exit 0, with the warning printed) |
| `test-search.sh` | the audit run on an unfinished search; `main` ignoring the audit's count; a pattern that matches no search; a panic | its cases fail: see the script's header |
| `harness/test-run.sh` | `run.sh` before its checks | its cases fail: 3 of the first 5 against the script as first committed, the 3 added later against the script before them |
| `search-expected.sh` | a kind removed from, added to or changed in `search.expected`, or a search's block dropped | exit 1: "the search differs" |

## Conventions

Plain Lean 4 with the layout of AWS Cedar's models: `Spec`, `Today`, `Design`, `Properties`,
`Breaks`, `Lemmas`, `Proofs`, `Axioms`, and a search executable. Package `WakeVerify`, namespace
`Wake`. A break found by the search is pasted into `Breaks.lean` and stays a kernel-checked fact.
A proof that will not close is a finding: the hypothesis `Sound.noSet` is one (`REPAIRS.md`, 3).

## The reviews

Two independent reviews were made of the first version of this directory, one of the model's
faithfulness to the source it cites and one of the properties and proofs. Neither found a fault
in the three hang findings or in the design's theorems as they then stood. What they did find,
and what changed:

**Found by the faithfulness review.**

- *The model had no page-ready interrupt.* `kvm_arch_async_page_present` raises the page-ready
  vector for the wake-everyone completion and for one whose "page not present" was injected, so a
  vCPU that halts with interrupts on right after such a completion does not block. The first
  version reported lost kicks "with interrupts on" for those cases that the code does not
  produce. The interrupt and the guest's page-ready slot are now state (`apfIrq`, `slot`), and
  those breaks are gone (BREAKS.md says which remain). With interrupts off the vector is masked,
  and no hang finding changed.
- *The slot.* #2131 and #2132 need the page-ready slot to be free in the snapshot's memory;
  otherwise the completion stays on the done list and the HLT cannot block. Modelled, and stated
  in BREAKS.md.
- *The harness did not run the queueing or the dequeue of #2132.* It now cuts those functions too.
- Smaller: a new application processor is created UNINITIALIZED; Xen's poll returns runnable
  whatever ended it; citations.

**Found by the proof and property review.**

- *The audit could not do its job.* It counted a hung source VM as alive (a snapshot restore
  revives it), ran in one configuration, accepted any flag on any vCPU as an explanation, and did
  not gate the exit status. All four are changed ("Audited", above).
- *"Proved" meant less than the README said.* The first version proved that seven state shapes are
  unreachable, and a sleeper whose lock byte was still `_Q_SLOW_VAL` was in none of them. That
  shape is now covered (`HasWaker`, with the invariant lines `lockHeld` and `queue`), and the
  README says in so many words that progress is audited, not proved.
- `NoStuckCall` was defined through the first property; it is now stated without it. `NoLostTimer`
  lacked a "nothing else pending" guard. `design_survives` would have passed for a run that had
  become impossible. One snapshot and restore per run was an unlisted bound: it is removed. Faults
  only in ordinary code, INIT and SIPI only with the other vCPU running, hotplug only on one vCPU
  were unlisted restrictions the proofs did not need: they are removed from the model (the first
  is a switch for the searches).

**Not reviewed a second time.** The changes made in answer to the reviews (the page-ready
interrupt and slot, `HasWaker` with its two invariant lines, the audit) were not sent back to
the reviewers. The interrupt and slot rules were read again against v7.3.3 x86.c:10582-10624 and
v6.18.60 kvm.c:317-334 by a second author, the harness runs the kernel side of them, and the
proofs are checked by Lean's kernel whoever reads them.

**Not checked by either review**, and so resting on the model's authors alone: kernel releases
between v6.15 and v7.3.3 (the model has v6.14 and v7.3.3); the nested event path, SEV and Xen
beyond the lines that write `mp_state`; `KVM_SET_LAPIC` in full; the rcuwait and halt-polling wake
protocol of `kvm_vcpu_block`; more than two vCPUs.
