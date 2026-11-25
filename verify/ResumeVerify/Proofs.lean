import ResumeVerify.Lemmas
import ResumeVerify.Today
import ResumeVerify.Design
import ResumeVerify.Configs
import ResumeVerify.Mutants

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
  refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
    ?_, ?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_, ?_⟩ <;> simp [init, bootVcpu, IrqSt.none]

theorem map_some {α β : Type} {f : α → β} {x : Option α} {y : β} (h : x.map f = some y) :
    ∃ a, x = some a ∧ f a = y := by
  cases x <;> simp_all

theorem ph_running {s : State} (h : ¬(s.ph != .running) = true) : s.ph = .running := by
  simpa using h

/-- A step that changes only the VM, keeps its counter, and is taken while the guest runs. -/
theorem running_inv {R : Rules} {C : Cfg} {s : State} {m' : VM} (h : Inv R C s)
    (hp : s.ph = .running) (hm : MOk C m') (hc : m'.ctr = s.vm.ctr) :
    Inv R C { s with vm := m' } := by
  refine ⟨hm, fun _ => hc ▸ h.run hp, fun hn => absurd hp hn, h.calm, fun i hi => ?_⟩
  exact absurd (hp ▸ (h.img i hi).1) (by decide)

theorem mok_ideal {C : Cfg} {m : VM} (h : MOk C m) (n : Nat) : MOk C { m with ideal := n } :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8⟩

theorem mok_ctr {C : Cfg} {m : VM} (h : MOk C m) (c : Ctr) : MOk C { m with ctr := c } :=
  ⟨h.1, h.2, h.3, h.4, h.5, h.6, h.7, h.8⟩

theorem save_wait {R : Rules} {s : State} (hR : Sound R)
    (hc : s.vm.ctr = .sync { t := s.since, o := 0 }) :
    save R { s with waits := s.waits + 1, since := s.since + 1,
                    vm := { s.vm with ctr := .sync { t := (s.since : Int) + 1, o := 0 } } }
      = save R s := by
  simp [save, hR.cntAtPause, hc, VM.get, source]

theorem resume_ok {R : Rules} {C : Cfg} {s : State} (h : MOk C s.vm) : MOk C (resumeVM R s) := by
  simp only [resumeVM]
  repeat' split
  all_goals exact ⟨enter_ok h.1, enter_ok h.2, h.3, h.4, h.5, h.6, h.7, h.8⟩

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
    refine ⟨hm, fun hp => hc ▸ h.run hp, fun hn => hc ▸ h.held hn, h.calm, fun i hi => ?_⟩
    have hp := (h.img i hi).1
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
    refine ⟨mok_ctr h.m _, fun hp => absurd hp hne, fun _ => ?_, h.calm, fun i hi => ?_⟩
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
    refine ⟨⟨pause_ok h.m.1, pause_ok h.m.2, h.m.3, h.m.4, h.m.5, h.m.6, h.m.7, h.m.8⟩,
      (fun hq => nomatch hq), fun _ => ?_, h.calm, (fun i hi => nomatch hi)⟩
    simpa using h.run hp
  | save =>
    simp only [step] at hs
    peel
    simp only [Option.some.injEq] at hs
    subst hs
    have hp : s.ph = .paused := by simp_all
    obtain ⟨hm, hc, hu⟩ := hR.quietSave C s.vm h.m
    refine ⟨hm, fun hq => absurd (hp ▸ hq) (by decide), fun hn => hc ▸ h.held hn, ?_, fun i hi => ?_⟩
    · simp [h.calm, hu]
    · simp only [Option.some.injEq] at hi
      exact ⟨hp, hi.symm⟩
  | resume =>
    cases hp : s.ph <;> simp only [step, hp] at hs
    · exact nomatch hs
    · simp only [Option.some.injEq] at hs
      subst hs
      have hc := h.held (by rw [hp]; decide)
      obtain ⟨hm, hc', hu⟩ := hR.quietResume C s.vm h.m
      refine ⟨resume_ok (s := { s with vm := srcOps C R.onResume s.vm, ph := .paused }) hm,
        fun _ => resume_ctr hR (s := { s with vm := srcOps C R.onResume s.vm, ph := .paused }) (hc' ▸ hc),
        fun hn => absurd rfl hn, ?_, (fun i hi => nomatch hi)⟩
      simp [h.calm, hu]
    · simp only [Option.some.injEq] at hs
      subst hs
      have hc := h.held (by rw [hp]; decide)
      obtain ⟨hm, hc', _⟩ := hR.quietResume C s.vm h.m
      refine ⟨resume_ok (s := { s with vm := srcOps C R.onResume s.vm, ph := .loaded }) hm,
        fun _ => resume_ctr hR (s := { s with vm := srcOps C R.onResume s.vm, ph := .loaded }) (hc' ▸ hc),
        fun hn => absurd rfl hn, h.calm, fun i hi => ?_⟩
      exact absurd (hp ▸ (h.img i hi).1) (by decide)
  | restore =>
    cases hi : s.img <;> simp only [step, hi] at hs
    · exact nomatch hs
    · rename_i i
      peel
      simp only [hR.noAdvance, Option.some.injEq] at hs
      subst hs
      obtain ⟨hp, rfl⟩ := h.img i hi
      have hc := h.held (by rw [hp]; decide)
      refine ⟨?_, (fun hq => nomatch hq), fun _ => ?_, h.calm, (fun i hi => nomatch hi)⟩
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

/-- R1: no cross-call is owed to a CPU whose SGI is neither pending nor active. -/
theorem sound_noLostIpi (hR : Sound R) (h : Reachable R C s) : lostIpi s = false := by
  have i := (reachable_inv hR h).m
  simp [lostIpi, anyVcpu, i.v0.call, i.v1.call]
  intro _
  constructor <;> (intro h1 h2; simp_all)

/-- R1: the device never waits with its interrupt neither pending nor active. -/
theorem sound_noLostDeviceIrq (hR : Sound R) (h : Reachable R C s) : lostDeviceIrq C s = false := by
  have i := (reachable_inv hR h).m
  have h1 := i.platch
  have h2 := i.pline
  cases hd : s.vm.dev <;> cases hl : C.spiLevel <;> cases ha : s.vm.spi.act <;>
    simp_all [lostDeviceIrq, pend]

/-- R1: an owed tick still has its timer armed. With `sound_clockRight` the timer fires when
it should; `roundTrip` (`Lemmas.lean`) is the stronger statement, that a restore changes no
timer register at all. -/
theorem sound_noLostTick (hR : Sound R) (h : Reachable R C s) : lostTick s = false := by
  have i := (reachable_inv hR h).m
  simp [lostTick, anyVcpu]
  intro _
  have o0 := i.v0.owe
  have o1 := i.v1.owe
  constructor <;> (intro h1; simp_all)

/-- R1 and R4: the counter is the one the guest should read whenever it runs, so an armed
timer fires when it should. -/
theorem sound_clockRight (hR : Sound R) (h : Reachable R C s) : clockOff s = false := by
  have i := reachable_inv hR h
  cases hp : s.ph <;> simp [clockOff, running, hp]
  exact i.run hp

theorem sound_timerCanFire (hR : Sound R) (h : Reachable R C s) : timerCannotFire s = false := by
  have i := reachable_inv hR h
  cases hp : s.ph <;> simp [timerCannotFire, running, hp]
  intro _
  rw [i.run hp]
  decide

/-- R2: no interrupt is active on a CPU that is not in its handler. -/
theorem sound_noStuckActive (hR : Sound R) (h : Reachable R C s) : stuckActive s = false := by
  have i := (reachable_inv hR h).m
  simp [stuckActive, anyVcpu, i.v0.tact, i.v0.sact, i.v1.tact, i.v1.sact, i.pact]

/-- R2: no active priority is left on a CPU that is in no handler. -/
theorem sound_noStuckPriority (hR : Sound R) (h : Reachable R C s) : stuckPriority s = false := by
  have i := (reachable_inv hR h).m
  simp [stuckPriority, anyVcpu, i.v0.ap, i.v1.ap]

/-- R2: no enable the guest set is lost. -/
theorem sound_noLostEnable (hR : Sound R) (h : Reachable R C s) : lostEnable s = false := by
  have i := (reachable_inv hR h).m
  simp [lostEnable, anyVcpu, i.gic, i.pen, i.v0.ten, i.v0.sen, i.v0.pmr, i.v0.grp, i.v1.ten, i.v1.sen,
    i.v1.pmr, i.v1.grp]

/-- R2: nothing the guest disabled is enabled. -/
theorem sound_noStrayEnable (hR : Sound R) (h : Reachable R C s) : strayEnable s = false := by
  have i := (reachable_inv hR h).m
  simp [strayEnable, anyVcpu, i.v0.ten, i.v0.sen, i.v1.ten, i.v1.sen]

/-- R2, for a guest hypervisor: a loaded nested vCPU keeps the mask it was given. -/
theorem sound_noLostNestedMask (hR : Sound R) (h : Reachable R C s) : lostNestedMask s = false := by
  have i := (reachable_inv hR h).m
  simp [lostNestedMask, anyVcpu, i.v0.ich, i.v1.ich]

/-- R2: no SGI is pending that nobody sent, and no latch sits on the timer interrupt. -/
theorem sound_noStrayPending (hR : Sound R) (h : Reachable R C s) : strayPending s = false := by
  have i := (reachable_inv hR h).m
  simp [strayPending, anyVcpu, i.v0.call, i.v1.call]
  intro _
  constructor <;> (intro h1; simp_all)

theorem bool3 (g p A B S : Bool) (h : (g && p && (A || B || S)) = true) :
    ((g && p && true && !false && A) || (g && p && true && !false && B)
      || (g && p && true && !false && S)) = true := by
  cases g <;> cases p <;> cases A <;> cases B <;> cases S <;> simp_all

/-- On a CPU with no active priority and its group on, what KVM counts as pending is
signalled. -/
theorem idle_signalled (C : Cfg) (m : VM) (v : Vid) (hap : (m.get v).ap = false)
    (hgrp : (m.get v).grp = true) (hk : kvmPending C m v = true) :
    (deliverable C m v .tmr || deliverable C m v .sgi || deliverable C m v .spi) = true := by
  cases v <;> simp only [VM.get_false, VM.get_true] at hap hgrp <;>
    simp only [kvmPending, deliverable, VM.get_false, VM.get_true, hap, hgrp] at hk ⊢ <;>
    exact bool3 _ _ _ _ _ hk

/-- R3: no vCPU is idle, runnable for KVM, and beyond every interrupt: an idle CPU has no
active priority and its group is on, so what KVM counts as pending is signalled. -/
theorem sound_noSpin (hR : Sound R) (h : Reachable R C s) : spins C s = false := by
  have i := (reachable_inv hR h).m
  cases hsp : spins C s
  · rfl
  · exfalso
    simp only [spins, anyVcpu, Bool.and_eq_true, Bool.or_eq_true, Bool.not_eq_true', beq_iff_eq] at hsp
    rcases hsp with ⟨_, hsp | hsp⟩
    · obtain ⟨⟨⟨⟨⟨⟨_, _⟩, hg⟩, hk⟩, h1⟩, h2⟩, h3⟩ := hsp
      have hd := idle_signalled C s.vm false (by simp [VM.get, i.v0.ap, hg]) (by simp [VM.get, i.v0.grp]) hk
      simp [h1, h2, h3] at hd
    · obtain ⟨⟨⟨⟨⟨⟨_, _⟩, hg⟩, hk⟩, h1⟩, h2⟩, h3⟩ := hsp
      have hd := idle_signalled C s.vm true (by simp [VM.get, i.v1.ap, hg]) (by simp [VM.get, i.v1.grp]) hk
      simp [h1, h2, h3] at hd

/-- A vCPU of a sound rule book's reachable state is not in KVM's background-timer loop: a
blocked thread's guest CPU is idle, so its timer interrupt is enabled, not active and not
masked, and a stopped vCPU's CPU is offline and has no timer armed. -/
theorem vcpu_noTimerLoop {m : VM} (v : Vid) (c : Vcpu) (hc : VOk c) (hg : m.gicEn = true) :
    (c.thr != .user &&
      ((c.thr == .blocked && !c.stopped && timerStuck m c && !kvmPending C m v) ||
       (c.stopped && timerDue m c))) = false := by
  obtain ⟨a, _, _, _, e, _, f, _, _, _, k, _, _, _, o, b⟩ := hc
  rcases ht : c.thr with _ | _ | _ <;> rcases hg' : c.g with _ | _ | (_ | _ | _) <;>
    cases hs : c.stopped <;> cases hd : timerDue m c <;>
    simp_all [timerStuck, timerDue]

/-- R3, the host's own loop. -/
theorem sound_noTimerLoop (hR : Sound R) (h : Reachable R C s) : timerLoops C s = false := by
  have i := (reachable_inv hR h).m
  simp only [timerLoops, anyVcpu, vcpu_noTimerLoop false s.vm.c0 i.v0 i.gic,
    vcpu_noTimerLoop true s.vm.c1 i.v1 i.gic, Bool.or_self, Bool.and_false]

/-- R5: nothing a sound rule book asks of a paused source changes a vCPU's interrupt, timer or
power state, or takes something from the device's interrupt. -/
theorem sound_sourceUndisturbed (hR : Sound R) (h : Reachable R C s) : sourceDisturbed s = false :=
  (reachable_inv hR h).calm

/-- R6: a CPU the guest has on is not stopped, and one it has off is. -/
theorem sound_powerRight (hR : Sound R) (h : Reachable R C s) : powerWrong s = false := by
  have i := (reachable_inv hR h).m
  simp [powerWrong, anyVcpu, i.v0.mp, i.v1.mp]

/-- Every requirement, in every reachable state. -/
theorem sound_healthy (hR : Sound R) (h : Reachable R C s) : healthy C s = true := by
  simp [healthy, checks, sound_noLostIpi hR h, sound_noLostDeviceIrq hR h, sound_timerCanFire hR h,
    sound_noLostTick hR h, sound_noLostNestedMask hR h, sound_noTimerLoop hR h,
    sound_noStuckActive hR h, sound_noStuckPriority hR h, sound_noLostEnable hR h,
    sound_noStrayEnable hR h, sound_noStrayPending hR h, sound_noSpin hR h, sound_clockRight hR h,
    clockOffClone,
    sound_sourceUndisturbed hR h, sound_powerRight hR h]

end

/-! ## The two designs are sound -/

theorem clearSet_inv (c s : Bool) : clearSet c (!s) s = s := by
  cases c <;> cases s <;> rfl

theorem Design.cloudHypervisor_sound : Sound Design.cloudHypervisor :=
  ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl, quiet_nil, quiet_kick, fun C i => by
    simp [build, Design.cloudHypervisor, apply, clrOf, clearSet_inv, rebuilt, vcpuOf, emptyVM,
      freshVcpu, VM.get, VM.set, Image.slot, IrqSt.none]⟩

theorem Design.firecracker_sound : Sound Design.firecracker :=
  ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl, quiet_devEvent, quiet_nil, fun C i => by
    simp [build, Design.firecracker, Today.fcOrder, apply, clrOf, clearSet_inv, rebuilt, vcpuOf,
      emptyVM, freshVcpu, VM.get, VM.set, Image.slot, IrqSt.none]⟩

theorem design_cloudHypervisor {C : Cfg} {s : State} (h : Reachable Design.cloudHypervisor C s) :
    healthy C s = true := sound_healthy Design.cloudHypervisor_sound h

theorem design_firecracker {C : Cfg} {s : State} (h : Reachable Design.firecracker C s) :
    healthy C s = true := sound_healthy Design.firecracker_sound h

/-! ## Today's rule books are not sound, and which requirement of `Sound` each fails -/

theorem cloudHypervisor_reads_vcpu0 : Today.cloudHypervisor.slotsFromRestore = true := rfl
theorem cloudHypervisor_no_freeze : Today.cloudHypervisor.freeze = false := rfl
theorem forkDefault_no_freeze : Today.forkDefault.freeze = false := rfl
theorem forkBefore630_no_freeze : Today.forkBefore630.freeze = false := rfl
theorem forkNested_counter_of_save : Today.forkNested.cntAtPause = false := rfl
theorem forkNested_load_not_frozen : Today.forkNested.freezeLoaded = false := rfl

theorem cloudHypervisorRepin_no_freeze : Today.cloudHypervisorRepin.freeze = false := rfl
theorem cloudHypervisorRepin_not_sound : ¬ Sound Today.cloudHypervisorRepin := fun h => nomatch h.freeze

theorem cloudHypervisorUpstream_no_freeze (a : Bool) :
    (Today.cloudHypervisorUpstream a).freeze = false := rfl
theorem cloudHypervisorUpstream_not_sound (a : Bool) : ¬ Sound (Today.cloudHypervisorUpstream a) :=
  fun h => nomatch h.freeze

theorem cloudHypervisor_not_sound : ¬ Sound Today.cloudHypervisor := fun h => nomatch h.ownSlots
theorem forkDefault_not_sound : ¬ Sound Today.forkDefault := fun h => nomatch h.freeze
theorem forkBefore630_not_sound : ¬ Sound Today.forkBefore630 := fun h => nomatch h.freeze
theorem forkNested_not_sound : ¬ Sound Today.forkNested := fun h => nomatch h.cntAtPause

/-! ## A recorded run is a reachable state -/

theorem replay_reachable {R : Rules} {C : Cfg} : ∀ (ls : List Label) (s t : State),
    Reachable R C s → replay R C ls s = some t → Reachable R C t
  | [], s, t, h, hr => by
    simp [replay] at hr
    exact hr ▸ h
  | l :: ls, s, t, h, hr => by
    simp only [replay] at hr
    cases hst : step R C l s with
    | none => simp [hst] at hr
    | some u =>
      simp [hst] at hr
      exact replay_reachable ls u t (Reachable.step l h hst) hr

/-- A `breaks_*` theorem of `Breaks.lean` names a reachable state that fails the test. -/
theorem break_reachable {R : Rules} {C : Cfg} {run : List Label} {test : State → Bool}
    (h : (replay R C run (init C)).map test = some true) : ∃ s, Reachable R C s ∧ test s = true := by
  obtain ⟨t, ht, hf⟩ := map_some h
  exact ⟨t, replay_reachable run _ t .init ht, hf⟩

/-- A rule book with a run that ends unhealthy is not `Sound`. This is how each mutant of
`Mutants.lean` is shown to need its repair: from a state it reaches, not from its flag. -/
theorem not_sound_of_run {R : Rules} {C : Cfg} {run : List Label}
    (h : (replay R C run (init C)).map (healthy C) = some false) : ¬ Sound R := by
  intro hR
  obtain ⟨t, ht, hf⟩ := map_some h
  have := sound_healthy hR (replay_reachable run _ t .init ht)
  simp_all

/-! ## R5 constrains a rule book -/

/-- A VMM that stops a vCPU at the save has disturbed the source the moment it saves. -/
theorem r5_fails_saveStops :
    (replay Mutant.chSaveStopsVcpu1 Cfg.plain [.pause, .save] (init Cfg.plain)).map sourceDisturbed
      = some true := by
  decide

/-- One that re-initialises a vCPU has disarmed its timer. -/
theorem r5_fails_saveInits :
    (replay Mutant.chSaveInitsVcpu0 Cfg.plain [.g false .arm, .pause, .save] (init Cfg.plain)).map
      sourceDisturbed = some true := by
  decide

/-- One that clears a pending register has taken an IPI the guest sent. -/
theorem r5_fails_saveUnpends :
    (replay Mutant.chSaveUnpendsVcpu1 Cfg.plain [.g false .send, .pause, .save] (init Cfg.plain)).map
      sourceDisturbed = some true := by
  decide

/-- And at the resume. -/
theorem r5_fails_resumeStops :
    (replay Mutant.fcResumeStopsVcpu1 Cfg.plain [.pause, .resume] (init Cfg.plain)).map sourceDisturbed
      = some true := by
  decide

/-- What today's VMMs do write to a paused source takes nothing from it, and shows as an
interrupt with nothing behind it: Cloud Hypervisor's resume raises the device's interrupt. -/
theorem cloudHypervisor_kicks :
    (replay Today.cloudHypervisor Cfg.plain [.pause, .resume] (init Cfg.plain)).map
      (fun s => sourceKicked s && !sourceDisturbed s && s.vm.spi.latch && !s.vm.dev) = some true := by
  decide

/-- Firecracker's save publishes an event of its vsock device on the source, with its
interrupt: not a kick, and nothing taken. -/
theorem firecracker_save_raises_device :
    (replay Today.forkDefault Cfg.plain [.pause, .save, .resume] (init Cfg.plain)).map
      (fun s => s.vm.dev && s.vm.spi.latch && !sourceKicked s && !sourceDisturbed s) = some true := by
  decide

/-! ## Cloud Hypervisor upstream's two clock modes, on one run -/

/-- Upstream replays the vCPUs' states on several threads. Either order builds the same VM:
the counter is written once more, through vCPU 0, after both. -/
theorem upstream_replay_order (advance : Bool) (C : Cfg) (i : Image) :
    build (Today.cloudHypervisorUpstream advance) C i
      = build (Today.cloudHypervisorUpstreamSwapped advance) C i := by
  cases hnv : C.nv <;> cases h0 : i.s0.l2 <;> cases h1 : i.s1.l2 <;> cases hc0 : i.s0.cnt <;>
    cases hc1 : i.s1.cnt <;>
    simp [build, Today.cloudHypervisorUpstream, Today.cloudHypervisorUpstreamSwapped, apply, clrOf,
      emptyVM, freshVcpu, VM.get, VM.set, Image.slot, IrqSt.none, hnv, h0, h1, hc0, hc1]

/-- The run: CPU 0 arms its timer one unit ahead, the VM is paused and saved, host time passes,
a clone is restored and resumed. -/
def Run.oldSnapshot : List Label := [.g false .arm, .pause, .save, .wait, .restore, .resume]

/-- `advance` (the default): the clone's counter is ahead by the host time since the pause, and
the timer armed before the pause is due the moment the clone runs. -/
theorem upstreamAdvance_oldSnapshot :
    (replay (Today.cloudHypervisorUpstream true) Cfg.dump Run.oldSnapshot (init Cfg.dump)).map
      (fun s => clockOffClone s && s.vm.c0.tmr.line) = some true := by
  decide

/-- `preserve`: the clone's counter is the one of the pause and the timer is not due. -/
theorem upstreamPreserve_oldSnapshot :
    (replay (Today.cloudHypervisorUpstream false) Cfg.dump Run.oldSnapshot (init Cfg.dump)).map
      (fun s => !clockOff s && !s.vm.c0.tmr.line) = some true := by
  decide

/-- A pause and a resume in place, in either mode: the source's counter has run through the
pause. -/
theorem upstream_resumeInPlace (advance : Bool) :
    (replay (Today.cloudHypervisorUpstream advance) Cfg.dump [.pause, .save, .wait, .resume]
      (init Cfg.dump)).map clockOff = some true := by
  cases advance <;> decide

end Resume
