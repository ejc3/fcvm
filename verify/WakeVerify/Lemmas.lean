import WakeVerify.Design
import WakeVerify.Properties

/-!
# What the proofs rest on

`KeepsKick` is what the proofs need of a kernel: that a kick, once recorded, is cleared only by a
step that also leaves the vCPU running. v6.14 and v7.3.3 with the #2130 patch have it; v7.3.3
unpatched does not (`present_keeps` fails: `Breaks.lean`).

`PInv me o lock` is the invariant, written for one vCPU (`me`) with the other (`o`) and the lock
byte. The system's invariant is `PInv` for each of the two.
-/

namespace Wake

/-- The positions right after a HLT: the only ones where KVM may hold the vCPU halted. -/
def Pc.afterHlt : Pc → Bool
  | .hdWoke | .ndWoke | .idWoke | .idWokeT | .l2Woke | .xenWoke | .off => true
  | _ => false

/-- The vCPU will sleep in `kvm_wait`, or sleeps there, and its lock byte has been released: it is
owed a kick. `State.due` is the part of this where it already sleeps. -/
def owed (me : Vcpu) (lock : Lock) : Bool :=
  lock != .slow &&
    (me.pc == .hdHlt || (me.pc == .ndHlt && me.hashed) ||
      (me.h.mp == .halted && (me.pc == .hdWoke || (me.pc == .ndWoke && me.hashed))))

/-- What the proofs need of a kernel's rules. -/
structure KeepsKick (K : Kernel) : Prop where
  halt_kick : ∀ ev st h, h.pv = true → h.mp = .runnable → (K.halt ev st h).mp = .runnable
  halt_apf : ∀ ev st h, (K.halt ev st h).apfHalted = h.apfHalted
  unhalt_not_halted : ∀ h, (K.unhalt h).mp ≠ .halted
  unhalt_runnable : ∀ h, h.mp = .runnable → (K.unhalt h).mp = .runnable ∧ (K.unhalt h).pv = h.pv
  unhalt_mp : ∀ h, (K.unhalt h).mp ≠ .runnable → h.mp ≠ .runnable
  unhalt_apf : ∀ h, (K.unhalt h).apfHalted = true → h.apfHalted = true
  /-- REPAIR 1 is this line: a completion leaves the kick alone. -/
  present_keeps : ∀ h, (K.present h).pv = h.pv ∧ (K.present h).mp = .runnable ∧
    (K.present h).apfHalted = false
  kick_sets : ∀ h, (K.kick h).pv = true ∧ (K.kick h).mp = h.mp ∧
    (K.kick h).apfHalted = h.apfHalted
  get_runnable : ∀ h, h.mp = .runnable → K.getMpState h = .runnable
  get_halted : ∀ h, K.getMpState h = .halted → h.mp = .halted ∧ h.pv = false
  get_not_runnable : ∀ h, K.getMpState h ≠ .runnable → h.mp ≠ .runnable
  set_mp : ∀ m h, (K.setMpState m h).mp = m ∧ (K.setMpState m h).apfHalted = h.apfHalted
  nested : ∀ h, (K.nestedExit h).mp = .runnable ∧ (K.nestedExit h).apfHalted = h.apfHalted
  xen_block : ∀ h, (K.xenBlock h).apfHalted = h.apfHalted
  xen_wake : ∀ h, (K.xenWake h).mp = .runnable ∧ (K.xenWake h).apfHalted = h.apfHalted
  init_ap : ∀ h, (K.initAp h).apfHalted = false
  sipi : ∀ h, (K.sipi h).mp = .runnable ∧ (K.sipi h).apfHalted = h.apfHalted
  snp : ∀ h, (K.snpReset h).mp = .runnable ∧ (K.snpReset h).apfHalted = h.apfHalted

/-- The invariant for one vCPU. -/
structure PInv (me o : Vcpu) (lock : Lock) : Prop where
  /-- a vCPU in guest mode is runnable -/
  guestRuns : me.loc = .guest → me.h.mp = .runnable
  /-- KVM holds a vCPU in another state only right after a HLT -/
  afterHlt : me.h.mp ≠ .runnable → me.pc.afterHlt = true
  /-- a vCPU that is owed a kick has it recorded, or the other vCPU is about to send it -/
  kick : owed me lock = true → me.h.pv = true ∨ o.pc = .ulKick
  /-- the owner is alone, and the lock byte says the lock is taken -/
  holds : me.pc.holds = true → lock ≠ .free ∧ o.pc.holds = false
  /-- a queued call has its interrupt pending -/
  ipi : me.csd = true → me.ipi = true
  /-- a synthetic halt has its completion coming -/
  apf : me.h.apfHalted = true → me.apf ≠ .none
  /-- a vCPU that halts for its timer has one -/
  timer : (me.pc = .idHltT ∨ (me.pc = .idWokeT ∧ me.h.mp = .halted)) → me.tmr ≠ .off
  /-- a node that was handed the queue head while halting is hashed by the new owner -/
  node : me.mcs = true → me.hashed = false →
    (me.pc = .ndChk ∨ me.pc = .ndHlt ∨ me.pc = .ndWoke) → o.pc = .hoKick
  /-- a lock byte that is not free has an owner -/
  lockHeld : lock ≠ .free → me.pc.holds = true ∨ o.pc.holds = true
  /-- a queue node that has not been handed the queue head has the head ahead of it: still
  taking the lock, or just the owner and about to hand over -/
  queue : me.pc.isNode = true → me.mcs = false → o.pc.isHead = true ∨ o.pc = .hoSet

/-- The invariant of the system. -/
def Inv (s : State) : Prop :=
  PInv s.a s.b s.lock ∧ PInv s.b s.a s.lock

theorem inv_init : Inv init := by
  constructor <;> constructor <;> simp [init, owed, Pc.holds, Pc.afterHlt, Pc.isNode]

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-! ## One step keeps the invariant -/

/-- Split the invariant of both vCPUs into its twenty parts and close each by rewriting. -/
macro "leaf" : tactic => `(tactic|
  (refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
     first
     | assumption
     | simp_all [owed, Pc.holds, Pc.afterHlt, Pc.headSpinning, Pc.isHead, Pc.nodeHalted,
         Pc.isNode, Vcpu.hlt]))

set_option maxHeartbeats 4000000 in
theorem advance_inv {K : Kernel} (hK : KeepsKick K) {me o : Vcpu} {lock : Lock} {w : View}
    (h : advance K me o lock = some w) (hg : me.loc = .guest)
    (im : PInv me o lock) (io : PInv o me lock) : PInv w.1 w.2.1 w.2.2 ∧ PInv w.2.1 w.1 w.2.2 := by
  obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
  obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
  have hr := m1 hg
  unfold advance gotLock at h
  split at h <;> (try split at h) <;> (try split at h) <;> (try split at h) <;> (try cases h) <;>
    (try leaf)
  all_goals
    (by_cases hp : me.h.pv = true <;>
      first
      | (simp_all [owed, Pc.holds, Pc.afterHlt, Pc.headSpinning, Pc.isHead, Pc.nodeHalted,
          Pc.isNode, Vcpu.hlt, hK.kick_sets, hK.halt_apf, hK.halt_kick]; done)
      | (cases hop : o.pc <;>
          simp_all [owed, Pc.holds, Pc.afterHlt, Pc.headSpinning, Pc.isHead, Pc.nodeHalted,
            Pc.isNode, Vcpu.hlt, hK.kick_sets, hK.halt_apf, hK.halt_kick]))

macro "leafK" hK:ident : tactic => `(tactic|
  (refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
     first
     | assumption
     | simp_all [owed, Pc.holds, Pc.afterHlt, Pc.headSpinning, Pc.isHead, Pc.nodeHalted,
         Pc.isNode, Vcpu.hlt, Vcpu.ifOn, ($hK).kick_sets, ($hK).halt_apf, ($hK).halt_kick,
         ($hK).nested, ($hK).xen_block]))

set_option maxHeartbeats 4000000 in
theorem guest_inv {K : Kernel} (hK : KeepsKick K) {C : Cfg} {act : Act}
    {me o : Vcpu} {lock : Lock} {w : View}
    (h : guest K C act me o lock = some w)
    (im : PInv me o lock) (io : PInv o me lock) : PInv w.1 w.2.1 w.2.2 ∧ PInv w.2.1 w.1 w.2.2 := by
  unfold guest at h
  split at h
  · cases h
  · rename_i hloc
    have hg : me.loc = .guest := by simpa using hloc
    cases act <;> dsimp only at h
    case step => exact advance_inv hK h hg im io
    all_goals
      (obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
       obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
       have hr := m1 hg
       first
       | (cases h; done)
       | (split at h <;> first | (cases h; done) | (cases h; leafK hK)))
    all_goals
      (cases hmp : me.pc <;>
        simp_all [owed, Pc.holds, Pc.afterHlt, Pc.headSpinning, Pc.isHead, Pc.nodeHalted,
          Pc.isNode, Vcpu.hlt, Vcpu.ifOn, hK.kick_sets, hK.halt_apf, hK.halt_kick, hK.nested,
          hK.xen_block])

/-! What a completion changes, field by field. -/

@[simp] theorem present_pc (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).pc = v.pc := rfl
@[simp] theorem present_irqsOn (K : Kernel) (k : ApfKind) (v : Vcpu) :
    (v.present K k).irqsOn = v.irqsOn := rfl
@[simp] theorem present_mcs (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).mcs = v.mcs := rfl
@[simp] theorem present_hashed (K : Kernel) (k : ApfKind) (v : Vcpu) :
    (v.present K k).hashed = v.hashed := rfl
@[simp] theorem present_csd (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).csd = v.csd := rfl
@[simp] theorem present_h (K : Kernel) (k : ApfKind) (v : Vcpu) :
    (v.present K k).h = K.present v.h := rfl
@[simp] theorem present_apf (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).apf = .none := rfl
@[simp] theorem present_ipi (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).ipi = v.ipi := rfl
@[simp] theorem present_tmr (K : Kernel) (k : ApfKind) (v : Vcpu) : (v.present K k).tmr = v.tmr := rfl

@[simp] theorem freshHv_apfHalted (second : Bool) : (freshHv second).apfHalted = false := by
  cases second <;> rfl

/-- A vCPU thread in KVM_RUN is in guest mode or in the run loop. -/
theorem loc_host {v : Vcpu} (h1 : (v.loc == .user) = false) (h2 : (v.loc == .guest) = false) :
    v.loc = .host := by
  cases hl : v.loc <;> simp_all

/-- Leaving `vcpu_block` (or an NMI or interrupt at a blocked vCPU) keeps the invariant. -/
theorem unhalt_inv {K : Kernel} (hK : KeepsKick K) {me o : Vcpu} {lock : Lock}
    (hloc : me.loc = .host) (im : PInv me o lock) (io : PInv o me lock) :
    PInv { me with h := K.unhalt me.h } o lock ∧ PInv o { me with h := K.unhalt me.h } lock := by
  obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
  obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
  have u1 := hK.unhalt_not_halted me.h
  have u2 := hK.unhalt_runnable me.h
  have u3 := hK.unhalt_mp me.h
  have u4 := hK.unhalt_apf me.h
  refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩
  · simp [hloc]
  · intro hne; exact m2 (u3 hne)
  · intro how
    by_cases hmr : me.h.mp = .runnable
    · have := u2 hmr
      simp_all [owed]
    · have ha := m2 hmr
      simp_all [owed, Pc.afterHlt]
      cases hpc : me.pc <;> simp_all
  · exact m4
  · exact m5
  · intro ha; exact m6 (u4 ha)
  · intro ht
    by_cases hmr : me.h.mp = .runnable
    · have := u2 hmr
      simp_all
    · have ha := m2 hmr
      simp_all [Pc.afterHlt]
  · exact m8
  · exact m9
  · exact m10
  all_goals simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]

/-- An event at a blocked vCPU (`Vcpu.wake`) keeps the invariant: the end of `vcpu_block`, or the
return of Xen's poll. -/
theorem wake_inv {K : Kernel} (hK : KeepsKick K) {me o : Vcpu} {lock : Lock}
    (hloc : me.loc = .host) (im : PInv me o lock) (io : PInv o me lock) :
    PInv (me.wake K) o lock ∧ PInv o (me.wake K) lock := by
  unfold Vcpu.wake
  split
  · rename_i hx
    have hpc : me.pc = .xenWoke := by simpa using hx
    obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
    obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
    have x := hK.xen_wake me.h
    refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
      simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode, Pc.isHead]
  · exact unhalt_inv hK hloc im io

set_option maxHeartbeats 4000000 in
theorem hostRun_inv {K : Kernel} (hK : KeepsKick K) {me o v : Vcpu} {lock : Lock}
    (h : hostRun K me = some v)
    (im : PInv me o lock) (io : PInv o me lock) : PInv v o lock ∧ PInv o v lock := by
  unfold hostRun at h
  split at h
  · cases h
  · split at h
    · -- running: enter the guest
      obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
      obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
      rename_i hrun
      have hmp : me.h.mp = .runnable := by simp_all [Vcpu.running]
      have hah : me.h.apfHalted = false := by simp_all [Vcpu.running]
      have p := hK.present_keeps me.h
      split at h
      · -- a completion is on the done list
        split at h
        · -- the slot is occupied: it stays there
          split at h
          · cases h
            refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
              ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
              simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
          · cases h
        · cases h
          refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
            ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
            simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
      · split at h
        · cases h
          refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩,
            ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
            simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
        · cases h
    · split at h
      · cases h
      · split at h
        · cases h
        · split at h
          · -- vcpu_block ends
            cases h
            rename_i hu hnr hgst hx hrb
            exact unhalt_inv hK (loc_host (by simpa using hu) (by simpa using hgst)) im io
          · cases h

set_option maxHeartbeats 4000000 in
theorem host_inv {K : Kernel} (hK : KeepsKick K) {C : Cfg} (hC : C.userSetMp = false)
    {paused : Bool} {act : Act} {me o : Vcpu} {lock : Lock} {w : View}
    (h : host K C paused act me o lock = some w)
    (im : PInv me o lock) (io : PInv o me lock) : PInv w.1 w.2.1 w.2.2 ∧ PInv w.2.1 w.1 w.2.2 := by
  unfold host at h
  cases act <;> dsimp only at h
  case hostRun =>
    simp only [Option.map_eq_some_iff] at h
    obtain ⟨v, hv, rfl⟩ := h
    exact hostRun_inv hK hv im io
  case nmi =>
    split at h
    · cases h
      rename_i hc
      exact wake_inv hK (by simp_all) im io
    · cases h
  case extIrq =>
    split at h
    · cases h
      rename_i hc
      exact wake_inv hK (by simp_all) im io
    · cases h
  case xenWake =>
    split at h
    · cases h
      rename_i hc
      exact wake_inv hK (by simp_all) im io
    · cases h
  case setMpSame => simp [hC] at h
  all_goals
    (obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
     obtain ⟨o1, o2, o3, o4, o5, o6, o7, o8, o9, o10⟩ := io
     first
     | (cases h; done)
     | (split at h <;> first
         | (cases h; done)
         | (cases h
            refine ⟨⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩, ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩⟩ <;>
              simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode, Pc.isHead, hK.init_ap, hK.sipi,
                hK.snp])))

/-- Any step of one vCPU keeps the invariant of both. -/
theorem vstep_inv {K : Kernel} (hK : KeepsKick K) {C : Cfg} (hC : C.userSetMp = false)
    {paused : Bool} {act : Act} {me o : Vcpu} {lock : Lock} {w : View}
    (h : vstep K C paused act me o lock = some w)
    (im : PInv me o lock) (io : PInv o me lock) : PInv w.1 w.2.1 w.2.2 ∧ PInv w.2.1 w.1 w.2.2 := by
  unfold vstep at h
  split at h
  · cases h
    rename_i hgst
    exact guest_inv hK hgst im io
  · exact host_inv hK hC h im io

/-! ## Snapshot and restore -/

/-- A restore in Firecracker's order with the kick last, written out. -/
theorem restore_kick (R : Rules) (C : Cfg) (second : Bool) (v : Vcpu)
    (hR : R.restore = firecrackerWithKick) :
    restoreVcpu R C second v =
      { v with
        loc := .user
        h := if R.kernel.getMpState v.h = .runnable then
               R.kernel.kick (R.kernel.setMpState (R.kernel.getMpState v.h) (freshHv second))
             else R.kernel.setMpState (R.kernel.getMpState v.h) (freshHv second)
        apf := if C.pvApf then .done .all else .none
        ipi := v.ipi
        apfIrq := v.apfIrq
        tmr := if v.tmr = .irr then .irr else if v.tmr = .armed ∨ v.tmr = .pending then .armed
               else .off } := by
  simp only [restoreVcpu, hR, firecrackerWithKick, firecracker, List.cons_append, List.nil_append,
    List.foldl, restoreOne, save, Vcpu.fresh]
  cases ht : v.tmr <;> by_cases hg : R.kernel.getMpState v.h = .runnable <;> simp [ht, hg]

/-- REPAIR 2 is this lemma: the restored VM satisfies the invariant because every vCPU saved
runnable gets a kick, and a vCPU saved halted had none to lose. -/
theorem restore_inv {R : Rules} {C : Cfg} (hK : KeepsKick R.kernel)
    (hR : R.restore = firecrackerWithKick) {sm so : Bool} {me o : Vcpu} {lock : Lock}
    (im : PInv me o lock) : PInv (restoreVcpu R C sm me) (restoreVcpu R C so o) lock := by
  rw [restore_kick R C sm me hR, restore_kick R C so o hR]
  obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
  have g1 := hK.get_runnable me.h
  have g2 := hK.get_halted me.h
  have g3 := hK.get_not_runnable me.h
  have s1 := hK.set_mp
  have k1 := hK.kick_sets
  by_cases hg : R.kernel.getMpState me.h = .runnable
  · refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
      simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
    all_goals (cases ht : me.tmr <;> simp_all)
  · have hnr := g3 hg
    have haf := m2 hnr
    by_cases hh : R.kernel.getMpState me.h = .halted
    · have hh2 := g2 hh
      refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
        simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
      all_goals (cases ht : me.tmr <;> simp_all)
    · refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
        simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode]
      all_goals (cases hpc : me.pc <;> simp_all)

/-- The VMM's pause keeps the invariant. A vCPU in Xen's poll returns from it. -/
theorem pause_inv {K : Kernel} (hK : KeepsKick K) {me o : Vcpu} {lock : Lock}
    (im : PInv me o lock) : PInv (me.pause K) (o.pause K) lock := by
  obtain ⟨m1, m2, m3, m4, m5, m6, m7, m8, m9, m10⟩ := im
  have xm := hK.xen_wake me.h
  have xo := hK.xen_wake o.h
  unfold Vcpu.pause
  by_cases hm : me.pc = .xenWoke <;> by_cases ho : o.pc = .xenWoke <;>
    (refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;>
      simp_all [owed, Pc.holds, Pc.afterHlt, Pc.isNode, Pc.isHead])

end Wake
