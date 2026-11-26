import ResumeVerify.Design

/-!
# Each repair put back, and each thing a VMM must not do

One rule book per mutation of a design. `lake exe resume-breaks mutants` searches each and
fails if one has no break; `MutantBreaks.lean` holds a shortest run to a break for each, checked
by `decide`, and from it that the mutant is not `Sound`. So "this repair is needed" is a
theorem about a reachable state, not a flag compared with itself.
-/

namespace Resume

namespace Mutant

private def ch := Design.cloudHypervisor
private def fc := Design.firecracker

/-- repair 1 undone: the save reads vCPU 0's redistributor and CPU interface for every vCPU -/
def chSlotsVcpu0 : Rules := { ch with name := "mutant-ch-slots-vcpu0", slotsFromRestore := true }
/-- repair 2 undone: the image holds the counter read at the save -/
def chCounterOfSave : Rules := { ch with name := "mutant-ch-counter-of-save", cntAtPause := false }
def chNoFreeze : Rules := { ch with name := "mutant-ch-no-freeze", freeze := false }
def chNoFreezeLoaded : Rules := { ch with name := "mutant-ch-no-freeze-loaded", freezeLoaded := false }
/-- the restore adds the wall time since the pause (upstream Cloud Hypervisor's default) -/
def chAdvance : Rules := { ch with name := "mutant-ch-advance", advance := true }
def chNoOffset : Rules :=
  { ch with name := "mutant-ch-no-offset", restore := ch.restore.filter (· != .offset) }
/-- no offset, and the last vCPU's counter is not replayed -/
def chLastCounterSkipped : Rules :=
  { ch with name := "mutant-ch-last-counter-skipped",
            restore := ch.restore.filter (fun o => o != .offset && o != .cntv true && o != .cntp true) }
/-- repair 3 undone -/
def chLinesNotSaved : Rules := { ch with name := "mutant-ch-lines-not-saved", levelInfo := false }
def chLinesNotRestored : Rules :=
  { ch with name := "mutant-ch-lines-not-restored", restore := ch.restore.filter (· != .level) }
/-- repair 4 undone -/
def chClearSavedBits : Rules := { ch with name := "mutant-ch-clear-saved-bits", clearInverted := false }
/-- repair 5 undone -/
def chNoNestedIcc : Rules := { ch with name := "mutant-ch-no-nested-icc", nestedIcc := false }
/-- `KVM_ARM_VCPU_INIT` after the register replay: it resets the timer's control register -/
def chInitLate : Rules :=
  { ch with name := "mutant-ch-init-late",
            restore := [.offset,
              .create false, .cntp false, .cntv false, .regs false, .init false false, .mp false,
              .create true, .cntp true, .cntv true, .regs true, .init true true, .mp true,
              .gic, .dist, .redist false, .redist true, .icc false, .icc true, .level] }
/-- vCPU 1's registers are not replayed: its timer's control and compare value among them -/
def chNoRegs1 : Rules :=
  { ch with name := "mutant-ch-no-regs1", restore := ch.restore.filter (· != .regs true) }
/-- vCPU 1's CPU interface is not restored: its priority mask stays shut -/
def chNoIcc1 : Rules :=
  { ch with name := "mutant-ch-no-icc1", restore := ch.restore.filter (· != .icc true) }
/-- vCPU 1's `mp_state` is not restored: it stays as `KVM_ARM_VCPU_POWER_OFF` left it -/
def chNoMp1 : Rules :=
  { ch with name := "mutant-ch-no-mp1", restore := ch.restore.filter (· != .mp true) }
/-- the save stops vCPU 1 and does not start it again -/
def chSaveStopsVcpu1 : Rules := { ch with name := "mutant-ch-save-stops-vcpu1", onSave := [.mpStop true] }
/-- the save re-initialises vCPU 0 -/
def chSaveInitsVcpu0 : Rules := { ch with name := "mutant-ch-save-inits-vcpu0", onSave := [.vinit false] }
/-- the save writes vCPU 1's `GICR_ISPENDR0` with zero -/
def chSaveUnpendsVcpu1 : Rules := { ch with name := "mutant-ch-save-unpends-vcpu1", onSave := [.unpend true] }

def fcCounterOfSave : Rules := { fc with name := "mutant-fc-counter-of-save", cntAtPause := false }
def fcNoFreeze : Rules := { fc with name := "mutant-fc-no-freeze", freeze := false }
def fcNoFreezeLoaded : Rules := { fc with name := "mutant-fc-no-freeze-loaded", freezeLoaded := false }
def fcNoOffset : Rules :=
  { fc with name := "mutant-fc-no-offset", restore := fc.restore.filter (· != .offset) }
def fcLinesNotSaved : Rules := { fc with name := "mutant-fc-lines-not-saved", levelInfo := false }
def fcLinesNotRestored : Rules :=
  { fc with name := "mutant-fc-lines-not-restored", restore := fc.restore.filter (· != .level) }
def fcClearSavedBits : Rules := { fc with name := "mutant-fc-clear-saved-bits", clearInverted := false }
def fcNoNestedIcc : Rules := { fc with name := "mutant-fc-no-nested-icc", nestedIcc := false }
/-- the resume stops vCPU 1 -/
def fcResumeStopsVcpu1 : Rules := { fc with name := "mutant-fc-resume-stops-vcpu1", onResume := [.mpStop true] }

end Mutant

open Mutant in
def mutants : List Rules :=
  [chSlotsVcpu0, chCounterOfSave, chNoFreeze, chNoFreezeLoaded, chAdvance, chNoOffset,
   chLastCounterSkipped, chLinesNotSaved, chLinesNotRestored, chClearSavedBits, chNoNestedIcc,
   chInitLate, chNoRegs1, chNoIcc1, chNoMp1, chSaveStopsVcpu1, chSaveInitsVcpu0, chSaveUnpendsVcpu1,
   fcCounterOfSave, fcNoFreeze, fcNoFreezeLoaded, fcNoOffset, fcLinesNotSaved, fcLinesNotRestored,
   fcClearSavedBits, fcNoNestedIcc, fcResumeStopsVcpu1]

end Resume
