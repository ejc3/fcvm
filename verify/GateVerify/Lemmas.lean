import GateVerify.Design
import GateVerify.Properties

/-!
# The invariant of the design: what holds in every reachable state

Two parts. `Scal` is about the settings, the rules and the phases: for each place a snapshot or a
restore can be, what the gate, `tcp_syncookies`, `rcu_expedited`, the freeze, the manifest and the
restore's record are there. `Socks` (below) is about the sockets and the lists of cookies.

Each is shown to hold at `init` and to be kept by every step of `Design.rules`, whatever the
configuration. `Proofs.lean` reads the properties off them.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-! ## Settings, rules and phases -/

/-- A guest no restore is running on: a source, or a published clone. -/
def SrcOk (C : Cfg) (s : State) : Prop :=
  s.sc = C.sc0 ∧ s.jr = false ∧ s.udp = false
  ∧ (s.rcu = true → (s.sp = .barIn ∧ s.rcuOwn = true) ∨ s.sp = .aborted)
  ∧ (s.sp = .closed4 → s.in4 = true ∧ s.out4 = true)
  ∧ (s.sp = .closed ∨ s.sp = .barIn ∨ s.sp = .bar ∨ s.sp = .dump4 ∨ s.sp = .dump6
      ∨ s.sp = .saved ∨ s.sp = .paused ∨ s.sp = .resumed →
        s.in4 = true ∧ s.in6 = true ∧ s.out4 = true ∧ s.out6 = true)
  ∧ (inPrepare s = true ∨ s.sp = .paused → s.frozen = true)
  ∧ (s.sp = .idle ∨ s.sp = .opened →
        s.in4 = false ∧ s.in6 = false ∧ s.out4 = false ∧ s.out6 = false)
  ∧ (s.sp = .idle → s.man = none)
  ∧ (s.rp = .done → s.cut = true)

/-- A clone whose agent has completed `n` steps of the design's restore. -/
def CloneOk (C : Cfg) (n : Nat) (s : State) : Prop :=
  n < 18 ∧ s.sp = .idle ∧ s.waiting = true
  ∧ (n ≤ 15 → s.in4 = true ∧ s.in6 = true)
  ∧ (16 ≤ n → s.in4 = false ∧ s.in6 = false ∧ s.udp = false)
  ∧ (n ≤ 10 → s.out4 = true ∧ s.out6 = true)
  ∧ (11 ≤ n → s.out4 = false ∧ s.out6 = false)
  ∧ (n ≤ 13 → s.frozen = true)
  ∧ (n ≤ 11 → s.cut = false) ∧ (12 ≤ n → s.cut = true)
  ∧ (n = 0 → s.jr = false) ∧ (1 ≤ n → s.jr = true ∧ s.jrSc = C.sc0)
  ∧ (n ≤ 1 ∨ n = 17 → s.sc = C.sc0) ∧ (2 ≤ n → n ≤ 16 → s.sc = false)
  ∧ (s.rcu = true → n = 5) ∧ (n = 5 → s.ag = true → s.rcuOwn = true)
  ∧ (13 ≤ n → s.man = none)

/-- A clone whose restore failed: closed as it was, on its way to the power-off. -/
def FailedOk (s : State) : Prop :=
  s.sp = .idle ∧ s.waiting = true
  ∧ (s.cut = false → s.in4 = true ∧ s.in6 = true)
  ∧ (s.cut = true → s.in4 = true ∨ s.in6 = true → s.sc = false)
  ∧ (s.in4 = s.in6)

def Scal (C : Cfg) (s : State) : Prop :=
  s.pw = true →
    match s.rp with
    | .none => SrcOk C s
    | .done => SrcOk C s
    | .at n => CloneOk C n s
    | .failed => FailedOk s

theorem scal_init (C : Cfg) : Scal C (init C) := by
  intro _
  simp [init, SrcOk, inPrepare]

/-- The fields `Scal` reads. A step that leaves them alone keeps `Scal`. -/
def State.scal (s : State) :=
  (s.pw, s.rp, s.sp, s.sc, s.jr, s.jrSc, s.udp, s.rcu, s.rcuOwn, s.in4, s.in6, s.out4, s.out6,
   s.frozen, s.man, s.cut, s.waiting, s.ag)

theorem scal_congr {C : Cfg} {s t : State} (h : t.scal = s.scal) (hi : Scal C s) : Scal C t := by
  simp only [State.scal, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, h11, h12, h13, h14, h15, h16, h17, h18⟩ := h
  unfold Scal SrcOk CloneOk FailedOk inPrepare at *
  rw [h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, h11, h12, h13, h14, h15, h16, h17, h18]
  exact hi

@[simp] theorem ctSet_scal (s : State) (c : Cli) (p : Port) (v : Option CtSt) :
    (s.ctSet c p v).scal = s.scal := by
  cases c <;> cases p <;> rfl

@[simp] theorem ckSet_scal (s : State) (c : Cli) (p : Port) : (s.ckSet c p).scal = s.scal := by
  cases c <;> cases p <;> rfl

@[simp] theorem add_scal (s : State) (k : Kind) (st : St) (g : Gen) :
    (s.add k st g).scal = s.scal := rfl

@[simp] theorem drop_scal (s : State) (k : Kind) : (s.drop k).scal = s.scal := rfl

@[simp] theorem upgrade_scal (s : State) (k : Kind) : (s.upgrade k).scal = s.scal := rfl

@[simp] theorem noteRst_scal (s : State) (b : Bool) : (s.noteRst b).scal = s.scal := rfl

@[simp] theorem noteLate_scal (s : State) (b : Bool) : (s.noteLate b).scal = s.scal := rfl

@[simp] theorem rejected_scal (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) :
    (rejected R C c p seg s).scal = s.scal := by
  unfold rejected
  split <;> simp

@[simp] theorem unanswered_scal (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (unanswered R C c p seg s).scal = s.scal := by
  unfold unanswered
  split <;> simp

@[simp] theorem answered_scal (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (r : Bool) (s : State) :
    (answered tr c p seg r s).scal = s.scal := by
  unfold answered
  split
  · rfl
  · split <;> simp

/-- Closes `f s = some t → t.scal = s.scal` for a step `f` that is unfolded in the goal. -/
macro "scal_same" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h)
  all_goals first
    | contradiction
    | (simp only [Option.some.injEq] at h; subst h; simp; done)
    | (simp only [Option.some.injEq] at h; subst h; rfl)
    | (simp only [Option.some.injEq] at h; subst h; split <;> simp)))

theorem pkt_scal {R : Rules} {C : Cfg} {c : Cli} {p : Port} {seg : Seg} {s t : State} :
    pkt R C c p seg s = some t → t.scal = s.scal := by
  unfold pkt
  scal_same

theorem udpC_scal {R : Rules} {C : Cfg} {s t : State} : udpC R C s = some t → t.scal = s.scal := by
  unfold udpC
  scal_same

theorem udpR_scal {C : Cfg} {s t : State} : udpR C s = some t → t.scal = s.scal := by
  unfold udpR
  scal_same

theorem udpS_scal {R : Rules} {C : Cfg} {s t : State} : udpS R C s = some t → t.scal = s.scal := by
  unfold udpS
  scal_same

theorem conn_scal {R : Rules} {C : Cfg} {w : Who} {s t : State} :
    conn R C w s = some t → t.scal = s.scal := by
  unfold conn
  scal_same

theorem shut_scal {w : Who} {s t : State} : shut w s = some t → t.scal = s.scal := by
  unfold shut
  scal_same

theorem pair_scal {C : Cfg} {b : Bool} {s t : State} : pair C b s = some t → t.scal = s.scal := by
  unfold pair
  scal_same

theorem unpair_scal {b : Bool} {s t : State} : unpair b s = some t → t.scal = s.scal := by
  unfold unpair
  scal_same

/-! ### The system's own steps -/

@[simp] theorem design_journal : Design.rules.restore.contains .journal = true := by decide

theorem lt18 {n : Nat} (h : n < 18) :
    n = 0 ∨ n = 1 ∨ n = 2 ∨ n = 3 ∨ n = 4 ∨ n = 5 ∨ n = 6 ∨ n = 7 ∨ n = 8 ∨ n = 9 ∨ n = 10
      ∨ n = 11 ∨ n = 12 ∨ n = 13 ∨ n = 14 ∨ n = 15 ∨ n = 16 ∨ n = 17 := by
  omega

set_option hygiene false in
/-- Opens `f s = some t → Scal C s → Scal C t` for a step `f` unfolded in the goal: one goal for
each way the step can succeed, with `t` replaced by the state it gives. -/
macro "open_step" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hi)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)))

/-- What `close_scal` unfolds. -/
macro "scal_simp" : tactic => `(tactic|
  simp_all [SrcOk, CloneOk, FailedOk, inPrepare, openGate, State.closeGate, State.unpublished,
    lockRestore, Design.rules, State.gated, State.running, State.dead])

set_option hygiene false in
/-- Closes `Scal C t` from `hi : Scal C s` once `t` is spelled out. For a clone in its restore
it goes through the eighteen places the restore can be when it must. -/
macro "close_scal" : tactic => `(tactic| (
  unfold Scal at hi ⊢
  intro hpw
  have hi := hi (by simp_all)
  cases hr : s.rp with
  | «at» n =>
    first
      | (scal_simp; done)
      | (have hn : n < 18 := by simp_all [CloneOk]
         rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
           | h0 | h0 | h0 | h0 <;> subst h0 <;> (try simp only [hr, RP.at.injEq] at *)
           <;> (try subst_vars) <;> scal_simp)
  | _ => scal_simp))

theorem snap_scal {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → Scal C s → Scal C t := by
  unfold snap
  open_step
  all_goals close_scal

theorem snapFail_scal {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → Scal C s → Scal C t := by
  unfold snapFail
  open_step
  all_goals close_scal

theorem snapKill_scal {C : Cfg} {s t : State} :
    snapKill C s = some t → Scal C s → Scal C t := by
  unfold snapKill
  open_step
  all_goals close_scal

theorem restore_scal {C : Cfg} {s t : State} :
    restore C s = some t → Scal C s → Scal C t := by
  unfold restore
  open_step
  all_goals close_scal

theorem kill_scal {C : Cfg} {s t : State} : kill C s = some t → Scal C s → Scal C t := by
  unfold kill
  open_step
  all_goals close_scal

theorem restart_scal {s t : State} {C : Cfg} :
    restart Design.rules C s = some t → Scal C s → Scal C t := by
  unfold restart
  open_step
  all_goals close_scal

theorem off_scal {C : Cfg} {s t : State} : off s = some t → Scal C s → Scal C t := by
  unfold off
  open_step
  all_goals close_scal

theorem timeout_scal {C : Cfg} {s t : State} : timeout s = some t → Scal C s → Scal C t := by
  unfold timeout
  open_step
  all_goals close_scal

theorem watchdog_scal {C : Cfg} {s t : State} :
    watchdog Design.rules C s = some t → Scal C s → Scal C t := by
  unfold watchdog
  open_step
  all_goals close_scal

theorem agent_pw {R : Rules} {C : Cfg} {s t : State} (h : agent R C s = some t) :
    s.pw = true := by
  unfold agent agentRuns State.running at h
  split at h
  · contradiction
  · simp_all

theorem agent_scal {C : Cfg} {s t : State} :
    agent Design.rules C s = some t → Scal C s → Scal C t := by
  intro h hi
  have hpw := agent_pw h
  unfold Scal at hi ⊢
  have hi := hi hpw
  intro hpt
  cases hr : s.rp with
  | «at» n =>
    simp only [hr] at hi
    have hn : n < 18 := hi.1
    rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
      | h0 | h0 | h0 | h0 <;> subst h0 <;>
      simp [agent, nextStep, hr, Design.rules, doStep, advance, agentRuns] at h <;>
      first
        | (obtain ⟨_, rfl⟩ := h; scal_simp; done)
        | (obtain ⟨_, h⟩ := h; split at h <;> (try simp at h) <;> (try subst h) <;> scal_simp)
  | _ => simp [agent, nextStep, hr] at h

theorem agentFail_pw {R : Rules} {C : Cfg} {s t : State} (h : agentFail R C s = some t) :
    s.pw = true := by
  unfold agentFail agentRuns State.running at h
  split at h
  · contradiction
  · simp_all

theorem agentFail_scal {C : Cfg} {s t : State} :
    agentFail Design.rules C s = some t → Scal C s → Scal C t := by
  intro h hi
  have hpw := agentFail_pw h
  unfold Scal at hi ⊢
  have hi := hi hpw
  intro hpt
  cases hr : s.rp with
  | «at» n =>
    simp only [hr] at hi
    have hn : n < 18 := hi.1
    rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
      | h0 | h0 | h0 | h0 <;> subst h0 <;>
      simp [agentFail, nextStep, hr, Design.rules, agentRuns] at h <;>
      (obtain ⟨_, rfl⟩ := h; scal_simp)
  | _ => simp [agentFail, nextStep, hr] at h

/-- Every step of the design keeps `Scal`. -/
theorem step_scal {C : Cfg} {l : Label} {s t : State}
    (h : step Design.rules C l s = some t) (hi : Scal C s) : Scal C t := by
  cases l with
  | pkt c p seg => exact scal_congr (pkt_scal h) hi
  | udpC => exact scal_congr (udpC_scal h) hi
  | udpR => exact scal_congr (udpR_scal h) hi
  | udpS => exact scal_congr (udpS_scal h) hi
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact scal_congr rfl hi
    · contradiction
  | conn w => exact scal_congr (conn_scal h) hi
  | shut w => exact scal_congr (shut_scal h) hi
  | pair b => exact scal_congr (pair_scal h) hi
  | unpair b => exact scal_congr (unpair_scal h) hi
  | snap => exact snap_scal h hi
  | snapFail => exact snapFail_scal h hi
  | snapKill => exact snapKill_scal h hi
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact scal_congr rfl hi
    · contradiction
  | guard => exact snap_scal h hi
  | restore => exact restore_scal h hi
  | agent => exact agent_scal h hi
  | agentFail => exact agentFail_scal h hi
  | kill => exact kill_scal h hi
  | restart => exact restart_scal h hi
  | off => exact off_scal h hi
  | timeout => exact timeout_scal h hi
  | watchdog => exact watchdog_scal h hi

theorem reachable_scal {C : Cfg} {s : State} (hr : Reachable Design.rules C s) : Scal C s := by
  induction hr with
  | init => exact scal_init C
  | step l _ h ih => exact step_scal h ih

/-! ## The records a step leaves -/

/-- No step has left a record of a break. The client datagram needs the host's gate
(`Cfg.udpReach = false`). -/
def Flags (C : Cfg) (s : State) : Prop :=
  s.rst = false ∧ s.eg = false ∧ s.hk = false ∧ s.cap = false ∧ s.late = false
  ∧ (C.udpReach = false → s.udpIn = false)

def State.fl (s : State) := (s.rst, s.eg, s.hk, s.cap, s.late, s.udpIn)

theorem flags_congr {C : Cfg} {s t : State} (h : t.fl = s.fl) (hi : Flags C s) : Flags C t := by
  simp only [State.fl, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, h4, h5, h6⟩ := h
  unfold Flags at *
  rw [h1, h2, h3, h4, h5, h6]
  exact hi

@[simp] theorem ctSet_fl (s : State) (c : Cli) (p : Port) (v : Option CtSt) :
    (s.ctSet c p v).fl = s.fl := by
  cases c <;> cases p <;> rfl

@[simp] theorem ckSet_fl (s : State) (c : Cli) (p : Port) : (s.ckSet c p).fl = s.fl := by
  cases c <;> cases p <;> rfl

@[simp] theorem add_fl (s : State) (k : Kind) (st : St) (g : Gen) : (s.add k st g).fl = s.fl := rfl
@[simp] theorem drop_fl (s : State) (k : Kind) : (s.drop k).fl = s.fl := rfl
@[simp] theorem upgrade_fl (s : State) (k : Kind) : (s.upgrade k).fl = s.fl := rfl

@[simp] theorem answered_fl (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (r : Bool) (s : State) :
    (answered tr c p seg r s).fl = s.fl := by
  unfold answered
  split
  · rfl
  · split <;> simp

@[simp] theorem unanswered_fl (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (unanswered R C c p seg s).fl = s.fl := by
  unfold unanswered
  split <;> simp

theorem noteLate_fl (s : State) (b : Bool) (hb : b = false) : (s.noteLate b).fl = s.fl := by
  subst hb
  simp [State.noteLate, State.fl]

theorem noteRst_fl (s : State) (b : Bool) (hb : b = false) : (s.noteRst b).fl = s.fl := by
  subst hb
  simp [State.noteRst, State.fl]

/-- The design's gate rejects nothing but a SYN, so it never resets a connection. -/
theorem rejected_fl {C : Cfg} {c : Cli} {p : Port} {seg : Seg} {s : State}
    (h : rejects Design.rules C c p seg s = true) :
    (rejected Design.rules C c p seg s).fl = s.fl := by
  have hs : (seg == Seg.syn || seg == Seg.synFull) = true := by
    simp [rejects, Design.rules] at h
    cases seg <;> simp_all
  unfold rejected
  rw [noteRst_fl _ _ (by simp [hs])]
  split <;> simp

/-- Behind a clone's closed ingress half, after its retirement, `tcp_syncookies` is 0. -/
theorem shut_cut_sc {C : Cfg} {s : State} {f : Fam} (hi : Scal C s) (hpw : s.pw = true)
    (hs : s.shutTo f = true) (hc : s.cut = true) : s.sc = false := by
  have hi := hi hpw
  unfold State.shutTo State.unpublished at hs
  cases hr : s.rp with
  | «at» n =>
    simp only [hr] at hi
    have hn : n < 18 := hi.1
    rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
      | h0 | h0 | h0 | h0 <;> subst h0 <;> cases f <;> simp_all [CloneOk, State.gateIn]
  | failed => cases f <;> simp_all [FailedOk, State.gateIn]
  | none => simp_all
  | done => simp_all

theorem pkt_flags {C : Cfg} {c : Cli} {p : Port} {seg : Seg} {s t : State}
    (hs : Scal C s) : pkt Design.rules C c p seg s = some t → t.fl = s.fl := by
  unfold pkt
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (try (simp; done))
  · rename_i hr; exact rejected_fl hr
  · -- a SYN that makes a request socket was not rejected: the ingress half is not in
    rename_i hr _
    have : (s.shutTo c.fam && s.cut) = false := by
      simp [rejects, Design.rules] at hr
      simp [State.shutTo, hr]
    simp [noteLate_fl _ _ this]
  · -- a cookie's ACK that makes a socket found `tcp_syncookies` on
    have hpw : s.pw = true := by
      cases hp : s.pw
      · simp_all [State.running]
      · rfl
    have hsc : s.sc = true := by simp_all
    have : (s.shutTo c.fam && s.cut) = false := by
      cases h1 : s.shutTo c.fam <;> cases h2 : s.cut <;> simp
      have := shut_cut_sc hs hpw h1 h2
      simp [this] at hsc
    simp [noteLate_fl _ _ this]

theorem udpC_flags {C : Cfg} {s t : State} :
    udpC Design.rules C s = some t → Flags C s → Flags C t := by
  unfold udpC
  repeat' split
  all_goals (intro h hi)
  all_goals (try contradiction)
  simp only [Option.some.injEq] at h
  subst h
  rename_i hreach _
  obtain ⟨h1, h2, h3, h4, h5, h6⟩ := hi
  refine ⟨h1, h2, h3, h4, h5, ?_⟩
  intro hu
  have h6 := h6 hu
  cases hun : s.unpublished <;> cases h4' : s.in4 <;> cases h6' : s.in6 <;>
    simp_all

set_option hygiene false in
/-- Closes `Flags C t` from `hi : Scal C s` and `hf : Flags C s` once `t` is spelled out. -/
macro "close_flags" : tactic => `(tactic| (
  unfold Scal at hi
  first
    | (simp_all [Flags, State.dead, openGate, State.closeGate]; done)
    | (have hi := hi (by simp_all)
       cases hr : s.rp <;>
         simp_all [Flags, SrcOk, CloneOk, FailedOk, inPrepare, openGate, State.closeGate,
           State.unpublished, lockRestore, Design.rules, State.gated, State.running, State.dead])))

set_option hygiene false in
/-- `open_step` with the records' invariant as a third hypothesis. -/
macro "open_step3" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hi hf)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)))

theorem snap_flags {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → Scal C s → Flags C s → Flags C t := by
  unfold snap
  open_step3
  all_goals close_flags

theorem snapFail_flags {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold snapFail
  open_step3
  all_goals close_flags

theorem snapKill_flags {C : Cfg} {s t : State} :
    snapKill C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold snapKill
  open_step3
  all_goals close_flags

theorem restore_flags {C : Cfg} {s t : State} :
    restore C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold restore
  open_step3
  all_goals close_flags

theorem kill_flags {C : Cfg} {s t : State} :
    kill C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold kill
  open_step3
  all_goals close_flags

theorem restart_flags {C : Cfg} {s t : State} :
    restart Design.rules C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold restart
  open_step3
  all_goals close_flags

theorem off_flags {C : Cfg} {s t : State} :
    off s = some t → Scal C s → Flags C s → Flags C t := by
  unfold off
  open_step3
  all_goals close_flags

theorem timeout_flags {C : Cfg} {s t : State} :
    timeout s = some t → Scal C s → Flags C s → Flags C t := by
  unfold timeout
  open_step3
  all_goals close_flags

theorem watchdog_flags {C : Cfg} {s t : State} :
    watchdog Design.rules C s = some t → Scal C s → Flags C s → Flags C t := by
  unfold watchdog
  open_step3
  all_goals close_flags

theorem agent_flags {C : Cfg} {s t : State} :
    agent Design.rules C s = some t → Scal C s → Flags C s → Flags C t := by
  intro h hi hf
  have hpw := agent_pw h
  have hi := hi hpw
  cases hr : s.rp with
  | «at» n =>
    simp only [hr] at hi
    have hn : n < 18 := hi.1
    rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
      | h0 | h0 | h0 | h0 <;> subst h0 <;>
      simp [agent, nextStep, hr, Design.rules, doStep, advance, agentRuns] at h <;>
      first
        | (obtain ⟨_, rfl⟩ := h; simp_all [Flags, CloneOk, openGate, State.closeGate]; done)
        | (obtain ⟨_, h⟩ := h; split at h <;> (try simp at h) <;> (try subst h) <;>
            simp_all [Flags, CloneOk, openGate, State.closeGate])
  | _ => simp [agent, nextStep, hr] at h

theorem advance_fl (R : Rules) (s : State) : (advance R s).fl = s.fl := by
  unfold advance
  split
  · split <;> rfl
  · rfl

theorem agentFail_flags {C : Cfg} {s t : State} :
    agentFail Design.rules C s = some t → Flags C s → Flags C t := by
  intro h hf
  cases hr : s.rp with
  | «at» n =>
    unfold agentFail at h
    split at h
    · contradiction
    · split at h
      · contradiction
      · split at h
        · contradiction
        · simp only [Option.some.injEq] at h; subst h
          exact flags_congr (by rw [advance_fl]; rfl) hf
      · contradiction
      · simp only [Option.some.injEq] at h; subst h; simpa [Flags] using hf
  | _ =>
    unfold agentFail nextStep at h
    simp [hr] at h

/-- The environment's other steps leave the records alone. -/
macro "fl_same" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h)
  all_goals first
    | contradiction
    | (simp only [Option.some.injEq] at h; subst h; simp; done)
    | (simp only [Option.some.injEq] at h; subst h; rfl)))

theorem udpR_fl {C : Cfg} {s t : State} : udpR C s = some t → t.fl = s.fl := by
  unfold udpR
  fl_same

theorem udpS_fl {R : Rules} {C : Cfg} {s t : State} : udpS R C s = some t → t.fl = s.fl := by
  unfold udpS
  fl_same

theorem conn_fl {R : Rules} {C : Cfg} {w : Who} {s t : State} :
    conn R C w s = some t → t.fl = s.fl := by
  unfold conn
  fl_same

theorem shut_fl {w : Who} {s t : State} : shut w s = some t → t.fl = s.fl := by
  unfold shut
  fl_same

theorem pair_fl {C : Cfg} {b : Bool} {s t : State} : pair C b s = some t → t.fl = s.fl := by
  unfold pair
  fl_same

theorem unpair_fl {b : Bool} {s t : State} : unpair b s = some t → t.fl = s.fl := by
  unfold unpair
  fl_same

/-- Every step of the design keeps `Flags`. -/
theorem step_flags {C : Cfg} {l : Label} {s t : State}
    (h : step Design.rules C l s = some t) (hi : Scal C s) (hf : Flags C s) : Flags C t := by
  cases l with
  | pkt c p seg => exact flags_congr (pkt_flags hi h) hf
  | udpC => exact udpC_flags h hf
  | udpR => exact flags_congr (udpR_fl h) hf
  | udpS => exact flags_congr (udpS_fl h) hf
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact flags_congr rfl hf
    · contradiction
  | conn w => exact flags_congr (conn_fl h) hf
  | shut w => exact flags_congr (shut_fl h) hf
  | pair b => exact flags_congr (pair_fl h) hf
  | unpair b => exact flags_congr (unpair_fl h) hf
  | snap => exact snap_flags h hi hf
  | snapFail => exact snapFail_flags h hi hf
  | snapKill => exact snapKill_flags h hi hf
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact flags_congr rfl hf
    · contradiction
  | guard => exact snap_flags h hi hf
  | restore => exact restore_flags h hi hf
  | agent => exact agent_flags h hi hf
  | agentFail => exact agentFail_flags h hf
  | kill => exact kill_flags h hi hf
  | restart => exact restart_flags h hi hf
  | off => exact off_flags h hi hf
  | timeout => exact timeout_flags h hi hf
  | watchdog => exact watchdog_flags h hi hf

theorem flags_init (C : Cfg) : Flags C (init C) := by
  simp [Flags, init]

theorem reachable_flags {C : Cfg} {s : State}
    (hr : Reachable Design.rules C s) : Flags C s := by
  induction hr with
  | init => exact flags_init C
  | step l hs h ih => exact step_flags h (reachable_scal hs) ih

end Gate
