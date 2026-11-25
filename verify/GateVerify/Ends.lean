import GateVerify.Socks
import GateVerify.Configs

/-!
# Every boundary ends: a measure the system's own steps decrease

`design_neverStuck` (`Proofs.lean`) says a step of the system is enabled while a gate is closed.
That is progress, not an end: it does not rule out a run that goes round for ever. This file
gives the end, for the states the design can reach.

`measure` is a number read off a state, at most 23 in a reachable one. Every step of the system
itself makes it smaller (`own_decreases`): a step of a `snapshot create` that has begun, the
guard's, a restore's, a failed try of either (the reopen and the write of `tcp_syncookies` back
are each tried again at most `retryMax` times), systemd's restart of the agent, the restarted
agent's start of the container, a failed clone's power-off, the host's timeout. A step of the
environment that begins nothing leaves it where it was or lower (`calm_keeps`). Three moves
raise it, each by a bounded amount: the user's next `snapshot create` (by 20), a clone restored
from the image (to at most 22), and fc-agent killed in a guest that is served a boot plan (by
2: the restart, and the container's start). So a run holds at most

    measure at its start + 2 × such kills + 20 × snapshot creates begun + 22 × clones restored

steps of the system (`design_bounded`). A kill in a clone adds nothing: a guest restored from a
memory image is served no boot plan, so its agent never comes back (`clone_never_restarts`),
and one restore is over within 22 steps of the system whatever is killed
(`design_restore_ends`).

Where a run ends (`design_settles`): a reachable state in which no step moves a boundary on,
the host's timeout not counted, is settled (powered off; or no snapshot and no restore under
way, and no half of the gate installed unless the host has reported the guest as failed and
still gated), or it is a clone that has not published and whose fc-agent is dead, for which the
host's timeout is enabled.

What the theorems do not say: that a step which stays enabled is taken. That the guest's
scheduler runs the agent, systemd restarts it, and the host's `snapshot create` runs on, is
assumed. And the bounds count steps of the model, not time.
-/

namespace Gate

set_option maxRecDepth 100000
set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-- The steps a `snapshot create` has left, at most. `r` is the tries its reopen has left. -/
def snapRank (p : SP) (r : Nat) : Nat :=
  match p with
  | .idle => 0
  | .opened => r + 1
  | .aborted | .resumed => r + 5
  | .paused => 9
  | .saved => r + 10
  | .dump6 => 14
  | .dump4 => 15
  | .bar => 16
  | .barIn => 17
  | .closed => 18
  | .closed4 => 19
  | .frozen => 20

/-- The steps a restore has left, at most, with the agent alive: its own steps, the tries its
write-back has left, a failure's power-off; and the whole of a second restore when the image
was taken inside one. -/
def agentRank (s : State) : Nat :=
  match s.rp with
  | .none | .done => 0
  | .failed => 1
  | .at n => (16 - n) + (retryMax - s.tries) + 1 + (if s.again then 21 else 0)

/-- With the container down, its start; with the agent dead, the restart first, in a guest that
is served a boot plan, and nothing in one that is not. -/
def restRank (s : State) : Nat :=
  if s.ag then agentRank s + (if s.up then 0 else 1)
  else if s.plan then agentRank s + 2 else 0

/-- The steps of the system a state has left, at most, if the environment begins nothing new. -/
def measure (s : State) : Nat :=
  if s.pw then snapRank s.sp (retryMax - s.tries) + restRank s + (if s.waiting then 1 else 0)
  else 0

/-- The user begins a `snapshot create`. -/
def begins (l : Label) (s : State) : Bool := l == .snap && s.sp == .idle

/-- fc-agent is killed in a guest that is served a boot plan, so systemd's restart brings it
back. -/
def killsBooted (l : Label) (s : State) : Bool := l == .kill && s.plan

/-- How many steps of a run satisfy `f`. -/
def tally (f : Label → State → Bool) (R : Rules) (C : Cfg) : List Label → State → Nat
  | [], _ => 0
  | l :: ls, s =>
    match step R C l s with
    | some t => (if f l s then 1 else 0) + tally f R C ls t
    | none => 0

theorem tally_cons {f : Label → State → Bool} {R : Rules} {C : Cfg} {l : Label}
    {ls : List Label} {s u : State} (h : step R C l s = some u) :
    tally f R C (l :: ls) s = (if f l s then 1 else 0) + tally f R C ls u := by
  simp [tally, h]

theorem measure_congr {s t : State} (h : t.scal = s.scal) : measure t = measure s := by
  simp only [State.scal, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, -, -, -, -, -, -, -, -, -, -, -, h15, h16, h17, h18, -, h20, h21, -⟩ := h
  simp [measure, restRank, agentRank, h1, h2, h3, h15, h16, h17, h18, h20, h21]

/-- In a reachable state the measure is at most 23. -/
theorem measure_le {C : Cfg} {s : State} (hs : Scal C s) : measure s ≤ 23 := by
  unfold measure
  cases hpw : s.pw
  · simp
  · have hi := hs hpw
    cases hr : s.rp with
    | «at» n =>
      simp only [hr] at hi
      simp [restRank, agentRank, hr, hi.idle, hi.noPlan, hi.once, snapRank, retryMax]
      repeat' split
      all_goals omega
    | failed =>
      simp only [hr] at hi
      simp [restRank, agentRank, hr, hi.1, hi.2.2.1, snapRank]
      repeat' split
      all_goals omega
    | none =>
      cases hsp : s.sp <;> simp [restRank, agentRank, hr, hsp, snapRank, retryMax] <;>
        (repeat' split) <;> omega
    | done =>
      cases hsp : s.sp <;> simp [restRank, agentRank, hr, hsp, snapRank, retryMax] <;>
        (repeat' split) <;> omega

/-! ## The system's own steps make the measure smaller -/

set_option hygiene false in
/-- Opens `f s = some t → Scal C s → …` for a step `f` unfolded in the goal. -/
macro "open_m" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hi)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)))

set_option hygiene false in
/-- Closes a goal about the measure of a state spelled out from `s`, from `hi : Scal C s`. In a
reachable state a snapshot in progress and a restore in progress exclude each other, which is
what lets the two halves of the measure share the count of tries. -/
macro "close_m" : tactic => `(tactic| (
  have hi := hi (by simp_all [State.running])
  cases hr : s.rp <;> cases hag : s.ag <;> cases hpl : s.plan <;> cases hup : s.up <;>
    simp_all [measure, restRank, agentRank, snapRank, retryMax, SrcOk, CloneOk, FailedOk,
      openGate, State.closeGate, State.dead, reopened, unreached, gaveUp, Design.rules,
      inPrepare, lockRestore, State.unpublished, State.running, Nat.not_le] <;>
    (try simp only [decide_eq_false_iff_not, decide_eq_true_eq, Nat.not_le] at *) <;>
    (try omega) <;> (try (repeat' split) <;> omega)))

set_option maxHeartbeats 1600000 in
-- thirteen places a `snapshot create` can be, times what the agent, its plan and the container are
theorem snap_decreases {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → Scal C s → (g = true ∨ s.sp ≠ .idle) →
      measure t < measure s := by
  unfold snap
  repeat' split
  all_goals (intro h hi hne)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)
  all_goals close_m
  all_goals (have h3 : ¬ (3 ≤ s.tries) := of_decide_eq_false (by assumption); omega)

theorem snapFail_decreases {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → Scal C s → measure t < measure s := by
  unfold snapFail
  open_m
  all_goals close_m

theorem agent_decreases {C : Cfg} {s t : State} (h : agent Design.rules C s = some t)
    (hs : Scal C s) : measure t < measure s := by
  have hpw := agent_pw h
  obtain ⟨n, r, hr, hnr, hag, ht⟩ := agent_spec h
  have hi := hs hpw
  simp only [hr] at hi
  have hn : n < 16 := hi.lt
  have hsp := hi.idle
  have hon := hi.once
  rcases ht with rfl | rfl
  · simp [measure, restRank, agentRank, hpw, hr, hag, hsp, snapRank]
    omega
  · rcases lt16 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
      | h0 | h0 <;> subst h0 <;> simp [Design.rules, Today.cookieB] at hnr <;> subst hnr <;>
      simp [measure, restRank, agentRank, advance, doStep, hpw, hr, hag, hsp, hon, snapRank,
        Design.rules, Today.cookieB, openGate, State.closeGate, retryMax] <;>
      (try omega) <;> (try (repeat' split) <;> omega)

theorem agentFail_decreases {C : Cfg} {s t : State} (h : agentFail Design.rules C s = some t)
    (hs : Scal C s) : measure t < measure s := by
  have hpw := agentFail_pw h
  have hi := hs hpw
  revert h
  unfold agentFail
  split
  · intro h; contradiction
  · rename_i hrun
    have hag : s.ag = true := by simp [agentRuns] at hrun; exact hrun.1.1.2
    cases hr : s.rp with
    | «at» n =>
      simp only [hr] at hi
      have hn : n < 16 := hi.lt
      have hsp := hi.idle
      simp only [nextStep, hr]
      cases hnr : Design.rules.restore[n]? with
      | none => intro h; contradiction
      | some r =>
        cases r <;> simp [Design.rules] <;>
          (try (intro h; subst h
                simp [measure, restRank, agentRank, hpw, hr, hag, hsp, snapRank]; omega)) <;>
          (try (intro h1 h; subst h
                simp [measure, restRank, agentRank, hpw, hr, hag, hsp, snapRank, retryMax] at *
                omega))
    | _ => simp [nextStep, hr]

theorem restart_decreases {C : Cfg} {s t : State} :
    restart C s = some t → measure t < measure s := by
  unfold restart
  split
  · intro h; contradiction
  · rename_i hc
    intro h
    simp only [Option.some.injEq] at h
    subst h
    have hpw : s.pw = true := by
      cases hp : s.pw <;> simp_all [State.running]
    have hag : s.ag = false := by simp at hc; exact hc.1.1.2
    have hpl : s.plan = true := by simp at hc; exact hc.1.2
    cases hup : s.up <;> simp [measure, restRank, agentRank, hpw, hag, hpl, hup]

theorem boot_decreases {R : Rules} {s t : State} :
    boot R s = some t → measure t < measure s := by
  unfold boot
  split
  · intro h; contradiction
  · rename_i hc
    have hpw : s.pw = true := by
      cases hp : s.pw <;> simp_all [State.running]
    have hag : s.ag = true := by simp at hc; exact hc.1.2
    have hup : s.up = false := by simp at hc; exact hc.2
    repeat' split
    all_goals (intro h)
    all_goals (try contradiction)
    all_goals (simp only [Option.some.injEq] at h; subst h)
    all_goals (simp [measure, restRank, agentRank, State.dead, hpw, hag, hup])
    all_goals (try omega)

theorem off_decreases {s t : State} : off s = some t → measure t < measure s := by
  unfold off
  split
  · rename_i hc
    intro h
    simp only [Option.some.injEq] at h
    subst h
    simp at hc
    simp [measure, restRank, agentRank, State.dead, hc]
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

/-- Every step of the system itself makes the measure smaller, in every state the design can
reach. -/
theorem own_decreases {C : Cfg} {l : Label} {s t : State} (hs : Scal C s) (ho : own l s = true)
    (h : step Design.rules C l s = some t) : measure t < measure s := by
  cases l <;> simp [own] at ho
  · exact snap_decreases h hs (Or.inr ho)
  · exact snapFail_decreases h hs
  · exact snap_decreases h hs (Or.inl rfl)
  · exact agent_decreases h hs
  · exact agentFail_decreases h hs
  · exact restart_decreases h
  · exact boot_decreases h
  · exact off_decreases h
  · exact timeout_decreases h

/-! ## The environment's moves -/

theorem snapKill_keeps {C : Cfg} {s t : State} :
    snapKill C s = some t → Scal C s → measure t ≤ measure s := by
  unfold snapKill
  open_m
  all_goals close_m

theorem watchdog_keeps {R : Rules} {C : Cfg} {s t : State} :
    watchdog R C s = some t → measure t = measure s := by
  unfold watchdog
  repeat' split
  all_goals (intro h)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h; rfl)

/-- The user's `snapshot create` begins: twenty steps to its end, at most. -/
theorem begin_raises {C : Cfg} {s t : State} (h : snap Design.rules C false s = some t)
    (hs : Scal C s) (hsp : s.sp = .idle) : measure t = measure s + 20 := by
  revert h hs
  unfold snap
  simp only [hsp]
  open_m
  all_goals close_m

/-- A clone is restored: at most twenty-two steps to its end. -/
theorem restore_raises {C : Cfg} {s t : State} (h : restore C s = some t) (hs : Scal C s) :
    measure t ≤ 22 := by
  revert h
  unfold restore
  split
  · rename_i hc
    intro h
    have hpw : s.pw = true := by simp at hc; exact hc.1.1
    have hsp : s.sp = .paused := by simp at hc; exact hc.1.2
    have hi := hs hpw
    cases hr : s.rp with
    | «at» n => simp only [hr] at hi; rw [hi.idle] at hsp; cases hsp
    | failed => simp only [hr] at hi; rw [hi.1] at hsp; cases hsp
    | _ =>
      simp only [hr] at hi
      have hon : s.again = false := hi.2.2.1
      simp only [hr, Option.some.injEq] at h
      subst h
      simp only [measure, restRank, agentRank, snapRank, retryMax, hon]
      repeat' split
      all_goals (simp_all)
      all_goals (try omega)
  · intro h; contradiction

/-- fc-agent is killed: two steps more, the restart and the container's start, in a guest that
is served a boot plan, and nothing more in one that is not. -/
theorem kill_raises {C : Cfg} {s t : State} :
    kill C s = some t → Scal C s → measure t ≤ measure s + (if s.plan then 2 else 0) := by
  unfold kill
  open_m
  all_goals close_m

/-- One step of a run, accounted: a step of the system pays one out of the measure; a kill in a
guest that is served a boot plan, a `snapshot create` begun and a clone restored pay in 2, 20
and 22; nothing else pays in. -/
theorem step_account {C : Cfg} {l : Label} {s t : State} (hs : Scal C s)
    (h : step Design.rules C l s = some t) :
    (if own l s then 1 else 0) + measure t
      ≤ measure s + 2 * (if killsBooted l s then 1 else 0) + 20 * (if begins l s then 1 else 0)
        + 22 * (if l == .restore then 1 else 0) := by
  cases l with
  | pkt c p seg => simp [own, begins, killsBooted, measure_congr (pkt_scal h)]
  | udpC => simp [own, begins, killsBooted, measure_congr (udpC_scal h)]
  | udpR => simp [own, begins, killsBooted, measure_congr (udpR_scal h)]
  | udpS => simp [own, begins, killsBooted, measure_congr (udpS_scal h)]
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      simp [own, begins, killsBooted, measure, restRank, agentRank]
    · contradiction
  | conn w => simp [own, begins, killsBooted, measure_congr (conn_scal h)]
  | shut w => simp [own, begins, killsBooted, measure_congr (shut_scal h)]
  | pair b => simp [own, begins, killsBooted, measure_congr (pair_scal h)]
  | unpair b => simp [own, begins, killsBooted, measure_congr (unpair_scal h)]
  | snap =>
    cases hi : s.sp == .idle
    · have hne : s.sp ≠ .idle := by simpa using hi
      have := snap_decreases h hs (Or.inr hne)
      simp [own, begins, killsBooted, hne]
      omega
    · have he : s.sp = .idle := by simpa using hi
      have := begin_raises h hs he
      simp [own, begins, killsBooted, he]
      omega
  | snapFail =>
    have := snapFail_decreases h hs
    simp [own, begins, killsBooted]
    omega
  | snapKill =>
    have := snapKill_keeps h hs
    simp [own, begins, killsBooted]
    omega
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      simp [own, begins, killsBooted, measure, restRank, agentRank]
    · contradiction
  | guard =>
    have := snap_decreases h hs (Or.inl rfl)
    simp [own, begins, killsBooted]
    omega
  | restore =>
    have := restore_raises h hs
    simp [own, begins, killsBooted]
    omega
  | agent =>
    have := agent_decreases h hs
    simp [own, begins, killsBooted]
    omega
  | agentFail =>
    have := agentFail_decreases h hs
    simp [own, begins, killsBooted]
    omega
  | kill =>
    have := kill_raises h hs
    cases hp : s.plan <;> simp [own, begins, killsBooted, hp] at this ⊢ <;> omega
  | restart =>
    have := restart_decreases h
    simp [own, begins, killsBooted]
    omega
  | boot =>
    have := boot_decreases h
    simp [own, begins, killsBooted]
    omega
  | off =>
    have := off_decreases h
    simp [own, begins, killsBooted]
    omega
  | timeout =>
    have := timeout_decreases h
    simp [own, begins, killsBooted]
    omega
  | watchdog => simp [own, begins, killsBooted, watchdog_keeps h]

/-- A step of the environment that begins nothing leaves the measure where it was, or lower. -/
theorem calm_keeps {C : Cfg} {l : Label} {s t : State} (hs : Scal C s)
    (h : step Design.rules C l s = some t) (ho : own l s = false) (hb : begins l s = false)
    (hk : killsBooted l s = false) (hr : l ≠ .restore) : measure t ≤ measure s := by
  have := step_account hs h
  simp [ho, hb, hk, hr] at this
  exact this

/-- **P6, every boundary ends.** In any run from a reachable state, the steps of the system
number at most the measure at the start, plus two for each kill of fc-agent in a guest that is
served a boot plan, twenty for each `snapshot create` begun and twenty-two for each clone
restored. -/
theorem design_bounded {C : Cfg} {ls : List Label} {s t : State}
    (hr : Reachable Design.rules C s) (h : replay Design.rules C ls s = some t) :
    tally own Design.rules C ls s + measure t
      ≤ measure s + 2 * tally killsBooted Design.rules C ls s
        + 20 * tally begins Design.rules C ls s
        + 22 * tally (fun l _ => l == .restore) Design.rules C ls s := by
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

/-- With the environment beginning nothing and no agent killed that can come back, a run holds
at most 23 steps of the system. -/
theorem design_ends {C : Cfg} {ls : List Label} {s t : State}
    (hr : Reachable Design.rules C s) (h : replay Design.rules C ls s = some t)
    (hk : tally killsBooted Design.rules C ls s = 0)
    (hb : tally begins Design.rules C ls s = 0)
    (hc : tally (fun l _ => l == .restore) Design.rules C ls s = 0) :
    tally own Design.rules C ls s ≤ 23 := by
  have h1 := design_bounded hr h
  have h2 := measure_le (reachable_scal hr)
  omega

/-- A guest restored from a memory image is served no boot plan, in the design as in the code:
its fc-agent, once dead, is never restarted into anything, and no restore runs twice. -/
theorem clone_never_restarts {C : Cfg} {s : State} (hr : Reachable Design.rules C s)
    (hc : s.rp ≠ .none) : step Design.rules C .restart s = none := by
  have hi := reachable_scal hr
  cases hpw : s.pw
  · simp [step, restart, State.running, hpw]
  · have hi := hi hpw
    have hpl : s.plan = false := by
      cases hrp : s.rp with
      | none => exact absurd hrp hc
      | «at» n => simp only [hrp] at hi; exact hi.noPlan
      | failed => simp only [hrp] at hi; exact hi.2.2.1
      | done => simp only [hrp] at hi; exact (hi.2.2.2.2.2.2.2.2.2.2.2 hrp).2
    simp [step, restart, hpl]

/-- No step serves a boot plan to a guest that has none. -/
theorem step_noPlan {C : Cfg} {l : Label} {s t : State}
    (h : step Design.rules C l s = some t) (hp : s.plan = false) :
    t.plan = false ∨ t.pw = false := by
  cases l with
  | pkt c p seg => have := pkt_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | udpC => have := udpC_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | udpR => have := udpR_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | udpS => have := udpS_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | conn w => have := conn_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | shut w => have := shut_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | pair b => have := pair_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | unpair b => have := unpair_scal h; simp [State.scal] at this; exact Or.inl (by rw [this.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.2.1]; exact hp)
  | agent =>
    obtain ⟨n, r, hr, hnr, hag, ht⟩ := agent_spec h
    rcases ht with rfl | rfl
    · exact Or.inl hp
    · refine Or.inl ?_
      have : (doStep Design.rules C r s).plan = s.plan := by
        cases r <;> simp [doStep, State.closeGate, openGate] <;> split <;> simp
      simp only [advance]
      repeat' split
      all_goals (simp only [this, hp])
  | _ =>
    simp only [step] at h
    revert h
    first
      | unfold snap
      | unfold snapFail
      | unfold snapKill
      | unfold restore
      | unfold agentFail
      | unfold kill
      | unfold restart
      | unfold boot
      | unfold off
      | unfold timeout
      | unfold watchdog
      | skip
    repeat' split
    all_goals (intro h)
    all_goals (try contradiction)
    all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
      Option.some.injEq, reduceCtorEq] at h)
    all_goals (try contradiction)
    all_goals (repeat (obtain ⟨_, h⟩ := h))
    all_goals (try subst h)
    all_goals first
      | exact Or.inl hp
      | exact Or.inr rfl
      | (simp_all [State.dead, reopened, unreached, gaveUp, openGate, State.closeGate, advance]
         done)
      | (simp [State.dead, reopened, unreached, gaveUp, openGate, State.closeGate, advance]
         repeat' split
         all_goals simp_all)

/-- In a guest that is served no boot plan no kill is one the bound counts, in any run. -/
theorem noPlan_kills {C : Cfg} : ∀ (ls : List Label) (s : State),
    (s.plan = false ∨ s.pw = false) → tally killsBooted Design.rules C ls s = 0 := by
  intro ls
  induction ls with
  | nil => intro s _; rfl
  | cons l ls ih =>
    intro s hs
    cases hst : step Design.rules C l s with
    | none => simp [tally, hst]
    | some u =>
      have hpw := step_pw hst
      have hp : s.plan = false := by
        rcases hs with h | h
        · exact h
        · rw [hpw] at h; cases h
      rw [tally_cons hst, ih u (step_noPlan hst hp)]
      simp [killsBooted, hp]

/-- One restore: from the clone's first moment, with no `snapshot create` begun and no further
clone restored from it, a run holds at most 22 steps of the system, whatever is killed. -/
theorem design_restore_ends {C : Cfg} {ls : List Label} {s u t : State}
    (hr : Reachable Design.rules C s) (h0 : step Design.rules C .restore s = some u)
    (h : replay Design.rules C ls u = some t)
    (hb : tally begins Design.rules C ls u = 0)
    (hc : tally (fun l _ => l == .restore) Design.rules C ls u = 0) :
    tally own Design.rules C ls u ≤ 22 := by
  have h1 := design_bounded (Reachable.step .restore hr h0) h
  have h2 : measure u ≤ 22 := restore_raises h0 (reachable_scal hr)
  have hp : u.plan = false := by
    have h0' := h0
    simp only [step, restore] at h0'
    split at h0'
    · cases hrp : s.rp <;> simp [hrp] at h0' <;> subst h0' <;> rfl
    · contradiction
  have hk := noPlan_kills (C := C) ls u (Or.inl hp)
  omega

/-! ## Where a run ends -/

/-- The steps that move a boundary on, the host's timeout left out. -/
def movesInGuest : List Label := [.snap, .guard, .agent, .restart, .boot, .off]

/-- No step that moves a boundary on is enabled, the host's timeout not asked. -/
def atRest (R : Rules) (C : Cfg) (s : State) : Bool :=
  !(movesInGuest.any (fun l => own l s && (step R C l s).isSome))

/-- Powered off, or: no snapshot under way, no restore under way, and no half of the gate
installed unless the host has reported the guest as failed and still gated. -/
def settled (s : State) : Bool :=
  !s.pw || (s.sp == .idle && !s.unpublished && (!s.gated || s.reported))

/-- A clone that has not published and whose fc-agent is dead: nothing in the guest can move it
on, and the host's `snapshot run` still waits, so its timeout ends it. -/
def orphaned (s : State) : Bool :=
  s.pw && s.unpublished && !s.ag && (timeout s).isSome

theorem design_next {n : Nat} (h : n < 16) : ∃ r, Design.rules.restore[n]? = some r :=
  ⟨_, List.getElem?_eq_getElem (by simpa [Design.rules, Today.cookieB] using h)⟩

/-- A guest no restore is running on, at rest: no snapshot is under way and no half of the gate
is installed. Whatever state a `snapshot create` is in, its next step, the guard's, or the end
of a guest that could not be reopened is enabled. -/
theorem settle_src {C : Cfg} {s : State} (hpw : s.pw = true) (hi : SrcOk C s)
    (hu : s.unpublished = false) (hl : lockRestore s = false)
    (h : atRest Design.rules C s = true) :
    (s.sp == .idle && !s.unpublished && (!s.gated || s.reported)) = true := by
  cases hsp : s.sp <;> cases hh : s.host <;> cases hag : s.ag <;> cases hrep : s.reported <;>
    simp_all [SrcOk, State.gated, atRest, movesInGuest, own, step, snap,
      Design.rules, inPrepare, reopened, unreached, gaveUp]
  all_goals (obtain ⟨h, -⟩ := h)
  all_goals (split at h)
  all_goals (cases h)

/-- **P6, where a boundary ends.** A reachable state in which no step moves a boundary on is
settled: the guest is powered off (a failed restore's `shutdown_vm`, the host's timeout, or the
host ending a source it could not reopen), or it runs with its gate open and neither a snapshot
nor a restore under way. The one exception is a clone whose fc-agent died before it published:
that one waits for the host's timeout, which is enabled. -/
theorem design_settles {C : Cfg} {s : State} (hr : Reachable Design.rules C s)
    (h : atRest Design.rules C s = true) : settled s = true ∨ orphaned s = true := by
  have hi := reachable_scal hr
  unfold settled orphaned
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp with
    | «at» n =>
      simp only [hrp] at hi
      obtain ⟨r, hnr⟩ := design_next hi.lt
      have hsp := hi.idle
      have hw := hi.waits
      cases hag : s.ag
      · simp [State.unpublished, hrp, timeout, hpw, hw]
      · exfalso
        simp [atRest, movesInGuest, own, step, agent, agentRuns, State.running, lockOneShot,
          nextStep, hpw, hag, hrp, hsp, hnr] at h
        split at h <;> simp at h
    | failed =>
      simp only [hrp] at hi
      have hw := hi.2.1
      cases hag : s.ag
      · simp [State.unpublished, hrp, timeout, hpw, hw]
      · exfalso
        simp [atRest, movesInGuest, own, step, off, hpw, hag, hrp] at h
    | none =>
      exact Or.inl (settle_src hpw (by simpa [hrp] using hi) (by simp [State.unpublished, hrp])
      (by simp [lockRestore, hrp]) h)
    | done =>
      exact Or.inl (settle_src hpw (by simpa [hrp] using hi) (by simp [State.unpublished, hrp])
      (by simp [lockRestore, hrp]) h)

/-! ## The kill -/

/-- In a guest that is served a boot plan and that no boundary holds, fc-agent can be killed and
come back any number of times, and each time is two steps of the system (the restart, and the
container's start): so no number bounds the system's steps when such kills are not bounded.
(These are recoveries. No restore is run again by any of them.) -/
theorem kill_restart_repeats {C : Cfg} (hk : C.kills = true) (k : Nat) :
    ∀ s : State, s.pw = true → s.sp = .idle → s.ag = true → s.plan = true → s.gated = false →
      ∃ ls t, replay Design.rules C ls s = some t
        ∧ tally killsBooted Design.rules C ls s = k
        ∧ tally own Design.rules C ls s = 2 * k
        ∧ t.pw = true ∧ t.sp = .idle ∧ t.ag = true ∧ t.plan = true ∧ t.gated = false := by
  induction k with
  | zero =>
    intro s hp hs ha hl ho
    exact ⟨[], s, rfl, rfl, rfl, hp, hs, ha, hl, ho⟩
  | succ k ih =>
    intro s hp hs ha hl ho
    have hgs := ho
    simp only [State.gated, Bool.or_eq_false_iff] at hgs
    obtain ⟨⟨⟨hi4, hi6⟩, ho4⟩, ho6⟩ := hgs
    have h1 : step Design.rules C .kill s
        = some { s with ag := false, up := false, saved := none, seen := [], lst := [],
                        rcuOwn := false } := by
      simp [step, kill, State.running, hk, hp, hs, ha]
    have h2 : step Design.rules C .restart
        { s with ag := false, up := false, saved := none, seen := [], lst := [],
                 rcuOwn := false }
        = some { s with ag := true, up := false, saved := none, seen := [], lst := [],
                        rcuOwn := false } := by
      simp [step, restart, State.running, hp, hs, hl, ho4]
    have h3 : step Design.rules C .boot
        { s with ag := true, up := false, saved := none, seen := [], lst := [],
                 rcuOwn := false }
        = some { s with ag := true, up := true, frozen := false, saved := none, seen := [],
                        lst := [], rcuOwn := false } := by
      simp [step, boot, State.running, State.gated, hp, hs, hi4, hi6, ho4, ho6, Design.rules]
    obtain ⟨ls, v, h4, h5, h6, vp, vs, va, vl, vo⟩ :=
      ih { s with ag := true, up := true, frozen := false, saved := none, seen := [], lst := [],
                  rcuOwn := false } hp hs rfl hl ho
    refine ⟨.kill :: .restart :: .boot :: ls, v, ?_, ?_, ?_, vp, vs, va, vl, vo⟩
    · simp [replay, h1, h2, h3, h4]
    · rw [tally_cons h1, tally_cons h2, tally_cons h3, h5]
      simp [killsBooted, hl]
      omega
    · rw [tally_cons h1, tally_cons h2, tally_cons h3, h6]
      simp [own]
      omega

/-! ## The theorems are about something

A hypothesis no state meets would make `design_settles` empty, and a measure that is never
reached would make the bounds loose. Runs replayed by the kernel. -/

/-- A snapshot up to the pause, a clone restored, and its restore to the end: 26 steps. -/
def Run.published : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

/-- A guest that no boundary holds is at rest and settled. -/
theorem restAtBoot :
    atRest Design.rules Runs.routed (init Runs.routed) = true
      ∧ settled (init Runs.routed) = true ∧ measure (init Runs.routed) = 0 := by decide

/-- A clone that has published is at rest and settled, with nothing left to run. -/
theorem restWhenPublished :
    (replay Design.rules Runs.routed Run.published (init Runs.routed)).map
      (fun s => atRest Design.rules Runs.routed s && settled s && s.rp == .done
        && measure s == 0) = some true := by decide

/-- The bounds are met: a `snapshot create` begun has measure 20, a clone just restored 21 (22
when its image holds an agent that had not yet started its container). -/
theorem boundsMet :
    (replay Design.rules Runs.routed [.snap] (init Runs.routed)).map measure = some 20
      ∧ (replay Design.rules Runs.routed (Run.published.take 10) (init Runs.routed)).map measure
        = some 21 := by decide

/-- A clone whose agent is killed inside its restore is orphaned: at rest, not settled, and the
host's timeout powers it off. -/
theorem orphanWaits :
    (replay Design.rules Runs.kills (Run.published.take 14 ++ [.kill]) (init Runs.kills)).map
      (fun s => atRest Design.rules Runs.kills s && !settled s && orphaned s) = some true
    ∧ (replay Design.rules Runs.kills (Run.published.take 14 ++ [.kill, .timeout])
        (init Runs.kills)).map (·.pw) = some false := by decide

/-- A source whose agent dies during the prepare, on Firecracker: the host tries the reopen
three times, no agent answers, and it gives up. The source runs on, gated, and the host has
recorded it as failed: not stuck, settled, and no further `snapshot create` is admitted. -/
theorem sourceReported :
    (replay Design.rules Runs.kills [.snap, .snap, .kill, .snap, .snap, .snap, .snap]
      (init Runs.kills)).map
        (fun s => s.pw && s.reported && s.gated && settled s && !stuck Design.rules Runs.kills s
          && !sourceDisturbed s && (step Design.rules Runs.kills .snap s).isNone)
      = some true := by decide

/-- What the code does today (#2158, observed in VMs): over vsock, the agent of a source dies
during the prepare and is restarted; its start of the container is refused and it powers the VM
off, whether the host's reopen landed first (the first run: the gate is reopened and the
`snapshot create` goes on to its end) or not (the second). -/
theorem restartPowersOffToday :
    (replay Today.cookieB Runs.killsVsock [.snap, .snap, .kill, .restart, .snap, .snap]
      (init Runs.killsVsock)).map (fun s => s.pw && !s.gated && s.sp == .idle && !s.up)
      = some true
    ∧ (replay Today.cookieB Runs.killsVsock [.snap, .snap, .kill, .restart, .snap, .snap, .boot]
      (init Runs.killsVsock)).map (·.pw) = some false
    ∧ (replay Today.cookieB Runs.killsVsock [.snap, .snap, .kill, .restart, .boot]
      (init Runs.killsVsock)).map (·.pw) = some false := by decide

/-- The same over vsock (Cloud Hypervisor): the restarted agent serves exec behind the closed
gate, the reopen lands, and then the container is started. Nothing is reported. -/
theorem sourceRecovers :
    (replay Design.rules Runs.killsVsock
      [.snap, .snap, .kill, .restart, .snap, .snap, .boot] (init Runs.killsVsock)).map
        (fun s => s.pw && !s.reported && !s.gated && s.ag && s.up && s.sp == .idle
          && s.man.isNone && !sourceDisturbed s)
      = some true
    -- and the container's start is held until then
    ∧ (replay Design.rules Runs.killsVsock [.snap, .snap, .kill, .restart, .boot]
        (init Runs.killsVsock)) = none := by decide

end Gate
