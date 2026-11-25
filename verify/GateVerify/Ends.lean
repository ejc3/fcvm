import GateVerify.Socks
import GateVerify.Configs

/-!
# Every boundary ends: a measure the system's own steps decrease

`design_neverStuck` (`Proofs.lean`) says a step of the system is enabled while a gate is closed.
That is progress, not an end: it does not rule out a run that goes round for ever. This file
gives the end.

`measure` is a number read off a state, at most 34. Every step of the system itself makes it
smaller (`own_decreases`). A step of the environment that begins nothing leaves it where it was
or lower (`calm_keeps`). Three moves of the environment raise it, each by a bounded amount: the
user's next `snapshot create` (by 12), a clone restored from the image (to at most 21), and
fc-agent killed (by at most 1). So a run holds at most

    measure at its start + kills + 12 × snapshot creates begun + 21 × clones restored

steps of the system (`design_bounded`). And a reachable state in which no step of the system is
enabled, the host's timeout and the failures not counted, is settled: powered off, or no
snapshot under way, no restore under way, the agent alive and the gate open (`design_settles`).

The kill is the one move that makes "at most N steps" false as a bound with no condition: a
kill before the restore's first step has landed is followed by a restart that begins the
restore again, and the two can repeat for ever (`kill_restart_repeats`). With at most `k` kills
the bound is 34 + k.

What the theorems do not say: that a step which stays enabled is taken. That the guest's
scheduler runs the agent, systemd restarts it, and the host's `snapshot create` runs on, is
assumed.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-- The steps a `snapshot create` has left, at most. -/
def snapRank : SP → Nat
  | .idle => 0
  | .opened => 1
  | .aborted => 2
  | .resumed => 3
  | .paused => 4
  | .saved => 5
  | .dump6 => 6
  | .dump4 => 7
  | .bar => 8
  | .barIn => 9
  | .closed => 10
  | .closed4 => 11
  | .frozen => 12

/-- The steps a restore has left, at most: its own steps, then a failure's power-off; with the
agent dead, the restart first. -/
def restRank (s : State) : Nat :=
  match s.rp with
  | .none | .done => if s.ag then 0 else 1
  | .failed => if s.ag then 1 else 2
  | .at n => if s.ag then 20 - n else if s.jr then 2 else 21

/-- The steps of the system a state has left, at most, if the environment begins nothing new. -/
def measure (s : State) : Nat :=
  if s.pw then snapRank s.sp + restRank s + (if s.waiting then 1 else 0) else 0

/-- A step of the system itself: a step of a `snapshot create` that has begun, the guard's, a
restore's, either of them failing, systemd's restart of the agent, the failed clone's power-off
and the host's timeout. -/
def own (l : Label) (s : State) : Bool :=
  match l with
  | .snap => s.sp != .idle
  | .guard | .snapFail | .agent | .agentFail | .restart | .off | .timeout => true
  | _ => false

/-- The user begins a `snapshot create`. -/
def begins (l : Label) (s : State) : Bool := l == .snap && s.sp == .idle

/-- The steps that move a boundary on, with no failure and no timeout among them. -/
def moves : List Label := [.snap, .guard, .agent, .restart, .off]

/-- How many steps of a run satisfy `f`. -/
def tally (f : Label → State → Bool) (R : Rules) (C : Cfg) : List Label → State → Nat
  | [], _ => 0
  | l :: ls, s =>
    match step R C l s with
    | some t => (if f l s then 1 else 0) + tally f R C ls t
    | none => 0

theorem snapRank_le (p : SP) : snapRank p ≤ 12 := by
  cases p <;> simp [snapRank]

theorem restRank_le (s : State) : restRank s ≤ 21 := by
  unfold restRank
  repeat' split
  all_goals omega

theorem measure_le (s : State) : measure s ≤ 34 := by
  have h1 := snapRank_le s.sp
  have h2 := restRank_le s
  unfold measure
  repeat' split
  all_goals omega

theorem measure_congr {s t : State} (h : t.scal = s.scal) : measure t = measure s := by
  simp only [State.scal, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, -, h5, -, -, -, -, -, -, -, -, -, -, -, h17, h18⟩ := h
  simp [measure, restRank, h1, h2, h3, h5, h17, h18]

theorem doStep_keeps (R : Rules) (C : Cfg) (r : RStep) (s : State) :
    (doStep R C r s).pw = s.pw ∧ (doStep R C r s).sp = s.sp ∧ (doStep R C r s).ag = s.ag
      ∧ (doStep R C r s).waiting = s.waiting ∧ (doStep R C r s).rp = s.rp := by
  cases r <;> simp [doStep, State.closeGate, openGate] <;> split <;> simp

/-! ## The system's own steps make the measure smaller -/

theorem snap_decreases {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → (g = true ∨ s.sp ≠ .idle) → measure t < measure s := by
  unfold snap
  repeat' split
  all_goals (intro h hne)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)
  all_goals (simp_all [measure, restRank, snapRank, openGate, State.closeGate])
  all_goals (try omega)

theorem snapFail_decreases {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → measure t < measure s := by
  unfold snapFail
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)
  all_goals (simp_all [measure, restRank, snapRank, openGate, State.closeGate, Design.rules])
  all_goals (try omega)

theorem agent_decreases {C : Cfg} {s t : State} (h : agent Design.rules C s = some t) :
    measure t < measure s := by
  have hpw := agent_pw h
  obtain ⟨n, r, hr, hnr, hag, rfl⟩ := agent_spec h
  have hn : n < 18 := by
    have := (List.getElem?_eq_some_iff.mp hnr).1
    simpa [Design.rules] using this
  obtain ⟨k1, k2, k3, k4, k5⟩ := doStep_keeps Design.rules C r s
  split
  · simp [measure, restRank, hpw, hr, hag]
    omega
  · have hlen : Design.rules.restore.length = 18 := by decide
    simp only [advance, k5, hr, hlen]
    split
    · simp [measure, restRank, hpw, hr, hag, k1, k2, k3, k4]
      omega
    · simp [measure, restRank, hpw, hr, hag, k1, k2, k3, k4]
      omega

theorem agentFail_decreases {C : Cfg} {s t : State} :
    agentFail Design.rules C s = some t → measure t < measure s := by
  intro h
  have hpw := agentFail_pw h
  revert h
  unfold agentFail
  split
  · intro h; contradiction
  · rename_i hrun
    have hag : s.ag = true := by simp [agentRuns] at hrun; exact hrun.1.1.2
    cases hr : s.rp with
    | «at» n =>
      simp only [nextStep, hr]
      cases hnr : Design.rules.restore[n]? with
      | none => intro h; contradiction
      | some r =>
        have hn : n < 18 := by
          have := (List.getElem?_eq_some_iff.mp hnr).1
          simpa [Design.rules] using this
        cases r <;> simp [Design.rules] <;>
          (try (intro h; subst h; simp [measure, restRank, hpw, hr, hag]; omega))
    | _ => simp [nextStep, hr]

theorem restart_decreases {C : Cfg} {s t : State} :
    restart Design.rules C s = some t → measure t < measure s := by
  unfold restart
  intro h
  cases hr : s.rp <;> cases hj : s.jr <;> cases hp : s.pw <;> cases ha : s.ag <;>
    simp [hr, hj, hp, ha, Design.rules] at h <;> subst h <;>
    simp [measure, restRank, hr, hj, hp, ha]

theorem off_decreases {s t : State} : off s = some t → measure t < measure s := by
  unfold off
  split
  · rename_i hc
    intro h
    simp only [Option.some.injEq] at h
    subst h
    simp at hc
    simp [measure, restRank, State.dead, hc]
    omega
  · intro h; contradiction

theorem timeout_decreases {s t : State} : timeout s = some t → measure t < measure s := by
  unfold timeout
  split
  · rename_i hc
    intro h
    simp only [Option.some.injEq] at h
    subst h
    simp at hc
    simp [measure, State.dead, hc]
  · intro h; contradiction

/-- Every step of the system itself makes the measure smaller. No hypothesis on the state. -/
theorem own_decreases {C : Cfg} {l : Label} {s t : State} (ho : own l s = true)
    (h : step Design.rules C l s = some t) : measure t < measure s := by
  cases l <;> simp [own] at ho
  · exact snap_decreases h (Or.inr ho)
  · exact snapFail_decreases h
  · exact snap_decreases h (Or.inl rfl)
  · exact agent_decreases h
  · exact agentFail_decreases h
  · exact restart_decreases h
  · exact off_decreases h
  · exact timeout_decreases h

/-! ## The environment's moves -/

theorem snapKill_keeps {C : Cfg} {s t : State} :
    snapKill C s = some t → measure t ≤ measure s := by
  unfold snapKill
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (simp_all [measure, restRank, snapRank])
  all_goals (try omega)

theorem watchdog_keeps {R : Rules} {C : Cfg} {s t : State} :
    watchdog R C s = some t → measure t = measure s := by
  unfold watchdog
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h; rfl)

/-- The user's `snapshot create` begins: twelve steps to its end. -/
theorem begin_raises {C : Cfg} {s t : State} (h : snap Design.rules C false s = some t)
    (hi : s.sp = .idle) : measure t = measure s + 12 := by
  revert h
  unfold snap
  simp only [hi]
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (simp_all [measure, restRank, snapRank])
  all_goals (try omega)

/-- A clone is restored: at most twenty-one steps to its end. -/
theorem restore_raises {C : Cfg} {s t : State} (h : restore C s = some t) : measure t ≤ 21 := by
  revert h
  unfold restore
  split
  · intro h
    simp only [Option.some.injEq] at h
    subst h
    simp only [measure, restRank, snapRank]
    repeat' split
    all_goals (simp_all)
    all_goals (try omega)
  · intro h; contradiction

/-- fc-agent is killed: one step more, the restart. In the design a restore killed after its
first step has the record on disk, so the restart fails it and it is two steps from its end. -/
theorem kill_raises {C : Cfg} {s t : State} (hs : Scal C s) (h : kill C s = some t) :
    measure t ≤ measure s + 1 := by
  revert h
  unfold kill
  split
  · intro h; contradiction
  · rename_i hc
    have hpw : s.pw = true := by simp at hc; exact hc.1.1
    have hag : s.ag = true := by simp at hc; exact hc.1.2
    have hi := hs hpw
    cases hr : s.rp with
    | «at» n =>
      simp only [hr] at hi
      have hn : n < 18 := hi.1
      have hj : 1 ≤ n → s.jr = true := fun h1 => (hi.2.2.2.2.2.2.2.2.2.2.2.1 h1).1
      simp only [Option.some.injEq]
      intro h
      subst h
      simp only [measure, restRank, hpw, hr, hag]
      cases hjr : s.jr
      · have h0 : n = 0 := by
          cases n with
          | zero => rfl
          | succ m => have := hj (by omega); rw [hjr] at this; cases this
        subst h0
        simp
        omega
      · simp
        omega
    | done =>
      simp only []
      split
      · intro h
        simp only [Option.some.injEq] at h
        subst h
        simp [measure, restRank, hpw, hr, hag]
        omega
      · intro h; contradiction
    | none => intro h; simp at h
    | failed => intro h; simp at h

/-- One step of a run, accounted: a step of the system pays one out of the measure; a kill, a
`snapshot create` begun and a clone restored pay in 1, 12 and 21; nothing else pays in. -/
theorem step_account {C : Cfg} {l : Label} {s t : State} (hs : Scal C s)
    (h : step Design.rules C l s = some t) :
    (if own l s then 1 else 0) + measure t
      ≤ measure s + (if l == .kill then 1 else 0) + 12 * (if begins l s then 1 else 0)
        + 21 * (if l == .restore then 1 else 0) := by
  cases l with
  | pkt c p seg => simp [own, begins, measure_congr (pkt_scal h)]
  | udpC => simp [own, begins, measure_congr (udpC_scal h)]
  | udpR => simp [own, begins, measure_congr (udpR_scal h)]
  | udpS => simp [own, begins, measure_congr (udpS_scal h)]
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      simp [own, begins, measure, restRank]
    · contradiction
  | conn w => simp [own, begins, measure_congr (conn_scal h)]
  | shut w => simp [own, begins, measure_congr (shut_scal h)]
  | pair b => simp [own, begins, measure_congr (pair_scal h)]
  | unpair b => simp [own, begins, measure_congr (unpair_scal h)]
  | snap =>
    cases hi : s.sp == .idle
    · have hne : s.sp ≠ .idle := by simpa using hi
      have := snap_decreases h (Or.inr hne)
      simp [own, begins, hne]
      omega
    · have he : s.sp = .idle := by simpa using hi
      have := begin_raises h he
      simp [own, begins, he]
      omega
  | snapFail =>
    have := snapFail_decreases h
    simp [own, begins]
    omega
  | snapKill =>
    have := snapKill_keeps h
    simp [own, begins]
    omega
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      simp [own, begins, measure, restRank]
    · contradiction
  | guard =>
    have := snap_decreases h (Or.inl rfl)
    simp [own, begins]
    omega
  | restore =>
    have := restore_raises h
    simp [own, begins]
    omega
  | agent =>
    have := agent_decreases h
    simp [own, begins]
    omega
  | agentFail =>
    have := agentFail_decreases h
    simp [own, begins]
    omega
  | kill =>
    have := kill_raises hs h
    simp [own, begins]
    omega
  | restart =>
    have := restart_decreases h
    simp [own, begins]
    omega
  | off =>
    have := off_decreases h
    simp [own, begins]
    omega
  | timeout =>
    have := timeout_decreases h
    simp [own, begins]
    omega
  | watchdog => simp [own, begins, watchdog_keeps h]

/-- A step of the environment that begins nothing and kills nothing leaves the measure where it
was, or lower. -/
theorem calm_keeps {C : Cfg} {l : Label} {s t : State} (hs : Scal C s)
    (h : step Design.rules C l s = some t) (ho : own l s = false) (hb : begins l s = false)
    (hk : l ≠ .kill) (hr : l ≠ .restore) : measure t ≤ measure s := by
  have := step_account hs h
  simp [ho, hb, hk, hr] at this
  exact this

/-- **P6, every boundary ends.** In any run from a reachable state, the steps of the system
number at most the measure at the start, plus one for each kill of fc-agent, twelve for each
`snapshot create` begun and twenty-one for each clone restored. -/
theorem design_bounded {C : Cfg} {ls : List Label} {s t : State}
    (hr : Reachable Design.rules C s) (h : replay Design.rules C ls s = some t) :
    tally own Design.rules C ls s + measure t
      ≤ measure s + tally (fun l _ => l == .kill) Design.rules C ls s
        + 12 * tally begins Design.rules C ls s
        + 21 * tally (fun l _ => l == .restore) Design.rules C ls s := by
  induction ls generalizing s with
  | nil =>
    simp only [replay, Option.some.injEq] at h
    subst h
    simp [tally]
  | cons l ls ih =>
    cases hst : step Design.rules C l s with
    | none => simp [replay, hst] at h
    | some u =>
      simp only [replay, hst] at h
      have h1 := step_account (reachable_scal hr) hst
      have h2 := ih (Reachable.step l hr hst) h
      simp only [tally, hst]
      omega

/-- With the environment beginning nothing and killing nothing, a run holds at most 34 steps of
the system. -/
theorem design_ends {C : Cfg} {ls : List Label} {s t : State}
    (hr : Reachable Design.rules C s) (h : replay Design.rules C ls s = some t)
    (hk : tally (fun l _ => l == .kill) Design.rules C ls s = 0)
    (hb : tally begins Design.rules C ls s = 0)
    (hc : tally (fun l _ => l == .restore) Design.rules C ls s = 0) :
    tally own Design.rules C ls s ≤ 34 := by
  have h1 := design_bounded hr h
  have h2 := measure_le s
  omega

/-- One restore: from the clone's first moment, with no kill of the agent, no `snapshot create`
begun and no further clone restored from it, a run holds at most 21 steps of the system. -/
theorem design_restore_ends {C : Cfg} {ls : List Label} {s u t : State}
    (hr : Reachable Design.rules C s) (h0 : step Design.rules C .restore s = some u)
    (h : replay Design.rules C ls u = some t)
    (hk : tally (fun l _ => l == .kill) Design.rules C ls u = 0)
    (hb : tally begins Design.rules C ls u = 0)
    (hc : tally (fun l _ => l == .restore) Design.rules C ls u = 0) :
    tally own Design.rules C ls u ≤ 21 := by
  have h1 := design_bounded (Reachable.step .restore hr h0) h
  have h2 : measure u ≤ 21 := restore_raises h0
  omega

/-! ## Where a run ends -/

/-- No step that moves a boundary on is enabled. The host's timeout and the failures are not
asked: with them left out the claim below is the stronger one. -/
def atRest (R : Rules) (C : Cfg) (s : State) : Bool :=
  !(moves.any (fun l => own l s && (step R C l s).isSome))

/-- Powered off, or: no snapshot under way, no restore under way, the agent alive, no half of
the gate installed. -/
def settled (s : State) : Bool :=
  !s.pw || (s.sp == .idle && !s.unpublished && s.ag && !s.gated)

theorem design_next {n : Nat} (h : n < 18) : ∃ r, Design.rules.restore[n]? = some r :=
  ⟨_, List.getElem?_eq_getElem (by simpa [Design.rules] using h)⟩

/-- **P6, where a boundary ends.** A reachable state in which no step moves a boundary on is
settled: the guest is powered off (a failed restore's `shutdown_vm`, or the host's timeout), or
it runs with its gate open, its agent alive, and neither a snapshot nor a restore under way. -/
theorem design_settles {C : Cfg} {s : State} (hr : Reachable Design.rules C s)
    (h : atRest Design.rules C s = true) : settled s = true := by
  have hi := reachable_scal hr
  unfold settled
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hag : s.ag
    · -- the agent is dead: systemd's restart is enabled
      exfalso
      simp [atRest, moves, own, step, restart, hpw, hag, Design.rules] at h
      cases hrp : s.rp <;> simp [hrp] at h
    · cases hrp : s.rp with
      | «at» n =>
        exfalso
        simp only [hrp] at hi
        obtain ⟨r, hnr⟩ := design_next hi.1
        have hsp : s.sp = .idle := hi.2.1
        simp [atRest, moves, own, step, agent, agentRuns, State.running, lockOneShot, nextStep,
          hpw, hag, hrp, hsp, hnr] at h
        split at h <;> simp at h
      | failed =>
        exfalso
        simp [atRest, moves, own, step, off, hpw, hag, hrp] at h
      | none =>
        cases hsp : s.sp <;> cases hh : s.host <;>
          simp_all [SrcOk, State.gated, State.unpublished, atRest, moves, own, step, snap,
            lockRestore, Design.rules, inPrepare]
      | done =>
        cases hsp : s.sp <;> cases hh : s.host <;>
          simp_all [SrcOk, State.gated, State.unpublished, atRest, moves, own, step, snap,
            lockRestore, Design.rules, inPrepare]

/-- A settled state that waits for nothing has measure 0: nothing of the system is left to run. -/
theorem settled_measure {s : State} (h : settled s = true) (hw : s.waiting = false) :
    measure s = 0 := by
  unfold settled at h
  cases hpw : s.pw
  · simp [measure, hpw]
  · cases hrp : s.rp <;>
      simp_all [measure, restRank, snapRank, State.unpublished]

/-! ## The kill -/

/-- Before the restore's first step has landed there is no record on disk, so a killed agent's
restart begins the restore again, and the state it reaches can be killed again. -/
theorem kill_restart {C : Cfg} {s : State} (hk : C.kills = true) (hp : s.pw = true)
    (ha : s.ag = true) (hr : s.rp = .at 0) (hj : s.jr = false) :
    ∃ t u, step Design.rules C .kill s = some t ∧ step Design.rules C .restart t = some u
      ∧ u.pw = true ∧ u.ag = true ∧ u.rp = .at 0 ∧ u.jr = false := by
  refine ⟨{ s with ag := false, saved := none, seen := [], lst := [], rcuOwn := false },
    { s with ag := true, rp := .at 0, saved := none, seen := [], lst := [], rcuOwn := false },
    ?_, ?_, hp, rfl, rfl, hj⟩
  · simp [step, kill, hk, hp, ha, hr]
  · simp [step, restart, hp, hj, hr, Design.rules]

/-- So no number bounds the system's steps when kills are not bounded: for every `k` there is a
run of `k` kills that holds `k` steps of the system and ends where it began. -/
theorem kill_restart_repeats {C : Cfg} (hk : C.kills = true) (k : Nat) :
    ∀ s : State, s.pw = true → s.ag = true → s.rp = .at 0 → s.jr = false →
      ∃ ls t, replay Design.rules C ls s = some t
        ∧ tally (fun l _ => l == .kill) Design.rules C ls s = k
        ∧ tally own Design.rules C ls s = k
        ∧ t.pw = true ∧ t.ag = true ∧ t.rp = .at 0 ∧ t.jr = false := by
  induction k with
  | zero =>
    intro s hp ha hr hj
    exact ⟨[], s, rfl, rfl, rfl, hp, ha, hr, hj⟩
  | succ k ih =>
    intro s hp ha hr hj
    obtain ⟨t, u, h1, h2, up, ua, ur, uj⟩ := kill_restart hk hp ha hr hj
    obtain ⟨ls, v, h3, h4, h5, vp, va, vr, vj⟩ := ih u up ua ur uj
    refine ⟨.kill :: .restart :: ls, v, ?_, ?_, ?_, vp, va, vr, vj⟩
    · simp [replay, h1, h2, h3]
    · simp [tally, h1, h2, h4]
      omega
    · simp [tally, h1, h2, h5, own]
      omega

/-! ## The theorems are about something

A hypothesis no state meets would make `design_settles` empty, and a measure that is never
reached would make the bounds loose. Three runs, replayed by the kernel. -/

set_option maxRecDepth 100000

/-- A snapshot up to the pause, a clone restored, and its restore to the end: 28 steps. -/
def Run.published : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

/-- A guest that no boundary holds is at rest and settled. -/
theorem restAtBoot :
    atRest Design.rules Runs.routed (init Runs.routed) = true
      ∧ settled (init Runs.routed) = true ∧ measure (init Runs.routed) = 0 := by decide

/-- A clone that has published is at rest and settled, with nothing left to run. -/
theorem restWhenPublished :
    after (replay Design.rules Runs.routed Run.published (init Runs.routed))
      (fun s => atRest Design.rules Runs.routed s && settled s && s.rp == .done
        && measure s == 0) := by decide

/-- The bounds are met: a `snapshot create` begun has measure 12, a clone just restored 21, and
the restore's run holds 18 steps of the system after the snapshot's 8. -/
theorem boundsMet :
    after (replay Design.rules Runs.routed [.snap] (init Runs.routed)) (fun s => measure s == 12)
      ∧ after (replay Design.rules Runs.routed (Run.published.take 10) (init Runs.routed))
        (fun s => measure s == 21)
      ∧ tally own Design.rules Runs.routed Run.published (init Runs.routed) = 26
      ∧ tally begins Design.rules Runs.routed Run.published (init Runs.routed) = 1 := by decide

end Gate
