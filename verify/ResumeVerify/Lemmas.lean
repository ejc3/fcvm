import ResumeVerify.Invariant

set_option linter.unusedSimpArgs false

/-!
# Every other step keeps the invariant

KVM's steps while the guest runs, the device, and the VMM's: pause, save, resume, restore.
-/

namespace Resume

/-! ## KVM's steps while the guest runs, and the device -/

theorem fire_ok {C : Cfg} {m m' : VM} {v : Vid} (h : MOk C m) (hs : fire m v = some m') :
    MOk C m' ∧ m'.ctr = m.ctr := by
  obtain ⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩, ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩,
    pa, p1, pl, pn, gi, pe, pd⟩ := h
  cases v <;>
    simp only [fire, VM.get_false, VM.get_true, VM.set_false, VM.set_true] at hs <;> peel <;>
    simp only [Option.some.injEq] at hs <;> subst hs <;>
    exact ⟨⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩,
      ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩, pa, p1, pl, pn, gi, pe, pd⟩, rfl⟩

theorem wake_ok {C : Cfg} {m m' : VM} {v : Vid} (h : MOk C m) (hs : wake C m v = some m') :
    MOk C m' ∧ m'.ctr = m.ctr := by
  obtain ⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩, ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩,
    pa, p1, pl, pn, gi, pe, pd⟩ := h
  cases v <;>
    simp only [wake, VM.get_false, VM.get_true, VM.set_false, VM.set_true] at hs <;> peel <;>
    simp only [Option.some.injEq] at hs <;> subst hs <;>
    exact ⟨⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩,
      ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩, pa, p1, pl, pn, gi, pe, pd⟩, rfl⟩

theorem devRaise_ok {C : Cfg} {m m' : VM} (h : MOk C m) (hs : devRaise C m = some m') :
    MOk C m' ∧ m'.ctr = m.ctr ∧ m'.c0 = m.c0 ∧ m'.c1 = m.c1 := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe, pd⟩ := h
  simp only [devRaise] at hs
  peel
  simp only [Option.some.injEq] at hs
  subst hs
  refine ⟨⟨v0, v1, ?_, p1, ?_, ?_, gi, ?_, ?_⟩, rfl, rfl, rfl⟩ <;>
    cases hl : C.spiLevel <;> simp_all

/-! ## The save and the restore -/

theorem enter_ok {m : VM} {c : Vcpu} (h : VOk c) : VOk (enter m c) :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8, h.9, h.10, h.11, h.12⟩

theorem pause_ok {c : Vcpu} (h : VOk c) : VOk (pauseVcpu c) :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8, h.9, h.10, h.11, h.12⟩

/-- With each slot read from its own vCPU and the lines saved, the VM built from the save of
a VM is that VM, its threads in userspace and its timer interrupts not yet mapped. -/
theorem rebuilt_ok {R : Rules} {C : Cfg} {s : State} (hR : Sound R) (h : MOk C s.vm) :
    MOk C (rebuilt (save R s)) := by
  obtain ⟨⟨a0, b0, c0, d0, e0, f0, g0, h0, i0, j0, k0, l0⟩, ⟨a1, b1, c1, d1, e1, f1, g1, h1, i1, j1, k1, l1⟩,
    pa, p1, pl, pn, gi, pe, pd⟩ := h
  have hown := hR.ownSlots
  have hlev := hR.levelInfo
  refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
    ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
    simp_all [rebuilt, save, vcpuOf, source, VM.get]

theorem save_since {R : Rules} {s : State} {e : Err} (hR : Sound R)
    (hc : s.vm.ctr = .sync { t := s.since, o := e.o }) :
    (save R s).s0.cnt = .sync { t := 0, o := e.o } := by
  simp [save, hR.cntAtPause, hc]

end Resume
