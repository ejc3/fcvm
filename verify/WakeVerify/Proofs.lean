import WakeVerify.Lemmas

/-!
# The repaired system keeps every property, in every reachable state

`Sound R C` says what a rule book and a configuration must have: a kernel that keeps a recorded
kick (`KeepsKick`), a restore that ends with a kick for every vCPU saved runnable (or no snapshots
at all), and no write of `mp_state` to a live VM. For any such pair, every state reachable from
`init` satisfies every property of `Properties.lean` (`sound_*`). `Design.rules` is such a rule
book (`design_sound`), and so the `design_*` theorems hold for every configuration, the nested,
Xen, hotplug and SEV paths included.

No `sorry`, and no axiom beyond Lean's own (`Axioms.lean` checks it on every build). The proof is
an inductive invariant (`Inv` in `Lemmas.lean`), not the bounded search: it holds for runs of any
length.
-/

namespace Wake

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-! ## Every reachable state -/

/-- The rule books and configurations the proofs cover: a kernel that keeps a kick, a restore
that ends with one (or no snapshots at all), and no userspace write of `mp_state` to a live VM. -/
structure Sound (R : Rules) (C : Cfg) : Prop where
  kernel : KeepsKick R.kernel
  restore : R.restore = firecrackerWithKick ∨ C.snapshot = false
  noSet : C.userSetMp = false

/-- The invariant, with what a configuration without snapshots adds: the VM is never paused. -/
def SInv (C : Cfg) (s : State) : Prop :=
  Inv s ∧ (C.snapshot = false → s.paused = false)

theorem step_inv {R : Rules} {C : Cfg} (hS : Sound R C) {l : Label} {s t : State}
    (h : step R C l s = some t) (hi : SInv C s) : SInv C t := by
  obtain ⟨⟨ia, ib⟩, hp⟩ := hi
  cases l with
  | vcpu i act =>
    simp only [step, Option.map_eq_some_iff] at h
    obtain ⟨w, hw, rfl⟩ := h
    cases i
    · have := vstep_inv hS.kernel hS.noSet hw ia ib
      exact ⟨⟨this.1, this.2⟩, hp⟩
    · have := vstep_inv hS.kernel hS.noSet hw ib ia
      exact ⟨⟨this.2, this.1⟩, hp⟩
  | pause =>
    simp only [step] at h
    split at h
    · cases h
      rename_i hc
      obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8⟩ := ia
      obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8⟩ := ib
      refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩, ?_⟩ <;>
        simp_all [owed]
    · cases h
  | resume =>
    simp only [step] at h
    split at h
    · cases h
      obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8⟩ := ia
      obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8⟩ := ib
      refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩, ?_⟩ <;>
        simp_all [owed]
    · cases h
  | clone =>
    simp only [step] at h
    split at h
    · cases h
      rename_i hc
      cases hS.restore with
      | inl hR => exact ⟨⟨restore_inv hS.kernel hR ia, restore_inv hS.kernel hR ib⟩, hp⟩
      | inr hns => simp_all
    · cases h

theorem reachable_inv {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : SInv C s := by
  induction hr with
  | init => exact ⟨inv_init, fun _ => rfl⟩
  | step l _ h ih => exact step_inv hS h ih

/-! ## The properties, for every reachable state of a sound rule book -/

theorem inv_get {s : State} (hi : Inv s) (i : Vid) : PInv (s.get i) (s.get i.other) s.lock := by
  cases i
  · exact hi.1
  · exact hi.2

/-- **P1.** No vCPU sleeps in `kvm_wait` with its kick due and gone. -/
theorem sound_noLostKick {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoLostKick s := by
  intro i
  have k := (inv_get (reachable_inv hS hr).1 i).kick
  unfold State.lostKick
  cases hd : s.due i
  · simp
  · cases hn : s.nothingPending i
    · simp
    · exfalso
      have ho : owed (s.get i) s.lock = true := by
        simp only [State.due, Bool.and_eq_true, Bool.or_eq_true] at hd
        simp only [owed, Bool.and_eq_true, Bool.or_eq_true]
        exact ⟨hd.1.2, Or.inr ⟨hd.1.1, hd.2⟩⟩
      have := k ho
      simp_all [State.nothingPending]

/-- P1 for the hang. -/
theorem sound_noHang {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoHang s := by
  intro i
  have := sound_noLostKick hS hr i
  simp [State.lostKickHard, this]

/-- **P2.** No cross-CPU call waits for a vCPU that never wakes, and no queued call is without its
interrupt. -/
theorem sound_noStuckCall {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoStuckCall s := by
  intro i
  have h1 := sound_noHang hS hr i.other
  have p := (inv_get (reachable_inv hS hr).1 i).ipi
  constructor
  · simp [State.stuckCall, h1]
  · simp only [State.lostIpi]
    cases hc : (s.get i).csd <;> simp_all

/-- **P3.** The lock has one owner at a time, whatever wakes a vCPU. -/
theorem sound_mutualExclusion {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : MutualExclusion s := by
  intro ⟨ha, hb⟩
  have p := ((reachable_inv hS hr).1.1.holds ha).2
  simp [State.get] at ha hb
  simp_all

theorem sound_noOrphanHalt {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoOrphanHalt s := by
  intro i
  have p := (inv_get (reachable_inv hS hr).1 i).apf
  simp only [State.orphanHalt]
  cases ha : (s.get i).h.apfHalted <;> simp_all

theorem sound_noLostTimer {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoLostTimer s := by
  intro i
  have p := (inv_get (reachable_inv hS hr).1 i).timer
  simp only [State.lostTimer]
  cases hp : (s.get i).pc == .idWokeT && (s.get i).h.mp == .halted
  · simp_all
  · simp_all

theorem sound_noStrandedNode {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoStrandedNode s := by
  intro i
  have p := (inv_get (reachable_inv hS hr).1 i).node
  simp only [State.strandedNode]
  cases hm : (s.get i).mcs <;> cases hh : (s.get i).hashed <;> simp_all

/-! ## Which kernels keep a kick -/

/-- The kernel rules, unfolded. -/
macro "kernel_rules" : tactic => `(tactic|
  simp [V614.kernel, V614.halt, V614.unhalt, V614.present, V614.kick, V614.getMpState,
    V614.setMpState, V614.nestedExit, V614.xenBlock, V614.xenWake, V614.initAp, V614.sipi,
    V614.snpReset, V733Patched.kernel, V733Patched.present, V733.kernel, V733.setMp, V733.halt,
    V733.unhalt, V733.kick, V733.getMpState, V733.setMpState, V733.nestedExit, V733.xenBlock,
    V733.xenWake, V733.initAp, V733.sipi, V733.snpReset])

/-- Decide one line of `KeepsKick` by trying every value of the vCPU state. -/
macro "by_values" : tactic => `(tactic|
  (first
   | (intro (ev : Bool) (st : Mp) (h : Hv); rcases h with ⟨mp, pv, ah⟩
      cases ev <;> cases st <;> cases mp <;> cases pv <;> cases ah <;> kernel_rules)
   | (intro (m : Mp) (h : Hv); rcases h with ⟨mp, pv, ah⟩
      cases m <;> cases mp <;> cases pv <;> cases ah <;> kernel_rules)
   | (intro (h : Hv); rcases h with ⟨mp, pv, ah⟩
      cases mp <;> cases pv <;> cases ah <;> kernel_rules)))

theorem v614_keepsKick : KeepsKick V614.kernel := by
  constructor <;> by_values

theorem v733Patched_keepsKick : KeepsKick V733Patched.kernel := by
  constructor <;> by_values

/-- v7.3.3 as released does not: a completion clears a recorded kick. This is #2125. -/
theorem v733_not_keepsKick : ¬ KeepsKick V733.kernel := by
  intro hK
  have := (hK.present_keeps { mp := .runnable, pv := true, apfHalted := false }).1
  simp [V733.kernel, V733.present, V733.setMp] at this

/-! ## The design -/

/-- REPAIR 1 and REPAIR 2 together are sound, in every configuration where userspace does not
write `mp_state` to a live VM. -/
theorem design_sound (C : Cfg) (hC : C.userSetMp = false) : Sound Design.rules C :=
  { kernel := v733Patched_keepsKick, restore := Or.inl rfl, noSet := hC }

theorem design_noLostKick {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoLostKick s :=
  sound_noLostKick (design_sound C hC) hr

theorem design_noHang {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoHang s :=
  sound_noHang (design_sound C hC) hr

theorem design_noStuckCall {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoStuckCall s :=
  sound_noStuckCall (design_sound C hC) hr

theorem design_mutualExclusion {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : MutualExclusion s :=
  sound_mutualExclusion (design_sound C hC) hr

theorem design_noOrphanHalt {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoOrphanHalt s :=
  sound_noOrphanHalt (design_sound C hC) hr

theorem design_noLostTimer {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoLostTimer s :=
  sound_noLostTimer (design_sound C hC) hr

theorem design_noStrandedNode {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoStrandedNode s :=
  sound_noStrandedNode (design_sound C hC) hr

/-! ## What else is sound -/

/-- v6.14 with the restore kick. -/
theorem v614Kick_sound (C : Cfg) (hC : C.userSetMp = false) : Sound Partial.v614Kick C :=
  { kernel := v614_keepsKick, restore := Or.inl rfl, noSet := hC }

/-- v6.14 as it is, when no snapshot is taken: v6.14 has no #2125. -/
theorem v614_sound_without_snapshots (C : Cfg) (hC : C.userSetMp = false)
    (hN : C.snapshot = false) : Sound Today.v614 C :=
  { kernel := v614_keepsKick, restore := Or.inr hN, noSet := hC }

/-- v7.3.3 with the #2130 patch alone, when no snapshot is taken. -/
theorem v733Patched_sound_without_snapshots (C : Cfg) (hC : C.userSetMp = false)
    (hN : C.snapshot = false) : Sound Partial.v733Patched C :=
  { kernel := v733Patched_keepsKick, restore := Or.inr hN, noSet := hC }

/-! ## P3, about the guest paths alone -/

/-- Every HLT in the guest paths is in a loop that reads again: the step after a HLT changes
neither the lock byte nor the other vCPU, and goes to the read (`hdTry2` reads the lock byte,
`ndCas` leads to the read of `node->locked`), or out of the idle loop. A wake-up for any reason is
therefore harmless. -/
theorem hlt_rechecks (K : Kernel) (me o : Vcpu) (lock : Lock) (w : View)
    (h : advance K me o lock = some w) :
    (me.pc = .hdWoke → w.1.pc = .hdTry2 ∧ w.2.1 = o ∧ w.2.2 = lock) ∧
    (me.pc = .ndWoke → w.1.pc = .ndCas ∧ w.2.1 = o ∧ w.2.2 = lock) ∧
    (me.pc = .idWoke ∨ me.pc = .idWokeT → w.1.pc = .run ∧ w.2.1 = o ∧ w.2.2 = lock) := by
  unfold advance at h
  refine ⟨?_, ?_, ?_⟩
  · intro hp; simp [hp] at h; subst h; simp
  · intro hp; simp [hp] at h; subst h; simp
  · intro hp; rcases hp with hp | hp <;> simp [hp] at h <;> subst h <;> simp

end Wake
