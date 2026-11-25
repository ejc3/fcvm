import ResumeVerify.Spec
import ResumeVerify.Today
import ResumeVerify.Design
import ResumeVerify.Properties

/-!
# What the proofs need of a VMM, the invariant, and that every step keeps it

`Sound R` is what a rule book must do for the theorems of `Proofs.lean`: the five repairs as
fields, and that its restore, whatever its order, builds exactly the VM the image describes
(`rebuilt`). `Inv` is the invariant: each vCPU's interrupt state agrees with where its guest
CPU is, the device's with the device, the counter with the time the guest has run, and an
image is the save of the paused VM it was taken from.
-/

namespace Resume

/-! ## The VM an image describes -/

def vcpuOf (s : Slot) : Vcpu :=
  { g := s.g, call := s.call, gOff := s.gOff, thr := .user, stopped := s.stopped,
    pmr := s.pmr, grp := s.grp, ap := s.ap,
    tmr := { en := s.tmr.en, latch := s.tmr.latch, line := s.tmr.line, act := s.tmr.act },
    sgi := { en := s.sgi.en, latch := s.sgi.latch, line := false, act := s.sgi.act },
    tEn := s.tEn, cv := s.cv, l2 := s.l2, hw := false }

def rebuilt (i : Image) : VM :=
  { c0 := vcpuOf i.s0, c1 := vcpuOf i.s1,
    spi := { en := i.spi.en, latch := i.spi.latch, line := i.spi.line, act := i.spi.act },
    dev := i.dev, gicEn := i.gicEn, ctr := i.s0.cnt, offFlag := true, ideal := i.ideal,
    restored := true }

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
  /-- repair 3: the lines of level interrupts are saved -/
  levelInfo : R.levelInfo = true
  /-- the restore, in whatever order, builds the VM the image describes -/
  builds : ∀ (C : Cfg) (i : Image), build R C i = rebuilt i

/-! ## The invariant -/

/-- A vCPU's interrupt state agrees with where its guest CPU is. -/
structure VOk (c : Vcpu) : Prop where
  tact : c.tmr.act = (c.g == .hdl .tmr)
  sact : c.sgi.act = (c.g == .hdl .sgi)
  ap : c.ap = !(c.g == .run || c.g == .idle)
  call : c.call = (c.sgi.latch || c.sgi.act)
  ten : c.tmr.en = true
  sen : c.sgi.en = true
  pmr : c.pmr = true
  grp : c.grp = true
  tl : c.tmr.latch = false
  sl : c.sgi.line = false
  mp : c.stopped = c.gOff
  /-- one cross-call at a time: the SGI is not pending again while it is handled -/
  once : (c.sgi.act && c.sgi.latch) = false

structure MOk (C : Cfg) (m : VM) : Prop where
  v0 : VOk m.c0
  v1 : VOk m.c1
  pact : m.spi.act = (m.c0.g == .hdl .spi)
  p1 : (m.c1.g == .hdl .spi) = false
  platch : m.spi.latch = (m.dev && !m.spi.act && !C.spiLevel)
  pline : m.spi.line = (m.dev && C.spiLevel)
  gic : m.gicEn = true
  pen : m.spi.en = true

structure Inv (R : Rules) (C : Cfg) (s : State) : Prop where
  m : MOk C s.vm
  run : s.ph = .running → s.vm.ctr = .sync { t := 0, o := 0 }
  held : s.ph ≠ .running → s.vm.ctr = .sync { t := s.since, o := 0 }
  kept : s.ph = .paused → s.kept = some (s.vm.c0.keep, s.vm.c1.keep)
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
/-- Close a step of vCPU 0 or 1: the step taken, then each part of the invariant. -/
macro "finish" : tactic =>
  `(tactic| (simp only [Option.some.injEq] at hs; subst hs
             refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
               ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
             simp_all (config := { decide := true }) [deliverable, pend, VM.get_false, VM.get_true]))

set_option linter.unusedSimpArgs false in
set_option maxHeartbeats 1600000 in
theorem guest_ok {C : Cfg} {m m' : VM} {v : Vid} {a : GAct} (h : MOk C m)
    (hs : guest C v a m = some m') : MOk C m' := by
  obtain ⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩, ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩,
    pa, p1, pl, pn, gi, pe⟩ := h
  cases v
  · rcases hg : m.c0.g with _ | _ | (_ | _ | _) <;> cases a
    case ack i | ack i | ack i | ack i | ack i =>
      all_goals cases i <;>
        simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
          Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish
    all_goals
      simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
        Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish
  · rcases hg : m.c1.g with _ | _ | (_ | _ | _) <;> cases a
    case ack i | ack i | ack i | ack i | ack i =>
      all_goals cases i <;>
        simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
          Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish
    all_goals
      simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
        Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish

end Resume
