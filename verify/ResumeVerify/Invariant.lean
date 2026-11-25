import ResumeVerify.Spec
import ResumeVerify.Properties

set_option linter.unusedSimpArgs false

/-!
# What the proofs need of a VMM, and the invariant

`Sound R` is what a rule book must do for the theorems of `Proofs.lean`: the repairs as fields,
that what it writes to a paused source takes nothing from it (`Quiet`), and that its restore,
whatever its order, builds exactly the VM the image describes (`rebuilt`). `Inv` is the
invariant: each vCPU's interrupt state agrees with where its guest CPU is, the device's with
the device, the counter with the time the guest has run, and an image is the save of the
paused VM it was taken from.
-/

namespace Resume

/-! ## The VM an image describes -/

def vcpuOf (s : Slot) : Vcpu :=
  { g := s.g, call := s.call, gOff := s.gOff, thr := .user, stopped := s.stopped,
    pmr := s.pmr, grp := s.grp, ap := s.ap,
    tmr := { en := s.tmr.en, latch := s.tmr.latch, line := s.tmr.line, act := s.tmr.act },
    sgi := { en := s.sgi.en, latch := s.sgi.latch, line := false, act := s.sgi.act },
    tEn := s.tEn, cv := s.cv, owed := s.owed, l2 := s.l2, ichv := s.ichv }

def rebuilt (i : Image) : VM :=
  { c0 := vcpuOf i.s0, c1 := vcpuOf i.s1,
    spi := { en := i.spi.en, latch := i.spi.latch, line := i.spi.line, act := i.spi.act },
    dev := i.dev, gicEn := i.gicEn, ctr := i.s0.cnt, offFlag := true, ideal := i.ideal,
    restored := true, clone := true }

/-! ## The invariant -/

/-- A vCPU's interrupt state agrees with where its guest CPU is. -/
structure VOk (c : Vcpu) : Prop where
  tact : c.tmr.act = (c.g == .hdl .tmr)
  sact : c.sgi.act = (c.g == .hdl .sgi)
  ap : c.ap = !(c.g == .run || c.g == .idle)
  call : c.call = (c.sgi.latch || c.sgi.act)
  ten : c.tmr.en = !c.gOff
  sen : c.sgi.en = !c.gOff
  pmr : c.pmr = true
  grp : c.grp = true
  tl : c.tmr.latch = false
  sl : c.sgi.line = false
  mp : c.stopped = c.gOff
  /-- one cross-call at a time: the SGI is not pending again while it is handled -/
  once : (c.sgi.act && c.sgi.latch) = false
  /-- an owed tick has its timer armed -/
  owe : (c.owed && !c.tEn) = false
  /-- a loaded nested vCPU has its mask -/
  ich : c.ichv = c.l2
  /-- an offline CPU has no timer armed -/
  off : (c.gOff && c.tEn) = false
  /-- a blocked thread's guest CPU is in the idle loop -/
  blk : (c.thr == .blocked && c.g != .idle) = false

structure MOk (C : Cfg) (m : VM) : Prop where
  v0 : VOk m.c0
  v1 : VOk m.c1
  pact : m.spi.act = (m.c0.g == .hdl .spi)
  p1 : (m.c1.g == .hdl .spi) = false
  /-- a waiting edge-triggered device that is not being handled has its latch. The latch may
  also be set with no event behind it: a VMM may raise the interrupt at a resume -/
  platch : (m.dev && !m.spi.act && !C.spiLevel && !m.spi.latch) = false
  pline : m.spi.line = (m.dev && C.spiLevel)
  gic : m.gicEn = true
  pen : m.spi.en = true

/-- A list of requests on a paused VM that takes nothing from it: the invariant survives, the
counter is as it was, and `undisturbed` holds. -/
def Quiet (C : Cfg) (ops : List SrcOp) : Prop :=
  ∀ m, MOk C m →
    MOk C (srcOps C ops m) ∧ (srcOps C ops m).ctr = m.ctr ∧ undisturbed m (srcOps C ops m) = true

/-- What the proofs need of a rule book. -/
structure Sound (R : Rules) : Prop where
  /-- repair 1: each slot is read from its own vCPU -/
  ownSlots : R.slotsFromRestore = false
  /-- repair 2: the image holds the counter of the pause -/
  cntAtPause : R.cntAtPause = true
  /-- repair 2: a resume takes out the time paused -/
  freeze : R.freeze = true
  /-- repair 2: and the time between a load and its resume -/
  freezeLoaded : R.freezeLoaded = true
  /-- the restore does not add wall time to the counter -/
  noAdvance : R.advance = false
  /-- repair 3: the lines of level interrupts are saved -/
  levelInfo : R.levelInfo = true
  /-- repair 5: a guest hypervisor's CPU-interface registers are saved -/
  nestedIcc : R.nestedIcc = true
  /-- what the VMM writes to a paused source at the save takes nothing from it -/
  quietSave : ∀ C, Quiet C R.onSave
  /-- nor what it writes at a resume -/
  quietResume : ∀ C, Quiet C R.onResume
  /-- the restore, in whatever order, builds the VM the image describes -/
  builds : ∀ (C : Cfg) (i : Image), build R C i = rebuilt i

structure Inv (R : Rules) (C : Cfg) (s : State) : Prop where
  m : MOk C s.vm
  run : s.ph = .running → s.vm.ctr = .sync { t := 0, o := 0 }
  held : s.ph ≠ .running → s.vm.ctr = .sync { t := s.since, o := 0 }
  calm : s.disturbed = false
  img : ∀ i, s.img = some i → s.ph = .paused ∧ i = save R s

theorem VM.get_false (m : VM) : m.get false = m.c0 := rfl
theorem VM.get_true (m : VM) : m.get true = m.c1 := rfl
theorem VM.set_false (m : VM) (c : Vcpu) : m.set false c = { m with c0 := c } := rfl
theorem VM.set_true (m : VM) (c : Vcpu) : m.set true c = { m with c1 := c } := rfl

/-! ## The guest's steps -/

theorem ite_none {α : Type} {p : Prop} [Decidable p] {x : Option α} {y : α}
    (h : (if p then none else x) = some y) : ¬p ∧ x = some y := by
  by_cases hp : p <;> simp_all

set_option hygiene false in
/-- Peel the guards off the hypothesis `hs` of a step. -/
macro "peel" : tactic =>
  `(tactic| repeat (have hh := ite_none hs; clear hs; obtain ⟨_, hs⟩ := hh))

set_option hygiene false in
/-- Split the invariant into its parts. -/
macro "parts" : tactic =>
  `(tactic| refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
               ?_, ?_, ?_, ?_, ?_, ?_⟩)

set_option hygiene false in
/-- Close a step of vCPU 0 or 1: the step taken, then each part of the invariant. -/
macro "finish" : tactic =>
  `(tactic| (simp only [Option.some.injEq] at hs; subst hs
             parts <;>
             simp_all (config := { decide := true }) [deliverable, pend, VM.get_false, VM.get_true] <;>
             (cases hd : m.dev <;> cases hl : C.spiLevel <;> simp_all)))

end Resume
