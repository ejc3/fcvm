import WakeVerify.Lemmas

/-!
# The repaired system keeps every property, in every reachable state

`Sound R C` says what a rule book and a configuration must have: a kernel that keeps a recorded
kick (`KeepsKick`), a restore that ends with a kick for every vCPU saved runnable (or no snapshots
at all), and no write of `mp_state` to a live VM. For any such pair, every state reachable from
`init` satisfies every property of `Properties.lean` (`sound_*`). `Design.rules` is such a rule
book (`design_sound`), and so the `design_*` theorems hold for every configuration, the nested,
Xen, hotplug and SEV paths included, and for any number of snapshots and restores in a row.

What the theorems are, and are not. Each says that no reachable state has a certain shape: a
sleeper whose kick is gone, a sleeper with nobody ahead of it to wake it, two owners, and so on.
They are proved by an inductive invariant (`Inv` in `Lemmas.lean`), so they hold for runs of any
length. None of them says that a vCPU which has a waker is in the end woken: that needs the other
vCPU to take its steps, which is a statement about every future of a run, and it is not proved
here. The search's audit (`Search.lean`) checks that part on the finite model.

They are about a VM whose whole history ran under the rule book. A VM that hung on an unpatched
host and is then restored on a repaired one starts from a state `Reachable` does not contain.

No `sorry`, and no axiom beyond Lean's own: the sweep in `Axioms.lean` checks every theorem
whenever that file is elaborated, which `check-sweep.sh` does on every run of `make verify` and
of the CI job (`lake build` alone does it only when something `Axioms.lean` imports changed).
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
      exact ⟨⟨pause_inv hS.kernel ia, pause_inv hS.kernel ib⟩, by simp_all⟩
    · cases h
  | resume =>
    simp only [step] at h
    split at h
    · cases h
      obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := ia
      obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := ib
      refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
        ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩, ?_⟩ <;> simp_all [owed]
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

/-- **P1 before the release.** Every vCPU asleep in `kvm_wait` has a kick recorded or about to be
sent, or has the other vCPU ahead of it in the lock's path: as the owner (whose unlock then finds
`_Q_SLOW_VAL` and kicks, `unlock_slow_kicks`), or as the queue head that hands over to it. -/
theorem sound_hasWaker {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : HasWaker s := by
  intro i
  have p := inv_get (reachable_inv hS hr).1 i
  have k := p.kick
  have lh := p.lockHeld
  have nd := p.node
  have q := p.queue
  cases ho : s.orphan i
  · rfl
  · exfalso
    simp only [State.orphan, State.sleeps, Bool.and_eq_true, Bool.or_eq_true, Bool.not_eq_true',
      beq_iff_eq, bne_iff_ne, ne_eq, Bool.not_eq_eq_eq_not, Bool.not_true, decide_eq_true_eq,
      decide_eq_false_iff_not, Bool.and_eq_false_imp, Bool.decide_eq_false] at ho
    obtain ⟨⟨⟨⟨⟨hmp, hpc⟩, hpv⟩, hul⟩, hho⟩, hnh⟩ := ho
    cases hl : s.lock <;> cases hm : (s.get i).mcs <;> cases hh : (s.get i).hashed <;>
      rcases hpc with hpc | hpc <;>
      simp_all [owed, Pc.holds, Pc.isNode, Pc.isHead]

/-- The two vCPUs are never both asleep in `kvm_wait` with no kick recorded for either. -/
theorem sound_noMutualSleep {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoMutualSleep s := by
  intro ⟨ha, hb, hpa, hpb⟩
  have wa := sound_hasWaker hS hr .a
  have wb := sound_hasWaker hS hr .b
  simp only [State.orphan, State.sleeps, State.get, Vid.other] at wa wb ha hb
  simp only [Bool.and_eq_true, Bool.or_eq_true, beq_iff_eq] at ha hb
  rcases ha with ⟨hma, hca | hca⟩ <;> rcases hb with ⟨hmb, hcb | hcb⟩ <;>
    simp_all [Pc.holds, Pc.isHead]

/-- **P2.** No vCPU waits in a cross-CPU call for a vCPU asleep in `kvm_wait` with interrupts off
and no kick recorded, and no queued call is without its interrupt. The first half is `HasWaker`
for the target: a caller in `csd_lock_wait` is neither the lock's owner nor the queue head, so it
is not the target's waker. -/
theorem sound_noStuckCall {R : Rules} {C : Cfg} (hS : Sound R C) {s : State}
    (hr : Reachable R C s) : NoStuckCall s := by
  intro i
  have w := sound_hasWaker hS hr i.other
  have p := (inv_get (reachable_inv hS hr).1 i).ipi
  constructor
  · cases hc : s.stuckCall i
    · rfl
    · exfalso
      simp only [State.stuckCall, Bool.and_eq_true, beq_iff_eq, Bool.not_eq_true'] at hc
      obtain ⟨⟨⟨⟨hpc, _⟩, hsl⟩, _⟩, hpv⟩ := hc
      have : (s.get i.other.other) = s.get i := by cases i <;> rfl
      simp [State.orphan, hsl, hpv, this, hpc, Pc.holds, Pc.isHead] at w
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

theorem design_hasWaker {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : HasWaker s :=
  sound_hasWaker (design_sound C hC) hr

theorem design_noMutualSleep {C : Cfg} (hC : C.userSetMp = false) {s : State}
    (hr : Reachable Design.rules C s) : NoMutualSleep s :=
  sound_noMutualSleep (design_sound C hC) hr

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

/-- Every HLT in the modelled Linux paths is in a loop that reads again: the step after a HLT
changes neither the lock byte nor the other vCPU, and goes to the read (`hdTry2` reads the lock
byte, `ndCas` leads to the read of `node->locked`), or out of the idle loop. A wake-up for any
reason is therefore harmless. An offline CPU's HLT is in a loop that only halts again: it waits
for nothing but INIT. The other two positions after a block are not HLTs of these paths:
`l2Woke` is the nested guest's own HLT, which the model leaves only by a nested VM-exit, and
`xenWoke` is a hypercall that returns to ordinary code. -/
theorem hlt_rechecks (K : Kernel) (me o : Vcpu) (lock : Lock) (w : View)
    (h : advance K me o lock = some w) :
    (me.pc = .hdWoke → w.1.pc = .hdTry2 ∧ w.2.1 = o ∧ w.2.2 = lock) ∧
    (me.pc = .ndWoke → w.1.pc = .ndCas ∧ w.2.1 = o ∧ w.2.2 = lock) ∧
    (me.pc = .idWoke ∨ me.pc = .idWokeT → w.1.pc = .run ∧ w.2.1 = o ∧ w.2.2 = lock) ∧
    (me.pc = .off → w.1.pc = .off ∧ w.2.1 = o ∧ w.2.2 = lock) := by
  unfold advance at h
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro hp; simp [hp] at h; subst h; simp
  · intro hp; simp [hp] at h; subst h; simp
  · intro hp; rcases hp with hp | hp <;> simp [hp] at h <;> subst h <;> simp
  · intro hp; simp [hp] at h; subst h; simp [Vcpu.hlt]

/-- The owner's way out of the lock when the byte is `_Q_SLOW_VAL`: the fast unlock fails, the
slow path releases the byte, and its last step is the kick of the other vCPU. `HasWaker` says a
sleeper whose byte is still `_Q_SLOW_VAL` has an owner; this is what the owner then does. -/
theorem unlock_slow_kicks (K : Kernel) (me o : Vcpu) (lock : Lock) :
    (me.pc = .crit → advance K me o lock = some ({ me with pc := .ulTry }, o, lock)) ∧
    (me.pc = .ulTry → advance K me o .slow = some ({ me with pc := .ulRel }, o, .slow)) ∧
    (me.pc = .ulRel → advance K me o lock = some ({ me with pc := .ulKick }, o, .free)) ∧
    (me.pc = .ulKick → advance K me o lock =
      some ({ me with pc := .run, irqsOn := true }, { o with h := K.kick o.h }, lock)) := by
  refine ⟨?_, ?_, ?_, ?_⟩ <;> intro hp <;> simp [advance, hp]

/-! ## The search misses no transition, and a recorded run is a reachable state -/

theorem allActs_complete : ∀ a : Act, a ∈ allActs := by
  intro a; cases a <;> decide

theorem allLabels_complete : ∀ l : Label, l ∈ allLabels := by
  intro l
  cases l with
  | vcpu i act => cases i <;> cases act <;> decide
  | pause => decide
  | resume => decide
  | clone => decide

/-- The search stores an edge of its graph as `target * 64 + label` (`Graph.succ` in
`Search.lean`), which is one-to-one only while there are at most 64 labels. -/
theorem allLabels_fit_edge_encoding : allLabels.length ≤ 64 := by decide

/-- The end of a possible run is a reachable state, so the theorems above apply to it. -/
theorem replay_reachable {R : Rules} {C : Cfg} : ∀ (ls : List Label) (s t : State),
    Reachable R C s → replay R C ls s = some t → Reachable R C t
  | [], s, t, hs, h => by simp [replay] at h; exact h ▸ hs
  | l :: ls, s, t, hs, h => by
    simp only [replay] at h
    split at h
    · rename_i u hu; exact replay_reachable ls u t (Reachable.step l hs hu) h
    · cases h

/-! ## Why the restore kick needs the #2130 patch (#2132) -/

/-- A restore in Firecracker's order, in a guest with async page faults on, leaves every vCPU
with the wake-everyone completion on its done list, and its page-ready slot as the snapshot's
memory has it. -/
theorem restore_queues_completion (R : Rules) (C : Cfg) (second : Bool) (v : Vcpu)
    (hR : R.restore = firecrackerWithKick) (hp : C.pvApf = true) :
    (restoreVcpu R C second v).apf = .done .all ∧ (restoreVcpu R C second v).slot = v.slot := by
  rw [restore_kick R C second v hR]; simp [hp]

/-- On v7.3.3 as released, a running vCPU that enters the guest with a completion on its done
list and its page-ready slot free has no kick afterwards, whatever it had before. Together with
`restore_queues_completion`: after a restore, the restore kick of a vCPU whose slot was free in
the snapshot never survives the first entry. -/
theorem v733_entry_clears_kick (v w : Vcpu) (hr : v.running = true) (k : ApfKind)
    (ha : v.apf = .done k) (hs : v.slot = false) (h : hostRun V733.kernel v = some w) :
    w.h.pv = false := by
  unfold hostRun at h
  split at h
  · cases h
  · simp [hr, ha, hs] at h
    subst h
    simp [V733.kernel, V733.present, V733.setMp]

/-- With the #2130 patch the same entry keeps the kick. -/
theorem v733Patched_entry_keeps_kick (v w : Vcpu) (hr : v.running = true) (k : ApfKind)
    (ha : v.apf = .done k) (hs : v.slot = false) (h : hostRun V733Patched.kernel v = some w) :
    w.h.pv = v.h.pv := by
  unfold hostRun at h
  split at h
  · cases h
  · simp [hr, ha, hs] at h
    subst h
    simp [V733Patched.kernel, V733Patched.present]

end Wake
