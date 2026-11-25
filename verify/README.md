# verify: how a guest vCPU's wake-up is lost, in Lean

A small Lean 4 model of the path a paravirtual spinlock kick takes from one guest vCPU to another
on x86_64, through the guest kernel, host KVM and the VMM, with rule books for the code as it is
(`Today.lean`, every rule citing file, line and release) and for the repaired system
(`Design.lean`). It follows the shape of AWS's Cedar models
([How We Built Cedar](https://arxiv.org/abs/2407.01688)): plain data, one function per step,
properties as decidable propositions, an exhaustive search for counterexamples against what
exists, and proofs for what is designed.

It was written after two hangs of restored clones (#2125, #2131), to find every way the same
wake-up can be lost, not only those two.

- `BREAKS.md`: every break the search found, per kernel and VMM variant, with its trace, what it
  is in the real system, and a severity.
- `REPAIRS.md`: what each break needs changed, where, and the wording proposed.

## Build

```sh
make verify           # cd verify && lake build: every proof and every recorded break, in the kernel
make verify-search    # lake exe breaks: the exhaustive search, every rule book and configuration
make verify-harness   # the model's kernel rules against the kernel's own functions (needs cc, network)
```

The toolchain is pinned in `lean-toolchain` (`leanprover/lean4:v4.35.0`; elan installs it on first
use). Lean's core only: no Mathlib, no other package.

Measured on an arm64 dev box (Neoverse, one build at a time), toolchain already installed:

| Command | Wall time | Peak memory |
|---|---|---|
| `lake build` from cold | 1 min 25 s | 0.8 GB |
| `lake exe breaks` (70 searches, 150.7 million states in all) | 9 min 43 s | 1.8 GB |
| `harness/run.sh` after `lake build` | 2 s | 0.1 GB |

## The files

| File | What it holds |
|---|---|
| `WakeVerify/Spec.lean` | `State` (two vCPUs, one lock, the VMM's view), the guest paths as one step per instruction (`advance`, `guest`), the host's steps (`hostRun`, `host`), the VMM's (`pause`, `resume`, `clone`), `step`, `replay`, `Reachable`. `Kernel` and `Rules` are the rule books' type; `Cfg` is what exists around the guest |
| `WakeVerify/Today.lean` | `V614.kernel`, `V733.kernel`: what each release does at every place that writes `mp_state` or the kick. `firecracker`: the pinned fork's restore order. `Today.v614`, `Today.v733` |
| `WakeVerify/Design.lean` | `V733Patched.kernel` (#2130), `firecrackerWithKick` (#2131), `Design.rules`; the partial combinations and the restore orders the search also runs |
| `WakeVerify/Properties.lean` | The properties, decidable on a state |
| `WakeVerify/Breaks.lean` | Each run the search found, replayed and checked by the kernel (`by decide`) |
| `WakeVerify/Lemmas.lean` | `KeepsKick` (what the proofs need of a kernel), the invariant `PInv`, and that every step keeps it |
| `WakeVerify/Proofs.lean` | `Sound`, `reachable_inv`, the `sound_*` and `design_*` theorems, which kernels are sound |
| `WakeVerify/Axioms.lean` | Fails the build if a theorem rests on more than `propext`, `Quot.sound`, `Classical.choice` |
| `Search.lean` | `lake exe breaks`: breadth-first search, kinds of break, shortest runs, the audit |
| `HostOps.lean`, `harness/` | `lake exe hostops` and the C harness: the kernel's own functions against the rule books |

## The properties

| | Property | v6.14 | v7.3.3 | Design |
|---|---|---|---|---|
| P1 | **NoLostKick**: no vCPU sleeps in `kvm_wait`, the lock byte it waits on released, with nothing pending that can wake it. `NoHang` is the case with interrupts off, where only an NMI helps | broken (#2131) | broken (#2125, #2131) | proved |
| P2 | **NoStuckCall**: no vCPU waits in a cross-CPU call for a vCPU that P1 says never wakes, and no queued call is without its interrupt | broken | broken | proved |
| P3 | **MutualExclusion**: whatever wakes a vCPU (a kick, a completion, an NMI, an interrupt meant for something else), it never enters the critical section beside the owner. With `hlt_rechecks`: the step after every HLT in the guest paths goes back to the read | holds | holds | proved |
| | **NoOrphanHalt**: a synthetic halt has its completion coming | holds | holds | proved |
| | **NoLostTimer**: a vCPU that halts for its timer still has one after a restore | holds | holds | proved |
| | **NoStrandedNode**: the guest's queue protocol never leaves a node asleep that was handed the queue head | holds | holds | proved |

"holds" is the exhaustive search finding no reachable state that breaks it. "proved" is a theorem
for runs of any length (`Proofs.lean`). The six kernel and VMM combinations, each with and without
the restore kick, are in `BREAKS.md`.

## What is proved, what is checked, what is assumed

**Proved** (Lean's kernel, no `sorry`, axioms `propext` and `Quot.sound` only):

- `design_noLostKick`, `design_noHang`, `design_noStuckCall`, `design_mutualExclusion`,
  `design_noOrphanHalt`, `design_noLostTimer`, `design_noStrandedNode`: for every configuration in
  which userspace does not write `mp_state` to a live VM (so with a nested guest, Xen, hotplug and
  SEV paths on as well), every state reachable from `init` under `Design.rules` has the property.
  The proof is an inductive invariant (`PInv`), not the search, so it does not depend on a bound on
  the length of a run. The bounds that remain are the model's own: two vCPUs, one lock.
- The same for any rule book that is `Sound`: `v614Kick_sound` (v6.14 with the restore kick),
  `v614_sound_without_snapshots` (v6.14 as it is has no #2125), and
  `v733Patched_sound_without_snapshots`.
- `v733_not_keepsKick`: v7.3.3 as released fails the one line of `KeepsKick` that the #2130 patch
  restores.
- Every run in `Breaks.lean`: replayed step by step, the property evaluated, by `decide`.

**Checked exhaustively, not proved**: that the rule books of today have no break other than those
listed (the search visits every reachable state of each finite model; the search program is
compiled Lean and is not itself verified), and the audit below.

**The audit** (`Search.lean`, `deadStates`): with interrupts from unmodelled sources arriving for
ever, the search computes every reachable state from which some vCPU can never execute another
instruction, without using the properties, and checks that each such state is on the way to one a
property flags. Under today's rules there are 1,060 to 2,136 such states and all are explained;
under the design there are none. This is the check that the properties, as written, do not miss a
hang.

**Checked against the kernel's own code** (`harness/`): `__kvm_emulate_halt`, `vcpu_block`,
`kvm_arch_vcpu_runnable`, `kvm_arch_async_page_present`, the `APIC_DM_REMRD` arm of
`__apic_accept_irq`, `KVM_GET_MP_STATE` and `KVM_SET_MP_STATE` are cut verbatim from v6.14 and
v7.3.3 (fetched and checksum-pinned, not kept here), compiled with stubs for what they call, and
run on every value of (`mp_state`, `pv_unhalted`, `apf.halted`) with and without an event pending,
and then on every recorded run. The output must equal what `Today.lean` and `Design.lean` compute:
494, 929 and 705 operations agree for v6.14, v7.3.3 and v7.3.3 with the #2130 patch. The harness
proves it can fail by running v7.3.3's functions against the patched rule book.

**Assumed** (each abstraction, and why it does not hide a lost wake-up):

| Assumption | Why it is safe |
|---|---|
| Each KVM function is one atomic step | Each one reads the kick at most once before it may clear it, and the only write from another thread is the kick setting it. Every interleaving equals one of the two orders of atomic steps |
| `kvm_vcpu_block`'s sleep and wake protocol (rcuwait, `kvm_vcpu_kick`) is correct | A blocked vCPU is modelled as running again exactly when `kvm_arch_vcpu_runnable` is true. A lost host-side wake of the vCPU thread is a different defect, not followed here |
| Interrupt injection is correct: a pending, unmasked interrupt reaches a guest that runs | `KVM_REQ_EVENT`, the interrupt window and posted interrupts are not modelled. Every writer in the model raises `KVM_REQ_EVENT` in the source (lapic.c:1454, 3347; x86.c:9086) |
| The page-ready slot is always free | With the slot occupied the completion waits on the done list, which keeps `kvm_vcpu_has_events` true: the vCPU cannot block. The free slot is the case in which a completion can clear a kick |
| A host page fault taken with interrupts off is invisible | `kvm_can_do_async_pf` refuses it (x86.c:10543-10547), so the vCPU thread waits in the fault itself and no KVM state changes |
| An async page fault starts only in ordinary code (`run`) | One taken elsewhere with interrupts on blocks the vCPU where it is and resumes there: the completions, kicks and interrupts that can arrive meanwhile are the same as for an earlier fault still in flight, which is modelled at every position |
| One outstanding async page fault a vCPU | Completions are independent, and one is enough to clear a kick |
| A guest starts nested entry, Xen poll and CPU offlining only from ordinary code, and sends INIT only to an offline CPU | Linux does. These paths are in the model so that the search, not judgement, says they clear no kick that is owed |
| Devices and the tick are interrupts that may arrive or not (`Cfg.rescue`) | They can only wake a vCPU. No property depends on one arriving |

## What is modelled

Two vCPUs and one PV qspinlock: the lock byte, both wait sites (`pv_wait_head_or_lock` and
`pv_wait_node`), `pv_kick_node`, lock stealing, the unlock fast and slow paths, with locks taken
with interrupts off and on. A cross-CPU call that waits for its target. The idle loop, with and
without its timer. Per vCPU in KVM: `mp_state`, `pv_unhalted`, `apf.halted`, one async page fault
in flight or done (page-not-present injected, synthetic halt, or the restore's wake-everyone), the
call-function interrupt in the IRR, the APIC timer, and where the vCPU thread is. The VMM: pause,
snapshot and restore into a new VM in the rule book's ioctl order, resume.

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
| `hoKick`: `pv_kick_node` | kernel/locking/qspinlock.c:371; pvq.h:353-389 |
| `ulTry`: `try_cmpxchg_release(&lock->locked, &locked, 0)` | pvq.h:543-556 |
| `ulRel`: `pv_unhash`, `smp_store_release(&lock->locked, 0)` | pvq.h:523-529 |
| `ulKick`: `pv_kick` to `kvm_kick_cpu`, `KVM_HC_KICK_CPU` | pvq.h:539; kvm.c:1081-1088 |
| `ccSend`, `ccWait`, `irq`: cross-CPU call | kernel/smp.c:829-859, 877, 461-464 |
| `idChk`, `idHlt`, `idArm`: idle loop | kernel/sched/idle.c:298-350; arch/x86/kernel/process.c:765-768 |

Host KVM (paths under arch/x86/kvm/; the v6.14 line, then the v7.3.3 line):

| Rule (`Kernel` field, or step) | v6.14 | v7.3.3 |
|---|---|---|
| `halt`: `__kvm_emulate_halt` | x86.c:11290-11310 | x86.c:8693-8712 |
| `Vcpu.runnable`: `kvm_arch_vcpu_runnable` | x86.c:11107-11166 | x86.c:8516-8572 |
| `hostRun`: one iteration of `vcpu_run`; `vcpu_block` | x86.c:11169-11237 | x86.c:8643-8691, 8575-8640 |
| `unhalt`: the switch that ends `vcpu_block` | x86.c:11220-11235 | x86.c:8625-8638 |
| `present`: tail of `kvm_arch_async_page_present` | x86.c:13476-13477 | x86.c:10603-10604 |
| completion handled on entry | x86.c:10849-10850 | x86.c:8216-8217; virt/kvm/async_pf.c:153-174 |
| `kick`: `APIC_DM_REMRD` | lapic.c:1351-1356 | lapic.c:1451-1456; hypercall x86.c:7383-7389 |
| `getMpState`: `KVM_GET_MP_STATE` | x86.c:11774-11779 | x86.c:9025-9030 |
| `setMpState`: `KVM_SET_MP_STATE` | x86.c:11827 | x86.c:9072; helper x86.h:143-148 |
| `faultPv`, `faultSynth`, `refault` | | x86.c:10533-10579, 8147-8152; mmu/mmu.c:4670-4678 |
| `apfDone`: `async_pf_execute` | | virt/kvm/async_pf.c:45-97; x86.c:10607-10616 |
| wake-everyone on `MSR_KVM_ASYNC_PF_EN` | x86.c:3550 | msrs.c:1384-1417; virt/kvm/async_pf.c:216-241 |
| `nestedExit` | vmx/nested.c:5073-5074; svm/nested.c:996-997 | vmx/nested.c:5187-5188; svm/nested.c:1340-1341 |
| `xenBlock`, `xenWake` | xen.c:1483, 1494 | xen.c:1581, 1592 |
| `initAp`, `sipi` | lapic.c:3397-3413; x86.c:12440-12442 | lapic.c:3617-3633; x86.c:9607-9609 |
| `snpReset` | svm/sev.c:3856-3858, 3895-3897 | svm/sev.c:4129-4146 |
| `tmrFire`, `tmrInject`, deadline write | | lapic.c:2053-2089, 3203-3211, 2749-2759; x86.c:8670-8671 |

VMM, ejc3/firecracker at 3e203733cb91 (paths under src/vmm/src/):

| Step | Source |
|---|---|
| `pause`, `resume` | lib.rs:463-484; vstate/vcpu.rs:240-345 |
| `save`: `KVM_GET_MP_STATE` first, then the LAPIC, the MSRs, the vCPU events | arch/x86_64/vcpu.rs:591-650 |
| `clone`: new vCPUs, then `firecracker` order | builder.rs:434-494; arch/x86_64/vcpu.rs:684-750 |
| `MSR_IA32_TSC_DEADLINE` restored last | arch/x86_64/vcpu.rs:40-47, 514-517 |

fcvm's own restore (`src/commands/snapshot.rs`, `fc-agent/src/restore.rs`) loads the snapshot with
`resume_vm: false`, patches the root drive and resumes; it issues no KVM state ioctl of its own, so
it is the `clone` then `resume` of the model. Its userfaultfd server is the source of the host page
faults (`faultPv`, `faultSynth`), which may start at any ordinary-code step and complete at any
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

## CI (proposed, not added here)

`lake build` from cold takes 1 min 25 s on the dev box, plus the toolchain download on a fresh
runner, which is over the two minutes this pull request was allowed for a new CI check. The job to
add when that is accepted:

```yaml
  lean:
    runs-on: ubuntu-latest
    timeout-minutes: 15
    steps:
      - uses: actions/checkout@v4
      - name: Install elan
        run: |
          curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh |
            sh -s -- -y --default-toolchain none
          echo "$HOME/.elan/bin" >> "$GITHUB_PATH"
      - uses: actions/cache@v4
        with:
          path: verify/.lake
          key: lake-${{ hashFiles('verify/lean-toolchain', 'verify/lakefile.toml', 'verify/**/*.lean') }}
      - run: cd verify && lake build
```

on `pull_request` and `push` to main with `paths: ['verify/**']`, in a workflow of its own so that
`ci.yml` keeps running for every pull request (`tests/test_ci_workflow_coverage.rs`). A broken
proof, an axiom beyond Lean's own, or a recorded run that no longer breaks its property fails it.
The search (`make verify-search`) and the harness (`make verify-harness`, which needs the network)
stay manual or nightly.

## Conventions

Plain Lean 4 with the layout of AWS Cedar's models: `Spec`, `Today`, `Design`, `Properties`,
`Breaks`, `Lemmas`, `Proofs`, `Axioms`, and a search executable. Package `WakeVerify`, namespace
`Wake`. A break found by the search is pasted into `Breaks.lean` and stays a kernel-checked fact.
A proof that will not close is a finding: the hypothesis `Sound.noSet` is one (`REPAIRS.md`, 3).
