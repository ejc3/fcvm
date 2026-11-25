import WakeVerify.Properties
import WakeVerify.Today
import WakeVerify.Design

/-!
# Today's rules, broken: each run the search found, checked by the kernel

Copied exactly from `lake exe breaks`: for each kind of break, the shortest run from `init` after
which a property fails. `by decide` replays the run and evaluates the property in the kernel, so
each is a checked fact about the model, not a printout: a run that stops being possible, or stops
breaking its property, fails the build. `verify/BREAKS.md` says each in words, with its trace.

`.vcpu .a` is vCPU 0 and `.vcpu .b` is vCPU 1. `.step` is the next instruction of the path the vCPU
is on.
-/

namespace Wake

set_option maxRecDepth 100000

/-- The fcvm configuration with the guest booted `no-kvmapf`. -/
def noKvmapf : Cfg := { Cfg.fcvm with pvApf := false }

/-- A VMM that writes `mp_state` back on a paused, live VM. -/
def writesBack : Cfg := { Cfg.fcvm with userSetMp := true }

/-- The idle loop programs its timer; no host page faults and no locks with interrupts on, to keep
the search of this configuration small. -/
def withTimer : Cfg := { Cfg.fcvm with timer := true, plainLocks := false, faults := false }

/-- **#2125.** v7.3.3: a page-ready completion clears the kick of a queue head between its last read of the lock byte and its HLT (15 steps). -/
def Run.pageReady : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockIrq,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_pageReady : ¬ after (replay Today.v733 Cfg.fcvm Run.pageReady init) NoHang := by decide

/-- #2125 at the other wait site: a queue node that `pv_kick_node` hashed (23 steps). -/
def Run.pageReadyNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultPv, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .apfDone, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_pageReadyNode : ¬ after (replay Today.v733 Cfg.fcvm Run.pageReadyNode init) NoHang := by decide

/-- #2125, then the other vCPU waits for ever in a cross-CPU call to the sleeper (17 steps): the soft lockup. -/
def Run.pageReadyCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockIrq,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_pageReadyCall : ¬ after (replay Today.v733 Cfg.fcvm Run.pageReadyCall init) NoStuckCall := by decide

/-- #2125 for a lock taken with interrupts on: the sleeper is woken by its next interrupt (15 steps). -/
def Run.pageReadyIrqsOn : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockPlain,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_pageReadyIrqsOn : ¬ after (replay Today.v733 Cfg.fcvm Run.pageReadyIrqsOn init) NoLostKick := by decide

/-- #2125 without the guest's async page fault interface (`no-kvmapf`): the completion of a synthetic halt that an interrupt had ended early clears the kick (21 steps). -/
def Run.synthHalt : List Label :=
  [.vcpu .a .call, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .hostRun, .vcpu .b .hostRun,
   .vcpu .b .irq, .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step,
   .vcpu .b .lockIrq, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_synthHalt : ¬ after (replay Today.v733 noKvmapf Run.synthHalt init) NoHang := by decide

/-- **#2131.** A kick pending for a runnable vCPU is in no ioctl's answer: the restored VM halts for ever (16 steps). v7.3.3. -/
def Run.snapshot : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshot : ¬ after (replay Today.v733 Cfg.fcvm Run.snapshot init) NoHang := by decide

/-- #2131 at a queue node (24 steps). -/
def Run.snapshotNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNode : ¬ after (replay Today.v733 Cfg.fcvm Run.snapshotNode init) NoHang := by decide

/-- #2131, then the other vCPU waits for ever in a cross-CPU call (18 steps). -/
def Run.snapshotCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_snapshotCall : ¬ after (replay Today.v733 Cfg.fcvm Run.snapshotCall init) NoStuckCall := by decide

/-- #2131 is not new: the same run on v6.14 (16 steps). -/
def Run.snapshot614 : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshot614 : ¬ after (replay Today.v614 Cfg.fcvm Run.snapshot614 init) NoHang := by decide

/-- **New (#2132).** v7.3.3 unpatched with the #2131 restore kick: restoring `MSR_KVM_ASYNC_PF_EN` queues the wake-everyone completion, and the vCPU's first entry runs it through `kvm_arch_async_page_present`, which clears the restore kick (16 steps). -/
def Run.wakeAll : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_wakeAll : ¬ after (replay Partial.v733Kick Cfg.fcvm Run.wakeAll init) NoHang := by decide

/-- The same at a queue node (24 steps). -/
def Run.wakeAllNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_wakeAllNode : ¬ after (replay Partial.v733Kick Cfg.fcvm Run.wakeAllNode init) NoHang := by decide

/-- The same, then the other vCPU waits for ever in a cross-CPU call (18 steps). -/
def Run.wakeAllCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_wakeAllCall : ¬ after (replay Partial.v733Kick Cfg.fcvm Run.wakeAllCall init) NoStuckCall := by decide

/-- The #2130 patch alone leaves #2131 (16 steps). -/
def Run.patchedNoKick : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_patchedNoKick : ¬ after (replay Partial.v733Patched Cfg.fcvm Run.patchedNoKick init) NoHang := by decide

/-- Outside the design: on the designed system, a VMM that reads `mp_state` of a paused VM and writes it back clears a pending kick (16 steps). Firecracker does not do this. -/
def Run.writeBack : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .vcpu .b .setMpSame, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_writeBack : ¬ after (replay Design.rules writesBack Run.writeBack init) NoHang := by decide

/-- Order: a restore kick sent before `KVM_SET_MP_STATE` is cleared by it on v6.15 and later (16 steps). -/
def Run.kickFirst : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_kickFirst : ¬ after (replay Order.kickFirst Cfg.fcvm Run.kickFirst init) NoHang := by decide

/-- Order: `MSR_IA32_TSC_DEADLINE` written before `KVM_SET_LAPIC` is dropped, and an idle vCPU sleeps for a timer it no longer has (5 steps). -/
def Run.msrsFirst : List Label :=
  [.vcpu .a .idle, .vcpu .a .idArm, .vcpu .a .step, .pause, .clone]

theorem breaks_msrsFirst : ¬ after (replay Order.msrsFirst withTimer Run.msrsFirst init) NoLostTimer := by decide

/-- Order: without `KVM_SET_LAPIC` a pending call-function interrupt is not restored (4 steps). -/
def Run.noLapic : List Label :=
  [.vcpu .a .call, .vcpu .a .step, .pause, .clone]

theorem breaks_noLapic : ¬ after (replay Order.noLapic Cfg.fcvm Run.noLapic init) NoStuckCall := by decide

/-- The repaired system on the same runs: every property holds after each (the proofs say so for
every run; these are the particular ones, replayed). -/
theorem design_survives :
    after (replay Design.rules Cfg.fcvm Run.pageReady init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.pageReadyNode init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.snapshot init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.snapshotNode init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.wakeAll init) NoLostKick ∧
    after (replay Design.rules noKvmapf Run.synthHalt init) NoLostKick := by decide

/-- v6.14 on the run that breaks v7.3.3: the kick survives the completion. -/
theorem v614_survives_pageReady :
    after (replay Today.v614 Cfg.fcvm Run.pageReady init) NoLostKick := by decide

/-- Every recorded run with its rule book and configuration, for the C harness
(`lake exe hostops`). -/
def recordedRuns : List (Rules × Cfg × List Label) :=
  [ (Today.v733, Cfg.fcvm, Run.pageReady),
    (Today.v733, Cfg.fcvm, Run.pageReadyNode),
    (Today.v733, Cfg.fcvm, Run.pageReadyCall),
    (Today.v733, Cfg.fcvm, Run.pageReadyIrqsOn),
    (Today.v733, noKvmapf, Run.synthHalt),
    (Today.v733, Cfg.fcvm, Run.snapshot),
    (Today.v733, Cfg.fcvm, Run.snapshotNode),
    (Today.v733, Cfg.fcvm, Run.snapshotCall),
    (Today.v614, Cfg.fcvm, Run.snapshot614),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAll),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAllNode),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAllCall),
    (Partial.v733Patched, Cfg.fcvm, Run.patchedNoKick),
    (Design.rules, writesBack, Run.writeBack),
    (Order.kickFirst, Cfg.fcvm, Run.kickFirst),
    (Order.msrsFirst, withTimer, Run.msrsFirst),
    (Order.noLapic, Cfg.fcvm, Run.noLapic),
    (Design.rules, Cfg.fcvm, Run.pageReady),
    (Design.rules, Cfg.fcvm, Run.snapshot),
    (Design.rules, Cfg.fcvm, Run.wakeAll),
    (Today.v614, Cfg.fcvm, Run.pageReady) ]

end Wake
