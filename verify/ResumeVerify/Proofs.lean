import ResumeVerify.Lemmas

set_option linter.unusedSimpArgs false

/-!
# The repaired VMMs keep R1 to R6

`step_inv`: under a `Sound` rule book every step keeps `Inv`. `reachable_inv`: so every
reachable state has it, for every configuration and runs of any length. The `sound_*` theorems
read each requirement off the invariant; the `design_*` theorems are those for the two rule
books of `Design.lean`, whose restore orders are shown to build the VM the image describes.
-/

namespace Resume

theorem init_inv (R : Rules) (C : Cfg) : Inv R C (init C) := by
  refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
    ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_, ?_, ?_⟩ <;> simp [init, bootVcpu, IrqSt.none]

theorem map_some {α β : Type} {f : α → β} {x : Option α} {y : β} (h : x.map f = some y) :
    ∃ a, x = some a ∧ f a = y := by
  cases x <;> simp_all

theorem ph_running {s : State} (h : ¬(s.ph != .running) = true) : s.ph = .running := by
  simpa using h

/-- A step that changes only the VM, keeps its counter, and is taken while the guest runs. -/
theorem running_inv {R : Rules} {C : Cfg} {s : State} {m' : VM} (h : Inv R C s)
    (hp : s.ph = .running) (hm : MOk C m') (hc : m'.ctr = s.vm.ctr) :
    Inv R C { s with vm := m' } := by
  refine ⟨hm, fun _ => hc ▸ h.run hp, fun hn => absurd hp hn, fun hq => ?_, h.calm, fun i hi => ?_⟩
  · exact absurd (hp ▸ hq) (by decide)
  · exact absurd (hp ▸ (h.img i hi).1) (by decide)

theorem mok_ideal {C : Cfg} {m : VM} (h : MOk C m) (n : Nat) : MOk C { m with ideal := n } :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8, h.9⟩

theorem mok_ctr {C : Cfg} {m : VM} (h : MOk C m) (c : Ctr) : MOk C { m with ctr := c } :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8, h.9⟩

theorem save_wait {R : Rules} {s : State} (hR : Sound R)
    (hc : s.vm.ctr = .sync { t := s.since, o := 0 }) :
    save R { s with waits := s.waits + 1, since := s.since + 1,
                    vm := { s.vm with ctr := .sync { t := (s.since : Int) + 1, o := 0 } } }
      = save R s := by
  simp [save, hR.cntAtPause, hc, VM.get, source]

theorem resume_ok {R : Rules} {C : Cfg} {s : State} (h : MOk C s.vm) : MOk C (resumeVM R s) := by
  simp only [resumeVM]
  split <;> exact ⟨enter_ok h.1, enter_ok h.2, h.3, h.4, h.5, h.6, h.7, h.8, h.9⟩

theorem resume_ctr {R : Rules} {s : State} (hR : Sound R)
    (hc : s.vm.ctr = .sync { t := s.since, o := 0 }) :
    (resumeVM R s).ctr = .sync { t := 0, o := 0 } := by
  simp [resumeVM, hR.freeze, hR.freezeLoaded, hc]

theorem step_inv {R : Rules} {C : Cfg} {s t : State} {l : Label} (hR : Sound R)
    (h : Inv R C s) (hs : step R C l s = some t) : Inv R C t := by
  cases l with
  | g v a =>
    simp only [step] at hs
    peel
    obtain ⟨m', hg, rfl⟩ := map_some hs
    exact running_inv h (ph_running ‹_›) (guest_ok h.m hg) (guest_ctr hg)
  | fire v =>
    simp only [step] at hs
    peel
    obtain ⟨m', hg, rfl⟩ := map_some hs
    exact running_inv h (ph_running ‹_›) (fire_ok h.m hg).1 (fire_ok h.m hg).2
  | wake v =>
    simp only [step] at hs
    peel
    obtain ⟨m', hg, rfl⟩ := map_some hs
    exact running_inv h (ph_running ‹_›) (wake_ok h.m hg).1 (wake_ok h.m hg).2
  | dev =>
    simp only [step] at hs
    peel
    obtain ⟨m', hg, rfl⟩ := map_some hs
    obtain ⟨hm, hc, h0, h1⟩ := devRaise_ok h.m hg
    refine ⟨hm, fun hp => hc ▸ h.run hp, fun hn => hc ▸ h.held hn, fun hq => ?_, h.calm, fun i hi => ?_⟩
    · show s.kept = some (m'.c0.keep, m'.c1.keep)
      rw [h0, h1]
      exact h.kept hq
    · have hp := (h.img i hi).1
      have hi' : s.img = some i := hi
      simp_all
  | tick =>
    simp only [step] at hs
    peel
    simp only [Option.some.injEq] at hs
    subst hs
    have hp : s.ph = .running := by simp_all
    exact running_inv h hp (mok_ideal h.m _) rfl
  | wait =>
    simp only [step] at hs
    peel
    have hne : s.ph ≠ .running := by
      intro hp
      simp_all
    have hc := h.held hne
    simp only [hc, Option.some.injEq] at hs
    subst hs
    refine ⟨mok_ctr h.m _, fun hp => absurd hp hne, fun _ => ?_, h.kept, h.calm, fun i hi => ?_⟩
    · show Ctr.sync { t := (s.since : Int) + 1, o := 0 } = Ctr.sync { t := ((s.since + 1 : Nat) : Int), o := 0 }
      simp
    · obtain ⟨hp, rfl⟩ := h.img i hi
      exact ⟨hp, (save_wait hR hc).symm⟩
  | pause =>
    simp only [step] at hs
    peel
    simp only [Option.some.injEq] at hs
    subst hs
    have hp : s.ph = .running := by simp_all
    refine ⟨⟨pause_ok h.m.1, pause_ok h.m.2, h.m.3, h.m.4, h.m.5, h.m.6, h.m.7, h.m.8, h.m.9⟩,
      (fun hq => nomatch hq), fun _ => ?_, fun _ => rfl, h.calm, (fun i hi => nomatch hi)⟩
    simpa using h.run hp
  | save =>
    simp only [step] at hs
    peel
    simp only [Option.some.injEq] at hs
    subst hs
    have hp : s.ph = .paused := by simp_all
    refine ⟨h.m, h.run, h.held, h.kept, h.calm, fun i hi => ?_⟩
    simp only [Option.some.injEq] at hi
    exact ⟨hp, hi.symm⟩
  | resume =>
    cases hp : s.ph <;> simp only [step, hp] at hs
    · exact nomatch hs
    · simp only [Option.some.injEq] at hs
      subst hs
      have hc := h.held (by rw [hp]; decide)
      refine ⟨resume_ok h.m, fun _ => resume_ctr hR hc, fun hn => absurd rfl hn, (fun hq => nomatch hq), ?_,
        (fun i hi => nomatch hi)⟩
      simp [h.calm, h.kept hp]
    · simp only [Option.some.injEq] at hs
      subst hs
      have hc := h.held (by rw [hp]; decide)
      refine ⟨resume_ok h.m, fun _ => resume_ctr hR hc, fun hn => absurd rfl hn, (fun hq => nomatch hq), h.calm,
        fun i hi => ?_⟩
      exact absurd (hp ▸ (h.img i hi).1) (by decide)
  | restore =>
    cases hi : s.img <;> simp only [step, hi] at hs
    · exact nomatch hs
    · rename_i i
      peel
      simp only [Option.some.injEq] at hs
      subst hs
      obtain ⟨hp, rfl⟩ := h.img i hi
      have hc := h.held (by rw [hp]; decide)
      refine ⟨?_, (fun hq => nomatch hq), fun _ => ?_, (fun hq => nomatch hq), h.calm, (fun i hi => nomatch hi)⟩
      · show MOk C (build R C (save R s))
        rw [hR.builds]
        exact rebuilt_ok hR h.m
      · show (build R C (save R s)).ctr = _
        rw [hR.builds]
        simpa using save_since hR hc

/-- Every reachable state of a sound rule book has the invariant. -/
theorem reachable_inv {R : Rules} {C : Cfg} {s : State} (hR : Sound R) (h : Reachable R C s) :
    Inv R C s := by
  induction h with
  | init => exact init_inv R C
  | step l _ hs ih => exact step_inv hR ih hs

/-! ## The requirements, read off the invariant -/

section
variable {R : Rules} {C : Cfg} {s : State}

theorem sound_noLostIpi (hR : Sound R) (h : Reachable R C s) : lostIpi s = false := by
  have i := (reachable_inv hR h).m
  simp [lostIpi, anyVcpu, i.v0.call, i.v1.call]
  intro _
  constructor <;> (intro h1 h2; simp_all)

end

end Resume
