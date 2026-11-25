import ResumeVerify.Invariant
import ResumeVerify.Guest0
import ResumeVerify.Guest1

set_option linter.unusedSimpArgs false

/-!
# Every other step keeps the invariant

KVM's steps while the guest runs, the device, what a VMM writes to a paused source, and the
round trip: the VM a sound restore builds from the save of a VM is that VM.
-/

namespace Resume

/-! ## The guest's steps -/

theorem guest_ok {C : Cfg} {m m' : VM} {v : Vid} {a : GAct} (h : MOk C m)
    (hs : guest C v a m = some m') : MOk C m' := by
  cases v
  · exact guest_ok0 h hs
  · exact guest_ok1 h hs

theorem guest_ctr {C : Cfg} {m m' : VM} {v : Vid} {a : GAct} (hs : guest C v a m = some m') :
    m'.ctr = m.ctr := by
  cases v
  · exact guest_ctr0 hs
  · exact guest_ctr1 hs

/-! ## KVM's steps while the guest runs, and the device -/

theorem fire_ok {C : Cfg} {m m' : VM} {v : Vid} (h : MOk C m) (hs : fire m v = some m') :
    MOk C m' ∧ m'.ctr = m.ctr := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  cases v0
  cases v1
  cases v <;>
    simp only [fire, VM.get_false, VM.get_true, VM.set_false, VM.set_true] at hs <;> peel <;>
    simp only [Option.some.injEq] at hs <;> subst hs <;>
    refine ⟨?_, rfl⟩ <;> parts <;> simp_all

theorem wake_ok {C : Cfg} {m m' : VM} {v : Vid} (h : MOk C m) (hs : wake C m v = some m') :
    MOk C m' ∧ m'.ctr = m.ctr := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  cases v0
  cases v1
  cases v <;>
    simp only [wake, VM.get_false, VM.get_true, VM.set_false, VM.set_true] at hs <;> peel <;>
    simp only [Option.some.injEq] at hs <;> subst hs <;>
    refine ⟨?_, rfl⟩ <;> parts <;> simp_all

theorem devRaise_ok {C : Cfg} {m m' : VM} (h : MOk C m) (hs : devRaise C m = some m') :
    MOk C m' ∧ m'.ctr = m.ctr ∧ m'.c0 = m.c0 ∧ m'.c1 = m.c1 := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  simp only [devRaise] at hs
  peel
  simp only [Option.some.injEq] at hs
  subst hs
  refine ⟨⟨v0, v1, ?_, p1, ?_, ?_, gi, ?_⟩, rfl, rfl, rfl⟩ <;>
    cases hl : C.spiLevel <;> simp_all

/-! ## Entering and leaving `KVM_RUN` -/

theorem enter_ok {m : VM} {c : Vcpu} (h : VOk c) : VOk (enter m c) := by
  cases h
  constructor <;> simp_all [enter]

theorem pause_ok {c : Vcpu} (h : VOk c) : VOk (pauseVcpu c) := by
  cases h
  constructor <;> simp_all [pauseVcpu]

/-! ## What a VMM writes to a paused source -/

theorem undisturbed_refl (m : VM) : undisturbed m m = true := by
  simp [undisturbed]

theorem quiet_nil (C : Cfg) : Quiet C [] := fun m h => ⟨h, rfl, undisturbed_refl m⟩

/-- The device publishing an event of its own, with its interrupt, takes nothing from the
source: Firecracker's vsock reset at the save. -/
theorem quiet_devEvent (C : Cfg) : Quiet C [.devEvent] := by
  intro m h
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  simp only [srcOps, List.foldl, srcApply]
  split
  · exact ⟨⟨v0, v1, pa, p1, pl, pn, gi, pe⟩, rfl, undisturbed_refl m⟩
  · refine ⟨⟨v0, v1, ?_, p1, ?_, ?_, gi, ?_⟩, ?_, ?_⟩ <;>
      cases hl : C.spiLevel <;> simp_all [undisturbed]

/-- Raising the device's interrupt with nothing behind it takes nothing from the source:
Cloud Hypervisor's resume. -/
theorem quiet_kick (C : Cfg) : Quiet C [.kick] := by
  intro m h
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  simp only [srcOps, List.foldl, srcApply]
  split
  · exact ⟨⟨v0, v1, pa, p1, pl, pn, gi, pe⟩, rfl, undisturbed_refl m⟩
  · refine ⟨⟨v0, v1, ?_, p1, ?_, ?_, gi, ?_⟩, ?_, ?_⟩ <;> simp_all [undisturbed]

/-! ## The save and the restore -/

/-- A vCPU out of `KVM_RUN`. -/
def parked (c : Vcpu) : Vcpu := { c with thr := .user }

/-- The round trip. For a sound rule book, the VM described by the save of a VM is that VM:
every interrupt bit, every timer register, where each guest CPU is, what the device waits for.
What differs is what a new VM differs in: its threads have not entered `KVM_RUN`, its counter
offset was set by `KVM_ARM_SET_COUNTER_OFFSET`, and its counter is the image's. -/
theorem roundTrip {R : Rules} {C : Cfg} {s : State} (hR : Sound R) (h : MOk C s.vm) :
    rebuilt (save R s) =
      { s.vm with c0 := parked s.vm.c0, c1 := parked s.vm.c1, ctr := (save R s).s0.cnt,
                  offFlag := true, restored := true, clone := true } := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  have s0 := v0.sl
  have s1 := v1.sl
  have i0 := v0.ich
  have i1 := v1.ich
  rcases hm : s.vm with ⟨c0, c1, spi, dev, gicEn, ctr, offFlag, ideal, restored, clone⟩
  rw [hm] at s0 s1 i0 i1
  rcases c0 with ⟨g0, call0, gOff0, thr0, st0, pmr0, grp0, ap0, ⟨te0, tl0, tn0, ta0⟩, ⟨se0, sl0, sn0, sa0⟩, tEn0, cv0, ow0, l20, ic0⟩
  rcases c1 with ⟨g1, call1, gOff1, thr1, st1, pmr1, grp1, ap1, ⟨te1, tl1, tn1, ta1⟩, ⟨se1, sl1, sn1, sa1⟩, tEn1, cv1, ow1, l21, ic1⟩
  rcases spi with ⟨pe', pl', pn', pa'⟩
  simp_all [rebuilt, save, vcpuOf, parked, source, VM.get, hR.ownSlots, hR.levelInfo, hR.nestedIcc]

theorem parked_ok {c : Vcpu} (h : VOk c) : VOk (parked c) := by
  cases h
  constructor <;> simp_all [parked]

/-- So the VM built from the save of a VM satisfies the invariant. -/
theorem rebuilt_ok {R : Rules} {C : Cfg} {s : State} (hR : Sound R) (h : MOk C s.vm) :
    MOk C (rebuilt (save R s)) := by
  rw [roundTrip hR h]
  exact ⟨parked_ok h.v0, parked_ok h.v1, h.pact, h.p1, h.platch, h.pline, h.gic, h.pen⟩

theorem save_since {R : Rules} {s : State} (hR : Sound R)
    (hc : s.vm.ctr = .sync { t := s.since, o := 0 }) :
    (rebuilt (save R s)).ctr = .sync { t := 0, o := 0 } := by
  simp [rebuilt, save, hR.cntAtPause, hc]

end Resume
