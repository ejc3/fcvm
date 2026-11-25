import GateVerify.Lemmas
import GateVerify.Socks
import GateVerify.Gens
import GateVerify.Ends

/-!
# The design keeps its properties, in every reachable state

For every configuration `C` and every state reachable from `init C` under `Design.rules`, by
runs of any length. What "every configuration" covers is the universe `Spec.lean` builds:
every value of `Cfg`, any number of cookies handed out and of restores, and at most one socket
of each of the fourteen kinds at a time (two clients, two ports, one outbound connection a
process, one connection inside the guest of each of two kinds), three tracked tuples, one
`snapshot create` at a time. `verify/GateVerify/README.md` has the list.

No `sorry`, and no axiom beyond Lean's own (`Axioms.lean` checks it on every build). The proofs
are inductive invariants (`Scal` and `Flags` in `Lemmas.lean`, `Sk` in `Socks.lean`, `Gn` in
`Gens.lean`) over the same `step` the search runs, not the search, and a measure (`Ends.lean`).

Not here: U1 before the clone's UDP rule is in. That no client datagram reaches a clone then is
the host's doing and is assumed (`Cfg.udpReach`). `design_udpRuleHolds` is the part the guest
does.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

variable {C : Cfg} {s : State}

/-- **P1.** No established connection of a guest being snapshotted is reset, and once no boundary
is in progress the guest has no half of the gate, no manifest, `rcu_expedited` as it was, and
its container thawed. That every boundary is over within a bounded number of steps, with the
source running ungated or ended by the host that could not reopen it, is `design_bounded` and
`design_settles`. -/
theorem design_sourceUndisturbed (hr : Reachable Design.rules C s) : sourceDisturbed s = false := by
  have hi := reachable_scal hr
  have hf := reachable_flags hr
  unfold sourceDisturbed
  cases hpw : s.pw
  · simp [hf.1]
  · have hi := hi hpw
    cases hrp : s.rp <;> cases hsp : s.sp <;>
      simp_all [SrcOk, State.gated, hf.1, inPrepare] <;>
      (intro hfr; simp_all)

/-- **P2.** No retirement destroys a socket a process of the clone opened, a socket a packet made
with the gate open, or an end of a connection inside the guest whose other end stays. -/
theorem design_fixedGeneration (hr : Reachable Design.rules C s) : generationBroken s = false :=
  (reachable_gn hr).gB

/-- **P2, without the record.** When the restoring agent is at its retirement, every socket the
retirement names, by cookie, in the manifest or on the restore's own list, is one it must
destroy: it was in the image, or a packet made it behind the closed gate. One the manifest
names was in the image. One with a mirror image among the guest's sockets is named by the
manifest and is no end of a loopback connection (so it is the end, on the guest's own address,
of a connection the snapshot named for retirement). And no socket a process of the clone
opened is reset by what the retirement destroys. -/
theorem design_retiresOnly (hr : Reachable Design.rules C s) (h10 : s.rp = .at 10)
    (ha : s.ag = true) :
    (∀ x ∈ s.socks, x.id ∈ s.man.getD [] ++ s.lst →
      (x.gen = .stale ∨ x.gen = .smuggled)
      ∧ (x.id ∈ s.man.getD [] → x.gen = .stale)
      ∧ (hasMirror s.socks x = true → x.id ∈ s.man.getD [] ∧ x.kind.loop = false))
    ∧ (∀ y ∈ s.socks, y.gen = .own →
        resetBy (s.socks.filter (fun x => (s.man.getD [] ++ s.lst).contains x.id)) y = false) := by
  have hg := reachable_gn hr
  have hnamed := named_ok hg h10 ha
  refine ⟨?_, ?_⟩
  · intro x hx hin
    rw [List.mem_append] at hin
    obtain ⟨a, b, c⟩ := hnamed x hx hin
    refine ⟨?_, hg.gM 10 h10 x hx, c⟩
    cases hgen : x.gen <;> simp_all
  · refine own_not_reset hg _ ?_
    intro x hx
    rw [List.mem_filter] at hx
    exact ⟨hx.1, (hnamed x hx.1 (by simpa using hx.2)).1⟩

/-- **P3, first half.** Once a restored guest's ingress is open in either family, it holds no
socket of a connection that leaves the guest, directly or through the egress proxy, that was in
the image or made behind the closed gate. -/
theorem design_noStaleAtPublication (hr : Reachable Design.rules C s) :
    staleAtPublication s = false := by
  have hi := reachable_scal hr
  have hk := reachable_sk hr
  unfold staleAtPublication
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    -- with ingress open, the retirement is behind
    have key : s.cut = true →
        (s.socks.any fun x => x.kind.outside && (x.gen == .stale || x.gen == .smuggled)) = false := by
      intro hc
      rw [List.any_eq_false]
      intro x hx hbad
      simp only [Bool.and_eq_true, Bool.or_eq_true, beq_iff_eq] at hbad
      exact hk.k3 hc x hx hbad.1 hbad.2
    cases hrp : s.rp with
    | none => simp
    | done =>
      have hc : s.cut = true := by simp_all [SrcOk]
      simp [key hc]
    | failed =>
      cases hc : s.cut
      · have : s.in4 = true ∧ s.in6 = true := by simp_all [FailedOk]
        simp [this.1, this.2]
      · simp [key hc]
    | «at» n =>
      simp only [hrp] at hi
      cases hc : s.cut
      · have hn : n < 16 := hi.lt
        have : s.in4 = true ∧ s.in6 = true := by
          rcases lt16 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
            | h0 | h0 | h0 <;> subst h0 <;> simp_all [CloneOk]
        simp [this.1, this.2]
      · simp [key hc]

/-- **P3, second half.** No packet from outside makes a socket in a clone between its retirement
and its publication. -/
theorem design_noSmuggledLate (hr : Reachable Design.rules C s) : smuggledLate s = false :=
  (reachable_flags hr).2.2.2.2

/-- **P4.** No socket is retired with the egress half closed. -/
theorem design_egressBeforeRetirement (hr : Reachable Design.rules C s) :
    retiredBehindEgress s = false :=
  (reachable_flags hr).2.1

/-- **P4.** The hook runs with egress open and ingress closed. -/
theorem design_hookPlaced (hr : Reachable Design.rules C s) : hookMisplaced s = false :=
  (reachable_flags hr).2.2.1

/-- **P4.** While a clone's egress half is closed its container does not run and nothing of it
has been retired. -/
theorem design_workloadEgress (hr : Reachable Design.rules C s) :
    workloadBehindEgress s = false := by
  have hi := reachable_scal hr
  unfold workloadBehindEgress
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp with
    | «at» n =>
      simp only [hrp] at hi
      have hn : n < 16 := hi.lt
      rcases lt16 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
        | h0 | h0 <;> subst h0 <;> simp_all [CloneOk, State.ctrRuns]
    | _ => simp

/-- **P5.** A published clone that no snapshot holds has `tcp_syncookies` as it was at boot and
no UDP rule. -/
theorem design_noLastingChange (hr : Reachable Design.rules C s) : lastingChange C s = false := by
  have hi := reachable_scal hr
  unfold lastingChange
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp <;> simp_all [SrcOk]

/-- **P5.** No memory image is taken while a setting is changed. -/
theorem design_noCapturedChange (hr : Reachable Design.rules C s) : capturedChange s = false :=
  (reachable_flags hr).2.2.2.1

/-- **P6.** While a half of the gate is installed or the guest is paused, a step that moves the
boundary on is enabled: the snapshot's next step, the guard's, the restore's, the restart of a
cold-booted guest's agent, a failed clone's power-off, or the host's timeout. The timeout is
the one left only for a clone whose fc-agent has died (`design_settles`). The user's next
`snapshot create` is not counted. This is progress; the end is `design_bounded`. -/
theorem design_neverStuck (hr : Reachable Design.rules C s) : stuck Design.rules C s = false := by
  cases hst : stuck Design.rules C s with
  | false => rfl
  | true =>
    exfalso
    simp only [stuck, moves, Bool.and_eq_true, Bool.not_eq_true', List.any_eq_false,
      List.mem_cons, List.mem_nil_iff, or_false, Bool.not_eq_true] at hst
    obtain ⟨⟨hpw, hg⟩, hno⟩ := hst
    have hrest : atRest Design.rules C s = true := by
      simp only [atRest, movesInGuest, Bool.not_eq_true', List.any_eq_false, List.mem_cons,
        List.mem_nil_iff, or_false, Bool.not_eq_true]
      intro l hl
      have := hno l (by rcases hl with h | h | h | h | h <;> simp [h])
      cases ho : own l s <;> cases hs : (step Design.rules C l s).isSome <;> simp_all
    rcases design_settles hr hrest with h | h
    · simp [settled, hpw] at h
      obtain ⟨⟨h1, -⟩, h3⟩ := h
      simp [h1, h3] at hg
    · simp only [orphaned, Bool.and_eq_true] at h
      have := hno .timeout (by simp)
      simp [own, step, h.2] at this

/-- **U1, the guest's part.** A client's datagram reaches a clone that has not published only
before the restore's UDP rule is in (its first three steps), or in a clone whose restore has
failed and that is on its way to the power-off. Before the rule, what keeps a datagram out is
the host's gate on the clone's TAP and, for a routed clone, its address: assumed
(`Cfg.udpReach`), not proved. -/
theorem design_udpRuleHolds {t : State} (hr : Reachable Design.rules C s)
    (h : udpC Design.rules C s = some t) (hnew : t.udpIn = true) (hold : s.udpIn = false) :
    s.rp = .failed ∨ ∃ n, s.rp = .at n ∧ n ≤ 2 := by
  have hi := reachable_scal hr
  unfold udpC at h
  split at h
  · contradiction
  · split at h
    · contradiction
    · split at h
      · contradiction
      · rename_i hrun _ hudp
        simp only [Option.some.injEq] at h
        subst h
        have hpw : s.pw = true := by
          cases hp : s.pw <;> simp_all [State.running]
        have hi := hi hpw
        simp only [hold, Bool.false_or, Bool.and_eq_true, Bool.or_eq_true] at hnew
        obtain ⟨hun, hin⟩ := hnew
        cases hrp : s.rp with
        | none => simp [State.unpublished, hrp] at hun
        | done => simp [State.unpublished, hrp] at hun
        | failed => exact Or.inl rfl
        | «at» n =>
          refine Or.inr ⟨n, rfl, ?_⟩
          simp only [hrp] at hi
          have hu : s.udp = false := by simpa using hudp
          cases Nat.lt_or_ge n 3 with
          | inl h3 => omega
          | inr h3 =>
            exfalso
            cases Nat.lt_or_ge n 15 with
            | inl h15 =>
              have := hi.udpIn h3 (by omega)
              rw [hu] at this; cases this
            | inr h15 =>
              have := hi.2.2.2.2.2.2.1 h15
              rcases hin with h | h
              · rw [this.1] at h; cases h
              · rw [this.2.1] at h; cases h

end Gate
